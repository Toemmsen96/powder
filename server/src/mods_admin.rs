//! The mods' admin API (`api.admin`) and JSON helpers (`api.json`): what a server mod needs to build an admin
//! tool, e.g. the admin page of `mods/admin/`.
//!
//!   api.json.decode(text) -> value | nil, err       api.json.encode(value) -> text (Python's json.dumps form)
//!   api.admin.configured                            whether [server] admins lists anyone (set at load)
//!   api.admin.token()                               a random URL-safe token (32 bytes), e.g. for a cookie
//!   api.admin.sign_in(email, password, address)     -> {profile_id, name} of an ADMIN account | nil, reason;
//!                                                   counts against the sign-in rate limit like Connect's
//!   api.admin.is_admin(profile_id)                  -> bool
//!   api.admin.players()                             -> [{profile_id, name, email, level, credits, created_at,
//!                                                   game, connect, admin}] (game/connect: online right now)
//!   api.admin.inventory(profile_id)                 -> {credits, items = [{item_id, quantity, last_modified}]}
//!   api.admin.grant(profile_id, item, amount)       -> {item_id, before, after} | nil, reason ("credits" ok)
//!   api.admin.stats(profile_id)                     -> [{key, value, double}]
//!   api.admin.set_stat(profile_id, key, value)      an integer stays an int64 stat, a float is a double
//!   api.admin.reset(profile_id)                     wipe the player (an owning account is kept)
//!   api.admin.notify(profile_id, type, content)     -> sockets reached: a notification on the player's websockets
//!   api.admin.chat(from, to, text)                  -> message | nil, reason: a Ubisoft Connect chat message
//!   api.admin.access()                              -> {whitelist, rules = [{kind, value, name, reason, created_at,
//!                                                   expires, by}]}: bans and the whitelist (services/access.rs)
//!   api.admin.ban(profile_id, reason, seconds, by)  -> true | nil, reason; seconds nil = until unbanned; kicks
//!   api.admin.unban(profile_id, by)                 -> whether there was a ban
//!   api.admin.ban_ip(address, reason, seconds, by)  -> the range banned ("192.0.2.0/24") | nil, reason; kicks
//!   api.admin.unban_ip(address, by)                 -> whether there was a ban | nil, reason
//!   api.admin.kick(profile_id, reason)              -> whether they were online (they may sign in again)
//!   api.admin.set_whitelist(on, by)                 while on, only its players and the admins get in; kicks
//!   api.admin.allow(profile_id, by), disallow(profile_id): on or off the whitelist
//!   (players() also says `banned` and `allowed` = on the whitelist)
//!
//! Admins are the profile ids in `[server] admins` (ids, not emails or names: players can change those). Mods
//! are the host's own code, so every mod gets this table; the server only checks the arguments. The services
//! are attached after the mods load (`ModHost::attach`): calls made during a mod's setup fail.

use crate::constants::{CONNECT_SPACE_ID, CREDITS_ITEM};
use crate::services::stats::{stat_value, Stat, LEVEL_KEY, PROFILE_GROUP, STAT_DOUBLE};
use crate::mods::SharedDeadline;
use crate::services::access::{ALLOW_PLAYER, BAN_PLAYER};
use crate::services::Services;
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde_json::{json, Value as Json};
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock, Weak};

/// Where the mods find the services once the server built them.
pub type ServicesSlot = Arc<OnceLock<Weak<Services>>>;

fn services(slot: &ServicesSlot) -> mlua::Result<Arc<Services>> {
    slot.get().and_then(Weak::upgrade)
        .ok_or_else(|| mlua::Error::external("api.admin is not available yet (during setup, or the server stopped)"))
}

fn ext(e: anyhow::Error) -> mlua::Error {
    mlua::Error::external(format!("{e:#}"))
}

/// A profile id as the database keeps it (lower-case UUID), or None.
fn profile_id(raw: &str) -> Option<String> {
    let pid = raw.trim().to_lowercase();
    uuid::Uuid::parse_str(&pid).ok().filter(|u| u.hyphenated().to_string() == pid).map(|_| pid)
}

