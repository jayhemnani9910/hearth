//! Layout persistence. Saved atomically with a schema version; the previous
//! file is kept as state.prev.json. A crash or reboot restores the layout
//! with NEW shell processes in the saved directories. Running programs are
//! never claimed to survive a server restart.

use crate::sync::Lock;
use crate::server::{Core, Server};
use crate::state::{Node, Session};
use hearth_core::paths;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

pub const SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct SavedBlock {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub schema: u32,
    pub saved_ms: u64,
    pub next_id: u64,
    pub sessions: Vec<Session>,
    pub blocks: HashMap<String, SavedBlock>,
}

pub fn capture(core: &Core) -> Saved {
    let mut blocks = HashMap::new();
    for id in core.s.all_blocks() {
        if let Some(b) = core.blocks.get(&id) {
            let inner = b.inner.lk();
            let cwd = inner.model.cwd.clone().or_else(|| crate::pty::proc_cwd(crate::pty::foreground_pgrp(b.master_fd()).unwrap_or(b.pid)));
            blocks.insert(id, SavedBlock { argv: b.spec.argv.clone(), cwd, title: inner.model.title.clone() });
        }
    }
    Saved { schema: SCHEMA, saved_ms: crate::block::now_ms(), next_id: core.s.next_id, sessions: core.s.sessions.clone(), blocks }
}

pub fn path() -> std::path::PathBuf {
    paths::state_dir().join("state.json")
}

pub fn write(s: &Saved) -> std::io::Result<()> {
    let dir = paths::state_dir();
    paths::ensure_private_dir(&dir)?;
    let tmp = dir.join("state.json.tmp");
    let data = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    let cur = path();
    if cur.exists() {
        let _ = std::fs::copy(&cur, dir.join("state.prev.json"));
    }
    std::fs::rename(&tmp, &cur)?;
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Ok(None): nothing saved. Err: unreadable or from a newer version; the
/// caller must not overwrite it.
pub fn load() -> Result<Option<Saved>, String> {
    let p = path();
    let data = match std::fs::read(&p) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    let v: serde_json::Value = serde_json::from_slice(&data).map_err(|e| format!("{}: corrupt state ({e}); kept as is", p.display()))?;
    let schema = v["schema"].as_u64().unwrap_or(0) as u32;
    if schema > SCHEMA {
        return Err(format!("{} was written by a newer Hearth (schema {schema} > {SCHEMA}); not loading or overwriting it", p.display()));
    }
    serde_json::from_value(v).map(Some).map_err(|e| format!("{}: {e}", p.display()))
}

fn rename_leaves(n: &mut Node, map: &HashMap<String, String>) {
    match n {
        Node::Leaf { block } => {
            if let Some(new) = map.get(block) {
                *block = new.clone();
            }
        }
        Node::Split { a, b, .. } => {
            rename_leaves(a, map);
            rename_leaves(b, map);
        }
    }
}

/// Recreates saved sessions/windows/layouts with new shells. Saved commands
/// are not re-run; their panes get a shell in the same directory.
pub fn restore(server: &Arc<Server>, saved: Saved) -> usize {
    let mut core = server.core.lk();
    core.s.next_id = core.s.next_id.max(saved.next_id);
    let mut count = 0;
    for mut sess in saved.sessions {
        let mut map = HashMap::new();
        let mut ids = vec![];
        for w in &sess.windows {
            w.layout.leaves(&mut ids);
        }
        for old in ids {
            let sb = saved.blocks.get(&old);
            let p = json!({"cwd": sb.and_then(|b| b.cwd.clone())});
            match server.create_block(&mut core, &sess.id, &p) {
                Ok(b) => {
                    core.restored_from.insert(b.id.clone(), old.clone());
                    map.insert(old, b.id.clone());
                    count += 1;
                }
                Err(e) => eprintln!("hearthd: restore {old}: {}", e.message),
            }
        }
        let mut ok = true;
        for w in &mut sess.windows {
            rename_leaves(&mut w.layout, &map);
            if let Some(f) = map.get(&w.focus) {
                w.focus = f.clone();
            }
            w.zoom = w.zoom.as_ref().and_then(|z| map.get(z).cloned());
            let mut l = vec![];
            w.layout.leaves(&mut l);
            ok &= l.iter().all(|b| core.blocks.contains_key(b));
        }
        if ok {
            sess.revision += 1;
            core.s.sessions.push(sess);
        }
    }
    core.dirty = true;
    count
}
