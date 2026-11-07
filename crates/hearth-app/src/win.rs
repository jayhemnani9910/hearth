//! One app window: a connection to one host, showing one session.
//!
//! Widgets live outside the `RefCell`; state lives inside. GTK signal
//! handlers go through `Win::with`, which defers to an idle callback when the
//! state is already borrowed (signals fired by our own widget updates).

use crate::keymap::{Keymap, Outcome};
use crate::palette;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use hearth_core::conn::{ApiError, Client, ConnectError, Incoming, Target};
use hearth_core::keys::Chord;
use hearth_core::scan::QueryFilter;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};
use vte::prelude::*;

type Callback = Box<dyn FnOnce(&Rc<Win>, &mut State, Result<Value, ApiError>)>;
type Connected = Result<(Client, Receiver<Incoming>), ConnectError>;

/// Bytes fed to terminals per tick before yielding to the UI.
const FEED_BUDGET: usize = 4 << 20;

pub struct Ui {
    pub window: adw::ApplicationWindow,
    pub tab_view: adw::TabView,
    pub banner: adw::Banner,
    pub toasts: adw::ToastOverlay,
    pub title: adw::WindowTitle,
    pub stack: gtk::Stack,
    pub session_button: gtk::MenuButton,
    pub session_list: gtk::ListBox,
}

thread_local! {
    /// Open windows, so a notification can find the one it came from.
    static WINDOWS: RefCell<Vec<(u32, std::rc::Weak<Win>)>> = const { RefCell::new(Vec::new()) };
}

/// Target of the `app.focus-block` action: "<window id>/<block>".
pub fn focus_from_notification(target: &str) {
    let Some((id, block)) = target.split_once('/') else { return };
    let win = WINDOWS.with(|ws| ws.borrow().iter().find(|(i, _)| i.to_string() == id).and_then(|(_, w)| w.upgrade()));
    if let Some(w) = win {
        w.ui.window.present();
        let block = block.to_string();
        w.with(move |w, s| w.focus_pane(s, &block));
    }
}

pub struct Win {
    pub ui: Ui,
    id: u32,
    /// Keystrokes in arrival order, sent by `flush_input`.
    outbox: RefCell<std::collections::VecDeque<(String, Vec<u8>)>>,
    st: RefCell<State>,
    /// Set while we change tabs ourselves, so tab signals are not echoed.
    syncing: Cell<bool>,
    tick: Cell<Option<glib::SourceId>>,
}

enum Conn {
    Up(Rc<Client>, Receiver<Incoming>),
    Connecting(Receiver<Connected>),
    Down { next: Instant, delay: Duration },
    Closed,
}

struct Pane {
    root: gtk::Box,
    term: vte::Terminal,
    title: gtk::Label,
    status: gtk::Label,
    note: gtk::Label,
    sub: Option<u32>,
    attaching: bool,
    /// Attach once the widget has its real size.
    want_attach: bool,
    /// Snapshots and data bytes fed to VTE (shown by the test probe).
    fed: (u64, u64),
    filter: QueryFilter,
    generation: String,
    offset: u64,
    pty: (u64, u64),
    owner: Option<String>,
    info: Value,
    exited: Option<String>,
    input_unsure: bool,
    resize_timer: Option<glib::SourceId>,
    /// Grid size last reported to the server.
    sent_size: (i64, i64),
}

struct Divider {
    paned: gtk::Paned,
    path: String,
    ratio: Rc<Cell<f64>>,
}

struct Page {
    page: adw::TabPage,
    holder: gtk::Box,
    key: String,
    dividers: Vec<Divider>,
}

pub struct State {
    host: String,
    conn: Conn,
    reason: String,
    client_id: String,
    incarnation: String,
    session: Option<String>,
    want_session: Option<String>,
    first_show: bool,
    sessions: BTreeMap<String, Value>,
    seq: u64,
    config: Value,
    pub keymap: Keymap,
    panes: HashMap<String, Pane>,
    subs: HashMap<u32, String>,
    orphans: HashMap<u32, Vec<Incoming>>,
    pages: HashMap<String, Page>,
    pending: Vec<(Receiver<Result<Value, ApiError>>, Callback)>,
    input_seq: u64,
    unacked: BTreeMap<u64, String>,
    remote_control: bool,
    focused: Option<String>,
    select_window: Option<String>,
    focus_block: Option<String>,
    font_scale: f64,
    log: Vec<String>,
    notified: Vec<String>,
    created_first: bool,
    /// Test probe: last nonce asked for and when the file was last written.
    probe_nonce: Value,
    probe_at: Option<Instant>,
}

pub struct Options {
    pub host: String,
    pub session: Option<String>,
    pub remote_control: bool,
}

fn probe_path() -> Option<String> {
    std::env::var("HEARTH_APP_PROBE").ok().filter(|s| !s.is_empty())
}

pub fn target_for(host: &str) -> Result<Target, String> {
    if host == "local" {
        Ok(Target::Unix(hearth_core::paths::socket_path()))
    } else {
        Target::parse(host)
    }
}

fn connect_blocking(host: String) -> Connected {
    let target = target_for(&host).map_err(ConnectError::Io)?;
    let local_default = matches!(&target, Target::Unix(p) if *p == hearth_core::paths::socket_path());
    match Client::connect(&target, "app", "hearth-app") {
        Err(ConnectError::NoServer(_)) if local_default => {
            hearth_core::conn::ensure_local_server()?;
            Client::connect(&target, "app", "hearth-app")
        }
        r => r,
    }
}

impl Win {
    pub fn new(app: &adw::Application, opts: Options) -> Rc<Win> {
        let ui = build_ui(app);
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        let win = Rc::new(Win {
            ui,
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            outbox: RefCell::default(),
            st: RefCell::new(State {
                host: opts.host,
                conn: Conn::Down { next: Instant::now(), delay: Duration::from_millis(250) },
                reason: String::new(),
                client_id: String::new(),
                incarnation: String::new(),
                session: None,
                want_session: opts.session,
                first_show: true,
                sessions: BTreeMap::new(),
                seq: 0,
                config: Value::Null,
                keymap: Keymap::from_config(&Value::Null),
                panes: HashMap::new(),
                subs: HashMap::new(),
                orphans: HashMap::new(),
                pages: HashMap::new(),
                pending: vec![],
                input_seq: 0,
                unacked: BTreeMap::new(),
                remote_control: opts.remote_control,
                focused: None,
                select_window: None,
                focus_block: None,
                font_scale: 1.0,
                log: vec![],
                notified: vec![],
                created_first: false,
                probe_nonce: Value::Null,
                probe_at: None,
            }),
            syncing: Cell::new(false),
            tick: Cell::new(None),
        });
        win.wire();
        WINDOWS.with(|ws| {
            let mut ws = ws.borrow_mut();
            ws.retain(|(_, w)| w.strong_count() > 0);
            ws.push((win.id, Rc::downgrade(&win)));
        });
        let w = win.clone();
        let id = glib::timeout_add_local(Duration::from_millis(8), move || {
            w.with(|w, s| w.tick(s));
            glib::ControlFlow::Continue
        });
        win.tick.set(Some(id));
        win.with(|w, s| w.refresh_title(s));
        win.ui.window.present();
        win
    }

