//! UbiServices endpoints with real logic, from the Python server's routes_ubi.py. Ported so far: the sign-in
//! (POST /v3/profiles/sessions), the NEX discovery (rendezvous configuration) and Ubisoft Connect's internet
//! check. Not yet: inventory, store offers, profile entities (+ their files) and Connect's cloud saves; those
//! fall through to the pack.

use super::{Ctx, Reply, Route};
use crate::constants::{APP_ID, CONNECT_SPACE_ID, SPACE_ID};
use crate::core::identity::{parse_authorization, LoginRequest};
use crate::core::sessions::ubi_shaped_ticket;
use crate::services::accounts::{basic_credentials, Error as AccountError};
use crate::util::{iso_in, iso_now, token_hex, token_urlsafe};
use serde_json::{json, Value};

/// In routes_ubi.py's registration order.
pub fn routes() -> Vec<Route> {
    vec![
        Route::new("GET", "/{prefix:.*}test_internet.txt", internet_check),
        Route::new("POST", "/v3/profiles/sessions", create_session),
        Route::new("GET", "/v1/spaces/{space}/sandboxes/{sandbox}/rendezvous/configuration", rendezvous_configuration),
    ]
}

/// A refused sign-in, shaped like UbiServices' (Ubisoft Connect then asks for email and password).
fn auth_refused(message: &str) -> Reply {
    Reply::json(&json!({"message": message, "errorCode": 1, "httpCode": 401,
                        "errorContext": "Profiles Client Facade Service", "moreInfo": "",
                        "transactionTime": iso_now(), "transactionId": token_hex(16)}),
                "refused", 401)
}

// ---- login --------------------------------------------------------------------------------------------------
/// POST /v3/profiles/sessions. The game authenticates with `uplaypc_v1 t=<Connect ticket>` (or `Basic` with
/// empty credentials when Connect has none); we issue our own ticket, which it then sends as `Ubi_v1 t=` and as
/// the NEX LoginWithToken token. Ubisoft Connect logs in here too (`rm_v1` remember-me ticket, `Ubi_v1`
/// refresh, or `Basic` email:password on a fresh sign-in): the account service decides who it is. Passwords are
/// never logged or stored in clear.
fn create_session(c: &Ctx) -> Reply {
    let (state, req) = (c.state, c.req);
    let (scheme, value) = parse_authorization(req.header("Authorization"));
    // What the game got from Ubisoft Connect (its platform ticket) says a lot about whether Connect still works,
    // e.g. with Ubisoft's servers unreachable. Log its shape only, never its content.
    let ticket = if value.is_empty() { "MISSING".to_string() } else { format!("{} chars", value.chars().count()) };
    log::info!(target: "powder.https", "login credentials: scheme={}, ticket {ticket}, app={}",
               if scheme.is_empty() { "(none)" } else { &scheme }, req.header("Ubi-AppId").unwrap_or("(none)"));
    let ip = req.client_ip();
    // only an email and password is a guess worth limiting (tickets are not guessable; the game sends `Basic`
    // with empty credentials when Ubisoft Connect has none)
    let creds = if scheme.eq_ignore_ascii_case("basic") { basic_credentials(&value) } else { None };
    let email = creds.as_ref().map(|(e, _)| e.as_str()).unwrap_or("");
    if creds.is_some() && state.login_failures.blocked(&ip, email) {
        log::warn!(target: "powder.https", "login from {ip} refused: too many failed sign-ins, try again in a few minutes");
        return auth_refused("Too many failed sign-ins, try again in a few minutes");
    }
    let headers = req.headers.iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    let login = LoginRequest { client_ip: ip.clone(), auth_scheme: scheme, auth_value: value, headers };
    let session = match state.accounts.login(&login) {
        Ok(s) => s,
        Err(AccountError::Auth(e)) => {
            if creds.is_some() {
                state.login_failures.failed(&ip, email);
            }
            log::warn!(target: "powder.https", "login from {ip} refused: {e}");
            return auth_refused("Invalid credentials");
        }
        Err(e) => return Reply::internal_error(&e),
    };
    if creds.is_some() {
        state.login_failures.succeeded(&ip, email);
    }
    let p = &session.player;
    let app_id = req.header("Ubi-AppId").unwrap_or("").to_lowercase();
    let connect = !app_id.is_empty() && app_id != APP_ID;
    let mut body = json!({"platformType": "uplay", "environment": "Prod", "profileId": p.profile_id,
                          "userId": p.user_id, "nameOnPlatform": p.name, "ticket": session.ticket,
                          "sessionId": session.session_id, "sessionKey": token_urlsafe(48),
                          "expiration": iso_in(3 * 3600), "serverTime": iso_now(), "clientIp": session.client_ip,
                          "clientIpCountry": state.config.server.country, "twoFactorAuthenticationTicket": null});
    let fields = body.as_object_mut().expect("an object");
    if connect {
        // Ubisoft Connect (any other app id) keeps the remember-me ticket and signs in with it next time
        // (`rm_v1`), which we accept like any other credential.
        let remember = ubi_shaped_ticket(&app_id, &session.session_id);
        if let Err(e) = state.accounts.remember(&session, &remember) {
            return Reply::internal_error(&e);
        }
        fields.insert("spaceId".into(), json!(CONNECT_SPACE_ID));
        fields.insert("rememberMeTicket".into(), json!(remember));
        fields.insert("codeGenerationPreference".into(), Value::Null);
        fields.insert("maskedPhone".into(), Value::Null);
    } else {
        fields.insert("spaceId".into(), json!(SPACE_ID));
        fields.insert("rememberMeTicket".into(), Value::Null);
    }
    if let Some(mods) = &state.mods {
        mods.lock().emit("login", &json!({"profile_id": p.profile_id, "user_id": p.user_id, "name": p.name,
                                          "session_id": session.session_id, "client_ip": session.client_ip,
                                          "connect": session.connect}));
    }
    Reply::json(&body, "login", 200)
}