/// An existing player's id, else the reason as the second return value.
fn known(s: &Services, raw: &str) -> mlua::Result<Result<String, String>> {
    let Some(pid) = profile_id(raw) else { return Ok(Err(format!("not a profile id: {raw:?}"))) };
    Ok(match s.repos.player(&pid).map_err(ext)? {
        Some(_) => Ok(pid),
        None => Err(format!("no player {pid}")),
    })
}

pub fn is_admin(s: &Services, pid: &str) -> bool {
    profile_id(pid).is_some_and(|pid| s.config.server.admins.iter().any(|a| a.trim().eq_ignore_ascii_case(&pid)))
}

type Refusal = (Value, Option<String>);

fn refused(reason: String) -> mlua::Result<Refusal> {
    Ok((Value::Nil, Some(reason)))
}

fn ok(lua: &Lua, v: &Json) -> mlua::Result<Refusal> {
    Ok((lua.to_value(v)?, None))
}

pub(crate) fn json_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("decode", lua.create_function(|lua, text: mlua::String| {
        match serde_json::from_slice::<Json>(&text.as_bytes()) {
            Ok(v) => Ok((lua.to_value(&v)?, None)),
            Err(e) => Ok((Value::Nil, Some(e.to_string()))),
        }
    })?)?;
    t.set("encode", lua.create_function(|lua, v: Value| Ok(crate::util::dumps(&lua.from_value::<Json>(v)?)))?)?;
    Ok(t)
}

