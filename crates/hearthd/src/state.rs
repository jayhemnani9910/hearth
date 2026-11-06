//! Shared structure: sessions, windows (tabs), split layouts and the blocks
//! they reference. Blocks own processes; layouts only reference them.

use hearth_core::conn::ApiError;
use hearth_core::proto::code;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Children side by side (a | b).
    Horizontal,
    /// Children stacked (a over b).
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn parse(s: &str) -> Result<Dir, ApiError> {
        Ok(match s {
            "left" => Dir::Left,
            "right" => Dir::Right,
            "up" => Dir::Up,
            "down" => Dir::Down,
            _ => return Err(ApiError::new(code::INVALID, format!("direction must be left/right/up/down, got '{s}'"))),
        })
    }
    fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        }
    }
    /// True when the new/target item goes after (right/below).
    fn after(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Node {
    Leaf { block: String },
    Split { axis: Axis, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    pub fn leaves(&self, out: &mut Vec<String>) {
        match self {
            Node::Leaf { block } => out.push(block.clone()),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        match self {
            Node::Leaf { block } => block == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    fn count(&self) -> usize {
        match self {
            Node::Leaf { .. } => 1,
            Node::Split { a, b, .. } => a.count() + b.count(),
        }
    }

    /// Replace leaf `id` with a split holding it and `new`.
    fn split_leaf(&mut self, id: &str, new: &str, dir: Dir, ratio: f32) -> bool {
        match self {
            Node::Leaf { block } if block == id => {
                let old = Node::Leaf { block: id.to_string() };
                let new = Node::Leaf { block: new.to_string() };
                let (a, b) = if dir.after() { (old, new) } else { (new, old) };
                let ratio = if dir.after() { ratio } else { 1.0 - ratio };
                *self = Node::Split { axis: dir.axis(), ratio, a: Box::new(a), b: Box::new(b) };
                true
            }
            Node::Leaf { .. } => false,
            Node::Split { a, b, .. } => a.split_leaf(id, new, dir, ratio) || b.split_leaf(id, new, dir, ratio),
        }
    }

    /// Remove leaf `id`; returns the remaining tree (None if it was the only leaf).
    fn remove(self, id: &str) -> Option<Node> {
        match self {
            Node::Leaf { ref block } if block == id => None,
            Node::Leaf { .. } => Some(self),
            Node::Split { axis, ratio, a, b } => match (a.remove(id), b.remove(id)) {
                (Some(a), Some(b)) => Some(Node::Split { axis, ratio, a: Box::new(a), b: Box::new(b) }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            },
        }
    }

    fn rename_leaf(&mut self, from: &str, to: &str) {
        match self {
            Node::Leaf { block } if block == from => *block = to.to_string(),
            Node::Leaf { .. } => {}
            Node::Split { a, b, .. } => {
                a.rename_leaf(from, to);
                b.rename_leaf(from, to);
            }
        }
    }

    /// Moves the divider nearest to `id` in `dir` by `delta`.
    fn grow(&mut self, id: &str, dir: Dir, delta: f32) -> bool {
        let Node::Split { axis, ratio, a, b } = self else { return false };
        let in_a = a.contains(id);
        let in_b = !in_a && b.contains(id);
        if !in_a && !in_b {
            return false;
        }
        // Deeper splits are nearer to the block.
        if (in_a && a.grow(id, dir, delta)) || (in_b && b.grow(id, dir, delta)) {
            return true;
        }
        if *axis != dir.axis() {
            return false;
        }
        match (in_a, dir.after()) {
            (true, true) => *ratio = (*ratio + delta).clamp(0.05, 0.95),
            (false, false) => *ratio = (*ratio - delta).clamp(0.05, 0.95),
            _ => return false,
        }
        true
    }

    fn balance(&mut self) {
        if let Node::Split { ratio, a, b, .. } = self {
            a.balance();
            b.balance();
            *ratio = a.count() as f32 / (a.count() + b.count()) as f32;
        }
    }

    fn at_path(&mut self, path: &str) -> Option<&mut Node> {
        let mut cur = self;
        for c in path.chars() {
            cur = match cur {
                Node::Split { a, b, .. } => {
                    if c == 'a' {
                        a
                    } else if c == 'b' {
                        b
                    } else {
                        return None;
                    }
                }
                Node::Leaf { .. } => return None,
            };
        }
        Some(cur)
    }

    /// Leaf rectangles in the unit square: (block, x, y, w, h).
    pub fn rects(&self, x: f32, y: f32, w: f32, h: f32, out: &mut Vec<(String, f32, f32, f32, f32)>) {
        match self {
            Node::Leaf { block } => out.push((block.clone(), x, y, w, h)),
            Node::Split { axis: Axis::Horizontal, ratio, a, b } => {
                a.rects(x, y, w * ratio, h, out);
                b.rects(x + w * ratio, y, w * (1.0 - ratio), h, out);
            }
            Node::Split { axis: Axis::Vertical, ratio, a, b } => {
                a.rects(x, y, w, h * ratio, out);
                b.rects(x, y + h * ratio, w, h * (1.0 - ratio), out);
            }
        }
    }
}

/// Neighbor of `from` in `dir` by geometry, if any.
pub fn neighbor(layout: &Node, from: &str, dir: Dir) -> Option<String> {
    let mut rects = Vec::new();
    layout.rects(0.0, 0.0, 1.0, 1.0, &mut rects);
    let (_, fx, fy, fw, fh) = rects.iter().find(|r| r.0 == from)?.clone();
    let (cx, cy) = (fx + fw / 2.0, fy + fh / 2.0);
    let eps = 1e-4;
    rects
        .iter()
        .filter(|r| r.0 != from)
        .filter(|(_, x, y, w, h)| match dir {
            Dir::Left => x + w <= fx + eps && *y < fy + fh - eps && y + h > fy + eps,
            Dir::Right => *x >= fx + fw - eps && *y < fy + fh - eps && y + h > fy + eps,
            Dir::Up => y + h <= fy + eps && *x < fx + fw - eps && x + w > fx + eps,
            Dir::Down => *y >= fy + fh - eps && *x < fx + fw - eps && x + w > fx + eps,
        })
        .min_by(|a, b| {
            let d = |r: &(String, f32, f32, f32, f32)| {
                let (rx, ry) = (r.1 + r.3 / 2.0, r.2 + r.4 / 2.0);
                (rx - cx).abs() + (ry - cy).abs()
            };
            d(a).partial_cmp(&d(b)).unwrap()
        })
        .map(|r| r.0.clone())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub name: String,
    pub layout: Node,
    pub focus: String,
    #[serde(default)]
    pub zoom: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub windows: Vec<Window>,
    pub active_window: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub created_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Structure {
    pub sessions: Vec<Session>,
    pub next_id: u64,
}

fn nf(what: &str, id: &str) -> ApiError {
    ApiError::new(code::NOT_FOUND, format!("no {what} '{id}'"))
}

impl Structure {
    pub fn alloc(&mut self, prefix: char) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    pub fn session(&self, id: &str) -> Result<&Session, ApiError> {
        self.sessions.iter().find(|s| s.id == id).ok_or_else(|| nf("session", id))
    }

    pub fn session_mut(&mut self, id: &str) -> Result<&mut Session, ApiError> {
        self.sessions.iter_mut().find(|s| s.id == id).ok_or_else(|| nf("session", id))
    }

    /// Resolves a session by id or unique name.
    pub fn resolve_session(&self, key: &str) -> Result<String, ApiError> {
        if self.sessions.iter().any(|s| s.id == key) {
            return Ok(key.to_string());
        }
        let m: Vec<&Session> = self.sessions.iter().filter(|s| s.name == key).collect();
        match m.len() {
            0 => Err(nf("session", key)),
            1 => Ok(m[0].id.clone()),
            _ => Err(ApiError::new(code::AMBIGUOUS, format!("{} sessions are named '{key}'; use an id", m.len()))),
        }
    }

    pub fn resolve_window(&self, key: &str) -> Result<(String, String), ApiError> {
        let mut hits = vec![];
        for s in &self.sessions {
            for w in &s.windows {
                if w.id == key {
                    return Ok((s.id.clone(), w.id.clone()));
                }
                if w.name == key {
                    hits.push((s.id.clone(), w.id.clone()));
                }
            }
        }
        match hits.len() {
            0 => Err(nf("window", key)),
            1 => Ok(hits.remove(0)),
            _ => Err(ApiError::new(code::AMBIGUOUS, format!("{} windows are named '{key}'; use an id", hits.len()))),
        }
    }

    /// (session, window) containing a block.
    pub fn locate(&self, block: &str) -> Option<(String, String)> {
        for s in &self.sessions {
            for w in &s.windows {
                if w.layout.contains(block) {
                    return Some((s.id.clone(), w.id.clone()));
                }
            }
        }
        None
    }

    pub fn window_mut(&mut self, sid: &str, wid: &str) -> Result<&mut Window, ApiError> {
        self.session_mut(sid)?.windows.iter_mut().find(|w| w.id == wid).ok_or_else(|| nf("window", wid))
    }

    fn bump(&mut self, sid: &str) {
        if let Ok(s) = self.session_mut(sid) {
            s.revision += 1;
        }
    }

    /// `sid` comes from `alloc('s')` so the first shell can know it.
    pub fn new_session(&mut self, sid: String, name: Option<String>, block: &str, now: u64) -> (String, String) {
        let wid = self.alloc('w');
        let name = name.unwrap_or_else(|| format!("session {}", self.sessions.len() + 1));
        self.sessions.push(Session {
            id: sid.clone(),
            name,
            windows: vec![Window { id: wid.clone(), name: String::new(), layout: Node::Leaf { block: block.to_string() }, focus: block.to_string(), zoom: None }],
            active_window: wid.clone(),
            revision: 1,
            created_ms: now,
        });
        (sid, wid)
    }

    pub fn new_window(&mut self, sid: &str, name: Option<String>, block: &str, index: Option<usize>) -> Result<String, ApiError> {
        let wid = self.alloc('w');
        let s = self.session_mut(sid)?;
        let w = Window { id: wid.clone(), name: name.unwrap_or_default(), layout: Node::Leaf { block: block.to_string() }, focus: block.to_string(), zoom: None };
        let at = index.unwrap_or(s.windows.len()).min(s.windows.len());
        s.windows.insert(at, w);
        s.active_window = wid.clone();
        s.revision += 1;
        Ok(wid)
    }

    pub fn split(&mut self, target: &str, new: &str, dir: Dir, ratio: f32) -> Result<(String, String), ApiError> {
        let (sid, wid) = self.locate(target).ok_or_else(|| nf("block", target))?;
        let w = self.window_mut(&sid, &wid)?;
        w.layout.split_leaf(target, new, dir, ratio.clamp(0.05, 0.95));
        w.focus = new.to_string();
        w.zoom = None;
        self.bump(&sid);
        Ok((sid, wid))
    }

    /// Removes a block from the layout, closing empty windows/sessions.
    /// Returns (session, window, window_closed, session_closed).
    pub fn remove_block(&mut self, block: &str) -> Option<(String, String, bool, bool)> {
        let (sid, wid) = self.locate(block)?;
        let s = self.session_mut(&sid).ok()?;
        let wi = s.windows.iter().position(|w| w.id == wid)?;
        let w = &mut s.windows[wi];
        let layout = std::mem::replace(&mut w.layout, Node::Leaf { block: String::new() });
        let mut window_closed = false;
        match layout.remove(block) {
            Some(rest) => {
                w.layout = rest;
                if w.focus == block {
                    let mut l = vec![];
                    w.layout.leaves(&mut l);
                    w.focus = l.first().cloned().unwrap_or_default();
                }
                if w.zoom.as_deref() == Some(block) {
                    w.zoom = None;
                }
            }
            None => {
                s.windows.remove(wi);
                window_closed = true;
                if s.active_window == wid {
                    s.active_window = s.windows.get(wi.min(s.windows.len().saturating_sub(1))).map(|w| w.id.clone()).unwrap_or_default();
                }
            }
        }
        s.revision += 1;
        let session_closed = s.windows.is_empty();
        if session_closed {
            self.sessions.retain(|x| x.id != sid);
        }
        Some((sid, wid, window_closed, session_closed))
    }

    pub fn grow(&mut self, block: &str, dir: Dir, delta: f32) -> Result<String, ApiError> {
        let (sid, wid) = self.locate(block).ok_or_else(|| nf("block", block))?;
        let w = self.window_mut(&sid, &wid)?;
        if !w.layout.grow(block, dir, delta) {
            return Err(ApiError::new(code::INVALID, "no divider in that direction"));
        }
        self.bump(&sid);
        Ok(sid)
    }

    pub fn set_ratio(&mut self, sid: &str, wid: &str, path: &str, r: f32) -> Result<(), ApiError> {
        let w = self.window_mut(sid, wid)?;
        match w.layout.at_path(path) {
            Some(Node::Split { ratio, .. }) => *ratio = r.clamp(0.05, 0.95),
            _ => return Err(ApiError::new(code::INVALID, format!("no split at path '{path}'"))),
        }
        self.bump(sid);
        Ok(())
    }

    pub fn balance(&mut self, sid: &str, wid: &str) -> Result<(), ApiError> {
        self.window_mut(sid, wid)?.layout.balance();
        self.bump(sid);
        Ok(())
    }

    pub fn swap(&mut self, x: &str, y: &str) -> Result<Vec<String>, ApiError> {
        let (sx, wx) = self.locate(x).ok_or_else(|| nf("block", x))?;
        let (sy, wy) = self.locate(y).ok_or_else(|| nf("block", y))?;
        if sx != sy {
            return Err(ApiError::new(code::INVALID, "blocks are in different sessions"));
        }
        let tmp = "\u{0}swap";
        self.window_mut(&sx, &wx)?.layout.rename_leaf(x, tmp);
        self.window_mut(&sy, &wy)?.layout.rename_leaf(y, x);
        self.window_mut(&sx, &wx)?.layout.rename_leaf(tmp, y);
        self.bump(&sx);
        Ok(vec![sx])
    }

    /// Moves `block` next to `target` (possibly in another window).
    pub fn move_block(&mut self, block: &str, target: &str, dir: Dir) -> Result<Vec<String>, ApiError> {
        if block == target {
            return Err(ApiError::new(code::INVALID, "cannot move a block next to itself"));
        }
        let (sb, _) = self.locate(block).ok_or_else(|| nf("block", block))?;
        let (st, _) = self.locate(target).ok_or_else(|| nf("block", target))?;
        if sb != st {
            return Err(ApiError::new(code::INVALID, "blocks are in different sessions"));
        }
        // Refuse to remove the last block of the session.
        let total: usize = self.session(&sb)?.windows.iter().map(|w| w.layout.count()).sum();
        if total < 2 {
            return Err(ApiError::new(code::INVALID, "nothing to move"));
        }
        self.remove_block(block);
        let (s2, w2) = self.locate(target).ok_or_else(|| nf("block", target))?;
        let w = self.window_mut(&s2, &w2)?;
        w.layout.split_leaf(target, block, dir, 0.5);
        w.focus = block.to_string();
        self.bump(&s2);
        Ok(vec![s2])
    }

    pub fn move_window(&mut self, wid: &str, index: usize) -> Result<String, ApiError> {
        let (sid, _) = self.resolve_window(wid)?;
        let s = self.session_mut(&sid)?;
        let from = s.windows.iter().position(|w| w.id == wid).ok_or_else(|| nf("window", wid))?;
        let w = s.windows.remove(from);
        let to = index.min(s.windows.len());
        s.windows.insert(to, w);
        s.revision += 1;
        Ok(sid)
    }

    /// All block ids referenced by layouts.
    pub fn all_blocks(&self) -> Vec<String> {
        let mut v = vec![];
        for s in &self.sessions {
            for w in &s.windows {
                w.layout.leaves(&mut v);
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(s: &Structure) -> Vec<String> {
        s.all_blocks()
    }

    #[test]
    fn split_remove_and_close() {
        let mut st = Structure::default();
        let sid = st.alloc('s');
        let (sid, wid) = st.new_session(sid, Some("p".into()), "b1", 0);
        st.split("b1", "b2", Dir::Right, 0.5).unwrap();
        st.split("b2", "b3", Dir::Down, 0.5).unwrap();
        assert_eq!(leaves(&st), ["b1", "b2", "b3"]);
        assert_eq!(neighbor(&st.session(&sid).unwrap().windows[0].layout, "b1", Dir::Right).as_deref(), Some("b2"));
        assert_eq!(neighbor(&st.session(&sid).unwrap().windows[0].layout, "b3", Dir::Up).as_deref(), Some("b2"));
        st.grow("b1", Dir::Right, 0.1).unwrap();
        st.balance(&sid, &wid).unwrap();
        st.swap("b1", "b3").unwrap();
        assert_eq!(leaves(&st), ["b3", "b2", "b1"]);
        st.move_block("b3", "b1", Dir::Down).unwrap();
        assert_eq!(leaves(&st), ["b2", "b1", "b3"]);
        let r = st.remove_block("b2").unwrap();
        assert!(!r.2);
        st.remove_block("b1");
        let r = st.remove_block("b3").unwrap();
        assert!(r.2 && r.3);
        assert!(st.sessions.is_empty());
    }

    #[test]
    fn ids_are_never_reused_and_names_are_checked() {
        let mut st = Structure::default();
        let a = st.alloc('s');
        st.new_session(a, Some("x".into()), "b1", 0);
        let b = st.alloc('s');
        st.new_session(b, Some("x".into()), "b2", 0);
        assert_eq!(st.resolve_session("x").unwrap_err().code, code::AMBIGUOUS);
        let a = st.alloc('b');
        let b = st.alloc('b');
        assert_ne!(a, b);
    }
}
