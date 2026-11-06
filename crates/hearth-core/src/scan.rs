//! Byte-level VT stream segmenter.
//!
//! Splits a terminal output stream into plain text and complete escape
//! sequences, and holds back any incomplete UTF-8 character or escape
//! sequence until it is finished. The daemon uses it to feed its terminal
//! model only at parser-safe boundaries (so a snapshot never sits inside a
//! sequence), and to observe OSC 7 / 133 / 7501, scroll regions and alternate
//! screen switches. The client uses it to strip terminal queries so that only
//! the server answers them.

/// Hard cap for bytes held back in an unfinished sequence. A program that
/// opens a string sequence and never terminates it must not grow memory
/// without bound; once exceeded the held bytes are released as-is.
pub const MAX_HELD: usize = 1 << 20;
/// Bytes of a CSI/OSC/DCS body kept for interpretation.
const MAX_BODY: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seq {
    /// CSI: `private` is a leading `<`/`=`/`>`/`?`, `params` the parameter
    /// bytes, `inter` intermediate bytes, `fin` the final byte.
    Csi { private: Option<u8>, params: Vec<u8>, inter: Vec<u8>, fin: u8 },
    /// OSC payload without the introducer and terminator.
    Osc { body: Vec<u8>, overflow: bool },
    /// DCS / SOS / PM / APC: `kind` is the introducing byte (P, X, ^, _).
    Str { kind: u8, body: Vec<u8>, overflow: bool },
    /// Plain escape: intermediates and final byte (e.g. `ESC ( B`, `ESC c`).
    Esc { inter: Vec<u8>, fin: u8 },
}

impl Seq {
    /// Numeric CSI parameters (sub-parameters after `:` are dropped).
    pub fn csi_params(&self) -> Vec<u32> {
        match self {
            Seq::Csi { params, .. } => params
                .split(|b| *b == b';')
                .map(|p| {
                    let p = p.split(|b| *b == b':').next().unwrap_or(&[]);
                    std::str::from_utf8(p).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
                })
                .collect(),
            _ => vec![],
        }
    }

