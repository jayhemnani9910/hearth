//! Snapshot fidelity: restore a snapshot into a fresh model, continue both
//! with the same bytes, and require identical emulator state.

use super::*;
use alacritty_terminal::term::cell::Flags;

fn dump(m: &Model) -> String {
    let t = m.term();
    let g = t.grid();
    let mut s = String::new();
    let _ = writeln!(s, "size {}x{} hist {}", g.columns(), g.screen_lines(), g.history_size());
    let _ = writeln!(s, "mode {:?}", *t.mode() & !TermMode::empty());
    let _ = writeln!(s, "cursor {:?} wrap {} tmpl {:?}/{:?}/{:?}", g.cursor.point, g.cursor.input_needs_wrap, g.cursor.template.fg, g.cursor.template.bg, g.cursor.template.flags);
    let _ = writeln!(s, "saved {:?}", g.saved_cursor.point);
    let _ = writeln!(s, "title {:?} cwd {:?}", m.title, m.cwd);
    for l in line_range(g, true) {
        let row = &g[Line(l)];
        let _ = write!(s, "{l:>4}|");
        for c in 0..g.columns() {
            let cell = &row[Column(c)];
            let _ = write!(s, "{}", pen_cell_text(cell));
            if cell.fg != Color::Named(NamedColor::Foreground) || cell.bg != Color::Named(NamedColor::Background) || !(cell.flags - Flags::WRAPLINE).is_empty() {
                let _ = write!(s, "{{{:?},{:?},{:?}}}", cell.fg, cell.bg, cell.flags - Flags::WRAPLINE);
            }
            if let Some(h) = cell.hyperlink() {
                let _ = write!(s, "<{}>", h.uri());
            }
        }
        let _ = writeln!(s, "|{}", row[Column(g.columns() - 1)].flags.contains(Flags::WRAPLINE));
    }
    s
}

fn feed(m: &mut Model, b: &[u8]) {
    m.advance(b);
    m.take_effects();
}

/// Attach at every split point of `prefix`, then continue with `suffix`.
fn check(name: &str, cols: u16, rows: u16, prefix: &[u8], suffix: &[u8], step: usize) {
    let mut i = 0;
    while i <= prefix.len() {
        let mut a = Model::new(cols, rows, 1000);
        feed(&mut a, &prefix[..i]);
        let at = a.applied() as usize;
        let snap = a.snapshot();
        let mut b = Model::new(cols, rows, 1000);
        feed(&mut b, &snap);
        feed(&mut b, &prefix[at..]);
        feed(&mut b, suffix);
        feed(&mut a, &prefix[i..]);
        feed(&mut a, suffix);
        let (da, db) = (dump(&a), dump(&b));
        if da != db {
            panic!("{name}: split {i} (applied {at}) differs\n--- original\n{da}\n--- restored\n{db}\nsnapshot: {:?}", String::from_utf8_lossy(&snap));
        }
        i += step;
    }
}

#[test]
fn plain_colored_history() {
    let mut p = Vec::new();
    for n in 0..40 {
        p.extend(format!("\x1b[3{}mline {n}\x1b[0m tail\r\n", n % 8).as_bytes());
    }
    p.extend(b"\x1b[1;4;38;2;10;20;30;48;5;200mstyled\x1b[0m prompt$ ");
    check("plain", 20, 5, &p, b"more\r\nnext", 7);
}

#[test]
fn wrapping_and_pending_wrap() {
    let p = b"abcdefghijklmnopqrstuvwxyz0123456789\r\n12345678901234567890";
    check("wrap", 20, 4, p, b"X\r\n", 1);
}

#[test]
fn unicode_wide_combining() {
    let p = "日本語テキスト e\u{301} 👍 ok 中文中文中文中文中文\r\n".as_bytes();
    check("unicode", 12, 4, p, "続き".as_bytes(), 1);
}

#[test]
fn alternate_screen_and_back() {
    let mut p = Vec::new();
    p.extend(b"primary 1\r\nprimary 2\r\n$ vim\r\n");
    p.extend(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[7mALT TOP\x1b[0m\x1b[3;5Hmiddle\x1b[?1h\x1b=");
    check("alt", 16, 5, &p, b"\x1b[5;1Hbottom\x1b[?1049l\x1b[?1l\x1b>back$ ", 3);
}

#[test]
fn scroll_region_origin_saved_cursor() {
    let p = b"head\r\n\x1b[2;4r\x1b[2;1Hin region 1\r\nr2\r\nr3\r\nr4 scrolls\x1b[1m\x1b7\x1b[0m\x1b[?6h\x1b[2;3H";
    check("region", 16, 6, p, b"X\r\nY\r\nZ\x1b8!\x1b[r\x1b[?6l\x1b[6;1Hend", 1);
}

#[test]
fn modes_title_cwd_links() {
    let p = b"\x1b]2;my title\x07\x1b]7;file://host/home/u%20x\x07\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?25l\x1b[4h\x1b]8;;https://e.x\x1b\\link\x1b]8;;\x1b\\ text\x1b[5 q";
    check("modes", 20, 4, p, b"\x1b[1;1Hins", 1);
}

#[test]
fn split_inside_utf8_and_sequences_is_exact() {
    let p = "\x1b[38;5;196mr\u{e9}d\x1b]0;t\u{e9}tle\x1b\\ \u{1f600}\x1bP$qm\x1b\\ ok".as_bytes();
    check("splits", 20, 3, p, b" done", 1);
}

#[test]
fn queries_are_answered_once_by_model() {
    let mut m = Model::new(80, 24, 100);
    m.advance(b"\x1b[5;7H\x1b[6n\x1b[c\x1b]7501;?\x1b\\");
    let replies: Vec<Vec<u8>> = m.take_effects().into_iter().filter_map(|e| if let Effect::Reply(r) = e { Some(r) } else { None }).collect();
    assert_eq!(replies[0], b"\x1b[5;7R");
    assert!(replies[1].starts_with(b"\x1b[?"));
    assert_eq!(replies[2], hearth_core::osc7501::REPLY);
}

#[test]
fn osc7501_and_prompt_lifecycle() {
    let mut m = Model::new(80, 24, 100);
    m.advance(b"\x1b]7501;state=working:app=x\x1b\\\x1b]7501;state=done:id=a\x07");
    assert_eq!(m.status.records().len(), 2);
    m.advance(b"\x1b]133;A\x07");
    let r = m.status.records();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].id, "a");
}

#[test]
fn capture_formats() {
    let mut m = Model::new(20, 3, 100);
    m.advance(b"a<b>\r\n\x1b[31mred\x1b[0m");
    assert_eq!(m.capture_text(false), "a<b>\nred\n");
    let h = m.capture_html(false);
    assert!(h.contains("a&lt;b&gt;"));
    assert!(h.contains("color:#cc6666"));
}

