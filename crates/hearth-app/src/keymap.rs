//! Key bindings, chord sequences and modes. Pure logic, no GTK.

use hearth_core::keys::{Chord, parse_sequence};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Binding {
    pub keys: String,
    pub seq: Vec<Chord>,
    pub action: String,
    pub args: Value,
    /// Only active while this mode is on.
    pub mode: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Run(String, Value),
    /// A prefix of a longer sequence: wait for the next chord.
    Pending,
    /// Consumed without an action (failed sequence, exclusive mode).
    Swallow,
    /// Not ours: let the terminal have it.
    Pass,
}

pub const DEFAULTS: &[(&str, &str)] = &[
    ("ctrl+shift+t", "new-tab"),
    ("ctrl+shift+n", "new-window"),
    ("ctrl+shift+w", "close-pane"),
    ("ctrl+shift+d", "split-right"),
    ("ctrl+shift+e", "split-down"),
    ("ctrl+shift+p", "command-palette"),
    ("ctrl+shift+s", "switch-session"),
    ("ctrl+shift+c", "copy"),
    ("ctrl+shift+v", "paste"),
    ("ctrl+shift+z", "zoom"),
    ("ctrl+pagedown", "next-tab"),
    ("ctrl+pageup", "prev-tab"),
    ("alt+left", "focus-left"),
    ("alt+right", "focus-right"),
    ("alt+up", "focus-up"),
    ("alt+down", "focus-down"),
    ("ctrl+plus", "font-bigger"),
    ("ctrl+minus", "font-smaller"),
    ("ctrl+0", "font-reset"),
];

#[derive(Default)]
pub struct Keymap {
    pub bindings: Vec<Binding>,
    /// Mode name -> exclusive.
    pub modes: HashMap<String, bool>,
    pub pending: Vec<Chord>,
    pub mode: Option<String>,
}

impl Keymap {
    /// Defaults first; a config binding for the same keys (and mode) replaces one.
    pub fn from_config(cfg: &Value) -> Keymap {
        let mut k = Keymap::default();
        for (keys, action) in DEFAULTS {
            k.add(keys, action, json!({}), None);
        }
        for b in cfg["bindings"].as_array().into_iter().flatten() {
            let mode = b["mode"].as_str().map(String::from);
            k.add(b["keys"].as_str().unwrap_or(""), b["action"].as_str().unwrap_or(""), b["args"].clone(), mode);
        }
        for m in cfg["modes"].as_array().into_iter().flatten() {
            let Some(name) = m["name"].as_str() else { continue };
            k.modes.insert(name.into(), m["exclusive"].as_bool().unwrap_or(false));
            for b in m["bindings"].as_array().into_iter().flatten() {
                k.add(b["keys"].as_str().unwrap_or(""), b["action"].as_str().unwrap_or(""), json!({}), Some(name.into()));
            }
        }
        k
    }

    fn add(&mut self, keys: &str, action: &str, args: Value, mode: Option<String>) {
        let Ok(seq) = parse_sequence(keys) else { return };
        if action.is_empty() {
            return;
        }
        self.bindings.retain(|b| !(b.seq == seq && b.mode == mode));
        let args = if args.is_object() { args } else { json!({}) };
        self.bindings.push(Binding { keys: keys.into(), seq, action: action.into(), args, mode });
    }

    fn exclusive(&self) -> bool {
        self.mode.as_ref().is_some_and(|m| self.modes.get(m).copied().unwrap_or(false))
    }

    /// Bindings that apply now; mode bindings come first so they shadow globals.
    fn active(&self) -> Vec<&Binding> {
        let mut v: Vec<&Binding> = vec![];
        if let Some(m) = &self.mode {
            v.extend(self.bindings.iter().filter(|b| b.mode.as_deref() == Some(m)));
            if self.exclusive() {
                return v;
            }
        }
        v.extend(self.bindings.iter().filter(|b| b.mode.is_none()));
        v
    }

    pub fn enter_mode(&mut self, name: &str) -> bool {
        if !self.modes.contains_key(name) && !self.bindings.iter().any(|b| b.mode.as_deref() == Some(name)) {
            return false;
        }
        self.mode = Some(name.into());
        self.pending.clear();
        true
    }

    pub fn press(&mut self, c: Chord) -> Outcome {
        let mut seq = self.pending.clone();
        seq.push(c.clone());
        let active = self.active();
        if let Some(b) = active.iter().find(|b| b.seq == seq) {
            let out = Outcome::Run(b.action.clone(), b.args.clone());
            self.pending.clear();
            return out;
        }
        if active.iter().any(|b| b.seq.len() > seq.len() && b.seq[..seq.len()] == seq[..]) {
            self.pending = seq;
            return Outcome::Pending;
        }
        let had_prefix = !self.pending.is_empty();
        self.pending.clear();
        if had_prefix {
            return Outcome::Swallow;
        }
        if self.mode.is_some() {
            let plain_escape = c.key == "escape" && !(c.ctrl || c.shift || c.alt || c.sup);
            if plain_escape {
                self.mode = None;
                return Outcome::Swallow;
            }
            if self.exclusive() {
                return Outcome::Swallow;
            }
        }
        Outcome::Pass
    }