    /// Runs `f` with the state, or soon after if the state is in use.
    pub fn with<F: FnOnce(&Rc<Win>, &mut State) + 'static>(self: &Rc<Win>, f: F) {
        match self.st.try_borrow_mut() {
            Ok(mut s) => f(self, &mut s),
            Err(_) => {
                let w = self.clone();
                glib::idle_add_local_once(move || w.with(f));
            }
        }
    }

    fn wire(self: &Rc<Win>) {
        let ui = &self.ui;
        // Window actions, used by menus, the context menu and notifications.
        for name in crate::actions::SIMPLE {
            let a = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(w) = w.upgrade() {
                    w.with(move |w, s| w.run_action(s, name, json!({})));
                }
            });
            ui.window.add_action(&a);
        }
        let rc = gio::SimpleAction::new_stateful("remote-control", None, &self.st.borrow().remote_control.to_variant());
        let w = Rc::downgrade(self);
        rc.connect_activate(move |_, _| {
            if let Some(w) = w.upgrade() {
                w.with(|w, s| w.run_action(s, "toggle-remote-control", json!({})));
            }
        });
        ui.window.add_action(&rc);

        // Keys: capture phase, so bindings win over the terminal.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(w) = w.upgrade() else { return glib::Propagation::Proceed };
            if w.ui.window.visible_dialog().is_some() {
                return glib::Propagation::Proceed;
            }
            let Some(chord) = chord_from(key, mods) else { return glib::Propagation::Proceed };
            w.press_chord(chord)
        });
        ui.window.add_controller(keys);

        let w = Rc::downgrade(self);
        ui.tab_view.connect_close_page(move |tv, page| {
            let Some(w) = w.upgrade() else { return glib::Propagation::Proceed };
            if w.syncing.get() {
                tv.close_page_finish(page, true);
                return glib::Propagation::Stop;
            }
            tv.close_page_finish(page, false);
            let holder = page.child();
            w.with(move |w, s| {
                if let Some(wid) = s.window_of_holder(&holder) {
                    w.confirm_close_window(s, wid);
                }
            });
            glib::Propagation::Stop
        });
        let w = Rc::downgrade(self);
        ui.tab_view.connect_selected_page_notify(move |tv| {
            let Some(w) = w.upgrade() else { return };
            if w.syncing.get() {
                return;
            }
            let Some(holder) = tv.selected_page().map(|p| p.child()) else { return };
            w.with(move |w, s| {
                let Some(wid) = s.window_of_holder(&holder) else { return };
                if let Some(p) = s.pages.get(&wid) {
                    p.page.set_needs_attention(false);
                }
                w.call(s, "window.select", json!({"window": wid}), |_, _, _| {});
                if let Some(b) = s.window_view(&wid).and_then(|v| v["focus"].as_str().map(String::from)) {
                    w.focus_pane(s, &b);
                }
                w.refresh_title(s);
            });
        });
        let w = Rc::downgrade(self);
        ui.tab_view.connect_page_reordered(move |_, page, index| {
            let Some(w) = w.upgrade() else { return };
            if w.syncing.get() {
                return;
            }
            let holder = page.child();
            w.with(move |w, s| {
                if let Some(wid) = s.window_of_holder(&holder) {
                    w.call(s, "window.move", json!({"window": wid, "index": index}), |_, _, _| {});
                }
            });
        });
        let w = Rc::downgrade(self);
        ui.banner.connect_button_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.with(|w, s| w.run_action(s, "reconnect", json!({})));
            }
        });
        let w = Rc::downgrade(self);
        ui.session_button.popover().unwrap().connect_show(move |_| {
            if let Some(w) = w.upgrade() {
                w.with(|w, s| w.fill_session_list(s));
            }
        });
        let w = Rc::downgrade(self);
        ui.session_list.connect_row_activated(move |_, row| {
            let Some(w) = w.upgrade() else { return };
            let what = row.widget_name().to_string();
            w.ui.session_button.popdown();
            w.with(move |w, s| {
                if let Some(sid) = what.strip_prefix("session:") {
                    w.switch_session(s, sid.to_string());
                } else if let Some(h) = what.strip_prefix("host:") {
                    w.switch_host(s, h.to_string());
                } else if what == "new-session" {
                    w.run_action(s, "new-session", json!({}));
                }
            });
        });
        let w = Rc::downgrade(self);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(w) = w.upgrade() {
                w.with(|_, s| s.panes.values().for_each(|p| apply_colors(&p.term)));
            }
        });
        let w = Rc::downgrade(self);
        ui.window.connect_close_request(move |_| {
            if let Some(w) = w.upgrade() {
                if let Some(id) = w.tick.take() {
                    id.remove();
                }
                w.with(|_, s| {
                    // Programs keep running on the server; this only disconnects.
                    s.conn = Conn::Closed;
                    s.pending.clear();
                });
            }
            glib::Propagation::Proceed
        });
    }

    pub fn press_chord(self: &Rc<Win>, chord: Chord) -> glib::Propagation {
        let Ok(mut s) = self.st.try_borrow_mut() else { return glib::Propagation::Proceed };
        let out = s.keymap.press(chord);
        self.refresh_title(&mut s);
        drop(s);
        match out {
            Outcome::Run(a, args) => {
                self.with(move |w, s| w.run_action(s, &a, args));
                glib::Propagation::Stop
            }
            Outcome::Pending | Outcome::Swallow => glib::Propagation::Stop,
            Outcome::Pass => glib::Propagation::Proceed,
        }
    }

    // ---------------------------------------------------------------- RPC

    pub fn call<F: FnOnce(&Rc<Win>, &mut State, Result<Value, ApiError>) + 'static>(self: &Rc<Win>, s: &mut State, method: &str, params: Value, cb: F) {
        match &s.conn {
            Conn::Up(c, _) => {
                let rx = c.call_async(method, params);
                s.pending.push((rx, Box::new(cb)));
            }
            _ => {
                let e = ApiError::new("not_ready", format!("not connected to {}", s.host));
                cb(self, s, Err(e));
            }
        }
    }

    /// Calls a method and shows its error, if any, as a toast.
    pub fn call_toast(self: &Rc<Win>, s: &mut State, method: &str, params: Value) {
        let m = method.to_string();
        self.call(s, method, params, move |w, s, r| {
            if let Err(e) = r {
                w.toast(s, &format!("{m}: {}", e.message));
            }
        });
    }

    pub fn toast(self: &Rc<Win>, s: &mut State, text: &str) {
        s.log.push(text.to_string());
        if s.log.len() > 50 {
            s.log.remove(0);
        }
        self.ui.toasts.add_toast(adw::Toast::new(text));
    }

    // --------------------------------------------------------------- tick

    fn tick(self: &Rc<Win>, s: &mut State) {
        match &s.conn {
            Conn::Down { next, .. } if Instant::now() >= *next => {
                let (tx, rx) = std::sync::mpsc::channel();
                let host = s.host.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(connect_blocking(host));
                });
                s.conn = Conn::Connecting(rx);
                self.refresh_title(s);
            }
            Conn::Connecting(rx) => match rx.try_recv() {
                Ok(Ok((c, rx))) => {
                    s.conn = Conn::Up(Rc::new(c), rx);
                    self.on_connected(s);
                }
                Ok(Err(e)) => self.connection_lost(s, e.to_string()),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.connection_lost(s, "connect thread failed".into()),
            },
            _ => {}
        }
        if let Conn::Up(_, rx) = &s.conn {
            let mut batch = vec![];
            let mut bytes = 0;
            while bytes < FEED_BUDGET {
                match rx.try_recv() {
                    Ok(m) => {
                        if let Incoming::Data { bytes: b, .. } | Incoming::Snapshot { bytes: b, .. } = &m {
                            bytes += b.len();
                        }
                        batch.push(m);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        batch.push(Incoming::Closed("connection closed".into()));
                        break;
                    }
                }
            }
            for m in batch {
                self.on_incoming(s, m);
                if !matches!(s.conn, Conn::Up(..)) {
                    break;
                }
            }
        }
        // Tests can read the app's state even while it is disconnected.
        if probe_path().is_some() && s.probe_at.is_none_or(|t| t.elapsed() > Duration::from_millis(100)) {
            self.write_probe(s);
        }
        // Attach panes once laid out, so the attach carries their real size.
        let ready: Vec<String> = s
            .panes
            .iter()
            .filter(|(_, p)| p.want_attach && p.term.is_mapped() && p.term.width() > 1)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            self.attach(s, &id, true);
        }
        // VTE has no signal for grid size changes; compare each tick.
        let changed: Vec<String> = s
            .panes
            .iter()
            .filter(|(_, p)| p.sub.is_some() && p.term.is_mapped() && (p.term.column_count(), p.term.row_count()) != p.sent_size)
            .map(|(id, _)| id.clone())
            .collect();
        for id in changed {
            self.schedule_resize(s, &id);
        }
        // Responses: run callbacks whose answers have arrived.
        let mut i = 0;
        while i < s.pending.len() {
            match s.pending[i].0.try_recv() {
                Ok(r) => {
                    let (_, cb) = s.pending.swap_remove(i);
                    cb(self, s, r);
                }
                Err(TryRecvError::Disconnected) => {
                    let (_, cb) = s.pending.swap_remove(i);
                    cb(self, s, Err(ApiError::new("failed", "connection closed")));
                }
                Err(TryRecvError::Empty) => i += 1,
            }
        }
    }

    fn connection_lost(self: &Rc<Win>, s: &mut State, reason: String) {
        if matches!(s.conn, Conn::Closed) {
            return;
        }
        let delay = match &s.conn {
            Conn::Down { delay, .. } => (*delay * 2).min(Duration::from_secs(10)),
            _ => Duration::from_millis(500),
        };
        let was_up = matches!(s.conn, Conn::Up(..));
        s.conn = Conn::Down { next: Instant::now() + delay, delay };
        s.reason = reason.clone();
        s.pending.clear();
        s.subs.clear();
        s.orphans.clear();
        let unsure: HashSet<String> = s.unacked.values().cloned().collect();
        s.unacked.clear();
        for (id, p) in s.panes.iter_mut() {
            p.sub = None;
            p.attaching = false;
            p.term.set_input_enabled(false);
            if unsure.contains(id) {
                p.input_unsure = true;
            }
            refresh_pane_header(p, &s.client_id);
        }
        let mut text = format!("Disconnected from {}: {reason}. Reconnecting…", s.host);
        if !unsure.is_empty() {
            let mut v: Vec<_> = unsure.into_iter().collect();
            v.sort();
            text.push_str(&format!(" Input sent to {} was not confirmed.", v.join(", ")));
        }
        self.ui.banner.set_title(&text);
        self.ui.banner.set_button_label(Some("Reconnect now"));
        self.ui.banner.set_revealed(true);
        if was_up {
            s.log.push(text);
        }
        self.refresh_title(s);
    }

    fn on_connected(self: &Rc<Win>, s: &mut State) {
        let Conn::Up(c, _) = &s.conn else { return };
        let welcome = c.welcome.clone();
        s.client_id = welcome["client_id"].as_str().unwrap_or("").into();
        let inc = welcome["server"]["incarnation"].as_str().unwrap_or("").to_string();
        if !s.incarnation.is_empty() && s.incarnation != inc {
            s.log.push("server restarted: terminals start from a fresh snapshot".into());
        }
        s.incarnation = inc;
        s.reason.clear();
        self.ui.banner.set_revealed(false);
        if s.remote_control {
            self.call_toast(s, "client.set", json!({"remote_control": true}));
        }
        self.load_config(s);
        self.call(s, "session.bootstrap", json!({"subscribe": true}), |w, s, r| match r {
            Ok(v) => w.on_bootstrap(s, v),
            Err(e) => w.toast(s, &format!("bootstrap failed: {}", e.message)),
        });
        self.refresh_title(s);
    }

    fn load_config(self: &Rc<Win>, s: &mut State) {
        self.call(s, "config.get", json!({}), |w, s, r| {
            if let Ok(cfg) = r {
                s.keymap = Keymap::from_config(&cfg);
                if let Some(e) = cfg["error"].as_str() {
                    w.toast(s, &format!("config error: {e}"));
                }
                s.config = cfg;
                w.refresh_title(s);
            }
        });
    }

    fn on_bootstrap(self: &Rc<Win>, s: &mut State, v: Value) {
        s.seq = v["seq"].as_u64().unwrap_or(0);
        s.sessions.clear();
        for view in v["sessions"].as_array().into_iter().flatten() {
            if let Some(id) = view["id"].as_str() {
                s.sessions.insert(id.into(), view.clone());
            }
        }
        let want = s.want_session.take();
        let chosen = want
            .as_deref()
            .and_then(|w| s.resolve_session(w))
            .or_else(|| s.session.clone().filter(|id| s.sessions.contains_key(id)))
            .or_else(|| s.sessions.keys().next().cloned());
        match chosen {
            Some(id) => {
                if s.session.as_ref() != Some(&id) {
                    s.first_show = true;
                }
                s.session = Some(id);
                self.sync(s);
            }
            None if !s.created_first => {
                // Nothing to show yet: start a session so the window is usable.
                s.created_first = true;
                let name = want.unwrap_or_default();
                let params = if name.is_empty() { json!({}) } else { json!({"name": name}) };
                self.call(s, "session.create", params, |w, s, r| match r {
                    Ok(ids) => {
                        if let Some(sid) = ids["session"].as_str() {
                            w.switch_session(s, sid.into());
                        }
                    }
                    Err(e) => w.toast(s, &format!("could not create a session: {}", e.message)),
                });
            }
            None => self.sync(s),
        }
    }

    // ------------------------------------------------------------ incoming

    fn on_incoming(self: &Rc<Win>, s: &mut State, m: Incoming) {
        match m {
            Incoming::Closed(r) => self.connection_lost(s, r),
            Incoming::Snapshot { sub, offset, bytes } => {
                let Some(b) = s.subs.get(&sub).cloned() else {
                    s.orphans.entry(sub).or_default().push(Incoming::Snapshot { sub, offset, bytes });
                    return;
                };
                if let Some(p) = s.panes.get_mut(&b) {
                    // Restore into a grid of the snapshot's size; if that is
                    // not our widget's size, VTE then resizes like any terminal.
                    let (cols, rows) = (p.pty.0 as i64, p.pty.1 as i64);
                    let resize = cols > 1 && rows > 1 && (p.term.column_count(), p.term.row_count()) != (cols, rows);
                    if resize {
                        p.term.set_size(cols, rows);
                    }
                    p.filter = QueryFilter::new();
                    p.term.reset(true, true);
                    p.term.feed(&p.filter.push(&bytes));
                    if resize {
                        p.term.queue_resize();
                    }
                    p.offset = offset;
                    p.fed.0 += 1;
                }
            }
            Incoming::Data { sub, offset, bytes } => {
                let Some(b) = s.subs.get(&sub).cloned() else {
                    s.orphans.entry(sub).or_default().push(Incoming::Data { sub, offset, bytes });
                    return;
                };
                let Some(p) = s.panes.get_mut(&b) else { return };
                if offset != p.offset {
                    // Never guess across a gap: start over from a snapshot.
                    s.log.push(format!("{b}: stream gap at {} (got {offset}); re-attaching", p.offset));
                    self.attach(s, &b, false);
                    return;
                }
                p.term.feed(&p.filter.push(&bytes));
                p.offset += bytes.len() as u64;
                p.fed.1 += bytes.len() as u64;
            }
            Incoming::Event(v) => self.on_event(s, v),
        }
    }

    fn on_event(self: &Rc<Win>, s: &mut State, v: Value) {
        if let Some(seq) = v["seq"].as_u64() {
            if seq <= s.seq {
                return;
            }
            if seq != s.seq + 1 {
                s.log.push(format!("event gap ({} -> {seq}); bootstrapping again", s.seq));
                self.call(s, "session.bootstrap", json!({"subscribe": true}), |w, s, r| {
                    if let Ok(v) = r {
                        w.on_bootstrap(s, v);
                    }
                });
                return;
            }
            s.seq = seq;
        }
        let d = &v["data"];
        let block = d["block"].as_str().unwrap_or("").to_string();
        match v["event"].as_str().unwrap_or("") {
            "session.changed" => {
                let view = &d["session"];
                let Some(id) = view["id"].as_str() else { return };
                let newer = s.sessions.get(id).is_none_or(|old| old["revision"].as_u64() <= view["revision"].as_u64());
                if newer {
                    s.sessions.insert(id.into(), view.clone());
                    if s.session.as_deref() == Some(id) {
                        self.sync(s);
                    }
                }
            }
            "session.closed" => {
                let Some(id) = d["session"].as_str() else { return };
                s.sessions.remove(id);
                if s.session.as_deref() == Some(id) {
                    let next = s.sessions.keys().next().cloned();
                    s.session = None;
                    match next {
                        Some(n) => self.switch_session(s, n),
                        None => self.sync(s),
                    }
                }
            }
            "terminal.title" => {
                if let Some(p) = s.panes.get_mut(&block) {
                    p.info["title"] = d["title"].clone();
                    refresh_pane_header(p, &s.client_id);
                    self.refresh_tabs(s);
                }
            }
            "terminal.status" => {
                if let Some(p) = s.panes.get_mut(&block) {
                    p.info["status"] = d["records"].clone();
                    p.info["status_summary"] = d["summary"].clone();
                    refresh_pane_header(p, &s.client_id);
                    let summary = d["summary"].as_str().unwrap_or("").to_string();
                    let msg = d["records"].as_array().and_then(|r| r.iter().find_map(|x| x["message"].as_str().map(String::from)));
                    self.refresh_tabs(s);
                    if matches!(summary.as_str(), "blocked" | "error" | "done") {
                        self.notify(s, &block, &summary, msg);
                    }
                }
            }
            "terminal.bell" => {
                if s.panes.contains_key(&block) && !self.pane_visible_and_active(s, &block)
                    && let Some(wid) = s.window_of_block(&block)
                        && let Some(p) = s.pages.get(&wid) {
                            p.page.set_needs_attention(true);
                        }
            }
            "terminal.clipboard" => {
                if s.panes.contains_key(&block) {
                    let text = d["text"].as_str().unwrap_or("");
                    self.ui.window.clipboard().set_text(text);
                    self.toast(s, &format!("{block} copied {} characters to the clipboard", text.chars().count()));
                }
            }
            "terminal.exit" => {
                if let Some(p) = s.panes.get_mut(&block) {
                    let how = match (d["code"].as_i64(), d["signal"].as_i64()) {
                        (Some(c), _) => format!("exited with code {c}"),
                        (_, Some(sig)) => format!("killed by signal {sig}"),
                        _ => "exited".into(),
                    };
                    p.exited = Some(how);
                    refresh_pane_header(p, &s.client_id);
                }
            }
            "terminal.resized" => {
                let sub = d["sub"].as_u64().unwrap_or(0) as u32;
                if let Some(p) = s.subs.get(&sub).and_then(|b| s.panes.get_mut(b)) {
                    p.pty = (d["cols"].as_u64().unwrap_or(0), d["rows"].as_u64().unwrap_or(0));
                    p.owner = d["owner"].as_str().map(String::from);
                    refresh_pane_header(p, &s.client_id);
                }
            }
            "terminal.lost" => {
                let sub = d["sub"].as_u64().unwrap_or(0) as u32;
                if let Some(b) = s.subs.remove(&sub) {
                    s.log.push(format!("{b}: viewer fell behind; re-attaching"));
                    self.attach(s, &b, false);
                }
            }
            "input.ack" => {
                if let Some(seq) = d["seq"].as_u64() {
                    s.unacked.remove(&seq);
                }
            }
            "client.action" => {
                let action = d["action"].as_str().unwrap_or("").to_string();
                let args = if d["args"].is_object() { d["args"].clone() } else { json!({}) };
                self.run_action(s, &action, args);
            }
            "terminal.failed" => {
                let why = d["reason"].as_str().unwrap_or("");
                self.toast(s, &format!("{block} was closed after an internal error: {why}"));
            }
            "config.reloaded" => self.load_config(s),
            "server.stopping" => {
                self.ui.banner.set_title("The server is stopping");
                self.ui.banner.set_revealed(true);
            }
            _ => {}
        }
    }

    fn notify(self: &Rc<Win>, s: &mut State, block: &str, summary: &str, msg: Option<String>) {
        if self.pane_visible_and_active(s, block) {
            return;
        }
        let title = s.panes.get(block).map(|p| pane_title(&p.info, block)).unwrap_or_else(|| block.to_string());
        let head = match summary {
            "blocked" => format!("{title} needs your attention"),
            "error" => format!("{title} reported an error"),
            _ => format!("{title} is done"),
        };
        let n = gio::Notification::new(&head);
        if let Some(m) = &msg {
            n.set_body(Some(m));
        }
        n.set_default_action_and_target_value("app.focus-block", Some(&format!("{}/{block}", self.id).to_variant()));
        if let Some(app) = self.ui.window.application() {
            app.send_notification(Some(&format!("hearth-{block}")), &n);
        }
        s.notified.push(format!("{block}: {head}"));
    }

    fn pane_visible_and_active(&self, s: &State, block: &str) -> bool {
        if !self.ui.window.is_active() {
            return false;
        }
        let Some(wid) = s.window_of_block(block) else { return false };
        let selected = self.ui.tab_view.selected_page().map(|p| p.child());
        s.pages.get(&wid).is_some_and(|p| Some(p.holder.clone().upcast::<gtk::Widget>()) == selected)
    }

    // ------------------------------------------------------------- layout

    fn switch_session(self: &Rc<Win>, s: &mut State, sid: String) {
        self.clear_view(s);
        s.session = Some(sid.clone());
        s.first_show = true;
        if !s.sessions.contains_key(&sid) {
            // Just created: the view may not have arrived as an event yet.
            self.call(s, "session.view", json!({"session": sid}), |w, s, r| {
                if let Ok(v) = r {
                    let id = v["id"].as_str().unwrap_or("").to_string();
                    let newer = s.sessions.get(&id).is_none_or(|old| old["revision"].as_u64() <= v["revision"].as_u64());
                    if newer {
                        s.sessions.insert(id, v);
                    }
                    w.sync(s);
                }
            });
        }
        self.sync(s);
    }

    fn switch_host(self: &Rc<Win>, s: &mut State, host: String) {
        self.clear_view(s);
        s.session = None;
        s.sessions.clear();
        s.host = host;
        s.incarnation.clear();
        s.created_first = false;
        s.conn = Conn::Down { next: Instant::now(), delay: Duration::from_millis(250) };
        self.refresh_title(s);
    }

    /// Detaches every terminal and removes all tabs.
    fn clear_view(self: &Rc<Win>, s: &mut State) {
        let subs: Vec<u32> = s.panes.values().filter_map(|p| p.sub).collect();
        for sub in subs {
            self.call(s, "terminal.detach", json!({"sub": sub}), |_, _, _| {});
        }
        s.panes.clear();
        s.subs.clear();
        self.syncing.set(true);
        for (_, p) in s.pages.drain() {
            self.ui.tab_view.close_page(&p.page);
        }
        self.syncing.set(false);
    }

    /// Makes tabs, layouts and panes match the current session view.
    fn sync(self: &Rc<Win>, s: &mut State) {
        let view = s.session.as_ref().and_then(|id| s.sessions.get(id)).cloned();
        let Some(view) = view else {
            self.ui.stack.set_visible_child_name("empty");
            self.refresh_title(s);
            return;
        };
        self.ui.stack.set_visible_child_name("tabs");
        self.syncing.set(true);
        let windows = view["windows"].as_array().cloned().unwrap_or_default();
        let wanted: HashSet<String> = windows.iter().filter_map(|w| w["id"].as_str().map(String::from)).collect();
        let gone: Vec<String> = s.pages.keys().filter(|k| !wanted.contains(*k)).cloned().collect();
        for k in gone {
            if let Some(p) = s.pages.remove(&k) {
                self.ui.tab_view.close_page(&p.page);
            }
        }
        // Panes: create new ones, update info, drop removed ones.
        let blocks = view["blocks"].as_object().cloned().unwrap_or_default();
        let old: Vec<String> = s.panes.keys().filter(|k| !blocks.contains_key(*k)).cloned().collect();
        for b in old {
            if let Some(p) = s.panes.remove(&b) {
                detach_widget(p.root.upcast_ref());
                if let Some(sub) = p.sub {
                    s.subs.remove(&sub);
                    self.call(s, "terminal.detach", json!({"sub": sub}), |_, _, _| {});
                }
            }
        }
        for (id, info) in &blocks {
            if !s.panes.contains_key(id) {
                let p = self.make_pane(id, s.font_scale);
                s.panes.insert(id.clone(), p);
            }
            let p = s.panes.get_mut(id).unwrap();
            if p.sub.is_none() && !p.attaching {
                // New, or back after a disconnect: attached by the tick once
                // laid out, resuming from what we already hold.
                p.want_attach = true;
            }
            p.info = info.clone();
            if info["exit"].is_object() && p.exited.is_none() {
                p.exited = Some("exited".into());
            }
            refresh_pane_header(p, &s.client_id);
        }
        for (i, w) in windows.iter().enumerate() {
            let wid = w["id"].as_str().unwrap_or("").to_string();
            if !s.pages.contains_key(&wid) {
                let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
                holder.set_hexpand(true);
                holder.set_vexpand(true);
                let page = self.ui.tab_view.insert(&holder, i as i32);
                s.pages.insert(wid.clone(), Page { page, holder, key: String::new(), dividers: vec![] });
            }
            let page = &s.pages[&wid].page;
            if self.ui.tab_view.page_position(page) != i as i32 {
                self.ui.tab_view.reorder_page(page, i as i32);
            }
            let key = layout_key(w);
            if s.pages[&wid].key != key {
                self.rebuild_layout(s, &wid, w, key);
            } else {
                let p = &s.pages[&wid];
                for d in &p.dividers {
                    if let Some(r) = ratio_at(&w["layout"], &d.path)
                        && (d.ratio.get() - r).abs() > 0.005 {
                            d.ratio.set(r);
                            apply_ratio(&d.paned, r);
                        }
                }
            }
        }
        self.syncing.set(false);
        let first = std::mem::replace(&mut s.first_show, false);
        let select = s.select_window.take().or_else(|| first.then(|| view["active_window"].as_str().map(String::from)).flatten());
        if let Some(wid) = select
            && let Some(p) = s.pages.get(&wid) {
                self.syncing.set(true);
                self.ui.tab_view.set_selected_page(&p.page);
                self.syncing.set(false);
                if s.focus_block.is_none() {
                    s.focus_block = s.window_view(&wid).and_then(|w| w["focus"].as_str().map(String::from));
                }
            }
        if let Some(b) = s.focus_block.take() {
            if s.panes.contains_key(&b) {
                self.focus_pane(s, &b);
            } else {
                s.focus_block = Some(b);
            }
        }
        self.refresh_tabs(s);
        self.refresh_title(s);
    }

    fn rebuild_layout(self: &Rc<Win>, s: &mut State, wid: &str, w: &Value, key: String) {
        // Take every pane out of the old tree first, so it can be reparented.
        if let Some(child) = s.pages[wid].holder.first_child() {
            s.pages[wid].holder.remove(&child);
            clear_tree(&child);
        }
        let mut dividers = vec![];
        let node = match w["zoom"].as_str() {
            Some(z) if s.panes.contains_key(z) => json!({"type": "leaf", "block": z}),
            _ => w["layout"].clone(),
        };
        if let Some(widget) = self.build_node(s, wid, &node, String::new(), &mut dividers) {
            s.pages[wid].holder.append(&widget);
        }
        let p = s.pages.get_mut(wid).unwrap();
        p.key = key;
        p.dividers = dividers;
    }

    fn build_node(self: &Rc<Win>, s: &State, wid: &str, node: &Value, path: String, out: &mut Vec<Divider>) -> Option<gtk::Widget> {
        match node["type"].as_str()? {
            "leaf" => {
                let p = s.panes.get(node["block"].as_str()?)?;
                detach_widget(p.root.upcast_ref());
                Some(p.root.clone().upcast())
            }
            _ => {
                let orient = if node["axis"] == "vertical" { gtk::Orientation::Vertical } else { gtk::Orientation::Horizontal };
                let paned = gtk::Paned::new(orient);
                paned.set_wide_handle(true);
                paned.set_resize_start_child(true);
                paned.set_resize_end_child(true);
                paned.set_shrink_start_child(false);
                paned.set_shrink_end_child(false);
                let a = self.build_node(s, wid, &node["a"], format!("{path}a"), out);
                let b = self.build_node(s, wid, &node["b"], format!("{path}b"), out);
                paned.set_start_child(a.as_ref());
                paned.set_end_child(b.as_ref());
                let ratio = Rc::new(Cell::new(node["ratio"].as_f64().unwrap_or(0.5)));
                let programmatic = Rc::new(Cell::new(false));
                // Size changes: keep the ratio, not the pixel position.
                let (r, prog) = (ratio.clone(), programmatic.clone());
                paned.connect_max_position_notify(move |p| {
                    prog.set(true);
                    apply_ratio(p, r.get());
                    prog.set(false);
                });
                // User drags: send the new ratio once the divider settles.
                let (r, prog) = (ratio.clone(), programmatic.clone());
                let timer: Rc<Cell<Option<glib::SourceId>>> = Rc::default();
                let weak = Rc::downgrade(self);
                let (wid, p2) = (wid.to_string(), path.clone());
                paned.connect_position_notify(move |p| {
                    if prog.get() || p.max_position() <= 0 || !p.is_mapped() {
                        return;
                    }
                    let ratio = p.position() as f64 / p.max_position() as f64;
                    if (ratio - r.get()).abs() < 0.002 {
                        return;
                    }
                    r.set(ratio);
                    if let Some(t) = timer.take() {
                        t.remove();
                    }
                    let (weak, wid, path, t2) = (weak.clone(), wid.clone(), p2.clone(), timer.clone());
                    timer.set(Some(glib::timeout_add_local_once(Duration::from_millis(150), move || {
                        t2.set(None);
                        if let Some(w) = weak.upgrade() {
                            w.with(move |w, s| w.call_toast(s, "layout.set_ratio", json!({"window": wid, "path": path, "ratio": ratio})));
                        }
                    })));
                });
                let wrap = programmatic.clone();
                wrap.set(true);
                apply_ratio(&paned, ratio.get());
                wrap.set(false);
                out.push(Divider { paned: paned.clone(), path, ratio });
                Some(paned.upcast())
            }
        }
    }

    fn make_pane(self: &Rc<Win>, block: &str, font_scale: f64) -> Pane {
        let term = vte::Terminal::new();
        term.set_hexpand(true);
        term.set_vexpand(true);
        term.set_scrollback_lines(10_000);
        term.set_allow_hyperlink(true);
        term.set_font_scale(font_scale);
        term.set_input_enabled(false);
        term.set_context_menu_model(Some(&context_menu()));
        apply_colors(&term);
        let title = gtk::Label::new(Some(block));
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_hexpand(true);
        title.set_xalign(0.0);
        let status = gtk::Label::new(None);
        status.add_css_class("hearth-status");
        let note = gtk::Label::new(None);
        note.add_css_class("dim-label");
        note.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.add_css_class("hearth-pane-header");
        header.append(&status);
        header.append(&title);
        header.append(&note);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("hearth-pane");
        root.append(&header);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_child(Some(&term));
        root.append(&scroll);

        let id = block.to_string();
        let weak = Rc::downgrade(self);
        term.connect_commit(move |_, text, _| {
            let Some(w) = weak.upgrade() else { return };
            // Queue first, so keystrokes keep their order even when sending
            // has to wait for the state.
            w.outbox.borrow_mut().push_back((id.clone(), text.as_bytes().to_vec()));
            w.with(|w, s| w.flush_input(s));
        });
        let focus = gtk::EventControllerFocus::new();
        let id = block.to_string();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            let Some(w) = weak.upgrade() else { return };
            let id = id.clone();
            w.with(move |w, s| {
                if s.focused.as_deref() == Some(&id) {
                    return;
                }
                if let Some(old) = s.focused.take().and_then(|o| s.panes.get(&o)) {
                    old.root.remove_css_class("focused");
                }
                if let Some(p) = s.panes.get(&id) {
                    p.root.add_css_class("focused");
                }
                s.focused = Some(id.clone());
                w.call(s, "block.focus", json!({"block": id}), |_, _, _| {});
                w.refresh_title(s);
            });
        });
        term.add_controller(focus);
        // Ctrl+click opens hyperlinks (OSC 8).
        let click = gtk::GestureClick::new();
        let t = term.clone();
        click.connect_released(move |g, _, x, y| {
            if !g.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                return;
            }
            if let Some(uri) = t.check_hyperlink_at(x, y) {
                gtk::UriLauncher::new(&uri).launch(t.root().and_downcast_ref::<gtk::Window>(), gio::Cancellable::NONE, |_| {});
            }
        });
        term.add_controller(click);

        Pane {
            root,
            term,
            title,
            status,
            note,
            sub: None,
            attaching: false,
            want_attach: false,
            fed: (0, 0),
            filter: QueryFilter::new(),
            generation: String::new(),
            offset: 0,
            pty: (0, 0),
            owner: None,
            info: Value::Null,
            exited: None,
            input_unsure: false,
            resize_timer: None,
            sent_size: (0, 0),
        }
    }

    /// Subscribes a pane. `resume` asks for the bytes after what we already have.
    fn attach(self: &Rc<Win>, s: &mut State, block: &str, resume: bool) {
        let Some(p) = s.panes.get_mut(block) else { return };
        if let Some(old) = p.sub.take() {
            s.subs.remove(&old);
        }
        let mut params = json!({"block": block});
        p.attaching = true;
        p.want_attach = false;
        p.sent_size = (0, 0);
        let (cols, rows) = (p.term.column_count(), p.term.row_count());
        if p.term.is_mapped() && cols > 1 && rows > 1 {
            params["cols"] = json!(cols);
            params["rows"] = json!(rows);
            params["cell_w"] = json!(p.term.char_width());
            params["cell_h"] = json!(p.term.char_height());
            p.sent_size = (cols, rows);
        }
        if resume && !p.generation.is_empty() {
            params["resume"] = json!({"generation": p.generation, "offset": p.offset});
        }
        let id = block.to_string();
        self.call(s, "terminal.attach", params, move |w, s, r| {
            if let Some(p) = s.panes.get_mut(&id) {
                p.attaching = false;
            }
            let v = match r {
                Ok(v) => v,
                Err(e) => {
                    s.log.push(format!("{id}: attach failed: {}", e.message));
                    return;
                }
            };
            let sub = v["sub"].as_u64().unwrap_or(0) as u32;
            let Some(p) = s.panes.get_mut(&id) else {
                w.call(s, "terminal.detach", json!({"sub": sub}), |_, _, _| {});
                return;
            };
            p.sub = Some(sub);
            p.generation = v["generation"].as_str().unwrap_or("").into();
            if v["mode"] == "resume" {
                p.offset = v["offset"].as_u64().unwrap_or(p.offset);
            }
            p.pty = (v["cols"].as_u64().unwrap_or(0), v["rows"].as_u64().unwrap_or(0));
            p.owner = v["owner"].as_str().map(String::from);
            p.term.set_input_enabled(true);
            s.subs.insert(sub, id.clone());
            for m in s.orphans.remove(&sub).unwrap_or_default() {
                w.on_incoming(s, m);
            }
            if let Some(p) = s.panes.get_mut(&id) {
                refresh_pane_header(p, &s.client_id);
            }
        });
    }

    fn flush_input(self: &Rc<Win>, s: &mut State) {
        while let Some((block, bytes)) = self.outbox.borrow_mut().pop_front() {
            self.send_input(s, &block, &bytes);
        }
    }

    fn send_input(self: &Rc<Win>, s: &mut State, block: &str, bytes: &[u8]) {
        let Conn::Up(c, _) = &s.conn else { return };
        let Some(p) = s.panes.get_mut(block) else { return };
        let Some(sub) = p.sub else { return };
        s.input_seq += 1;
        if c.send_input(sub, s.input_seq, bytes).is_ok() {
            s.unacked.insert(s.input_seq, block.to_string());
        }
        if p.input_unsure {
            p.input_unsure = false;
            refresh_pane_header(p, &s.client_id);
        }
    }

    fn schedule_resize(self: &Rc<Win>, s: &mut State, block: &str) {
        let Some(p) = s.panes.get_mut(block) else { return };
        if p.resize_timer.is_some() || p.sub.is_none() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = block.to_string();
        p.resize_timer = Some(glib::timeout_add_local_once(Duration::from_millis(40), move || {
            let Some(w) = weak.upgrade() else { return };
            w.with(move |w, s| {
                let Some(p) = s.panes.get_mut(&id) else { return };
                p.resize_timer = None;
                let (Some(sub), cols, rows) = (p.sub, p.term.column_count(), p.term.row_count()) else { return };
                if cols < 2 || rows < 2 || !p.term.is_mapped() {
                    return;
                }
                let params = json!({"sub": sub, "cols": cols, "rows": rows, "cell_w": p.term.char_width(), "cell_h": p.term.char_height()});
                p.sent_size = (cols, rows);
                w.call(s, "terminal.resize", params, |_, _, _| {});
            });
        }));
    }

    pub fn focus_pane(self: &Rc<Win>, s: &mut State, block: &str) {
        let Some(wid) = s.window_of_block(block) else { return };
        if let Some(p) = s.pages.get(&wid)
            && self.ui.tab_view.selected_page().as_ref() != Some(&p.page) {
                self.syncing.set(true);
                self.ui.tab_view.set_selected_page(&p.page);
                self.syncing.set(false);
            }
        if let Some(p) = s.panes.get(block) {
            p.term.grab_focus();
        }
    }

    fn refresh_tabs(&self, s: &State) {
        for (wid, page) in &s.pages {
            let Some(w) = s.window_view(wid) else { continue };
            let mut ids = vec![];
            leaves(&w["layout"], &mut ids);
            let focus = w["focus"].as_str().unwrap_or("");
            let name = w["name"].as_str().unwrap_or("");
            let title = if name.is_empty() {
                s.panes.get(focus).map(|p| pane_title(&p.info, focus)).unwrap_or_else(|| wid.clone())
            } else {
                name.to_string()
            };
            page.page.set_title(&title);
            let rank = |x: &str| match x {
                "blocked" => 5,
                "error" => 4,
                "working" => 3,
                "done" => 2,
                _ => 0,
            };
            let summary = ids
                .iter()
                .filter_map(|b| s.panes.get(b).and_then(|p| p.info["status_summary"].as_str()))
                .max_by_key(|x| rank(x))
                .unwrap_or("");
            page.page.set_loading(summary == "working");
            let icon = match summary {
                "blocked" => Some("dialog-warning-symbolic"),
                "error" => Some("dialog-error-symbolic"),
                "done" => Some("object-select-symbolic"),
                _ => None,
            };
            page.page.set_indicator_icon(icon.map(gio::ThemedIcon::new).as_ref());
            page.page.set_indicator_tooltip(summary);
        }
    }

    pub fn refresh_title(&self, s: &mut State) {
        let name = s.session.as_ref().and_then(|id| s.sessions.get(id)).and_then(|v| v["name"].as_str()).unwrap_or("Hearth");
        self.ui.title.set_title(name);
        self.ui.session_button.set_label(name);
        let mut sub = vec![s.host.clone()];
        match &s.conn {
            Conn::Up(..) => {}
            Conn::Connecting(_) => sub.push("connecting…".into()),
            Conn::Down { .. } => sub.push("disconnected".into()),
            Conn::Closed => {}
        }
        if let Some(k) = s.keymap.describe() {
            sub.push(k);
        }
        self.ui.title.set_subtitle(&sub.join(" · "));
    }

    fn fill_session_list(self: &Rc<Win>, s: &mut State) {
        let list = &self.ui.session_list;
        while let Some(c) = list.first_child() {
            list.remove(&c);
        }
        let row = |name: &str, label: &str, sub: &str| {
            let r = adw::ActionRow::builder().title(label).subtitle(sub).activatable(true).build();
            r.set_widget_name(name);
            list.append(&r);
        };
        for (id, v) in &s.sessions {
            let n = v["windows"].as_array().map_or(0, |w| w.len());
            let mark = if s.session.as_deref() == Some(id.as_str()) { " (current)" } else { "" };
            row(&format!("session:{id}"), &format!("{}{mark}", v["name"].as_str().unwrap_or(id)), &format!("{id} · {n} tab(s)"));
        }
        row("new-session", "New session", &format!("on {}", s.host));
        let mut hosts = vec![("local".to_string(), "This computer".to_string())];
        for h in s.config["hosts"].as_array().into_iter().flatten() {
            if let (Some(n), Some(a)) = (h["name"].as_str(), h["address"].as_str()) {
                hosts.push((a.to_string(), n.to_string()));
            }
        }
        for (addr, name) in hosts {
            if addr != s.host {
                row(&format!("host:{addr}"), &format!("Connect to {name}"), &addr);
            }
        }
    }

    fn confirm_close_window(self: &Rc<Win>, s: &mut State, wid: String) {
        let n = s.window_view(&wid).map(|w| {
            let mut v = vec![];
            leaves(&w["layout"], &mut v);
            v.len()
        });
        let d = adw::AlertDialog::new(Some("Close this tab?"), Some(&format!("The programs in its {} pane(s) will be terminated.", n.unwrap_or(1))));
        d.add_responses(&[("cancel", "Cancel"), ("close", "Close Tab")]);
        d.set_response_appearance("close", adw::ResponseAppearance::Destructive);
        d.set_default_response(Some("cancel"));
        let weak = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            if r != "close" {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            let wid = wid.clone();
            w.with(move |w, s| w.call_toast(s, "window.close", json!({"window": wid})));
        });
        d.present(Some(&self.ui.window));
    }

    // ------------------------------------------------------------ actions

    pub fn focused_block(&self, s: &State) -> Option<String> {
        if let Some(f) = s.focused.clone().filter(|b| s.panes.contains_key(b)) {
            return Some(f);
        }
        let page = self.ui.tab_view.selected_page()?.child();
        let wid = s.window_of_holder(&page)?;
        s.window_view(&wid)?["focus"].as_str().map(String::from)
    }

    fn focused_window(&self, s: &State) -> Option<String> {
        let page = self.ui.tab_view.selected_page()?.child();
        s.window_of_holder(&page)
    }

    fn cwd_of(&self, s: &State, block: &Option<String>) -> Option<String> {
        let info = &s.panes.get(block.as_ref()?)?.info;
        info["cwd"]["reported"].as_str().or(info["cwd"]["process"].as_str()).map(String::from)
    }

    pub fn run_action(self: &Rc<Win>, s: &mut State, name: &str, args: Value) {
        let block = self.focused_block(s);
        let window = self.focused_window(s);
        let cwd = self.cwd_of(s, &block);
        let with_cwd = |mut v: Value| {
            if let Some(c) = &cwd {
                v["cwd"] = json!(c);
            }
            v
        };
        match name {
            "new-tab" => {
                let Some(sid) = s.session.clone() else { return self.run_action(s, "new-session", args) };
                self.call(s, "window.create", with_cwd(json!({"session": sid})), |w, s, r| match r {
                    Ok(ids) => {
                        s.select_window = ids["window"].as_str().map(String::from);
                        s.focus_block = ids["block"].as_str().map(String::from);
                        w.sync(s);
                    }
                    Err(e) => w.toast(s, &format!("new tab: {}", e.message)),
                });
            }
            "new-session" => {
                self.call(s, "session.create", with_cwd(json!({})), |w, s, r| match r {
                    Ok(ids) => {
                        if let Some(sid) = ids["session"].as_str() {
                            w.switch_session(s, sid.into());
                        }
                    }
                    Err(e) => w.toast(s, &format!("new session: {}", e.message)),
                });
            }
            "new-window" => {
                if let Some(app) = self.ui.window.application().and_downcast::<adw::Application>() {
                    let host = s.host.clone();
                    let rc = s.remote_control;
                    glib::idle_add_local_once(move || {
                        let w = Win::new(&app, Options { host, session: None, remote_control: rc });
                        w.with(|w, s| {
                            s.created_first = true;
                            w.run_action(s, "new-session", json!({}));
                        });
                    });
                }
            }
            "split-right" | "split-left" | "split-up" | "split-down" => {
                let Some(b) = block else { return };
                let dir = &name["split-".len()..];
                self.call(s, "block.split", with_cwd(json!({"block": b, "direction": dir})), |w, s, r| match r {
                    Ok(ids) => {
                        s.focus_block = ids["block"].as_str().map(String::from);
                        w.sync(s);
                    }
                    Err(e) => w.toast(s, &format!("split: {}", e.message)),
                });
            }
            "close-pane" => {
                if let Some(b) = block {
                    self.call_toast(s, "block.close", json!({"block": b}));
                }
            }
            "close-tab" => {
                if let Some(wid) = window {
                    self.confirm_close_window(s, wid);
                }
            }
            "focus-left" | "focus-right" | "focus-up" | "focus-down" => {
                let Some(b) = block else { return };
                let dir = &name["focus-".len()..];
                self.call(s, "block.focus", json!({"block": b, "direction": dir}), |w, s, r| {
                    if let Ok(ids) = r
                        && let Some(t) = ids["block"].as_str() {
                            w.focus_pane(s, t);
                        }
                });
            }
            "grow-left" | "grow-right" | "grow-up" | "grow-down" => {
                if let Some(b) = block {
                    let dir = &name["grow-".len()..];
                    self.call_toast(s, "block.resize", json!({"block": b, "direction": dir}));
                }
            }
            "zoom" => {
                if let Some(b) = block {
                    s.focus_block = Some(b.clone());
                    self.call_toast(s, "block.zoom", json!({"block": b}));
                }
            }
            "balance" => {
                if let Some(wid) = window {
                    self.call_toast(s, "layout.balance", json!({"window": wid}));
                }
            }
            "take-size" => {
                if let Some(b) = block {
                    self.call_toast(s, "block.own", json!({"block": b, "mode": "take"}));
                }
            }
            "next-tab" => {
                self.ui.tab_view.select_next_page();
            }
            "prev-tab" => {
                self.ui.tab_view.select_previous_page();
            }
            "rename-tab" => {
                let Some(wid) = window else { return };
                let current = s.window_view(&wid).and_then(|w| w["name"].as_str().map(String::from)).unwrap_or_default();
                match args["name"].as_str() {
                    Some(n) => self.call_toast(s, "window.rename", json!({"window": wid, "name": n})),
                    None => palette::prompt(self, "Rename Tab", &current, move |w, s, n| w.call_toast(s, "window.rename", json!({"window": wid, "name": n}))),
                }
            }
            "rename-session" => {
                let Some(sid) = s.session.clone() else { return };
                let current = s.sessions.get(&sid).and_then(|v| v["name"].as_str().map(String::from)).unwrap_or_default();
                match args["name"].as_str() {
                    Some(n) => self.call_toast(s, "session.rename", json!({"session": sid, "name": n})),
                    None => palette::prompt(self, "Rename Session", &current, move |w, s, n| w.call_toast(s, "session.rename", json!({"session": sid, "name": n}))),
                }
            }
            "copy" => {
                if let Some(p) = block.and_then(|b| s.panes.get(&b)) {
                    p.term.copy_clipboard_format(vte::Format::Text);
                }
            }
            "paste" => {
                if let Some(p) = block.and_then(|b| s.panes.get(&b)) {
                    p.term.paste_clipboard();
                }
            }
            "select-all" => {
                if let Some(p) = block.and_then(|b| s.panes.get(&b)) {
                    p.term.select_all();
                }
            }
            "font-bigger" | "font-smaller" | "font-reset" => {
                s.font_scale = match name {
                    "font-bigger" => (s.font_scale * 1.1).min(4.0),
                    "font-smaller" => (s.font_scale / 1.1).max(0.3),
                    _ => 1.0,
                };
                for p in s.panes.values() {
                    p.term.set_font_scale(s.font_scale);
                }
            }
            "command-palette" => palette::open(self, s, args["query"].as_str().unwrap_or(""), false),
            "switch-session" => palette::open(self, s, "", true),
            "switch-session-to" => {
                let sid = args["session"].as_str().and_then(|w| s.resolve_session(w));
                match sid {
                    Some(sid) => self.switch_session(s, sid),
                    None => self.toast(s, "no such session"),
                }
            }
            "reload-config" => {
                self.call(s, "config.reload", json!({}), |w, s, r| match r {
                    Ok(v) if v["ok"] == true => w.toast(s, "Configuration reloaded"),
                    Ok(v) => w.toast(s, &format!("config: {} (previous configuration kept)", v["error"].as_str().unwrap_or("load failed"))),
                    Err(e) => w.toast(s, &format!("config: {}", e.message)),
                });
            }
            "enter-mode" => {
                let m = args["mode"].as_str().unwrap_or("");
                if !s.keymap.enter_mode(m) {
                    self.toast(s, &format!("no mode named '{m}'"));
                }
                self.refresh_title(s);
            }
            "exit-mode" => {
                s.keymap.mode = None;
                s.keymap.pending.clear();
                self.refresh_title(s);
            }
            "send-text" => {
                if let Some(b) = block {
                    self.call_toast(s, "terminal.send_text", json!({"block": b, "text": args["text"].as_str().unwrap_or(""), "paste": args["paste"].as_bool().unwrap_or(false)}));
                }
            }
            "send-keys" => {
                if let Some(b) = block {
                    self.call_toast(s, "terminal.send_keys", json!({"block": b, "keys": args["keys"].clone()}));
                }
            }
            "reconnect" => {
                if let Conn::Down { .. } = s.conn {
                    s.conn = Conn::Down { next: Instant::now(), delay: Duration::from_millis(250) };
                }
            }
            "toggle-remote-control" => {
                s.remote_control = !s.remote_control;
                let on = s.remote_control;
                if let Some(a) = self.ui.window.lookup_action("remote-control") {
                    a.change_state(&on.to_variant());
                }
                self.call_toast(s, "client.set", json!({"remote_control": on}));
                self.toast(s, if on { "Remote control enabled for this window" } else { "Remote control disabled" });
            }
            "probe" if probe_path().is_some() => {
                s.probe_nonce = args["nonce"].clone();
                self.write_probe(s);
            }
            "test-clipboard" if probe_path().is_some() => {
                let w = self.clone();
                self.ui.window.clipboard().read_text_async(gio::Cancellable::NONE, move |r| {
                    let text = r.ok().flatten().map(|t| t.to_string()).unwrap_or_default();
                    w.with(move |_, s| s.log.push(format!("clipboard={text}")));
                });
            }
            "test-type" if probe_path().is_some() => {
                if let Some(p) = block.and_then(|b| s.panes.get(&b)) {
                    // Through VTE, exactly like a keystroke: emits `commit`.
                    let (term, text) = (p.term.clone(), args["text"].as_str().unwrap_or("").to_string());
                    let delay = Duration::from_millis(args["delay_ms"].as_u64().unwrap_or(0));
                    glib::timeout_add_local_once(delay, move || term.feed_child(text.as_bytes()));
                }
            }
            "test-key" if probe_path().is_some() => {
                let keys = args["keys"].as_str().unwrap_or("").to_string();
                let w = self.clone();
                glib::idle_add_local_once(move || {
                    for c in hearth_core::keys::parse_sequence(&keys).unwrap_or_default() {
                        w.press_chord(c);
                    }
                });
            }
            "test-drag" if probe_path().is_some() => {
                // Moves a divider through the same signal path as a mouse drag.
                let (Some(wid), path) = (window, args["path"].as_str().unwrap_or("")) else { return };
                if let Some(d) = s.pages.get(&wid).and_then(|p| p.dividers.iter().find(|d| d.path == path)) {
                    let pos = (d.paned.max_position() as f64 * args["ratio"].as_f64().unwrap_or(0.5)).round() as i32;
                    d.paned.set_position(pos);
                }
            }
            "test-palette" if probe_path().is_some() => palette::run_query(self, s, args["query"].as_str().unwrap_or("")),
            "test-close-dialog" if probe_path().is_some() => {
                if let Some(d) = self.ui.window.visible_dialog() {
                    d.close();
                }
            }
            other => {
                let def = s.config["actions"].as_array().and_then(|a| a.iter().find(|x| x["name"] == other)).cloned();
                match def {
                    Some(def) => palette::run_config_action(self, s, &def, args),
                    None => self.toast(s, &format!("unknown action '{other}'")),
                }
            }
        }
    }

    fn write_probe(&self, s: &mut State) {
        let Some(path) = probe_path() else { return };
        s.probe_at = Some(Instant::now());
        let mut panes = serde_json::Map::new();
        for (id, p) in &s.panes {
            let text = p.term.text_format(vte::Format::Text).map(|g| g.to_string()).unwrap_or_default();
            let (cc, cr) = p.term.cursor_position();
            panes.insert(
                id.clone(),
                json!({
                    "sub": p.sub, "cols": p.term.column_count(), "rows": p.term.row_count(),
                    "pty": [p.pty.0, p.pty.1], "owner": p.owner, "offset": p.offset,
                    "cursor": [cr, cc], "text": text, "mapped": p.term.is_mapped(),
                    "focused": p.term.has_focus(), "input_enabled": p.term.is_input_enabled(),
                    "title": p.title.text().to_string(), "status": p.status.text().to_string(), "note": p.note.text().to_string(),
                    "font_scale": p.term.font_scale(), "fed": [p.fed.0, p.fed.1],
                }),
            );
        }
        let tabs: Vec<Value> = (0..self.ui.tab_view.n_pages())
            .map(|i| {
                let page = self.ui.tab_view.nth_page(i);
                let wid = s.window_of_holder(&page.child()).unwrap_or_default();
                let dividers: Vec<Value> = s.pages.get(&wid).map(|p| p.dividers.iter().map(|d| json!({"path": d.path, "ratio": d.ratio.get(), "position": d.paned.position(), "max": d.paned.max_position()})).collect()).unwrap_or_default();
                json!({
                    "window": wid, "title": page.title().to_string(), "selected": page.is_selected(),
                    "loading": page.is_loading(), "needs_attention": page.needs_attention(),
                    "indicator": page.indicator_tooltip().to_string(), "dividers": dividers,
                })
            })
            .collect();
        let state = json!({
            "nonce": s.probe_nonce,
            "written_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
            "connected": matches!(s.conn, Conn::Up(..)),
            "host": s.host, "client_id": s.client_id, "session": s.session,
            "sessions": s.sessions.keys().collect::<Vec<_>>(),
            "focused": self.focused_block(s), "tabs": tabs, "panes": panes,
            "banner": if self.ui.banner.is_revealed() { Some(self.ui.banner.title().to_string()) } else { None },
            "mode": s.keymap.mode, "keys": s.keymap.describe(), "subtitle": self.ui.title.subtitle().to_string(),
            "palette": palette::visible_items(), "log": s.log, "notified": s.notified,
            "remote_control": s.remote_control, "unacked": s.unacked.len(),
            "stack": self.ui.stack.visible_child_name().map(|n| n.to_string()),
            "clipboard_note": s.log.iter().rev().find(|l| l.contains("clipboard")),
        });
        let tmp = format!("{path}.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&state).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

impl State {
    fn window_view(&self, wid: &str) -> Option<&Value> {
        let v = self.sessions.get(self.session.as_ref()?)?;
        v["windows"].as_array()?.iter().find(|w| w["id"] == wid)
    }

    fn window_of_block(&self, block: &str) -> Option<String> {
        let v = self.sessions.get(self.session.as_ref()?)?;
        v["windows"].as_array()?.iter().find_map(|w| {
            let mut ids = vec![];
            leaves(&w["layout"], &mut ids);
            ids.iter().any(|b| b == block).then(|| w["id"].as_str().unwrap_or("").to_string())
        })
    }

    fn window_of_holder(&self, w: &gtk::Widget) -> Option<String> {
        self.pages.iter().find(|(_, p)| p.holder.upcast_ref::<gtk::Widget>() == w).map(|(k, _)| k.clone())
    }

    fn resolve_session(&self, want: &str) -> Option<String> {
        if self.sessions.contains_key(want) {
            return Some(want.into());
        }
        let m: Vec<&String> = self.sessions.iter().filter(|(_, v)| v["name"] == want).map(|(k, _)| k).collect();
        (m.len() == 1).then(|| m[0].clone())
    }

    pub fn sessions(&self) -> &BTreeMap<String, Value> {
        &self.sessions
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub fn config(&self) -> &Value {
        &self.config
    }
}

fn leaves(node: &Value, out: &mut Vec<String>) {
    match node["type"].as_str() {
        Some("leaf") => out.extend(node["block"].as_str().map(String::from)),
        Some(_) => {
            leaves(&node["a"], out);
            leaves(&node["b"], out);
        }
        None => {}
    }
}

/// Tree shape, blocks and zoom; ratios are left out so a drag does not rebuild.
fn layout_key(w: &Value) -> String {
    fn shape(n: &Value, out: &mut String) {
        match n["type"].as_str() {
            Some("leaf") => out.push_str(n["block"].as_str().unwrap_or("?")),
            Some(_) => {
                out.push_str(if n["axis"] == "vertical" { "V(" } else { "H(" });
                shape(&n["a"], out);
                out.push(',');
                shape(&n["b"], out);
                out.push(')');
            }
            None => {}
        }
    }
    let mut k = String::new();
    shape(&w["layout"], &mut k);
    k.push_str(&format!("|z={}", w["zoom"].as_str().unwrap_or("")));
    k
}

fn ratio_at(node: &Value, path: &str) -> Option<f64> {
    let mut n = node;
    for c in path.chars() {
        n = &n[if c == 'a' { "a" } else { "b" }];
    }
    n["ratio"].as_f64()
}

fn apply_ratio(p: &gtk::Paned, ratio: f64) {
    let max = p.max_position();
    if max > 0 {
        p.set_position((max as f64 * ratio).round() as i32);
    }
}

/// Removes a widget from a Paned or Box parent.
fn detach_widget(w: &gtk::Widget) {
    let Some(parent) = w.parent() else { return };
    if let Some(p) = parent.downcast_ref::<gtk::Paned>() {
        if p.start_child().as_ref() == Some(w) {
            p.set_start_child(gtk::Widget::NONE);
        } else {
            p.set_end_child(gtk::Widget::NONE);
        }
    } else if let Some(b) = parent.downcast_ref::<gtk::Box>() {
        b.remove(w);
    }
}

/// Empties a tree of Paneds so its panes are free to be placed again.
fn clear_tree(w: &gtk::Widget) {
    if let Some(p) = w.downcast_ref::<gtk::Paned>() {
        for c in [p.start_child(), p.end_child()].into_iter().flatten() {
            clear_tree(&c);
        }
        p.set_start_child(gtk::Widget::NONE);
        p.set_end_child(gtk::Widget::NONE);
    }
}

fn pane_title(info: &Value, block: &str) -> String {
    match info["title"].as_str().filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => info["process"]["name"].as_str().map(String::from).unwrap_or_else(|| block.to_string()),
    }
}

fn refresh_pane_header(p: &mut Pane, client_id: &str) {
    let block = p.info["id"].as_str().unwrap_or("").to_string();
    p.title.set_text(&pane_title(&p.info, &block));
    let summary = p.info["status_summary"].as_str().unwrap_or("");
    let label = match summary {
        "blocked" => "● blocked",
        "error" => "● error",
        "working" => "● working",
        "done" => "● done",
        "idle" => "● idle",
        _ => "",
    };
    for c in ["blocked", "error", "working", "done", "idle"] {
        p.status.remove_css_class(c);
    }
    if !summary.is_empty() {
        p.status.add_css_class(summary);
    }
    p.status.set_text(label);
    let mut notes = vec![];
    if p.sub.is_none() && !p.want_attach && !p.attaching {
        notes.push("not connected".to_string());
    }
    if p.input_unsure {
        notes.push("⚠ input before the disconnect was not confirmed".into());
    }
    if let Some(e) = &p.exited {
        notes.push(e.clone());
    }
    let (cols, rows) = (p.term.column_count() as u64, p.term.row_count() as u64);
    let other_owner = p.owner.as_deref().is_some_and(|o| o != client_id);
    if p.sub.is_some() && other_owner && p.pty.0 > 0 && (p.pty.0, p.pty.1) != (cols, rows) {
        notes.push(format!("sized {}×{} by another viewer", p.pty.0, p.pty.1));
    }
    p.note.set_text(&notes.join(" · "));
}

/// Terminal colors follow the desktop's light/dark style.
fn apply_colors(term: &vte::Terminal) {
    let dark = adw::StyleManager::default().is_dark();
    let (fg, bg) = if dark { ("#ffffff", "#1e1e1e") } else { ("#1e1e1e", "#ffffff") };
    let rgba = |s: &str| gdk::RGBA::parse(s).unwrap();
    term.set_colors(Some(&rgba(fg)), Some(&rgba(bg)), &[]);
}

fn context_menu() -> gio::Menu {
    let m = gio::Menu::new();
    let edit = gio::Menu::new();
    edit.append(Some("Copy"), Some("win.copy"));
    edit.append(Some("Paste"), Some("win.paste"));
    edit.append(Some("Select All"), Some("win.select-all"));
    m.append_section(None, &edit);
    let pane = gio::Menu::new();
    pane.append(Some("Split Right"), Some("win.split-right"));
    pane.append(Some("Split Down"), Some("win.split-down"));
    pane.append(Some("Zoom"), Some("win.zoom"));
    pane.append(Some("Take Size"), Some("win.take-size"));
    pane.append(Some("Close Pane"), Some("win.close-pane"));
    m.append_section(None, &pane);
    m
}

/// A GDK key event as a chord, or None for bare modifiers.
pub fn chord_from(key: gdk::Key, mods: gdk::ModifierType) -> Option<Chord> {
    use gdk::Key as K;
    let lower = key.to_lower();
    let named = match lower {
        K::Return | K::KP_Enter => "return",
        K::Tab | K::ISO_Left_Tab => "tab",
        K::Escape => "escape",
        K::space => "space",
        K::BackSpace => "backspace",
        K::Delete | K::KP_Delete => "delete",
        K::Insert => "insert",
        K::Home => "home",
        K::End => "end",
        K::Page_Up => "pageup",
        K::Page_Down => "pagedown",
        K::Left => "left",
        K::Right => "right",
        K::Up => "up",
        K::Down => "down",
        K::plus | K::KP_Add => "plus",
        K::minus | K::KP_Subtract => "minus",
        K::comma => "comma",
        K::period => "period",
        K::F1 => "f1",
        K::F2 => "f2",
        K::F3 => "f3",
        K::F4 => "f4",
        K::F5 => "f5",
        K::F6 => "f6",
        K::F7 => "f7",
        K::F8 => "f8",
        K::F9 => "f9",
        K::F10 => "f10",
        K::F11 => "f11",
        K::F12 => "f12",
        _ => "",
    };
    let (key, letter) = if !named.is_empty() {
        (named.to_string(), false)
    } else {
        let c = lower.to_unicode().filter(|c| !c.is_control())?;
        (c.to_string(), c.is_alphabetic())
    };
    // Shift that produced a symbol (`+`, `?`) is part of the key, not a modifier.
    let shift_is_key = !letter && (key.chars().count() == 1 || key == "plus");
    Some(Chord {
        ctrl: mods.contains(gdk::ModifierType::CONTROL_MASK),
        shift: mods.contains(gdk::ModifierType::SHIFT_MASK) && !shift_is_key,
        alt: mods.contains(gdk::ModifierType::ALT_MASK),
        sup: mods.contains(gdk::ModifierType::SUPER_MASK),
        key,
    })
}

fn build_ui(app: &adw::Application) -> Ui {
    let window = adw::ApplicationWindow::builder().application(app).default_width(1100).default_height(720).title("Hearth").build();
    let tab_view = adw::TabView::new();
    let tab_bar = adw::TabBar::builder().view(&tab_view).autohide(false).build();
    let title = adw::WindowTitle::new("Hearth", "");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let session_list = gtk::ListBox::new();
    session_list.add_css_class("boxed-list");
    session_list.set_selection_mode(gtk::SelectionMode::None);
    let pop = gtk::Popover::new();
    let sc = gtk::ScrolledWindow::builder().propagate_natural_height(true).max_content_height(500).min_content_width(320).child(&session_list).build();
    pop.set_child(Some(&sc));
    let session_button = gtk::MenuButton::builder().label("Hearth").popover(&pop).tooltip_text("Sessions and hosts").build();
    header.pack_start(&session_button);
    let new_tab = gtk::Button::from_icon_name("tab-new-symbolic");
    new_tab.set_action_name(Some("win.new-tab"));
    new_tab.set_tooltip_text(Some("New Tab"));
    header.pack_start(&new_tab);

    let menu = gio::Menu::new();
    let s1 = gio::Menu::new();
    s1.append(Some("New Window"), Some("win.new-window"));
    s1.append(Some("New Session"), Some("win.new-session"));
    s1.append(Some("Rename Session…"), Some("win.rename-session"));
    s1.append(Some("Rename Tab…"), Some("win.rename-tab"));
    menu.append_section(None, &s1);
    let s2 = gio::Menu::new();
    s2.append(Some("Command Palette"), Some("win.command-palette"));
    s2.append(Some("Reload Configuration"), Some("win.reload-config"));
    s2.append(Some("Allow Remote Control"), Some("win.remote-control"));
    menu.append_section(None, &s2);
    let main_menu = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Main Menu").build();
    header.pack_end(&main_menu);

    let banner = adw::Banner::new("");
    let empty = adw::StatusPage::builder().icon_name("utilities-terminal-symbolic").title("No session").description("Every session on this host is closed.").build();
    let new_btn = gtk::Button::with_label("New Session");
    new_btn.add_css_class("pill");
    new_btn.add_css_class("suggested-action");
    new_btn.set_halign(gtk::Align::Center);
    new_btn.set_action_name(Some("win.new-session"));
    empty.set_child(Some(&new_btn));
    let tabs_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    tabs_box.append(&tab_bar);
    tab_view.set_vexpand(true);
    tabs_box.append(&tab_view);
    let stack = gtk::Stack::new();
    stack.add_named(&tabs_box, Some("tabs"));
    stack.add_named(&empty, Some("empty"));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    stack.set_vexpand(true);
    content.append(&stack);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&toasts));
    window.set_content(Some(&tv));
    Ui { window, tab_view, banner, toasts, title, stack, session_button, session_list }
}
