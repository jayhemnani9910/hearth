//! hearth: command-line control of a Hearth server. Works without the GUI.
//! stdout carries results (JSON with --json); diagnostics go to stderr.

mod agent;
mod attach;
mod script;

use hearth_core::conn::{self, ApiError, Client, ConnectError, Target};
use hearth_core::proto::code;
use serde_json::{Value, json};
use std::process::exit;
use std::time::Duration;

const HELP: &str = "\
hearth - control a Hearth terminal server

usage: hearth [--host ADDR] [--json] <command> [args]

sessions and layout
  ls                                   list sessions, windows and terminals
  new [-n NAME] [-c DIR] [-- CMD...]   new session
  tab [-t SESSION] [-n NAME] [-c DIR] [-- CMD...]
  split [-t BLOCK] [-d right|down|left|up] [-c DIR] [-- CMD...]
  rename session|window ID NAME
  kill [-t BLOCK]                      terminate a terminal's process group
  close-window WINDOW | close-session SESSION
  focus [-t BLOCK] [-d DIR] | zoom [-t BLOCK] | balance -w WINDOW
  resize [-t BLOCK] -d DIR [--amount F] | swap A B | move BLOCK --to TARGET -d DIR
  move-window WINDOW INDEX | select-window WINDOW
terminals
  run [-p window|session|right|down|left|up] [-t TARGET] [--shell none|login|interactive]
      [-c DIR] [--close] [--wait] -- CMD...
  send [-t BLOCK] [--paste] TEXT...     write text
  keys [-t BLOCK] KEY...               Enter C-c M-x Up F5 ...
  capture [-t BLOCK] [--format text|html|vt] [--history]
  wait [-t BLOCK] [--timeout MS] [--text TEXT]   exit status = the program's
  info [-t BLOCK] | status [-t BLOCK] | size -t BLOCK COLS ROWS
  attach [-t BLOCK]                    view a terminal here (Ctrl-] detaches)
automation
  events                               stream server events (JSON lines)
  api list | api describe METHOD | api call METHOD [JSON]
  action list | action run NAME [key=value...]
  config show | config reload
  lua FILE [ARGS...]                   run a Lua script against the server
  agent setup|remove [--agent claude|codex|all] [--dry-run] | agent context
server
  server status | server stop [--force] | server start
  proxy                                bridge stdio to the local server (for ssh)
  version

Targets default to $HEARTH_BLOCK / $HEARTH_SESSION when run inside Hearth.
Exit codes: 0 ok, 1 failed, 2 usage, 3 not found, 4 ambiguous, 5 timeout,
6 rejected, 7 no server.";

pub struct Args {
    pub json: bool,
    pub host: Option<String>,
    rest: Vec<String>,
}

pub fn die(code_: i32, msg: impl std::fmt::Display) -> ! {
    eprintln!("hearth: {msg}");
    exit(code_)
}

pub fn exit_code(e: &ApiError) -> i32 {
    match e.code.as_str() {
        code::INVALID | code::UNKNOWN_METHOD => 2,
        code::NOT_FOUND => 3,
        code::AMBIGUOUS => 4,
        code::TIMEOUT => 5,
        code::DENIED => 6,
        "disconnected" => 7,
        _ => 1,
    }
}

pub fn connect(host: &Option<String>, kind: &str) -> (Client, std::sync::mpsc::Receiver<conn::Incoming>) {
    let target = match host.as_deref().or(std::env::var("HEARTH_HOST").ok().as_deref()) {
        Some(h) => Target::parse(h).unwrap_or_else(|e| die(2, e)),
        None => Target::Unix(hearth_core::paths::socket_path()),
    };
    let local_default = matches!(&target, Target::Unix(p) if *p == hearth_core::paths::socket_path());
    match Client::connect(&target, kind, "hearth-cli") {
        Ok(c) => c,
        Err(ConnectError::NoServer(_)) if local_default => {
            if let Err(e) = conn::ensure_local_server() {
                die(7, e);
            }
            Client::connect(&target, kind, "hearth-cli").unwrap_or_else(|e| die(7, e))
        }
        Err(e @ ConnectError::Rejected(_)) => die(6, e),
        Err(e) => die(7, e),
    }
}

