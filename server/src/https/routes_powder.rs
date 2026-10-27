//! /_powder/: the server's own endpoints (not Ubisoft's), from routes_powder.py, plus the client mods. Used by the
//! launcher. The status needs no sign-in; it shows the addresses of connected games and Connect clients only to
//! the server's own PC (loopback) and to each caller for its own address (others: names only; on a public
//! server not at all). The account calls carry the account's email and password.
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
use std::net::{IpAddr, SocketAddr};

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

/// The address part of a peer ("10.0.0.5:4001", "[::1]:4001" or a bare address).
fn peer_ip(peer: &str) -> Option<IpAddr> {
    peer.parse::<SocketAddr>().map(|a| a.ip()).or_else(|_| peer.parse::<IpAddr>()).ok()
        .map(|ip| match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            ip => ip,
        })
}

/// Who sees which entries of the status: the server's own PC (loopback) everything; anyone else its own
/// entries (same address) as they are, the others' without their address, and on a public server not at all.
/// The launcher uses its own entries ("connect", "game" with the peer it is seen from) and the names in "game".
fn shown(c: &Ctx, peer: &str, mut entry: Value) -> Option<Value> {
    let you = peer_ip(&c.req.client_ip());
    if you.is_some_and(|ip| ip.is_loopback()) || (you.is_some() && peer_ip(peer) == you) {
        return Some(entry);
    }
    if c.state.config.server.public {
        return None;
    }
    entry.as_object_mut()?.shift_remove("peer");
    Some(entry)
}

