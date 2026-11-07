//! A terminal block: PTY, child process, canonical model, output ring and
//! the viewers subscribed to it.

use crate::sync::Lock;
use crate::conn::ConnOut;
use crate::model::{Effect, Model};
use crate::pty::{self, SpawnSpec};
use hearth_core::proto::Frame;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Retained output for resume after a short disconnect.
pub const RING_CAP: usize = 4 << 20;
/// Bytes per Data frame sent to viewers.
const CHUNK: usize = 64 << 10;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub at_ms: u64,
}

pub struct Sub {
    pub conn: Arc<ConnOut>,
    pub sub: u32,
    pub client: String,
    /// Viewer's preferred geometry (cols, rows, cell px w, h).
    pub pref: Option<(u16, u16, u16, u16)>,
}

pub struct Inner {
    pub model: Model,
    ring: VecDeque<u8>,
    ring_start: u64,
    pub total: u64,
    pub subs: Vec<Sub>,
    pub owner: Option<String>,
    pub exit: Option<ExitInfo>,
    pub eof: bool,
    pub last_output: Option<Instant>,
    pub killed: bool,
}

pub struct Block {
    pub id: String,
    /// Changes with every daemon incarnation and process; resume offsets
    /// from another generation are rejected.
    pub generation: String,
    pub pid: i32,
    pub start_time: Option<u64>,
    pub spec: SpawnSpec,
    pub created_ms: u64,
    /// Closed (None) once the block is retired.
    master: Mutex<Option<OwnedFd>>,
    input: Mutex<Option<Sender<Vec<u8>>>>,
    pub inner: Mutex<Inner>,
    pub exited: Condvar,
}

/// Notifications from block threads to the server (outside block locks).
pub enum Notice {
    Output(String),
    Effects(String, Vec<Effect>),
    Exit(String, ExitInfo),
    Eof(String),
    /// The reader thread panicked; the block's state can't be trusted.
    Failed(String, String),
}