pub(crate) fn admin_table(lua: &Lua, slot: ServicesSlot, configured: bool, deadline: SharedDeadline) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("configured", configured)?;
    t.set("token", lua.create_function(|_, ()| Ok(crate::util::token_urlsafe(32)))?)?;

    let sl = slot.clone();
    t.set("sign_in", lua.create_function(move |lua, (email, password, address): (String, String, String)| {
        let s = services(&sl)?;
        let email = email.trim().to_lowercase();
        // counted before the password is hashed (a success takes it back): concurrent guesses can't slip past
        if !s.login_failures.attempt(&address, &email) {
            return refused("too many failed sign-ins, try again in a few minutes".into());
        }
        // seconds of scrypt (more while other sign-ins hash) are the server's time, not the mod's
        let account = match crate::mods::uncounted(&deadline, || s.accounts.authenticate(&email, &password)) {
            Ok(a) => a,
            Err(_) => {
                log::warn!(target: "powder.mods", "admin sign-in from {address} refused: invalid credentials");
                return refused("invalid email or password".into());
            }
        };
        s.login_failures.succeeded(&address, &email);
        if !is_admin(&s, &account.profile_id) {
            log::warn!(target: "powder.mods", "admin sign-in from {address} refused: {} is not an admin",
                       account.profile_id);
            return refused("this account is not a server admin".into());
        }
        let name = s.repos.player(&account.profile_id).map_err(ext)?.map(|p| p.name).unwrap_or_default();
        log::info!(target: "powder.mods", "admin {name} signed in from {address}");
        ok(lua, &json!({"profile_id": account.profile_id, "name": name}))
    })?)?;

    let sl = slot.clone();
    t.set("is_admin", lua.create_function(move |_, pid: String| Ok(is_admin(&*services(&sl)?, &pid)))?)?;

    let sl = slot.clone();
    t.set("players", lua.create_function(move |lua, ()| {
        let s = services(&sl)?;
        let game: BTreeSet<String> = s.sessions.players().iter().filter(|x| x.channels.nex().is_some())
            .map(|x| x.profile_id().to_lowercase()).collect();
        let connect: BTreeSet<String> = s.connect_clients.profile_ids().iter().map(|p| p.to_lowercase()).collect();
        let rules = s.accounts.access.list().map_err(ext)?;
        let of = |kind: &str| -> BTreeSet<String> { rules.iter().filter(|r| r.kind == kind).map(|r| r.value.clone()).collect() };
        let (banned, allowed) = (of(BAN_PLAYER), of(ALLOW_PLAYER));
        let mut out = vec![];
        for p in s.repos.players().map_err(ext)? {
            let pid = &p.profile_id;
            let level = s.stats.get(pid, LEVEL_KEY, PROFILE_GROUP).map_err(ext)?.map(|l| l.to_string());
            let email = s.repos.account_by_profile(pid).map_err(ext)?.map(|a| a.email);
            out.push(json!({"profile_id": pid, "name": p.name, "email": email, "level": level,
                            "credits": s.economy.credits(pid).map_err(ext)?, "created_at": p.created_at,
                            "game": game.contains(pid), "connect": connect.contains(pid), "admin": is_admin(&s, pid),
                            "banned": banned.contains(pid), "allowed": allowed.contains(pid)}));
        }
        lua.to_value(&out)
    })?)?;

    let sl = slot.clone();
    t.set("inventory", lua.create_function(move |lua, pid: String| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return refused(why),
        };
        let items: Vec<Json> = s.repos.inventory(&pid).map_err(ext)?.into_iter()
            .map(|r| json!({"item_id": r.item_id, "quantity": r.quantity, "last_modified": r.last_modified}))
            .collect();
        ok(lua, &json!({"credits": s.economy.credits(&pid).map_err(ext)?, "items": items}))
    })?)?;

    let sl = slot.clone();
    t.set("grant", lua.create_function(move |lua, (pid, item, amount): (String, String, i64)| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return refused(why),
        };
        let item = item.trim();
        let item = if item.eq_ignore_ascii_case("credits") { CREDITS_ITEM } else { item };
        if item.is_empty() || item.len() > 64 {
            return refused("no item id".into());
        }
        match s.economy.grant(&pid, item, amount, "admin").map_err(ext)? {
            Ok(c) => {
                log::info!(target: "powder.mods", "admin grant {pid}: {} {:+} -> {}", c.item_id, c.delta, c.after);
                ok(lua, &json!({"item_id": c.item_id, "before": c.before, "after": c.after}))
            }
            Err(e) => refused(e.message),
        }
    })?)?;

    let sl = slot.clone();
    t.set("stats", lua.create_function(move |lua, pid: String| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return refused(why),
        };
        let rows: Vec<Json> = s.stats.group(&pid, PROFILE_GROUP).map_err(ext)?.iter().map(|r| match stat_value(r) {
            Stat::Int(i) => json!({"key": r.key, "value": i, "double": false}),
            Stat::Double(d) => json!({"key": r.key, "value": d, "double": r.kind == STAT_DOUBLE}),
        }).collect();
        ok(lua, &Json::Array(rows))
    })?)?;

    let sl = slot.clone();
    t.set("set_stat", lua.create_function(move |_, (pid, key, value): (String, String, Value)| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return Ok((false, Some(why))),
        };
        let value = match value {
            Value::Integer(i) => Stat::Int(i),
            Value::Number(n) if n.is_finite() => Stat::Double(n),
            _ => return Ok((false, Some("the value must be a number".into()))),
        };
        if key.is_empty() || key.len() > 128 {
            return Ok((false, Some("no stat key".into())));
        }
        s.stats.set(&pid, &key, value, PROFILE_GROUP).map_err(ext)?;
        log::info!(target: "powder.mods", "admin set stat {pid}: {key} = {value}");
        Ok((true, None))
    })?)?;

    let sl = slot.clone();
    t.set("reset", lua.create_function(move |_, pid: String| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return Ok((false, Some(why))),
        };
        s.reset(&pid).map_err(ext)?;
        s.sessions.drop_player(&pid);
        log::warn!(target: "powder.mods", "admin reset player {pid}");
        Ok((true, None))
    })?)?;

    let sl = slot.clone();
    t.set("notify", lua.create_function(move |lua, (pid, kind, content): (String, String, Value)| {
        let s = services(&sl)?;
        let Some(pid) = profile_id(&pid) else { return Ok(0) };
        Ok(s.sessions.push_ws(&pid, &kind, lua.from_value(content)?, serde_json::Map::new()))
    })?)?;

    let sl = slot.clone();
    t.set("chat", lua.create_function(move |lua, (from, to, text): (String, String, String)| {
        let s = services(&sl)?;
        let (from, to) = match (known(&s, &from)?, known(&s, &to)?) {
            (Ok(f), Ok(t)) => (f, t),
            (Err(why), _) | (_, Err(why)) => return refused(why),
        };
        let dm = s.channels.create(&from, CONNECT_SPACE_ID, &json!({"channel": {"type": "DM"}, "members": [to]}));
        let Some(id) = dm.get("id").and_then(Json::as_str) else { return refused("no channel".into()) };
        match s.channels.post(&from, id, &json!({"type": "TEXT_MESSAGE", "textMessage": text})) {
            Some(m) => ok(lua, &m),
            None => refused("could not post".into()),
        }
    })?)?;
    access_functions(lua, &t, slot)?;
    Ok(t)
}