    pub fn is_dec_mode(&self, set: bool, mode: u32) -> bool {
        matches!(self, Seq::Csi { private: Some(b'?'), inter, fin, .. }
            if inter.is_empty() && *fin == if set { b'h' } else { b'l' })
            && self.csi_params().contains(&mode)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Plain bytes (text and C0 controls).
    Text(Vec<u8>),
    /// A complete escape sequence and its raw bytes.
    Seq(Seq, Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Ground,
    Utf8 { left: u8 },
    Esc,
    EscInter,
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

#[derive(Debug)]
pub struct Segmenter {
    st: St,
    /// Raw bytes of the sequence (or UTF-8 char) currently being collected.
    held: Vec<u8>,
    private: Option<u8>,
    params: Vec<u8>,
    inter: Vec<u8>,
    body: Vec<u8>,
    overflow: bool,
    str_kind: u8,
    text: Vec<u8>,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        Segmenter {
            st: St::Ground,
            held: Vec::new(),
            private: None,
            params: Vec::new(),
            inter: Vec::new(),
            body: Vec::new(),
            overflow: false,
            str_kind: 0,
            text: Vec::new(),
        }
    }

    /// Number of bytes currently held back (not yet emitted).
    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    pub fn at_ground(&self) -> bool {
        self.st == St::Ground
    }

    fn flush_text(&mut self, out: &mut Vec<Piece>) {
        if !self.text.is_empty() {
            out.push(Piece::Text(std::mem::take(&mut self.text)));
        }
    }

    fn reset_seq(&mut self) {
        self.private = None;
        self.params.clear();
        self.inter.clear();
        self.body.clear();
        self.overflow = false;
    }

    fn emit(&mut self, seq: Seq, out: &mut Vec<Piece>) {
        self.flush_text(out);
        out.push(Piece::Seq(seq, std::mem::take(&mut self.held)));
        self.reset_seq();
        self.st = St::Ground;
    }

    /// Abort the current sequence, releasing its bytes as text.
    fn abort(&mut self) {
        let held = std::mem::take(&mut self.held);
        self.text.extend_from_slice(&held);
        self.reset_seq();
        self.st = St::Ground;
    }

    fn push_body(&mut self, b: u8) {
        if self.body.len() < MAX_BODY {
            self.body.push(b);
        } else {
            self.overflow = true;
        }
    }

    /// Process bytes; complete pieces are appended to `out`. Incomplete
    /// trailing data stays held until a later call (or `MAX_HELD`).
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<Piece>) {
        for &b in bytes {
            self.step(b, out);
            if self.held.len() > MAX_HELD {
                self.abort();
            }
        }
        // Text bytes are always complete characters; only `held` waits.
        self.flush_text(out);
    }

    /// Release everything held, as text (used on stream end).
    pub fn finish(&mut self, out: &mut Vec<Piece>) {
        if self.st != St::Ground {
            self.abort();
        }
        self.flush_text(out);
    }

    fn step(&mut self, b: u8, out: &mut Vec<Piece>) {
        match self.st {
            St::Ground => match b {
                0x1b => {
                    self.held.push(b);
                    self.st = St::Esc;
                }
                0xc0..=0xdf => self.start_utf8(b, 1),
                0xe0..=0xef => self.start_utf8(b, 2),
                0xf0..=0xf7 => self.start_utf8(b, 3),
                _ => self.text.push(b),
            },
            St::Utf8 { left } => {
                if (0x80..=0xbf).contains(&b) {
                    self.held.push(b);
                    if left == 1 {
                        let held = std::mem::take(&mut self.held);
                        self.text.extend_from_slice(&held);
                        self.st = St::Ground;
                    } else {
                        self.st = St::Utf8 { left: left - 1 };
                    }
                } else {
                    // Invalid continuation: release and reprocess this byte.
                    self.abort();
                    self.step(b, out);
                }
            }
            St::Esc => {
                self.held.push(b);
                match b {
                    b'[' => self.st = St::Csi,
                    b']' => self.st = St::Osc,
                    b'P' | b'X' | b'^' | b'_' => {
                        self.str_kind = b;
                        self.st = St::Str;
                    }
                    0x20..=0x2f => {
                        self.inter.push(b);
                        self.st = St::EscInter;
                    }
                    0x18 | 0x1a => self.abort(),
                    0x1b => {
                        // ESC ESC: the first one is complete (meaningless) text.
                        self.held.pop();
                        self.abort();
                        self.held.push(0x1b);
                        self.st = St::Esc;
                    }
                    0x00..=0x1f => {
                        // C0 inside escape executes; keep collecting.
                    }
                    _ => {
                        let inter = std::mem::take(&mut self.inter);
                        self.emit(Seq::Esc { inter, fin: b }, out);
                    }
                }
            }
            St::EscInter => {
                self.held.push(b);
                match b {
                    0x20..=0x2f => self.inter.push(b),
                    0x18 | 0x1a => self.abort(),
                    0x00..=0x1f => {}
                    _ => {
                        let inter = std::mem::take(&mut self.inter);
                        self.emit(Seq::Esc { inter, fin: b }, out);
                    }
                }
            }
            St::Csi => {
                self.held.push(b);
                match b {
                    b'<' | b'=' | b'>' | b'?'
                        if self.params.is_empty() && self.private.is_none() && self.inter.is_empty() =>
                    {
                        self.private = Some(b)
                    }
                    0x30..=0x3f => {
                        if self.params.len() < MAX_BODY {
                            self.params.push(b)
                        }
                    }
                    0x20..=0x2f => self.inter.push(b),
                    0x40..=0x7e => {
                        let seq = Seq::Csi {
                            private: self.private,
                            params: std::mem::take(&mut self.params),
                            inter: std::mem::take(&mut self.inter),
                            fin: b,
                        };
                        self.emit(seq, out);
                    }
                    0x18 | 0x1a => self.abort(),
                    0x1b => {
                        self.held.pop();
                        self.abort();
                        self.held.push(0x1b);
                        self.st = St::Esc;
                    }
                    _ => {}
                }
            }
            St::Osc => {
                self.held.push(b);
                match b {
                    0x07 => {
                        let seq = Seq::Osc { body: std::mem::take(&mut self.body), overflow: self.overflow };
                        self.emit(seq, out);
                    }
                    0x1b => self.st = St::OscEsc,
                    0x18 | 0x1a => self.abort(),
                    _ => self.push_body(b),
                }
            }
            St::OscEsc => {
                self.held.push(b);
                if b == b'\\' {
                    let seq = Seq::Osc { body: std::mem::take(&mut self.body), overflow: self.overflow };
                    self.emit(seq, out);
                } else {
                    // ESC aborts the OSC; the ESC begins a new sequence.
                    let last = self.held.pop();
                    self.held.pop();
                    self.abort();
                    self.held.push(0x1b);
                    self.st = St::Esc;
                    if let Some(l) = last {
                        self.step(l, out);
                    }
                }
            }
            St::Str => {
                self.held.push(b);
                match b {
                    0x1b => self.st = St::StrEsc,
                    0x18 | 0x1a => self.abort(),
                    _ => self.push_body(b),
                }
            }
            St::StrEsc => {
                self.held.push(b);
                if b == b'\\' {
                    let seq = Seq::Str {
                        kind: self.str_kind,
                        body: std::mem::take(&mut self.body),
                        overflow: self.overflow,
                    };
                    self.emit(seq, out);
                } else {
                    let last = self.held.pop();
                    self.held.pop();
                    self.abort();
                    self.held.push(0x1b);
                    self.st = St::Esc;
                    if let Some(l) = last {
                        self.step(l, out);
                    }
                }
            }
        }
    }

    fn start_utf8(&mut self, b: u8, left: u8) {
        self.held.push(b);
        self.st = St::Utf8 { left };
    }
}

/// True when a sequence asks the terminal to reply. Clients drop these so
/// that the server's terminal model is the single reply authority.
pub fn is_query(seq: &Seq) -> bool {
    match seq {
        Seq::Csi { private, params, inter, fin } => match (private, inter.as_slice(), fin) {
            // DA1 / DA2 / DA3
            (None | Some(b'>') | Some(b'='), [], b'c') => true,
            // DSR (5n, 6n) and DEC DSR (?6n etc.)
            (None | Some(b'?'), [], b'n') => true,
            // DECRQM
            (_, [b'$'], b'p') => true,
            // XTVERSION
            (Some(b'>'), [], b'q') => true,
            // kitty keyboard query
            (Some(b'?'), [], b'u') => true,
            // XTWINOPS reports (13..21 are queries)
            (None, [], b't') => {
                let n: u32 = std::str::from_utf8(params)
                    .ok()
                    .and_then(|s| s.split(';').next().and_then(|v| v.parse().ok()))
                    .unwrap_or(0);
                (11..=21).contains(&n)
            }
            _ => false,
        },
        Seq::Osc { body, .. } => {
            let mut it = body.splitn(2, |b| *b == b';');
            let num = it.next().unwrap_or(&[]);
            let rest = it.next().unwrap_or(&[]);
            match num {
                b"4" => rest.ends_with(b"?"),
                b"10" | b"11" | b"12" | b"17" | b"19" => rest == b"?",
                b"52" => rest.ends_with(b";?"),
                b"7501" => rest == b"?",
                _ => false,
            }
        }
        // DECRQSS and XTGETTCAP
        Seq::Str { kind: b'P', body, .. } => body.starts_with(b"$q") || body.starts_with(b"+q"),
        _ => false,
    }
}

/// Client-side stream filter: holds incomplete sequences and drops queries.
#[derive(Default)]
pub struct QueryFilter {
    seg: Segmenter,
    pieces: Vec<Piece>,
}

impl QueryFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the bytes safe to feed to a local emulator.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pieces.clear();
        self.seg.push(bytes, &mut self.pieces);
        let mut out = Vec::with_capacity(bytes.len());
        for p in self.pieces.drain(..) {
            match p {
                Piece::Text(t) => out.extend_from_slice(&t),
                Piece::Seq(s, raw) => {
                    if !is_query(&s) {
                        out.extend_from_slice(&raw)
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> (Vec<Piece>, Segmenter) {
        let mut s = Segmenter::new();
        let mut out = vec![];
        for c in chunks {
            s.push(c, &mut out);
        }
        (out, s)
    }

    fn raw(pieces: &[Piece]) -> Vec<u8> {
        let mut v = vec![];
        for p in pieces {
            match p {
                Piece::Text(t) => v.extend_from_slice(t),
                Piece::Seq(_, r) => v.extend_from_slice(r),
            }
        }
        v
    }

    #[test]
    fn utf8_split_is_held() {
        let e = "é".as_bytes();
        let (out, s) = collect(&[b"a", &e[..1]]);
        assert_eq!(raw(&out), b"a");
        assert!(!s.at_ground());
        let (out, s) = collect(&[b"a", &e[..1], &e[1..]]);
        assert_eq!(raw(&out), "aé".as_bytes());
        assert!(s.at_ground());
    }

    #[test]
    fn every_split_point_roundtrips() {
        let input: &[u8] = b"hi\x1b[1;31mred\x1b]0;title\x07\x1b]7501;state=done\x1b\\\xe2\x9c\x93\x1bP$qm\x1b\\\x1b(B\x1b[?1049h end";
        for i in 0..=input.len() {
            let (out, s) = collect(&[&input[..i], &input[i..]]);
            assert!(s.at_ground(), "split {i}");
            assert_eq!(raw(&out), input, "split {i}");
        }
    }

    #[test]
    fn sequences_are_classified() {
        let (out, _) = collect(&[b"\x1b[?1049h\x1b[6n\x1b]11;?\x07\x1b]2;x\x07"]);
        let seqs: Vec<&Seq> = out
            .iter()
            .filter_map(|p| if let Piece::Seq(s, _) = p { Some(s) } else { None })
            .collect();
        assert!(seqs[0].is_dec_mode(true, 1049));
        assert!(!is_query(seqs[0]));
        assert!(is_query(seqs[1]));
        assert!(is_query(seqs[2]));
        assert!(!is_query(seqs[3]));
    }

    #[test]
    fn query_filter_drops_split_query() {
        let mut f = QueryFilter::new();
        let mut out = f.push(b"a\x1b[");
        out.extend(f.push(b"cb\x1b[0m"));
        assert_eq!(out, b"ab\x1b[0m");
    }

    #[test]
    fn unterminated_string_is_bounded() {
        let mut s = Segmenter::new();
        let mut out = vec![];
        s.push(b"\x1bP", &mut out);
        s.push(&vec![b'x'; MAX_HELD + 10], &mut out);
        assert!(s.held_len() <= MAX_HELD);
    }
}
