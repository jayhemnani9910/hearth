//! `hearth agent setup|remove|context`: explicit, reversible integration
//! with coding agents. Setup is idempotent and only touches entries it owns;
//! unrelated settings are preserved byte-for-byte in meaning and key order.

use crate::{Args, Flags, die};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const HOOK_CMD: &str = "hearth agent context";
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

Report your state to the terminal with OSC 7501 so the person sees it in
the tab: printf '\e]7501;state=%s:msg=%s\e\\' working "$(printf %s 'Running tests' | base64 -w0)"
States: idle working blocked done error. Use blocked when you need the person.
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

fn has_our_hook(v: &Value) -> bool {
    v["hooks"]["SessionStart"].as_array().into_iter().flatten().any(|g| g["hooks"].as_array().into_iter().flatten().any(|h| h["command"] == HOOK_CMD))
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
    if install {
        if has_our_hook(&v) {
            return Ok(None);
        }
        if v.get("hooks").is_none() {
            v["hooks"] = json!({});
        }
        if v["hooks"].get("SessionStart").is_none() {
            v["hooks"]["SessionStart"] = json!([]);
        }
        v["hooks"]["SessionStart"]
            .as_array_mut()
            .ok_or("hooks.SessionStart is not a list")?
            .push(json!({"hooks": [{"type": "command", "command": HOOK_CMD}]}));
    } else {
        if !has_our_hook(&v) {
            return Ok(None);
        }
        if let Some(groups) = v["hooks"]["SessionStart"].as_array_mut() {
            for g in groups.iter_mut() {
                if let Some(hs) = g["hooks"].as_array_mut() {
                    hs.retain(|h| h["command"] != HOOK_CMD);
                }
            }
            groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
            if groups.is_empty() {
                v["hooks"].as_object_mut().unwrap().remove("SessionStart");
            }
        }
        if v["hooks"].as_object().is_some_and(|o| o.is_empty()) {
            v.as_object_mut().unwrap().remove("hooks");
        }
    }
    Ok(Some(serde_json::to_string_pretty(&v).unwrap() + "\n"))
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

pub fn main(args: &Args, rest: &[String]) {
    let mut f = Flags::new(rest);
    let which = f.opt(&["--agent"]).unwrap_or("all".into());
    let dry = f.has(&["--dry-run"]);
    let cdir = claude_dir(&mut f);
    let xdir = codex_dir(&mut f);
    let pos = f.positional();
    let sub = pos.first().map(|s| s.as_str()).unwrap_or("");
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
        _ => die(2, "usage: hearth agent setup|remove|context [--agent claude|codex|all] [--dry-run]"),
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
