//! hearthd: the Hearth server. Owns sessions, PTYs and programs; clients
//! attach and detach without affecting them.

use hearth_core::paths;
use hearthd::server::{Options, Server};
use hearthd::{lua, persist, remote};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: hearthd [--socket PATH] [--config FILE] [--listen tcp:ADDR:PORT]... [--auto-exit] [--no-persist] [--no-restore]";

fn peer_uid(fd: i32) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut _, &mut len) };
    (r == 0).then_some(cred.uid)
}

fn main() {
    let mut socket = paths::socket_path();
    let mut config = lua::config_path();
    let mut auto_exit = false;
    let mut persist = true;
    let mut restore = true;
    let mut listen = vec![];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().expect(USAGE).into(),
            "--config" => config = args.next().expect(USAGE).into(),
            "--listen" => listen.push(args.next().expect(USAGE)),
            "--auto-exit" => auto_exit = true,
            "--no-persist" => persist = false,
            "--no-restore" => restore = false,
            "--version" => {
                println!("hearthd {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }

    // Block termination signals in every thread; one thread waits for them.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }

    if let Some(dir) = socket.parent() {
        if let Err(e) = paths::ensure_private_dir(dir) {
            eprintln!("hearthd: runtime directory {}: {e}", dir.display());
            std::process::exit(1);
        }
    }
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        eprintln!("hearthd: a server is already running at {}", socket.display());
        std::process::exit(1);
    }
    if socket.exists() {
        // Only remove a stale socket that is ours.
        if let Err(e) = paths::check_socket_owner(&socket) {
            eprintln!("hearthd: refusing to replace {}: {e}", socket.display());
            std::process::exit(1);
        }
        let _ = std::fs::remove_file(&socket);
    }
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hearthd: bind {}: {e}", socket.display());
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));

    let (server, notices) = Server::new(Options { socket: socket.clone(), auto_exit, persist: AtomicBool::new(persist), listen: Mutex::new(vec![]) });
    if let Err(e) = server.lua.load_file(config.clone()) {
        eprintln!("hearthd: config: {e} (continuing with defaults)");
    }
    let settings = server.lua.settings();
    let mut all_listen = settings.listen.clone();
    all_listen.extend(listen);

    let mut persist_ok = persist;
    if persist && restore && settings.restore {
        match persist::load() {
            Ok(Some(saved)) => {
                let n = persist::restore(&server, saved);
                eprintln!("hearthd: restored layout with {n} new shell(s)");
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("hearthd: {e}; persistence disabled for this run");
                persist_ok = false;
            }
        }
    }
    server.opts.persist.store(persist_ok, Ordering::Relaxed);
    *server.opts.listen.lock().unwrap() = all_listen.clone();
    server.core.lock().unwrap().dirty = true;

    {
        let s = server.clone();
        std::thread::Builder::new()
            .name("notices".into())
            .spawn(move || {
                let mut activity = HashMap::new();
                for n in notices {
                    s.handle_notice(n, &mut activity);
                }
            })
            .unwrap();
    }
    {
        let s = server.clone();
        std::thread::Builder::new()
            .name("accept".into())
            .spawn(move || {
                use std::os::fd::AsRawFd;
                let me = unsafe { libc::getuid() };
                for stream in listener.incoming().flatten() {
                    if peer_uid(stream.as_raw_fd()) != Some(me) {
                        // Another account must not reach our terminals.
                        continue;
                    }
                    let s = s.clone();
                    std::thread::spawn(move || {
                        let r = stream.try_clone().unwrap();
                        s.serve_stream(r, stream, format!("uid:{me}"), "unix");
                    });
                }
            })
            .unwrap();
    }
    for addr in &all_listen {
        let a = addr.strip_prefix("tcp:").unwrap_or(addr);
        if let Err(e) = remote::listen(server.clone(), a) {
            eprintln!("hearthd: listen {addr}: {e}");
        }
    }
    {
        let s = server.clone();
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                let mut sig = 0;
                unsafe { libc::sigwait(&set, &mut sig) };
                eprintln!("hearthd: signal {sig}, stopping");
                s.stop.store(true, Ordering::Relaxed);
            })
            .unwrap();
    }
    eprintln!("hearthd {} listening on {}", env!("CARGO_PKG_VERSION"), socket.display());

    let mut idle_since: Option<Instant> = None;
    while !server.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        server.persist_now();
        if auto_exit {
            if server.idle() {
                let t = *idle_since.get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_secs(30) {
                    eprintln!("hearthd: no sessions or clients for 30s, exiting");
                    break;
                }
            } else {
                idle_since = None;
            }
        }
    }
    server.persist_now();
    let _ = std::fs::remove_file(&socket);
    // Closing the PTY masters on exit hangs up any remaining programs.
}
