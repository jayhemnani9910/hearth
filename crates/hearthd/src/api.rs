//! Typed API registry (self-description) and method dispatch. The GUI, CLI
//! and Lua all go through `Server::call`.

use crate::block::Block;
use crate::server::{Core, Ctx, Server, session_view};
use crate::state::{Dir, neighbor};
use hearth_core::conn::ApiError;
use hearth_core::proto::code;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub struct Method {
    pub name: &'static str,
    pub summary: &'static str,
    pub params: Value,
    pub result: Value,
    pub example: Value,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

const S: &str = "string";

fn t(ty: &str, d: &str) -> Value {
    json!({"type": ty, "description": d})
}

fn spawn_props() -> serde_json::Map<String, Value> {
    let v = json!({
        "command": {"type": "array", "items": {"type": "string"}, "description": "argv; omit for the user's shell. One element is a shell command line when shell is login/interactive"},
        "shell": {"type": "string", "enum": ["none", "login", "interactive"], "description": "none: exec argv directly (default with command); login/interactive: run through $SHELL"},
        "cwd": t(S, "working directory"),
        "env": {"type": "object", "description": "extra environment variables"},
        "cols": t("integer", "initial columns"),
        "rows": t("integer", "initial rows"),
        "close_on_exit": t("boolean", "remove the block when its process exits (default: true for shells, false for commands)"),
    });
    v.as_object().unwrap().clone()
}

fn with_spawn(mut extra: serde_json::Map<String, Value>) -> Value {
    extra.extend(spawn_props());
    Value::Object(extra)
}

fn props(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

pub fn methods() -> Vec<Method> {
    let ids = json!({"type": "object", "properties": {"session": t(S, ""), "window": t(S, ""), "block": t(S, "")}});
    let ok = json!({"type": "object", "properties": {"ok": t("boolean", "")}, "required": ["ok"]});
    let view = json!({"type": "object", "properties": {"id": t(S, ""), "name": t(S, ""), "revision": t("integer", ""), "windows": t("array", ""), "blocks": t("object", "")}, "required": ["id", "name", "revision", "windows", "blocks"]});
    vec![
        Method { name: "server.version", summary: "Server and protocol version", params: obj(json!({}), &[]), result: obj(json!({"version": t(S, ""), "protocol": t(S, ""), "incarnation": t(S, "")}), &["version", "protocol", "incarnation"]), example: json!({}) },
        Method { name: "server.status", summary: "Server process, listeners, clients and config state", params: obj(json!({}), &[]), result: obj(json!({"pid": t("integer", ""), "sessions": t("integer", ""), "blocks": t("integer", ""), "clients": t("array", "")}), &["pid", "sessions", "blocks", "clients"]), example: json!({}) },
        Method { name: "server.stop", summary: "Stop the server. Refuses while blocks run unless force is true (their processes are terminated; the layout is kept for the next start)", params: obj(json!({"force": t("boolean", "")}), &[]), result: ok.clone(), example: json!(null) },
        Method { name: "api.list", summary: "List API methods", params: obj(json!({}), &[]), result: t("array", "method names and summaries"), example: json!({}) },
        Method { name: "api.describe", summary: "Describe one method: params/result schemas and an example", params: obj(json!({"method": t(S, "")}), &["method"]), result: obj(json!({"name": t(S, ""), "params": t("object", ""), "result": t("object", "")}), &["name", "params", "result"]), example: json!({"method": "session.list"}) },
        Method { name: "session.list", summary: "List sessions", params: obj(json!({}), &[]), result: t("array", "sessions"), example: json!({}) },
        Method { name: "session.bootstrap", summary: "Full view of every session, optionally subscribing to events atomically; events after `seq` follow", params: obj(json!({"subscribe": t("boolean", "default true")}), &[]), result: obj(json!({"seq": t("integer", "event sequence at the snapshot"), "sessions": t("array", "")}), &["seq", "sessions"]), example: json!({"subscribe": false}) },
        Method { name: "session.view", summary: "Full view of one session", params: obj(json!({"session": t(S, "id or unique name")}), &["session"]), result: view.clone(), example: json!(null) },
        Method { name: "session.create", summary: "Create a session with one window and one terminal", params: obj(with_spawn(props(json!({"name": t(S, "")}))), &[]), result: ids.clone(), example: json!(null) },
        Method { name: "session.rename", summary: "Rename a session", params: obj(json!({"session": t(S, ""), "name": t(S, "")}), &["session", "name"]), result: ok.clone(), example: json!(null) },
        Method { name: "session.close", summary: "Terminate a session's processes and remove it", params: obj(json!({"session": t(S, "")}), &["session"]), result: ok.clone(), example: json!(null) },
        Method { name: "window.create", summary: "Create a window (tab) in a session", params: obj(with_spawn(props(json!({"session": t(S, ""), "name": t(S, ""), "index": t("integer", "")}))), &["session"]), result: ids.clone(), example: json!(null) },
        Method { name: "window.rename", summary: "Rename a window", params: obj(json!({"window": t(S, ""), "name": t(S, "")}), &["window", "name"]), result: ok.clone(), example: json!(null) },
        Method { name: "window.close", summary: "Terminate a window's processes and remove it", params: obj(json!({"window": t(S, "")}), &["window"]), result: ok.clone(), example: json!(null) },
        Method { name: "window.move", summary: "Reorder a window within its session", params: obj(json!({"window": t(S, ""), "index": t("integer", "")}), &["window", "index"]), result: ok.clone(), example: json!(null) },
        Method { name: "window.select", summary: "Set the session's shared active window (does not move other clients' views)", params: obj(json!({"window": t(S, "")}), &["window"]), result: ok.clone(), example: json!(null) },
        Method { name: "block.split", summary: "Split a block, starting a new terminal beside it", params: obj(with_spawn(props(json!({"block": t(S, ""), "direction": {"type": "string", "enum": ["left", "right", "up", "down"]}, "ratio": t("number", "share kept by the existing block")}))), &["block", "direction"]), result: ids.clone(), example: json!(null) },
        Method { name: "block.close", summary: "Terminate a block's process group (SIGHUP, then SIGKILL after 3s) and remove it", params: obj(json!({"block": t(S, "")}), &["block"]), result: ok.clone(), example: json!(null) },
        Method { name: "block.focus", summary: "Set the shared focus of a window to a block, or to its neighbor in a direction", params: obj(json!({"block": t(S, ""), "direction": t(S, "optional left/right/up/down")}), &["block"]), result: ids.clone(), example: json!(null) },
        Method { name: "block.zoom", summary: "Zoom or unzoom a block in its window", params: obj(json!({"block": t(S, ""), "zoom": t("boolean", "omit to toggle")}), &["block"]), result: obj(json!({"zoom": t("boolean", "")}), &["zoom"]), example: json!(null) },
        Method { name: "block.resize", summary: "Move the nearest divider in a direction", params: obj(json!({"block": t(S, ""), "direction": t(S, ""), "amount": t("number", "fraction, default 0.05")}), &["block", "direction"]), result: ok.clone(), example: json!(null) },
        Method { name: "block.swap", summary: "Swap two blocks' positions", params: obj(json!({"block": t(S, ""), "other": t(S, "")}), &["block", "other"]), result: ok.clone(), example: json!(null) },
        Method { name: "block.move", summary: "Move a block beside another block", params: obj(json!({"block": t(S, ""), "target": t(S, ""), "direction": t(S, "")}), &["block", "target", "direction"]), result: ok.clone(), example: json!(null) },
        Method { name: "block.info", summary: "Process, size, status and ownership of a block", params: obj(json!({"block": t(S, "")}), &["block"]), result: obj(json!({"id": t(S, ""), "pid": t("integer", ""), "cols": t("integer", ""), "rows": t("integer", "")}), &["id", "pid", "cols", "rows"]), example: json!(null) },
        Method { name: "block.own", summary: "Size ownership: take, request (only if unowned) or release", params: obj(json!({"block": t(S, ""), "mode": {"type": "string", "enum": ["take", "request", "release"]}}), &["block", "mode"]), result: obj(json!({"owner": t(S, "")}), &[]), example: json!(null) },
        Method { name: "layout.set_ratio", summary: "Set a divider ratio; path is a/b steps from the window root", params: obj(json!({"window": t(S, ""), "path": t(S, ""), "ratio": t("number", "")}), &["window", "path", "ratio"]), result: ok.clone(), example: json!(null) },
        Method { name: "layout.balance", summary: "Give every pane in a window equal space", params: obj(json!({"window": t(S, "")}), &["window"]), result: ok.clone(), example: json!(null) },
        Method { name: "terminal.run", summary: "Run a program in a new window, new session or split", params: obj(with_spawn(props(json!({"placement": {"type": "string", "enum": ["session", "window", "left", "right", "up", "down"]}, "target": t(S, "session (for window) or block (for splits)"), "name": t(S, "")}))), &["placement"]), result: ids.clone(), example: json!(null) },
        Method { name: "terminal.send_text", summary: "Write text to a terminal; paste=true wraps it in bracketed paste when the program enabled it", params: obj(json!({"block": t(S, ""), "text": t(S, ""), "paste": t("boolean", "")}), &["block", "text"]), result: ok.clone(), example: json!(null) },
        Method { name: "terminal.send_keys", summary: "Send named keys (Enter, C-c, M-x, Up, F5, …)", params: obj(json!({"block": t(S, ""), "keys": {"type": "array", "items": {"type": "string"}}}), &["block", "keys"]), result: ok.clone(), example: json!(null) },
        Method { name: "terminal.capture", summary: "Capture terminal content as text, html or vt (restorable state)", params: obj(json!({"block": t(S, ""), "format": {"type": "string", "enum": ["text", "html", "vt"]}, "history": t("boolean", "include scrollback (bounded)")}), &["block"]), result: obj(json!({"format": t(S, ""), "content": t(S, ""), "cols": t("integer", ""), "rows": t("integer", ""), "cursor": t("array", "")}), &["format", "content"]), example: json!(null) },
        Method { name: "terminal.wait", summary: "Wait until the block's child process exits. For a shell this means the shell exited", params: obj(json!({"block": t(S, ""), "timeout_ms": t("integer", "")}), &["block"]), result: obj(json!({"code": t("integer", ""), "signal": t("integer", "")}), &[]), example: json!(null) },
        Method { name: "terminal.wait_text", summary: "Wait until text appears on screen (polling capture)", params: obj(json!({"block": t(S, ""), "text": t(S, ""), "timeout_ms": t("integer", "")}), &["block", "text"]), result: ok.clone(), example: json!(null) },
        Method { name: "terminal.attach", summary: "Subscribe to a terminal: a snapshot (or resumed bytes) then live output arrive as data frames", params: obj(json!({"block": t(S, ""), "cols": t("integer", ""), "rows": t("integer", ""), "cell_w": t("integer", ""), "cell_h": t("integer", ""), "resume": {"type": "object", "properties": {"generation": t(S, ""), "offset": t("integer", "")}}}), &["block"]), result: obj(json!({"sub": t("integer", ""), "mode": t(S, "snapshot or resume"), "offset": t("integer", ""), "generation": t(S, "")}), &["sub", "mode", "offset", "generation"]), example: json!(null) },
        Method { name: "terminal.detach", summary: "End a terminal subscription (the process keeps running)", params: obj(json!({"sub": t("integer", "")}), &["sub"]), result: ok.clone(), example: json!(null) },
        Method { name: "terminal.resize", summary: "Report this viewer's preferred size; applied when it owns the block", params: obj(json!({"sub": t("integer", ""), "cols": t("integer", ""), "rows": t("integer", ""), "cell_w": t("integer", ""), "cell_h": t("integer", "")}), &["sub", "cols", "rows"]), result: obj(json!({"applied": t("boolean", ""), "owner": t(S, "")}), &["applied"]), example: json!(null) },
        Method { name: "terminal.set_size", summary: "Force a block's PTY size (automation; viewers follow)", params: obj(json!({"block": t(S, ""), "cols": t("integer", ""), "rows": t("integer", "")}), &["block", "cols", "rows"]), result: ok.clone(), example: json!(null) },
        Method { name: "terminal.status", summary: "OSC 7501 program status records of a block", params: obj(json!({"block": t(S, "")}), &["block"]), result: obj(json!({"records": t("array", ""), "summary": t(S, "")}), &["records"]), example: json!(null) },
        Method { name: "events.subscribe", summary: "Receive server events on this connection", params: obj(json!({}), &[]), result: obj(json!({"seq": t("integer", "")}), &["seq"]), example: json!(null) },
        Method { name: "events.unsubscribe", summary: "Stop receiving events", params: obj(json!({}), &[]), result: ok.clone(), example: json!(null) },
        Method { name: "client.list", summary: "Connected clients", params: obj(json!({}), &[]), result: t("array", ""), example: json!({}) },
        Method { name: "client.set", summary: "Set this client's flags (remote_control allows client.action to drive it)", params: obj(json!({"remote_control": t("boolean", "")}), &[]), result: ok.clone(), example: json!(null) },
        Method { name: "client.action", summary: "Queue a GUI action for connected apps with remote control enabled", params: obj(json!({"action": t(S, ""), "args": t("object", ""), "client": t(S, "")}), &["action"]), result: obj(json!({"delivered": t("integer", "")}), &["delivered"]), example: json!(null) },
        Method { name: "config.get", summary: "Loaded configuration: bindings, modes, actions, hosts, settings, diagnostics", params: obj(json!({}), &[]), result: obj(json!({"bindings": t("array", ""), "modes": t("array", ""), "actions": t("array", ""), "hosts": t("array", ""), "diagnostics": t("array", "")}), &["bindings", "modes", "actions", "hosts", "diagnostics"]), example: json!({}) },
        Method { name: "config.reload", summary: "Re-evaluate config.lua; a failed load keeps the previous configuration", params: obj(json!({}), &[]), result: obj(json!({"ok": t("boolean", ""), "error": t(S, ""), "diagnostics": t("array", "")}), &["ok", "diagnostics"]), example: json!(null) },
        Method { name: "action.list", summary: "Custom actions defined in config.lua", params: obj(json!({}), &[]), result: t("array", ""), example: json!({}) },
        Method { name: "action.run", summary: "Run a custom action's callback with typed arguments", params: obj(json!({"name": t(S, ""), "args": t("object", "")}), &["name"]), result: obj(json!({"result": t("any", "")}), &[]), example: json!(null) },
    ]
}

pub fn is_blocking(method: &str) -> bool {
    matches!(method, "terminal.wait" | "terminal.wait_text" | "action.run" | "config.reload")
}

fn s<'a>(p: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    p[k].as_str().ok_or_else(|| ApiError::new(code::INVALID, format!("missing string parameter '{k}'")))
}

fn ok() -> Result<Value, ApiError> {
    Ok(json!({"ok": true}))
}

/// Checks params against the method's schema: required keys, known keys,
/// basic types. Keeps every caller honest, including Lua.
fn validate(m: &Method, p: &Value) -> Result<(), ApiError> {
    let Some(obj) = p.as_object() else {
        return Err(ApiError::new(code::INVALID, "params must be an object"));
    };
    let props = m.params["properties"].as_object().cloned().unwrap_or_default();
    for r in m.params["required"].as_array().into_iter().flatten() {
        let k = r.as_str().unwrap();
        if obj.get(k).is_none_or(|v| v.is_null()) {
            return Err(ApiError::new(code::INVALID, format!("{}: missing required parameter '{k}'", m.name)));
        }
    }
    for (k, v) in obj {
        let Some(spec) = props.get(k) else {
            return Err(ApiError::new(code::INVALID, format!("{}: unknown parameter '{k}'", m.name)));
        };
        if v.is_null() {
            continue;
        }
        let ty = spec["type"].as_str().unwrap_or("any");
        let good = match ty {
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "array" => v.is_array() || (k == "command" && v.is_string()),
            "object" => v.is_object(),
            _ => true,
        };
        if !good {
            return Err(ApiError::new(code::INVALID, format!("{}: parameter '{k}' must be {ty}", m.name)));
        }
        if let Some(en) = spec["enum"].as_array() {
            if !en.contains(v) {
                return Err(ApiError::new(code::INVALID, format!("{}: parameter '{k}' must be one of {}", m.name, Value::Array(en.clone()))));
            }
        }
    }
    Ok(())
}

impl Server {
    fn block(&self, p: &Value) -> Result<Arc<Block>, ApiError> {
        let id = s(p, "block")?;
        Server::find_block(&self.core.lock().unwrap(), id)
    }

    fn kill_blocks(core: &mut Core, ids: &[String]) {
        for id in ids {
            if let Some(b) = core.blocks.get(id).cloned() {
                b.kill();
            }
            Server::retire_block(core, id);
        }
    }

    pub fn call(self: &Arc<Self>, ctx: &Ctx, method: &str, p: &Value) -> Result<Value, ApiError> {
        let all = methods();
        let m = all.iter().find(|m| m.name == method).ok_or_else(|| ApiError::new(code::UNKNOWN_METHOD, format!("unknown method '{method}'; see api.list")))?;
        validate(m, p)?;
        match method {
            "server.version" => Ok(json!({"version": env!("CARGO_PKG_VERSION"), "protocol": hearth_core::proto::PROTOCOL, "incarnation": self.incarnation})),
            "server.status" => {
                let core = self.core.lock().unwrap();
                let clients: Vec<Value> = core.clients.values().map(|c| json!({"id": c.id, "kind": c.kind, "name": c.name, "principal": c.principal, "transport": c.transport, "remote_control": c.remote_control, "subscriptions": c.subs.len()})).collect();
                let cfg = self.lua.loaded();
                Ok(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "incarnation": self.incarnation,
                    "pid": std::process::id(),
                    "socket": self.socket,
                    "started_ms": self.started_ms,
                    "sessions": core.s.sessions.len(),
                    "blocks": core.blocks.len(),
                    "clients": clients,
                    "listen": *self.opts.listen.lock().unwrap(),
                    "persist": self.opts.persist.load(std::sync::atomic::Ordering::Relaxed),
                    "auto_exit": self.opts.auto_exit,
                    "config": {"path": cfg.path, "loaded": cfg.loaded, "diagnostics": cfg.diagnostics, "error": cfg.error},
                }))
            }
            "server.stop" => {
                {
                    let mut core = self.core.lock().unwrap();
                    let n = core.blocks.len();
                    if n > 0 && p["force"].as_bool() != Some(true) {
                        return Err(ApiError::new(code::FAILED, format!("{n} terminal(s) still running; stopping ends them. Pass force=true")));
                    }
                    core.dirty = true;
                }
                // Keep the layout for the next start, as a stop by signal does,
                // then stop saving so ending the programs does not erase it.
                self.persist_now();
                self.opts.persist.store(false, std::sync::atomic::Ordering::Relaxed);
                let mut core = self.core.lock().unwrap();
                let ids: Vec<String> = core.blocks.keys().cloned().collect();
                Self::kill_blocks(&mut core, &ids);
                Server::emit(&mut core, "server.stopping", json!({}));
                self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                ok()
            }
            "api.list" => Ok(Value::Array(all.iter().map(|m| json!({"name": m.name, "summary": m.summary})).collect())),
            "api.describe" => {
                let name = s(p, "method")?;
                let d = all.iter().find(|m| m.name == name).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no method '{name}'")))?;
                Ok(json!({"name": d.name, "summary": d.summary, "params": d.params, "result": d.result, "blocking": is_blocking(d.name), "example": d.example}))
            }
            "session.list" => {
                let core = self.core.lock().unwrap();
                Ok(Value::Array(core.s.sessions.iter().map(|s| json!({"id": s.id, "name": s.name, "windows": s.windows.len(), "revision": s.revision, "active_window": s.active_window})).collect()))
            }
            "session.bootstrap" => {
                let mut core = self.core.lock().unwrap();
                if p["subscribe"].as_bool() != Some(false) {
                    if let Some(c) = &ctx.conn {
                        if !core.subscribers.iter().any(|x| x.id == c.id) {
                            core.subscribers.push(c.clone());
                        }
                    }
                }
                let sessions: Vec<Value> = core.s.sessions.iter().map(|s| session_view(&core, s)).collect();
                Ok(json!({"seq": core.event_seq, "sessions": sessions, "client_id": ctx.client}))
            }
            "session.view" => {
                let core = self.core.lock().unwrap();
                let sid = core.s.resolve_session(s(p, "session")?)?;
                Ok(session_view(&core, core.s.session(&sid)?))
            }
            "session.create" => {
                let mut core = self.core.lock().unwrap();
                // Allocate the session id first so the shell's environment has it.
                let sid = core.s.alloc('s');
                let b = self.create_block(&mut core, &sid, p)?;
                let (sid, wid) = core.s.new_session(sid, p["name"].as_str().map(String::from), &b.id, crate::block::now_ms());
                Server::emit_session(&mut core, &sid);
                Ok(json!({"session": sid, "window": wid, "block": b.id}))
            }
            "session.rename" => {
                let mut core = self.core.lock().unwrap();
                let sid = core.s.resolve_session(s(p, "session")?)?;
                let sess = core.s.session_mut(&sid)?;
                sess.name = s(p, "name")?.to_string();
                sess.revision += 1;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "session.close" => {
                let mut core = self.core.lock().unwrap();
                let sid = core.s.resolve_session(s(p, "session")?)?;
                let mut ids = vec![];
                for w in &core.s.session(&sid)?.windows {
                    w.layout.leaves(&mut ids);
                }
                Self::kill_blocks(&mut core, &ids);
                core.s.sessions.retain(|x| x.id != sid);
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "window.create" => {
                let mut core = self.core.lock().unwrap();
                let sid = core.s.resolve_session(s(p, "session")?)?;
                let b = self.create_block(&mut core, &sid, p)?;
                let wid = core.s.new_window(&sid, p["name"].as_str().map(String::from), &b.id, p["index"].as_u64().map(|i| i as usize))?;
                Server::emit_session(&mut core, &sid);
                Ok(json!({"session": sid, "window": wid, "block": b.id}))
            }
            "window.rename" => {
                let mut core = self.core.lock().unwrap();
                let (sid, wid) = core.s.resolve_window(s(p, "window")?)?;
                core.s.window_mut(&sid, &wid)?.name = s(p, "name")?.to_string();
                core.s.session_mut(&sid)?.revision += 1;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "window.close" => {
                let mut core = self.core.lock().unwrap();
                let (sid, wid) = core.s.resolve_window(s(p, "window")?)?;
                let mut ids = vec![];
                core.s.window_mut(&sid, &wid)?.layout.leaves(&mut ids);
                Self::kill_blocks(&mut core, &ids);
                ok()
            }
            "window.move" => {
                let mut core = self.core.lock().unwrap();
                let (_, wid) = core.s.resolve_window(s(p, "window")?)?;
                let sid = core.s.move_window(&wid, p["index"].as_u64().unwrap_or(0) as usize)?;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "window.select" => {
                let mut core = self.core.lock().unwrap();
                let (sid, wid) = core.s.resolve_window(s(p, "window")?)?;
                let sess = core.s.session_mut(&sid)?;
                sess.active_window = wid;
                sess.revision += 1;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "block.split" => {
                let dir = Dir::parse(s(p, "direction")?)?;
                let mut core = self.core.lock().unwrap();
                let target = s(p, "block")?;
                let (sid, _) = core.s.locate(target).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no block '{target}' in a layout")))?;
                let b = self.create_block(&mut core, &sid, p)?;
                let ratio = p["ratio"].as_f64().unwrap_or(0.5) as f32;
                let (sid, wid) = core.s.split(target, &b.id, dir, ratio)?;
                Server::emit_session(&mut core, &sid);
                Ok(json!({"session": sid, "window": wid, "block": b.id}))
            }
            "block.close" => {
                let mut core = self.core.lock().unwrap();
                let id = s(p, "block")?.to_string();
                if !core.blocks.contains_key(&id) {
                    return Err(ApiError::new(code::NOT_FOUND, format!("no running block '{id}'")));
                }
                Self::kill_blocks(&mut core, &[id]);
                ok()
            }
            "block.focus" => {
                let mut core = self.core.lock().unwrap();
                let id = s(p, "block")?;
                let (sid, wid) = core.s.locate(id).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no block '{id}'")))?;
                let w = core.s.window_mut(&sid, &wid)?;
                let target = match p["direction"].as_str() {
                    Some(d) => neighbor(&w.layout, id, Dir::parse(d)?).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no pane {d} of {id}")))?,
                    None => id.to_string(),
                };
                w.focus = target.clone();
                let sess = core.s.session_mut(&sid)?;
                sess.active_window = wid.clone();
                sess.revision += 1;
                Server::emit_session(&mut core, &sid);
                Ok(json!({"session": sid, "window": wid, "block": target}))
            }
            "block.zoom" => {
                let mut core = self.core.lock().unwrap();
                let id = s(p, "block")?;
                let (sid, wid) = core.s.locate(id).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no block '{id}'")))?;
                let w = core.s.window_mut(&sid, &wid)?;
                let on = p["zoom"].as_bool().unwrap_or(w.zoom.as_deref() != Some(id));
                w.zoom = on.then(|| id.to_string());
                if on {
                    w.focus = id.to_string();
                }
                core.s.session_mut(&sid)?.revision += 1;
                Server::emit_session(&mut core, &sid);
                Ok(json!({"zoom": on}))
            }
            "block.resize" => {
                let mut core = self.core.lock().unwrap();
                let sid = core.s.grow(s(p, "block")?, Dir::parse(s(p, "direction")?)?, p["amount"].as_f64().unwrap_or(0.05) as f32)?;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "block.swap" => {
                let mut core = self.core.lock().unwrap();
                for sid in core.s.swap(s(p, "block")?, s(p, "other")?)? {
                    Server::emit_session(&mut core, &sid);
                }
                ok()
            }
            "block.move" => {
                let mut core = self.core.lock().unwrap();
                for sid in core.s.move_block(s(p, "block")?, s(p, "target")?, Dir::parse(s(p, "direction")?)?)? {
                    Server::emit_session(&mut core, &sid);
                }
                ok()
            }
            "block.info" => {
                let core = self.core.lock().unwrap();
                let b = Server::find_block(&core, s(p, "block")?)?;
                let mut info = b.info();
                info["location"] = json!(core.s.locate(&b.id).map(|(s, w)| json!({"session": s, "window": w})));
                info["running"] = json!(core.blocks.contains_key(&b.id));
                Ok(info)
            }
            "block.own" => {
                let b = self.block(p)?;
                let client = ctx.client.clone().unwrap_or_else(|| "internal".into());
                match s(p, "mode")? {
                    "take" => {
                        b.set_owner_and_size(&client);
                    }
                    "request" => {
                        let unowned = b.inner.lock().unwrap().owner.is_none();
                        if unowned {
                            b.set_owner_and_size(&client);
                        }
                    }
                    _ => b.release_owner(&client),
                }
                Ok(json!({"owner": b.inner.lock().unwrap().owner}))
            }
            "layout.set_ratio" => {
                let mut core = self.core.lock().unwrap();
                let (sid, wid) = core.s.resolve_window(s(p, "window")?)?;
                core.s.set_ratio(&sid, &wid, p["path"].as_str().unwrap_or(""), p["ratio"].as_f64().unwrap_or(0.5) as f32)?;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "layout.balance" => {
                let mut core = self.core.lock().unwrap();
                let (sid, wid) = core.s.resolve_window(s(p, "window")?)?;
                core.s.balance(&sid, &wid)?;
                Server::emit_session(&mut core, &sid);
                ok()
            }
            "terminal.run" => {
                let placement = s(p, "placement")?;
                let mut q = p.clone();
                let o = q.as_object_mut().unwrap();
                o.remove("placement");
                let target = o.remove("target");
                match placement {
                    "session" => self.call(ctx, "session.create", &q),
                    "window" => {
                        let session = target.and_then(|t| t.as_str().map(String::from)).ok_or_else(|| ApiError::new(code::INVALID, "placement=window needs target (a session)"))?;
                        q["session"] = json!(session);
                        self.call(ctx, "window.create", &q)
                    }
                    d => {
                        let block = target.and_then(|t| t.as_str().map(String::from)).ok_or_else(|| ApiError::new(code::INVALID, "split placement needs target (a block)"))?;
                        o.remove("name");
                        q["block"] = json!(block);
                        q["direction"] = json!(d);
                        self.call(ctx, "block.split", &q)
                    }
                }
            }
            "terminal.send_text" => {
                let b = self.block(p)?;
                let text = s(p, "text")?;
                let bracketed = p["paste"].as_bool() == Some(true) && b.inner.lock().unwrap().model.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE);
                let mut bytes = Vec::new();
                if bracketed {
                    bytes.extend(b"\x1b[200~");
                    // A paste must not be able to end the bracket itself.
                    bytes.extend(text.replace("\x1b[201~", "").as_bytes());
                    bytes.extend(b"\x1b[201~");
                } else {
                    bytes.extend(text.as_bytes());
                }
                b.write_input(bytes);
                ok()
            }
            "terminal.send_keys" => {
                let b = self.block(p)?;
                let app = b.inner.lock().unwrap().model.mode().contains(alacritty_terminal::term::TermMode::APP_CURSOR);
                let mut bytes = Vec::new();
                for k in p["keys"].as_array().into_iter().flatten() {
                    let k = k.as_str().ok_or_else(|| ApiError::new(code::INVALID, "keys must be strings"))?;
                    bytes.extend(hearth_core::keys::encode(k, app).map_err(|e| ApiError::new(code::INVALID, e))?);
                }
                b.write_input(bytes);
                ok()
            }
            "terminal.capture" => {
                let b = self.block(p)?;
                let inner = b.inner.lock().unwrap();
                let history = p["history"].as_bool().unwrap_or(false);
                let fmt = p["format"].as_str().unwrap_or("text");
                let content = match fmt {
                    "text" => inner.model.capture_text(history),
                    "html" => inner.model.capture_html(history),
                    _ => String::from_utf8_lossy(&inner.model.snapshot()).into_owned(),
                };
                let (row, col) = inner.model.cursor();
                Ok(json!({"format": fmt, "content": content, "cols": inner.model.cols(), "rows": inner.model.rows(), "cursor": [row, col], "alternate_screen": inner.model.alt_screen(), "offset": inner.model.applied()}))
            }
            "terminal.wait" => {
                let b = self.block(p)?;
                let timeout = p["timeout_ms"].as_u64().map(Duration::from_millis);
                match b.wait_exit(timeout) {
                    Some(e) => Ok(json!({"code": e.code, "signal": e.signal})),
                    None => Err(ApiError::new(code::TIMEOUT, "process still running")),
                }
            }
            "terminal.wait_text" => {
                let b = self.block(p)?;
                let text = s(p, "text")?;
                let deadline = p["timeout_ms"].as_u64().map(|t| std::time::Instant::now() + Duration::from_millis(t));
                loop {
                    let (found, exited) = {
                        let inner = b.inner.lock().unwrap();
                        (inner.model.capture_text(false).contains(text), inner.exit.is_some() && inner.eof)
                    };
                    if found {
                        return ok();
                    }
                    if exited {
                        return Err(ApiError::new(code::FAILED, "process exited before the text appeared"));
                    }
                    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        return Err(ApiError::new(code::TIMEOUT, format!("'{text}' did not appear")));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            "terminal.attach" => {
                let conn = ctx.conn.clone().ok_or_else(|| ApiError::new(code::INVALID, "attach needs a connection"))?;
                let b = self.block(p)?;
                let (sub, client) = {
                    let mut core = self.core.lock().unwrap();
                    let c = core.clients.get_mut(&ctx.conn_id).ok_or_else(|| ApiError::new(code::FAILED, "client gone"))?;
                    c.next_sub += 1;
                    c.subs.insert(c.next_sub, b.id.clone());
                    (c.next_sub, c.id.clone())
                };
                let pref = match (p["cols"].as_u64(), p["rows"].as_u64()) {
                    (Some(c), Some(r)) => Some((c as u16, r as u16, p["cell_w"].as_u64().unwrap_or(8) as u16, p["cell_h"].as_u64().unwrap_or(16) as u16)),
                    _ => None,
                };
                let resume = p["resume"]["generation"].as_str().zip(p["resume"]["offset"].as_u64());
                let info = b.attach(&conn, sub, &client, resume, pref);
                Ok(info)
            }
            "terminal.detach" => {
                let sub = p["sub"].as_u64().unwrap_or(0) as u32;
                let block = {
                    let mut core = self.core.lock().unwrap();
                    let c = core.clients.get_mut(&ctx.conn_id).ok_or_else(|| ApiError::new(code::FAILED, "client gone"))?;
                    let bid = c.subs.remove(&sub).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no subscription {sub}")))?;
                    Server::find_block(&core, &bid).ok()
                };
                if let Some(b) = block {
                    b.detach(ctx.conn_id, Some(sub));
                }
                ok()
            }
            "terminal.resize" => {
                let sub = p["sub"].as_u64().unwrap_or(0) as u32;
                let block = {
                    let core = self.core.lock().unwrap();
                    let c = core.clients.get(&ctx.conn_id).ok_or_else(|| ApiError::new(code::FAILED, "client gone"))?;
                    let bid = c.subs.get(&sub).ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no subscription {sub}")))?;
                    Server::find_block(&core, bid)?
                };
                let pref = (
                    p["cols"].as_u64().unwrap_or(80) as u16,
                    p["rows"].as_u64().unwrap_or(24) as u16,
                    p["cell_w"].as_u64().unwrap_or(8) as u16,
                    p["cell_h"].as_u64().unwrap_or(16) as u16,
                );
                let (applied, owner) = block.set_pref(ctx.conn_id, sub, pref);
                Ok(json!({"applied": applied, "owner": owner}))
            }
            "terminal.set_size" => {
                let b = self.block(p)?;
                b.resize_direct(p["cols"].as_u64().unwrap_or(80) as u16, p["rows"].as_u64().unwrap_or(24) as u16);
                ok()
            }
            "terminal.status" => {
                let b = self.block(p)?;
                let inner = b.inner.lock().unwrap();
                Ok(json!({"records": inner.model.status.records(), "summary": inner.model.status.summary()}))
            }
            "events.subscribe" => {
                let conn = ctx.conn.clone().ok_or_else(|| ApiError::new(code::INVALID, "needs a connection"))?;
                let mut core = self.core.lock().unwrap();
                if !core.subscribers.iter().any(|x| x.id == conn.id) {
                    core.subscribers.push(conn);
                }
                Ok(json!({"seq": core.event_seq}))
            }
            "events.unsubscribe" => {
                let mut core = self.core.lock().unwrap();
                core.subscribers.retain(|c| c.id != ctx.conn_id);
                ok()
            }
            "client.list" => {
                let core = self.core.lock().unwrap();
                Ok(Value::Array(core.clients.values().map(|c| json!({"id": c.id, "kind": c.kind, "name": c.name, "principal": c.principal, "transport": c.transport, "remote_control": c.remote_control, "connected_ms": c.connected_ms})).collect()))
            }
            "client.set" => {
                let mut core = self.core.lock().unwrap();
                if let Some(c) = core.clients.get_mut(&ctx.conn_id) {
                    if let Some(rc) = p["remote_control"].as_bool() {
                        c.remote_control = rc;
                    }
                }
                ok()
            }
            "client.action" => {
                let core = self.core.lock().unwrap();
                let want = p["client"].as_str();
                let mut n = 0;
                for c in core.clients.values() {
                    if c.kind == "app" && c.remote_control && want.is_none_or(|w| w == c.id) {
                        c.conn.send_ordered(crate::block::event_frame("client.action", &json!({"action": p["action"], "args": p["args"]})));
                        n += 1;
                    }
                }
                if n == 0 {
                    return Err(ApiError::new(code::NOT_READY, "no connected app has Remote Control enabled"));
                }
                Ok(json!({"delivered": n}))
            }
            "config.get" => Ok(self.lua.loaded().to_json()),
            "config.reload" => {
                let r = self.lua.reload();
                let cfg = self.lua.loaded();
                let mut core = self.core.lock().unwrap();
                Server::emit(&mut core, "config.reloaded", json!({"ok": r.is_ok()}));
                Ok(json!({"ok": r.is_ok(), "error": r.err(), "diagnostics": cfg.diagnostics}))
            }
            "action.list" => Ok(json!(self.lua.loaded().actions)),
            "action.run" => {
                let name = s(p, "name")?;
                let args = if p["args"].is_null() { json!({}) } else { p["args"].clone() };
                self.lua.run_action(name, args, ctx.client.clone()).map(|r| json!({"result": r}))
            }
            _ => Err(ApiError::new(code::UNKNOWN_METHOD, format!("method '{method}' is described but not implemented"))),
        }
    }
}
