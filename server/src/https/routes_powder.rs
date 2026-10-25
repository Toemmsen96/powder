//! /_powder/: the server's own endpoints (not Ubisoft's), from routes_powder.py, plus the client mods. Used by the
//! launcher. The status needs no sign-in and shows player names and the address of every connected game, which
//! is fine for a server meant for a local network of friends; don't expose it to the internet. The account
//! calls carry the account's email and password.
//!
//!   GET /_powder/status          who is connected: Ubisoft Connect (demux) clients and game (NEX) sessions, and
//!                                the caller's address as the server sees it ("you": a launcher finds its own PC)
//!   GET /_powder/account/new     whether registering takes over the existing player
//!   POST /_powder/account/register|login|email|password|name   accounts, for the launcher (JSON; each call
//!                                carries email and password, failed ones are rate-limited per address)
//!   GET /_powder/ca.pem          the local CA certificate (public part), for clients that must trust it
//!   GET /_powder/overlay/powder.js   the Powder part of Ubisoft Connect's in-game overlay (builtin/connect_ui.rs)
//!   GET /_powder/mods[/<name>/<path>]   the mods' client files (client_mods.rs)

use super::{Ctx, Reply, Route};
use crate::builtin::connect_ui;
use crate::client_mods;
use crate::repos::AccountRow;
use crate::services::accounts::{AccountService, Error as AccountError, Result as AccountResult};
use crate::util::unquote_path;
use serde_json::{json, Map, Value};

/// Registrations count like failed sign-ins (hashing is slow), under this key instead of an email.
const REGISTER_KEY: &str = "#register";

pub fn routes() -> Vec<Route> {
    vec![
        Route::new("GET", "/_powder/status", status),
        Route::new("GET", "/_powder/account/new", account_new),
        Route::new("POST", "/_powder/account/register", account_register),
        Route::new("POST", "/_powder/account/login", account_login),
        Route::new("POST", "/_powder/account/email", account_email),
        Route::new("POST", "/_powder/account/password", account_password),
        Route::new("POST", "/_powder/account/name", account_name),
        Route::new("GET", "/_powder/ca.pem", ca_pem),
        Route::new("GET", connect_ui::POWDER_PART_PATH, overlay_part),
        Route::new("GET", client_mods::INDEX_PATH, client_mod_index),
        Route::new("GET", "/_powder/mods/{rest:.+}", client_mod_file),
    ]
}

fn status(c: &Ctx) -> Reply {
    match status_body(c) {
        Ok(body) => Reply::json(&body, "powder-status", 200),
        Err(e) => Reply::internal_error(&e),
    }
}

fn status_body(c: &Ctx) -> anyhow::Result<Value> {
    let st = c.state;
    let connect: Vec<Value> = st.connect_clients.snapshot().into_iter().filter(|cl| cl.profile_id.is_some())
        .map(|cl| json!({"player": cl.player_name, "peer": cl.peer, "services": cl.services})).collect();
    // Python lists its sessions by player in first-login order; ours come from a map, so oldest session first.
    // The NEX connection does not know its address here: the session's (the PC it signed in from) stands in.
    let mut sessions: Vec<_> = st.sessions.players().into_iter().filter(|s| s.channels.nex().is_some()).collect();
    sessions.sort_by(|a, b| a.created.total_cmp(&b.created));
    let game: Vec<Value> = sessions.iter()
        .map(|s| json!({"player": s.player.name, "nex": true, "peer": s.client_ip})).collect();
    Ok(json!({"server": "powder", "version": crate::constants::version(), "you": c.req.client_ip(),
              "public_host": st.config.server.public_host, "accounts": st.accounts.required()?,
              "players": st.repos.players()?.len(), "gamedata": !st.assets.is_empty(),
              "connect": connect, "game": game}))
}

/// The local CA's certificate (the one `run` creates in <data>/tls).
fn ca_pem(c: &Ctx) -> Reply {
    let path = c.state.config.data_dir().join("tls").join("ca.pem");
    match std::fs::read(&path) {
        Ok(pem) => Reply::new(200, "application/x-pem-file", pem, "powder-ca"),
        Err(_) => Reply::new(404, "text/plain; charset=utf-8", b"no CA".to_vec(), "powder-ca"),
    }
}

/// The Powder part of Ubisoft Connect's in-game overlay; the overlay loads it cross-origin, like Ubisoft's parts
/// from their CDN.
fn overlay_part(_: &Ctx) -> Reply {
    let mut r = Reply::new(200, "application/javascript; charset=utf-8", connect_ui::POWDER_PART_JS.as_bytes().to_vec(),
                           "powder-overlay");
    r.headers.insert(0, ("Access-Control-Allow-Origin".into(), "*".into()));
    r
}

