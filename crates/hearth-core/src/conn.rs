//! Blocking client connection used by the CLI and the desktop app.

use crate::paths;
use crate::proto::{self, Frame};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Unix(PathBuf),
    Tcp(String),
    /// Runs `ssh -T <dest> hearth proxy` and speaks the protocol over stdio.
    Ssh(String),
    /// Arbitrary command whose stdio is a hearth protocol stream.
    Command(Vec<String>),
}

impl Target {
    /// Parses `unix:/path`, `tcp:host:port`, `ssh:user@host`, or `local`.
    pub fn parse(s: &str) -> Result<Target, String> {
        if s == "local" || s.is_empty() {
            return Ok(Target::Unix(paths::socket_path()));
        }
        if let Some(p) = s.strip_prefix("unix:") {
            return Ok(Target::Unix(PathBuf::from(p)));
        }
        if let Some(a) = s.strip_prefix("tcp:") {
            return Ok(Target::Tcp(a.to_string()));
        }
        if let Some(d) = s.strip_prefix("ssh:") {
            return Ok(Target::Ssh(d.to_string()));
        }
        if let Some(c) = s.strip_prefix("cmd:") {
            return Ok(Target::Command(c.split_whitespace().map(String::from).collect()));
        }
        Err(format!("unknown host address '{s}' (use local, unix:PATH, tcp:HOST:PORT or ssh:DEST)"))
    }

    pub fn describe(&self) -> String {
        match self {
            Target::Unix(p) => format!("unix:{}", p.display()),
            Target::Tcp(a) => format!("tcp:{a}"),
            Target::Ssh(d) => format!("ssh:{d}"),
            Target::Command(c) => format!("cmd:{}", c.join(" ")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        ApiError { code: code.to_string(), message: message.into() }
    }
}

/// Distinct connection failure classes shown to the user.
#[derive(Debug, Clone)]
pub enum ConnectError {
    NoServer(String),
    Unreachable(String),
    Rejected(String),
    Incompatible(String),
    Io(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoServer(m) => write!(f, "no server running: {m}"),
            ConnectError::Unreachable(m) => write!(f, "host unreachable: {m}"),
            ConnectError::Rejected(m) => write!(f, "connection rejected: {m}"),
            ConnectError::Incompatible(m) => write!(f, "incompatible server: {m}"),
            ConnectError::Io(m) => write!(f, "connection failed: {m}"),
        }
    }
}

impl std::error::Error for ConnectError {}

#[derive(Debug)]
pub enum Incoming {
    Event(Value),
    Data { sub: u32, offset: u64, bytes: Vec<u8> },
    Snapshot { sub: u32, offset: u64, bytes: Vec<u8> },
    Closed(String),
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, ApiError>>>>>;

pub struct Client {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: Pending,
    next_id: AtomicU64,
    pub welcome: Value,
    pub target: Target,
    child: Mutex<Option<Child>>,
    shutdown: Box<dyn Fn() + Send + Sync>,
}

impl Drop for Client {
    fn drop(&mut self) {
        (self.shutdown)();
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn open(target: &Target) -> Result<(Box<dyn Read + Send>, Box<dyn Write + Send>, Option<Child>, Box<dyn Fn() + Send + Sync>), ConnectError> {
    match target {
        Target::Unix(path) => {
            if let Err(e) = paths::check_socket_owner(path) {
                return Err(if e.kind() == std::io::ErrorKind::NotFound {
                    ConnectError::NoServer(path.display().to_string())
                } else {
                    ConnectError::Rejected(e.to_string())
                });
            }
            let s = UnixStream::connect(path).map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                    ConnectError::NoServer(path.display().to_string())
                }
                _ => ConnectError::Io(e.to_string()),
            })?;
            let r = s.try_clone().map_err(|e| ConnectError::Io(e.to_string()))?;
            let sd = s.try_clone().map_err(|e| ConnectError::Io(e.to_string()))?;
            Ok((Box::new(r), Box::new(s), None, Box::new(move || {
                let _ = sd.shutdown(std::net::Shutdown::Both);
            })))
        }
        Target::Tcp(addr) => {
            use std::net::ToSocketAddrs;
            let addrs: Vec<_> = addr
                .to_socket_addrs()
                .map_err(|e| ConnectError::Unreachable(format!("{addr}: {e}")))?
                .collect();
            let mut last = None;
            for a in addrs {
                match std::net::TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
                    Ok(s) => {
                        let _ = s.set_nodelay(true);
                        let r = s.try_clone().map_err(|e| ConnectError::Io(e.to_string()))?;
                        let sd = s.try_clone().map_err(|e| ConnectError::Io(e.to_string()))?;
                        return Ok((Box::new(r), Box::new(s), None, Box::new(move || {
                            let _ = sd.shutdown(std::net::Shutdown::Both);
                        })));
                    }
                    Err(e) => last = Some(e),
                }
            }
            Err(ConnectError::Unreachable(format!("{addr}: {}", last.map(|e| e.to_string()).unwrap_or("no address".into()))))
        }
        Target::Ssh(dest) => open(&Target::Command(vec![
            "ssh".into(),
            "-T".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            dest.clone(),
            "hearth".into(),
            "proxy".into(),
        ])),
        Target::Command(argv) => {
            let (prog, args) = argv.split_first().ok_or(ConnectError::Io("empty command".into()))?;
            let mut child = Command::new(prog)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| ConnectError::Unreachable(format!("{prog}: {e}")))?;
            let r = child.stdout.take().unwrap();
            let w = child.stdin.take().unwrap();
            Ok((Box::new(r), Box::new(w), Some(child), Box::new(|| {})))
        }
    }
}

