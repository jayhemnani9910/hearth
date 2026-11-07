//! Server-side terminal model: the canonical emulator for one terminal,
//! its reply authority, and the serializer that turns its state into a
//! byte snapshot a fresh emulator can restore exactly.

use crate::sync::Lock;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};
use hearth_core::osc7501::{self, Store};
use hearth_core::scan::{Piece, Segmenter, Seq};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

pub const DEFAULT_HISTORY: usize = 10_000;

/// Things the model wants the owner to act on after `advance`.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Bytes to write back to the PTY (query replies).
    Reply(Vec<u8>),
    Title(Option<String>),
    Cwd(String),
    StatusChanged,
    Prompt,
    Bell,
    /// OSC 52 clipboard write request from the program.
    Clipboard(String),
}

#[derive(Default)]
struct Shared {
    effects: Vec<Effect>,
    cell_w: u16,
    cell_h: u16,
    cols: u16,
    rows: u16,
}

#[derive(Clone)]
pub struct Listener(Arc<Mutex<Shared>>);

/// Default palette used to answer color queries (OSC 4/10/11).
pub fn default_color(i: usize) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0x1d, 0x1f, 0x21), (0xcc, 0x66, 0x66), (0xb5, 0xbd, 0x68), (0xf0, 0xc6, 0x74),
        (0x81, 0xa2, 0xbe), (0xb2, 0x94, 0xbb), (0x8a, 0xbe, 0xb7), (0xc5, 0xc8, 0xc6),
        (0x66, 0x66, 0x66), (0xd5, 0x4e, 0x53), (0xb9, 0xca, 0x4a), (0xe7, 0xc5, 0x47),
        (0x7a, 0xa6, 0xda), (0xc3, 0x97, 0xd8), (0x70, 0xc0, 0xb1), (0xea, 0xea, 0xea),
    ];
    let (r, g, b) = match i {
        0..=15 => BASE[i],
        16..=231 => {
            let n = i - 16;
            let v = |x: usize| if x == 0 { 0 } else { (x * 40 + 55) as u8 };
            (v(n / 36), v((n / 6) % 6), v(n % 6))
        }
        232..=255 => {
            let v = (8 + (i - 232) * 10) as u8;
            (v, v, v)
        }
        256 => (0xc5, 0xc8, 0xc6), // foreground
        257 => (0x1d, 0x1f, 0x21), // background
        _ => (0xc5, 0xc8, 0xc6),   // cursor
    };
    Rgb { r, g, b }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut s = self.0.lk();
        let fx = match event {
            Event::PtyWrite(text) => Effect::Reply(text.into_bytes()),
            Event::Title(t) => Effect::Title(Some(t)),
            Event::ResetTitle => Effect::Title(None),
            Event::Bell => Effect::Bell,
            Event::ClipboardStore(_, text) => Effect::Clipboard(text),
            Event::ColorRequest(i, f) => Effect::Reply(f(default_color(i)).into_bytes()),
            Event::TextAreaSizeRequest(f) => {
                let ws = WindowSize { num_lines: s.rows, num_cols: s.cols, cell_width: s.cell_w, cell_height: s.cell_h };
                Effect::Reply(f(ws).into_bytes())
            }
            // Clipboard reads from programs are denied: no reply.
            _ => return,
        };
        s.effects.push(fx);
    }
}

pub struct Model {
    term: Term<Listener>,
    parser: Processor,
    shared: Arc<Mutex<Shared>>,
    seg: Segmenter,
    pieces: Vec<Piece>,
    /// Stream bytes consumed by the segmenter (applied + held).
    consumed: u64,
    scroll_region: Option<(u16, u16)>,
    /// Primary screen serialized at the moment the alternate screen was
    /// entered (the primary cannot change while the alternate is active).
    primary_cache: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub status: Store,
    stop_9_4: bool,
}

fn size(cols: u16, rows: u16) -> TermSize {
    TermSize::new(cols.max(2) as usize, rows.max(1) as usize)
}

