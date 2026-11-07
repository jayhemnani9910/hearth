//! Lua configuration and custom actions.
//!
//! config.lua runs on a dedicated thread. Loading only *declares* things
//! (bindings, modes, actions, hosts, settings); live API calls are refused
//! during load and allowed inside action callbacks. A load that fails keeps
//! the previous configuration. Invalid individual declarations are skipped
//! and reported as diagnostics.

use crate::sync::Lock;
use crate::server::{Ctx, Server};
use hearth_core::conn::ApiError;
use hearth_core::proto::code;
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, Value as LV, VmState};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

static SERVER: OnceLock<Weak<Server>> = OnceLock::new();

pub fn bind_server(s: &Arc<Server>) {
    let _ = SERVER.set(Arc::downgrade(s));
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub history: usize,
    pub default_cwd: Option<String>,
    pub listen: Vec<String>,
    pub allow: Vec<String>,
    pub whois: String,
    pub restore: bool,
    pub action_timeout_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            history: crate::model::DEFAULT_HISTORY,
            default_cwd: None,
            listen: vec![],
            allow: vec![],
            whois: "tailscale".into(),
            restore: true,
            action_timeout_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub path: Option<PathBuf>,
    pub loaded: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<String>,
    pub bindings: Vec<Value>,
    pub modes: Vec<Value>,
    pub actions: Vec<Value>,
    pub hosts: Vec<Value>,
    pub settings: Settings,
}

impl Loaded {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "loaded": self.loaded,
            "error": self.error,
            "diagnostics": self.diagnostics,
            "bindings": self.bindings,
            "modes": self.modes,
            "actions": self.actions,
            "hosts": self.hosts,
            "settings": {
                "history": self.settings.history,
                "default_cwd": self.settings.default_cwd,
                "listen": self.settings.listen,
                "allow": self.settings.allow,
                "restore": self.settings.restore,
            },
        })
    }
}

enum Req {
    Load(PathBuf, Sender<Result<(), String>>),
    Run(String, Value, Option<String>, Sender<Result<Value, ApiError>>),
}

pub struct Engine {
    tx: Mutex<Sender<Req>>,
    loaded: Arc<Mutex<Loaded>>,
    cancel: Arc<AtomicBool>,
}

pub fn config_path() -> PathBuf {
    hearth_core::paths::config_dir().join("config.lua")
}

impl Engine {
    pub fn start() -> Engine {
        let (tx, rx) = mpsc::channel::<Req>();
        let loaded = Arc::new(Mutex::new(Loaded::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let l2 = loaded.clone();
        let c2 = cancel.clone();
        std::thread::Builder::new()
            .name("lua".into())
            .spawn(move || {
                let mut current: Option<Lua> = None;
                for req in rx {
                    match req {
                        Req::Load(path, reply) => {
                            let r = load(&path);
                            let res = match r {
                                Ok((lua, decl)) => {
                                    current = Some(lua);
                                    *l2.lk() = decl;
                                    Ok(())
                                }
                                Err(e) => {
                                    let mut l = l2.lk();
                                    l.error = Some(e.clone());
                                    l.path = Some(path);
                                    Err(e)
                                }
                            };
                            let _ = reply.send(res);
                        }
                        Req::Run(name, args, client, reply) => {
                            let timeout = l2.lk().settings.action_timeout_ms;
                            c2.store(false, Ordering::Relaxed);
                            let r = match &current {
                                Some(lua) => run(lua, &name, args, client, Duration::from_millis(timeout), &c2),
                                None => Err(ApiError::new(code::NOT_FOUND, "no configuration loaded")),
                            };
                            let _ = reply.send(r);
                        }
                    }
                }
            })
            .expect("lua thread");
        Engine { tx: Mutex::new(tx), loaded, cancel }
    }

    pub fn loaded(&self) -> Loaded {
        self.loaded.lk().clone()
    }

    pub fn settings(&self) -> Settings {
        self.loaded.lk().settings.clone()
    }

    pub fn load_file(&self, path: PathBuf) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.lk().send(Req::Load(path, tx));
        rx.recv().unwrap_or(Err("lua thread gone".into()))
    }

    pub fn reload(&self) -> Result<(), String> {
        let path = self.loaded.lk().path.clone().unwrap_or_else(config_path);
        self.load_file(path)
    }

    pub fn run_action(&self, name: &str, args: Value, client: Option<String>) -> Result<Value, ApiError> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.lk().send(Req::Run(name.into(), args, client, tx));
        rx.recv().unwrap_or(Err(ApiError::new(code::FAILED, "lua thread gone")))
    }

