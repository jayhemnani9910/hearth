//! GUI tests: the real hearth-app, headless on a GTK Broadway display,
//! against a real hearthd. The app is driven over remote control
//! (`client.action`) and inspected through its probe file
//! (`HEARTH_APP_PROBE`), which reads the live VTE widgets.
//!
//! Needs `gtk4-broadwayd` (package libgtk-4-bin) and `cargo build` first.

use hearth_core::conn::{Client, Incoming, Target};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

static N: AtomicU32 = AtomicU32::new(0);

fn bin(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_BIN_EXE_hearth-app")).with_file_name(name);
    assert!(p.exists(), "{} missing: run `cargo build` before the tests", p.display());
    p
}

fn kill(c: &mut Child) {
    let _ = c.kill();
    let _ = c.wait();
}

struct Env {
    dir: PathBuf,
    display: u32,
    broadway: Child,
    daemon: Option<Child>,
    app: Option<Child>,
    nonce: u64,
}

impl Env {
    fn new(config: Option<&str>) -> Env {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
        let dir = PathBuf::from(base).join(format!("hgui-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cfg")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        if let Some(c) = config {
            std::fs::write(dir.join("cfg/config.lua"), c).unwrap();
        }
        let display = 40 + (std::process::id() % 100) * 8 + n;
        let broadway = Command::new("gtk4-broadwayd")
            .arg(format!(":{display}"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("broadway.log")).unwrap())
            .spawn()
            .expect("gtk4-broadwayd not found (install libgtk-4-bin)");
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", 8080 + display as u16)).is_err() {
            assert!(Instant::now() < deadline, "broadwayd :{display} did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Env { dir, display, broadway, daemon: None, app: None, nonce: 0 }
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("s.sock")
    }

    fn envs(&self) -> Vec<(String, String)> {
        let d = |p: &str| self.dir.join(p).display().to_string();
        vec![
            ("HEARTH_RUNTIME_DIR".into(), self.dir.display().to_string()),
            ("HEARTH_SOCKET".into(), d("s.sock")),
            ("HEARTH_STATE_DIR".into(), d("state")),
            ("HEARTH_CONFIG_DIR".into(), d("cfg")),
            ("HEARTH_APP_PROBE".into(), d("probe.json")),
            ("GDK_BACKEND".into(), "broadway".into()),
            ("BROADWAY_DISPLAY".into(), format!(":{}", self.display)),
            // Shells must not depend on the user's dotfiles.
            ("SHELL".into(), "/bin/bash".into()),
            ("HOME".into(), d("home")),
            ("PATH".into(), format!("{}:{}", bin("hearthd").parent().unwrap().display(), std::env::var("PATH").unwrap_or_default())),
        ]
    }

    fn start_daemon(&mut self) {
        let mut cmd = Command::new(bin("hearthd"));
        cmd.arg("--no-persist").stdin(Stdio::null()).stdout(Stdio::null()).stderr(std::fs::File::create(self.dir.join("daemon.log")).unwrap());
        cmd.envs(self.envs());
        self.daemon = Some(cmd.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(self.sock()).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn start_app(&mut self, args: &[&str]) {
        let _ = std::fs::remove_file(self.dir.join("probe.json"));
        let mut cmd = Command::new(bin("hearth-app"));
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(std::fs::File::create(self.dir.join("app.log")).unwrap());
        cmd.envs(self.envs());
        self.app = Some(cmd.spawn().unwrap());
    }

    fn client(&self) -> (Client, Receiver<Incoming>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Client::connect(&Target::Unix(self.sock()), "cli", "gui-test") {
                Ok(c) => return c,
                Err(e) => {
                    assert!(Instant::now() < deadline, "cannot connect: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Sends a GUI action; returns how many apps received it.
    fn act(&self, c: &Client, action: &str, args: Value) -> u64 {
        let r = c.call("client.action", json!({"action": action, "args": args})).unwrap();
        r["delivered"].as_u64().unwrap()
    }

    /// The app's state after everything sent so far. While the app is
    /// disconnected (no remote control), a state written after this call.
    fn probe(&mut self, c: &Client) -> Value {
        self.nonce += 1;
        let n = self.nonce;
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let via_server = c.call("client.action", json!({"action": "probe", "args": {"nonce": n}})).is_ok();
        let path = self.dir.join("probe.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                let fresh = if via_server { v["nonce"] == n } else { v["written_ms"].as_u64().unwrap_or(0) > since };
                if fresh {
                    return v;
                }
            }
            assert!(Instant::now() < deadline, "no probe answer; app log:\n{}", self.app_log());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Probes until `f` accepts the app state.
    fn wait<T>(&mut self, c: &Client, what: &str, f: impl Fn(&Value) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v = self.probe(c);
            if let Some(t) = f(&v) {
                return t;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; last probe:\n{}\napp log:\n{}", serde_json::to_string_pretty(&v).unwrap(), self.app_log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits until the app has connected and enabled remote control.
    fn wait_app(&self, c: &Client) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let list = c.call("client.list", json!({})).unwrap();
            if list.as_array().unwrap().iter().any(|x| x["kind"] == "app" && x["remote_control"] == true) {
                return;
            }
            assert!(Instant::now() < deadline, "app did not connect; log:\n{}", self.app_log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn app_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("app.log")).unwrap_or_default()
    }

    fn stop_app(&mut self) {
        if let Some(mut a) = self.app.take() {
            kill(&mut a);
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_app();
        if let Some(mut d) = self.daemon.take() {
            kill(&mut d);
        } else if let Ok(c) = Client::connect(&Target::Unix(self.sock()), "cli", "gui-test-cleanup") {
            // An autostarted server: stop it and its shells.
            let _ = c.0.call("server.stop", json!({"force": true}));
        }
        kill(&mut self.broadway);
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn call(c: &Client, m: &str, p: Value) -> Value {
    c.call(m, p).unwrap_or_else(|e| panic!("{m}: {}", e.message))
}

fn view(c: &Client) -> Value {
    call(c, "session.list", json!({}))
}

fn first_session(c: &Client) -> Value {
    let list = view(c);
    let id = list[0]["id"].as_str().expect("a session").to_string();
    call(c, "session.view", json!({"session": id}))
}

fn leaves(n: &Value, out: &mut Vec<String>) {
    if n["type"] == "leaf" {
        out.push(n["block"].as_str().unwrap().into());
    } else {
        leaves(&n["a"], out);
        leaves(&n["b"], out);
    }
}

/// Screen rows without trailing blanks, without trailing empty rows.
fn norm(s: &str) -> String {
    let mut v: Vec<&str> = s.lines().map(|l| l.trim_end()).collect();
    while v.last() == Some(&"") {
        v.pop();
    }
    v.join("\n")
}

/// The server's screen text, soft-wrapped rows joined like VTE's.
fn server_text(c: &Client, block: &str) -> String {
    norm(call(c, "terminal.capture", json!({"block": block, "join": true}))["content"].as_str().unwrap())
}

/// VTE's screen text; soft-wrapped rows come back joined.
fn pane_text(p: &Value, block: &str) -> String {
    norm(p["panes"][block]["text"].as_str().unwrap_or(""))
}

/// Types a line once the shell has drawn its prompt (readline discards
/// input that arrives while it sets up the terminal).
fn send(c: &Client, block: &str, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while server_text(c, block).is_empty() {
        assert!(Instant::now() < deadline, "{block} never drew a prompt");
        std::thread::sleep(Duration::from_millis(50));
    }
    call(c, "terminal.send_text", json!({"block": block, "text": format!("{text}\r")}));
}

fn wait_server_text(c: &Client, block: &str, needle: &str) {
    let r = c.call("terminal.wait_text", json!({"block": block, "text": needle, "timeout_ms": 10000}));
    assert!(r.is_ok(), "server never showed {needle:?} in {block}: {}", server_text(c, block));
}

/// The VTE widget shows exactly what the server's model holds.
fn assert_same_screen(e: &mut Env, c: &Client, block: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = pane_text(&e.probe(c), block);
        let want = server_text(c, block);
        if got == want {
            return;
        }
        assert!(Instant::now() < deadline, "VTE screen of {block} differs from the server\nserver: {want:?}\nVTE:    {got:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_file(p: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !p.exists() {
        assert!(Instant::now() < deadline, "{} never appeared", p.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn gui_p01_first_launch_starts_a_server_and_gives_a_working_terminal() {
    let mut e = Env::new(None);
    // No server running and no config: the app must start one itself.
    e.start_app(&["--remote-control"]);
    wait_for_file(&e.sock());
    let (c, _rx) = e.client();
    e.wait_app(&c);
    let p = e.wait(&c, "one attached, focused pane", |p| {
        let panes = p["panes"].as_object()?;
        (panes.len() == 1 && p["stack"] == "tabs").then(|| panes.keys().next().unwrap().clone()).filter(|b| p["panes"][b]["sub"].is_u64() && p["panes"][b]["focused"] == true)
    });
    let b = p;
    // Typing goes through VTE's commit signal, like a real keystroke.
    e.act(&c, "test-type", json!({"text": "echo first-$((40+2))\r"}));
    wait_server_text(&c, &b, "first-42");
    e.wait(&c, "output in the VTE widget", |p| pane_text(p, &b).contains("first-42").then_some(()));
    let s = first_session(&c);
    assert_eq!(s["windows"].as_array().unwrap().len(), 1);
}

#[test]
fn gui_p02_p03_programs_survive_the_app_and_vte_matches_the_server() {
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, _rx) = e.client();
    let ids = call(&c, "session.create", json!({"name": "work"}));
    let b = ids["block"].as_str().unwrap().to_string();
    let pid = call(&c, "block.info", json!({"block": b}))["pid"].clone();
    e.start_app(&["--remote-control", "--session", "work"]);
    e.wait_app(&c);
    e.wait(&c, "pane attached", |p| p["panes"][&b]["sub"].is_u64().then_some(()));
    // Colors, wide characters, a hyperlink and a scroll region.
    send(&c, &b, "clear; printf '\\033[1;31mred\\033[0m wide:日本 \\033]8;;https://example.com\\033\\\\link\\033]8;;\\033\\\\\\n'; for i in 1 2 3; do echo line-$i; done; echo M$((1+0))");
    wait_server_text(&c, &b, "M1");
    assert_same_screen(&mut e, &c, &b);

    // Kill the app outright; the shell must not notice.
    e.stop_app();
    send(&c, &b, "echo while-away-$((2+3))");
    wait_server_text(&c, &b, "while-away-5");
    assert_eq!(call(&c, "block.info", json!({"block": b}))["pid"], pid);

    // A new app restores the exact screen from a snapshot.
    e.start_app(&["--remote-control", "--session", "work"]);
    e.wait_app(&c);
    assert_same_screen(&mut e, &c, &b);

    // Alternate screen with the primary screen kept underneath.
    send(&c, &b, "printf '\\033[?1049h\\033[H\\033[2Jfull-screen-app\\033[5;3Hat-5-3'; sleep 30");
    wait_server_text(&c, &b, "at-5-3");
    e.stop_app();
    e.start_app(&["--remote-control", "--session", "work"]);
    e.wait_app(&c);
    assert_same_screen(&mut e, &c, &b);
    call(&c, "terminal.send_keys", json!({"block": b, "keys": ["C-c"]}));
    send(&c, &b, "printf '\\033[?1049l'; echo back-$((1+1))");
    wait_server_text(&c, &b, "back-2");
    assert_same_screen(&mut e, &c, &b);
    assert!(server_text(&c, &b).contains("while-away-5"), "primary screen came back");
}

#[test]
fn gui_p04_p05_tabs_splits_zoom_and_divider_drag() {
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, _rx) = e.client();
    call(&c, "session.create", json!({"name": "s"}));
    e.start_app(&["--remote-control"]);
    e.wait_app(&c);
    e.wait(&c, "first pane", |p| (p["panes"].as_object()?.len() == 1).then_some(()));

    e.act(&c, "split-right", json!({}));
    e.wait(&c, "focus on the new pane", |p| (p["panes"].as_object()?.len() == 2 && p["focused"] != p["panes"].as_object()?.keys().min().unwrap().as_str()).then_some(()));
    e.act(&c, "split-down", json!({}));
    // Each pane's PTY gets the size of its widget.
    let p = e.wait(&c, "three panes attached and sized", |p| {
        let panes = p["panes"].as_object()?;
        let ok = panes.len() == 3 && panes.values().all(|x| x["sub"].is_u64() && x["mapped"] == true && x["pty"] == json!([x["cols"], x["rows"]]));
        (ok && p["focused"].is_string()).then(|| p.clone())
    });
    let s = first_session(&c);
    let w = &s["windows"][0];
    let mut ids = vec![];
    leaves(&w["layout"], &mut ids);
    assert_eq!(ids.len(), 3);
    assert_eq!(w["layout"]["axis"], "horizontal");
    assert_eq!(w["layout"]["b"]["axis"], "vertical");
    // The split-off pane has focus, here and on the server.
    let focused = p["focused"].as_str().unwrap().to_string();
    assert_eq!(focused, ids[2]);
    assert_eq!(w["focus"], ids[2].as_str());

    // Dragging the root divider is sent to the server as a ratio.
    let wid = w["id"].as_str().unwrap().to_string();
    e.act(&c, "test-drag", json!({"path": "", "ratio": 0.3}));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let r = first_session(&c)["windows"][0]["layout"]["ratio"].as_f64().unwrap();
        if (r - 0.3).abs() < 0.02 {
            break;
        }
        assert!(Instant::now() < deadline, "server ratio is {r}, wanted ~0.3");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A ratio set elsewhere moves our divider, and resizes the PTYs.
    call(&c, "layout.set_ratio", json!({"window": wid, "path": "", "ratio": 0.7}));
    e.wait(&c, "divider at 0.7", |p| {
        let d = p["tabs"][0]["dividers"].as_array()?.iter().find(|d| d["path"] == "")?.clone();
        let pos = d["position"].as_f64()? / d["max"].as_f64()?;
        ((pos - 0.7).abs() < 0.02).then_some(())
    });
    e.wait(&c, "left PTY follows the wider pane", |p| {
        let pp = &p["panes"][&ids[0]];
        (pp["pty"] == json!([pp["cols"], pp["rows"]]) && pp["cols"].as_u64()? > 70).then_some(())
    });

    // Zoom shows only the focused pane; unzoom restores the split.
    e.act(&c, "zoom", json!({}));
    e.wait(&c, "zoomed", |p| (p["panes"][&ids[2]]["mapped"] == true && p["panes"][&ids[0]]["mapped"] == false).then_some(()));
    e.act(&c, "zoom", json!({}));
    e.wait(&c, "unzoomed", |p| p["panes"].as_object()?.values().all(|x| x["mapped"] == true).then_some(()));

    // Tabs: new, reorder from another client, close from another client.
    e.act(&c, "new-tab", json!({}));
    let p = e.wait(&c, "two tabs, the new one selected", |p| {
        let t = p["tabs"].as_array()?;
        (t.len() == 2 && t[1]["selected"] == true).then(|| p.clone())
    });
    let new_wid = p["tabs"][1]["window"].as_str().unwrap().to_string();
    call(&c, "window.move", json!({"window": new_wid, "index": 0}));
    e.wait(&c, "tab order follows the server", |p| (p["tabs"][0]["window"] == new_wid.as_str()).then_some(()));
    call(&c, "window.rename", json!({"window": new_wid, "name": "logs"}));
    e.wait(&c, "tab title follows a rename", |p| (p["tabs"][0]["title"] == "logs").then_some(()));
    call(&c, "window.close", json!({"window": new_wid}));
    e.wait(&c, "tab removed", |p| (p["tabs"].as_array()?.len() == 1).then_some(()));

    // Closing a pane from the app.
    e.act(&c, "close-pane", json!({}));
    e.wait(&c, "two panes left", |p| (p["panes"].as_object()?.len() == 2).then_some(()));
    let mut left = vec![];
    leaves(&first_session(&c)["windows"][0]["layout"], &mut left);
    assert_eq!(left.len(), 2);
}

#[test]
fn gui_p07_size_ownership_between_app_and_another_viewer() {
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, rx) = e.client();
    let ids = call(&c, "session.create", json!({}));
    let b = ids["block"].as_str().unwrap().to_string();
    e.start_app(&["--remote-control"]);
    e.wait_app(&c);
    let app_size = e.wait(&c, "app owns the size", |p| {
        let pp = &p["panes"][&b];
        (pp["owner"].is_string() && pp["pty"] == json!([pp["cols"], pp["rows"]])).then(|| pp["pty"].clone())
    });
    // Another viewer takes the size.
    let sub = call(&c, "terminal.attach", json!({"block": b, "cols": 50, "rows": 12}))["sub"].clone();
    call(&c, "block.own", json!({"block": b, "mode": "take"}));
    call(&c, "terminal.resize", json!({"sub": sub, "cols": 50, "rows": 12}));
    let note = e.wait(&c, "app shows who sized it", |p| {
        let pp = &p["panes"][&b];
        (pp["pty"] == json!([50, 12])).then(|| pp["note"].as_str().unwrap_or("").to_string())
    });
    assert!(note.contains("sized 50×12 by another viewer"), "note: {note}");
    // Typing in the app takes the size back.
    e.act(&c, "test-type", json!({"text": "x"}));
    e.wait(&c, "app owns the size again", |p| (p["panes"][&b]["pty"] == app_size && p["panes"][&b]["note"] == "").then_some(()));
    drop(rx);
}

#[test]
fn gui_p10_reconnect_resumes_and_reports_unconfirmed_input() {
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, _rx) = e.client();
    let ids = call(&c, "session.create", json!({}));
    let b = ids["block"].as_str().unwrap().to_string();
    // Connect through a proxy process so the link can be cut while the server lives.
    let host = format!("cmd:{} proxy", bin("hearth").display());
    e.start_app(&["--remote-control", "--host", &host]);
    e.wait_app(&c);
    e.wait(&c, "attached", |p| p["panes"][&b]["sub"].is_u64().then_some(()));
    let app_pid = e.app.as_ref().unwrap().id();
    let proxy = || -> i32 {
        let out = Command::new("pgrep").args(["-P", &app_pid.to_string(), "-x", "hearth"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().expect("proxy process")
    };

    // 1. Plain link loss: output produced meanwhile arrives after resume.
    let px = proxy();
    unsafe { libc::kill(px, libc::SIGKILL) };
    let banner = e.wait(&c, "disconnect banner", |p| p["banner"].as_str().map(String::from));
    assert!(banner.contains("Reconnecting"), "{banner}");
    send(&c, &b, "echo during-outage-$((3*3))");
    wait_server_text(&c, &b, "during-outage-9");
    e.wait(&c, "reconnected", |p| (p["connected"] == true && p["banner"].is_null() && p["panes"][&b]["sub"].is_u64()).then_some(()));
    assert_same_screen(&mut e, &c, &b);
    assert!(pane_text(&e.probe(&c), &b).contains("during-outage-9"));

    // 2. Input that was never acknowledged is reported, not resent.
    // The typing is scheduled in the app before the link freezes, and
    // happens while it is frozen, so no acknowledgement can come back.
    let px = proxy();
    e.act(&c, "test-type", json!({"text": "echo lost-input\r", "delay_ms": 600}));
    std::thread::sleep(Duration::from_millis(150));
    unsafe { libc::kill(px, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_millis(900));
    unsafe { libc::kill(px, libc::SIGKILL) };
    let banner = e.wait(&c, "unconfirmed-input banner", |p| p["banner"].as_str().filter(|b| b.contains("not confirmed")).map(String::from));
    assert!(banner.contains(&b), "{banner}");
    let note = e.wait(&c, "reconnected with a pane warning", |p| {
        (p["connected"] == true && p["panes"][&b]["sub"].is_u64()).then(|| p["panes"][&b]["note"].as_str().unwrap_or("").to_string())
    });
    assert!(note.contains("not confirmed"), "{note}");
    assert!(!server_text(&c, &b).contains("lost-input"), "unacknowledged input must not be replayed");
    // The warning clears once the user types again.
    e.act(&c, "test-type", json!({"text": "echo ok-$((5+5))\r"}));
    wait_server_text(&c, &b, "ok-10");
    e.wait(&c, "warning cleared", |p| (p["panes"][&b]["note"] == "").then_some(()));
}

const CONFIG: &str = r#"
hearth.bind("ctrl+a c", "new-tab")
hearth.bind("ctrl+a r", "enter-mode", { args = { mode = "resize" } })
hearth.mode{ name = "resize", exclusive = true, bindings = { h = "grow-left", q = "exit-mode" } }
hearth.bind("ctrl+shift+r", "rename-to-x")
hearth.action{ name = "rename-to-x", title = "Rename Session To X",
  run = function(a) hearth.api.session_rename{ session = "s1", name = "renamed-by-lua" } end }
hearth.action{ name = "needs-arg", title = "Needs An Argument",
  args = { { name = "n", type = "integer", required = true } },
  run = function(a) hearth.api.session_rename{ session = "s1", name = "n-" .. a.n } end }
"#;

#[test]
fn gui_p14_bindings_sequences_modes_palette_and_config_actions() {
    let mut e = Env::new(Some(CONFIG));
    e.start_daemon();
    let (c, _rx) = e.client();
    let ids = call(&c, "session.create", json!({}));
    assert_eq!(ids["session"], "s1");
    e.start_app(&["--remote-control"]);
    e.wait_app(&c);
    e.wait(&c, "first pane", |p| (p["panes"].as_object()?.len() == 1).then_some(()));

    // A two-chord sequence: the prefix is shown, then the action runs.
    e.act(&c, "test-key", json!({"keys": "ctrl+a"}));
    e.wait(&c, "pending prefix shown", |p| (p["subtitle"].as_str()?.contains("ctrl+a …")).then_some(()));
    e.act(&c, "test-key", json!({"keys": "c"}));
    e.wait(&c, "new tab from the sequence", |p| (p["tabs"].as_array()?.len() == 2 && p["keys"].is_null()).then_some(()));

    // An exclusive mode: its keys work, it stays on, escape leaves.
    let before = first_session(&c)["windows"][1]["layout"].clone();
    assert_eq!(before["type"], "leaf");
    e.act(&c, "split-right", json!({}));
    e.wait(&c, "split", |p| (p["panes"].as_object()?.len() == 3).then_some(()));
    let r0 = first_session(&c)["windows"][1]["layout"]["ratio"].as_f64().unwrap();
    e.act(&c, "test-key", json!({"keys": "ctrl+a r"}));
    e.wait(&c, "mode on", |p| (p["mode"] == "resize" && p["subtitle"].as_str()?.contains("mode: resize")).then_some(()));
    e.act(&c, "test-key", json!({"keys": "h h"}));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let r = first_session(&c)["windows"][1]["layout"]["ratio"].as_f64().unwrap();
        if r < r0 - 0.05 {
            break;
        }
        assert!(Instant::now() < deadline, "grow-left did not move the divider ({r0} -> {r})");
        std::thread::sleep(Duration::from_millis(50));
    }
    let p = e.probe(&c);
    assert_eq!(p["mode"], "resize", "mode stays on after an action");
    e.act(&c, "test-key", json!({"keys": "escape"}));
    e.wait(&c, "mode off", |p| p["mode"].is_null().then_some(()));

    // A config action bound to a chord.
    e.act(&c, "test-key", json!({"keys": "ctrl+shift+r"}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while first_session(&c)["name"] != "renamed-by-lua" {
        assert!(Instant::now() < deadline, "config action did not run: {}", e.app_log());
        std::thread::sleep(Duration::from_millis(50));
    }

    // The palette lists built-ins with their keys, config actions and modes, and filters.
    e.act(&c, "command-palette", json!({"query": "split"}));
    let items = e.wait(&c, "palette open", |p| p["palette"].as_array().cloned());
    let titles: Vec<&str> = items.iter().map(|x| x.as_str().unwrap()).collect();
    assert!(titles.contains(&"Split Right") && titles.contains(&"Split Down"), "{titles:?}");
    assert!(!titles.contains(&"New Tab"), "query filters: {titles:?}");
    e.act(&c, "test-close-dialog", json!({}));
    e.wait(&c, "palette closed", |p| p["palette"].is_null().then_some(()));
    // Running a palette entry (what Enter does).
    call(&c, "session.rename", json!({"session": "s1", "name": "plain"}));
    e.act(&c, "test-palette", json!({"query": "rename session to x"}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while first_session(&c)["name"] != "renamed-by-lua" {
        assert!(Instant::now() < deadline, "palette entry did not run");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A config action with typed arguments.
    e.act(&c, "needs-arg", json!({"n": 7}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while first_session(&c)["name"] != "n-7" {
        assert!(Instant::now() < deadline, "action with args did not run");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A failing reload keeps the old bindings.
    std::fs::write(e.dir.join("cfg/config.lua"), "this is not lua").unwrap();
    assert_eq!(call(&c, "config.reload", json!({}))["ok"], false);
    e.act(&c, "test-key", json!({"keys": "ctrl+a c"}));
    e.wait(&c, "old bindings still work", |p| (p["tabs"].as_array()?.len() == 3).then_some(()));
}

#[test]
fn gui_p16_status_indicators_notifications_bell_and_clipboard() {
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, _rx) = e.client();
    let ids = call(&c, "session.create", json!({}));
    let b = ids["block"].as_str().unwrap().to_string();
    e.start_app(&["--remote-control"]);
    e.wait_app(&c);
    e.wait(&c, "attached", |p| p["panes"][&b]["sub"].is_u64().then_some(()));

    send(&c, &b, "printf '\\033]7501;state=working:msg=QnVpbGRpbmc=\\033\\\\'; echo W$((0+1))");
    let p = e.wait(&c, "working indicator", |p| (p["tabs"][0]["loading"] == true).then(|| p.clone()));
    assert_eq!(p["panes"][&b]["status"], "● working");

    // Move to another tab so the pane is out of sight, then block.
    e.act(&c, "new-tab", json!({}));
    e.wait(&c, "second tab selected", |p| (p["tabs"][1]["selected"] == true).then_some(()));
    send(&c, &b, "printf '\\033]7501;state=blocked:msg=QXBwcm92ZSBkZXBsb3k/\\033\\\\'; printf '\\a'; echo B$((0+1))");
    let p = e.wait(&c, "blocked indicator and notification", |p| {
        (p["tabs"][0]["indicator"] == "blocked" && p["notified"].as_array()?.iter().any(|n| n.as_str().unwrap_or("").contains("needs your attention"))).then(|| p.clone())
    });
    assert_eq!(p["tabs"][0]["loading"], false);
    assert_eq!(p["tabs"][0]["needs_attention"], true, "bell in a hidden tab marks it");
    assert_eq!(p["panes"][&b]["status"], "● blocked");
    // Selecting the tab clears the attention mark.
    e.act(&c, "prev-tab", json!({}));
    e.wait(&c, "attention cleared", |p| (p["tabs"][0]["selected"] == true && p["tabs"][0]["needs_attention"] == false).then_some(()));

    // OSC 52 from a program reaches the desktop clipboard.
    send(&c, &b, "printf '\\033]52;c;%s\\a' $(printf 'from-osc52' | base64)");
    e.wait(&c, "clipboard toast", |p| p["clipboard_note"].as_str().filter(|s| s.contains(&b)).map(|_| ()));
    e.act(&c, "test-clipboard", json!({}));
    e.wait(&c, "clipboard content", |p| p["log"].as_array()?.iter().any(|l| l == "clipboard=from-osc52").then_some(()));

    // A pane closed elsewhere disappears.
    call(&c, "block.close", json!({"block": b}));
    e.wait(&c, "pane removed after close", |p| p["panes"].get(&b).is_none().then_some(()));
}

#[test]
fn gui_remote_control_is_off_unless_enabled() {
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, _rx) = e.client();
    call(&c, "session.create", json!({}));
    e.start_app(&[]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let list = call(&c, "client.list", json!({}));
        if let Some(app) = list.as_array().unwrap().iter().find(|x| x["kind"] == "app") {
            assert_eq!(app["remote_control"], false);
            break;
        }
        assert!(Instant::now() < deadline, "app did not connect");
        std::thread::sleep(Duration::from_millis(50));
    }
    let r = c.call("client.action", json!({"action": "new-tab", "args": {}}));
    assert_eq!(r.unwrap_err().code, "not_ready", "no app may accept client.action without opting in");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(first_session(&c)["windows"].as_array().unwrap().len(), 1);
}

#[test]
fn gui_p03_recorded_programs_match_in_vte_live_and_after_reopening() {
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hearthd/tests/corpus");
    let mut e = Env::new(None);
    e.start_daemon();
    let (c, _rx) = e.client();
    let ids = call(&c, "session.create", json!({"name": "rec"}));
    let b = ids["block"].as_str().unwrap().to_string();
    e.start_app(&["--remote-control", "--session", "rec"]);
    e.wait_app(&c);
    e.wait(&c, "attached", |p| p["panes"][&b]["sub"].is_u64().then_some(()));
    for (i, name) in ["vim", "less", "nano", "whiptail", "progress", "git-log"].iter().enumerate() {
        let f = corpus.join(format!("{name}.vt"));
        assert!(f.exists(), "{} missing", f.display());
        // Reset the character set and colors so the marker prints as text.
        // The recordings ask the terminal questions; the server's answers
        // arrive as input, so swallow them before the next command.
        send(&c, &b, &format!("clear; cat '{}'; read -rs -t 1 -d '' _; printf '\\033(B\\033[0m\\n'; echo END-$(({i}+100))", f.display()));
        wait_server_text(&c, &b, &format!("END-{}", i + 100));
        assert_same_screen(&mut e, &c, &b);
    }
    // Leave it inside vim's alternate screen, then reopen the app.
    send(&c, &b, &format!("clear; head -c 20000 '{}'; sleep 600", corpus.join("vim.vt").display()));
    std::thread::sleep(Duration::from_millis(500));
    assert_same_screen(&mut e, &c, &b);
    e.stop_app();
    e.start_app(&["--remote-control", "--session", "rec"]);
    e.wait_app(&c);
    assert_same_screen(&mut e, &c, &b);
}
