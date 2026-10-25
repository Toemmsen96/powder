//! Server mods: small Lua scripts the host drops into the mods folder to watch events and answer calls,
//! the Rust rework's take on the Python server's plugins (same config, see [`crate::config::ModsConfig`]).
//!
//! A mod is either `<mods>/<name>.lua` or `<mods>/<name>/mod.lua` (also `init.lua`); a name starting with
//! `_` or `.` is skipped, so `_example/` ships as documentation. A mod folder may also ship files for the
//! players' games (`client/`, `mod.toml`): see client_mods.rs. Either part is optional. Each mod is a Lua chunk that hands the
//! server a `setup` function — the recommended form returns it:
//!
//! ```lua
//! return function(api)
//!     api.log.info("loaded, API v" .. api.version)
//!     api.on("purchase", function(ev) api.log.info(ev.profile_id .. " " .. ev.offer_id) end)
//! end
//! ```
//!
//! `function setup(api) ... end` (no `return`) works too. Each mod runs in its own environment (its own globals
//! and its own copies of the library tables), so two mods don't clobber each other.
//!
//! Trust: server mods are installed by the host and run with the host's trust (they answer requests and read and
//! write the server's database through `api`). The sandbox is defense in depth against a buggy mod, not a
//! boundary for untrusted code: the standard library is reduced to `string`, `table`, `math`, `utf8`,
//! `coroutine`, `os.{time,clock,date,difftime}` and the base functions (no `io`, `os.execute`/`remove`/`exit`/
//! `getenv`/…, `require`, `package`, `dofile`, `loadfile`, `debug`; `load` takes text chunks only), the Lua
//! state may use at most [`MOD_MEMORY_LIMIT`] bytes, and a hook or event handler running longer than
//! [`MOD_HOOK_TIME_LIMIT`] (a mod's loading: [`MOD_LOAD_TIME_LIMIT`]) is aborted with a logged error.
//!
//! The `api` table:
//!   api.version                         this API's version ([`MOD_API_VERSION`]); bumped on a breaking change
//!   api.name                            the mod's name (its folder/file name)
//!   api.config                          a read-only snapshot: server.{name,public_host,country,region,public},
//!                                       gameplay toggles, mods.dir
//!   api.log.{debug,info,warn,error}(…)  log under `powder.mod.<name>`; extra args are space-joined
//!   api.on(event, fn)                   events: "login", "ws_connected", "nex_connected", "purchase"
//!   api.before_http(method, glob, fn)   method "GET"/… or "*"; glob matches the URL path (`*` and `?`)
//!   api.before_rmc(method, fn)          method e.g. "PlayerStatsProtocol::GetProfile_V1"
//!   api.save(key, value) / api.load(key, default)   per-mod storage (SQLite kv, scope "mod:<name>")
//!   api.json.{decode,encode}, api.admin.*   JSON, and players / inventory / stats / notifications for admin
//!                                       tools (see mods_admin.rs; admins are `[server] admins`)
//!
//! A hook runs on the request path: `before_http(req) -> response | nil` and `before_rmc(ctx, params) ->
//! bytes | nil`, where `nil` lets the server answer as usual and the first non-nil wins. Errors raised by a
//! mod are logged and swallowed, never reaching the game.

use crate::client_mods::{self, ClientMod};
use crate::config::Config;
use crate::mods_admin::ServicesSlot;
use crate::repos::Repos;
use mlua::{ChunkMode, Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value, Variadic, VmState};
use parking_lot::Mutex;
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bumped when a change would break existing mods; a mod can read it as `api.version`.
pub const MOD_API_VERSION: i64 = 1;
/// Memory the mods' Lua state may use in all (small scripts need a few hundred KB).
pub const MOD_MEMORY_LIMIT: usize = 128 * 1024 * 1024;
/// Longest a hook or event handler may run: it runs on the request path, holding the mods' lock.
pub const MOD_HOOK_TIME_LIMIT: Duration = Duration::from_millis(100);
/// Longest a mod's chunk and its `setup(api)` may run at load time.
pub const MOD_LOAD_TIME_LIMIT: Duration = Duration::from_secs(2);
/// The time limit is checked every this many Lua instructions.
const HOOK_EVERY: u32 = 10_000;