impl Model {
    pub fn new(cols: u16, rows: u16, history: usize) -> Model {
        let shared = Arc::new(Mutex::new(Shared { cols, rows, cell_w: 8, cell_h: 16, ..Default::default() }));
        let config = Config { scrolling_history: history, kitty_keyboard: true, ..Default::default() };
        let term = Term::new(config, &size(cols, rows), Listener(shared.clone()));
        Model {
            term,
            parser: Processor::new(),
            shared,
            seg: Segmenter::new(),
            pieces: Vec::new(),
            consumed: 0,
            scroll_region: None,
            primary_cache: None,
            cwd: None,
            title: None,
            status: Store::default(),
            stop_9_4: false,
        }
    }

    pub fn cols(&self) -> u16 {
        self.term.columns() as u16
    }

    pub fn rows(&self) -> u16 {
        self.term.screen_lines() as u16
    }

    /// Stream offset up to which the model state is exact (excludes bytes
    /// held in an unfinished sequence). Snapshots are valid at this offset.
    pub fn applied(&self) -> u64 {
        self.consumed - self.seg.held_len() as u64
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn set_cell_size(&mut self, w: u16, h: u16) {
        let mut s = self.shared.lk();
        s.cell_w = w;
        s.cell_h = h;
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(size(cols, rows));
        self.scroll_region = None;
        let mut s = self.shared.lk();
        s.cols = cols;
        s.rows = rows;
    }

    /// Drains pending effects. Call after every `advance`.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        let fx = std::mem::take(&mut self.shared.lk().effects);
        for e in &fx {
            if let Effect::Title(t) = e {
                self.title = t.clone();
            }
        }
        fx
    }

    fn push_effect(&self, e: Effect) {
        self.shared.lk().effects.push(e);
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.consumed += bytes.len() as u64;
        let mut pieces = std::mem::take(&mut self.pieces);
        self.seg.push(bytes, &mut pieces);
        for p in pieces.drain(..) {
            match p {
                Piece::Text(t) => self.parser.advance(&mut self.term, &t),
                Piece::Seq(seq, raw) => self.apply_seq(&seq, &raw),
            }
        }
        self.pieces = pieces;
    }

    /// End of stream: apply anything still held.
    pub fn finish(&mut self) {
        let mut pieces = Vec::new();
        self.seg.finish(&mut pieces);
        for p in pieces {
            if let Piece::Text(t) = p {
                self.parser.advance(&mut self.term, &t);
            }
        }
    }

    fn apply_seq(&mut self, seq: &Seq, raw: &[u8]) {
        match seq {
            Seq::Csi { private: None, inter, fin: b'r', .. } if inter.is_empty() => {
                let p = seq.csi_params();
                let rows = self.rows();
                let top = p.first().copied().filter(|v| *v > 0).unwrap_or(1) as u16;
                let bot = p.get(1).copied().filter(|v| *v > 0).unwrap_or(rows as u32) as u16;
                self.scroll_region = if top == 1 && bot >= rows || top >= bot { None } else { Some((top, bot.min(rows))) };
            }
            Seq::Csi { private: Some(b'?'), params, fin: b'h' | b'l', .. } if params.as_slice() == b"2026" => {
                // Synchronized output only affects rendering timing; the
                // model applies updates immediately so snapshots stay exact.
                return;
            }
            Seq::Csi { .. }
                if (seq.is_dec_mode(true, 1049) || seq.is_dec_mode(true, 1047) || seq.is_dec_mode(true, 47)) && !self.alt_screen() =>
            {
                self.primary_cache = Some(self.screen_state(true));
            }
            Seq::Osc { body, overflow } => {
                let mut it = body.splitn(2, |b| *b == b';');
                let num = it.next().unwrap_or(&[]);
                let rest = it.next().unwrap_or(&[]);
                match num {
                    b"7501" => {
                        if *overflow {
                            return;
                        }
                        match self.status.apply(rest, raw.len()) {
                            osc7501::Outcome::Query => self.push_effect(Effect::Reply(osc7501::REPLY.to_vec())),
                            osc7501::Outcome::Changed => {
                                self.stop_9_4 = true;
                                self.push_effect(Effect::StatusChanged)
                            }
                            osc7501::Outcome::Ignored => {}
                        }
                        return;
                    }
                    b"7" => {
                        if let Some(p) = parse_osc7(rest) {
                            self.cwd = Some(p.clone());
                            self.push_effect(Effect::Cwd(p));
                        }
                    }
                    b"133" => {
                        if rest.first() == Some(&b'A') && self.status.on_prompt_or_exit() {
                            self.push_effect(Effect::StatusChanged);
                        }
                        if rest.first() == Some(&b'A') {
                            self.push_effect(Effect::Prompt);
                        }
                    }
                    b"9" if rest.starts_with(b"4;") && !self.stop_9_4 => {
                        // OSC 9;4 progress mapped to the root record.
                        let parts: Vec<&[u8]> = rest.split(|b| *b == b';').collect();
                        let st = parts.get(1).copied().unwrap_or(b"0");
                        let pr = parts.get(2).and_then(|p| std::str::from_utf8(p).ok()).unwrap_or("");
                        let body = match st {
                            b"0" => "state=clear".to_string(),
                            b"2" => "state=error".to_string(),
                            _ => format!("state=working:progress={pr}"),
                        };
                        if self.status.apply(body.as_bytes(), body.len()) == osc7501::Outcome::Changed {
                            self.push_effect(Effect::StatusChanged);
                        }
                    }
                    _ => {}
                }
            }
            Seq::Esc { inter, fin: b'c' } if inter.is_empty() => {
                self.scroll_region = None;
                self.primary_cache = None;
                self.stop_9_4 = false;
                if self.status.reset() {
                    self.push_effect(Effect::StatusChanged);
                }
            }
            _ => {}
        }
        let was_alt = self.alt_screen();
        self.parser.advance(&mut self.term, raw);
        if was_alt && !self.alt_screen() {
            self.primary_cache = None;
        }
    }

