//! OSC 7501 program status records (spec revision 0.4).
//!
//! Reports are untrusted. Every limit is checked before the store changes;
//! a report that breaks a limit is discarded whole.

use base64::Engine;
use serde::Serialize;
use std::collections::BTreeMap;

pub const MAX_SEQUENCE: usize = 4096;
pub const MAX_RECORDS: usize = 256;
pub const REPLY: &[u8] = b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    pub id: String,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    /// Monotonic update counter, used for least-recently-updated eviction.
    #[serde(skip)]
    pub touched: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Report applied or cleared; the store changed.
    Changed,
    /// Feature-detection query; the caller must write `REPLY`.
    Query,
    /// Discarded (invalid / over limit).
    Ignored,
}

fn valid_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

fn valid_id(id: &str) -> bool {
    id.len() <= 128 && id.split('/').count() <= 8 && id.split('/').all(valid_segment)
}

fn decode_text(v: &str, max_enc: usize, max_dec: usize) -> Result<String, ()> {
    if v.len() > max_enc {
        return Err(());
    }
    let cfg = base64::engine::GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent);
    let eng = base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, cfg);
    let bytes = eng.decode(v).map_err(|_| ())?;
    if bytes.len() > max_dec {
        return Err(());
    }
    let s = String::from_utf8(bytes).map_err(|_| ())?;
    if s.chars().any(|c| (c as u32) < 0x20 || (0x7f..=0x9f).contains(&(c as u32))) {
        return Err(());
    }
    Ok(s)
}

/// Removes bidi overrides and invisible formatting characters for display
/// outside the terminal grid.
pub fn disarm(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(*c as u32, 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2069 | 0xfeff))
        .collect()
}

#[derive(Debug, Default, Clone)]
pub struct Store {
    records: BTreeMap<String, Record>,
    clock: u64,
}

fn is_descendant(id: &str, ancestor: &str) -> bool {
    id == ancestor || (id.len() > ancestor.len() && id.starts_with(ancestor) && id.as_bytes()[ancestor.len()] == b'/')
}

impl Store {
    pub fn records(&self) -> Vec<Record> {
        let mut v: Vec<Record> = self.records.values().cloned().collect();
        // Fill inherited `app` from the nearest ancestor that has one.
        for r in &mut v {
            if r.app.is_none() {
                let mut cur = r.id.clone();
                loop {
                    let parent = match cur.rfind('/') {
                        Some(i) => cur[..i].to_string(),
                        None if !cur.is_empty() => String::new(),
                        None => break,
                    };
                    if let Some(p) = self.records.get(&parent) {
                        if p.app.is_some() {
                            r.app = p.app.clone();
                            break;
                        }
                    }
                    cur = parent;
                    if cur.is_empty() && !self.records.contains_key("") {
                        break;
                    }
                    if cur.is_empty() {
                        break;
                    }
                }
            }
        }
        v
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Apply the body of an OSC 7501 sequence (the bytes after `7501;`).
    /// `seq_len` is the full length from OSC through ST.
    pub fn apply(&mut self, body: &[u8], seq_len: usize) -> Outcome {
        if seq_len > MAX_SEQUENCE {
            return Outcome::Ignored;
        }
        let Ok(body) = std::str::from_utf8(body) else { return Outcome::Ignored };
        if body.trim() == "?" {
            return Outcome::Query;
        }
        let mut pairs: BTreeMap<&str, &str> = BTreeMap::new();
        for pair in body.split(':') {
            let pair = pair.trim();
            let Some((k, v)) = pair.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() {
                continue;
            }
            if k.len() > 16 {
                return Outcome::Ignored;
            }
            if !k.bytes().all(|b| b.is_ascii_lowercase()) || !v.bytes().all(valid_value_byte) {
                continue;
            }
            pairs.insert(k, v);
        }
        let state = match pairs.get("state").copied() {
            Some("idle") => Some(State::Idle),
            Some("working") => Some(State::Working),
            Some("done") => Some(State::Done),
            Some("blocked") => Some(State::Blocked),
            Some("error") => Some(State::Error),
            Some("clear") => None,
            _ => return Outcome::Ignored,
        };
        let id = pairs.get("id").copied().unwrap_or("");
        if !id.is_empty() && !valid_id(id) {
            return Outcome::Ignored;
        }
        let app = match pairs.get("app") {
            Some(a) if a.len() > 32 => return Outcome::Ignored,
            Some(a) if !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')) => {
                Some(a.to_string())
            }
            _ => None,
        };
        let msg = match pairs.get("msg") {
            Some(v) => match decode_text(v, 2732, 2048) {
                Ok(s) => Some(s),
                Err(_) => return Outcome::Ignored,
            },
            None => None,
        };
        let title = match pairs.get("title") {
            Some(v) => match decode_text(v, 256, 192) {
                Ok(s) => Some(s),
                Err(_) => return Outcome::Ignored,
            },
            None => None,
        };
        let Some(state) = state else {
            if id.is_empty() {
                self.records.clear();
            } else {
                self.records.retain(|k, _| !is_descendant(k, id));
            }
            return Outcome::Changed;
        };
        let kind = if state == State::Blocked {
            pairs.get("kind").filter(|k| matches!(**k, "permission" | "question" | "auth")).map(|k| k.to_string())
        } else {
            None
        };
        let progress = if matches!(state, State::Working | State::Blocked) {
            pairs.get("progress").and_then(|p| {
                if p.bytes().all(|b| b.is_ascii_digit()) { p.parse::<u8>().ok().filter(|n| *n <= 100) } else { None }
            })
        } else {
            None
        };
        self.clock += 1;
        if !self.records.contains_key(id) && self.records.len() >= MAX_RECORDS {
            if let Some(oldest) = self.records.values().min_by_key(|r| r.touched).map(|r| r.id.clone()) {
                self.records.remove(&oldest);
            }
        }
        self.records.insert(
            id.to_string(),
            Record { id: id.to_string(), state, kind, progress, app, title, msg, touched: self.clock },
        );
        Outcome::Changed
    }