fn status_body(c: &Ctx) -> anyhow::Result<Value> {
    let st = c.state;
    let connect: Vec<Value> = st.connect_clients.snapshot().into_iter().filter(|cl| cl.profile_id.is_some())
        .filter_map(|cl| shown(c, &cl.peer, json!({"player": cl.player_name, "peer": cl.peer,
                                                    "services": cl.services})))
        .collect();
    // Python lists its sessions by player in first-login order; ours come from a map, so oldest session first.
    // The NEX connection does not know its address here: the session's (the PC it signed in from) stands in.
    let mut sessions: Vec<_> = st.sessions.players().into_iter().filter(|s| s.channels.nex().is_some()).collect();
    sessions.sort_by(|a, b| a.created.total_cmp(&b.created));
    let game: Vec<Value> = sessions.iter()
        .filter_map(|s| shown(c, &s.client_ip, json!({"player": s.player.name, "nex": true, "peer": s.client_ip})))
        .collect();
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
    // counted before hashing (concurrent calls can't all pass); a registration always counts, a call whose
    // password is right takes its count back
    if !limiter.attempt(&ip, key) {
        return error("Too many attempts, try again in a few minutes.", 429);
    }
    let accounts = &st.accounts;
    let result = match change {
        None => accounts.register_from(&email, &field(&body, "password"), &field(&body, "name"), &ip),
        Some(change) => accounts.authenticate(&email, &field(&body, "password")).and_then(|account| {
            limiter.succeeded(&ip, &email);
            // a banned player (or one off the whitelist) learns why here: the launcher shows it
            if let Some(refusal) = accounts.access.check(&account.profile_id, &ip)? {
                return Err(AccountError::Account(refusal.message()));
            }
            change(accounts, account, &body)
        }),
    };
    match result.and_then(|account| accounts.info(&account).map_err(AccountError::from)) {
        Ok(info) => Reply::json(&info, "powder-account", 200),
        Err(AccountError::Auth(_)) => error("Wrong email or password.", 401),
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

/// What registering would do here: take over an existing player (the first account on a server, registered
/// from the server's own PC; services/accounts.rs register_from).
fn account_new(c: &Ctx) -> Reply {
    match c.state.accounts.claimable_player_name_for(&c.req.client_ip()) {
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

    /// A fresh server: only a registration from its own PC takes over the default player (and its progress).
    #[test]
    fn only_the_servers_pc_takes_over_the_player() {
        for public in [false, true] {
            let mut config = Config::default();
            config.server.public = public;
            let (st, name) = with_player(config);
            let pid = st.accounts.default_profile_id().unwrap();
            let remote = |path: &str, body: Option<Value>| {
                let mut r = req_from("203.0.113.7", if body.is_some() { "POST" } else { "GET" }, path, &[]);
                r.body = Bytes::from(body.map(|b| crate::util::dumps(&b)).unwrap_or_default());
                handle(&st, &r)
            };
            assert_eq!(body(&remote("/_powder/account/new", None)), json!({"takes_over": null}));
            let r = remote("/_powder/account/register",
                           Some(json!({"email": "stranger@example.com", "password": PASSWORD, "name": "Stranger"})));
            assert_eq!(r.status, 200);
            assert_ne!(body(&r)["profile_id"], pid.as_str(), "a new player, not the host's");
            assert_eq!(st.repos.player(&pid).unwrap().unwrap().name, name, "the default player untouched");
            assert!(st.repos.account_by_profile(&pid).unwrap().is_none());
        }
        // from the server's PC (also over IPv6): taken over
        let (st, name) = with_player(Config::default());
        let r = handle(&st, &req_from("[::1]", "GET", "/_powder/account/new", &[]));
        assert_eq!(body(&r), json!({"takes_over": name}));
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

    /// Other players' addresses only for the server's own PC; a caller sees its own entries as the launcher
    /// needs them (peer = its address), the others by name (on a public server: not at all).
    #[test]
    fn status_hides_other_players_addresses() {
        struct Nex;
        impl crate::core::sessions::NexChannel for Nex {
            fn send_request(&self, _: &str, _: &str, _: &[u8], _: Option<u32>) -> u32 {
                0
            }
        }
        for public in [false, true] {
            let mut config = Config::default();
            config.server.public = public;
            let st = state_with(config, vec![], None);
            let (ann, bob) = (st.accounts.player("00000000-0000-4000-8000-00000000000a").unwrap(),
                              st.accounts.player("00000000-0000-4000-8000-00000000000b").unwrap());
            let mut clients = vec![];
            for (player, ip) in [(&ann, "10.0.0.5"), (&bob, "10.0.0.9")] {
                st.sessions.issue(player.clone(), ip, "").channels.set_nex(Some(Arc::new(Nex)));
                let c = st.connect_clients.add(&format!("{ip}:4000"));
                c.update(|c| {
                    c.profile_id = Some(player.profile_id.clone());
                    c.player_name = Some(player.name.clone());
                });
                clients.push(c);
                std::thread::sleep(std::time::Duration::from_millis(2)); // session order
            }
            let b = body(&handle(&st, &req_from("10.0.0.9", "GET", "/_powder/status", &[])));
            let mut want_game = vec![json!({"player": bob.name, "nex": true, "peer": "10.0.0.9"})];
            let mut want_connect = vec![json!({"player": bob.name, "peer": "10.0.0.9:4000", "services": []})];
            if !public {
                want_game.insert(0, json!({"player": ann.name, "nex": true}));
                want_connect.insert(0, json!({"player": ann.name, "services": []}));
            }
            assert_eq!((&b["game"], &b["connect"]), (&json!(want_game), &json!(want_connect)), "public {public}");
            assert_eq!(b["you"], "10.0.0.9");
            // the server's PC sees everything
            for local in ["127.0.0.1", "[::1]"] {
                let all = body(&handle(&st, &req_from(local, "GET", "/_powder/status", &[])));
                assert_eq!(all["game"][0]["peer"], "10.0.0.5", "{local}");
                assert_eq!(all["connect"].as_array().unwrap().len(), 2);
            }
        }
        assert_eq!(peer_ip("[::ffff:10.0.0.1]:5"), Some("10.0.0.1".parse().unwrap()));
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
        let overlay = "https://connect.cdn.ubisoft.com";
        let r = handle(&st, &req("GET", "/_powder/overlay/powder.js", &[("Origin", overlay)]));
        let allowed: Vec<_> = r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("access-control-allow-origin"))
            .map(|(_, v)| v.as_str()).collect();
        assert_eq!(allowed, [overlay]);
    }
}