// ---- accounts (the launcher registers and manages them) -----------------------------------------------------
// JSON bodies; every call that reads or changes an account carries its email and current password, so the
// launcher needs no session of its own. Bodies are never logged (passwords).

/// What a call does with the account once its email and password are checked; -> the account to show.
type Change = fn(&AccountService, AccountRow, &Map<String, Value>) -> AccountResult<AccountRow>;

/// A body field as Python's `str(body.get(key, ""))`.
fn field(body: &Map<String, Value>, key: &str) -> String {
    match body.get(key) {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "None".into(),
        Some(Value::Bool(b)) => (if *b { "True" } else { "False" }).into(),
        Some(Value::Number(n)) if n.is_f64() => crate::util::py_float(n.as_f64().unwrap_or_default()),
        Some(v) => v.to_string(),
    }
}

fn error(message: &str, status: u16) -> Reply {
    Reply::json(&json!({"error": message}), "powder-account", status)
}

/// `change` None: a registration.
fn account_call(c: &Ctx, change: Option<Change>) -> Reply {
    let (st, ip) = (c.state, c.req.client_ip());
    let limiter = &st.login_failures;
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&c.req.body) else {
        return error("bad request", 400);
    };
    let email = field(&body, "email");
    let key = if change.is_none() { REGISTER_KEY } else { email.as_str() };
    if limiter.blocked(&ip, key) {
        return error("Too many attempts, try again in a few minutes.", 429);
    }
    let accounts = &st.accounts;
    let result = match change {
        None => {
            limiter.failed(&ip, REGISTER_KEY);
            accounts.register(&email, &field(&body, "password"), &field(&body, "name"))
        }
        Some(change) => accounts.authenticate(&email, &field(&body, "password")).and_then(|account| {
            limiter.succeeded(&ip, &email);
            change(accounts, account, &body)
        }),
    };
    match result.and_then(|account| accounts.info(&account).map_err(AccountError::from)) {
        Ok(info) => Reply::json(&info, "powder-account", 200),
        Err(AccountError::Auth(_)) => {
            limiter.failed(&ip, &email);
            error("Wrong email or password.", 401)
        }
        Err(AccountError::Account(m)) => error(&m, 400),
        Err(e @ AccountError::Internal(_)) => Reply::internal_error(&e),
    }
}

fn account_register(c: &Ctx) -> Reply {
    account_call(c, None)
}

fn account_login(c: &Ctx) -> Reply {
    account_call(c, Some(|_, account, _| Ok(account)))
}

fn account_email(c: &Ctx) -> Reply {
    account_call(c, Some(|accounts, account, body| accounts.change_email(&account, &field(body, "new_email"))))
}

fn account_password(c: &Ctx) -> Reply {
    account_call(c, Some(|accounts, account, body| accounts.change_password(&account, &field(body, "new_password"))))
}

fn account_name(c: &Ctx) -> Reply {
    account_call(c, Some(|accounts, account, body| {
        accounts.rename(&account, &field(body, "name"))?;
        Ok(account)
    }))
}

/// What registering would do here: take over an existing player (the first account on a server).
fn account_new(c: &Ctx) -> Reply {
    match c.state.accounts.claimable_player_name() {
        Ok(name) => Reply::json(&json!({"takes_over": name}), "powder-account", 200),
        Err(e) => Reply::internal_error(&e),
    }
}

// ---- client mods --------------------------------------------------------------------------------------------
/// The mods' client files the launcher may download (only those its player approved get loaded).
fn client_mod_index(ctx: &Ctx) -> Reply {
    let index = match &ctx.state.mods {
        Some(mods) => client_mods::index(mods.lock().client_mods()),
        None => client_mods::index(&[]),
    };
    Reply::json(&index, "powder", 200)
}