/// Builds the sandbox, run once with the full libraries still in reach; returns `new_env()`, which makes one
/// mod's globals. Coroutines get the time check as a Lua hook of their own (a Rust hook only watches the main
/// thread); `debug` stays reachable only as this chunk's upvalue.
const SANDBOX: &str = r#"
local over, every = ...
local sethook, base_load, co_create, co_resume = debug.sethook, load, coroutine.create, coroutine.resume
local pack, unpack = table.pack, table.unpack
local function hook()
    if over() then
        sethook(hook, "", 1) -- from now on every instruction raises it, so a pcall can't hold on to the coroutine
        error(ABORTED, 0)
    end
end
local function create(f)
    local co = co_create(f)
    sethook(co, hook, "", every)
    return co
end
local function wrap(f)
    local co = create(f)
    return function(...)
        local r = pack(co_resume(co, ...))
        if not r[1] then error(r[2], 2) end
        return unpack(r, 2, r.n)
    end
end
local base = {}
for _, k in ipairs({"assert", "collectgarbage", "error", "getmetatable", "ipairs", "next", "pairs", "pcall", "print",
                    "rawequal", "rawget", "rawlen", "rawset", "select", "setmetatable", "tonumber", "tostring", "type",
                    "xpcall", "_VERSION"}) do
    base[k] = _G[k]
end
local libs = {
    string = string, table = table, math = math, utf8 = utf8,
    os = {time = os.time, clock = os.clock, date = os.date, difftime = os.difftime},
    coroutine = {create = create, wrap = wrap, resume = co_resume, yield = coroutine.yield, status = coroutine.status,
                 running = coroutine.running, isyieldable = coroutine.isyieldable, close = coroutine.close},
}
-- the shared string table stays out of reach (("x"):upper() still works)
getmetatable("").__metatable = false
return function()
    local env = {}
    for k, v in pairs(base) do env[k] = v end
    for name, lib in pairs(libs) do
        local copy = {}
        for k, v in pairs(lib) do copy[k] = v end
        env[name] = copy
    end
    env._G = env
    env.load = function(chunk, name, _, e) return base_load(chunk, name, "t", e or env) end
    return env
end
"#;

/// A `before_http` registration (one mod's hook for a method + path glob).
#[derive(Clone)]
struct HttpHook {
    mod_name: String,
    method: String,
    glob: String,
    func: Function,
}

/// A `before_rmc` registration (one mod's hook for a NEX method).
#[derive(Clone)]
struct RmcHook {
    mod_name: String,
    func: Function,
}

/// What the mods registered, filled while each `setup(api)` runs and read on the request path.
#[derive(Default)]
struct Registrations {
    events: HashMap<String, Vec<(String, Function)>>,
    http: Vec<HttpHook>,
    rmc: HashMap<String, Vec<RmcHook>>,
}

/// The loaded mods and the Lua state they live in. One per server; not `Sync` (the Lua VM is single
/// threaded), so the server reaches it behind its own lock.
pub struct ModHost {
    lua: Lua,
    /// Makes a mod's globals (see [`SANDBOX`]).
    new_env: Function,
    deadline: SharedDeadline,
    repos: Arc<Repos>,
    reg: Arc<Mutex<Registrations>>,
    config: Json,
    loaded: Vec<String>,
    client: Vec<ClientMod>,
    /// The services, for `api.admin` (attached once the server has built them).
    services: ServicesSlot,
    admins_configured: bool,
}