/// Debug builds only: panics on output containing `$HEARTH_TEST_PANIC_ON`,
/// so tests can check that one failing terminal leaves the rest running.
fn test_panic(bytes: &[u8]) {
    if cfg!(debug_assertions) {
        static MARK: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        if let Some(m) = MARK.get_or_init(|| std::env::var("HEARTH_TEST_PANIC_ON").ok().filter(|m| !m.is_empty())) {
            if bytes.windows(m.len()).any(|w| w == m.as_bytes()) {
                panic!("test panic on output marker");
            }
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Block {
    pub fn spawn(id: String, generation: String, spec: SpawnSpec, history: usize, notify: Sender<Notice>) -> std::io::Result<Arc<Block>> {
        let sp = pty::spawn(&spec)?;
        let reader = File::from(sp.master.try_clone()?);
        let mut writer = File::from(sp.master.try_clone()?);
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let block = Arc::new(Block {
            id: id.clone(),
            generation,
            pid: sp.pid,
            start_time: pty::proc_start_time(sp.pid),
            created_ms: now_ms(),
            master: Mutex::new(Some(sp.master)),
            input: Mutex::new(Some(tx)),
            inner: Mutex::new(Inner {
                model: Model::new(spec.cols, spec.rows, history),
                ring: VecDeque::new(),
                ring_start: 0,
                total: 0,
                subs: Vec::new(),
                owner: None,
                exit: None,
                eof: false,
                last_output: None,
                killed: false,
            }),
            exited: Condvar::new(),
            spec,
        });
        // Writer: the only place that writes to the PTY, so client input and
        // model replies keep one order and never block the reader.
        std::thread::Builder::new().name(format!("pty-w-{id}")).spawn(move || {
            for buf in rx {
                if writer.write_all(&buf).is_err() {
                    break;
                }
            }
        })?;
        let b = block.clone();
        let n = notify.clone();
        std::thread::Builder::new().name(format!("pty-r-{id}")).spawn(move || {
            // A bug triggered by one program's output must not end the others.
            let id = b.id.clone();
            let n2 = n.clone();
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.read_loop(reader, n))) {
                let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "unknown panic".into());
                let _ = n2.send(Notice::Failed(id, why));
            }
        })?;
        let b = block.clone();
        std::thread::Builder::new().name(format!("wait-{id}")).spawn(move || {
            let (code, signal) = pty::wait(b.pid);
            let info = ExitInfo { code, signal, at_ms: now_ms() };
            let changed = {
                let mut inner = b.inner.lk();
                inner.exit = Some(info.clone());
                inner.model.on_child_exit()
            };
            b.exited.notify_all();
            if changed {
                let _ = notify.send(Notice::Effects(b.id.clone(), vec![Effect::StatusChanged]));
            }
            let _ = notify.send(Notice::Exit(b.id.clone(), info));
        })?;
        Ok(block)
    }

    fn read_loop(self: Arc<Self>, mut reader: File, notify: Sender<Notice>) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // EIO: every slave descriptor is closed.
                Err(_) => break,
            };
            let effects = self.ingest(&buf[..n]);
            let _ = notify.send(Notice::Output(self.id.clone()));
            if !effects.is_empty() {
                let mut replies = Vec::new();
                let mut rest = Vec::new();
                for e in effects {
                    match e {
                        Effect::Reply(r) => replies.extend(r),
                        other => rest.push(other),
                    }
                }
                if !replies.is_empty() {
                    self.write_input(replies);
                }
                if !rest.is_empty() {
                    let _ = notify.send(Notice::Effects(self.id.clone(), rest));
                }
            }
        }
        {
            let mut inner = self.inner.lk();
            inner.model.finish();
            inner.eof = true;
        }
        let _ = notify.send(Notice::Eof(self.id.clone()));
    }

    /// Appends output to the stream: ring, model, then every viewer, all
    /// under one lock so attach can never interleave with it.
    fn ingest(&self, bytes: &[u8]) -> Vec<Effect> {
        let mut inner = self.inner.lk();
        test_panic(bytes);
        let offset = inner.total;
        inner.total += bytes.len() as u64;
        inner.ring.extend(bytes);
        while inner.ring.len() > RING_CAP {
            let drop = inner.ring.len() - RING_CAP;
            inner.ring.drain(..drop);
            inner.ring_start += drop as u64;
        }
        inner.model.advance(bytes);
        inner.last_output = Some(Instant::now());
        let frame = Frame::Data { sub: 0, offset, bytes: bytes.to_vec() };
        inner.subs.retain(|s| s.conn.send_data(s.sub, &frame));
        inner.model.take_effects()
    }

    pub fn write_input(&self, bytes: Vec<u8>) {
        if let Some(tx) = &*self.input.lk() {
            let _ = tx.send(bytes);
        }
    }

    /// Runs `f` with the PTY master, unless the block was retired.
    pub fn with_master<R>(&self, f: impl FnOnce(i32) -> R) -> Option<R> {
        self.master.lk().as_ref().map(|m| f(m.as_raw_fd()))
    }

    pub fn foreground_pgrp(&self) -> Option<i32> {
        self.with_master(pty::foreground_pgrp).flatten()
    }

    /// Ring bytes in `[from, total)`, if still retained.
    fn ring_from(inner: &Inner, from: u64) -> Option<Vec<u8>> {
        if from < inner.ring_start || from > inner.total {
            return None;
        }
        let skip = (from - inner.ring_start) as usize;
        Some(inner.ring.iter().skip(skip).copied().collect())
    }

    /// Subscribes a viewer. Sends either the retained bytes after `resume`
    /// or a snapshot plus the bytes after its offset. Returns attach info.
    pub fn attach(&self, conn: &Arc<ConnOut>, sub: u32, client: &str, resume: Option<(&str, u64)>, pref: Option<(u16, u16, u16, u16)>) -> Value {
        let mut inner = self.inner.lk();
        // A size given here counts as reporting one: an owner's (or the first
        // viewer's) size applies before the snapshot, so it is taken at the
        // size this viewer will draw.
        if let Some((c, r, w, h)) = pref {
            if inner.owner.is_none() {
                inner.owner = Some(client.to_string());
            }
            if inner.owner.as_deref() == Some(client) {
                self.apply_size(&mut inner, c, r, (w, h));
            }
        }
        let resumed = match resume {
            Some((generation, offset)) if generation == self.generation => Self::ring_from(&inner, offset).map(|b| (offset, b)),
            _ => None,
        };
        let (cols, rows) = (inner.model.cols(), inner.model.rows());
        let mut info = json!({
            "sub": sub, "block": self.id, "generation": self.generation,
            "cols": cols, "rows": rows, "total": inner.total, "owner": inner.owner,
        });
        match resumed {
            Some((offset, bytes)) => {
                info["mode"] = json!("resume");
                info["offset"] = json!(offset);
                conn.send_ordered(event_frame("terminal.attached", &info));
                for (i, c) in bytes.chunks(CHUNK).enumerate() {
                    conn.send_ordered(Frame::Data { sub, offset: offset + (i * CHUNK) as u64, bytes: c.to_vec() });
                }
            }
            None => {
                let at = inner.model.applied();
                info["mode"] = json!("snapshot");
                info["offset"] = json!(at);
                conn.send_ordered(event_frame("terminal.attached", &info));
                conn.send_ordered(Frame::Snapshot { sub, offset: at, bytes: inner.model.snapshot() });
                if let Some(tail) = Self::ring_from(&inner, at) {
                    if !tail.is_empty() {
                        conn.send_ordered(Frame::Data { sub, offset: at, bytes: tail });
                    }
                }
            }
        }
        inner.subs.push(Sub { conn: conn.clone(), sub, client: client.to_string(), pref });
        info
    }

    pub fn detach(&self, conn_id: u64, sub: Option<u32>) -> bool {
        let mut inner = self.inner.lk();
        let before = inner.subs.len();
        inner.subs.retain(|s| !(s.conn.id == conn_id && sub.is_none_or(|x| x == s.sub)));
        before != inner.subs.len()
    }

    /// Applies a geometry to the PTY and the model, then tells every viewer
    /// in stream order.
    pub fn apply_size(&self, inner: &mut Inner, cols: u16, rows: u16, px: (u16, u16)) -> bool {
        let cols = cols.clamp(2, 1000);
        let rows = rows.clamp(1, 500);
        if (inner.model.cols(), inner.model.rows()) == (cols, rows) {
            return false;
        }
        self.with_master(|fd| pty::set_size(fd, cols, rows, cols.saturating_mul(px.0), rows.saturating_mul(px.1)));
        inner.model.resize(cols, rows);
        inner.model.set_cell_size(px.0, px.1);
        let owner = inner.owner.clone();
        let total = inner.total;
        for s in &inner.subs {
            s.conn.send_ordered(event_frame(
                "terminal.resized",
                &json!({"sub": s.sub, "block": self.id, "cols": cols, "rows": rows, "owner": owner, "offset": total}),
            ));
        }
        true
    }

    /// Size ownership: the owner's preference wins. Returns true on change.
    pub fn set_owner_and_size(&self, client: &str) -> bool {
        let mut inner = self.inner.lk();
        inner.owner = Some(client.to_string());
        let pref = inner.subs.iter().find(|s| s.client == client).and_then(|s| s.pref);
        match pref {
            Some((c, r, w, h)) => self.apply_size(&mut inner, c, r, (w, h)),
            None => false,
        }
    }

    pub fn set_pref(&self, conn_id: u64, sub: u32, pref: (u16, u16, u16, u16)) -> (bool, Option<String>) {
        let mut inner = self.inner.lk();
        let mut client = None;
        for s in inner.subs.iter_mut() {
            if s.conn.id == conn_id && s.sub == sub {
                s.pref = Some(pref);
                client = Some(s.client.clone());
            }
        }
        let Some(client) = client else { return (false, inner.owner.clone()) };
        if inner.owner.is_none() {
            inner.owner = Some(client.clone());
        }
        let applied = inner.owner.as_deref() == Some(client.as_str()) && self.apply_size(&mut inner, pref.0, pref.1, (pref.2, pref.3));
        (applied, inner.owner.clone())
    }

    /// Releases ownership held by a departing client; the next viewer with
    /// a preference becomes owner.
    pub fn release_owner(&self, client: &str) {
        let mut inner = self.inner.lk();
        if inner.owner.as_deref() == Some(client) {
            let next = inner.subs.iter().find(|s| s.client != client && s.pref.is_some()).map(|s| (s.client.clone(), s.pref.unwrap()));
            inner.owner = next.as_ref().map(|n| n.0.clone());
            if let Some((_, (c, r, w, h))) = next {
                self.apply_size(&mut inner, c, r, (w, h));
            }
        }
    }

    pub fn resize_direct(&self, cols: u16, rows: u16) -> bool {
        let mut inner = self.inner.lk();
        self.apply_size(&mut inner, cols, rows, (8, 16))
    }

    /// Terminates the process group: SIGHUP, then SIGKILL after a grace.
    /// Releases a retired terminal's PTY, writer thread, output ring and
    /// scrollback; its exit status and last screen stay available.
    pub fn release(&self) {
        // Dropping the sender ends the writer thread, which closes its fd.
        self.input.lk().take();
        self.master.lk().take();
        let mut inner = self.inner.lk();
        inner.ring_start += inner.ring.len() as u64;
        inner.ring = VecDeque::new();
        inner.model.drop_history();
    }

    pub fn kill(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lk();
            if inner.exit.is_some() {
                return;
            }
            inner.killed = true;
        }
        unsafe {
            libc::kill(-self.pid, libc::SIGHUP);
            libc::kill(self.pid, libc::SIGHUP);
        }
        let b = self.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut inner = b.inner.lk();
            while inner.exit.is_none() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    unsafe {
                        libc::kill(-b.pid, libc::SIGKILL);
                        libc::kill(b.pid, libc::SIGKILL);
                    }
                    break;
                }
                inner = b.exited.wait_timeout(inner, left).unwrap().0;
            }
        });
    }

    /// Wait for the child to exit (or timeout).
    pub fn wait_exit(&self, timeout: Option<Duration>) -> Option<ExitInfo> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut inner = self.inner.lk();
        loop {
            if let Some(e) = &inner.exit {
                return Some(e.clone());
            }
            match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    inner = self.exited.wait_timeout(inner, left).unwrap().0;
                }
                None => inner = self.exited.wait(inner).unwrap(),
            }
        }
    }

    /// Metadata reported for this block. Sources are named because they can
    /// be absent or stale.
    pub fn info(&self) -> Value {
        let inner = self.inner.lk();
        let fg = self.foreground_pgrp();
        let fg_name = fg.and_then(pty::proc_name);
        let proc_cwd = if inner.exit.is_none() { pty::proc_cwd(fg.unwrap_or(self.pid)) } else { None };
        json!({
            "id": self.id,
            "generation": self.generation,
            "created_ms": self.created_ms,
            "pid": self.pid,
            "start_time": self.start_time,
            "argv": self.spec.argv,
            "shell": self.spec.shell,
            "cols": inner.model.cols(),
            "rows": inner.model.rows(),
            "title": inner.model.title,
            "cwd": {"reported": inner.model.cwd, "process": proc_cwd},
            "process": {"foreground_pgrp": fg, "name": fg_name},
            "status": inner.model.status.records(),
            "status_summary": inner.model.status.summary(),
            "owner": inner.owner,
            "viewers": inner.subs.len(),
            "exit": inner.exit,
            "killed": inner.killed,
            "eof": inner.eof,
            "output_bytes": inner.total,
            "idle_ms": inner.last_output.map(|t| t.elapsed().as_millis() as u64),
        })
    }
}

pub fn event_frame(name: &str, data: &Value) -> Frame {
    Frame::Json(json!({"type": "event", "event": name, "data": data}))
}