// ---- NEX discovery ------------------------------------------------------------------------------------------
/// Points the game at our NEX server and publishes the key that signs its handshake.
fn rendezvous_configuration(c: &Ctx) -> Reply {
    let cfg = &c.state.config.server;
    Reply::json(&json!({
        "http_urls": [],
        "prudp_loadbalancer_url": format!("prudp:/address={};port={}", cfg.public_host, cfg.nex_port),
        "prudp_urls": [],
        "http_loadbalancer_url": "",
        "public_key": c.state.nex_public_key,
    }), "rendezvous", 200)
}

// ---- Ubisoft Connect ----------------------------------------------------------------------------------------
/// static3.cdn.ubi.com/.../test_internet.txt: Ubisoft Connect shows "no internet connection" unless this answers
/// like the real CDN.
fn internet_check(_: &Ctx) -> Reply {
    Reply::new(200, "text/plain; charset=utf-8", b"I AM ALIVE!".to_vec(), "internet-check")
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::{handle, HttpState};
    use crate::config::Config;
    use crate::constants::CONNECT_APP_ID;
    use crate::core::ratelimit::FailureLimiter;
    use crate::util::{b64, unb64};
    use serde_json::Value;
    use std::sync::Arc;

    const PASSWORD: &str = "correct horse";

    /// A state whose default player exists (the Python tests' imported player).
    fn with_player() -> (HttpState, String) {
        let st = state(vec![], None);
        let pid = st.accounts.default_profile_id().unwrap();
        st.accounts.player(&pid).unwrap();
        (st, pid)
    }

    fn basic(email: &str, password: &str) -> String {
        format!("Basic {}", b64(format!("{email}:{password}").as_bytes()))
    }

    fn connect_login(st: &HttpState, auth: &str) -> super::Reply {
        handle(st, &req("POST", "/v3/profiles/sessions", &[("Ubi-AppId", CONNECT_APP_ID), ("Authorization", auth)]))
    }

    fn game_login(st: &HttpState) -> super::Reply {
        handle(st, &req("POST", "/v3/profiles/sessions", &[("Authorization", "uplaypc_v1 t=connect-made-ticket")]))
    }

    fn register(st: &HttpState, email: &str, name: &str) {
        st.accounts.register(email, PASSWORD, name).unwrap();
    }

    #[test]
    fn login_issues_our_ticket_for_the_default_player() {
        let (st, pid) = with_player();
        let r = game_login(&st);
        assert_eq!((r.status, r.how.as_str()), (200, "login"));
        assert_eq!(header(&r, "content-type"), Some(super::super::JSON));
        let b = body(&r);
        assert_eq!(b["profileId"], pid.as_str());
        assert_eq!(b["spaceId"], crate::constants::SPACE_ID);
        assert!(b["ticket"].as_str().unwrap().starts_with("powder."));
        assert!(st.sessions.by_token(b["ticket"].as_str().unwrap()).is_some());
        // Python's key order and json.dumps separators
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["platformType", "environment", "profileId", "userId", "nameOnPlatform", "ticket",
                          "sessionId", "sessionKey", "expiration", "serverTime", "clientIp", "clientIpCountry",
                          "twoFactorAuthenticationTicket", "spaceId", "rememberMeTicket"]);
        assert!(String::from_utf8_lossy(&r.body).starts_with(r#"{"platformType": "uplay", "environment": "Prod", "#));
        assert_eq!((b["clientIp"].as_str(), b["clientIpCountry"].as_str()), (Some("127.0.0.1"), Some("US")));
        assert!(b["rememberMeTicket"].is_null() && b["twoFactorAuthenticationTicket"].is_null());
    }

    /// Connect signs in with a remember-me ticket, email:password or a refresh; all are accepted (no accounts
    /// yet), and it gets a Ubisoft-shaped ticket plus a new remember-me ticket.
    #[test]
    fn ubisoft_connect_login() {
        for auth in ["rm_v1 t=remember-me", "Basic dXNlckBleGFtcGxlLmNvbTpzZWNyZXQ=", "ubi_v1 t=x"] {
            let (st, pid) = with_player();
            let r = connect_login(&st, auth);
            assert_eq!(r.status, 200, "{auth}");
            let b = body(&r);
            assert_eq!(b["profileId"], pid.as_str());
            let head = unb64(b["ticket"].as_str().unwrap().split('.').next().unwrap()).unwrap();
            let head: Value = serde_json::from_slice(&head).unwrap();
            assert_eq!(head["aid"], CONNECT_APP_ID);
            assert_eq!(head["sid"], b["sessionId"]);
            assert!(b["rememberMeTicket"].as_str().is_some_and(|t| !t.is_empty()));
            assert!(b["twoFactorAuthenticationTicket"].is_null());
            assert_eq!(b["spaceId"], crate::constants::CONNECT_SPACE_ID);
            let keys: Vec<&str> = b.as_object().unwrap().keys().skip(13).map(String::as_str).collect();
            assert_eq!(keys, ["spaceId", "rememberMeTicket", "codeGenerationPreference", "maskedPhone"]);
            assert_eq!(st.sessions.by_token(b["ticket"].as_str().unwrap()).unwrap().profile_id(), pid);
        }
    }

    #[test]
    fn cors_preflight_for_sign_in() {
        let (st, _) = with_player();
        let r = handle(&st, &req("OPTIONS", "/v3/profiles/sessions", &[
            ("Origin", "https://connect.cdn.ubisoft.com"), ("Access-Control-Request-Method", "POST"),
            ("Access-Control-Request-Headers", "ubi-appid,authorization")]));
        assert_eq!(r.status, 200);
        assert_eq!(header(&r, "access-control-allow-origin"), Some("https://connect.cdn.ubisoft.com"));
        assert_eq!(header(&r, "access-control-allow-headers"), Some("ubi-appid,authorization"));
        let r = handle(&st, &req("POST", "/v3/profiles/sessions", &[("Origin", "https://connect.cdn.ubisoft.com")]));
        assert_eq!((r.how.as_str(), header(&r, "access-control-allow-origin")),
                   ("login", Some("https://connect.cdn.ubisoft.com")));
    }

    #[test]
    fn connect_internet_check() {
        let (st, _) = with_player();
        for path in ["/health_check/test_internet.txt?cacheBuster=1", "/orbit/uplay_launcher_2_0/test_internet.txt"] {
            let r = handle(&st, &req("GET", path, &[]));
            assert_eq!((r.status, r.body.as_slice()), (200, &b"I AM ALIVE!"[..]));
            assert_eq!(header(&r, "content-type"), Some("text/plain; charset=utf-8"));
        }
    }

    #[test]
    fn rendezvous_points_at_our_nex_server() {
        let (st, _) = with_player();
        let path = format!("/v1/spaces/{}/sandboxes/STEEP_PC_TU7_A/rendezvous/configuration", crate::constants::SPACE_ID);
        let r = handle(&st, &req("GET", &path, &[]));
        let cfg = &st.config.server;
        let want = format!(r#"{{"http_urls": [], "prudp_loadbalancer_url": "prudp:/address={};port={}", "prudp_urls": [], "http_loadbalancer_url": "", "public_key": "TESTKEY"}}"#,
                           cfg.public_host, cfg.nex_port);
        assert_eq!(String::from_utf8(r.body).unwrap(), want);
        // a method without a route falls through to the pack (then: unmatched)
        assert_eq!(handle(&st, &req("DELETE", &path, &[])).how, "UNMATCHED");
    }

    // ---- accounts (the Python tests/test_accounts.py) ----------------------------------------------------------
    #[test]
    fn connect_signs_in_with_email_and_password_then_remember_me() {
        let (st, pid) = with_player();
        register(&st, "rider@example.com", "Rider");
        let r = connect_login(&st, &basic("rider@example.com", "wrong password"));
        assert_eq!(r.status, 401);
        let b = body(&r);
        assert_eq!((b["errorCode"].as_i64(), b["message"].as_str()), (Some(1), Some("Invalid credentials")));
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["message", "errorCode", "httpCode", "errorContext", "moreInfo", "transactionTime",
                          "transactionId"]);
        let r = connect_login(&st, &basic("rider@example.com", PASSWORD));
        assert_eq!(r.status, 200);
        let b = body(&r);
        assert_eq!(b["profileId"], pid.as_str(), "the first account takes over the default player");
        let rm = format!("rm_v1 t={}", b["rememberMeTicket"].as_str().unwrap());
        assert_eq!(connect_login(&st, &rm).status, 200);
        st.sessions.drop_player(&pid); // a server restart: remember-me still works
        assert_eq!(connect_login(&st, &rm).status, 200);
        assert_eq!(connect_login(&st, "rm_v1 t=made-up").status, 401);
    }

    #[test]
    fn game_follows_connect_on_the_same_pc() {
        let (st, pid) = with_player();
        register(&st, "rider@example.com", "Rider");
        assert_eq!(game_login(&st).status, 401, "nobody signed in to Connect here yet");
        connect_login(&st, &basic("rider@example.com", PASSWORD));
        let r = game_login(&st);
        assert_eq!((r.status, body(&r)["profileId"].as_str()), (200, Some(pid.as_str())));
    }

    /// A server from before accounts: until the first registration, logins work as they did.
    #[test]
    fn without_accounts_every_credential_still_works() {
        let (st, _) = with_player();
        assert_eq!(connect_login(&st, &basic("anyone@example.com", "anything")).status, 200);
        assert_eq!(game_login(&st).status, 200);
    }

    fn limited(max_failures: usize) -> HttpState {
        let mut st = state_with(Config::default(), vec![], None);
        Arc::get_mut(&mut st.services).unwrap().login_failures = Arc::new(FailureLimiter::new(max_failures, 30, 300.0));
        st
    }

    #[test]
    fn failed_sign_ins_are_rate_limited() {
        let st = limited(3);
        register(&st, "rider@example.com", "Rider");
        for _ in 0..3 {
            assert_eq!(connect_login(&st, &basic("rider@example.com", "wrong password")).status, 401);
        }
        let r = connect_login(&st, &basic("rider@example.com", PASSWORD));
        assert_eq!(r.status, 401);
        assert!(body(&r)["message"].as_str().unwrap().contains("Too many"));
    }

    #[test]
    fn signing_in_to_ones_own_account_does_not_reset_the_guess_count() {
        let st = limited(3);
        register(&st, "rider@example.com", "Rider");
        register(&st, "attacker@example.com", "Attacker");
        for _ in 0..2 {
            connect_login(&st, &basic("rider@example.com", "wrong password"));
            assert_eq!(connect_login(&st, &basic("attacker@example.com", PASSWORD)).status, 200);
        }
        connect_login(&st, &basic("rider@example.com", "wrong password"));
        let r = connect_login(&st, &basic("rider@example.com", PASSWORD));
        assert_eq!(r.status, 401);
        assert!(body(&r)["message"].as_str().unwrap().contains("Too many"));
    }

    #[test]
    fn empty_game_credentials_are_not_counted_as_guesses() {
        let st = limited(2);
        register(&st, "rider@example.com", "Rider");
        let empty = format!("Basic {}", b64(b":"));
        for _ in 0..5 {
            // the game without a Connect session
            let r = handle(&st, &req("POST", "/v3/profiles/sessions", &[("Authorization", &empty)]));
            assert_eq!(r.status, 401);
        }
        assert_eq!(connect_login(&st, &basic("rider@example.com", PASSWORD)).status, 200);
    }

    #[test]
    fn a_mod_hears_the_login() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.lua"), r#"return function(api)
            api.on("login", function(ev) api.save("last", ev.name .. "/" .. tostring(ev.connect)) end)
        end"#).unwrap();
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Arc::new(crate::repos::Repos::new(db));
        let mut config = Config::default();
        config.mods.dir = dir.path().to_path_buf();
        let host = crate::mods::ModHost::load(&config, repos.clone());
        let st = state(vec![], Some(host));
        let b = body(&game_login(&st));
        let saved = repos.kv_get("mod:m", "last").unwrap().unwrap();
        assert_eq!(saved, Value::String(format!("{}/false", b["nameOnPlatform"].as_str().unwrap())));
    }
}
