//! `hearth agent setup|remove|context`: explicit, reversible integration
//! with coding agents. Setup is idempotent and only touches entries it owns;
//! unrelated settings are preserved byte-for-byte in meaning and key order.

use crate::{Args, Flags, die};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Claude Code hooks this tool owns: (event, command).
const HOOKS: &[(&str, &str)] = &[("SessionStart", "hearth agent context")];

/// Hooks an earlier version installed and setup now removes: Claude Code
/// reports its own state with OSC 7501, so status hooks only duplicated it.
const RETIRED: &[(&str, &str)] = &[
    ("UserPromptSubmit", "hearth agent status working --app claude"),
    ("PostToolUse", "hearth agent status working --app claude"),
    ("Notification", "hearth agent status blocked --app claude"),
    ("Stop", "hearth agent status done --app claude"),
];
const MARK_BEGIN: &str = "<!-- hearth:begin -->";
const MARK_END: &str = "<!-- hearth:end -->";
const SKILL_MARK: &str = "hearth-managed: true";

const GUIDE: &str = r#"Hearth is the terminal you are running in. Its server owns every terminal,
so programs keep running when windows close. Control it with the `hearth` CLI.

Your own terminal is $HEARTH_BLOCK in session $HEARTH_SESSION. Commands
target those by default, so you act on your own context, not on whatever
pane the person is looking at. Pass -t to target something else.

  hearth ls --json                         sessions, windows, terminals
  hearth run -p right -- npm test          run beside you (new split)
  hearth run -p window --wait -- make      run in a new tab; exit status = make's
  hearth capture -t BLOCK [--history]      read a terminal's screen
  hearth send -t BLOCK 'text' ; hearth keys -t BLOCK Enter C-c
  hearth wait -t BLOCK --text 'Listening'  wait for output
  hearth api list / hearth api describe METHOD

Report your state so the person sees it on your tab, and gets a desktop
notification when you need them:
  hearth agent status working --app NAME -m 'Running tests'
States: idle working blocked done error. Use blocked when you need the person,
done when you finish. (This writes OSC 7501 to your terminal.)
"#;

fn claude_dir(f: &mut Flags) -> PathBuf {
    f.opt(&["--claude-dir"])
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| hearth_core::paths::home().join(".claude"))
}

fn codex_dir(f: &mut Flags) -> PathBuf {
    f.opt(&["--codex-dir"])
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .unwrap_or_else(|| hearth_core::paths::home().join(".codex"))
}

fn write_atomic(path: &Path, data: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("hearth-tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(tmp, path)
}

fn has_hook(v: &Value, event: &str, cmd: &str) -> bool {
    v["hooks"][event].as_array().into_iter().flatten().any(|g| g["hooks"].as_array().into_iter().flatten().any(|h| h["command"] == cmd))
}

/// Returns the planned new settings text, or None if nothing changes.
fn claude_settings(path: &Path, install: bool) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !install {
                return Ok(None);
            }
            "{}".into()
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut v: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON ({e}); not touching it", path.display()))?;
    if !v.is_object() {
        return Err(format!("{} is not a JSON object; not touching it", path.display()));
    }
    let retired = RETIRED.iter().any(|(e, c)| has_hook(&v, e, c));
    if install {
        let missing: Vec<_> = HOOKS.iter().filter(|(e, c)| !has_hook(&v, e, c)).collect();
        if missing.is_empty() && !retired {
            return Ok(None);
        }
        remove_hooks(&mut v, RETIRED);
        if v.get("hooks").is_none() {
            v["hooks"] = json!({});
        }
        for (event, cmd) in missing {
            if v["hooks"].get(*event).is_none() {
                v["hooks"][*event] = json!([]);
            }
            v["hooks"][*event]
                .as_array_mut()
                .ok_or_else(|| format!("hooks.{event} is not a list"))?
                .push(json!({"hooks": [{"type": "command", "command": cmd}]}));
        }
    } else {
        if !retired && !HOOKS.iter().any(|(e, c)| has_hook(&v, e, c)) {
            return Ok(None);
        }
        remove_hooks(&mut v, HOOKS);
        remove_hooks(&mut v, RETIRED);
    }
    Ok(Some(serde_json::to_string_pretty(&v).unwrap() + "\n"))
}