    /// Process exit or new shell prompt (OSC 133;A): drop working/blocked.
    pub fn on_prompt_or_exit(&mut self) -> bool {
        let before = self.records.len();
        self.records.retain(|_, r| !matches!(r.state, State::Working | State::Blocked));
        before != self.records.len()
    }

    /// Full reset (RIS) removes every record.
    pub fn reset(&mut self) -> bool {
        let changed = !self.records.is_empty();
        self.records.clear();
        changed
    }

    /// The single most important state, for tab indicators.
    pub fn summary(&self) -> Option<State> {
        let rank = |s: State| match s {
            State::Blocked => 5,
            State::Error => 4,
            State::Working => 3,
            State::Done => 2,
            State::Idle => 1,
        };
        self.records.values().map(|r| r.state).max_by_key(|s| rank(*s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ap(s: &mut Store, body: &str) -> Outcome {
        s.apply(body.as_bytes(), body.len() + 8)
    }

    #[test]
    fn spec_examples() {
        let mut s = Store::default();
        assert_eq!(ap(&mut s, "state=working:app=brew:msg=SW5zdGFsbGluZyB1cGRhdGVz"), Outcome::Changed);
        assert_eq!(s.records()[0].msg.as_deref(), Some("Installing updates"));
        ap(&mut s, "state=done:app=brew:msg=VXBncmFkZWQgMTIgcGFja2FnZXM=");
        assert_eq!(s.records()[0].state, State::Done);
        s.on_prompt_or_exit();
        assert_eq!(s.records().len(), 1, "done survives prompt");
    }

    #[test]
    fn hierarchy_inherit_and_clear() {
        let mut s = Store::default();
        ap(&mut s, "state=working:app=deploy:msg=RGVwbG95aW5nIHYyLjQuMQ==");
        ap(&mut s, "state=working:id=us-east:title=VVMgRWFzdA==:progress=40:msg=UHVzaGluZyBpbWFnZQ==");
        ap(&mut s, "state=blocked:id=us-east/db:kind=permission");
        let r = s.records();
        let east = r.iter().find(|r| r.id == "us-east").unwrap();
        assert_eq!(east.app.as_deref(), Some("deploy"));
        assert_eq!(east.progress, Some(40));
        assert_eq!(s.summary(), Some(State::Blocked));
        ap(&mut s, "state=clear:id=us-east");
        assert_eq!(s.records().len(), 1);
        ap(&mut s, "state=clear");
        assert!(s.is_empty());
    }

    #[test]
    fn replacement_drops_absent_keys() {
        let mut s = Store::default();
        ap(&mut s, "state=working:app=x:progress=10");
        ap(&mut s, "state=working");
        let r = &s.records()[0];
        assert_eq!(r.app, None);
        assert_eq!(r.progress, None);
    }

    #[test]
    fn invalid_reports() {
        let mut s = Store::default();
        assert_eq!(ap(&mut s, "app=x"), Outcome::Ignored, "missing state");
        assert_eq!(ap(&mut s, "state=sleepy"), Outcome::Ignored);
        assert_eq!(ap(&mut s, "state=done:id=a//b"), Outcome::Ignored, "bad id");
        // '!' is outside the value set: the msg pair is skipped, the report applies.
        assert_eq!(ap(&mut s, "state=done:msg=!!!"), Outcome::Changed);
        s.reset();
        // msg with control char (newline): "a\nb" -> YQpi
        assert_eq!(ap(&mut s, "state=done:msg=YQpi"), Outcome::Ignored);
        assert_eq!(ap(&mut s, &format!("state=done:app={}", "a".repeat(33))), Outcome::Ignored);
        assert_eq!(s.apply(b"state=done", 5000), Outcome::Ignored, "over 4096");
        assert!(s.is_empty());
        // unknown keys and malformed pairs are skipped
        assert_eq!(ap(&mut s, "state=idle:future=1:junk:kind=permission"), Outcome::Changed);
        assert_eq!(s.records()[0].kind, None, "kind only for blocked");
        assert_eq!(ap(&mut s, "?"), Outcome::Query);
    }

    #[test]
    fn record_cap_evicts_oldest() {
        let mut s = Store::default();
        for i in 0..MAX_RECORDS + 5 {
            ap(&mut s, &format!("state=idle:id=r{i}"));
        }
        let r = s.records();
        assert_eq!(r.len(), MAX_RECORDS);
        assert!(!r.iter().any(|r| r.id == "r0"));
    }

}