impl Client {
    /// Connects and negotiates. Returns the client and its incoming stream
    /// of events and terminal data.
    pub fn connect(target: &Target, kind: &str, name: &str) -> Result<(Client, Receiver<Incoming>), ConnectError> {
        let (r, w, child, shutdown) = open(target)?;
        let mut reader = BufReader::with_capacity(1 << 16, r);
        let writer: Box<dyn Write + Send> = Box::new(BufWriter::new(w));
        let writer = Arc::new(Mutex::new(writer));
        let hello = json!({
            "type": "hello",
            "protocol": proto::PROTOCOL,
            "min_version": proto::PROTOCOL_VERSION,
            "max_version": proto::PROTOCOL_VERSION,
            "client": {"kind": kind, "name": name, "version": env!("CARGO_PKG_VERSION")},
        });
        proto::write_frame(&mut *writer.lock().unwrap(), &Frame::Json(hello)).map_err(|e| ConnectError::Io(e.to_string()))?;
        let welcome = match proto::read_frame(&mut reader) {
            Ok(Some(Frame::Json(v))) => v,
            Ok(Some(_)) => return Err(ConnectError::Incompatible("unexpected first frame".into())),
            Ok(None) => return Err(ConnectError::Rejected("server closed the connection during handshake".into())),
            Err(e) => return Err(ConnectError::Incompatible(e.to_string())),
        };
        match welcome["type"].as_str() {
            Some("welcome") => {}
            Some("reject") => {
                let reason = welcome["reason"].as_str().unwrap_or("rejected").to_string();
                return Err(if welcome["code"] == "version" { ConnectError::Incompatible(reason) } else { ConnectError::Rejected(reason) });
            }
            _ => return Err(ConnectError::Incompatible("not a hearth server".into())),
        }
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        let p2 = pending.clone();
        std::thread::Builder::new()
            .name("hearth-conn-reader".into())
            .spawn(move || {
                let reason = loop {
                    match proto::read_frame(&mut reader) {
                        Ok(Some(Frame::Json(v))) => match v["type"].as_str() {
                            Some("response") => {
                                let id = v["id"].as_u64().unwrap_or(0);
                                if let Some(s) = p2.lock().unwrap().remove(&id) {
                                    let res = if v["ok"].as_bool() == Some(true) {
                                        Ok(v["result"].clone())
                                    } else {
                                        Err(ApiError {
                                            code: v["error"]["code"].as_str().unwrap_or("failed").into(),
                                            message: v["error"]["message"].as_str().unwrap_or("").into(),
                                        })
                                    };
                                    let _ = s.send(res);
                                }
                            }
                            Some("event") => {
                                if tx.send(Incoming::Event(v)).is_err() {
                                    break "receiver dropped".to_string();
                                }
                            }
                            Some("close") => break v["reason"].as_str().unwrap_or("closed by server").to_string(),
                            _ => {}
                        },
                        Ok(Some(Frame::Data { sub, offset, bytes })) => {
                            let _ = tx.send(Incoming::Data { sub, offset, bytes });
                        }
                        Ok(Some(Frame::Snapshot { sub, offset, bytes })) => {
                            let _ = tx.send(Incoming::Snapshot { sub, offset, bytes });
                        }
                        Ok(Some(Frame::Input { .. })) => {}
                        Ok(None) => break "server closed the connection".to_string(),
                        Err(e) => break format!("stream error: {e}"),
                    }
                };
                for (_, s) in p2.lock().unwrap().drain() {
                    let _ = s.send(Err(ApiError::new("disconnected", reason.clone())));
                }
                let _ = tx.send(Incoming::Closed(reason));
            })
            .expect("spawn reader");
        Ok((
            Client {
                writer,
                pending,
                next_id: AtomicU64::new(1),
                welcome,
                target: target.clone(),
                child: Mutex::new(child),
                shutdown,
            },
            rx,
        ))
    }