fn remove_hooks(v: &mut Value, which: &[(&str, &str)]) {
    for (event, cmd) in which {
        // get_mut, not indexing: indexing a mutable Value inserts nulls.
        if let Some(groups) = v.get_mut("hooks").and_then(|h| h.get_mut(*event)).and_then(|g| g.as_array_mut()) {
            for g in groups.iter_mut() {
                if let Some(hs) = g["hooks"].as_array_mut() {
                    hs.retain(|h| h["command"] != *cmd);
                }
            }
            groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
            if groups.is_empty() {
                v["hooks"].as_object_mut().unwrap().remove(*event);
            }
        }
    }
    if v["hooks"].as_object().is_some_and(|o| o.is_empty()) {
        v.as_object_mut().unwrap().remove("hooks");
    }
}

fn skill_text() -> String {
    format!("---\nname: hearth\ndescription: Control Hearth terminals, sessions and splits from the shell with the hearth CLI; report agent status with OSC 7501.\n# {SKILL_MARK}\n---\n\n{GUIDE}")
}

fn agents_md(path: &Path, install: bool) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let section = format!("{MARK_BEGIN}\n## Hearth terminal\n\n{GUIDE}{MARK_END}\n");
    let existing = match (text.find(MARK_BEGIN), text.find(MARK_END)) {
        (Some(a), Some(b)) if b > a => Some((a, b + MARK_END.len())),
        _ => None,
    };
    let out = match (install, existing) {
        (true, Some((a, b))) => {
            let mut end = b;
            if text[end..].starts_with('\n') {
                end += 1;
            }
            if text[a..end] == section {
                return Ok(None);
            }
            format!("{}{section}{}", &text[..a], &text[end..])
        }
        (true, None) => {
            let sep = if text.is_empty() || text.ends_with("\n\n") { "" } else if text.ends_with('\n') { "\n" } else { "\n\n" };
            format!("{text}{sep}{section}")
        }
        (false, Some((a, b))) => {
            let mut end = b;
            if text[end..].starts_with('\n') {
                end += 1;
            }
            let mut start = a;
            if text[..start].ends_with("\n\n") {
                start -= 1;
            }
            format!("{}{}", &text[..start], &text[end..])
        }
        (false, None) => return Ok(None),
    };
    Ok(Some(out))
}

/// `hearth agent status STATE [--app NAME] [--message TEXT]`, run by agent
/// hooks: shows the state on this terminal's tab via OSC 7501. The message
/// is taken from a hook's JSON on stdin (`message`) when not given. Prints
/// nothing and always succeeds, so it can never disturb the agent; outside
/// Hearth it does nothing.
fn status(state: &str, app: Option<String>, mut msg: Option<String>) {
    use base64::Engine;
    use std::io::{IsTerminal, Read, Write};
    if std::env::var_os("HEARTH_BLOCK").is_none() || !matches!(state, "idle" | "working" | "blocked" | "done" | "error") {
        return;
    }
    if msg.is_none() && !std::io::stdin().is_terminal() {
        let mut input = String::new();
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
        msg = serde_json::from_str::<Value>(&input).ok().and_then(|v| v["message"].as_str().map(String::from));
    }
    let mut osc = format!("\x1b]7501;state={state}");
    if let Some(a) = app.filter(|a| !a.is_empty() && !a.contains([':', ';', '\x1b', '\x07'])) {
        osc.push_str(&format!(":app={a}"));
    }
    if let Some(m) = msg.filter(|m| !m.is_empty()) {
        osc.push_str(&format!(":msg={}", base64::engine::general_purpose::STANDARD.encode(m.chars().take(500).collect::<String>())));
    }
    osc.push_str("\x1b\\");
    // The agent's terminal, even when the hook's own output is captured.
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(osc.as_bytes());
    }
}

