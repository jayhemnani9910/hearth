//! Named keys for `send-keys` (e.g. `Enter`, `C-c`, `M-x`, `Up`, `F5`).

/// Encodes one key name to the bytes a terminal in normal (non-application)
/// cursor mode would send. `app_cursor` selects SS3 arrow sequences.
pub fn encode(name: &str, app_cursor: bool) -> Result<Vec<u8>, String> {
    let (mods, base) = split_mods(name);
    let ctrl = mods.contains(&'C');
    let alt = mods.contains(&'M');
    let mut out = match base {
        "Enter" | "Return" => b"\r".to_vec(),
        "Tab" => b"\t".to_vec(),
        "Space" => b" ".to_vec(),
        "Escape" | "Esc" => b"\x1b".to_vec(),
        "BSpace" | "Backspace" => b"\x7f".to_vec(),
        "Up" | "Down" | "Right" | "Left" => {
            let c = match base { "Up" => 'A', "Down" => 'B', "Right" => 'C', _ => 'D' };
            if ctrl || alt {
                let m = 1 + if alt { 2 } else { 0 } + if ctrl { 4 } else { 0 };
                return Ok(format!("\x1b[1;{m}{c}").into_bytes());
            }
            if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") }.into_bytes()
        }
        "Home" => b"\x1b[H".to_vec(),
        "End" => b"\x1b[F".to_vec(),
        "PageUp" | "PPage" => b"\x1b[5~".to_vec(),
        "PageDown" | "NPage" => b"\x1b[6~".to_vec(),
        "Insert" => b"\x1b[2~".to_vec(),
        "Delete" | "DC" => b"\x1b[3~".to_vec(),
        f if f.len() >= 2 && f.starts_with('F') && f[1..].parse::<u8>().is_ok() => {
            let n: u8 = f[1..].parse().unwrap();
            let s = match n {
                1 => "\x1bOP", 2 => "\x1bOQ", 3 => "\x1bOR", 4 => "\x1bOS",
                5 => "\x1b[15~", 6 => "\x1b[17~", 7 => "\x1b[18~", 8 => "\x1b[19~",
                9 => "\x1b[20~", 10 => "\x1b[21~", 11 => "\x1b[23~", 12 => "\x1b[24~",
                _ => return Err(format!("unknown key {name}")),
            };
            s.as_bytes().to_vec()
        }
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            if ctrl {
                let lc = c.to_ascii_lowercase();
                let b = match lc {
                    'a'..='z' => lc as u8 - b'a' + 1,
                    '@' | ' ' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '7' | '/' => 0x1f,
                    '?' | '8' => 0x7f,
                    _ => return Err(format!("no control code for {name}")),
                };
                vec![b]
            } else {
                s.as_bytes().to_vec()
            }
        }
        _ => return Err(format!("unknown key '{name}'")),
    };
    if alt {
        out.insert(0, 0x1b);
    }
    Ok(out)
}

fn split_mods(name: &str) -> (Vec<char>, &str) {
    let mut mods = vec![];
    let mut rest = name;
    while rest.len() > 2 && rest.as_bytes()[1] == b'-' && matches!(rest.as_bytes()[0], b'C' | b'M' | b'S') {
        mods.push(rest.as_bytes()[0] as char);
        rest = &rest[2..];
    }
    (mods, rest)
}

/// A parsed key chord such as `ctrl+shift+t`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Chord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    #[serde(rename = "super")]
    pub sup: bool,
    /// Lowercase key name: a single character or a named key.
    pub key: String,
}

const NAMED: &[&str] = &[
    "return", "enter", "tab", "escape", "space", "backspace", "delete", "insert", "home", "end",
    "pageup", "pagedown", "left", "right", "up", "down", "plus", "minus", "comma", "period",
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
];

/// Parses one chord. Errors name the bad part.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let parts: Vec<&str> = s.split('+').collect();
    let (key, mods) = parts.split_last().ok_or("empty chord")?;
    let mut c = Chord { ctrl: false, shift: false, alt: false, sup: false, key: key.to_lowercase() };
    for m in mods {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => c.ctrl = true,
            "shift" => c.shift = true,
            "alt" => c.alt = true,
            "super" | "meta" => c.sup = true,
            other => return Err(format!("unknown modifier '{other}' in '{s}'")),
        }
    }
    if c.key.is_empty() || !(c.key.chars().count() == 1 || NAMED.contains(&c.key.as_str())) {
        return Err(format!("unknown key '{key}' in '{s}'"));
    }
    if c.key == "enter" {
        c.key = "return".into();
    }
    Ok(c)
}

/// Parses a space-separated chord sequence (prefix chords then a final one).
pub fn parse_sequence(s: &str) -> Result<Vec<Chord>, String> {
    let v: Result<Vec<Chord>, String> = s.split_whitespace().map(parse_chord).collect();
    match v {
        Ok(v) if v.is_empty() => Err("empty key sequence".into()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{encode, parse_sequence};

    #[test]
    fn chords() {
        let s = parse_sequence("ctrl+a shift+c").unwrap();
        assert!(s[0].ctrl && s[0].key == "a" && s[1].shift);
        assert!(parse_sequence("hyper+x").is_err());
        assert!(parse_sequence("ctrl+notakey").is_err());
    }
    #[test]
    fn keys() {
        assert_eq!(encode("C-c", false).unwrap(), b"\x03");
        assert_eq!(encode("Enter", false).unwrap(), b"\r");
        assert_eq!(encode("M-x", false).unwrap(), b"\x1bx");
        assert_eq!(encode("Up", true).unwrap(), b"\x1bOA");
        assert_eq!(encode("C-Left", false).unwrap(), b"\x1b[1;5D");
        assert!(encode("Nope", false).is_err());
    }
}
