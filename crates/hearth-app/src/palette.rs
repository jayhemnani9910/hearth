//! Command palette, text prompts and argument forms for config actions.

use crate::actions::BUILTIN;
use crate::win::{State, Win};
use adw::prelude::*;
use gtk::glib;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
struct Item {
    title: String,
    detail: String,
    action: String,
    args: Value,
}

thread_local! {
    /// The open palette, for the test probe.
    static OPEN: RefCell<Option<(adw::Dialog, gtk::ListBox)>> = const { RefCell::new(None) };
}

fn items(s: &State, sessions_only: bool) -> Vec<Item> {
    let mut v = vec![];
    for (id, view) in s.sessions() {
        if s.session_id() == Some(id.as_str()) {
            continue;
        }
        let name = view["name"].as_str().unwrap_or(id);
        v.push(Item { title: format!("Switch to session {name}"), detail: id.clone(), action: "switch-session-to".into(), args: json!({"session": id}) });
    }
    if sessions_only {
        return v;
    }
    for (name, title) in BUILTIN {
        if *name == "command-palette" {
            continue;
        }
        let keys = s.keymap.keys_for(name).unwrap_or("").to_string();
        v.push(Item { title: title.to_string(), detail: keys, action: name.to_string(), args: json!({}) });
    }
    for a in s.config()["actions"].as_array().into_iter().flatten() {
        let name = a["name"].as_str().unwrap_or("");
        let title = a["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(name);
        let keys = s.keymap.keys_for(name).unwrap_or("");
        let detail = if keys.is_empty() { format!("config action {name}") } else { format!("config action {name} · {keys}") };
        v.push(Item { title: title.into(), detail, action: name.into(), args: json!({}) });
    }
    for m in s.keymap.modes.keys() {
        v.push(Item { title: format!("Enter Key Mode {m}"), detail: String::new(), action: "enter-mode".into(), args: json!({"mode": m}) });
    }
    v
}

fn matches(it: &Item, q: &str) -> bool {
    let q = q.to_lowercase();
    let hay = format!("{} {} {}", it.title, it.detail, it.action).to_lowercase();
    q.split_whitespace().all(|w| hay.contains(w))
}

fn run_item(w: &Rc<Win>, s: &mut State, it: Item) {
    w.run_action(s, &it.action, it.args);
}

pub fn open(w: &Rc<Win>, s: &mut State, query: &str, sessions_only: bool) {
    let all = items(s, sessions_only);
    let dialog = adw::Dialog::builder().title(if sessions_only { "Switch Session" } else { "Command Palette" }).content_width(560).content_height(460).build();
    let entry = gtk::SearchEntry::builder().placeholder_text("Type to search").text(query).hexpand(true).build();
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&list).build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_top(6);
    body.set_margin_bottom(6);
    body.set_margin_start(6);
    body.set_margin_end(6);
    body.append(&entry);
    body.append(&scroll);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(&body));
    dialog.set_child(Some(&tv));

    let shown: Rc<RefCell<Vec<Item>>> = Rc::default();
    let fill = {
        let (list, shown, all) = (list.clone(), shown.clone(), all.clone());
        move |q: &str| {
            while let Some(c) = list.first_child() {
                list.remove(&c);
            }
            let v: Vec<Item> = all.iter().filter(|i| matches(i, q)).cloned().collect();
            for it in &v {
                let row = adw::ActionRow::builder().title(glib_escape(&it.title)).subtitle(glib_escape(&it.detail)).activatable(true).build();
                list.append(&row);
            }
            if let Some(r) = list.row_at_index(0) {
                list.select_row(Some(&r));
            }
            *shown.borrow_mut() = v;
        }
    };
    fill(query);
    let f = fill.clone();
    entry.connect_search_changed(move |e| f(&e.text()));
    let activate = {
        let (w, shown, dialog) = (w.clone(), shown.clone(), dialog.clone());
        move |i: usize| {
            let Some(it) = shown.borrow().get(i).cloned() else { return };
            dialog.close();
            w.with(move |w, s| run_item(w, s, it));
        }
    };
    let a = activate.clone();
    list.connect_row_activated(move |_, row| a(row.index() as usize));
    let (a, l) = (activate.clone(), list.clone());
    entry.connect_activate(move |_| a(l.selected_row().map_or(0, |r| r.index() as usize)));
    let l = list.clone();
    entry.connect_next_match(move |_| move_sel(&l, 1));
    let l = list.clone();
    entry.connect_previous_match(move |_| move_sel(&l, -1));
    let keys = gtk::EventControllerKey::new();
    let l = list.clone();
    keys.connect_key_pressed(move |_, key, _, _| match key {
        gtk::gdk::Key::Down => {
            move_sel(&l, 1);
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Up => {
            move_sel(&l, -1);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    entry.add_controller(keys);
    dialog.connect_closed(|_| OPEN.with(|o| *o.borrow_mut() = None));
    OPEN.with(|o| *o.borrow_mut() = Some((dialog.clone(), list.clone())));
    dialog.present(Some(&w.ui.window));
    entry.grab_focus();
}

fn glib_escape(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

fn move_sel(l: &gtk::ListBox, d: i32) {
    let i = l.selected_row().map_or(0, |r| r.index()) + d;
    if let Some(r) = l.row_at_index(i.max(0)) {
        l.select_row(Some(&r));
        r.grab_focus();
    }
}

/// Runs the first palette entry matching `query` (what Enter does).
pub fn run_query(w: &Rc<Win>, s: &mut State, query: &str) {
    match items(s, false).into_iter().find(|i| matches(i, query)) {
        Some(it) => run_item(w, s, it),
        None => w.toast(s, &format!("no palette entry matches '{query}'")),
    }
}

pub fn visible_items() -> Option<Vec<String>> {
    OPEN.with(|o| {
        let o = o.borrow();
        let (_, list) = o.as_ref()?;
        let mut v = vec![];
        let mut i = 0;
        while let Some(r) = list.row_at_index(i) {
            if let Some(a) = r.downcast_ref::<adw::ActionRow>() {
                v.push(a.title().to_string());
            }
            i += 1;
        }
        Some(v)
    })
}

/// Asks for one line of text.
pub fn prompt<F: FnOnce(&Rc<Win>, &mut State, String) + 'static>(w: &Rc<Win>, title: &str, initial: &str, done: F) {
    let d = adw::AlertDialog::new(Some(title), None);
    let entry = gtk::Entry::builder().text(initial).activates_default(true).build();
    d.set_extra_child(Some(&entry));
    d.add_responses(&[("cancel", "Cancel"), ("ok", "OK")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("ok"));
    let done = RefCell::new(Some(done));
    let w2 = w.clone();
    d.connect_response(None, move |_, r| {
        if r != "ok" {
            return;
        }
        let text = entry.text().to_string();
        if let Some(f) = done.borrow_mut().take() {
            w2.with(move |w, s| f(w, s, text));
        }
    });
    d.present(Some(&w.ui.window));
}

/// Runs a config action, asking for required arguments that were not given.
pub fn run_config_action(w: &Rc<Win>, s: &mut State, def: &Value, args: Value) {
    let name = def["name"].as_str().unwrap_or("").to_string();
    let specs: Vec<Value> = def["args"].as_array().cloned().unwrap_or_default();
    let missing = specs.iter().any(|a| a["required"].as_bool().unwrap_or(false) && args.get(a["name"].as_str().unwrap_or("")).is_none());
    if !missing {
        call_action(w, s, name, args);
        return;
    }
    let title = def["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(&name).to_string();
    let d = adw::AlertDialog::new(Some(&title), Some("Arguments"));
    let form = gtk::ListBox::new();
    form.add_css_class("boxed-list");
    form.set_selection_mode(gtk::SelectionMode::None);
    let mut rows = vec![];
    for a in &specs {
        let n = a["name"].as_str().unwrap_or("").to_string();
        let ty = a["type"].as_str().unwrap_or("string").to_string();
        let req = a["required"].as_bool().unwrap_or(false);
        let row = adw::EntryRow::builder().title(format!("{n} ({ty}{})", if req { ", required" } else { "" })).build();
        if let Some(v) = args.get(&n) {
            row.set_text(&v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
        }
        form.append(&row);
        rows.push((n, ty, row));
    }
    d.set_extra_child(Some(&form));
    d.add_responses(&[("cancel", "Cancel"), ("run", "Run")]);
    d.set_response_appearance("run", adw::ResponseAppearance::Suggested);
    d.set_default_response(Some("run"));
    let w2 = w.clone();
    d.connect_response(None, move |_, r| {
        if r != "run" {
            return;
        }
        let mut out = serde_json::Map::new();
        let mut bad = None;
        for (n, ty, row) in &rows {
            let t = row.text().to_string();
            if t.is_empty() {
                continue;
            }
            let v = match ty.as_str() {
                "integer" => t.parse::<i64>().map(Value::from).ok(),
                "number" => t.parse::<f64>().ok().map(Value::from),
                "boolean" => match t.as_str() {
                    "true" | "yes" | "1" => Some(Value::Bool(true)),
                    "false" | "no" | "0" => Some(Value::Bool(false)),
                    _ => None,
                },
                _ => Some(Value::String(t.clone())),
            };
            match v {
                Some(v) => {
                    out.insert(n.clone(), v);
                }
                None => bad = Some(format!("'{t}' is not a valid {ty} for {n}")),
            }
        }
        let name = name.clone();
        w2.with(move |w, s| match bad {
            Some(b) => w.toast(s, &b),
            None => call_action(w, s, name, Value::Object(out)),
        });
    });
    d.present(Some(&w.ui.window));
}

fn call_action(w: &Rc<Win>, s: &mut State, name: String, args: Value) {
    let n = name.clone();
    w.call(s, "action.run", json!({"name": name, "args": args}), move |w, s, r| {
        if let Err(e) = r {
            w.toast(s, &format!("{n}: {}", e.message));
        }
    });
}