    pub fn on_child_exit(&mut self) -> bool {
        self.status.on_prompt_or_exit()
    }

    /// Terminal state as bytes, valid at `applied()`. Feeding these bytes to
    /// a fresh emulator of the same size reproduces screen contents (both
    /// screens), history, cursor, saved cursor, modes and title.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = String::new();
        if self.alt_screen() {
            out.push_str(self.primary_cache.as_deref().unwrap_or(""));
            out.push_str("\x1b[?1049h");
            out.push_str(&self.screen_state(false));
        } else {
            out.push_str(&self.screen_state(true));
        }
        self.modes(&mut out);
        out.into_bytes()
    }

    /// Contents, saved cursor and cursor of the active grid.
    fn screen_state(&self, history: bool) -> String {
        let grid = self.term.grid();
        let mut out = String::new();
        out.push_str("\x1b[0m");
        let mut pen = Pen::default();
        write_grid(grid, history, &mut out, &mut pen);
        pen.reset(&mut out);
        // Saved cursor (DECSC slot), with its rendition.
        let sc = &grid.saved_cursor;
        let _ = write!(out, "\x1b[{};{}H", sc.point.line.0 + 1, sc.point.column.0 + 1);
        Pen::from_cell(&sc.template).set(&mut out);
        out.push_str("\x1b7\x1b[0m");
        cursor_bytes(grid, 0, &mut out);
        out
    }

    fn modes(&self, out: &mut String) {
        let m = *self.term.mode();
        let grid = self.term.grid();
        if let Some((t, b)) = self.scroll_region {
            let _ = write!(out, "\x1b[{t};{b}r");
        }
        if self.scroll_region.is_some() || m.contains(TermMode::ORIGIN) {
            // DECSTBM and DECOM home the cursor; position it again.
            let top = if m.contains(TermMode::ORIGIN) {
                out.push_str("\x1b[?6h");
                self.scroll_region.map(|r| r.0 - 1).unwrap_or(0)
            } else {
                0
            };
            cursor_bytes(grid, top, out);
        }
        let set = |out: &mut String, on: bool, n: u32| {
            if on {
                let _ = write!(out, "\x1b[?{n}h");
            }
        };
        set(out, m.contains(TermMode::APP_CURSOR), 1);
        if m.contains(TermMode::APP_KEYPAD) {
            out.push_str("\x1b=");
        }
        set(out, m.contains(TermMode::MOUSE_REPORT_CLICK), 1000);
        set(out, m.contains(TermMode::MOUSE_DRAG), 1002);
        set(out, m.contains(TermMode::MOUSE_MOTION), 1003);
        set(out, m.contains(TermMode::FOCUS_IN_OUT), 1004);
        set(out, m.contains(TermMode::UTF8_MOUSE), 1005);
        set(out, m.contains(TermMode::SGR_MOUSE), 1006);
        set(out, m.contains(TermMode::BRACKETED_PASTE), 2004);
        if !m.contains(TermMode::ALTERNATE_SCROLL) {
            out.push_str("\x1b[?1007l");
        }
        if !m.contains(TermMode::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        if !m.contains(TermMode::LINE_WRAP) {
            out.push_str("\x1b[?7l");
        }
        if m.contains(TermMode::INSERT) {
            out.push_str("\x1b[4h");
        }
        if m.contains(TermMode::LINE_FEED_NEW_LINE) {
            out.push_str("\x1b[20h");
        }
        let kitty = [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ]
        .iter()
        .filter(|(f, _)| m.contains(*f))
        .map(|(_, v)| v)
        .sum::<u32>();
        if kitty != 0 {
            let _ = write!(out, "\x1b[>{kitty}u");
        }
        let style = self.term.cursor_style();
        let n = match (style.shape, style.blinking) {
            (CursorShape::Block, false) => 0,
            (CursorShape::Block, true) => 1,
            (CursorShape::Underline, true) => 3,
            (CursorShape::Underline, false) => 4,
            (CursorShape::Beam, true) => 5,
            (CursorShape::Beam, false) => 6,
            _ => 0,
        };
        if n != 0 {
            let _ = write!(out, "\x1b[{n} q");
        }
        let colors = self.term.colors();
        for i in 0..259usize {
            if let Some(c) = colors[i] {
                let hex = format!("rgb:{:02x}/{:02x}/{:02x}", c.r, c.g, c.b);
                let _ = match i {
                    0..=255 => write!(out, "\x1b]4;{i};{hex}\x1b\\"),
                    256 => write!(out, "\x1b]10;{hex}\x1b\\"),
                    257 => write!(out, "\x1b]11;{hex}\x1b\\"),
                    _ => write!(out, "\x1b]12;{hex}\x1b\\"),
                };
            }
        }
        if let Some(t) = &self.title {
            let clean: String = t.chars().filter(|c| !c.is_control()).collect();
            let _ = write!(out, "\x1b]2;{clean}\x1b\\");
        }
        if let Some(cwd) = &self.cwd {
            let enc: String = cwd.chars().filter(|c| !c.is_control()).collect();
            let _ = write!(out, "\x1b]7;file://localhost{enc}\x1b\\");
        }
        let pen = Pen::from_cell(&grid.cursor.template);
        pen.set(out);
        if let Some((id, uri)) = &pen.link {
            let _ = write!(out, "\x1b]8;id={id};{uri}\x1b\\");
        }
    }

    /// Plain-text capture of visible screen (and optionally history).
    /// Screen text, one line per row; `join` puts soft-wrapped rows back
    /// together into the line the program wrote.
    pub fn capture_text(&self, history: bool, join: bool) -> String {
        let grid = self.term.grid();
        let mut out = String::new();
        for l in line_range(grid, history) {
            let row = &grid[Line(l)];
            let mut s = String::new();
            for c in 0..grid.columns() {
                let cell = &row[Column(c)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push_str(&pen_cell_text(cell));
            }
            if join && row[Column(grid.columns() - 1)].flags.contains(Flags::WRAPLINE) {
                out.push_str(&s);
                continue;
            }
            out.push_str(s.trim_end());
            out.push('\n');
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    pub fn capture_html(&self, history: bool) -> String {
        let grid = self.term.grid();
        let mut out = String::from("<pre class=\"hearth-capture\">");
        for l in line_range(grid, history) {
            let row = &grid[Line(l)];
            let mut c = 0;
            let last = (0..grid.columns()).rev().find(|i| !is_blank(&row[Column(*i)])).map(|i| i + 1).unwrap_or(0);
            while c < last {
                let cell = &row[Column(c)];
                let style = css(cell);
                let mut text = String::new();
                while c < last && css(&row[Column(c)]) == style {
                    let cl = &row[Column(c)];
                    if !cl.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                        text.push_str(&pen_cell_text(cl));
                    }
                    c += 1;
                }
                let esc = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
                if style.is_empty() {
                    out.push_str(&esc);
                } else {
                    let _ = write!(out, "<span style=\"{style}\">{esc}</span>");
                }
            }
            out.push('\n');
        }
        out.push_str("</pre>\n");
        out
    }

    pub fn cursor(&self) -> (u16, u16) {
        let p = self.term.grid().cursor.point;
        (p.line.0 as u16, p.column.0 as u16)
    }

    #[cfg(test)]
    pub fn term(&self) -> &Term<Listener> {
        &self.term
    }
}

/// Positions the cursor (row offset by `top` for origin mode) and recreates
/// a pending wrap by re-printing the last cell.
fn cursor_bytes(grid: &Grid<Cell>, top: u16, out: &mut String) {
    let cur = &grid.cursor;
    let row = (cur.point.line.0 as u16 + 1).saturating_sub(top).max(1);
    let col = cur.point.column.0 as u16 + 1;
    let _ = write!(out, "\x1b[{row};{col}H");
    if cur.input_needs_wrap {
        let line = &grid[cur.point.line];
        let last = cur.point.column.0;
        let cell = &line[Column(last)];
        let (cell, at) = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && last > 0 {
            (&line[Column(last - 1)], last - 1)
        } else {
            (cell, last)
        };
        let _ = write!(out, "\x1b[{};{}H", row, at + 1);
        Pen::from_cell(cell).set(out);
        out.push_str(&pen_cell_text(cell));
        out.push_str("\x1b[0m");
    }
}

fn line_range(grid: &Grid<Cell>, history: bool) -> std::ops::Range<i32> {
    let top = if history { -(grid.history_size() as i32) } else { 0 };
    top..grid.screen_lines() as i32
}

fn parse_osc7(rest: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(rest).ok()?;
    let path = s.strip_prefix("file://")?;
    let path = &path[path.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().filter(|p| !p.chars().any(|c| c.is_control()))
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == Color::Named(NamedColor::Background)
        && cell.fg == Color::Named(NamedColor::Foreground)
        && (cell.flags - Flags::WRAPLINE).is_empty()
        && cell.underline_color().is_none()
        && cell.zerowidth().is_none()
        && cell.hyperlink().is_none()
}

fn pen_cell_text(cell: &Cell) -> String {
    let mut s = String::new();
    s.push(if cell.c == '\0' { ' ' } else { cell.c });
    if let Some(z) = cell.zerowidth() {
        s.extend(z.iter());
    }
    s
}

/// Current SGR + hyperlink state while writing cells.
#[derive(Clone, PartialEq)]
struct Pen {
    fg: Option<Color>,
    bg: Option<Color>,
    ul: Option<Color>,
    flags: Flags,
    link: Option<(String, String)>,
}

fn color_sgr(c: &Color, base: u8, out: &mut String) {
    match c {
        Color::Named(n) => {
            let n = *n as usize;
            match n {
                0..=7 => {
                    let _ = write!(out, ";{}", base as usize + n);
                }
                8..=15 => {
                    let _ = write!(out, ";{}", base as usize + 60 + n - 8);
                }
                _ => {}
            }
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{};5;{}", base + 8, i);
        }
        Color::Spec(Rgb { r, g, b }) => {
            let _ = write!(out, ";{};2;{};{};{}", base + 8, r, g, b);
        }
    }
}

impl Default for Pen {
    fn default() -> Pen {
        Pen { fg: None, bg: None, ul: None, flags: Flags::empty(), link: None }
    }
}

impl Pen {
    fn from_cell(cell: &Cell) -> Pen {
        let fg = (cell.fg != Color::Named(NamedColor::Foreground)).then_some(cell.fg);
        let bg = (cell.bg != Color::Named(NamedColor::Background)).then_some(cell.bg);
        let attrs = Flags::BOLD | Flags::DIM | Flags::ITALIC | Flags::ALL_UNDERLINES | Flags::INVERSE | Flags::HIDDEN | Flags::STRIKEOUT;
        Pen {
            fg,
            bg,
            ul: cell.underline_color(),
            flags: cell.flags & attrs,
            link: cell.hyperlink().map(|h| (h.id().to_string(), h.uri().to_string())),
        }
    }

    fn set(&self, out: &mut String) {
        out.push_str("\x1b[0");
        let f = self.flags;
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if f.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        if let Some(c) = &self.fg {
            color_sgr(c, 30, out);
        }
        if let Some(c) = &self.bg {
            color_sgr(c, 40, out);
        }
        if let Some(c) = &self.ul {
            match c {
                Color::Indexed(i) => {
                    let _ = write!(out, ";58;5;{i}");
                }
                Color::Spec(Rgb { r, g, b }) => {
                    let _ = write!(out, ";58;2;{r};{g};{b}");
                }
                Color::Named(n) if (*n as usize) < 16 => {
                    let _ = write!(out, ";58;5;{}", *n as usize);
                }
                _ => {}
            }
        }
        out.push('m');
    }

    /// Switch the pen to a cell's rendition, writing only on change.
    fn cell(&mut self, cell: &Cell, out: &mut String) {
        let next = Pen::from_cell(cell);
        if next.link != self.link {
            match &next.link {
                Some((id, uri)) => {
                    let _ = write!(out, "\x1b]8;id={id};{uri}\x1b\\");
                }
                None => out.push_str("\x1b]8;;\x1b\\"),
            }
        }
        if next.fg != self.fg || next.bg != self.bg || next.ul != self.ul || next.flags != self.flags {
            next.set(out);
        }
        *self = next;
    }

    fn reset(&mut self, out: &mut String) {
        if self.link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        if *self != Pen::default() {
            out.push_str("\x1b[0m");
        }
        *self = Pen::default();
    }
}

fn write_grid(grid: &Grid<Cell>, history: bool, out: &mut String, pen: &mut Pen) {
    let cols = grid.columns();
    let lines: Vec<i32> = line_range(grid, history).collect();
    for (n, &l) in lines.iter().enumerate() {
        let row = &grid[Line(l)];
        let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wrapped {
            cols
        } else {
            (0..cols).rev().find(|c| !is_blank(&row[Column(*c)])).map(|c| c + 1).unwrap_or(0)
        };
        for c in 0..end {
            let cell = &row[Column(c)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            pen.cell(cell, out);
            out.push_str(&pen_cell_text(cell));
        }
        if n + 1 < lines.len() && !wrapped {
            pen.reset(out);
            out.push_str("\r\n");
        }
    }
}

fn css(cell: &Cell) -> String {
    let mut s = String::new();
    let rgb = |c: &Color| -> Option<Rgb> {
        match c {
            Color::Spec(rgb) => Some(*rgb),
            Color::Indexed(i) => Some(default_color(*i as usize)),
            Color::Named(n) if (*n as usize) < 16 => Some(default_color(*n as usize)),
            Color::Named(_) => None,
        }
    };
    let (mut fg, mut bg) = (rgb(&cell.fg), rgb(&cell.bg));
    if cell.flags.contains(Flags::INVERSE) {
        let f = fg.unwrap_or(default_color(256));
        let b = bg.unwrap_or(default_color(257));
        fg = Some(b);
        bg = Some(f);
    }
    if let Some(c) = fg {
        let _ = write!(s, "color:#{:02x}{:02x}{:02x};", c.r, c.g, c.b);
    }
    if let Some(c) = bg {
        let _ = write!(s, "background:#{:02x}{:02x}{:02x};", c.r, c.g, c.b);
    }
    if cell.flags.contains(Flags::BOLD) {
        s.push_str("font-weight:bold;");
    }
    if cell.flags.contains(Flags::ITALIC) {
        s.push_str("font-style:italic;");
    }
    if cell.flags.intersects(Flags::ALL_UNDERLINES) {
        s.push_str("text-decoration:underline;");
    }
    if cell.flags.contains(Flags::STRIKEOUT) {
        s.push_str("text-decoration:line-through;");
    }
    s
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