    /// Text for the header: active mode and pending prefix.
    pub fn describe(&self) -> Option<String> {
        let pend: Vec<String> = self.pending.iter().map(chord_text).collect();
        match (&self.mode, pend.is_empty()) {
            (None, true) => None,
            (None, false) => Some(format!("{} …", pend.join(" "))),
            (Some(m), true) => Some(format!("mode: {m}")),
            (Some(m), false) => Some(format!("mode: {m} · {} …", pend.join(" "))),
        }
    }

    /// The first key sequence bound to an action (for menus and the palette).
    pub fn keys_for(&self, action: &str) -> Option<&str> {
        self.bindings.iter().find(|b| b.action == action && b.mode.is_none()).map(|b| b.keys.as_str())
    }
}

pub fn chord_text(c: &Chord) -> String {
    let mut s = String::new();
    for (on, name) in [(c.ctrl, "ctrl+"), (c.shift, "shift+"), (c.alt, "alt+"), (c.sup, "super+")] {
        if on {
            s.push_str(name);
        }
    }
    s.push_str(&c.key);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_core::keys::parse_chord;

    fn ch(s: &str) -> Chord {
        parse_chord(s).unwrap()
    }

    fn cfg() -> Value {
        json!({
            "bindings": [
                {"keys": "ctrl+a c", "action": "new-tab", "args": {}},
                {"keys": "ctrl+a r", "action": "enter-mode", "args": {"mode": "resize"}},
                {"keys": "ctrl+shift+t", "action": "my-action", "args": {"x": 1}},
                {"keys": "q", "action": "exit-mode", "args": {}, "mode": "resize"},
            ],
            "modes": [
                {"name": "resize", "exclusive": true, "bindings": [{"keys": "h", "action": "grow-left"}]},
                {"name": "loose", "exclusive": false, "bindings": [{"keys": "x", "action": "loose-x"}]},
            ]
        })
    }

    #[test]
    fn config_overrides_default_and_keeps_others() {
        let mut k = Keymap::from_config(&cfg());
        assert_eq!(k.press(ch("ctrl+shift+t")), Outcome::Run("my-action".into(), json!({"x": 1})));
        assert_eq!(k.press(ch("ctrl+shift+d")), Outcome::Run("split-right".into(), json!({})));
        assert_eq!(k.keys_for("split-right"), Some("ctrl+shift+d"));
    }

    #[test]
    fn sequences_wait_then_run_or_swallow() {
        let mut k = Keymap::from_config(&cfg());
        assert_eq!(k.press(ch("ctrl+a")), Outcome::Pending);
        assert_eq!(k.describe().as_deref(), Some("ctrl+a …"));
        assert_eq!(k.press(ch("c")), Outcome::Run("new-tab".into(), json!({})));
        assert_eq!(k.press(ch("ctrl+a")), Outcome::Pending);
        assert_eq!(k.press(ch("z")), Outcome::Swallow);
        assert!(k.pending.is_empty());
        assert_eq!(k.press(ch("z")), Outcome::Pass);
    }

    #[test]
    fn exclusive_mode_swallows_and_escape_exits() {
        let mut k = Keymap::from_config(&cfg());
        assert!(k.enter_mode("resize"));
        assert_eq!(k.describe().as_deref(), Some("mode: resize"));
        assert_eq!(k.press(ch("h")), Outcome::Run("grow-left".into(), json!({})));
        assert_eq!(k.press(ch("h")), Outcome::Run("grow-left".into(), json!({})), "mode stays on");
        assert_eq!(k.press(ch("a")), Outcome::Swallow);
        assert_eq!(k.press(ch("ctrl+shift+d")), Outcome::Swallow, "globals are off in an exclusive mode");
        assert_eq!(k.press(ch("q")), Outcome::Run("exit-mode".into(), json!({})), "a bind with a mode option joins the mode");
        assert_eq!(k.press(ch("escape")), Outcome::Swallow);
        assert_eq!(k.mode, None);
        assert_eq!(k.press(ch("a")), Outcome::Pass);
    }

    #[test]
    fn open_mode_passes_unbound_keys_and_keeps_globals() {
        let mut k = Keymap::from_config(&cfg());
        assert!(k.enter_mode("loose"));
        assert_eq!(k.press(ch("x")), Outcome::Run("loose-x".into(), json!({})));
        assert_eq!(k.press(ch("a")), Outcome::Pass);
        assert_eq!(k.press(ch("ctrl+shift+d")), Outcome::Run("split-right".into(), json!({})));
        assert!(!k.enter_mode("nope"));
    }
}