impl ModHost {
    /// Discover and load every enabled mod. Resilient by design: a mod that fails to load is logged and
    /// skipped so the server still starts (like the Python plugin loader).
    pub fn load(config: &Config, repos: Arc<Repos>) -> ModHost {
        let deadline = SharedDeadline::default();
        let (lua, new_env) = sandbox(deadline.clone()).expect("the mods' Lua sandbox");
        let mut host = ModHost {
            lua,
            new_env,
            deadline,
            repos,
            reg: Arc::new(Mutex::new(Registrations::default())),
            config: config_snapshot(config),
            loaded: vec![],
            client: vec![],
            services: ServicesSlot::default(),
            admins_configured: !config.server.admins.is_empty(),
        };
        let dir = &config.mods.dir;
        if !dir.is_dir() {
            log::debug!(target: "powder.mods", "no mods folder at {}", dir.display());
            return host;
        }
        for found in discover(dir) {
            let name = &found.name;
            if !config.mods.allows(name) {
                log::debug!(target: "powder.mods", "mod {name} is not enabled, skipped");
                continue;
            }
            if let Some(entry) = &found.entry {
                match host.load_one(name, entry) {
                    Ok(()) => log::info!(target: "powder.mods", "loaded mod {name}"),
                    Err(e) => log::error!(target: "powder.mods", "mod {name} failed to load: {e:#}"),
                }
            }
            if let Some(dir) = &found.dir {
                let client = client_mods::Manifest::load(dir).and_then(|m| client_mods::scan(name, dir, &m));
                match client {
                    Ok(Some(c)) => {
                        log::info!(target: "powder.mods", "mod {name}: {} client file(s) for the launcher{}", c.files.len(),
                                   if c.inject.is_empty() { String::new() } else { format!(" (injects {})", c.inject.join(", ")) });
                        host.client.push(c);
                    }
                    Ok(None) => {}
                    Err(e) => log::error!(target: "powder.mods", "mod {name}: client files not published: {e:#}"),
                }
            }
        }
        log::info!(target: "powder.mods", "{} mod(s) loaded, {} with client files", host.loaded.len(), host.client.len());
        host
    }

    /// Give the mods' `api.admin` the server's services (a weak link: the services own this host).
    pub fn attach(&self, services: &Arc<crate::services::Services>) {
        let _ = self.services.set(Arc::downgrade(services));
    }

    /// Names of the mods that loaded, in load order.
    pub fn loaded(&self) -> &[String] {
        &self.loaded
    }

    /// The mods' client parts, published for the launcher (see client_mods.rs).
    pub fn client_mods(&self) -> &[ClientMod] {
        &self.client
    }

    /// What the loaded mods registered, for the CLI (`powder-server mods`) and the status page.
    pub fn report(&self) -> ModReport {
        let reg = self.reg.lock();
        let mut events: Vec<(String, Vec<String>)> = reg
            .events
            .iter()
            .map(|(e, hs)| (e.clone(), hs.iter().map(|(m, _)| m.clone()).collect()))
            .collect();
        events.sort();
        let http = reg.http.iter().map(|h| (h.mod_name.clone(), h.method.clone(), h.glob.clone())).collect();
        let mut rmc: Vec<(String, Vec<String>)> = reg
            .rmc
            .iter()
            .map(|(m, hs)| (m.clone(), hs.iter().map(|h| h.mod_name.clone()).collect()))
            .collect();
        rmc.sort();
        let client = self.client.iter().map(|c| (c.name.clone(), c.files.len(), c.inject.clone())).collect();
        ModReport { loaded: self.loaded.clone(), events, http, rmc, client }
    }