/// A duration from Lua: nil = until removed, else whole seconds > 0.
fn seconds(v: Option<i64>) -> Result<Option<i64>, String> {
    match v {
        None => Ok(None),
        Some(n) if n > 0 => Ok(Some(n)),
        Some(_) => Err("the duration must be a number of seconds above 0, or nil".into()),
    }
}

fn by(who: Option<String>) -> String {
    who.map(|w| crate::services::access::clean_reason(&w)).filter(|w| !w.is_empty()).unwrap_or_else(|| "a mod".into())
}

/// Kick whoever a change keeps out, now (the server's sweep would within seconds).
fn enforce(s: &Services) {
    if let Err(e) = s.enforce_access() {
        log::warn!(target: "powder.mods", "applying the access rules failed: {e:#}");
    }
}

fn access_functions(lua: &Lua, t: &Table, slot: ServicesSlot) -> mlua::Result<()> {
    let sl = slot.clone();
    t.set("access", lua.create_function(move |lua, ()| {
        lua.to_value(&services(&sl)?.accounts.access.summary().map_err(ext)?)
    })?)?;

    let sl = slot.clone();
    t.set("ban", lua.create_function(move |lua, (pid, reason, secs, who): (String, Option<String>, Option<i64>, Option<String>)| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return refused(why),
        };
        let secs = match seconds(secs) {
            Ok(v) => v,
            Err(why) => return refused(why),
        };
        match s.accounts.access.ban_player(&pid, reason.as_deref().unwrap_or(""), secs, &by(who)).map_err(ext)? {
            Ok(()) => {
                enforce(&s);
                ok(lua, &json!(true))
            }
            Err(why) => refused(why),
        }
    })?)?;

    let sl = slot.clone();
    t.set("unban", lua.create_function(move |_, (pid, who): (String, Option<String>)| {
        let s = services(&sl)?;
        let Some(pid) = profile_id(&pid) else { return Ok(false) };
        s.accounts.access.unban_player(&pid, &by(who)).map_err(ext)
    })?)?;

    let sl = slot.clone();
    t.set("ban_ip", lua.create_function(move |lua, (address, reason, secs, who): (String, Option<String>, Option<i64>, Option<String>)| {
        let s = services(&sl)?;
        let secs = match seconds(secs) {
            Ok(v) => v,
            Err(why) => return refused(why),
        };
        match s.accounts.access.ban_address(&address, reason.as_deref().unwrap_or(""), secs, &by(who)).map_err(ext)? {
            Ok(range) => {
                enforce(&s);
                ok(lua, &json!(range))
            }
            Err(why) => refused(why),
        }
    })?)?;

    let sl = slot.clone();
    t.set("unban_ip", lua.create_function(move |lua, (address, who): (String, Option<String>)| {
        let s = services(&sl)?;
        match s.accounts.access.unban_address(&address, &by(who)).map_err(ext)? {
            Ok(had) => ok(lua, &json!(had)),
            Err(why) => refused(why),
        }
    })?)?;

    let sl = slot.clone();
    t.set("kick", lua.create_function(move |_, (pid, reason): (String, Option<String>)| {
        let s = services(&sl)?;
        let Some(pid) = profile_id(&pid) else { return Ok(false) };
        let reason = crate::services::access::clean_reason(reason.as_deref().unwrap_or(""));
        // a Connect without a session (signed in before a restart) is closed by the kick notice too
        let connect = s.connect_clients.profile_ids().iter().any(|p| p.eq_ignore_ascii_case(&pid));
        let why = if reason.is_empty() { "kicked by an admin".to_string() } else { reason };
        Ok(s.accounts.access.kick(&pid, &why) || connect)
    })?)?;

    let sl = slot.clone();
    t.set("set_whitelist", lua.create_function(move |_, (on, who): (bool, Option<String>)| {
        let s = services(&sl)?;
        s.accounts.access.set_whitelist(on, &by(who)).map_err(ext)?;
        enforce(&s);
        Ok(true)
    })?)?;

    let sl = slot.clone();
    t.set("allow", lua.create_function(move |_, (pid, who): (String, Option<String>)| {
        let s = services(&sl)?;
        let pid = match known(&s, &pid)? {
            Ok(p) => p,
            Err(why) => return Ok((false, Some(why))),
        };
        s.accounts.access.allow(&pid, &by(who)).map_err(ext)?;
        Ok((true, None))
    })?)?;

    t.set("disallow", lua.create_function(move |_, pid: String| {
        let s = services(&slot)?;
        let Some(pid) = profile_id(&pid) else { return Ok(false) };
        let had = s.accounts.access.disallow(&pid).map_err(ext)?;
        enforce(&s);
        Ok(had)
    })?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The admin mod as shipped (mods/admin/mod.lua) behind the HTTPS pipeline.
    use crate::config::Config;
    use crate::https::testing::{body, header, state_with};
    use crate::https::{handle, HttpState, Req};
    use crate::mods::ModHost;
    use bytes::Bytes;
    use hyper::header::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::{json, Value};

    const HOST: &str = "127.0.0.1:18443";

    const ADMIN: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const RIDER: &str = "bbbbbbbb-0000-4000-8000-000000000002";

    /// A server with the shipped admin mod and two accounts (Boss, an admin if `admin`, and Rider).
    fn server(admin: bool) -> HttpState {
        let mut config = Config::default();
        config.mods.dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mods");
        config.mods.enabled = vec!["admin".into()];
        if admin {
            config.server.admins = vec![ADMIN.to_uppercase()];
        }
        let db = std::sync::Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let host = ModHost::load(&config, std::sync::Arc::new(crate::repos::Repos::new(db)));
        let st = state_with(config, vec![], Some(host));
        for (email, name, pid) in [("admin@example.com", "Boss", ADMIN), ("rider@example.com", "Rider", RIDER)] {
            crate::services::seed::create_player(&st.config, &st.repos, pid, Some(name), None).unwrap();
            let acc = st.accounts.register(email, "correct horse", &format!("{name}2")).unwrap();
            st.accounts.link(&acc, pid).unwrap();
        }
        st.mods.as_ref().unwrap().lock().attach(&st.services);
        st
    }

    fn req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Req {
        let mut map = HeaderMap::new();
        map.insert("host", HeaderValue::from_static(HOST));
        for (k, v) in headers {
            map.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        Req { method: method.into(), host: "127.0.0.1".into(), path_qs: path.into(), headers: map,
              body: Bytes::from(body.to_string()), peer: "127.0.0.1:5000".parse().unwrap() }
    }

    fn post(st: &HttpState, path: &str, cookie: &str, body: Value) -> crate::https::Reply {
        handle(st, &req("POST", path, &[("x-powder-admin", "1"), ("origin", &format!("https://{HOST}")),
                                         ("cookie", cookie), ("content-type", "application/json")], &body.to_string()))
    }

    fn sign_in(st: &HttpState, email: &str) -> crate::https::Reply {
        post(st, "/_powder/admin/api/login", "", json!({"email": email, "password": "correct horse"}))
    }

    #[test]
    fn page_and_sign_in() {
        let (st, admin) = (server(true), ADMIN);
        let page = handle(&st, &req("GET", "/_powder/admin", &[], ""));
        assert_eq!((page.status, header(&page, "content-type")), (200, Some("text/html; charset=utf-8")));
        assert!(header(&page, "content-security-policy").unwrap().contains("frame-ancestors 'none'"));
        assert_eq!(handle(&st, &req("GET", "/_powder/admin/app.js", &[], "")).status, 200);
        // not from the page (no header): refused before any password check
        let r = handle(&st, &req("POST", "/_powder/admin/api/login", &[], r#"{"email": "admin@example.com"}"#));
        assert_eq!(r.status, 403);
        assert_eq!(post(&st, "/_powder/admin/api/login", "", json!({"email": "admin@example.com", "password": "nope"}))
                       .status, 401);
        let r = sign_in(&st, "rider@example.com");
        assert_eq!((r.status, body(&r)["error"].as_str()), (401, Some("this account is not a server admin")));
        let r = sign_in(&st, "admin@example.com");
        assert_eq!(r.status, 200);
        let cookie = header(&r, "set-cookie").unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("SameSite=Strict"));
        let cookie = cookie.split(';').next().unwrap().to_string();
        let players = handle(&st, &req("GET", "/_powder/admin/api/players", &[("cookie", &cookie)], ""));
        let b = body(&players);
        assert_eq!(b["me"]["profile_id"], admin);
        assert!(b["players"].as_array().unwrap().iter().any(|p| p["name"] == "Rider" && p["admin"] == false));
        assert!(b["players"].as_array().unwrap().iter().any(|p| p["admin"] == true && p["profile_id"] == ADMIN));
        // without the cookie, or a made-up one
        assert_eq!(handle(&st, &req("GET", "/_powder/admin/api/players", &[], "")).status, 401);
        assert_eq!(handle(&st, &req("GET", "/_powder/admin/api/players", &[("cookie", "powder_admin=x")], "")).status, 401);
        // signed out: the cookie is dead
        assert_eq!(post(&st, "/_powder/admin/api/logout", &cookie, json!({})).status, 200);
        assert_eq!(handle(&st, &req("GET", "/_powder/admin/api/players", &[("cookie", &cookie)], "")).status, 401);
    }

    #[test]
    fn admin_actions() {
        let (st, rider) = (server(true), RIDER);
        let r = sign_in(&st, "admin@example.com");
        let cookie = header(&r, "set-cookie").unwrap().split(';').next().unwrap().to_string();
        let before = st.economy.credits(rider).unwrap();
        let r = post(&st, "/_powder/admin/api/grant", &cookie, json!({"id": rider, "item": "credits", "amount": 500}));
        assert_eq!(body(&r)["result"]["after"].as_i64(), Some(before + 500));
        let r = post(&st, "/_powder/admin/api/grant", &cookie, json!({"id": rider, "item": "credits", "amount": -(before + 501)}));
        assert_eq!(r.status, 400, "never below zero");
        let r = post(&st, "/_powder/admin/api/grant", &cookie, json!({"id": "../../x", "item": "credits", "amount": 1}));
        assert_eq!(r.status, 400);
        let r = post(&st, "/_powder/admin/api/stat", &cookie, json!({"id": rider, "key": crate::services::stats::LEVEL_KEY,
                                                                      "value": "42", "double": false}));
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let level = st.stats.get(rider, crate::services::stats::LEVEL_KEY, crate::services::stats::PROFILE_GROUP).unwrap();
        assert_eq!(level.map(|l| l.to_string()).as_deref(), Some("42"));
        let r = handle(&st, &req("GET", &format!("/_powder/admin/api/player?id={rider}"), &[("cookie", &cookie)], ""));
        assert!(body(&r)["stats"].as_array().unwrap().iter().any(|s| s["value"] == 42));
        // a message to everyone online
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let session = st.sessions.issue(st.accounts.player(rider).unwrap(), "10.0.0.9", "");
        session.channels.add_websocket(tx);
        session.channels.set_nex(Some(std::sync::Arc::new(NoNex)));
        let r = post(&st, "/_powder/admin/api/broadcast", &cookie, json!({"text": "restart in 5 minutes"}));
        assert_eq!(body(&r)["result"]["websockets"], 1);
        let pushed = rx.try_recv().unwrap();
        assert_eq!((pushed["notificationType"].as_str(), pushed["content"]["message"].as_str()),
                   (Some("POWDER_ADMIN_MESSAGE"), Some("restart in 5 minutes")));
        // another site cannot use the admin's cookie
        let r = handle(&st, &req("POST", "/_powder/admin/api/grant", &[("x-powder-admin", "1"), ("cookie", &cookie),
                                                                        ("origin", "https://evil.example")],
                                 &json!({"id": rider, "item": "credits", "amount": 1}).to_string()));
        assert_eq!(r.status, 403);
        let r = post(&st, "/_powder/admin/api/reset", &cookie, json!({"id": rider}));
        assert_eq!(r.status, 200);
        assert_eq!(st.economy.credits(rider).unwrap(), st.config.gameplay.starting_credits);
    }

    #[test]
    fn bans_kicks_and_the_whitelist() {
        let (st, rider) = (server(true), RIDER);
        let r = sign_in(&st, "admin@example.com");
        let cookie = header(&r, "set-cookie").unwrap().split(';').next().unwrap().to_string();
        let get = |path: &str| body(&handle(&st, &req("GET", path, &[("cookie", &cookie)], "")));
        let session = st.sessions.issue(st.accounts.player(rider).unwrap(), "10.0.0.9", "");
        let r = post(&st, "/_powder/admin/api/kick", &cookie, json!({"id": rider, "reason": "be nice"}));
        assert_eq!(body(&r)["result"]["online"], true);
        assert!(st.sessions.by_token(&session.ticket).is_none(), "the session is gone");
        assert_eq!(body(&post(&st, "/_powder/admin/api/kick", &cookie, json!({"id": rider})))["result"]["online"], false);

        let session = st.sessions.issue(st.accounts.player(rider).unwrap(), "10.0.0.9", "");
        let r = post(&st, "/_powder/admin/api/ban", &cookie, json!({"id": rider, "reason": "griefing", "seconds": 3600}));
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        assert!(st.sessions.by_token(&session.ticket).is_none(), "kicked at once");
        let access = get("/_powder/admin/api/access");
        assert_eq!((access["rules"][0]["kind"].as_str(), access["rules"][0]["name"].as_str()),
                   (Some("ban_player"), Some("Rider")));
        assert!(access["rules"][0]["by"].as_str().is_some_and(|by| by.starts_with("Boss")), "the admin's name");
        assert!(access["rules"][0]["expires"].is_i64());
        assert!(get("/_powder/admin/api/players")["players"].as_array().unwrap().iter()
                    .any(|p| p["profile_id"] == rider && p["banned"] == true));
        let r = post(&st, "/_powder/admin/api/ban", &cookie, json!({"id": ADMIN}));
        assert_eq!(r.status, 400, "not an admin");
        assert_eq!(body(&post(&st, "/_powder/admin/api/unban", &cookie, json!({"id": rider})))["result"], true);

        let r = post(&st, "/_powder/admin/api/ban_ip", &cookie, json!({"address": "198.51.100.7/24"}));
        assert_eq!(body(&r)["result"], "198.51.100.0/24");
        assert_eq!(post(&st, "/_powder/admin/api/ban_ip", &cookie, json!({"address": "nope"})).status, 400);
        assert_eq!(body(&post(&st, "/_powder/admin/api/unban_ip", &cookie, json!({"address": "198.51.100.0/24"})))
                       ["result"], true);

        let session = st.sessions.issue(st.accounts.player(rider).unwrap(), "10.0.0.9", "");
        assert_eq!(post(&st, "/_powder/admin/api/whitelist", &cookie, json!({"on": true})).status, 200);
        assert!(st.sessions.by_token(&session.ticket).is_none(), "not on the whitelist: kicked");
        assert_eq!(get("/_powder/admin/api/access")["whitelist"], true);
        assert_eq!(post(&st, "/_powder/admin/api/allow", &cookie, json!({"id": rider})).status, 200);
        assert_eq!(st.accounts.access.check(rider, "10.0.0.9").unwrap(), None);
        assert_eq!(post(&st, "/_powder/admin/api/whitelist", &cookie, json!({"on": false})).status, 200);
        // the admin's own page keeps working throughout (admins pass the whitelist)
        assert_eq!(handle(&st, &req("GET", "/_powder/admin/api/players", &[("cookie", &cookie)], "")).status, 200);
    }

    struct NoNex;
    impl crate::core::sessions::NexChannel for NoNex {
        fn send_request(&self, _: &str, _: &str, _: &[u8], _: Option<u32>) -> u32 {
            0
        }
    }

    #[test]
    fn off_without_admins() {
        let st = server(false);
        assert_ne!(handle(&st, &req("GET", "/_powder/admin", &[], "")).how, "mod");
        assert_ne!(sign_in(&st, "admin@example.com").how, "mod");
    }
}
