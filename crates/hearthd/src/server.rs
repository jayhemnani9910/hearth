//! Server core: connections, handshake, event bus, block lifecycle and
//! persistence. API methods live in api.rs.

use crate::sync::Lock;
use crate::block::{self, Block, ExitInfo, Notice, now_ms};
use crate::conn::ConnOut;
use crate::lua::{self, Engine};
use crate::model::Effect;
use crate::persist;
use crate::pty::{ShellMode, SpawnSpec};
use crate::state::Structure;
use hearth_core::conn::ApiError;
use hearth_core::proto::{self, Frame, code};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct ClientInfo {
    pub id: String,
    pub conn: Arc<ConnOut>,
    pub kind: String,
    pub name: String,
    pub principal: String,
    pub transport: String,
    pub remote_control: bool,
    pub connected_ms: u64,
    /// sub id -> block id
    pub subs: HashMap<u32, String>,
    pub next_sub: u32,
}

pub struct Core {
    pub s: Structure,
    pub blocks: HashMap<String, Arc<Block>>,
    pub close_on_exit: HashSet<String>,
    /// Recently closed blocks, kept for `wait`/`capture`/`info` after close.
    pub finished: VecDeque<Arc<Block>>,
    pub clients: HashMap<u64, ClientInfo>,
    pub event_seq: u64,
    pub subscribers: Vec<Arc<ConnOut>>,
    pub dirty: bool,
    /// new block id -> block id it replaced at restore
    pub restored_from: HashMap<String, String>,
}

pub struct Options {
    pub socket: PathBuf,
    pub auto_exit: bool,
    pub persist: AtomicBool,
    pub listen: Mutex<Vec<String>>,
}

pub struct Server {
    pub incarnation: String,
    pub started_ms: u64,
    pub socket: PathBuf,
    pub core: Mutex<Core>,
    pub notify: Sender<Notice>,
    pub lua: Engine,
    pub stop: AtomicBool,
    pub opts: Options,
    conn_counter: AtomicU64,
}

pub struct Ctx {
    pub conn: Option<Arc<ConnOut>>,
    pub conn_id: u64,
    pub client: Option<String>,
}

impl Ctx {
    pub fn internal() -> Ctx {
        Ctx { conn: None, conn_id: 0, client: None }
    }
}

pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl Server {
    pub fn new(opts: Options) -> (Arc<Server>, Receiver<Notice>) {
        let (tx, rx) = mpsc::channel();
        let server = Arc::new(Server {
            incarnation: random_hex(8),
            started_ms: now_ms(),
            socket: opts.socket.clone(),
            core: Mutex::new(Core {
                s: Structure::default(),
                blocks: HashMap::new(),
                close_on_exit: HashSet::new(),
                finished: VecDeque::new(),
                clients: HashMap::new(),
                event_seq: 0,
                subscribers: Vec::new(),
                dirty: false,
                restored_from: HashMap::new(),
            }),
            notify: tx,
            lua: Engine::start(),
            stop: AtomicBool::new(false),
            opts,
            conn_counter: AtomicU64::new(0),
        });
        lua::bind_server(&server);
        (server, rx)
    }

    pub fn emit(core: &mut Core, name: &str, data: Value) {
        core.event_seq += 1;
        let frame = Frame::Json(json!({"type": "event", "seq": core.event_seq, "event": name, "data": data}));
        core.subscribers.retain(|c| {
            c.send_ordered(frame.clone());
            !c.is_closed()
        });
    }

    pub fn emit_session(core: &mut Core, sid: &str) {
        core.dirty = true;
        match core.s.session(sid) {
            Ok(s) => {
                let view = session_view(core, s);
                Self::emit(core, "session.changed", json!({"session": view}));
            }
            Err(_) => Self::emit(core, "session.closed", json!({"session": sid})),
        }
    }

    pub fn find_block(core: &Core, id: &str) -> Result<Arc<Block>, ApiError> {
        if let Some(b) = core.blocks.get(id) {
            return Ok(b.clone());
        }
        if let Some(b) = core.finished.iter().find(|b| b.id == id) {
            return Ok(b.clone());
        }
        Err(ApiError::new(code::NOT_FOUND, format!("no block '{id}'")))
    }

    /// Creates a block (and its process). The caller places it in a layout.
    pub fn create_block(&self, core: &mut Core, sid: &str, p: &Value) -> Result<Arc<Block>, ApiError> {
        let id = core.s.alloc('b');
        let argv: Vec<String> = match &p["command"] {
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            Value::String(s) => vec![s.clone()],
            _ => vec![],
        };
        let shell = match p["shell"].as_str() {
            Some("none") | Some("direct") => ShellMode::None,
            Some("login") => ShellMode::Login,
            Some("interactive") => ShellMode::Interactive,
            None => {
                if argv.is_empty() {
                    ShellMode::Login
                } else {
                    ShellMode::None
                }
            }
            Some(o) => return Err(ApiError::new(code::INVALID, format!("shell must be none, login or interactive, got '{o}'"))),
        };
        let mut env = vec![
            ("HEARTH_SESSION".to_string(), sid.to_string()),
            ("HEARTH_BLOCK".to_string(), id.clone()),
            ("HEARTH_SERVER".to_string(), self.socket.display().to_string()),
        ];
        if let Some(obj) = p["env"].as_object() {
            for (k, v) in obj {
                if let Some(v) = v.as_str() {
                    env.push((k.clone(), v.to_string()));
                }
            }
        }
        let cwd = p["cwd"].as_str().map(PathBuf::from).or_else(|| self.lua.settings().default_cwd.map(PathBuf::from));
        let spec = SpawnSpec {
            argv,
            shell,
            cwd,
            env,
            cols: p["cols"].as_u64().unwrap_or(80).clamp(2, 1000) as u16,
            rows: p["rows"].as_u64().unwrap_or(24).clamp(1, 500) as u16,
        };
        let generation = format!("{}-{}", self.incarnation, id);
        let block = Block::spawn(id.clone(), generation, spec, self.lua.settings().history, self.notify.clone())
            .map_err(|e| ApiError::new(code::FAILED, format!("spawn failed: {e}")))?;
        let close = p["close_on_exit"].as_bool().unwrap_or(p["command"].is_null());
        if close {
            core.close_on_exit.insert(id.clone());
        }
        core.blocks.insert(id, block.clone());
        core.dirty = true;
        Ok(block)
    }

    /// Takes a block out of the layout and its process table entry.
    pub fn retire_block(core: &mut Core, id: &str) {
        let removed = core.s.remove_block(id);
        if let Some(b) = core.blocks.remove(id) {
            core.finished.push_back(b);
            while core.finished.len() > 256 {
                core.finished.pop_front();
            }
        }
        core.close_on_exit.remove(id);
        if let Some((sid, _, _, _)) = removed {
            Self::emit_session(core, &sid);
        }
    }

    pub fn handle_notice(self: &Arc<Self>, n: Notice, activity: &mut HashMap<String, Instant>) {
        match n {
            Notice::Output(id) => {
                let now = Instant::now();
                let due = activity.get(&id).is_none_or(|t| now.duration_since(*t) > Duration::from_secs(1));
                if due {
                    activity.insert(id.clone(), now);
                    let mut core = self.core.lk();
                    Self::emit(&mut core, "terminal.activity", json!({"block": id}));
                }
            }
            Notice::Effects(id, fx) => {
                let block = { self.core.lk().blocks.get(&id).cloned() };
                let mut core = self.core.lk();
                for e in fx {
                    match e {
                        Effect::Title(t) => Self::emit(&mut core, "terminal.title", json!({"block": id, "title": t})),
                        Effect::Cwd(c) => {
                            core.dirty = true;
                            Self::emit(&mut core, "terminal.cwd", json!({"block": id, "cwd": c}))
                        }
                        Effect::StatusChanged => {
                            if let Some(b) = &block {
                                let inner = b.inner.lk();
                                let data = json!({"block": id, "records": inner.model.status.records(), "summary": inner.model.status.summary()});
                                drop(inner);
                                Self::emit(&mut core, "terminal.status", data);
                            }
                        }
                        Effect::Prompt => Self::emit(&mut core, "terminal.prompt", json!({"block": id})),
                        Effect::Bell => Self::emit(&mut core, "terminal.bell", json!({"block": id})),
                        Effect::Clipboard(text) => Self::emit(&mut core, "terminal.clipboard", json!({"block": id, "text": text})),
                        Effect::Reply(_) => {}
                    }
                }
            }
            Notice::Exit(id, info) => {
                let mut core = self.core.lk();
                let info: &ExitInfo = &info;
                Self::emit(&mut core, "terminal.exit", json!({"block": id, "code": info.code, "signal": info.signal}));
                if core.close_on_exit.contains(&id) {
                    Self::retire_block(&mut core, &id);
                }
            }
            Notice::Eof(id) => {
                let mut core = self.core.lk();
                Self::emit(&mut core, "terminal.eof", json!({"block": id}));
            }
            Notice::Failed(id, why) => {
                eprintln!("hearthd: terminal {id} failed ({why}); closing it");
                let mut core = self.core.lk();
                Self::emit(&mut core, "terminal.failed", json!({"block": id, "reason": why}));
                Self::kill_blocks(&mut core, std::slice::from_ref(&id));
            }
        }
    }

    // ---- connections -------------------------------------------------

    pub fn serve_stream(self: &Arc<Self>, r: impl Read + Send + 'static, w: impl Write + Send + 'static, principal: String, transport: &str) {
        let conn_id = self.conn_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let (out, rx) = ConnOut::new(conn_id);
        let o2 = out.clone();
        let writer = std::thread::Builder::new().name(format!("conn-w-{conn_id}")).spawn(move || o2.run_writer(rx, w));
        if writer.is_err() {
            return;
        }
        let mut reader = BufReader::with_capacity(1 << 16, r);
        // Handshake: first frame must be a compatible hello.
        let hello = match proto::read_frame(&mut reader) {
            Ok(Some(Frame::Json(v))) if v["type"] == "hello" => v,
            _ => {
                out.send_ordered(Frame::Json(json!({"type": "reject", "code": "protocol", "reason": "expected hello"})));
                out.close("bad handshake");
                return;
            }
        };
        let min = hello["min_version"].as_u64().unwrap_or(1);
        let max = hello["max_version"].as_u64().unwrap_or(1);
        if hello["protocol"] != proto::PROTOCOL || !(min..=max).contains(&(proto::PROTOCOL_VERSION as u64)) {
            out.send_ordered(Frame::Json(json!({
                "type": "reject", "code": "version",
                "reason": format!("server speaks {} version {}", proto::PROTOCOL, proto::PROTOCOL_VERSION),
            })));
            out.close("version mismatch");
            return;
        }
        let client_id = format!("c{conn_id}");
        {
            let mut core = self.core.lk();
            core.clients.insert(conn_id, ClientInfo {
                id: client_id.clone(),
                conn: out.clone(),
                kind: hello["client"]["kind"].as_str().unwrap_or("unknown").into(),
                name: hello["client"]["name"].as_str().unwrap_or("").into(),
                principal: principal.clone(),
                transport: transport.into(),
                remote_control: false,
                connected_ms: now_ms(),
                subs: HashMap::new(),
                next_sub: 0,
            });
            Self::emit(&mut core, "client.connected", json!({"client": client_id, "kind": hello["client"]["kind"], "transport": transport}));
        }
        out.send_ordered(Frame::Json(json!({
            "type": "welcome",
            "protocol": proto::PROTOCOL,
            "version": proto::PROTOCOL_VERSION,
            "client_id": client_id,
            "principal": principal,
            "server": {
                "version": env!("CARGO_PKG_VERSION"),
                "incarnation": self.incarnation,
                "pid": std::process::id(),
                "host": hostname(),
                "user": username(),
            },
            "limits": {"max_frame": proto::MAX_FRAME, "queue_frames": crate::conn::QUEUE_FRAMES},
        })));
        let ctx = Arc::new(Ctx { conn: Some(out.clone()), conn_id, client: Some(client_id.clone()) });
        loop {
            if out.is_closed() {
                break;
            }
            match proto::read_frame(&mut reader) {
                Ok(Some(Frame::Json(v))) if v["type"] == "request" => self.on_request(&ctx, v),
                Ok(Some(Frame::Input { sub, seq, bytes })) => self.on_input(&ctx, sub, seq, bytes),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        self.drop_client(conn_id);
        out.close("disconnected");
    }

    fn on_request(self: &Arc<Self>, ctx: &Arc<Ctx>, v: Value) {
        let id = v["id"].clone();
        let method = v["method"].as_str().unwrap_or("").to_string();
        let params = if v["params"].is_null() { json!({}) } else { v["params"].clone() };
        let respond = {
            let conn = ctx.conn.clone().unwrap();
            move |r: Result<Value, ApiError>| {
                let msg = match r {
                    Ok(result) => json!({"type": "response", "id": id, "ok": true, "result": result}),
                    Err(e) => json!({"type": "response", "id": id, "ok": false, "error": {"code": e.code, "message": e.message}}),
                };
                conn.send_ordered(Frame::Json(msg));
            }
        };
        if crate::api::is_blocking(&method) {
            let s = self.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || respond(s.call(&ctx, &method, &params)));
        } else {
            respond(self.call(ctx, &method, &params));
        }
    }

    fn on_input(&self, ctx: &Ctx, sub: u32, seq: u64, bytes: Vec<u8>) {
        let (block, client) = {
            let core = self.core.lk();
            let Some(c) = core.clients.get(&ctx.conn_id) else { return };
            let Some(bid) = c.subs.get(&sub) else { return };
            (core.blocks.get(bid).cloned(), c.id.clone())
        };
        let conn = ctx.conn.as_ref().unwrap();
        let Some(block) = block else {
            conn.send_ordered(block::event_frame("input.ack", &json!({"sub": sub, "seq": seq, "delivered": false})));
            return;
        };
        // The client that types owns the terminal size.
        let owner_changed = { block.inner.lk().owner.as_deref() != Some(client.as_str()) };
        if owner_changed {
            block.set_owner_and_size(&client);
        }
        block.write_input(bytes);
        conn.send_ordered(block::event_frame("input.ack", &json!({"sub": sub, "seq": seq, "delivered": true})));
    }

    fn drop_client(&self, conn_id: u64) {
        let info = {
            let mut core = self.core.lk();
            core.subscribers.retain(|c| c.id != conn_id);
            core.clients.remove(&conn_id).map(|c| {
                let blocks: Vec<Arc<Block>> = c.subs.values().filter_map(|b| core.blocks.get(b).cloned()).collect();
                (c.id, blocks)
            })
        };
        if let Some((cid, blocks)) = info {
            for b in blocks {
                b.detach(conn_id, None);
                b.release_owner(&cid);
            }
            let mut core = self.core.lk();
            Self::emit(&mut core, "client.disconnected", json!({"client": cid}));
        }
    }

    pub fn persist_now(&self) {
        if !self.opts.persist.load(Ordering::Relaxed) {
            return;
        }
        let snapshot = {
            let mut core = self.core.lk();
            if !core.dirty {
                return;
            }
            core.dirty = false;
            persist::capture(&core)
        };
        if let Err(e) = persist::write(&snapshot) {
            eprintln!("hearthd: cannot save state: {e}");
        }
    }

    pub fn idle(&self) -> bool {
        let core = self.core.lk();
        core.blocks.is_empty() && core.clients.is_empty()
    }
}

pub fn session_view(core: &Core, s: &crate::state::Session) -> Value {
    let mut ids = vec![];
    for w in &s.windows {
        w.layout.leaves(&mut ids);
    }
    let blocks: serde_json::Map<String, Value> = ids
        .iter()
        .filter_map(|id| core.blocks.get(id).map(|b| {
            let mut info = b.info();
            if let Some(old) = core.restored_from.get(id) {
                info["restored_from"] = json!(old);
            }
            (id.clone(), info)
        }))
        .collect();
    json!({
        "id": s.id,
        "name": s.name,
        "revision": s.revision,
        "active_window": s.active_window,
        "created_ms": s.created_ms,
        "windows": s.windows,
        "blocks": blocks,
    })
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn username() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() {
            return std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        }
    }
    String::new()
}