/// The launcher quotes each path segment (a space is %20); after decoding, only listed files match.
fn client_mod_file(ctx: &Ctx) -> Reply {
    let rest = unquote_path(ctx.param("rest"));
    let data = ctx.state.mods.as_ref().and_then(|m| client_mods::file(m.lock().client_mods(), &rest));
    match data {
        Some(bytes) => Reply::new(200, "application/octet-stream", bytes, "powder"),
        None => Reply::new(404, "text/plain; charset=utf-8", b"404: Not Found".to_vec(), "powder"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::{handle, HttpState, Req};
    use super::*;
    use crate::config::Config;
    use crate::core::ratelimit::FailureLimiter;
    use bytes::Bytes;
    use std::sync::Arc;

    const PASSWORD: &str = "correct horse";

    /// A state whose default player exists (the Python tests' imported player); -> its name.
    fn with_player(config: Config) -> (HttpState, String) {
        let st = state_with(config, vec![], None);
        let pid = st.accounts.default_profile_id().unwrap();
        let name = st.accounts.player(&pid).unwrap().name;
        (st, name)
    }

    fn post(st: &HttpState, path: &str, body: Value) -> Reply {
        let mut r: Req = req("POST", path, &[("Content-Type", "application/json")]);
        r.body = Bytes::from(crate::util::dumps(&body));
        handle(st, &r)
    }

    fn register(st: &HttpState, email: &str, name: &str) -> Reply {
        post(st, "/_powder/account/register", json!({"email": email, "password": PASSWORD, "name": name}))
    }

    #[test]
    fn launcher_account_api() {
        let (st, name) = with_player(Config::default());
        let pid = st.accounts.default_profile_id().unwrap();
        let r = handle(&st, &req("GET", "/_powder/account/new", &[]));
        assert_eq!((r.how.as_str(), body(&r)), ("powder-account", json!({"takes_over": name})));
        let r = register(&st, "rider@example.com", "");
        assert_eq!((r.status, body(&r)["profile_id"].as_str()), (200, Some(pid.as_str())));
        let info = json!({"account_id": body(&r)["account_id"], "email": "rider@example.com", "profile_id": pid,
                          "name": name});
        assert_eq!(r.body, crate::util::dumps(&info).into_bytes());
        assert!(body(&handle(&st, &req("GET", "/_powder/account/new", &[])))["takes_over"].is_null());
        let r = register(&st, "RIDER@example.com", "Other");
        assert!(r.status == 400 && body(&r)["error"].as_str().unwrap().contains("already exists"));
        let r = post(&st, "/_powder/account/login", json!({"email": "rider@example.com", "password": "nope"}));
        assert_eq!((r.status, body(&r)), (401, json!({"error": "Wrong email or password."})));
        let r = post(&st, "/_powder/account/login", json!({"email": "rider@example.com", "password": PASSWORD}));
        assert_eq!((r.status, body(&r)["name"].as_str()), (200, Some(name.as_str())));
        let r = post(&st, "/_powder/account/name", json!({"email": "rider@example.com", "password": PASSWORD,
                                                         "name": "NewName"}));
        assert_eq!((r.status, body(&r)["name"].as_str()), (200, Some("NewName")));
        let r = post(&st, "/_powder/account/email", json!({"email": "rider@example.com", "password": PASSWORD,
                                                          "new_email": "moved@example.com"}));
        assert_eq!(body(&r)["email"], "moved@example.com");
        let r = post(&st, "/_powder/account/password", json!({"email": "moved@example.com", "password": PASSWORD,
                                                             "new_password": "x"}));
        assert_eq!(r.status, 400);
        let r = post(&st, "/_powder/account/password", json!({"email": "moved@example.com", "password": PASSWORD,
                                                             "new_password": "brand new secret"}));
        assert_eq!(r.status, 200);
        assert!(st.accounts.authenticate("moved@example.com", "brand new secret").is_ok());
    }

    #[test]
    fn bad_account_requests() {
        let (st, _) = with_player(Config::default());
        for raw in [&b""[..], b"[1]", b"not json", b"\xff"] {
            let mut r = req("POST", "/_powder/account/login", &[]);
            r.body = Bytes::from(raw.to_vec());
            let r = handle(&st, &r);
            assert_eq!((r.status, body(&r)), (400, json!({"error": "bad request"})));
        }
        // a field that is not a string is read as Python's str() of it
        let r = register(&st, "rider@example.com", "");
        assert_eq!(r.status, 200);
        let r = post(&st, "/_powder/account/login", json!({"email": "rider@example.com", "password": 5}));
        assert_eq!(r.status, 401);
        let mut config = Config::default();
        config.players.registration = crate::config::Registration::Closed;
        let (closed, _) = with_player(config);
        let r = register(&closed, "rider@example.com", "");
        assert!(r.status == 400 && body(&r)["error"].as_str().unwrap().contains("does not take registrations"));
    }

    #[test]
    fn account_calls_are_rate_limited() {
        let (mut st, _) = with_player(Config::default());
        Arc::get_mut(&mut st.services).unwrap().login_failures = Arc::new(FailureLimiter::new(3, 30, 300.0));
        assert_eq!(register(&st, "rider@example.com", "").status, 200);
        let wrong = json!({"email": "rider@example.com", "password": "wrong password"});
        for _ in 0..3 {
            assert_eq!(post(&st, "/_powder/account/login", wrong.clone()).status, 401);
        }
        let right = json!({"email": "rider@example.com", "password": PASSWORD});
        let r = post(&st, "/_powder/account/login", right.clone());
        assert_eq!((r.status, body(&r)), (429, json!({"error": "Too many attempts, try again in a few minutes."})));
        // another address is not blocked
        let mut other = req_from("10.0.0.9", "POST", "/_powder/account/login", &[]);
        other.body = Bytes::from(crate::util::dumps(&right));
        assert_eq!(handle(&st, &other).status, 200);
        // registrations count like failures
        for i in 0..2 {
            register(&st, &format!("r{i}@example.com"), &format!("Rider{i}"));
        }
        assert_eq!(register(&st, "r9@example.com", "Rider9").status, 429);
    }

    #[test]
    fn status_lists_connect_clients_and_games() {
        let (st, name) = with_player(Config::default());
        let pid = st.accounts.default_profile_id().unwrap();
        let r = handle(&st, &req_from("10.0.0.5", "GET", "/_powder/status", &[]));
        assert_eq!((r.status, r.how.as_str()), (200, "powder-status"));
        assert_eq!(r.body, crate::util::dumps(&json!({
            "server": "powder", "version": crate::constants::version(), "you": "10.0.0.5",
            "public_host": st.config.server.public_host, "accounts": false, "players": 1, "gamedata": false,
            "connect": [], "game": []})).into_bytes());

        // a Connect client counts once it signed in; a game once its NEX connection is up
        let anonymous = st.connect_clients.add("10.0.0.5:4000");
        let signed_in = st.connect_clients.add("10.0.0.5:4001");
        signed_in.update(|c| {
            c.profile_id = Some(pid.clone());
            c.player_name = Some(name.clone());
            c.services = vec!["friends".into(), "ownership".into()];
        });
        struct Nex;
        impl crate::core::sessions::NexChannel for Nex {
            fn send_request(&self, _: &str, _: &str, _: &[u8], _: Option<u32>) -> u32 {
                0
            }
        }
        let s = st.sessions.issue(st.accounts.player(&pid).unwrap(), "10.0.0.5", "");
        s.channels.set_nex(Some(Arc::new(Nex)));
        let b = body(&handle(&st, &req("GET", "/_powder/status", &[])));
        let services = ["friends", "ownership"];
        assert_eq!(b["connect"], json!([{"player": name, "peer": "10.0.0.5:4001", "services": services}]));
        assert_eq!(b["game"], json!([{"player": name, "nex": true, "peer": "10.0.0.5"}]));
        drop((anonymous, signed_in));
        assert_eq!(body(&handle(&st, &req("GET", "/_powder/status", &[])))["connect"], json!([]));
    }

    #[test]
    fn ca_pem_and_the_overlay_part() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.data_dir = dir.path().to_path_buf();
        let st = state_with(config, vec![], None);
        let r = handle(&st, &req("GET", "/_powder/ca.pem", &[]));
        assert_eq!((r.status, r.body.as_slice(), r.how.as_str()), (404, &b"no CA"[..], "powder-ca"));
        let pki = crate::tls::ensure_pki(&dir.path().join("tls")).unwrap();
        let r = handle(&st, &req("GET", "/_powder/ca.pem", &[]));
        assert_eq!((r.status, header(&r, "content-type")), (200, Some("application/x-pem-file")));
        assert_eq!(r.body, std::fs::read(&pki.ca_pem).unwrap());

        let r = handle(&st, &req("GET", "/_powder/overlay/powder.js", &[]));
        assert_eq!((r.status, r.how.as_str()), (200, "powder-overlay"));
        assert_eq!(header(&r, "access-control-allow-origin"), Some("*"));
        assert_eq!(header(&r, "content-type"), Some("application/javascript; charset=utf-8"));
        let script = String::from_utf8(r.body).unwrap();
        assert!(script.contains(r#"Powder.apiVersion = "1.1.0""#) && script.contains(r#"_export("default", Powder)"#));
        // from Connect's overlay (an Origin): its origin replaces the "*", as aiohttp's CORS middleware does
        let r = handle(&st, &req("GET", "/_powder/overlay/powder.js", &[("Origin", "https://overlay")]));
        let allowed: Vec<_> = r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("access-control-allow-origin"))
            .map(|(_, v)| v.as_str()).collect();
        assert_eq!(allowed, ["https://overlay"]);
    }
}
