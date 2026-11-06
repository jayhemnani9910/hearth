//! Opt-in TCP listener for tailnet access. Every connection's identity is
//! resolved with `tailscale whois` for its peer address and checked against
//! the `allow` list; an empty list admits nobody. Identity is never taken
//! from anything the peer sends.

use crate::server::Server;
use hearth_core::proto::{self, Frame};
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// Returns the tailnet login name for a peer address.
pub fn whois(cmd: &str, peer: &str) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(["whois", "--json", peer])
        .output()
        .map_err(|e| format!("cannot run {cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} whois {peer} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("bad whois output: {e}"))?;
    v["UserProfile"]["LoginName"].as_str().map(String::from).ok_or_else(|| "whois returned no LoginName".into())
}

pub fn authorize(server: &Server, peer: &str) -> Result<String, String> {
    let s = server.lua.settings();
    let login = whois(&s.whois, peer)?;
    if s.allow.iter().any(|a| a == &login) {
        Ok(login)
    } else {
        Err(format!("identity '{login}' is not in the allow list"))
    }
}

fn reject(mut stream: TcpStream, reason: String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    // Let the client read a structured rejection after its hello.
    let mut r = std::io::BufReader::new(stream.try_clone().unwrap());
    let _ = proto::read_frame(&mut r);
    let _ = proto::write_frame(&mut stream, &Frame::Json(json!({"type": "reject", "code": "identity", "reason": reason})));
}

pub fn listen(server: Arc<Server>, addr: &str) -> std::io::Result<()> {
    let l = TcpListener::bind(addr)?;
    eprintln!("hearthd: listening on tcp:{} (tailnet identity required)", l.local_addr()?);
    std::thread::Builder::new().name(format!("tcp-{addr}")).spawn(move || {
        for s in l.incoming().flatten() {
            let server = server.clone();
            std::thread::spawn(move || {
                let peer = match s.peer_addr() {
                    Ok(p) => p.to_string(),
                    Err(_) => return,
                };
                let _ = s.set_nodelay(true);
                match authorize(&server, &peer) {
                    Ok(login) => {
                        let r = s.try_clone().unwrap();
                        server.serve_stream(r, s, format!("tailscale:{login}"), "tcp");
                    }
                    Err(reason) => {
                        eprintln!("hearthd: rejected {peer}: {reason}");
                        reject(s, reason);
                    }
                }
            });
        }
    })?;
    Ok(())
}