    fn load_one(&mut self, name: &str, path: &Path) -> mlua::Result<()> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| mlua::Error::external(format!("reading {}: {e}", path.display())))?;
        // Each mod gets its own globals and its own copies of the library tables.
        let env: Table = self.new_env.call(())?;
        let chunk = self.lua.load(&source).set_name(format!("@{name}")).set_mode(ChunkMode::Text)
            .set_environment(env.clone());
        let ret: Value = self.limited(MOD_LOAD_TIME_LIMIT, || chunk.eval())?;
        let setup = match ret {
            Value::Function(f) => f,
            _ => match env.get::<Value>("setup")? {
                Value::Function(f) => f,
                _ => {
                    return Err(mlua::Error::external(
                        "no setup function: return a function(api), or define `function setup(api)`",
                    ))
                }
            },
        };
        let api = self.build_api(name)?;
        self.limited(MOD_LOAD_TIME_LIMIT, || setup.call::<()>(api))?;
        self.loaded.push(name.to_string());
        Ok(())
    }

    fn build_api(&self, name: &str) -> mlua::Result<Table> {
        let api = self.lua.create_table()?;
        api.set("version", MOD_API_VERSION)?;
        api.set("name", name)?;
        api.set("config", self.lua.to_value(&self.config)?)?;
        api.set("log", self.build_log(name)?)?;
        api.set("json", crate::mods_admin::json_table(&self.lua)?)?;
        api.set("admin", crate::mods_admin::admin_table(&self.lua, self.services.clone(), self.admins_configured)?)?;

        let reg = self.reg.clone();
        let who = name.to_string();
        api.set(
            "on",
            self.lua.create_function(move |_, (event, func): (String, Function)| {
                reg.lock().events.entry(event).or_default().push((who.clone(), func));
                Ok(())
            })?,
        )?;

        let reg = self.reg.clone();
        let who = name.to_string();
        api.set(
            "before_http",
            self.lua.create_function(move |_, (method, glob, func): (String, String, Function)| {
                reg.lock().http.push(HttpHook { mod_name: who.clone(), method, glob, func });
                Ok(())
            })?,
        )?;

        let reg = self.reg.clone();
        let who = name.to_string();
        api.set(
            "before_rmc",
            self.lua.create_function(move |_, (method, func): (String, Function)| {
                reg.lock().rmc.entry(method).or_default().push(RmcHook { mod_name: who.clone(), func });
                Ok(())
            })?,
        )?;

        let repos = self.repos.clone();
        let scope = format!("mod:{name}");
        api.set(
            "save",
            self.lua.create_function(move |lua, (key, value): (String, Value)| {
                let json: Json = lua.from_value(value)?;
                repos.kv_set(&scope, &key, &json).map_err(mlua::Error::external)
            })?,
        )?;

        let repos = self.repos.clone();
        let scope = format!("mod:{name}");
        api.set(
            "load",
            self.lua.create_function(move |lua, (key, default): (String, Value)| {
                match repos.kv_get(&scope, &key).map_err(mlua::Error::external)? {
                    Some(v) => lua.to_value(&v),
                    None => Ok(default),
                }
            })?,
        )?;
        Ok(api)
    }

    fn build_log(&self, name: &str) -> mlua::Result<Table> {
        let log = self.lua.create_table()?;
        let target = format!("powder.mod.{name}");
        for (field, level) in [
            ("debug", log::Level::Debug),
            ("info", log::Level::Info),
            ("warn", log::Level::Warn),
            ("error", log::Level::Error),
        ] {
            let target = target.clone();
            log.set(
                field,
                self.lua.create_function(move |_, args: Variadic<Value>| {
                    let msg = args.iter().map(display_value).collect::<Vec<_>>().join(" ");
                    log::log!(target: target.as_str(), level, "{msg}");
                    Ok(())
                })?,
            )?;
        }
        Ok(log)
    }

    /// Fire an event at every mod that registered for it. `payload` reaches the handler as a Lua value
    /// (a table for an object). Handler errors are logged, not propagated.
    pub fn emit(&self, event: &str, payload: &Json) {
        let handlers = self.reg.lock().events.get(event).cloned().unwrap_or_default();
        if handlers.is_empty() {
            return;
        }
        let value = match self.lua.to_value(payload) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(target: "powder.mods", "event {event}: bad payload: {e}");
                return;
            }
        };
        for (who, func) in handlers {
            if let Err(e) = self.limited(MOD_HOOK_TIME_LIMIT, || func.call::<()>(value.clone())) {
                log::warn!(target: "powder.mods", "mod {who}: event {event} handler failed: {e}");
            }
        }
    }

    /// Offer an HTTP request to the mods. The first hook (by registration order) whose method and path glob
    /// match and that returns a non-nil response wins; `None` means no mod answered. `request` and the
    /// returned response are plain JSON the server shapes.
    pub fn before_http(&self, method: &str, path: &str, request: &Json) -> Option<Json> {
        let hooks: Vec<HttpHook> = {
            let reg = self.reg.lock();
            if reg.http.is_empty() {
                return None;
            }
            reg.http
                .iter()
                .filter(|h| (h.method == "*" || h.method.eq_ignore_ascii_case(method)) && glob_match(&h.glob, path))
                .cloned()
                .collect()
        };
        for h in hooks {
            let who = &h.mod_name;
            let req = match self.lua.to_value(request) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(target: "powder.mods", "mod {who}: before_http: bad request: {e}");
                    continue;
                }
            };
            match self.limited(MOD_HOOK_TIME_LIMIT, || h.func.call::<Value>(req)) {
                Ok(Value::Nil) => {}
                Ok(v) => match self.lua.from_value::<Json>(v) {
                    Ok(resp) => return Some(resp),
                    Err(e) => log::warn!(target: "powder.mods", "mod {who}: before_http: bad response: {e}"),
                },
                Err(e) => log::warn!(target: "powder.mods", "mod {who}: before_http {method} {path} failed: {e}"),
            }
        }
        None
    }

    /// Run a call into Lua, aborted (with an error) once it has run longer than `limit`.
    fn limited<R>(&self, limit: Duration, f: impl FnOnce() -> mlua::Result<R>) -> mlua::Result<R> {
        let tight = {
            let mut d = self.deadline.lock();
            d.until = Some(Instant::now() + limit);
            std::mem::take(&mut d.tight)
        };
        if tight {
            install_hook(&self.lua, self.deadline.clone(), HOOK_EVERY);
        }
        let out = f();
        self.deadline.lock().until = None;
        out
    }

    /// Offer a NEX RMC call to the mods. The hook gets the call context (JSON) and the raw request params as
    /// a Lua string; returning a Lua string supplies the response bytes, `nil` lets the server answer. The
    /// first mod to answer wins.
    pub fn before_rmc(&self, method: &str, ctx: &Json, params: &[u8]) -> Option<Vec<u8>> {
        let hooks = self.reg.lock().rmc.get(method).cloned().unwrap_or_default();
        if hooks.is_empty() {
            return None;
        }
        let ctx_val = self.lua.to_value(ctx).ok()?;
        let params_str = self.lua.create_string(params).ok()?;
        for h in hooks {
            let args = (ctx_val.clone(), params_str.clone());
            match self.limited(MOD_HOOK_TIME_LIMIT, || h.func.call::<Option<mlua::String>>(args)) {
                Ok(Some(s)) => return Some(s.as_bytes().to_vec()),
                Ok(None) => {}
                Err(e) => {
                    log::warn!(target: "powder.mods", "mod {}: before_rmc {method} failed: {e}", h.mod_name)
                }
            }
        }
        None
    }
}

