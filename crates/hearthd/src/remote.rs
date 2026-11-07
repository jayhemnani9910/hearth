//! Opt-in TCP listener for tailnet access. Every connection's identity is
//! resolved with `tailscale whois` for its peer address and checked against
//! the `allow` list; an empty list admits nobody. Identity is never taken
//! from anything the peer sends.
//!
//! `whois` on this machine's own address names the machine's owner, whoever
//! actually connected. So a connection from a local address must also come
//! from a socket owned by our own uid (looked up in /proc/net/tcp).

use crate::server::Server;
use hearth_core::proto::{self, Frame};
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
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

/// True for loopback and for any address of this machine's interfaces.
pub fn is_local(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if ip.is_loopback() {
        return true;
    }
    let mut found = false;
    unsafe {
        let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifs) != 0 {
            return true; // Can't tell: treat as local, which is the stricter path.
        }
        let mut cur = ifs;
        while !cur.is_null() {
            let a = (*cur).ifa_addr;
            if !a.is_null() {
                let addr = match (*a).sa_family as i32 {
                    libc::AF_INET => Some(IpAddr::V4(Ipv4Addr::from(u32::from_be((*(a as *const libc::sockaddr_in)).sin_addr.s_addr)))),
                    libc::AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from((*(a as *const libc::sockaddr_in6)).sin6_addr.s6_addr))),
                    _ => None,
                };
                found |= addr == Some(ip);
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(ifs);
    }
    found
}

/// One `/proc/net/tcp{,6}` address ("0100007F:1F90"): 32-bit words in host order.
fn proc_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let mut bytes = vec![];
    for i in (0..ip.len()).step_by(8) {
        bytes.extend(u32::from_str_radix(ip.get(i..i + 8)?, 16).ok()?.to_ne_bytes());
    }
    let ip = match bytes.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?),
        16 => IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?),
        _ => return None,
    };
    Some(SocketAddr::new(ip.to_canonical(), port))
}

/// The uid owning the client end of a local connection `peer` -> `me`.
fn find_uid(table: &str, peer: SocketAddr, me: SocketAddr) -> Option<u32> {
    let norm = |a: SocketAddr| SocketAddr::new(a.ip().to_canonical(), a.port());
    let (peer, me) = (norm(peer), norm(me));
    table.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 7 && proc_addr(f[1])? == peer && proc_addr(f[2])? == me).then(|| f[7].parse().ok()).flatten()
    })
}

fn local_peer_uid(peer: SocketAddr, me: SocketAddr) -> Option<u32> {
    ["/proc/net/tcp", "/proc/net/tcp6"].iter().find_map(|f| find_uid(&std::fs::read_to_string(f).ok()?, peer, me))
}

pub fn authorize(server: &Server, peer: SocketAddr, me: SocketAddr) -> Result<String, String> {
    if is_local(peer.ip()) {
        let mine = unsafe { libc::getuid() };
        match local_peer_uid(peer, me) {
            Some(u) if u == mine => {}
            Some(u) => return Err(format!("connection from another local account (uid {u})")),
            None => return Err("cannot tell which local account opened this connection".into()),
        }
    }
    let s = server.lua.settings();
    let login = whois(&s.whois, &peer.to_string())?;
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
                let (Ok(peer), Ok(me)) = (s.peer_addr(), s.local_addr()) else { return };
                let _ = s.set_nodelay(true);
                match authorize(&server, peer, me) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:D431 0100007F:1E15 01 00000000:00000000 00:00000000 00000000  1001        0 1 1
   1: 0100007F:1E15 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 2 1
   2: 0100007F:D432 0100007F:1E15 01 00000000:00000000 00:00000000 00000000  1000        0 3 1
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0000000000000000FFFF00000100007F:D433 0000000000000000FFFF00000100007F:1E15 01 00000000:00000000 00:00000000 00000000  1002        0 4 1
";

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn finds_the_account_behind_a_local_connection() {
        let server = a("127.0.0.1:7701");
        assert_eq!(find_uid(TCP, a("127.0.0.1:54321"), server), Some(1001), "another account");
        assert_eq!(find_uid(TCP, a("127.0.0.1:54322"), server), Some(1000));
        assert_eq!(find_uid(TCP, a("127.0.0.1:1"), server), None);
        // A v6 listener sees v4-mapped peers.
        assert_eq!(find_uid(TCP6, a("[::ffff:127.0.0.1]:54323"), a("[::ffff:127.0.0.1]:7701")), Some(1002));
    }

    #[test]
    fn local_addresses_are_recognised() {
        assert!(is_local("127.0.0.1".parse().unwrap()));
        assert!(is_local("::1".parse().unwrap()));
        assert!(is_local("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_local("203.0.113.9".parse().unwrap()), "TEST-NET-3 is never assigned here");
    }
}
