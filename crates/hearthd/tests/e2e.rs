//! End-to-end tests: real hearthd processes, real PTYs, real programs,
//! driven through the public protocol and CLI. Run `cargo build` first so
//! the `hearth` CLI binary exists next to hearthd.

use hearth_core::conn::{Client, ConnectError, Incoming, Target};
use hearth_core::proto::{self, Frame};
use hearthd::model::Model;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

static N: AtomicU32 = AtomicU32::new(0);

struct Daemon {
    child: Option<Child>,
    dir: PathBuf,
    sock: PathBuf,
}

fn bin(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_BIN_EXE_hearthd")).with_file_name(name);
    assert!(p.exists(), "{} missing: run `cargo build` before the tests", p.display());
    p
}

impl Daemon {
    fn dir() -> PathBuf {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
        let d = PathBuf::from(base).join(format!("he2e-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(d.join("cfg")).unwrap();
        d
    }

    fn start(dir: PathBuf, config: Option<&str>, args: &[&str]) -> Daemon {
        if let Some(c) = config {
            std::fs::write(dir.join("cfg/config.lua"), c).unwrap();
        }
        let sock = dir.join("s.sock");
        let mut cmd = Command::new(bin("hearthd"));
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(std::fs::File::create(dir.join("daemon.log")).unwrap());
        for (k, v) in Self::env_for(&dir) {
            cmd.env(k, v);
        }
        let child = cmd.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&sock).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start: {}", std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon { child: Some(child), dir, sock }
    }

    fn new(config: Option<&str>) -> Daemon {
        Self::start(Self::dir(), config, &["--no-persist"])
    }

    fn env_for(dir: &Path) -> Vec<(String, String)> {
        vec![
            ("HEARTH_RUNTIME_DIR".into(), dir.display().to_string()),
            ("HEARTH_SOCKET".into(), dir.join("s.sock").display().to_string()),
            ("HEARTH_STATE_DIR".into(), dir.join("state").display().to_string()),
            ("HEARTH_CONFIG_DIR".into(), dir.join("cfg").display().to_string()),
            // Debug builds panic on this output (see block.rs `test_panic`).
            ("HEARTH_TEST_PANIC_ON".into(), "HEARTH-TEST-PANIC-7f3a".into()),
        ]
    }

    fn client(&self) -> (Client, Receiver<Incoming>) {
        Client::connect(&Target::Unix(self.sock.clone()), "app", "e2e").unwrap()
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new(bin("hearth"));
        cmd.args(args).env_remove("HEARTH_BLOCK").env_remove("HEARTH_SESSION");
        for (k, v) in Self::env_for(&self.dir) {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn stop(&mut self, signal: i32) {
        if let Some(mut c) = self.child.take() {
            unsafe { libc::kill(c.id() as i32, signal) };
            let _ = c.wait();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop(libc::SIGTERM);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn call(c: &Client, m: &str, p: Value) -> Value {
    c.call_timeout(m, p, Duration::from_secs(20)).unwrap_or_else(|e| panic!("{m}: {e}"))
}

fn sh(script: &str) -> Value {
    json!(["sh", "-c", script])
}

fn new_session(c: &Client, cmd: Value) -> String {
    let v = call(c, "session.create", json!({"command": cmd, "cols": 60, "rows": 12}));
    v["block"].as_str().unwrap().to_string()
}

fn wait_text(c: &Client, block: &str, text: &str) {
    call(c, "terminal.wait_text", json!({"block": block, "text": text, "timeout_ms": 10000}));
}

/// Collects a subscription's stream: snapshot + data with continuity checks.
struct Stream {
    sub: u32,
    snapshot: Option<(u64, Vec<u8>)>,
    data: Vec<u8>,
    next: u64,
}

impl Stream {
    fn new(sub: u32) -> Stream {
        Stream { sub, snapshot: None, data: vec![], next: 0 }
    }
    fn take(&mut self, m: &Incoming) {
        match m {
            Incoming::Snapshot { sub, offset, bytes } if *sub == self.sub => {
                self.snapshot = Some((*offset, bytes.clone()));
                self.next = *offset;
            }
            Incoming::Data { sub, offset, bytes } if *sub == self.sub => {
                assert_eq!(*offset, self.next, "sub {} gap/duplicate: got offset {offset}, expected {}", self.sub, self.next);
                self.next += bytes.len() as u64;
                self.data.extend_from_slice(bytes);
            }
            _ => {}
        }
    }
    fn model(&self, cols: u16, rows: u16) -> Model {
        let mut m = Model::new(cols, rows, 100_000);
        m.advance(&self.snapshot.as_ref().expect("snapshot").1);
        m.advance(&self.data);
        m.take_effects();
        m
    }
}

fn drain(rx: &Receiver<Incoming>, streams: &mut [&mut Stream], quiet: Duration) {
    while let Ok(m) = rx.recv_timeout(quiet) {
        for s in streams.iter_mut() {
            s.take(&m);
        }
    }
}

// ---- P01/P02: server owns programs --------------------------------------

#[test]
fn p02_client_exit_and_crash_keep_the_same_process() {
    let d = Daemon::new(None);
    let file = d.dir.join("count");
    let (c, _rx) = d.client();
    let b = new_session(&c, sh(&format!("i=0; while :; do i=$((i+1)); echo $i >> {}; echo tick $i; sleep 0.05; done", file.display())));
    let before = call(&c, "block.info", json!({"block": b}));
    call(&c, "terminal.attach", json!({"block": b}));
    drop(c); // clean client exit
    // A client process killed with SIGKILL while attached.
    let mut crash = Command::new(bin("hearth"))
        .args(["events"])
        .envs(Daemon::env_for(&d.dir))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    crash.kill().unwrap();
    let _ = crash.wait();
    let n1: u64 = std::fs::read_to_string(&file).unwrap().lines().last().unwrap().parse().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let (c, _rx) = d.client();
    let after = call(&c, "block.info", json!({"block": b}));
    let n2: u64 = std::fs::read_to_string(&file).unwrap().lines().last().unwrap().parse().unwrap();
    assert_eq!(before["pid"], after["pid"]);
    assert_eq!(before["start_time"], after["start_time"], "same process, not a new one with a reused pid");
    assert!(after["exit"].is_null());
    assert!(n2 > n1, "program kept running while detached ({n1} -> {n2})");
    let sessions = call(&c, "session.list", json!({}));
    assert_eq!(sessions.as_array().unwrap().len(), 1, "no replacement session/shell appeared");
}

#[test]
fn p01_cli_autostarts_a_personal_server_offline() {
    let dir = Daemon::dir();
    let mut cmd = Command::new(bin("hearth"));
    cmd.args(["--json", "new", "--", "sh", "-c", "echo ready; sleep 5"]);
    let env = Daemon::env_for(&dir);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.env("HEARTHD", bin("hearthd"));
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let (c, _rx) = Client::connect(&Target::Unix(dir.join("s.sock")), "cli", "t").unwrap();
    wait_text(&c, v["block"].as_str().unwrap(), "ready");
    let st = call(&c, "server.status", json!({}));
    assert_eq!(st["auto_exit"], true);
    call(&c, "server.stop", json!({"force": true}));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- P03: exact terminal state on attach ----------------------------------

#[test]
fn p03_repeated_attach_during_output_is_exact() {
    let d = Daemon::new(None);
    let (c, rx) = d.client();
    let b = new_session(&c, sh("i=0; while [ $i -lt 4000 ]; do i=$((i+1)); printf '\\033[3%dm%05d \\303\\251 \\346\\227\\245\\346\\234\\254\\033[0m\\n' $((i%8)) $i; done; printf END; sleep 2"));
    let mut streams = vec![];
    for _ in 0..12 {
        let v = call(&c, "terminal.attach", json!({"block": b}));
        streams.push(Stream::new(v["sub"].as_u64().unwrap() as u32));
        std::thread::sleep(Duration::from_millis(15));
    }
    wait_text(&c, &b, "END");
    let fin = call(&c, "terminal.attach", json!({"block": b}));
    let mut last = Stream::new(fin["sub"].as_u64().unwrap() as u32);
    {
        let mut refs: Vec<&mut Stream> = streams.iter_mut().collect();
        refs.push(&mut last);
        drain(&rx, &mut refs, Duration::from_millis(500));
    }
    let server_text = call(&c, "terminal.capture", json!({"block": b, "history": true}))["content"].as_str().unwrap().to_string();
    assert!(server_text.contains("04000"));
    let offsets: Vec<u64> = streams.iter().map(|s| s.snapshot.as_ref().unwrap().0).collect();
    assert!(offsets.windows(2).any(|w| w[0] != w[1]), "attaches happened at different offsets: {offsets:?}");
    for s in streams.iter().chain(std::iter::once(&last)) {
        let m = s.model(60, 12);
        assert_eq!(m.capture_text(true, false), server_text, "sub {} (snapshot at {}) diverged", s.sub, s.snapshot.as_ref().unwrap().0);
        assert_eq!(m.snapshot(), last.model(60, 12).snapshot(), "full state (modes, cursor, colors) diverged");
    }
}

#[test]
fn p03_alternate_screen_and_modes_survive_reattach() {
    let d = Daemon::new(None);
    let (c, rx) = d.client();
    let b = new_session(&c, sh("printf 'shell line\\n'; printf '\\033[?1049h\\033[2J\\033[HTOP\\033[5;3H\\033[1;32mgreen\\033[?1h\\033[?2004h\\033]2;edit\\007READY'; sleep 30"));
    wait_text(&c, &b, "READY");
    let v = call(&c, "terminal.attach", json!({"block": b}));
    let mut s = Stream::new(v["sub"].as_u64().unwrap() as u32);
    drain(&rx, &mut [&mut s], Duration::from_millis(300));
    let m = s.model(60, 12);
    let server = call(&c, "terminal.capture", json!({"block": b}));
    assert!(server["alternate_screen"] == true);
    assert!(m.alt_screen());
    assert_eq!(m.capture_text(false, false), server["content"].as_str().unwrap());
    use alacritty_terminal::term::TermMode;
    assert!(m.mode().contains(TermMode::APP_CURSOR | TermMode::BRACKETED_PASTE));
    assert_eq!(m.title.as_deref(), Some("edit"));
    assert_eq!(m.cursor(), (4, 12));
}

#[test]
fn p03_detached_output_is_drained() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    // 8 MiB with no viewer attached: far above the PTY buffer.
    let b = new_session(&c, sh("head -c 8388608 /dev/zero | tr '\\0' x; echo; echo FINISHED"));
    let t = Instant::now();
    let r = call(&c, "terminal.wait", json!({"block": b, "timeout_ms": 20000}));
    assert_eq!(r["code"], 0);
    assert!(t.elapsed() < Duration::from_secs(20));
    let info = call(&c, "block.info", json!({"block": b}));
    assert!(info["output_bytes"].as_u64().unwrap() > 8_000_000);
}

#[test]
fn p03_server_is_the_only_query_responder() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let script = "printf '\\033[3;5H\\033[6n'; IFS= read -rsd R -t 3 a && echo \"got1:${a#*[}\"; IFS= read -rsd R -t 1 b && echo got2 || echo none2; sleep 5";
    // Zero viewers.
    let b0 = new_session(&c, json!(["bash", "--norc", "--noprofile", "-c", script]));
    wait_text(&c, &b0, "none2");
    let t = call(&c, "terminal.capture", json!({"block": b0}))["content"].as_str().unwrap().to_string();
    assert!(t.contains("got1:3;5"), "{t}");
    // Two viewers attached (protocol clients never answer; the GUI filters queries).
    let (c2, _rx2) = d.client();
    let b2 = new_session(&c, json!(["bash", "--norc", "--noprofile", "-c", format!("sleep 0.5; {script}")]));
    call(&c, "terminal.attach", json!({"block": b2}));
    call(&c2, "terminal.attach", json!({"block": b2}));
    wait_text(&c, &b2, "none2");
    let t = call(&c, "terminal.capture", json!({"block": b2}))["content"].as_str().unwrap().to_string();
    assert!(t.contains("got1:3;5") && !t.contains("got2"), "{t}");
}

// ---- P04-P07: structure, independence, geometry ----------------------------

#[test]
fn p04_p05_structure_ids_and_layout() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let v = call(&c, "session.create", json!({"name": "proj"}));
    let (s, w1, b1) = (v["session"].as_str().unwrap().to_string(), v["window"].as_str().unwrap().to_string(), v["block"].as_str().unwrap().to_string());
    let w2 = call(&c, "window.create", json!({"session": s, "name": "logs"}))["window"].as_str().unwrap().to_string();
    let b2 = call(&c, "block.split", json!({"block": b1, "direction": "right"}))["block"].as_str().unwrap().to_string();
    let b3 = call(&c, "block.split", json!({"block": b2, "direction": "down"}))["block"].as_str().unwrap().to_string();
    call(&c, "block.resize", json!({"block": b1, "direction": "right", "amount": 0.1}));
    call(&c, "layout.balance", json!({"window": w1}));
    call(&c, "block.zoom", json!({"block": b3}));
    call(&c, "block.swap", json!({"block": b1, "other": b3}));
    call(&c, "session.rename", json!({"session": s, "name": "renamed"}));
    call(&c, "window.move", json!({"window": w2, "index": 0}));
    let view = call(&c, "session.view", json!({"session": s}));
    assert_eq!(view["name"], "renamed");
    assert_eq!(view["windows"][0]["id"], w2.as_str(), "reordered, same id");
    assert_eq!(view["windows"][1]["id"], w1.as_str());
    assert_eq!(view["windows"][1]["zoom"], b3.as_str());
    // Ambiguous names fail before mutation.
    call(&c, "session.create", json!({"name": "dup"}));
    call(&c, "session.create", json!({"name": "dup"}));
    let e = c.call("session.rename", json!({"session": "dup", "name": "x"})).unwrap_err();
    assert_eq!(e.code, "ambiguous");
    assert_eq!(call(&c, "session.list", json!({})).as_array().unwrap().iter().filter(|s| s["name"] == "dup").count(), 2);
    // Closing the last pane of a window closes the window; ids are not reused.
    call(&c, "window.close", json!({"window": w2}));
    let b4 = call(&c, "block.split", json!({"block": b2, "direction": "left"}))["block"].as_str().unwrap().to_string();
    for old in [&b1, &b2, &b3] {
        assert_ne!(&b4, old);
    }
    let view = call(&c, "session.view", json!({"session": s}));
    assert_eq!(view["windows"].as_array().unwrap().len(), 1);
    // CLI over the same server: stable JSON.
    let out = d.cli(&["--json", "ls"]);
    let ls: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(ls.as_array().unwrap().iter().any(|x| x["id"] == s.as_str()));
}

#[test]
fn p06_p07_size_ownership_and_independent_viewers() {
    let d = Daemon::new(None);
    let (a, rxa) = d.client();
    let (b, rxb) = d.client();
    let blk = new_session(&a, json!(["sh"]));
    let sa = call(&a, "terminal.attach", json!({"block": blk}))["sub"].as_u64().unwrap();
    let sb = call(&b, "terminal.attach", json!({"block": blk}))["sub"].as_u64().unwrap();
    let r = call(&a, "terminal.resize", json!({"sub": sa, "cols": 100, "rows": 30}));
    assert_eq!(r["applied"], true, "first viewer to report a size owns an unowned block");
    let r = call(&b, "terminal.resize", json!({"sub": sb, "cols": 50, "rows": 15}));
    assert_eq!(r["applied"], false, "non-owner preference does not resize the PTY");
    let info = call(&a, "block.info", json!({"block": blk}));
    assert_eq!((info["cols"].as_u64(), info["rows"].as_u64()), (Some(100), Some(30)));
    // Resize ping-pong: only the owner's sizes apply.
    for i in 0..40 {
        let _ = a.call("terminal.resize", json!({"sub": sa, "cols": 100 + (i % 2), "rows": 30}));
        let _ = b.call("terminal.resize", json!({"sub": sb, "cols": 50 + (i % 2), "rows": 15}));
    }
    let info = call(&a, "block.info", json!({"block": blk}));
    assert!(info["cols"].as_u64().unwrap() >= 100);
    // Typing transfers ownership; the new owner's size applies.
    b.send_input(sb as u32, 1, b"true\r").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let info = call(&a, "block.info", json!({"block": blk}));
    assert_eq!((info["cols"].as_u64(), info["rows"].as_u64()), (Some(51), Some(15)), "{info}");
    assert_eq!(info["owner"], b.welcome["client_id"]);
    // Both viewers were told, in stream order.
    let resized = |rx: &Receiver<Incoming>| rx.try_iter().filter(|m| matches!(m, Incoming::Event(v) if v["event"] == "terminal.resized")).count();
    assert!(resized(&rxa) >= 2 && resized(&rxb) >= 2);
    // Viewer independence: B detaching doesn't affect A's stream.
    call(&b, "terminal.detach", json!({"sub": sb}));
    a.send_input(sa as u32, 1, b"echo still-here\r").unwrap();
    wait_text(&a, &blk, "still-here");
    // Owner disconnect hands ownership back.
    drop(b);
    std::thread::sleep(Duration::from_millis(300));
    let info = call(&a, "block.info", json!({"block": blk}));
    assert_eq!(info["owner"], a.welcome["client_id"]);
    assert_eq!(info["cols"].as_u64(), Some(101));
}

// ---- P10-like transport: resume, slow viewer ---------------------------------

#[test]
fn resume_within_window_and_fresh_attach_after() {
    let d = Daemon::new(None);
    let (c, rx) = d.client();
    let b = new_session(&c, sh("i=0; while [ $i -lt 200 ]; do i=$((i+1)); echo line $i; sleep 0.01; done; echo DONE; sleep 30"));
    let v = call(&c, "terminal.attach", json!({"block": b}));
    let generation = v["generation"].as_str().unwrap().to_string();
    let mut s = Stream::new(v["sub"].as_u64().unwrap() as u32);
    drain(&rx, &mut [&mut s], Duration::from_millis(200));
    let have = s.next;
    drop(c); // connection lost
    let (c, rx) = d.client();
    wait_text(&c, &b, "DONE");
    let v = call(&c, "terminal.attach", json!({"block": b, "resume": {"generation": generation, "offset": have}}));
    assert_eq!(v["mode"], "resume");
    let mut r = Stream { sub: v["sub"].as_u64().unwrap() as u32, snapshot: s.snapshot.clone(), data: s.data.clone(), next: have };
    drain(&rx, &mut [&mut r], Duration::from_millis(300));
    let server = call(&c, "terminal.capture", json!({"block": b, "history": true}));
    assert_eq!(r.model(60, 12).capture_text(true, false), server["content"].as_str().unwrap(), "resumed state is exact");
    // Wrong generation (e.g. another server incarnation) -> fresh snapshot.
    let v = call(&c, "terminal.attach", json!({"block": b, "resume": {"generation": "old-x", "offset": have}}));
    assert_eq!(v["mode"], "snapshot");
    // Offset beyond what exists -> fresh snapshot, never garbage.
    let v = call(&c, "terminal.attach", json!({"block": b, "resume": {"generation": generation, "offset": 1u64 << 40}}));
    assert_eq!(v["mode"], "snapshot");
}

#[test]
fn slow_viewer_is_cut_loose_without_stalling_others() {
    let d = Daemon::new(None);
    let (c, rx) = d.client();
    // More than the 8 MiB a connection may queue, so the slow viewer overflows.
    let b = new_session(&c, sh("sleep 0.5; head -c 20000000 /dev/zero | tr '\\0' y; echo; echo FINISHED; sleep 30"));
    // A raw client that attaches and then never reads.
    let mut slow = std::os::unix::net::UnixStream::connect(&d.sock).unwrap();
    proto::write_frame(&mut slow, &Frame::Json(json!({"type":"hello","protocol":"hearth.v1","min_version":1,"max_version":1,"client":{"kind":"app"}}))).unwrap();
    let mut r = std::io::BufReader::new(slow.try_clone().unwrap());
    proto::read_frame(&mut r).unwrap();
    proto::write_frame(&mut slow, &Frame::Json(json!({"type":"request","id":1,"method":"terminal.attach","params":{"block": b}}))).unwrap();
    let v = call(&c, "terminal.attach", json!({"block": b}));
    let sub = v["sub"].as_u64().unwrap() as u32;
    let t = Instant::now();
    // A debug build on a busy CI runner can need more than wait_text's 10 s,
    // so wait as long as the assertion below allows.
    c.call_timeout("terminal.wait_text", json!({"block": b, "text": "FINISHED", "timeout_ms": 30000}), Duration::from_secs(35)).unwrap();
    assert!(t.elapsed() < Duration::from_secs(30), "workload was not blocked by the slow viewer");
    // The healthy viewer received a continuous stream (Stream asserts it).
    let mut s = Stream::new(sub);
    while let Ok(m) = rx.recv_timeout(Duration::from_millis(300)) {
        s.take(&m);
    }
    assert!(s.next > 20_000_000);
    // The slow viewer is told explicitly that it lost the stream.
    slow.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut told = false;
    while let Ok(Some(f)) = proto::read_frame(&mut r) {
        if let Frame::Json(v) = f {
            if v["event"] == "terminal.lost" || v["type"] == "close" {
                told = true;
                break;
            }
        }
    }
    assert!(told, "slow viewer must get terminal.lost or close, not silent gaps");
}

// ---- P11-P13: CLI, introspection, capture, events ------------------------------

#[test]
fn p12_every_described_method_exists_and_validates() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let list = call(&c, "api.list", json!({}));
    let names: Vec<String> = list.as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap().to_string()).collect();
    assert!(names.len() >= 40);
    for n in &names {
        let desc = call(&c, "api.describe", json!({"method": n}));
        assert_eq!(desc["params"]["type"], "object", "{n}");
        // Unknown parameters are rejected by the running server for every method.
        let e = c.call(n, json!({"__bogus": 1})).unwrap_err();
        assert_eq!(e.code, "invalid_params", "{n}: {e}");
        // Methods with examples return results matching their schema.
        if !desc["example"].is_null() {
            let r = call(&c, n, desc["example"].clone());
            let ty = desc["result"]["type"].as_str().unwrap_or("any");
            match ty {
                "object" => {
                    for k in desc["result"]["required"].as_array().into_iter().flatten() {
                        assert!(!r[k.as_str().unwrap()].is_null(), "{n}: result lacks {k}: {r}");
                    }
                }
                "array" => assert!(r.is_array(), "{n}"),
                _ => {}
            }
        }
    }
    assert_eq!(c.call("no.such", json!({})).unwrap_err().code, "unknown_method");
}

#[test]
fn p11_p13_cli_json_capture_wait_and_exit_codes() {
    let d = Daemon::new(None);
    let out = d.cli(&["--json", "run", "-p", "session", "--", "sh", "-c", "printf '\\033[1;31mbold red\\033[0m <tag>\\n'; exit 7"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let b = v["block"].as_str().unwrap();
    let w = d.cli(&["wait", "-t", b, "--timeout", "5000"]);
    assert_eq!(w.status.code(), Some(7), "wait propagates the child's exit status");
    let t = d.cli(&["capture", "-t", b]);
    assert!(String::from_utf8_lossy(&t.stdout).contains("bold red <tag>"));
    let h = d.cli(&["capture", "-t", b, "--format", "html"]);
    let h = String::from_utf8_lossy(&h.stdout);
    assert!(h.contains("&lt;tag&gt;") && h.contains("font-weight:bold"), "{h}");
    let vt = d.cli(&["--json", "capture", "-t", b, "--format", "vt"]);
    let vt: Value = serde_json::from_slice(&vt.stdout).unwrap();
    let mut m = Model::new(vt["cols"].as_u64().unwrap() as u16, vt["rows"].as_u64().unwrap() as u16, 1000);
    m.advance(vt["content"].as_str().unwrap().as_bytes());
    assert!(m.capture_text(false, false).contains("bold red <tag>"));
    // Failures: nonzero, structured, nothing on stdout.
    let e = d.cli(&["capture", "-t", "b999"]);
    assert_eq!(e.status.code(), Some(3));
    assert!(e.stdout.is_empty() && !e.stderr.is_empty());
    let e = d.cli(&["capture"]);
    assert_eq!(e.status.code(), Some(2), "no target outside Hearth: usage error, not the frontmost pane");
    let e = d.cli(&["wait", "-t", b, "--text", "never", "--timeout", "300"]);
    assert_ne!(e.status.code(), Some(0));
}

#[test]
fn p13_events_are_ordered_and_bootstrap_is_atomic() {
    let d = Daemon::new(None);
    let (c, rx) = d.client();
    let boot = call(&c, "session.bootstrap", json!({}));
    let seq0 = boot["seq"].as_u64().unwrap();
    let b = new_session(&c, sh("printf '\\033]2;T1\\007'; printf '\\033]7501;state=blocked:kind=permission:msg=T2s/\\033\\\\'; sleep 0.2; exit 3"));
    let mut seqs = vec![];
    let mut names = vec![];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !names.iter().any(|n: &String| n == "terminal.exit") {
        if let Ok(Incoming::Event(v)) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(s) = v["seq"].as_u64() {
                seqs.push(s);
                names.push(v["event"].as_str().unwrap().to_string());
                if v["event"] == "terminal.status" && !v["data"]["records"].as_array().unwrap().is_empty() {
                    assert_eq!(v["data"]["records"][0]["kind"], "permission");
                }
                if v["event"] == "terminal.exit" {
                    assert_eq!(v["data"]["code"], 3);
                    assert_eq!(v["data"]["block"], b.as_str());
                }
            }
        }
    }
    assert!(seqs.iter().all(|s| *s > seq0), "events after bootstrap have higher seq");
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "contiguous, ordered: {seqs:?}");
    for n in ["session.changed", "terminal.title", "terminal.status", "terminal.exit"] {
        assert!(names.iter().any(|x| x == n), "missing {n} in {names:?}");
    }
    // Exit cleared the blocked record (working/blocked expire on exit).
    let st = call(&c, "terminal.status", json!({"block": b}));
    assert!(st["records"].as_array().unwrap().is_empty(), "{st}");
}

// ---- P14: Lua -----------------------------------------------------------------

const CONFIG: &str = r#"
local ok = pcall(hearth.call, "session.list")
hearth.bind("ctrl+shift+t", "new-tab")
hearth.bind("ctrl+a c", ok and "call-allowed" or "call-refused")
hearth.bind("hyper+x", "broken")
hearth.mode{ name = "resize", exclusive = true, bindings = { ["h"] = "grow-left", ["bad+key+zz"] = "x" } }
hearth.host{ name = "box", address = "ssh:me@box" }
hearth.set{ history = 5000, action_timeout_ms = 400, nonsense = 1 }
hearth.action{ name = "mk", title = "Make session", args = { { name = "name", type = "string", required = true }, { name = "n", type = "integer" } },
  run = function(a, ctx) local r = hearth.api.session_create{ name = a.name }; return { session = r.session, n = a.n } end }
hearth.action{ name = "spin", run = function() while true do end end }
hearth.action{ name = "boom", run = function() error("kaput") end }
"#;

#[test]
fn p14_lua_config_actions_reload_and_errors() {
    let d = Daemon::new(Some(CONFIG));
    let (c, _rx) = d.client();
    let cfg = call(&c, "config.get", json!({}));
    let binds: Vec<&str> = cfg["bindings"].as_array().unwrap().iter().map(|b| b["action"].as_str().unwrap()).collect();
    assert_eq!(binds, ["new-tab", "call-refused"], "live API refused during load; invalid binding skipped");
    let diags = cfg["diagnostics"].to_string();
    assert!(diags.contains("hyper") && diags.contains("bad+key+zz") && diags.contains("nonsense"), "{diags}");
    assert_eq!(cfg["modes"][0]["exclusive"], true);
    assert_eq!(cfg["hosts"][0]["name"], "box");
    assert_eq!(cfg["settings"]["history"], 5000);
    // Typed arguments.
    assert_eq!(c.call("action.run", json!({"name": "mk", "args": {}})).unwrap_err().code, "invalid_params");
    assert_eq!(c.call("action.run", json!({"name": "mk", "args": {"name": "x", "n": "two"}})).unwrap_err().code, "invalid_params");
    let r = call(&c, "action.run", json!({"name": "mk", "args": {"name": "from-lua", "n": 2}}));
    assert_eq!(r["result"]["n"], 2);
    let sid = r["result"]["session"].as_str().unwrap();
    assert_eq!(call(&c, "session.view", json!({"session": sid}))["name"], "from-lua");
    // Slow callbacks are cut off and don't block the server.
    let t = Instant::now();
    assert_eq!(c.call_timeout("action.run", json!({"name": "spin"}), Duration::from_secs(10)).unwrap_err().code, "timeout");
    assert!(t.elapsed() < Duration::from_secs(5));
    let e = c.call("action.run", json!({"name": "boom"})).unwrap_err();
    assert!(e.message.contains("kaput"));
    // Syntax error on reload keeps the last good configuration.
    std::fs::write(d.dir.join("cfg/config.lua"), "hearth.bind(\"ctrl+x\", \"y\"\n").unwrap();
    let r = call(&c, "config.reload", json!({}));
    assert_eq!(r["ok"], false);
    assert!(r["error"].as_str().unwrap().contains("config.lua"));
    let cfg = call(&c, "config.get", json!({}));
    assert_eq!(cfg["actions"].as_array().unwrap().len(), 3, "previous actions still loaded");
    // A good reload replaces it.
    std::fs::write(d.dir.join("cfg/config.lua"), "hearth.action{ name = 'only', run = function() return 1 end }").unwrap();
    assert_eq!(call(&c, "config.reload", json!({}))["ok"], true);
    assert_eq!(call(&c, "action.run", json!({"name": "only"}))["result"], 1);
}

#[test]
fn p14_lua_scripts_drive_a_live_server_with_events() {
    let d = Daemon::new(None);
    let script = d.dir.join("s.lua");
    std::fs::write(
        &script,
        r#"
local r, code
hearth.on("terminal.exit", function(name, data)
  if r and data.block == r.block then code = data.code; hearth.stop() end
end)
hearth.on("terminal.exit", function() error("handler errors are reported, not fatal") end)
r = hearth.api.session_create{ command = { "sh", "-c", "sleep 0.3; echo hi; exit 4" } }
hearth.run(5000)
print("exit=" .. tostring(code) .. " arg1=" .. tostring(arg[1]))
"#,
    )
    .unwrap();
    let out = d.cli(&["lua", script.to_str().unwrap(), "x"]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(so.contains("exit=4"), "{so}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("handler errors are reported"));
}

// ---- P15: agent integration ---------------------------------------------------------

#[test]
fn p15_agent_setup_is_idempotent_and_reversible() {
    let d = Daemon::new(None);
    let home = d.dir.join("agents");
    let cdir = home.join("claude");
    let xdir = home.join("codex");
    std::fs::create_dir_all(&cdir).unwrap();
    std::fs::create_dir_all(&xdir).unwrap();
    let original = "{\n  \"zeta\": 1,\n  \"model\": \"x\",\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"mine\"\n          }\n        ]\n      }\n    ]\n  },\n  \"alpha\": true\n}\n";
    std::fs::write(cdir.join("settings.json"), original).unwrap();
    std::fs::write(xdir.join("AGENTS.md"), "# My rules\n\nBe nice.\n").unwrap();
    let args = |sub: &str| vec!["agent".to_string(), sub.to_string(), "--claude-dir".into(), cdir.display().to_string(), "--codex-dir".into(), xdir.display().to_string()];
    let run = |a: Vec<String>| d.cli(&a.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let dry = run([args("setup"), vec!["--dry-run".into()]].concat());
    assert!(String::from_utf8_lossy(&dry.stdout).contains("would update"));
    assert_eq!(std::fs::read_to_string(cdir.join("settings.json")).unwrap(), original, "dry run changes nothing");
    assert!(run(args("setup")).status.success());
    let s1 = std::fs::read_to_string(cdir.join("settings.json")).unwrap();
    let again = run(args("setup"));
    assert!(String::from_utf8_lossy(&again.stdout).contains("nothing to change"), "setup twice is idempotent");
    let v: Value = serde_json::from_str(&s1).unwrap();
    let pos: Vec<usize> = ["\"zeta\"", "\"model\"", "\"hooks\"", "\"alpha\""].iter().map(|k| s1.find(k).unwrap()).collect();
    assert!(pos.windows(2).all(|w| w[0] < w[1]), "unrelated keys and their order preserved:\n{s1}");
    assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "mine");
    assert!(s1.contains("hearth agent context"));
    assert!(!s1.contains("hearth agent status"), "Claude reports its own status; no status hooks:\n{s1}");
    assert!(cdir.join("skills/hearth/SKILL.md").exists());
    assert!(std::fs::read_to_string(xdir.join("AGENTS.md")).unwrap().starts_with("# My rules\n\nBe nice.\n"));
    assert!(run(args("remove")).status.success());
    let after: Value = serde_json::from_str(&std::fs::read_to_string(cdir.join("settings.json")).unwrap()).unwrap();
    let before: Value = serde_json::from_str(original).unwrap();
    assert_eq!(after, before, "removal restores the original settings");
    assert!(!cdir.join("skills/hearth").exists());
    assert_eq!(std::fs::read_to_string(xdir.join("AGENTS.md")).unwrap(), "# My rules\n\nBe nice.\n");
    // The context hook reports the agent's own terminal.
    let mut cmd = Command::new(bin("hearth"));
    let o = cmd.args(["agent", "context"]).env("HEARTH_SESSION", "s9").env("HEARTH_BLOCK", "b42").output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("block b42"));
    // Status hooks installed by 0.1.1 are taken out again by setup.
    let old = json!({"hooks": {
        "SessionStart": [{"hooks": [{"type": "command", "command": "hearth agent context"}]}],
        "Stop": [{"hooks": [{"type": "command", "command": "hearth agent status done --app claude"}, {"type": "command", "command": "mine"}]}],
        "Notification": [{"hooks": [{"type": "command", "command": "hearth agent status blocked --app claude"}]}],
    }});
    std::fs::write(cdir.join("settings.json"), serde_json::to_string_pretty(&old).unwrap()).unwrap();
    assert!(run(args("setup")).status.success());
    let up: Value = serde_json::from_str(&std::fs::read_to_string(cdir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(up["hooks"]["SessionStart"].as_array().unwrap().len(), 1, "no duplicate context hook");
    assert_eq!(up["hooks"]["Stop"], json!([{"hooks": [{"type": "command", "command": "mine"}]}]), "only ours removed");
    assert!(up["hooks"].get("Notification").is_none());
}

#[test]
fn agent_status_hook_reports_on_its_own_tab_and_is_silent_elsewhere() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    // Run by a hook inside a Hearth terminal: hook JSON on stdin.
    let script = format!(
        "printf '%s' '{{\"hook_event_name\":\"Notification\",\"message\":\"Claude needs your permission to use Bash\"}}' | {} agent status blocked --app claude; echo MARK-$((1+1)); sleep 600",
        bin("hearth").display()
    );
    let b = new_session(&c, sh(&script));
    wait_text(&c, &b, "MARK-2");
    let st = call(&c, "terminal.status", json!({"block": b}));
    let r = &st["records"][0];
    assert_eq!(r["state"], "blocked", "{st}");
    assert_eq!(r["app"], "claude", "{st}");
    assert_eq!(r["msg"], "Claude needs your permission to use Bash", "{st}");
    // Nothing visible was printed into the terminal.
    assert!(!call(&c, "terminal.capture", json!({"block": b}))["content"].as_str().unwrap().contains("7501"));
    // Outside Hearth: no output, success, no error.
    let o = Command::new(bin("hearth")).args(["agent", "status", "done", "--app", "claude"]).env_remove("HEARTH_BLOCK").stdin(Stdio::null()).output().unwrap();
    assert!(o.status.success() && o.stdout.is_empty() && o.stderr.is_empty(), "{o:?}");
}

#[test]
fn p15_cli_inside_a_terminal_targets_its_own_block() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let hearth = bin("hearth");
    // Two sessions; the "frontmost" (focused, newest) is the other one.
    let mine = new_session(&c, json!(["sh", "-c", format!("sleep 0.5; {} send 'echo from-agent' && {} keys Enter; sleep 30", hearth.display(), hearth.display())]));
    let other = new_session(&c, json!(["sh"]));
    call(&c, "block.focus", json!({"block": other}));
    // HEARTH_SOCKET is inherited from the daemon environment.
    std::thread::sleep(Duration::from_millis(1500));
    let t_mine = call(&c, "terminal.capture", json!({"block": mine}))["content"].as_str().unwrap().to_string();
    let t_other = call(&c, "terminal.capture", json!({"block": other}))["content"].as_str().unwrap().to_string();
    assert!(t_mine.contains("from-agent"), "{t_mine}");
    assert!(!t_other.contains("from-agent"));
}

// ---- P16: program status lifecycle ------------------------------------------------

#[test]
fn p16_osc7501_records_and_prompt_lifetime() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let b = new_session(&c, json!(["bash", "--norc", "--noprofile"]));
    let send = |t: &str| call(&c, "terminal.send_text", json!({"block": b, "text": format!("{t}\r")}));
    send("printf '\\033]7501;state=working:app=deploy:msg=RGVwbG95aW5n\\033\\\\'; printf '\\033]7501;state=done:id=us/east:title=VVM=\\033\\\\'; echo S$((1+0))");
    wait_text(&c, &b, "S1\n");
    let st = call(&c, "terminal.status", json!({"block": b}));
    let recs = st["records"].as_array().unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(st["summary"], "working");
    let east = recs.iter().find(|r| r["id"] == "us/east").unwrap();
    assert_eq!(east["app"], "deploy", "app inherited from ancestor");
    assert_eq!(east["title"], "US");
    // A shell prompt (OSC 133;A) ends working records; done survives.
    send("printf '\\033]133;A\\007'; echo S$((2+0))");
    wait_text(&c, &b, "S2\n");
    let st = call(&c, "terminal.status", json!({"block": b}));
    assert_eq!(st["records"].as_array().unwrap().len(), 1);
    assert_eq!(st["summary"], "done");
    // Malformed / oversized reports are ignored entirely.
    send(&format!("printf '\\033]7501;state=error:msg={}\\033\\\\'; printf '\\033]7501;state=bogus\\033\\\\'; echo S$((3+0))", "A".repeat(3000)));
    wait_text(&c, &b, "S3\n");
    assert_eq!(call(&c, "terminal.status", json!({"block": b}))["summary"], "done");
    // Feature detection reply comes from the server.
    // Echo goes off first: the reply can arrive before `read -s` would do it.
    send("stty -echo; printf '\\033]7501;?\\033\\\\'; IFS= read -rs -t 2 -d '\\' r; stty echo; printf '%s' \"$r\" | od -c | head -2; echo S$((4+0))");
    wait_text(&c, &b, "S4\n");
    let t = call(&c, "terminal.capture", json!({"block": b}))["content"].as_str().unwrap().to_string();
    assert!(t.contains("7   5   0   1   ;   ?"), "{t}");
}

// ---- P08/P09: hosts and tailnet identity ---------------------------------------------

fn fake_whois(dir: &Path, login: &str) -> PathBuf {
    let p = dir.join("whois.sh");
    std::fs::write(&p, format!("#!/bin/sh\n[ \"$1\" = whois ] || exit 1\necho '{{\"UserProfile\":{{\"LoginName\":\"{login}\"}}}}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn p09_tcp_identity_policy_with_stub_whois() {
    let dir = Daemon::dir();
    let who = fake_whois(&dir, "alice@example.com");
    let port = free_port();
    let cfg = format!("hearth.set{{ listen = {{ 'tcp:127.0.0.1:{port}' }}, allow = {{ 'alice@example.com' }}, whois = '{}' }}", who.display());
    let d = Daemon::start(dir, Some(&cfg), &["--no-persist"]);
    // The daemon opens the TCP listener after the Unix socket Daemon::start waits for.
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "tcp listener did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (c, _rx) = Client::connect(&Target::Tcp(format!("127.0.0.1:{port}")), "app", "t").unwrap();
    assert_eq!(c.welcome["principal"], "tailscale:alice@example.com");
    // Policy change: identity no longer allowed.
    std::fs::write(d.dir.join("cfg/config.lua"), format!("hearth.set{{ allow = {{ 'bob@example.com' }}, whois = '{}' }}", who.display())).unwrap();
    call(&c, "config.reload", json!({}));
    match Client::connect(&Target::Tcp(format!("127.0.0.1:{port}")), "app", "t") {
        Err(ConnectError::Rejected(r)) => assert!(r.contains("alice@example.com"), "{r}"),
        other => panic!("expected rejection, got {:?}", other.map(|_| ())),
    }
    // Empty allow list admits nobody.
    std::fs::write(d.dir.join("cfg/config.lua"), format!("hearth.set{{ whois = '{}' }}", who.display())).unwrap();
    call(&c, "config.reload", json!({}));
    assert!(matches!(Client::connect(&Target::Tcp(format!("127.0.0.1:{port}")), "app", "t"), Err(ConnectError::Rejected(_))));
    // Whois failure (not a tailnet peer) is a rejection.
    std::fs::write(d.dir.join("cfg/config.lua"), "hearth.set{ allow = { 'alice@example.com' }, whois = '/bin/false' }").unwrap();
    call(&c, "config.reload", json!({}));
    assert!(matches!(Client::connect(&Target::Tcp(format!("127.0.0.1:{port}")), "app", "t"), Err(ConnectError::Rejected(_))));
}

#[test]
fn p08_two_servers_route_to_the_right_host_via_proxy_transport() {
    let a = Daemon::new(None);
    let b = Daemon::new(None);
    // The ssh transport runs `hearth proxy` on the far side; exercise that
    // bridge locally with the second server's environment.
    let mut argv = vec!["env".to_string()];
    for (k, v) in Daemon::env_for(&b.dir) {
        argv.push(format!("{k}={v}"));
    }
    argv.push(bin("hearth").display().to_string());
    argv.push("proxy".into());
    let (ca, _ra) = a.client();
    let (cb, _rb) = Client::connect(&Target::Command(argv), "app", "t").unwrap();
    assert_ne!(ca.welcome["server"]["incarnation"], cb.welcome["server"]["incarnation"]);
    call(&ca, "session.create", json!({"name": "on-a"}));
    call(&cb, "session.create", json!({"name": "on-b"}));
    let la = call(&ca, "session.list", json!({}));
    let lb = call(&cb, "session.list", json!({}));
    assert_eq!(la.as_array().unwrap().len(), 1);
    assert_eq!(la[0]["name"], "on-a");
    assert_eq!(lb[0]["name"], "on-b");
}

// ---- Persistence / recovery ------------------------------------------------------

#[test]
fn layout_restores_after_server_stop_with_new_processes() {
    let dir = Daemon::dir();
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut d = Daemon::start(dir.clone(), None, &[]);
    let (c, _rx) = d.client();
    let v = call(&c, "session.create", json!({"name": "keep", "cwd": work}));
    let b1 = v["block"].as_str().unwrap().to_string();
    call(&c, "block.split", json!({"block": b1, "direction": "down", "cwd": work}));
    let old_pid = call(&c, "block.info", json!({"block": b1}))["pid"].clone();
    std::thread::sleep(Duration::from_millis(800));
    drop(c);
    d.stop(libc::SIGTERM);
    assert!(dir.join("state/state.json").exists());
    let d2 = Daemon::start(dir.clone(), None, &[]);
    let (c, _rx) = d2.client();
    let view = call(&c, "session.view", json!({"session": "keep"}));
    assert_eq!(view["id"], v["session"]);
    assert_eq!(view["windows"][0]["id"], v["window"]);
    let blocks = view["blocks"].as_object().unwrap();
    assert_eq!(blocks.len(), 2);
    for (id, info) in blocks {
        assert_ne!(id, &b1, "restored terminals are new blocks");
        assert_ne!(info["pid"], old_pid);
        assert!(info["restored_from"].is_string(), "labelled as restored: {info}");
        assert_eq!(info["cwd"]["process"].as_str(), Some(work.to_str().unwrap()));
    }
    drop(d2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn newer_state_schema_is_left_untouched() {
    let dir = Daemon::dir();
    std::fs::create_dir_all(dir.join("state")).unwrap();
    let future = r#"{"schema": 99, "future": true}"#;
    std::fs::write(dir.join("state/state.json"), future).unwrap();
    let d = Daemon::start(dir.clone(), None, &[]);
    let (c, _rx) = d.client();
    call(&c, "session.create", json!({}));
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(std::fs::read_to_string(dir.join("state/state.json")).unwrap(), future);
    assert_eq!(call(&c, "server.status", json!({}))["persist"], false);
    assert!(std::fs::read_to_string(dir.join("daemon.log")).unwrap().contains("newer"));
}

// ---- Local security ---------------------------------------------------------------------

#[test]
fn socket_and_runtime_dir_are_private() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = Daemon::new(None);
    let m = std::fs::metadata(&d.sock).unwrap();
    assert_eq!(m.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&d.dir).unwrap().mode() & 0o077, 0, "runtime dir is 0700");
    // A second server refuses to steal a live socket.
    let mut cmd = Command::new(bin("hearthd"));
    let out = cmd.envs(Daemon::env_for(&d.dir)).arg("--no-persist").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
    // A non-socket at the socket path is not treated as a server.
    let fake = d.dir.join("fake.sock");
    std::fs::File::create(&fake).unwrap().write_all(b"x").unwrap();
    assert!(matches!(Client::connect(&Target::Unix(fake), "app", "t"), Err(ConnectError::Rejected(_))));
}

#[test]
fn server_stop_ends_programs_but_keeps_the_layout() {
    let dir = Daemon::dir();
    let mut d = Daemon::start(dir.clone(), None, &[]);
    let (c, _rx) = d.client();
    let v = call(&c, "session.create", json!({"name": "kept"}));
    let b = v["block"].as_str().unwrap().to_string();
    let pid = call(&c, "block.info", json!({"block": b}))["pid"].as_i64().unwrap() as i32;
    assert!(c.call("server.stop", json!({})).is_err(), "refuses without force while terminals run");
    call(&c, "server.stop", json!({"force": true}));
    let status = d.child.take().unwrap().wait().unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "program {pid} outlived the server");
        std::thread::sleep(Duration::from_millis(50));
    }
    let d2 = Daemon::start(dir.clone(), None, &[]);
    let (c, _rx) = d2.client();
    let view = call(&c, "session.view", json!({"session": "kept"}));
    assert_eq!(view["id"], v["session"]);
    assert_eq!(view["blocks"].as_object().unwrap().len(), 1);
    drop(d2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crashing_terminal_leaves_the_others_running() {
    let d = Daemon::new(None);
    let (c, rx) = d.client();
    call(&c, "events.subscribe", json!({}));
    let bad = new_session(&c, sh("sleep 0.3; echo HEARTH-TEST-PANIC-$((6+1))f3a; sleep 600"));
    let good = new_session(&c, json!(["sh"]));
    let good_pid = call(&c, "block.info", json!({"block": good}))["pid"].clone();
    // The bad block's reader panics while holding its lock; only it closes.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut failed = false;
    while !failed {
        assert!(Instant::now() < deadline, "no terminal.failed event");
        if let Ok(Incoming::Event(v)) = rx.recv_timeout(Duration::from_millis(200)) {
            failed = v["event"] == "terminal.failed" && v["data"]["block"] == bad.as_str();
        }
    }
    let bad_pid = call(&c, "block.info", json!({"block": bad}))["pid"].as_i64().unwrap() as i32;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = call(&c, "block.info", json!({"block": bad}));
        if info["running"] == false && info["location"].is_null() && unsafe { libc::kill(bad_pid, 0) } != 0 {
            break;
        }
        assert!(Instant::now() < deadline, "failed block was not closed: {info}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Everything else still works, including locks the panic touched.
    assert_eq!(call(&c, "block.info", json!({"block": good}))["pid"], good_pid);
    call(&c, "terminal.send_text", json!({"block": good, "text": "echo alive-$((40+2))\r"}));
    wait_text(&c, &good, "alive-42");
    let (c2, _rx2) = d.client();
    assert_eq!(call(&c2, "session.list", json!({})).as_array().unwrap().len(), 1);
}

#[test]
fn closed_terminals_release_their_threads_and_files() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let pid = d.child.as_ref().unwrap().id();
    let count = |what: &str| std::fs::read_dir(format!("/proc/{pid}/{what}")).unwrap().count();
    // A first terminal settles one-time threads (Lua, accept, ...).
    let b = new_session(&c, sh("sleep 3600"));
    call(&c, "block.close", json!({"block": b}));
    std::thread::sleep(Duration::from_millis(500));
    let (threads, fds) = (count("task"), count("fd"));
    for _ in 0..10 {
        let b = new_session(&c, sh("echo hi; sleep 3600"));
        wait_text(&c, &b, "hi");
        call(&c, "block.close", json!({"block": b}));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while (count("task"), count("fd")) != (threads, fds) {
        assert!(Instant::now() < deadline, "threads {} (was {threads}), fds {} (was {fds})", count("task"), count("fd"));
        std::thread::sleep(Duration::from_millis(100));
    }
    // A closed terminal still reports how it ended.
    assert_eq!(call(&c, "block.info", json!({"block": b}))["running"], false);
}

#[test]
fn closes_are_logged_with_what_and_who() {
    let d = Daemon::new(None);
    let (c, _rx) = d.client();
    let me = c.welcome["client_id"].as_str().unwrap().to_string();
    let v = call(&c, "session.create", json!({"name": "logged", "command": ["sleep", "600"]}));
    let b = v["block"].as_str().unwrap().to_string();
    let w2 = call(&c, "window.create", json!({"session": "logged", "name": "second", "command": ["sleep", "601"]}));
    call(&c, "window.close", json!({"window": w2["window"]}));
    call(&c, "block.close", json!({"block": b}));
    let log = std::fs::read_to_string(d.dir.join("daemon.log")).unwrap();
    let tab = log.lines().find(|l| l.contains("closed tab")).unwrap_or_else(|| panic!("no tab close in log:\n{log}"));
    assert!(tab.contains("\"second\"") && tab.contains("sleep") && tab.contains(&format!("by {me} (app e2e, uid:")), "{tab}");
    let term = log.lines().find(|l| l.contains("closed terminal")).unwrap_or_else(|| panic!("no terminal close in log:\n{log}"));
    assert!(term.contains(&b) && term.contains("sleep") && term.contains(&me), "{term}");
}
