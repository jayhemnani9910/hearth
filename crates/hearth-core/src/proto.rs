//! hearth.v1 wire protocol: length-prefixed frames over any byte stream
//! (Unix socket, TCP, or an SSH-forwarded stdio pipe). See docs/PROTOCOL.md.

use serde_json::Value;
use std::io::{self, Read, Write};

pub const PROTOCOL: &str = "hearth.v1";
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 16 << 20;

const K_JSON: u8 = 1;
const K_DATA: u8 = 2;
const K_SNAPSHOT: u8 = 3;
const K_INPUT: u8 = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Control message (hello/welcome/request/response/event).
    Json(Value),
    /// Terminal output for subscription `sub`, starting at stream `offset`.
    Data { sub: u32, offset: u64, bytes: Vec<u8> },
    /// Terminal state valid at stream `offset`; Data continues from there.
    Snapshot { sub: u32, offset: u64, bytes: Vec<u8> },
    /// Client input for the terminal behind `sub`; `seq` is acknowledged.
    Input { sub: u32, seq: u64, bytes: Vec<u8> },
}

pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut body = Vec::new();
    match frame {
        Frame::Json(v) => {
            body.push(K_JSON);
            body.extend_from_slice(&serde_json::to_vec(v).expect("json"));
        }
        Frame::Data { sub, offset, bytes } | Frame::Snapshot { sub, offset, bytes } => {
            body.push(if matches!(frame, Frame::Data { .. }) { K_DATA } else { K_SNAPSHOT });
            body.extend_from_slice(&sub.to_le_bytes());
            body.extend_from_slice(&offset.to_le_bytes());
            body.extend_from_slice(bytes);
        }
        Frame::Input { sub, seq, bytes } => {
            body.push(K_INPUT);
            body.extend_from_slice(&sub.to_le_bytes());
            body.extend_from_slice(&seq.to_le_bytes());
            body.extend_from_slice(bytes);
        }
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&encode(frame))?;
    w.flush()
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Reads one frame. Returns `Ok(None)` on clean EOF at a frame boundary.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(bad("frame length out of range"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let kind = body[0];
    let rest = &body[1..];
    let head = |rest: &[u8]| -> io::Result<(u32, u64)> {
        if rest.len() < 12 {
            return Err(bad("short frame"));
        }
        Ok((u32::from_le_bytes(rest[..4].try_into().unwrap()), u64::from_le_bytes(rest[4..12].try_into().unwrap())))
    };
    Ok(Some(match kind {
        K_JSON => Frame::Json(serde_json::from_slice(rest).map_err(|_| bad("invalid json"))?),
        K_DATA => {
            let (sub, offset) = head(rest)?;
            Frame::Data { sub, offset, bytes: rest[12..].to_vec() }
        }
        K_SNAPSHOT => {
            let (sub, offset) = head(rest)?;
            Frame::Snapshot { sub, offset, bytes: rest[12..].to_vec() }
        }
        K_INPUT => {
            let (sub, seq) = head(rest)?;
            Frame::Input { sub, seq, bytes: rest[12..].to_vec() }
        }
        _ => return Err(bad("unknown frame kind")),
    }))
}

/// Structured API error codes (stable; CLI exit codes derive from them).
pub mod code {
    pub const INVALID: &str = "invalid_params";
    pub const NOT_FOUND: &str = "not_found";
    pub const AMBIGUOUS: &str = "ambiguous";
    pub const UNKNOWN_METHOD: &str = "unknown_method";
    pub const DENIED: &str = "denied";
    pub const TIMEOUT: &str = "timeout";
    pub const FAILED: &str = "failed";
    pub const NOT_READY: &str = "not_ready";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip() {
        let frames = vec![
            Frame::Json(json!({"type":"hello"})),
            Frame::Data { sub: 3, offset: 99, bytes: b"abc".to_vec() },
            Frame::Snapshot { sub: 1, offset: 0, bytes: vec![] },
            Frame::Input { sub: 7, seq: 2, bytes: b"\x03".to_vec() },
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut r = &buf[..];
        for f in &frames {
            assert_eq!(&read_frame(&mut r).unwrap().unwrap(), f);
        }
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn rejects_huge_length() {
        let buf = (u32::MAX).to_le_bytes();
        assert!(read_frame(&mut &buf[..]).is_err());
    }
}
