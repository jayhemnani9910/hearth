//! `hearth attach` (view a terminal inside the current terminal) and
//! `hearth proxy` (stdio bridge used by the ssh transport).

use crate::die;
use hearth_core::conn::{Client, Incoming};
use hearth_core::scan::QueryFilter;
use serde_json::json;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

const DETACH: u8 = 0x1d; // Ctrl-]

fn winsize() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}

struct Raw(Option<libc::termios>);

impl Raw {
    fn enter() -> Raw {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Raw(None);
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Raw(Some(orig))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        }
    }
}

pub fn attach(c: &Client, rx: Receiver<Incoming>, block: &str) -> i32 {
    let (cols, rows) = winsize().unwrap_or((80, 24));
    let attach = |c: &Client| c.call("terminal.attach", json!({"block": block, "cols": cols, "rows": rows}));
    let info = attach(c).unwrap_or_else(|e| die(crate::exit_code(&e), e));
    let sub = Arc::new(std::sync::atomic::AtomicU32::new(info["sub"].as_u64().unwrap_or(0) as u32));
    // Typing into this terminal claims size ownership.
    let _ = c.call("block.own", json!({"block": block, "mode": "take"}));
    let raw = Raw::enter();
    let done = Arc::new(AtomicBool::new(false));
    let code = Arc::new(std::sync::atomic::AtomicI32::new(0));

    std::thread::scope(|scope| {
        let d = done.clone();
        let s2 = sub.clone();
        let code2 = code.clone();
        let block_id = block.to_string();
        scope.spawn(move || {
            let mut out = std::io::stdout().lock();
            let mut filter = QueryFilter::new();
            while !d.load(Ordering::Relaxed) {
                let Ok(m) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
                match m {
                    Incoming::Snapshot { sub, bytes, .. } if sub == s2.load(Ordering::Relaxed) => {
                        filter = QueryFilter::new();
                        let _ = out.write_all(b"\x1bc");
                        let _ = out.write_all(&filter.push(&bytes));
                    }
                    Incoming::Data { sub, bytes, .. } if sub == s2.load(Ordering::Relaxed) => {
                        let _ = out.write_all(&filter.push(&bytes));
                    }
                    Incoming::Event(v) => match v["event"].as_str() {
                        Some("terminal.exit") if v["data"]["block"] == block_id.as_str() => {
                            code2.store(v["data"]["code"].as_i64().unwrap_or(1) as i32, Ordering::Relaxed);
                        }
                        Some("terminal.eof") if v["data"]["block"] == block_id.as_str() => {
                            d.store(true, Ordering::Relaxed);
                        }
                        Some("terminal.lost") => {
                            // Re-attach for a fresh snapshot; nothing was replayed twice.
                            let _ = out.write_all(b"\r\n[hearth: viewer fell behind; resynchronizing]\r\n");
                            if let Ok(i) = attach(c) {
                                s2.store(i["sub"].as_u64().unwrap_or(0) as u32, Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    },
                    Incoming::Closed(r) => {
                        let _ = write!(out, "\r\n[hearth: {r}]\r\n");
                        d.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
                let _ = out.flush();
            }
        });
        let d = done.clone();
        let s3 = sub.clone();
        scope.spawn(move || {
            let mut last = winsize();
            while !d.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let now = winsize();
                if now != last {
                    if let Some((cols, rows)) = now {
                        let _ = c.call("terminal.resize", json!({"sub": s3.load(Ordering::Relaxed), "cols": cols, "rows": rows}));
                    }
                    last = now;
                }
            }
        });
        // Input: stdin -> input frames (poll so we notice exit).
        let mut seq = 0u64;
        let mut buf = [0u8; 4096];
        while !done.load(Ordering::Relaxed) {
            let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 100) } <= 0 {
                continue;
            }
            let n = match std::io::stdin().lock().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let chunk = &buf[..n];
            let (send, detach) = match chunk.iter().position(|b| *b == DETACH) {
                Some(i) => (&chunk[..i], true),
                None => (chunk, false),
            };
            if !send.is_empty() {
                seq += 1;
                let _ = c.send_input(sub.load(Ordering::Relaxed), seq, send);
            }
            if detach {
                break;
            }
        }
        done.store(true, Ordering::Relaxed);
    });
    drop(raw);
    let _ = c.call("terminal.detach", json!({"sub": sub.load(Ordering::Relaxed)}));
    // Leave our own terminal sane: main screen, cursor visible, modes off.
    print!("\x1b[?1049l\x1b[?25h\x1b[?1l\x1b>\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[0m\r\n[detached from {block}]\r\n");
    let _ = std::io::stdout().flush();
    code.load(Ordering::Relaxed)
}

pub fn proxy() -> ! {
    if let Err(e) = hearth_core::conn::ensure_local_server() {
        die(7, e);
    }
    let sock = hearth_core::paths::socket_path();
    let s = std::os::unix::net::UnixStream::connect(&sock).unwrap_or_else(|e| die(7, format!("{}: {e}", sock.display())));
    let mut r = s.try_clone().unwrap();
    let mut w = s;
    // Plain read/write loops on the raw descriptors: no buffering, so each
    // frame is forwarded as soon as it arrives.
    use std::os::fd::FromRawFd;
    let t = std::thread::spawn(move || {
        let mut out = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(1) });
        let mut buf = vec![0u8; 1 << 16];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 || out.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });
    let mut inp = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
    let mut buf = vec![0u8; 1 << 16];
    while let Ok(n) = inp.read(&mut buf) {
        if n == 0 || w.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = w.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
    std::process::exit(0)
}