/// Simple flag parser: `flag(&["-t", "--target"])` takes a value.
pub struct Flags {
    v: Vec<String>,
    pub tail: Vec<String>,
}

impl Flags {
    pub fn new(args: &[String]) -> Flags {
        let (v, tail) = match args.iter().position(|a| a == "--") {
            Some(i) => (args[..i].to_vec(), args[i + 1..].to_vec()),
            None => (args.to_vec(), vec![]),
        };
        Flags { v, tail }
    }
    pub fn opt(&mut self, names: &[&str]) -> Option<String> {
        let i = self.v.iter().position(|a| names.contains(&a.as_str()))?;
        if i + 1 >= self.v.len() {
            die(2, format!("{} needs a value", names[0]));
        }
        let val = self.v.remove(i + 1);
        self.v.remove(i);
        Some(val)
    }
    pub fn has(&mut self, names: &[&str]) -> bool {
        match self.v.iter().position(|a| names.contains(&a.as_str())) {
            Some(i) => {
                self.v.remove(i);
                true
            }
            None => false,
        }
    }
    pub fn positional(&mut self) -> Vec<String> {
        if let Some(bad) = self.v.iter().find(|a| a.starts_with('-') && a.len() > 1 && a.parse::<f64>().is_err()) {
            die(2, format!("unknown option {bad}"));
        }
        std::mem::take(&mut self.v)
    }
}

fn block_target(f: &mut Flags) -> String {
    f.opt(&["-t", "--target"])
        .or_else(|| std::env::var("HEARTH_BLOCK").ok())
        .unwrap_or_else(|| die(2, "no target terminal: pass -t BLOCK or run inside a Hearth terminal"))
}

fn session_target(f: &mut Flags) -> String {
    f.opt(&["-t", "--target"])
        .or_else(|| std::env::var("HEARTH_SESSION").ok())
        .unwrap_or_else(|| die(2, "no target session: pass -t SESSION or run inside a Hearth terminal"))
}

fn spawn_params(f: &mut Flags, p: &mut Value) {
    if let Some(c) = f.opt(&["-c", "--cwd"]) {
        let abs = std::fs::canonicalize(&c).map(|p| p.display().to_string()).unwrap_or(c);
        p["cwd"] = json!(abs);
    }
    if let Some(s) = f.opt(&["--shell"]) {
        p["shell"] = json!(s);
    }
    if !f.tail.is_empty() {
        p["command"] = json!(f.tail);
    }
}

fn print(args: &Args, v: &Value, human: impl FnOnce(&Value)) {
    if args.json {
        println!("{}", serde_json::to_string_pretty(v).unwrap());
    } else {
        human(v)
    }
}

fn call(c: &Client, method: &str, p: Value) -> Value {
    c.call(method, p).unwrap_or_else(|e| die(exit_code(&e), e))
}

fn print_ids(v: &Value) {
    let parts: Vec<String> = ["session", "window", "block"]
        .iter()
        .filter_map(|k| v[*k].as_str().map(|s| format!("{k}={s}")))
        .collect();
    println!("{}", parts.join(" "));
}

fn ls(args: &Args, c: &Client) {
    let v = call(c, "session.bootstrap", json!({"subscribe": false}));
    if args.json {
        println!("{}", serde_json::to_string_pretty(&v["sessions"]).unwrap());
        return;
    }
    let sessions = v["sessions"].as_array().cloned().unwrap_or_default();
    if sessions.is_empty() {
        println!("no sessions");
    }
    for s in sessions {
        println!("{} {}", s["id"].as_str().unwrap_or(""), s["name"].as_str().unwrap_or(""));
        for w in s["windows"].as_array().into_iter().flatten() {
            let active = if w["id"] == s["active_window"] { "*" } else { " " };
            println!("  {active}{} {}", w["id"].as_str().unwrap_or(""), w["name"].as_str().unwrap_or(""));
            let mut leaves = vec![];
            collect_leaves(&w["layout"], &mut leaves);
            for b in leaves {
                let info = &s["blocks"][&b];
                let focus = if w["focus"] == b.as_str() { ">" } else { " " };
                let name = info["process"]["name"].as_str().unwrap_or("?");
                let cwd = info["cwd"]["reported"].as_str().or(info["cwd"]["process"].as_str()).unwrap_or("");
                let st = info["status_summary"].as_str().map(|s| format!(" [{s}]")).unwrap_or_default();
                let ex = if info["exit"].is_null() { String::new() } else { format!(" (exited {})", info["exit"]["code"]) };
                println!("    {focus}{b} {}x{} pid {} {name} {cwd}{st}{ex}", info["cols"], info["rows"], info["pid"]);
            }
        }
    }
}