    #[allow(dead_code)]
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn lerr(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::RuntimeError(e.to_string())
}

struct Decl {
    bindings: Vec<Value>,
    modes: Vec<Value>,
    actions: Vec<Value>,
    hosts: Vec<Value>,
    settings: Settings,
    diagnostics: Vec<String>,
}

fn check_bind(keys: &str, action: &str) -> Result<(), String> {
    hearth_core::keys::parse_sequence(keys)?;
    if action.is_empty() {
        return Err(format!("binding '{keys}' has no action"));
    }
    Ok(())
}

fn install_api(lua: &Lua, decl: Arc<Mutex<Decl>>, in_config: Arc<AtomicBool>) -> mlua::Result<()> {
    let hearth = lua.create_table()?;
    let d = decl.clone();
    hearth.set(
        "bind",
        lua.create_function(move |lua, (keys, action, opts): (String, String, Option<Table>)| {
            let mode: Option<String> = opts.as_ref().and_then(|o| o.get("mode").ok());
            let args: Value = match opts.as_ref().and_then(|o| o.get::<LV>("args").ok()) {
                Some(v) if !v.is_nil() => lua.from_value(v)?,
                _ => json!({}),
            };
            let mut d = d.lk();
            match check_bind(&keys, &action) {
                Ok(()) => d.bindings.push(json!({"keys": keys, "action": action, "args": args, "mode": mode})),
                Err(e) => d.diagnostics.push(format!("bind: {e}")),
            }
            Ok(())
        })?,
    )?;
    let d = decl.clone();
    hearth.set(
        "mode",
        lua.create_function(move |_, t: Table| {
            let name: Option<String> = t.get("name")?;
            let mut d = d.lk();
            let Some(name) = name else {
                d.diagnostics.push("mode: missing name".into());
                return Ok(());
            };
            let exclusive: bool = t.get::<Option<bool>>("exclusive")?.unwrap_or(false);
            let mut binds = vec![];
            if let Some(b) = t.get::<Option<Table>>("bindings")? {
                for pair in b.pairs::<String, String>() {
                    let (k, a) = pair?;
                    match check_bind(&k, &a) {
                        Ok(()) => binds.push(json!({"keys": k, "action": a})),
                        Err(e) => d.diagnostics.push(format!("mode {name}: {e}")),
                    }
                }
            }
            d.modes.push(json!({"name": name, "exclusive": exclusive, "bindings": binds}));
            Ok(())
        })?,
    )?;
    let d = decl.clone();
    hearth.set(
        "action",
        lua.create_function(move |lua, t: Table| {
            let name: Option<String> = t.get("name")?;
            let run: Option<Function> = t.get("run")?;
            let mut dd = d.lk();
            let (Some(name), Some(run)) = (name, run) else {
                dd.diagnostics.push("action: needs name and run=function".into());
                return Ok(());
            };
            if dd.actions.iter().any(|a| a["name"] == name) {
                dd.diagnostics.push(format!("action {name}: defined twice; keeping the first"));
                return Ok(());
            }
            let mut args = vec![];
            if let Some(a) = t.get::<Option<Table>>("args")? {
                for spec in a.sequence_values::<Table>() {
                    let spec = spec?;
                    let an: Option<String> = spec.get("name")?;
                    let ty: String = spec.get::<Option<String>>("type")?.unwrap_or("string".into());
                    let req: bool = spec.get::<Option<bool>>("required")?.unwrap_or(false);
                    match an {
                        Some(an) if matches!(ty.as_str(), "string" | "number" | "integer" | "boolean") => {
                            args.push(json!({"name": an, "type": ty, "required": req}))
                        }
                        _ => {
                            dd.diagnostics.push(format!("action {name}: argument needs a name and type string/number/integer/boolean"));
                            return Ok(());
                        }
                    }
                }
            }
            let title: String = t.get::<Option<String>>("title")?.unwrap_or_else(|| name.clone());
            let registry: Table = lua.named_registry_value("hearth_actions")?;
            registry.set(name.as_str(), run)?;
            dd.actions.push(json!({"name": name, "title": title, "args": args}));
            Ok(())
        })?,
    )?;
    let d = decl.clone();
    hearth.set(
        "host",
        lua.create_function(move |_, t: Table| {
            let name: Option<String> = t.get("name")?;
            let address: Option<String> = t.get("address")?;
            let mut d = d.lk();
            match (name, address) {
                (Some(n), Some(a)) => match hearth_core::conn::Target::parse(&a) {
                    Ok(_) => d.hosts.push(json!({"name": n, "address": a})),
                    Err(e) => d.diagnostics.push(format!("host {n}: {e}")),
                },
                _ => d.diagnostics.push("host: needs name and address".into()),
            }
            Ok(())
        })?,
    )?;
    let d = decl.clone();
    hearth.set(
        "set",
        lua.create_function(move |_, t: Table| {
            let mut d = d.lk();
            for pair in t.pairs::<String, LV>() {
                let (k, v) = pair?;
                let s = &mut d.settings;
                let res: Result<(), String> = match (k.as_str(), &v) {
                    ("history", LV::Integer(n)) if *n >= 0 && *n <= 1_000_000 => {
                        s.history = *n as usize;
                        Ok(())
                    }
                    ("default_cwd", LV::String(x)) => {
                        s.default_cwd = Some(x.to_str().map_err(lerr)?.to_string());
                        Ok(())
                    }
                    ("restore", LV::Boolean(b)) => {
                        s.restore = *b;
                        Ok(())
                    }
                    ("whois", LV::String(x)) => {
                        s.whois = x.to_str().map_err(lerr)?.to_string();
                        Ok(())
                    }
                    ("action_timeout_ms", LV::Integer(n)) if *n > 0 => {
                        s.action_timeout_ms = *n as u64;
                        Ok(())
                    }
                    ("listen" | "allow", LV::Table(tt)) => {
                        let mut list = vec![];
                        for x in tt.sequence_values::<String>() {
                            list.push(x?);
                        }
                        if k == "listen" {
                            s.listen = list
                        } else {
                            s.allow = list
                        }
                        Ok(())
                    }
                    _ => Err(format!("set: unknown setting or bad value for '{k}'")),
                };
                if let Err(e) = res {
                    d.diagnostics.push(e);
                }
            }
            Ok(())
        })?,
    )?;
    let ic = in_config.clone();
    hearth.set(
        "call",
        lua.create_function(move |lua, (method, params): (String, Option<LV>)| {
            if ic.load(Ordering::Relaxed) {
                return Err(lerr("hearth.call is not available while config.lua loads; call it inside an action's run function"));
            }
            if method == "action.run" || method == "config.reload" {
                return Err(lerr(format!("{method} cannot be called from Lua")));
            }
            let params: Value = match params {
                Some(v) if !v.is_nil() => lua.from_value(v)?,
                _ => json!({}),
            };
            let server = SERVER.get().and_then(|w| w.upgrade()).ok_or_else(|| lerr("server gone"))?;
            match server.call(&Ctx::internal(), &method, &params) {
                Ok(v) => lua.to_value(&v),
                Err(e) => Err(lerr(format!("{}: {}", e.code, e.message))),
            }
        })?,
    )?;
    let ic = in_config.clone();
    hearth.set(
        "client_action",
        lua.create_function(move |lua, (action, args): (String, Option<LV>)| {
            if ic.load(Ordering::Relaxed) {
                return Err(lerr("client_action is only available inside action callbacks"));
            }
            let args: Value = match args {
                Some(v) if !v.is_nil() => lua.from_value(v)?,
                _ => json!({}),
            };
            let server = SERVER.get().and_then(|w| w.upgrade()).ok_or_else(|| lerr("server gone"))?;
            match server.call(&Ctx::internal(), "client.action", &json!({"action": action, "args": args})) {
                Ok(v) => lua.to_value(&v),
                Err(e) => Err(lerr(format!("{}: {}", e.code, e.message))),
            }
        })?,
    )?;
    hearth.set(
        "log",
        lua.create_function(|_, msg: String| {
            eprintln!("hearthd[lua]: {msg}");
            Ok(())
        })?,
    )?;
    // hearth.api.session_create{...} == hearth.call("session.create", {...})
    let api = lua.create_table()?;
    let call: Function = hearth.get("call")?;
    for m in crate::api::methods() {
        let name = m.name.to_string();
        let c = call.clone();
        api.set(m.name.replace('.', "_"), lua.create_function(move |_, params: Option<LV>| c.call::<LV>((name.clone(), params)))?)?;
    }
    hearth.set("api", api)?;
    lua.globals().set("hearth", hearth)?;
    lua.set_named_registry_value("hearth_actions", lua.create_table()?)?;
    Ok(())
}

fn load(path: &PathBuf) -> Result<(Lua, Loaded), String> {
    let lua = Lua::new();
    let decl = Arc::new(Mutex::new(Decl {
        bindings: vec![],
        modes: vec![],
        actions: vec![],
        hosts: vec![],
        settings: Settings::default(),
        diagnostics: vec![],
    }));
    let in_config = Arc::new(AtomicBool::new(true));
    install_api(&lua, decl.clone(), in_config.clone()).map_err(|e| e.to_string())?;
    let src = match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if let Some(src) = &src {
        // Bound the load too, so a loop in config.lua cannot hang the server.
        let deadline = Instant::now() + Duration::from_secs(5);
        lua.set_hook(HookTriggers::new().every_nth_instruction(10_000), move |_, _| {
            if Instant::now() > deadline {
                return Err(lerr("config.lua took longer than 5s to load"));
            }
            Ok(VmState::Continue)
        })
        .map_err(|e| e.to_string())?;
        lua.load(src.as_str()).set_name(format!("@{}", path.display())).exec().map_err(|e| e.to_string())?;
        lua.remove_hook();
    }
    in_config.store(false, Ordering::Relaxed);
    let d = decl.lk();
    Ok((
        lua,
        Loaded {
            path: Some(path.clone()),
            loaded: src.is_some(),
            error: None,
            diagnostics: d.diagnostics.clone(),
            bindings: d.bindings.clone(),
            modes: d.modes.clone(),
            actions: d.actions.clone(),
            hosts: d.hosts.clone(),
            settings: d.settings.clone(),
        },
    ))
}

fn check_args(spec: &Value, args: &Value) -> Result<(), ApiError> {
    let obj = args.as_object().ok_or_else(|| ApiError::new(code::INVALID, "args must be an object"))?;
    let specs = spec["args"].as_array().cloned().unwrap_or_default();
    for s in &specs {
        let name = s["name"].as_str().unwrap_or("");
        match obj.get(name) {
            None | Some(Value::Null) if s["required"] == true => {
                return Err(ApiError::new(code::INVALID, format!("missing required argument '{name}'")));
            }
            None | Some(Value::Null) => {}
            Some(v) => {
                let good = match s["type"].as_str().unwrap_or("string") {
                    "string" => v.is_string(),
                    "number" => v.is_number(),
                    "integer" => v.is_i64() || v.is_u64(),
                    "boolean" => v.is_boolean(),
                    _ => false,
                };
                if !good {
                    return Err(ApiError::new(code::INVALID, format!("argument '{name}' must be {}", s["type"].as_str().unwrap_or("string"))));
                }
            }
        }
    }
    for k in obj.keys() {
        if !specs.iter().any(|s| s["name"] == k.as_str()) {
            return Err(ApiError::new(code::INVALID, format!("unknown argument '{k}'")));
        }
    }
    Ok(())
}

fn run(lua: &Lua, name: &str, args: Value, client: Option<String>, timeout: Duration, cancel: &Arc<AtomicBool>) -> Result<Value, ApiError> {
    let spec = {
        let server = SERVER.get().and_then(|w| w.upgrade());
        let actions = server.map(|s| s.lua.loaded().actions).unwrap_or_default();
        actions.into_iter().find(|a| a["name"] == name)
    }
    .ok_or_else(|| ApiError::new(code::NOT_FOUND, format!("no action '{name}'")))?;
    check_args(&spec, &args)?;
    let registry: Table = lua.named_registry_value("hearth_actions").map_err(|e| ApiError::new(code::FAILED, e.to_string()))?;
    let f: Function = registry.get(name).map_err(|e| ApiError::new(code::FAILED, e.to_string()))?;
    let deadline = Instant::now() + timeout;
    let c = cancel.clone();
    let _ = lua.set_hook(HookTriggers::new().every_nth_instruction(1_000), move |_, _| {
        if c.load(Ordering::Relaxed) {
            return Err(lerr("action cancelled"));
        }
        if Instant::now() > deadline {
            return Err(lerr("action timed out"));
        }
        Ok(VmState::Continue)
    });
    let largs = lua.to_value(&args).map_err(|e| ApiError::new(code::FAILED, e.to_string()))?;
    let ctx = lua.create_table().map_err(|e| ApiError::new(code::FAILED, e.to_string()))?;
    let _ = ctx.set("client", client);
    let r = f.call::<LV>((largs, ctx));
    lua.remove_hook();
    match r {
        Ok(v) => lua.from_value::<Value>(v).map_err(|e| ApiError::new(code::FAILED, e.to_string())),
        Err(e) => {
            let msg = e.to_string();
            let c = if msg.contains("timed out") { code::TIMEOUT } else { code::FAILED };
            Err(ApiError::new(c, format!("action '{name}' failed: {msg}")))
        }
    }
}