const ABORTED: &str = "aborted: the mod ran past its time limit";

/// When the running call into Lua must have returned (None: none running), and whether the main thread's hook
/// was switched to every instruction (an overrun) and must be put back.
#[derive(Default)]
struct Deadline {
    until: Option<Instant>,
    tight: bool,
}

type SharedDeadline = Arc<Mutex<Deadline>>;

impl Deadline {
    fn over(&self) -> bool {
        self.until.is_some_and(|until| Instant::now() > until)
    }
}

/// The main thread's time check every `every` instructions; once over time, on every instruction (so a mod's
/// pcall can't catch the abort for long).
fn install_hook(lua: &Lua, deadline: SharedDeadline, every: u32) {
    lua.set_hook(HookTriggers::new().every_nth_instruction(every), move |lua, _| {
        if !deadline.lock().over() {
            return Ok(VmState::Continue);
        }
        if every != 1 {
            deadline.lock().tight = true;
            install_hook(lua, deadline.clone(), 1);
        }
        Err(mlua::Error::runtime(ABORTED))
    });
}

/// The mods' Lua state: the reduced standard library, the memory limit and the time limit's hook. -> (state,
/// `new_env`).
fn sandbox(deadline: SharedDeadline) -> mlua::Result<(Lua, Function)> {
    let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::OS | StdLib::COROUTINE;
    // SAFETY: `debug` is loaded only for SANDBOX's coroutine hooks and is out of the mods' reach (their globals are
    // new tables, the shared ones are cleared below); there is no `package` to load C modules, chunks are text.
    let lua = unsafe { Lua::unsafe_new_with(libs | StdLib::DEBUG, LuaOptions::default()) };
    lua.set_memory_limit(MOD_MEMORY_LIMIT)?;
    install_hook(&lua, deadline.clone(), HOOK_EVERY);
    let over = lua.create_function(move |_, ()| Ok(deadline.lock().over()))?;
    let new_env: Function = lua.load(SANDBOX.replace("ABORTED", &format!("{ABORTED:?}"))).set_name("=sandbox")
        .call((over, HOOK_EVERY))?;
    let globals = lua.globals();
    for name in ["debug", "io", "os", "package", "require", "dofile", "loadfile", "load"] {
        globals.raw_set(name, Value::Nil)?;
    }
    Ok((lua, new_env))
}