pub fn main(args: &Args, rest: &[String]) {
    let mut f = Flags::new(rest);
    let which = f.opt(&["--agent"]).unwrap_or("all".into());
    let dry = f.has(&["--dry-run"]);
    let cdir = claude_dir(&mut f);
    let xdir = codex_dir(&mut f);
    let app = f.opt(&["--app"]);
    let message = f.opt(&["--message", "-m"]);
    let pos = f.positional();
    let sub = pos.first().map(|s| s.as_str()).unwrap_or("");
    if sub == "status" {
        status(pos.get(1).map(|s| s.as_str()).unwrap_or(""), app, message);
        return;
    }
    if sub == "context" {
        // Run by agent session hooks: tell the agent where it is.
        if let (Ok(s), Ok(b)) = (std::env::var("HEARTH_SESSION"), std::env::var("HEARTH_BLOCK")) {
            println!("You are running inside a Hearth terminal: session {s}, block {b}. `hearth` commands target this block and session by default. See `hearth help`.");
        }
        return;
    }
    let install = match sub {
        "setup" | "install" => true,
        "remove" | "uninstall" => false,
        _ => die(2, "usage: hearth agent setup|remove|context|status STATE [--agent claude|codex|all] [--dry-run]"),
    };
    if !matches!(which.as_str(), "claude" | "codex" | "all") {
        die(2, "--agent must be claude, codex or all");
    }
    let mut changes: Vec<(PathBuf, Option<String>)> = vec![]; // None content = delete
    if which == "claude" || which == "all" {
        let settings = cdir.join("settings.json");
        match claude_settings(&settings, install) {
            Ok(Some(t)) => changes.push((settings, Some(t))),
            Ok(None) => {}
            Err(e) => die(1, e),
        }
        let skill = cdir.join("skills/hearth/SKILL.md");
        let cur = std::fs::read_to_string(&skill).ok();
        if install {
            if cur.as_deref() != Some(skill_text().as_str()) {
                if cur.as_ref().is_some_and(|c| !c.contains(SKILL_MARK)) {
                    die(1, format!("{} exists and was not written by hearth; not replacing it", skill.display()));
                }
                changes.push((skill, Some(skill_text())));
            }
        } else if cur.as_ref().is_some_and(|c| c.contains(SKILL_MARK)) {
            changes.push((skill, None));
        }
    }
    if which == "codex" || which == "all" {
        let md = xdir.join("AGENTS.md");
        match agents_md(&md, install) {
            Ok(Some(t)) => changes.push((md, Some(t))),
            Ok(None) => {}
            Err(e) => die(1, e),
        }
    }
    if changes.is_empty() {
        if args.json {
            println!("{}", json!({"changed": []}));
        } else {
            println!("nothing to change");
        }
        return;
    }
    for (path, content) in &changes {
        let what = if content.is_none() { "remove" } else if path.exists() { "update" } else { "create" };
        if !args.json {
            println!("{}{what} {}", if dry { "would " } else { "" }, path.display());
        }
        if dry {
            continue;
        }
        let r = match content {
            Some(c) => write_atomic(path, c),
            None => std::fs::remove_file(path).and_then(|_| {
                if let Some(d) = path.parent() {
                    let _ = std::fs::remove_dir(d);
                }
                Ok(())
            }),
        };
        if let Err(e) = r {
            die(1, format!("{}: {e}", path.display()));
        }
    }
    if args.json {
        println!("{}", json!({"dry_run": dry, "changed": changes.iter().map(|(p, c)| json!({"path": p, "action": if c.is_some() {"write"} else {"remove"}})).collect::<Vec<_>>()}));
    }
}
