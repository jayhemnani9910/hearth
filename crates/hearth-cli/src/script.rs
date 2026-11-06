//! `hearth lua FILE`: Lua scripts that drive a running server.
//!
//!   hearth.call(method, params)      any API method
//!   hearth.api.session_list()        same, one function per method
//!   hearth.on(event, fn) -> id       handle events ("*" for all)
//!   hearth.off(id)
//!   hearth.run([timeout_ms])         dispatch events until hearth.stop()
//!   hearth.stop()  hearth.sleep(ms)  hearth.env  arg

use hearth_core::conn::{Client, Incoming};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value as LV};
use serde_json::{Value, json};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn lerr(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::RuntimeError(e.to_string())
}

pub fn run(client: Client, rx: Receiver<Incoming>, args: &[String]) -> i32 {
    let Some(file) = args.first() else { crate::die(2, "usage: hearth lua FILE [ARGS...]") };
    let src = std::fs::read_to_string(file).unwrap_or_else(|e| crate::die(2, format!("{file}: {e}")));
    let client = Arc::new(client);
    let lua = Lua::new();
    let handlers: Arc<Mutex<Vec<(u64, String, mlua::RegistryKey)>>> = Arc::new(Mutex::new(vec![]));
    let stop = Arc::new(Mutex::new(false));
    let subscribed = Arc::new(Mutex::new(false));
    let result = (|| -> mlua::Result<()> {
        let h = lua.create_table()?;
        let c = client.clone();
        let call = lua.create_function(move |lua, (method, params): (String, Option<LV>)| {
            let p: Value = match params {
                Some(v) if !v.is_nil() => lua.from_value(v)?,
                _ => json!({}),
            };
            match c.call_timeout(&method, p, Duration::from_secs(3600)) {
                Ok(v) => lua.to_value(&v),
                Err(e) => Err(lerr(format!("{}: {}", e.code, e.message))),
            }
        })?;
        h.set("call", call.clone())?;
        let api = lua.create_table()?;
        let methods = client.call("api.list", json!({})).map_err(lerr)?;
        for m in methods.as_array().into_iter().flatten() {
            let name = m["name"].as_str().unwrap_or("").to_string();
            let c = call.clone();
            let n2 = name.clone();
            api.set(name.replace('.', "_"), lua.create_function(move |_, p: Option<LV>| c.call::<LV>((n2.clone(), p)))?)?;
        }
        h.set("api", api)?;
        let hs = handlers.clone();
        let sub = subscribed.clone();
        let c = client.clone();
        let next = Arc::new(Mutex::new(0u64));
        h.set(
            "on",
            lua.create_function(move |lua, (event, f): (String, Function)| {
                if !*sub.lock().unwrap() {
                    c.call("events.subscribe", json!({})).map_err(lerr)?;
                    *sub.lock().unwrap() = true;
                }
                *next.lock().unwrap() += 1;
                let id = *next.lock().unwrap();
                hs.lock().unwrap().push((id, event, lua.create_registry_value(f)?));
                Ok(id)
            })?,
        )?;
        let hs = handlers.clone();
        h.set(
            "off",
            lua.create_function(move |_, id: u64| {
                hs.lock().unwrap().retain(|(i, _, _)| *i != id);
                Ok(())
            })?,
        )?;
        let st = stop.clone();
        h.set(
            "stop",
            lua.create_function(move |_, ()| {
                *st.lock().unwrap() = true;
                Ok(())
            })?,
        )?;
        h.set(
            "sleep",
            lua.create_function(|_, ms: u64| {
                std::thread::sleep(Duration::from_millis(ms));
                Ok(())
            })?,
        )?;
        let env = lua.create_table()?;
        for k in ["HEARTH_SESSION", "HEARTH_BLOCK", "HEARTH_SERVER"] {
            if let Ok(v) = std::env::var(k) {
                env.set(k, v)?;
            }
        }
        h.set("env", env)?;
        let hs = handlers.clone();
        let st = stop.clone();
        h.set(
            "run",
            lua.create_function(move |lua, timeout: Option<u64>| {
                let deadline = timeout.map(|t| Instant::now() + Duration::from_millis(t));
                *st.lock().unwrap() = false;
                while !*st.lock().unwrap() && !hs.lock().unwrap().is_empty() {
                    let wait = deadline.map(|d| d.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_millis(200));
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return Ok(false);
                    }
                    let Ok(m) = rx.recv_timeout(wait.min(Duration::from_millis(200))) else { continue };
                    let v = match m {
                        Incoming::Event(v) => v,
                        Incoming::Closed(r) => return Err(lerr(format!("connection closed: {r}"))),
                        _ => continue,
                    };
                    let name = v["event"].as_str().unwrap_or("").to_string();
                    let data = lua.to_value(&v["data"])?;
                    let matching: Vec<Function> = hs
                        .lock().unwrap()
                        .iter()
                        .filter(|(_, e, _)| e == &name || e == "*")
                        .filter_map(|(_, _, k)| lua.registry_value::<Function>(k).ok())
                        .collect();
                    for f in matching {
                        // A failing handler is reported; the loop goes on.
                        if let Err(e) = f.call::<()>((name.clone(), data.clone())) {
                            eprintln!("hearth lua: handler for {name} failed: {e}");
                        }
                    }
                }
                Ok(true)
            })?,
        )?;
        lua.globals().set("hearth", h)?;
        let arg: Table = lua.create_table()?;
        for (i, a) in args.iter().enumerate() {
            arg.set(i as i64, a.clone())?;
        }
        lua.globals().set("arg", arg)?;
        lua.load(&src).set_name(format!("@{file}")).exec()
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hearth lua: {e}");
            1
        }
    }
}