/// What the loaded mods registered: the loaded names, and for each event / RMC method the mods watching it,
/// plus every HTTP hook as (mod, method, glob).
#[derive(Clone, Debug, Default)]
pub struct ModReport {
    pub loaded: Vec<String>,
    pub events: Vec<(String, Vec<String>)>,
    pub http: Vec<(String, String, String)>,
    pub rmc: Vec<(String, Vec<String>)>,
    /// (mod, client files, DLLs to inject)
    pub client: Vec<(String, usize, Vec<String>)>,
}

/// A snapshot of the config a mod may read. Read-only: a mod changing this table changes nothing on the
/// server.
fn config_snapshot(config: &Config) -> Json {
    json!({
        "server": {
            "name": config.server.name,
            "public_host": config.server.public_host,
            "country": config.server.country,
            "region": config.server.region,
            "public": config.server.public,
        },
        "gameplay": config.gameplay,
        "mods": { "dir": config.mods.dir.display().to_string() },
    })
}

/// A mod found in the mods folder: its Lua entry (server part) and/or its folder (mod.toml, client/).
struct Found {
    name: String,
    entry: Option<PathBuf>,
    dir: Option<PathBuf>,
}

/// Mods in a folder, sorted by name: `<name>.lua`, or `<name>/` with `{mod,init}.lua` and/or `client/`; names
/// starting with `_` or `.` are skipped.
fn discover(dir: &Path) -> Vec<Found> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if fname.starts_with('_') || fname.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let lua = ["mod.lua", "init.lua"].iter().map(|c| path.join(c)).find(|p| p.is_file());
            if lua.is_some() || path.join("client").is_dir() {
                out.push(Found { name: fname, entry: lua, dir: Some(path) });
            }
        } else if let Some(stem) = fname.strip_suffix(".lua") {
            out.push(Found { name: stem.to_string(), entry: Some(path), dir: None });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A string for a Lua value in a log line (scalars plainly, anything else via its debug form).
fn display_value(v: &Value) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.to_string_lossy(),
        other => format!("{other:?}"),
    }
}