    fn send(&self, frame: &Frame) -> Result<(), ApiError> {
        proto::write_frame(&mut *self.writer.lock().unwrap(), frame).map_err(|e| ApiError::new("disconnected", e.to_string()))
    }

    /// Sends a request and returns a receiver for its response.
    pub fn call_async(&self, method: &str, params: Value) -> Receiver<Result<Value, ApiError>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx.clone());
        let req = json!({"type": "request", "id": id, "method": method, "params": params});
        if let Err(e) = self.send(&Frame::Json(req)) {
            self.pending.lock().unwrap().remove(&id);
            let _ = tx.send(Err(e));
        }
        rx
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        self.call_async(method, params)
            .recv()
            .unwrap_or_else(|_| Err(ApiError::new("disconnected", "connection closed")))
    }

    pub fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, ApiError> {
        match self.call_async(method, params).recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => Err(ApiError::new(proto::code::TIMEOUT, format!("no response to {method}"))),
        }
    }

    pub fn send_input(&self, sub: u32, seq: u64, bytes: &[u8]) -> Result<(), ApiError> {
        self.send(&Frame::Input { sub, seq, bytes: bytes.to_vec() })
    }
}

fn find_daemon() -> PathBuf {
    if let Some(p) = std::env::var_os("HEARTHD") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("hearthd");
            if c.exists() {
                return c;
            }
        }
    }
    PathBuf::from("hearthd")
}

/// Ensures a personal server is listening on the default socket, starting
/// one in the background when needed. Returns true if it was started.
pub fn ensure_local_server() -> Result<bool, ConnectError> {
    let sock = paths::socket_path();
    if UnixStream::connect(&sock).is_ok() {
        return Ok(false);
    }
    let unit_started = std::env::var_os("HEARTH_SOCKET").is_none()
        && Command::new("systemctl")
            .args(["--user", "start", "hearth.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    if !unit_started {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(find_daemon());
        cmd.arg("--auto-exit").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| ConnectError::NoServer(format!("cannot start hearthd: {e}")))?;
        // Reap it if it exits while this process is still running.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(&sock).is_ok() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(ConnectError::NoServer(format!("server did not start at {}", sock.display())))
}