fn collect_leaves(n: &Value, out: &mut Vec<String>) {
    match n["type"].as_str() {
        Some("leaf") => out.push(n["block"].as_str().unwrap_or("").to_string()),
        Some("split") => {
            collect_leaves(&n["a"], out);
            collect_leaves(&n["b"], out);
        }
        _ => {}
    }
}

fn wait_cmd(c: &Client, block: &str, timeout: Option<u64>, text: Option<String>) -> i32 {
    let t = timeout.map(Duration::from_millis).unwrap_or(Duration::from_secs(365 * 86400)) + Duration::from_secs(5);
    if let Some(text) = text {
        return match c.call_timeout("terminal.wait_text", json!({"block": block, "text": text, "timeout_ms": timeout}), t) {
            Ok(_) => 0,
            Err(e) => die(exit_code(&e), e),
        };
    }
    match c.call_timeout("terminal.wait", json!({"block": block, "timeout_ms": timeout}), t) {
        Ok(v) => match (v["code"].as_i64(), v["signal"].as_i64()) {
            (Some(code_), _) => code_ as i32,
            (None, Some(sig)) => 128 + sig as i32,
            _ => 1,
        },
        Err(e) => die(exit_code(&e), e),
    }
}

fn main() {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args { json: false, host: None, rest: vec![] };
    while let Some(first) = raw.first() {
        match first.as_str() {
            "--json" => {
                args.json = true;
                raw.remove(0);
            }
            "--host" | "-H" => {
                raw.remove(0);
                if raw.is_empty() {
                    die(2, "--host needs a value");
                }
                args.host = Some(raw.remove(0));
            }
            _ => break,
        }
    }
    // --json is accepted after the command too.
    if let Some(i) = raw.iter().position(|a| a == "--json") {
        if raw.iter().position(|a| a == "--").is_none_or(|d| i < d) {
            raw.remove(i);
            args.json = true;
        }
    }
    args.rest = raw;
    let Some(cmd) = args.rest.first().cloned() else {
        println!("{HELP}");
        return;
    };
    let rest: Vec<String> = args.rest[1..].to_vec();
    let mut f = Flags::new(&rest);
    match cmd.as_str() {
        "help" | "-h" | "--help" => println!("{HELP}"),
        "version" | "--version" => println!("hearth {}", env!("CARGO_PKG_VERSION")),
        "proxy" => attach::proxy(),
        "agent" => agent::main(&args, &rest),
        "lua" => {
            let pos = f.positional();
            let (c, rx) = connect(&args.host, "cli");
            exit(script::run(c, rx, &pos));
        }
        "attach" => {
            let block = block_target(&mut f);
            let (c, rx) = connect(&args.host, "cli");
            exit(attach::attach(&c, rx, &block));
        }
        "events" => {
            let (c, rx) = connect(&args.host, "cli");
            call(&c, "events.subscribe", json!({}));
            for m in rx {
                match m {
                    conn::Incoming::Event(v) => println!("{}", serde_json::to_string(&v).unwrap()),
                    conn::Incoming::Closed(r) => die(7, r),
                    _ => {}
                }
            }
        }
        "server" => {
            let force = f.has(&["--force"]);
            let sub = f.positional();
            match sub.first().map(|s| s.as_str()) {
                Some("start") => match conn::ensure_local_server() {
                    Ok(true) => println!("started"),
                    Ok(false) => println!("already running"),
                    Err(e) => die(7, e),
                },
                Some("status") | None => {
                    let (c, _rx) = connect(&args.host, "cli");
                    let v = call(&c, "server.status", json!({}));
                    print(&args, &v, |v| {
                        println!("hearthd {} pid {} incarnation {}", v["version"].as_str().unwrap_or(""), v["pid"], v["incarnation"].as_str().unwrap_or(""));
                        println!("socket {}", v["socket"].as_str().unwrap_or(""));
                        println!("{} session(s), {} terminal(s), {} client(s)", v["sessions"], v["blocks"], v["clients"].as_array().map(|a| a.len()).unwrap_or(0));
                        if let Some(l) = v["listen"].as_array().filter(|l| !l.is_empty()) {
                            println!("listening on {}", l.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "));
                        }
                        for d in v["config"]["diagnostics"].as_array().into_iter().flatten() {
                            println!("config: {}", d.as_str().unwrap_or(""));
                        }
                        if let Some(e) = v["config"]["error"].as_str() {
                            println!("config error: {e}");
                        }
                    });
                }
                Some("stop") => {
                    let (c, _rx) = connect(&args.host, "cli");
                    call(&c, "server.stop", json!({"force": force}));
                }
                Some(o) => die(2, format!("unknown server command '{o}'")),
            }
        }
        _ => {
            let (c, _rx) = connect(&args.host, "cli");
            run_command(&args, &c, &cmd, &mut f);
        }
    }
}

fn run_command(args: &Args, c: &Client, cmd: &str, f: &mut Flags) {
    match cmd {
        "ls" | "list" => ls(args, c),
        "new" | "new-session" => {
            let mut p = json!({});
            if let Some(n) = f.opt(&["-n", "--name"]) {
                p["name"] = json!(n);
            }
            spawn_params(f, &mut p);
            f.positional();
            let v = call(c, "session.create", p);
            print(args, &v, print_ids);
        }
        "tab" | "new-window" => {
            let mut p = json!({"session": session_target(f)});
            if let Some(n) = f.opt(&["-n", "--name"]) {
                p["name"] = json!(n);
            }
            spawn_params(f, &mut p);
            f.positional();
            let v = call(c, "window.create", p);
            print(args, &v, print_ids);
        }
        "split" => {
            let mut p = json!({"block": block_target(f), "direction": f.opt(&["-d", "--direction"]).unwrap_or("right".into())});
            if let Some(r) = f.opt(&["--ratio"]) {
                p["ratio"] = json!(r.parse::<f64>().unwrap_or_else(|_| die(2, "--ratio must be a number")));
            }
            spawn_params(f, &mut p);
            f.positional();
            let v = call(c, "block.split", p);
            print(args, &v, print_ids);
        }
        "run" => {
            let placement = f.opt(&["-p", "--placement"]).unwrap_or("window".into());
            let target = f.opt(&["-t", "--target"]).or_else(|| match placement.as_str() {
                "window" => std::env::var("HEARTH_SESSION").ok(),
                "session" => None,
                _ => std::env::var("HEARTH_BLOCK").ok(),
            });
            let wait = f.has(&["--wait"]);
            let close = f.has(&["--close"]);
            let mut p = json!({"placement": placement, "close_on_exit": close});
            if let Some(t) = target {
                p["target"] = json!(t);
            } else if placement != "session" {
                // Outside Hearth with no target: start a new session.
                p["placement"] = json!("session");
            }
            if let Some(n) = f.opt(&["-n", "--name"]) {
                p["name"] = json!(n);
            }
            spawn_params(f, &mut p);
            f.positional();
            if p["command"].is_null() {
                die(2, "run needs a command after --");
            }
            let v = call(c, "terminal.run", p);
            if wait {
                let b = v["block"].as_str().unwrap_or("").to_string();
                if !args.json {
                    print_ids(&v);
                }
                exit(wait_cmd(c, &b, None, None));
            }
            print(args, &v, print_ids);
        }
        "send" => {
            let b = block_target(f);
            let paste = f.has(&["--paste"]);
            let text = f.positional().join(" ") + &f.tail.join(" ");
            call(c, "terminal.send_text", json!({"block": b, "text": text, "paste": paste}));
        }
        "keys" | "send-keys" => {
            let b = block_target(f);
            let mut keys = f.positional();
            keys.extend(f.tail.clone());
            call(c, "terminal.send_keys", json!({"block": b, "keys": keys}));
        }
        "capture" => {
            let b = block_target(f);
            let fmt = f.opt(&["--format", "-f"]).unwrap_or("text".into());
            let history = f.has(&["--history"]);
            f.positional();
            let v = call(c, "terminal.capture", json!({"block": b, "format": fmt, "history": history}));
            print(args, &v, |v| print!("{}", v["content"].as_str().unwrap_or("")));
        }
        "wait" => {
            let b = block_target(f);
            let timeout = f.opt(&["--timeout"]).map(|t| t.parse().unwrap_or_else(|_| die(2, "--timeout is milliseconds")));
            let text = f.opt(&["--text"]);
            f.positional();
            exit(wait_cmd(c, &b, timeout, text));
        }
        "info" => {
            let b = block_target(f);
            let v = call(c, "block.info", json!({"block": b}));
            print(args, &v, |v| println!("{}", serde_json::to_string_pretty(v).unwrap()));
        }
        "status" => {
            let b = block_target(f);
            let v = call(c, "terminal.status", json!({"block": b}));
            print(args, &v, |v| {
                for r in v["records"].as_array().into_iter().flatten() {
                    let id = if r["id"] == "" { "(root)" } else { r["id"].as_str().unwrap_or("") };
                    let msg = r["msg"].as_str().map(hearth_core::osc7501::disarm).unwrap_or_default();
                    println!("{id} {} {} {msg}", r["state"].as_str().unwrap_or(""), r["app"].as_str().unwrap_or(""));
                }
            });
        }
        "size" => {
            let b = block_target(f);
            let pos = f.positional();
            let (cols, rows) = match (pos.first().and_then(|x| x.parse::<u64>().ok()), pos.get(1).and_then(|x| x.parse::<u64>().ok())) {
                (Some(a), Some(b)) => (a, b),
                _ => die(2, "usage: hearth size -t BLOCK COLS ROWS"),
            };
            call(c, "terminal.set_size", json!({"block": b, "cols": cols, "rows": rows}));
        }
        "rename" => {
            let pos = f.positional();
            if pos.len() < 3 {
                die(2, "usage: hearth rename session|window ID NAME");
            }
            let name = pos[2..].join(" ");
            match pos[0].as_str() {
                "session" => call(c, "session.rename", json!({"session": pos[1], "name": name})),
                "window" | "tab" => call(c, "window.rename", json!({"window": pos[1], "name": name})),
                _ => die(2, "rename session|window"),
            };
        }
        "kill" | "close" => {
            let b = block_target(f);
            call(c, "block.close", json!({"block": b}));
        }
        "close-window" => {
            let pos = f.positional();
            call(c, "window.close", json!({"window": pos.first().unwrap_or_else(|| die(2, "close-window WINDOW"))}));
        }
        "close-session" => {
            let pos = f.positional();
            call(c, "session.close", json!({"session": pos.first().unwrap_or_else(|| die(2, "close-session SESSION"))}));
        }
        "focus" => {
            let b = block_target(f);
            let mut p = json!({"block": b});
            if let Some(d) = f.opt(&["-d", "--direction"]) {
                p["direction"] = json!(d);
            }
            let v = call(c, "block.focus", p);
            print(args, &v, print_ids);
        }
        "zoom" => {
            let b = block_target(f);
            let v = call(c, "block.zoom", json!({"block": b}));
            print(args, &v, |v| println!("zoom {}", v["zoom"]));
        }
        "resize" => {
            let b = block_target(f);
            let d = f.opt(&["-d", "--direction"]).unwrap_or_else(|| die(2, "resize needs -d DIR"));
            let amount: f64 = f.opt(&["--amount"]).map(|a| a.parse().unwrap_or_else(|_| die(2, "--amount is a fraction"))).unwrap_or(0.05);
            call(c, "block.resize", json!({"block": b, "direction": d, "amount": amount}));
        }
        "swap" => {
            let pos = f.positional();
            if pos.len() != 2 {
                die(2, "usage: hearth swap BLOCK OTHER");
            }
            call(c, "block.swap", json!({"block": pos[0], "other": pos[1]}));
        }
        "move" => {
            let to = f.opt(&["--to"]).unwrap_or_else(|| die(2, "move needs --to TARGET"));
            let d = f.opt(&["-d", "--direction"]).unwrap_or("right".into());
            let pos = f.positional();
            let b = pos.first().cloned().or_else(|| std::env::var("HEARTH_BLOCK").ok()).unwrap_or_else(|| die(2, "move BLOCK --to TARGET"));
            call(c, "block.move", json!({"block": b, "target": to, "direction": d}));
        }
        "balance" => {
            let w = f.opt(&["-w", "--window"]).unwrap_or_else(|| die(2, "balance -w WINDOW"));
            call(c, "layout.balance", json!({"window": w}));
        }
        "move-window" => {
            let pos = f.positional();
            let (w, i) = match (pos.first(), pos.get(1).and_then(|x| x.parse::<u64>().ok())) {
                (Some(w), Some(i)) => (w.clone(), i),
                _ => die(2, "usage: hearth move-window WINDOW INDEX"),
            };
            call(c, "window.move", json!({"window": w, "index": i}));
        }
        "select-window" => {
            let pos = f.positional();
            call(c, "window.select", json!({"window": pos.first().unwrap_or_else(|| die(2, "select-window WINDOW"))}));
        }
        "api" => {
            let pos = f.positional();
            match pos.first().map(|s| s.as_str()) {
                Some("list") | None => {
                    let v = call(c, "api.list", json!({}));
                    print(args, &v, |v| {
                        for m in v.as_array().into_iter().flatten() {
                            println!("{:<22} {}", m["name"].as_str().unwrap_or(""), m["summary"].as_str().unwrap_or(""));
                        }
                    });
                }
                Some("describe") => {
                    let m = pos.get(1).unwrap_or_else(|| die(2, "api describe METHOD"));
                    let v = call(c, "api.describe", json!({"method": m}));
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                }
                Some("call") => {
                    let m = pos.get(1).unwrap_or_else(|| die(2, "api call METHOD [JSON]"));
                    let p: Value = match pos.get(2) {
                        Some(j) => serde_json::from_str(j).unwrap_or_else(|e| die(2, format!("params are not JSON: {e}"))),
                        None => json!({}),
                    };
                    let v = call(c, m, p);
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                }
                Some(o) => die(2, format!("unknown api command '{o}'")),
            }
        }
        "action" => {
            let pos = f.positional();
            match pos.first().map(|s| s.as_str()) {
                Some("list") | None => {
                    let v = call(c, "action.list", json!({}));
                    print(args, &v, |v| {
                        for a in v.as_array().into_iter().flatten() {
                            println!("{:<20} {}", a["name"].as_str().unwrap_or(""), a["title"].as_str().unwrap_or(""));
                        }
                    });
                }
                Some("run") => {
                    let name = pos.get(1).unwrap_or_else(|| die(2, "action run NAME [key=value...]"));
                    let mut a = serde_json::Map::new();
                    for kv in &pos[2..] {
                        let (k, v) = kv.split_once('=').unwrap_or_else(|| die(2, format!("argument '{kv}' is not key=value")));
                        // Numbers and booleans are typed; quote nothing.
                        let val: Value = serde_json::from_str(v).ok().filter(|x: &Value| !x.is_object() && !x.is_array()).unwrap_or(json!(v));
                        a.insert(k.to_string(), val);
                    }
                    let v = c
                        .call_timeout("action.run", json!({"name": name, "args": a}), Duration::from_secs(600))
                        .unwrap_or_else(|e| die(exit_code(&e), e));
                    print(args, &v, |v| {
                        if !v["result"].is_null() {
                            println!("{}", serde_json::to_string_pretty(&v["result"]).unwrap())
                        }
                    });
                }
                Some(o) => die(2, format!("unknown action command '{o}'")),
            }
        }
        "config" => {
            let pos = f.positional();
            match pos.first().map(|s| s.as_str()) {
                Some("reload") => {
                    let v = call(c, "config.reload", json!({}));
                    print(args, &v, |v| {
                        match v["error"].as_str() {
                            Some(e) => println!("reload failed, previous configuration kept:\n{e}"),
                            None => println!("reloaded"),
                        }
                        for d in v["diagnostics"].as_array().into_iter().flatten() {
                            println!("warning: {}", d.as_str().unwrap_or(""));
                        }
                    });
                    if v["ok"] != true {
                        exit(1);
                    }
                }
                Some("show") | None => {
                    let v = call(c, "config.get", json!({}));
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                }
                Some(o) => die(2, format!("unknown config command '{o}'")),
            }
        }
        other => die(2, format!("unknown command '{other}'; see hearth help")),
    }
}