/// fnmatch-style glob on a path: `*` matches any run of characters, `?` matches one. No character classes.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn host_with(mods: &[(&str, &str)]) -> (tempfile::TempDir, ModHost) {
        let dir = tempfile::tempdir().unwrap();
        let mods_dir = dir.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        for (name, src) in mods {
            std::fs::write(mods_dir.join(format!("{name}.lua")), src).unwrap();
        }
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Arc::new(Repos::new(db));
        let mut config = Config::default();
        config.mods.dir = mods_dir;
        let host = ModHost::load(&config, repos);
        (dir, host)
    }

    #[test]
    fn glob() {
        assert!(glob_match("/v1/*", "/v1/profiles/123"));
        assert!(glob_match("*", "/anything"));
        assert!(glob_match("/v?/x", "/v1/x"));
        assert!(glob_match("/a/*/c", "/a/bbbb/c"));
        assert!(!glob_match("/v1/x", "/v1/y"));
        assert!(!glob_match("/a/*/c", "/a/b/d"));
        assert!(glob_match("ab*cd*", "abXcdYY"));
    }

    #[test]
    fn events_and_per_mod_storage() {
        let (_d, host) = host_with(&[(
            "counter",
            r#"return function(api)
                api.on("purchase", function(ev)
                    api.save("count", (api.load("count", 0)) + 1)
                    api.save("last", ev.offer_id)
                end)
            end"#,
        )]);
        assert_eq!(host.loaded(), &["counter".to_string()]);
        host.emit("purchase", &json!({"profile_id": "abc", "offer_id": "store_credits"}));
        host.emit("purchase", &json!({"profile_id": "abc", "offer_id": "store_heli"}));
        let count = host.repos.kv_get("mod:counter", "count").unwrap().unwrap();
        assert_eq!(count, json!(2));
        let last = host.repos.kv_get("mod:counter", "last").unwrap().unwrap();
        assert_eq!(last, json!("store_heli"));
    }

    #[test]
    fn before_rmc_answers_or_defers() {
        let (_d, host) = host_with(&[(
            "rmc",
            r#"return function(api)
                api.before_rmc("Proto::Answered_V1", function(ctx, params)
                    return "ok:" .. ctx.call_id
                end)
                api.before_rmc("Proto::Deferred_V1", function(ctx, params) return nil end)
            end"#,
        )]);
        let out = host.before_rmc("Proto::Answered_V1", &json!({"call_id": 7}), b"params");
        assert_eq!(out.as_deref(), Some(&b"ok:7"[..]));
        assert_eq!(host.before_rmc("Proto::Deferred_V1", &json!({"call_id": 1}), b""), None);
        assert_eq!(host.before_rmc("Proto::Unhooked_V1", &json!({}), b""), None);
    }

    #[test]
    fn before_http_matches_and_shapes_response() {
        let (_d, host) = host_with(&[(
            "http",
            r#"return function(api)
                api.before_http("GET", "/v1/ping*", function(req)
                    return { status = 200, body = "pong " .. req.path }
                end)
            end"#,
        )]);
        let resp = host.before_http("GET", "/v1/ping/42", &json!({"path": "/v1/ping/42"})).unwrap();
        assert_eq!(resp, json!({"status": 200, "body": "pong /v1/ping/42"}));
        // method and path that no hook matches
        assert_eq!(host.before_http("POST", "/v1/ping/42", &json!({})), None);
        assert_eq!(host.before_http("GET", "/v1/other", &json!({})), None);
    }

    #[test]
    fn a_broken_mod_is_skipped_others_load() {
        let (_d, host) = host_with(&[
            ("broken", "this is not lua )("),
            ("ok", "return function(api) api.save('ran', true) end"),
        ]);
        assert_eq!(host.loaded(), &["ok".to_string()]);
        assert_eq!(host.repos.kv_get("mod:ok", "ran").unwrap(), Some(json!(true)));
    }

    #[test]
    fn report_lists_registrations() {
        let (_d, host) = host_with(&[(
            "r",
            r#"return function(api)
                api.on("login", function() end)
                api.before_rmc("P::M_V1", function() end)
                api.before_http("GET", "/x/*", function() end)
            end"#,
        )]);
        let r = host.report();
        assert_eq!(r.loaded, vec!["r".to_string()]);
        assert_eq!(r.events, vec![("login".to_string(), vec!["r".to_string()])]);
        assert_eq!(r.rmc, vec![("P::M_V1".to_string(), vec!["r".to_string()])]);
        assert_eq!(r.http, vec![("r".to_string(), "GET".to_string(), "/x/*".to_string())]);
    }

    #[test]
    fn client_only_and_mixed_mods_publish_their_client_files() {
        let dir = tempfile::tempdir().unwrap();
        let mods_dir = dir.path().join("mods");
        std::fs::create_dir_all(mods_dir.join("hud/client")).unwrap();
        std::fs::write(mods_dir.join("hud/client/hud.dll"), b"MZ").unwrap();
        std::fs::create_dir_all(mods_dir.join("both/client")).unwrap();
        std::fs::write(mods_dir.join("both/client/data.bin"), b"d").unwrap();
        std::fs::write(mods_dir.join("both/mod.lua"), "return function(api) end").unwrap();
        std::fs::create_dir_all(mods_dir.join("empty")).unwrap(); // neither part: not a mod
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let mut config = Config::default();
        config.mods.dir = mods_dir;
        let host = ModHost::load(&config, Arc::new(Repos::new(db)));
        assert_eq!(host.loaded(), &["both".to_string()]);
        let r = host.report();
        assert_eq!(r.client, vec![("both".to_string(), 1, vec![]), ("hud".to_string(), 1, vec!["hud.dll".to_string()])]);
    }

    #[test]
    fn mods_get_a_reduced_standard_library_of_their_own() {
        let (_d, host) = host_with(&[
            ("a", r#"return function(api)
                string.marker = "a"
                local gone = {}
                for _, name in ipairs({"io", "require", "dofile", "loadfile", "package", "debug"}) do
                    if _G[name] ~= nil then gone[#gone + 1] = name end
                end
                for _, name in ipairs({"execute", "remove", "rename", "exit", "getenv", "tmpname"}) do
                    if os[name] ~= nil then gone[#gone + 1] = "os." .. name end
                end
                local co = coroutine.wrap(function(x) coroutine.yield(x + 1) end)
                api.save("seen", {
                    present = table.concat(gone, ","),
                    upper = ("abc"):upper(), time = type(os.time()), date = os.date("!%Y", 0), math = math.floor(2.5),
                    utf8 = utf8.char(72), co = co(1), strmeta = tostring(getmetatable("")),
                    text = load("return 1 + 1")(), binary = select(2, load(string.dump(function() end))) ~= nil,
                })
            end"#),
            ("b", r#"return function(api) api.save("marker", tostring(string.marker)) end"#),
        ]);
        assert_eq!(host.loaded(), &["a".to_string(), "b".to_string()]);
        let seen = host.repos.kv_get("mod:a", "seen").unwrap().unwrap();
        assert_eq!(seen, json!({"present": "", "upper": "ABC", "time": "number", "date": "1970", "math": 2,
                                "utf8": "H", "co": 2, "strmeta": "false", "text": 2, "binary": true}));
        // a's change to its string table is its own
        assert_eq!(host.repos.kv_get("mod:b", "marker").unwrap(), Some(json!("nil")));
    }

    #[test]
    fn a_hook_running_too_long_is_aborted() {
        let (_d, host) = host_with(&[(
            "slow",
            r#"return function(api)
                api.before_rmc("P::Loop_V1", function() while true do end end)
                api.before_rmc("P::Caught_V1", function() while true do pcall(function() while true do end end) end end)
                api.before_rmc("P::Coroutine_V1", function()
                    local co = coroutine.create(function() while true do end end)
                    coroutine.resume(co)
                    return "resumed"
                end)
                api.before_rmc("P::Memory_V1", function()
                    local t = {}
                    for i = 1, 1e9 do t[i] = ("x"):rep(1e6) .. i end
                end)
                api.before_rmc("P::Fine_V1", function() return "fine" end)
            end"#,
        )]);
        for method in ["P::Loop_V1", "P::Caught_V1", "P::Memory_V1"] {
            let started = Instant::now();
            assert_eq!(host.before_rmc(method, &json!({}), b""), None, "{method}");
            assert!(started.elapsed() < Duration::from_secs(5), "{method} took {:?}", started.elapsed());
        }
        // the coroutine's loop is aborted too (the hook then answers or is aborted itself)
        let started = Instant::now();
        host.before_rmc("P::Coroutine_V1", &json!({}), b"");
        assert!(started.elapsed() < Duration::from_secs(5));
        // the state still works afterwards
        assert_eq!(host.before_rmc("P::Fine_V1", &json!({}), b"").as_deref(), Some(&b"fine"[..]));
    }

    #[test]
    fn a_mod_that_never_returns_from_loading_is_skipped() {
        let (_d, host) = host_with(&[("hang", "while true do end"), ("ok", "return function(api) end")]);
        assert_eq!(host.loaded(), &["ok".to_string()]);
    }

    #[test]
    fn disabled_mods_do_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let mods_dir = dir.path().join("mods");
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join("a.lua"), "return function(api) end").unwrap();
        std::fs::write(mods_dir.join("b.lua"), "return function(api) end").unwrap();
        std::fs::write(mods_dir.join("_skip.lua"), "error('should never run')").unwrap();
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Arc::new(Repos::new(db));
        let mut config = Config::default();
        config.mods.dir = mods_dir;
        config.mods.enabled = vec!["a".into()];
        let host = ModHost::load(&config, repos);
        assert_eq!(host.loaded(), &["a".to_string()]);
    }
}
