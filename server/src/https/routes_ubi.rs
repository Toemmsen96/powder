//! UbiServices endpoints with real logic, from the Python server's routes_ubi.py: the sign-in
//! (POST /v3/profiles/sessions), the NEX discovery (rendezvous configuration), inventory and store offers
//! (services/economy.rs), profile entities and the files behind them (services/entities.rs), and Ubisoft
//! Connect's internet check and cloud saves (services/cloudsaves.rs: the same instance the demux hands its
//! links out from). Player-related endpoints that only need a plausible answer are routes_profile's; game-wide
//! data comes from the pack (the fallback).

use super::{Ctx, Reply, Req, Route};
use crate::builtin::http::Query;
use crate::constants::{APP_ID, CONNECT_SPACE_ID, SPACE_ID};
use crate::core::identity::{parse_authorization, LoginRequest};
use crate::core::sessions::ubi_shaped_ticket;
use crate::services::accounts::{basic_credentials, Error as AccountError};
use crate::services::cloudsaves::{self, CloudSaveError};
use crate::services::entities::Entity;
use crate::util::{iso_in, iso_now, token_hex, token_urlsafe};
use anyhow::Context as _;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;

/// In routes_ubi.py's registration order.
pub fn routes() -> Vec<Route> {
    vec![
        Route::new("GET", "/v1/profiles/{profile_id}/entities", get_entities_v1),
        Route::new("POST", "/v1/profiles/{profile_id}/entities", write_cloud_entity),
        Route::new("PUT", "/v1/profiles/entities/{entity_id}", write_cloud_entity),
        Route::new("PUT", "/v1/profiles/entitites/{entity_id}", write_cloud_entity), // sic: what Connect sends
        Route::new("GET", "/_powder/cloudsaves/legacy-list/{product}", cloud_save_legacy_list),
        Route::new("*", "/_powder/cloudsaves/{product}/{slot}", cloud_save_file),
        Route::new("GET", "/{prefix:.*}test_internet.txt", internet_check),
        Route::new("POST", "/v3/profiles/sessions", create_session),
        Route::new("GET", "/v1/spaces/{space}/sandboxes/{sandbox}/rendezvous/configuration", rendezvous_configuration),
        Route::new("GET", "/v1/profiles/{profile_id}/inventory", get_inventory),
        Route::new("POST", "/v1/profiles/{profile_id}/inventory", post_inventory),
        Route::new("GET", "/v1/spaces/{space}/offers", get_offers),
        Route::new("GET", "/v2/profiles/entities", get_entities_batch),
        Route::new("POST", "/v2/profiles/entities", post_entity),
        Route::new("GET", "/v2/profiles/entities/{entity_id}", get_entity),
        Route::new("PUT", "/v2/profiles/entities/{entity_id}", put_entity),
        Route::new("DELETE", "/v2/profiles/entities/{entity_id}", delete_entity),
        Route::new("GET", "/v2/profiles/{profile_id}/entities", get_profile_entities),
        Route::new("POST", "/v2/profiles/{profile_id}/entities", post_entity),
        Route::new("*", "/_powder/entity-files/{entity_id}", entity_file),
    ]
}

// ---- shared with routes_ugc ---------------------------------------------------------------------------------
/// The Host header as sent, port included (aiohttp's `request.host`): where the links we hand out point.
pub(super) fn host(req: &Req) -> &str {
    req.header("Host").filter(|h| !h.is_empty()).unwrap_or(&req.host)
}

/// The decoded query parameters (aiohttp's `request.query`).
pub(super) fn query(req: &Req) -> Query {
    Query::parse(req.path_qs.split_once('?').map(|(_, q)| q).unwrap_or(""))
}

/// A reply without a body or Content-Type, as aiohttp's bare `web.Response(status=...)`. Python's routes leave
/// some of them without a `how`: the access log shows "?".
pub(super) fn empty(status: u16, how: &str) -> Reply {
    Reply { status, headers: vec![], body: vec![], how: how.into() }
}

/// A file as aiohttp's `FileResponse` (with Content-Type application/octet-stream) sends it; hyper leaves the
/// body out for HEAD.
pub(super) fn file_reply(path: &std::path::Path, how: &str) -> Reply {
    match std::fs::read(path).with_context(|| format!("reading {}", path.display())) {
        Ok(data) => Reply::new(200, "application/octet-stream", data, how),
        Err(e) => Reply::internal_error(&e),
    }
}

/// The signed-in player (the session's), if any.
pub(super) fn owner(c: &Ctx) -> Option<String> {
    c.session.as_ref().map(|s| s.profile_id().to_string())
}

// ---- who may act ------------------------------------------------------------------------------------------
// A server without accounts trusts everyone (one local player, the default one, or [players.bind]): routes act
// for the session's player, else the default player, as before accounts existed. Once accounts are required (an
// account exists, or [server] public), a route that acts for a player needs a session (401 otherwise, never the
// default player as a stand-in), writes go only to the session's own player, entities and UGC (403 otherwise),
// and reads of player data need a session too (any player: others' public data stays readable). Not affected:
// the sign-in, the internet check, the rendezvous configuration, game-wide data (pack, built-in replies,
// offers), the public /_powder/ routes and the links whose token is the credential (cloud-save slots, entity
// file uploads); file downloads (entity files, photos) stay open like the S3 links they stand in for.

/// The player a request acts for, and whether it may act only on its own data (accounts are required).
pub(super) struct Actor {
    pub pid: String,
    pub guarded: bool,
}

static PROFILE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap());

/// A profile id from the URL, lower-cased, if it is shaped like one (a UUID): anything else never reaches the
/// database or a file path.
pub(super) fn url_profile_id(raw: &str) -> Option<String> {
    let pid = raw.to_lowercase();
    PROFILE_ID.is_match(&pid).then_some(pid)
}

pub(super) fn accounts_required(c: &Ctx) -> Result<bool, Reply> {
    c.state.accounts.required().map_err(|e| Reply::internal_error(&e))
}

/// A request without a session where one is needed: 401, shaped like UbiServices' refused sign-in.
pub(super) fn no_session(c: &Ctx) -> Reply {
    log::warn!(target: "powder.https", "{} {} from {} refused: no session (accounts are required)", c.req.method,
               c.req.path(), c.req.client_ip());
    let mut r = auth_refused("Not signed in (no valid ticket)");
    r.how = "no-session".into();
    r
}

/// Writing what belongs to another player (accounts required).
pub(super) fn forbidden(c: &Ctx, what: &str) -> Reply {
    log::warn!(target: "powder.https", "{} {} by {} refused: {what}", c.req.method, c.req.path(),
               owner(c).unwrap_or_else(|| "?".into()));
    error(1, what, 403, "forbidden")
}

/// Who acts: the session's player; without a session the default player (no accounts) or a 401.
pub(super) fn acting(c: &Ctx) -> Result<Actor, Reply> {
    let guarded = accounts_required(c)?;
    match (owner(c), guarded) {
        (Some(pid), _) => Ok(Actor { pid, guarded }),
        (None, false) => Ok(Actor { pid: default_player(c).map_err(|e| Reply::internal_error(&e))?, guarded }),
        (None, true) => Err(no_session(c)),
    }
}

/// Reads of player data: anyone on a server without accounts, else a signed-in player (-> its profile id).
pub(super) fn reader(c: &Ctx) -> Result<Option<String>, Reply> {
    match owner(c) {
        Some(pid) => Ok(Some(pid)),
        None if accounts_required(c)? => Err(no_session(c)),
        None => Ok(None),
    }
}

/// The reply, or the refusal (Err) that came first.
pub(super) fn allowed(r: Result<Reply, Reply>) -> Reply {
    r.unwrap_or_else(|refused| refused)
}

/// `items[offset:offset + limit]`, as Python slices (negative numbers count from the end).
pub(super) fn py_slice<T>(items: Vec<T>, offset: i64, limit: i64) -> Vec<T> {
    let n = items.len() as i64;
    let at = |i: i64| if i < 0 { (n + i).max(0) } else { i.min(n) };
    let (start, stop) = (at(offset), at(offset.saturating_add(limit)));
    items.into_iter().skip(start as usize).take((stop - start).max(0) as usize).collect()
}

/// A UbiServices-style error.
fn error(code: i64, message: &str, status: u16, how: &str) -> Reply {
    Reply::json(&json!({"errorCode": code, "message": message, "httpCode": status}), how, status)
}

/// The request body as a JSON object. Not JSON: `bad(message)` (aiohttp's `request.json()` raises a ValueError
/// the Python routes catch); JSON but not an object: a 500 (where the Python routes fail).
fn json_object(c: &Ctx, bad: impl Fn(&str) -> Reply) -> Result<Entity, Reply> {
    match serde_json::from_slice::<Value>(&c.req.body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(Reply::internal_error(&format!("{} {}: the body is not a JSON object", c.req.method,
                                                    c.req.path()))),
        Err(e) => Err(bad(&e.to_string())),
    }
}

fn default_player(c: &Ctx) -> anyhow::Result<String> {
    c.state.accounts.default_profile_id()
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
    // counted before the password is hashed (concurrent guesses can't all pass), taken back if it works
    if creds.is_some() && !state.login_failures.attempt(&ip, email) {
        log::warn!(target: "powder.https", "login from {ip} refused: too many failed sign-ins, try again in a few minutes");
        return auth_refused("Too many failed sign-ins, try again in a few minutes");
    }
    let headers = req.headers.iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
    let login = LoginRequest { client_ip: ip.clone(), auth_scheme: scheme, auth_value: value, headers };
    let session = match state.accounts.login(&login) {
        Ok(s) => s,
        Err(AccountError::Auth(e)) => {
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

// ---- inventory & store --------------------------------------------------------------------------------------
fn get_inventory(c: &Ctx) -> Reply {
    if let Err(refused) = reader(c) {
        return refused;
    }
    let pid = c.param("profile_id").to_lowercase();
    match c.state.repos.player(&pid) {
        // someone this server has no record of: owns nothing
        Ok(None) => Reply::json(&json!({"items": []}), "inventory(unknown player)", 200),
        Ok(Some(_)) => match c.state.economy.inventory_reply(&pid) {
            Ok(body) => Reply::json(&body, "inventory", 200),
            Err(e) => Reply::internal_error(&e),
        },
        Err(e) => Reply::internal_error(&e),
    }
}

/// Shop purchase or reward: {"spaceId", "offerId", "offerRevision"}. Mods hear it as "purchase" {profile_id,
/// offer_id, changes [{item_id, delta, before, after}]}.
fn post_inventory(c: &Ctx) -> Reply {
    let st = c.state;
    let pid = c.param("profile_id").to_lowercase();
    let me = match acting(c) {
        Ok(me) => me,
        Err(refused) => return refused,
    };
    if me.pid != pid {
        if me.guarded {
            return forbidden(c, "not your inventory");
        }
        // no accounts: anyone may, as before, but a player record is made only for the one acting
        match st.repos.player(&pid) {
            Ok(Some(_)) => log::warn!(target: "powder.https", "inventory POST for {pid} by {}", me.pid),
            Ok(None) => return error(1, &format!("profile {pid} not found"), 404, "unknown-player"),
            Err(e) => return Reply::internal_error(&e),
        }
    }
    let bad = |msg: &str| error(1, &format!("bad request: {msg}"), 400, "bad-request");
    let body = match json_object(c, bad) {
        Ok(b) => b,
        Err(reply) => return reply,
    };
    if let Err(e) = st.accounts.player(&pid) {
        return Reply::internal_error(&e);
    }
    let Some(offer) = body.get("offerId") else { return bad("'offerId'") }; // Python's KeyError
    let offer_id = offer.as_str().map(str::to_string).unwrap_or_else(|| offer.to_string());
    let revision = body.get("offerRevision").and_then(Value::as_i64);
    let (reply, changes) = match st.economy.apply_offer(&pid, &offer_id, revision) {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => {
            log::warn!(target: "powder.https", "inventory POST {pid} refused: {}", e.message);
            return Reply::json(&e.body(), "economy-error", e.status);
        }
        Err(e) => return Reply::internal_error(&e),
    };
    if let Some(mods) = &st.mods {
        let changes: Vec<Value> = changes.iter()
            .map(|ch| json!({"item_id": ch.item_id, "delta": ch.delta, "before": ch.before, "after": ch.after}))
            .collect();
        mods.lock().emit("purchase", &json!({"profile_id": pid, "offer_id": offer_id, "changes": changes}));
    }
    Reply::json(&reply, "economy", 200)
}

/// Store catalogue, filtered like the real service (assets.rs' OfferCatalog::query).
fn get_offers(c: &Ctx) -> Reply {
    let q = query(c.req);
    let number = |name: &str, default: i64| q.get(name).map_or(Ok(default), |v| v.trim().parse::<i64>());
    let (offset, limit) = match (number("offset", 0), number("limit", 100)) {
        (Ok(o), Ok(l)) => (o, l),
        (Err(e), _) | (_, Err(e)) => return Reply::internal_error(&format!("offers: offset/limit: {e}")),
    };
    let mut all = c.state.assets.catalog.query(q.get("tags"), q.get("type"), q.get("itemIds"), 0, usize::MAX);
    if c.state.assets.synthesizes_offers() && q.get("tags").is_none() && q.get("type").is_none() {
        for item in q.get("itemIds").unwrap_or("").split(',').filter(|s| !s.is_empty()) {
            let granted = all.iter().any(|o| o.get("items").and_then(Value::as_array).is_some_and(|items| {
                items.iter().any(|i| i.get("itemId").and_then(Value::as_str).is_some_and(|x| x.eq_ignore_ascii_case(item)))
            }));
            if let (false, Some(offer)) = (granted, crate::assets::synthetic_offer(item)) {
                log::info!(target: "powder.economy", "no offer grants item {item}: synthetic free offer {}",
                           offer["offerId"].as_str().unwrap_or_default());
                all.push(offer);
            }
        }
    }
    let offers = py_slice(all, offset, limit);
    let how = format!("catalog({})", offers.len());
    Reply::json(&json!({"offers": offers}), &how, 200)
}

// ---- profile entities (rider card, favorites) ---------------------------------------------------------------
static ENTITY_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f-]{36}$").unwrap());
const STORAGE_LINK_S: i64 = 3600;

/// An `offset` / `limit` parameter; one that is not a number is the default.
fn int_param(q: &Query, name: &str, default: i64) -> i64 {
    q.get(name).map_or(default, |v| v.trim().parse().unwrap_or(default))
}

/// A `type` filter ("" filters nothing, as in Python).
fn kind(q: &Query) -> Option<&str> {
    q.get("type").filter(|t| !t.is_empty())
}

/// Signs the upload links (putUrl / deleteUrl) of entity files; new on every start (the links last an hour).
static LINK_KEY: LazyLock<Vec<u8>> = LazyLock::new(|| crate::util::random_bytes(32));

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<u8>>();
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(message).finalize();
    Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize().into()
}

/// `<expiry>.<signature>`: lets the holder write one entity's file until the expiry (unix seconds), like an S3
/// presigned link (the game may upload without its ticket).
fn file_token(entity_id: &str, expires: i64) -> String {
    let mac = hmac_sha256(&LINK_KEY, format!("entity-file:{entity_id}:{expires}").as_bytes());
    format!("{expires}.{}", hex::encode(mac))
}

fn file_token_ok(entity_id: &str, token: &str) -> bool {
    let Some(expires) = token.split_once('.').and_then(|(e, _)| e.parse::<i64>().ok()) else { return false };
    let want = file_token(entity_id, expires);
    expires >= chrono::Utc::now().timestamp() && want.len() == token.len()
        && want.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// An entity with a file (extendedStorage, e.g. a saved replay or a challenge ghost) gets links to it on this
/// server, as Ubisoft's gave links to S3: putUrl / deleteUrl after a write (the game uploads there), getUrl /
/// headUrl on reads. `signed`: the write links carry a token (accounts required; see entity_file).
fn storage(req: &Req, mut entity: Entity, write: bool, signed: bool) -> Entity {
    let Some(Value::Object(ext)) = entity.get("extendedStorage") else { return entity };
    let id = entity.get("entityId").and_then(Value::as_str).unwrap_or_default();
    let url = format!("https://{}/_powder/entity-files/{id}", host(req));
    let expires = chrono::Utc::now() + chrono::Duration::seconds(STORAGE_LINK_S);
    let mut ext = ext.clone();
    ext.insert("provider".into(), json!("s3"));
    if write {
        let write_url = if signed { format!("{url}?t={}", file_token(id, expires.timestamp())) } else { url.clone() };
        ext.insert("putUrl".into(), json!(write_url));
        ext.insert("deleteUrl".into(), json!(write_url));
    }
    ext.insert("headUrl".into(), json!(url));
    ext.insert("getUrl".into(), json!(url));
    ext.insert("expirationTime".into(), json!(expires.format("%Y-%m-%dT%H:%M:%SZ").to_string()));
    entity.insert("extendedStorage".into(), Value::Object(ext));
    entity
}

fn entities_reply(req: &Req, items: Vec<Entity>) -> Reply {
    let how = format!("entities({})", items.len());
    let items: Vec<Value> = items.into_iter().map(|e| Value::Object(storage(req, e, false, false))).collect();
    Reply::json(&json!({"entities": items}), &how, 200)
}

/// The file behind an entity's links: PUT (upload), GET / HEAD (download), DELETE. With accounts required, a
/// write needs the link's token or a session of the entity's owner (403 otherwise); downloads stay open.
fn entity_file(c: &Ctx) -> Reply {
    let id = c.param("entity_id").to_lowercase();
    if !ENTITY_FILE.is_match(&id) {
        return empty(404, "?");
    }
    let entity = match c.state.entities.get(&id) {
        Ok(Some(e)) => e,
        Ok(None) => return empty(404, "?"),
        Err(e) => return Reply::internal_error(&e),
    };
    let method = c.req.method.as_str();
    if matches!(method, "PUT" | "POST" | "DELETE") {
        let guarded = match accounts_required(c) {
            Ok(g) => g,
            Err(reply) => return reply,
        };
        let owners = owner(c).is_some_and(|me| entity.get("profileId").and_then(Value::as_str) == Some(me.as_str()));
        if guarded && !owners && !query(c.req).get("t").is_some_and(|t| file_token_ok(&id, t)) {
            log::warn!(target: "powder.https", "entity file {id}: {method} from {} refused (no valid link token, \
                       not the owner's session)", c.req.client_ip());
            return empty(403, "forbidden");
        }
    }
    let path = c.state.config.data_dir().join("entity-files").join(&id);
    match method {
        "PUT" | "POST" => {
            let written = path.parent().map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::write(&path, &c.req.body));
            if let Err(e) = written.with_context(|| format!("writing {}", path.display())) {
                return Reply::internal_error(&e);
            }
            log::info!(target: "powder.https", "entity file {id} uploaded ({} B)", c.req.body.len());
            empty(200, "?")
        }
        "DELETE" => {
            remove_file(&path);
            empty(204, "?")
        }
        method => {
            if !path.exists() {
                return empty(404, "?");
            }
            if method == "GET" {
                // the access log skips /_powder/ paths
                let by = owner(c).unwrap_or_else(|| "?".into());
                log::info!(target: "powder.https", "entity file {id} downloaded by {by}");
            }
            file_reply(&path, "?")
        }
    }
}

/// Deletes a file if it is there (Python's `unlink(missing_ok=True)`); other failures are logged.
pub(super) fn remove_file(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            log::warn!(target: "powder.https", "deleting {}: {e}", path.display());
        }
        _ => {}
    }
}

/// GET /v2/profiles/{pid}/entities?type=&tags=&offset=&limit= (own player from the DB).
fn get_profile_entities(c: &Ctx) -> Reply {
    if let Err(refused) = reader(c) {
        return refused;
    }
    let q = query(c.req);
    match c.state.entities.for_profile(c.param("profile_id"), kind(&q), q.get("tags"), 0, usize::MAX) {
        Ok(items) => entities_reply(c.req, py_slice(items, int_param(&q, "offset", 0), int_param(&q, "limit", 100))),
        Err(e) => Reply::internal_error(&e),
    }
}

/// GET /v2/profiles/entities?profileIds=a,b&type=PROFILE (rider cards of friends and self), also with tags (the
/// friends' trails: type=FAVORITE_CHALLENGE&tags=TRAIL,WORLD_<id>,SANDBOX_A).
fn get_entities_batch(c: &Ctx) -> Reply {
    if let Err(refused) = reader(c) {
        return refused;
    }
    let q = query(c.req);
    let ids: Vec<String> = q.get("profileIds").unwrap_or("").split(',').filter(|p| !p.is_empty())
        .map(str::to_string).collect();
    match c.state.entities.for_profiles(&ids, kind(&q), q.get("tags"), 0, usize::MAX) {
        Ok(items) => entities_reply(c.req, py_slice(items, int_param(&q, "offset", 0), int_param(&q, "limit", 100))),
        Err(e) => Reply::internal_error(&e),
    }
}

fn get_entity(c: &Ctx) -> Reply {
    if let Err(refused) = reader(c) {
        return refused;
    }
    match c.state.entities.get(c.param("entity_id")) {
        Ok(Some(e)) => Reply::json(&Value::Object(storage(c.req, e, false, false)), "entity", 200),
        Ok(None) => error(1002, "entity not found", 404, "entity-missing"),
        Err(e) => Reply::internal_error(&e),
    }
}

/// An entity written (PUT, POST): its links to upload a file to.
fn entity_written(c: &Ctx, written: anyhow::Result<Entity>, signed: bool, how: impl Fn(&Entity) -> String)
                  -> Reply {
    match written {
        Ok(e) => Reply::json(&Value::Object(storage(c.req, e.clone(), true, signed)), &how(&e), 200),
        // EntityService::put's refusal (Python's ValueError); anything else failed
        Err(e) if e.to_string() == "entity without an owner" => error(1, &e.to_string(), 400, "bad-request"),
        Err(e) => Reply::internal_error(&e),
    }
}

fn text_field<'a>(m: &'a Entity, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// With accounts required: the entity (if it exists) must be the actor's, and a body naming an owner must name
/// the actor (EntityService::put would take it for a new entity).
fn check_entity_owner(c: &Ctx, me: &Actor, existing: Option<&Entity>, body: Option<&Entity>) -> Result<(), Reply> {
    if !me.guarded {
        return Ok(());
    }
    let others = |owner: Option<&str>| owner.is_some_and(|o| o.to_lowercase() != me.pid);
    if others(existing.and_then(|e| text_field(e, "profileId"))) {
        return Err(forbidden(c, "not your entity"));
    }
    if existing.is_none() && others(body.and_then(|b| text_field(b, "profileId"))) {
        return Err(forbidden(c, "not your profile"));
    }
    Ok(())
}

fn existing_entity(c: &Ctx, id: &str) -> Result<Option<Entity>, Reply> {
    c.state.entities.get(id).map_err(|e| Reply::internal_error(&e))
}

fn put_entity(c: &Ctx) -> Reply {
    allowed((|| {
        let me = acting(c)?;
        let body = json_object(c, |msg| error(1, msg, 400, "bad-request"))?;
        // no accounts: the session's player (none: the body must name the owner), as before
        let writer = if me.guarded { Some(me.pid.clone()) } else { owner(c) };
        if me.guarded {
            check_entity_owner(c, &me, existing_entity(c, c.param("entity_id"))?.as_ref(), Some(&body))?;
        }
        let written = c.state.entities.put(c.param("entity_id"), &body, writer.as_deref());
        Ok(entity_written(c, written, me.guarded,
                          |e| format!("entity-saved(rev {})", e.get("revision").unwrap_or(&Value::Null))))
    })())
}

/// POST /v2/profiles/entities and /v2/profiles/{pid}/entities (e.g. a new player's rider card).
fn post_entity(c: &Ctx) -> Reply {
    allowed((|| {
        let me = acting(c)?;
        let owner = match Some(c.param("profile_id")).filter(|p| !p.is_empty()) {
            None => me.pid.clone(),
            Some(p) if p.to_lowercase() == me.pid => me.pid.clone(),
            Some(_) if me.guarded => return Err(forbidden(c, "not your profile")),
            Some(p) => url_profile_id(p).ok_or_else(|| error(1, "bad profile id", 400, "bad-request"))?,
        };
        let body = json_object(c, |msg| error(1, msg, 400, "bad-request"))?;
        check_entity_owner(c, &me, None, Some(&body))?;
        Ok(entity_written(c, c.state.entities.create(&body, &owner), me.guarded, |_| "entity-created".into()))
    })())
}

fn delete_entity(c: &Ctx) -> Reply {
    allowed((|| {
        let id = c.param("entity_id");
        let me = acting(c)?;
        check_entity_owner(c, &me, existing_entity(c, id)?.as_ref(), None)?;
        let deleted = c.state.entities.delete(id).map_err(|e| Reply::internal_error(&e))?;
        if ENTITY_FILE.is_match(&id.to_lowercase()) {
            remove_file(&c.state.config.data_dir().join("entity-files").join(id.to_lowercase()));
        }
        Ok(Reply::json(&json!({}), if deleted { "entity-deleted" } else { "entity-absent" }, 200))
    })())
}

// ---- Ubisoft Connect ----------------------------------------------------------------------------------------
/// static3.cdn.ubi.com/.../test_internet.txt: Ubisoft Connect shows "no internet connection" unless this answers
/// like the real CDN.
fn internet_check(_: &Ctx) -> Reply {
    Reply::new(200, "text/plain; charset=utf-8", b"I AM ALIVE!".to_vec(), "internet-check")
}

/// Whose cloud save: the signed-in player (Connect's ticket). Without a session: on a server without accounts the
/// profile in the path (if shaped like one), else the default player; once accounts are required, a 401.
fn cloud_owner(c: &Ctx) -> Result<String, Reply> {
    if let Some(me) = owner(c) {
        return Ok(me.to_lowercase());
    }
    if accounts_required(c)? {
        return Err(no_session(c));
    }
    match url_profile_id(c.param("profile_id")) {
        Some(pid) => Ok(pid),
        None => default_player(c).map(|p| p.to_lowercase()).map_err(|e| Reply::internal_error(&e)),
    }
}

/// GET /v1/profiles/{pid}/entities?name=upcCloudSaveUplayId_<product>: Ubisoft Connect's cloud-save entity
/// (services/cloudsaves.rs), only when its files are all stored; other names: none.
fn get_entities_v1(c: &Ctx) -> Reply {
    let product = query(c.req).get("name").and_then(cloudsaves::product_of);
    let owner = match cloud_owner(c) {
        Ok(o) => o,
        Err(refused) => return refused,
    };
    let Some(product) = product.filter(|_| owner == c.param("profile_id").to_lowercase()) else {
        return Reply::json(&json!({"entities": []}), "entities-v1", 200);
    };
    match c.state.cloudsaves.offered(&owner, product) {
        Ok(Some(e)) => Reply::json(&json!({"entities": [e]}), "cloud-save", 200),
        Ok(None) => Reply::json(&json!({"entities": []}), "no-cloud-save", 200),
        Err(e) => Reply::internal_error(&e),
    }
}

/// A value as Python's repr() shows it (for the log).
fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(other) => crate::util::dumps(other),
    }
}

/// PUT /v1/profiles/entities/{entity_id} (Connect at game exit, before uploading the files) and
/// POST /v1/profiles/{pid}/entities (creating it; not recorded, so logged in full).
fn write_cloud_entity(c: &Ctx) -> Reply {
    let body = match serde_json::from_slice::<Value>(&c.req.body) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    };
    let product = body.as_ref().and_then(|b| b.get("name")).and_then(Value::as_str).and_then(cloudsaves::product_of);
    let (Some(mut body), Some(product)) = (body, product) else {
        return error(1, "not a cloud save", 400, "bad-request");
    };
    match cloud_entity(c, &mut body, product) {
        Ok(reply) => reply,
        Err(e) => Reply::internal_error(&e),
    }
}

fn cloud_entity(c: &Ctx, body: &mut Entity, product: u64) -> anyhow::Result<Reply> {
    let st = c.state;
    let owner = match cloud_owner(c) {
        Ok(o) => o,
        Err(refused) => return Ok(refused),
    };
    let path_pid = c.param("profile_id").to_lowercase();
    let guarded = match accounts_required(c) {
        Ok(g) => g,
        Err(reply) => return Ok(reply),
    };
    if guarded && !path_pid.is_empty() && path_pid != owner {
        return Ok(forbidden(c, "not your profile"));
    }
    let slots = body.get("obj").and_then(|o| o.get("slots")).and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_object)
        .map(|s| format!("({}, {})", py_repr(s.get("filename")), py_repr(s.get("size"))))
        .collect::<Vec<_>>().join(", ");
    // one save for shared products
    body.insert("name".into(), json!(cloudsaves::entity_name(cloudsaves::shared(product))));
    let entity_id = match c.params.get("entity_id") {
        None => {
            log::info!(target: "powder.https", "cloud save entity created by {owner}: product {product}, \
                       slots [{slots}]");
            match st.cloudsaves.entity(&owner, product)? {
                Some(existing) => existing.get("entityId").and_then(Value::as_str).unwrap_or_default().to_string(),
                None => uuid::Uuid::new_v4().to_string(),
            }
        }
        Some(id) => {
            if let Some(existing) = st.entities.get(id)? {
                if existing.get("profileId").and_then(Value::as_str) != Some(owner.as_str()) {
                    return Ok(error(1, "not your entity", 403, "forbidden"));
                }
                let revision = body.get("revision").filter(|r| !r.is_null());
                if revision.is_some_and(|r| Some(r) != existing.get("revision")) {
                    log::warn!(target: "powder.https", "cloud save of {owner}: revision {} written over {}",
                               py_repr(revision), py_repr(existing.get("revision")));
                }
            }
            log::info!(target: "powder.https", "cloud save of {owner} updated: product {product}, slots [{slots}]");
            id.clone()
        }
    };
    body.insert("type".into(), json!(cloudsaves::TYPE));
    let mut e = st.entities.put(&entity_id, body, Some(&owner))?;
    let how = format!("cloud-save-written(rev {})", e.get("revision").unwrap_or(&Value::Null));
    e.insert("name".into(), json!(cloudsaves::entity_name(product)));
    Ok(Reply::json(&Value::Object(e), &how, 200))
}

const EMPTY_S3_LISTING: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult \
    xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>savegames</Name><Prefix></Prefix><KeyCount>0</KeyCount>\
    <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated></ListBucketResult>";

/// The listing Connect makes when migrating from Ubisoft's old cloud saves (an S3 bucket listing): empty.
fn cloud_save_legacy_list(_: &Ctx) -> Reply {
    Reply::new(200, "application/xml; charset=utf-8", EMPTY_S3_LISTING.as_bytes().to_vec(), "cloud-save-legacy-list")
}

/// The links Connect gets from the demux cloudsave_service: PUT uploads a slot, GET / HEAD download it, DELETE.
fn cloud_save_file(c: &Ctx) -> Reply {
    let (product, slot) = (c.param("product"), c.param("slot"));
    let Ok(number) = product.trim().parse::<i64>() else {
        return Reply::internal_error(&format!("cloud save file: product {product:?} is not a number"));
    };
    match cloud_file(c, number, slot) {
        Ok(reply) => reply,
        Err(e) => match e.downcast_ref::<CloudSaveError>() {
            Some(CloudSaveError(msg)) => {
                log::warn!(target: "powder.https", "cloud save file {product}/{slot}: {msg}");
                empty(if msg.contains("link") { 403 } else if msg.contains("damaged") { 400 } else { 404 }, "?")
            }
            None => Reply::internal_error(&e),
        },
    }
}

fn cloud_file(c: &Ctx, product: i64, slot: &str) -> anyhow::Result<Reply> {
    let saves = &c.state.cloudsaves;
    let method = c.req.method.as_str();
    let product = u64::try_from(product).map_err(|_| CloudSaveError("unknown or expired link".into()))?;
    let link = saves.resolve(query(c.req).get("t").unwrap_or(""), product, slot, Some(method))?;
    match method {
        "DELETE" => {
            saves.remove(&link)?;
            Ok(empty(204, "cloud-save-delete"))
        }
        "PUT" => {
            saves.store(&link, &c.req.body, c.req.header("Content-MD5"))?;
            Ok(empty(200, "cloud-save-upload"))
        }
        // HEAD: hyper sends the Content-Length without the body
        _ => Ok(Reply::new(200, "application/octet-stream", saves.read(&link)?, "cloud-save-download")),
    }
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

    /// The Authorization header of a new session of this player.
    fn auth_for(st: &HttpState, pid: &str) -> String {
        format!("Ubi_v1 t={}", st.sessions.issue(st.accounts.player(pid).unwrap(), "127.0.0.1", "").ticket)
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

    /// Guesses sent at once: each is counted before its password is hashed, so only max_failures get hashed.
    #[test]
    fn concurrent_guesses_are_limited_too() {
        let st = Arc::new(limited(3));
        register(&st, "rider@example.com", "Rider");
        let barrier = Arc::new(std::sync::Barrier::new(12));
        let threads: Vec<_> = (0..12).map(|_| {
            let (st, barrier) = (st.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                body(&connect_login(&st, &basic("rider@example.com", "wrong password")))["message"].as_str()
                    .unwrap().to_string()
            })
        }).collect();
        let messages: Vec<String> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(messages.iter().filter(|m| *m == "Invalid credentials").count(), 3, "{messages:?}");
        assert_eq!(messages.iter().filter(|m| m.contains("Too many")).count(), 9);
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

    // ---- inventory, store, entities, cloud saves (tests/test_https.py, test_progression.py, test_cloudsaves.py,
    // test_ugc.py's entities with files) ----------------------------------------------------------------------
    use crate::assets::{Assets, OfferCatalog};
    use crate::constants::{CREDITS_ITEM, SPACE_ID};
    use crate::services::cloudsaves::CloudSaveService;
    use crate::services::Services;
    use bytes::Bytes;
    use md5::{Digest, Md5};
    use serde_json::json;

    const SKI_A: &str = "0a000000-0000-4000-8000-00000000000a";
    const SKI_B: &str = "0b000000-0000-4000-8000-00000000000b";
    const SKI_A_ITEM: &str = "1a000000-0000-4000-8000-00000000001a";
    const SKI_A_PRICE: i64 = 12_000;
    const CREDITS: i64 = 50_000;
    const FRIEND: &str = "f0000000-0000-4000-8000-0000000000f0";

    fn offers() -> Vec<Value> {
        vec![
            json!({"offerId": SKI_A, "type": "shop", "revision": 6, "nameId": "Ski A", "tags": ["SKI", "BASIC"],
                   "items": [{"itemId": SKI_A_ITEM.to_uppercase(), "quantity": 1},
                             {"itemId": CREDITS_ITEM, "quantity": -SKI_A_PRICE}]}),
            json!({"offerId": SKI_B, "type": "shop", "revision": 2, "tags": ["SKI", "DELUXE"],
                   "items": [{"itemId": "1b000000-0000-4000-8000-00000000001b", "quantity": 1}]}),
        ]
    }

    /// A state over a data folder (files: entity files, cloud saves) with a catalogue and the default player,
    /// who has CREDITS credits.
    fn files_state(dir: &std::path::Path, mods: Option<crate::mods::ModHost>) -> (HttpState, String) {
        let mut config = Config::default();
        config.server.data_dir = dir.to_path_buf();
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let assets = Assets { catalog: OfferCatalog::new(offers()), ..Default::default() };
        let services = Services::new(Arc::new(config), Arc::new(assets), Arc::new(crate::repos::Repos::new(db)),
                                     mods.map(|m| Arc::new(parking_lot::Mutex::new(m))));
        let st = HttpState { services: Arc::new(services), nex_public_key: "TESTKEY".into(), demux: None };
        let pid = st.accounts.default_profile_id().unwrap();
        st.accounts.player(&pid).unwrap();
        st.economy.grant(&pid, CREDITS_ITEM, CREDITS, "seed").unwrap().unwrap();
        (st, pid)
    }

    fn send(st: &HttpState, method: &str, path: &str, headers: &[(&str, &str)], data: &[u8]) -> super::Reply {
        let mut r = req(method, path, headers);
        r.body = Bytes::copy_from_slice(data);
        handle(st, &r)
    }

    fn send_json(st: &HttpState, method: &str, path: &str, headers: &[(&str, &str)], data: &Value) -> super::Reply {
        send(st, method, path, headers, serde_json::to_string(data).unwrap().as_bytes())
    }

    /// The path of one of our links (https://<host>/<path>).
    fn path_of(url: &str) -> String {
        url.split_once(".com").unwrap().1.to_string()
    }

    fn md5_b64(data: &[u8]) -> String {
        b64(&Md5::digest(data))
    }

    #[test]
    fn purchase_round_trip_and_the_mod_hears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mods = tmp.path().join("mods");
        std::fs::create_dir(&mods).unwrap();
        std::fs::write(mods.join("m.lua"), r#"return function(api)
            api.on("purchase", function(ev)
                local c = ev.changes[2]
                api.save("last", ev.offer_id .. " " .. c.item_id .. " " .. c.before .. "->" .. c.after)
            end)
        end"#).unwrap();
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Arc::new(crate::repos::Repos::new(db));
        let mut config = Config::default();
        config.mods.dir = mods;
        let (st, pid) = files_state(tmp.path(), Some(crate::mods::ModHost::load(&config, repos.clone())));
        let ticket = body(&game_login(&st))["ticket"].as_str().unwrap().to_string();
        let auth = format!("Ubi_v1 t={ticket}");
        let auth = [("Authorization", auth.as_str())];
        let inv = format!("/v1/profiles/{pid}/inventory");
        let credits = || {
            let r = handle(&st, &req("GET", &format!("{inv}?spaceId={SPACE_ID}"), &auth));
            assert_eq!(r.how, "inventory");
            let b = body(&r);
            b["items"].as_array().unwrap().iter().find(|i| i["itemId"] == CREDITS_ITEM).unwrap()["quantity"].as_i64()
        };
        assert_eq!(credits(), Some(CREDITS));
        let r = send_json(&st, "POST", &inv, &auth, &json!({"spaceId": SPACE_ID, "offerId": SKI_A, "offerRevision": 6}));
        assert_eq!((r.status, r.how.as_str()), (200, "economy"));
        let reply = body(&r);
        assert!(reply["transactionId"].as_str().is_some_and(|t| !t.is_empty()));
        let keys: Vec<&str> = reply.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["primaryStoreItemQuantity", "primaryStoreItemIsConsumable", "items", "additionalDetails",
                          "transactionId"]);
        assert_eq!(credits(), Some(CREDITS - SKI_A_PRICE));
        let heard = repos.kv_get("mod:m", "last").unwrap().unwrap();
        assert_eq!(heard, json!(format!("{SKI_A} {CREDITS_ITEM} {CREDITS}->{}", CREDITS - SKI_A_PRICE)));

        let r = send_json(&st, "POST", &inv, &auth, &json!({"spaceId": SPACE_ID, "offerId": "nope"}));
        assert_eq!((r.status, r.how.as_str()), (404, "economy-error"));
        assert_eq!(String::from_utf8(r.body).unwrap(),
                   r#"{"errorCode": 1001, "message": "offer nope not found", "httpCode": 404}"#);
        let r = send_json(&st, "POST", &inv, &auth, &json!({"spaceId": SPACE_ID}));
        assert_eq!((r.status, body(&r)["message"].as_str()), (400, Some("bad request: 'offerId'")));
        assert_eq!(send(&st, "POST", &inv, &auth, b"not json").status, 400);
        // someone this server has no record of owns nothing
        let r = handle(&st, &req("GET", &format!("/v1/profiles/{FRIEND}/inventory"), &[]));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("inventory(unknown player)", &br#"{"items": []}"#[..]));
    }

    #[test]
    fn offers_from_the_pack_catalogue() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _) = files_state(tmp.path(), None);
        let ids = |path: &str| {
            let r = handle(&st, &req("GET", path, &[]));
            body(&r)["offers"].as_array().unwrap().iter().map(|o| o["offerId"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&format!("/v1/spaces/{SPACE_ID}/offers?tags=SKI&offset=0&limit=100")), [SKI_A, SKI_B]);
        assert_eq!(ids(&format!("/v1/spaces/{SPACE_ID}/offers?tags=SKI,DELUXE&offset=0&limit=5")), [SKI_B]);
        assert_eq!(ids(&format!("/v1/spaces/{SPACE_ID}/offers?tags=SKI%2CBASIC")), [SKI_A]);
        assert_eq!(ids(&format!("/v1/spaces/{SPACE_ID}/offers?offset=1&limit=1")), [SKI_B]);
        assert_eq!(ids(&format!("/v1/spaces/{SPACE_ID}/offers?offset=-1")), [SKI_B], "Python's slices");
        let r = handle(&st, &req("GET", &format!("/v1/spaces/{SPACE_ID}/offers?type=shop"), &[]));
        assert_eq!(r.how, "catalog(2)");
        assert_eq!(handle(&st, &req("GET", &format!("/v1/spaces/{SPACE_ID}/offers?limit=x"), &[])).status, 500);
    }

    #[test]
    fn synthetic_offers_without_a_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, pid) = files_state(tmp.path(), None);
        let reward = "8d9430c2-d08f-493e-8028-2fc2d448fdb0";
        let query = |q: &str| body(&handle(&st, &req("GET", &format!("/v1/spaces/{SPACE_ID}/offers?{q}"), &[])))
            ["offers"].as_array().unwrap().clone();
        // an item no offer grants: one free synthetic offer for it
        let got = query(&format!("itemIds={reward}&offset=0&limit=100"));
        assert_eq!(got.len(), 1);
        let offer_id = got[0]["offerId"].as_str().unwrap().to_string();
        assert_eq!(offer_id, "28319567-752a-ec9b-258d-8a6771ed5815", "the item's UUID, every byte XOR 0xA5");
        assert_eq!(got[0]["items"], json!([{"itemId": reward, "quantity": 1}]));
        // an item the catalogue has: its real offer only; tag/type queries never synthesize
        let real = query(&format!("itemIds={}", SKI_A_ITEM.to_uppercase()));
        assert_eq!(real.iter().map(|o| o["offerId"].as_str().unwrap()).collect::<Vec<_>>(), [SKI_A]);
        assert_eq!(query("tags=REWARDS").len(), 0);
        // buying it grants +1 of exactly that item
        let auth = auth_for(&st, &pid);
        let auth = [("Authorization", auth.as_str())];
        let inv = format!("/v1/profiles/{pid}/inventory");
        let r = send_json(&st, "POST", &inv, &auth, &json!({"spaceId": SPACE_ID, "offerId": offer_id, "offerRevision": 1}));
        assert_eq!((r.status, r.how.as_str()), (200, "economy"));
        assert_eq!(st.repos.quantity(&pid, reward).unwrap(), 1);
        assert_eq!(st.repos.quantity(&pid, CREDITS_ITEM).unwrap(), CREDITS, "free");
        // not a synthetic id (not a UUID): still unknown
        let r = send_json(&st, "POST", &inv, &auth, &json!({"spaceId": SPACE_ID, "offerId": "nope"}));
        assert_eq!(r.status, 404);
        // with a pack: no synthesis
        let mut assets = Assets { catalog: OfferCatalog::new(offers()), ..Default::default() };
        assets.manifest = Some(Default::default());
        assert!(!assets.synthesizes_offers());
        assert!(crate::assets::synthetic_item(&offer_id).as_deref() == Some(reward));
    }

    #[test]
    fn entities_over_https() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, pid) = files_state(tmp.path(), None);
        let ticket = body(&game_login(&st))["ticket"].as_str().unwrap().to_string();
        let auth = format!("Ubi_v1 t={ticket}");
        let auth = [("Authorization", auth.as_str())];
        // own rider card: a new one created by the game, then changed and read back
        let r = send_json(&st, "POST", &format!("/v2/profiles/{pid}/entities"), &auth,
                          &json!({"type": "PROFILE", "obj": {"RIDER_CARD": {"title": 1}}}));
        assert_eq!(r.how, "entity-created");
        let card = body(&r);
        assert_eq!((card["profileId"].as_str(), card["revision"].as_i64()), (Some(pid.as_str()), Some(1)));
        let card_id = card["entityId"].as_str().unwrap();
        let own = body(&handle(&st, &req("GET", &format!("/v2/profiles/{pid}/entities?type=PROFILE"), &auth)));
        assert_eq!(own["entities"][0]["entityId"], card_id);
        let r = send_json(&st, "PUT", &format!("/v2/profiles/entities/{card_id}"), &auth,
                          &json!({"type": "PROFILE", "obj": {"RIDER_CARD": {"title": 99}}}));
        assert_eq!(r.how, "entity-saved(rev 2)");
        let saved = body(&r);
        assert_eq!((saved["obj"]["RIDER_CARD"]["title"].as_i64(), saved["profileId"].as_str()), (Some(99), Some(pid.as_str())));
        assert!(saved.get("extendedStorage").is_none(), "no file, no links");

        // batch: own card from the DB; a player this server does not know has none
        let r = handle(&st, &req("GET", &format!("/v2/profiles/entities?profileIds={pid},{FRIEND}&type=PROFILE"), &auth));
        assert_eq!(r.how, "entities(1)");
        let cards = body(&r);
        assert_eq!((cards["entities"][0]["profileId"].as_str(), cards["entities"][0]["obj"]["RIDER_CARD"]["title"].as_i64()),
                   (Some(pid.as_str()), Some(99)));

        // favorites: create (by the session's player), list by tags, delete
        let fav = body(&send_json(&st, "POST", "/v2/profiles/entities", &auth,
                                  &json!({"type": "FAVORITE_CHALLENGE", "tags": ["TRAIL"], "obj": {"c": 1}})));
        assert_eq!(fav["profileId"], pid.as_str());
        let fav_id = fav["entityId"].as_str().unwrap();
        let path = format!("/v2/profiles/{pid}/entities?type=FAVORITE_CHALLENGE&tags=TRAIL");
        let favs = body(&handle(&st, &req("GET", &path, &auth)));
        assert_eq!(favs["entities"].as_array().unwrap().len(), 1);
        assert_eq!(favs["entities"][0]["entityId"], fav_id);
        let r = handle(&st, &req("DELETE", &format!("/v2/profiles/entities/{fav_id}"), &auth));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("entity-deleted", &b"{}"[..]));
        assert!(st.entities.get(fav_id).unwrap().is_none());
        assert_eq!(handle(&st, &req("DELETE", &format!("/v2/profiles/entities/{fav_id}"), &auth)).how, "entity-absent");
        let r = handle(&st, &req("GET", &format!("/v2/profiles/entities/{fav_id}"), &auth));
        assert_eq!((r.status, String::from_utf8(r.body).unwrap()),
                   (404, r#"{"errorCode": 1002, "message": "entity not found", "httpCode": 404}"#.to_string()));
        let r = send(&st, "PUT", &format!("/v2/profiles/entities/{fav_id}"), &auth, b"");
        assert_eq!((r.status, r.how.as_str()), (400, "bad-request"));
        // no session and no owner: refused
        let r = send_json(&st, "PUT", "/v2/profiles/entities/00000000-0000-4000-8000-000000000001", &[], &json!({}));
        assert_eq!((r.status, body(&r)["message"].as_str()), (400, Some("entity without an owner")));
    }

    /// Saving a replay: an entity asking for extended storage gets a putUrl; reads get getUrl / headUrl.
    #[test]
    fn a_saved_replay_gets_its_file_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _) = files_state(tmp.path(), None);
        let created = body(&send_json(&st, "POST", "/v2/profiles/entities", &[], &json!({
            "type": "FAVORITE_REPLAY", "name": "DefaultEntityName", "tags": ["TRAIL", "REPLAY"],
            "obj": {"savename": "TrailReport_1"}, "extendedStorage": {"provider": "s3", "fileExtension": ""}})));
        let storage = &created["extendedStorage"];
        let keys: Vec<&str> = storage.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["provider", "fileExtension", "putUrl", "deleteUrl", "headUrl", "getUrl", "expirationTime"]);
        let id = created["entityId"].as_str().unwrap();
        assert_eq!(storage["putUrl"], format!("https://public-ubiservices.ubi.com/_powder/entity-files/{id}"));
        assert!(storage["expirationTime"].as_str().unwrap().ends_with('Z'));
        let path = path_of(storage["putUrl"].as_str().unwrap());
        assert_eq!(send(&st, "PUT", &path, &[], b"REPLAYDATA").status, 200);
        let read = body(&handle(&st, &req("GET", &format!("/v2/profiles/entities/{id}"), &[])));
        assert!(read["extendedStorage"].get("putUrl").is_none());
        assert_eq!(read["extendedStorage"]["getUrl"], storage["putUrl"]);
        let r = handle(&st, &req("GET", &path, &[]));
        assert_eq!((r.status, r.body.as_slice()), (200, &b"REPLAYDATA"[..]));
        assert_eq!(header(&r, "content-type"), Some("application/octet-stream"));
        // the links name the host the client asked for
        let read = body(&handle(&st, &req("GET", &format!("/v2/profiles/entities/{id}"), &[("Host", "localhost:8443")])));
        assert_eq!(read["extendedStorage"]["headUrl"], format!("https://localhost:8443/_powder/entity-files/{id}"));
        handle(&st, &req("DELETE", &format!("/v2/profiles/entities/{id}"), &[]));
        assert_eq!(handle(&st, &req("GET", &path, &[])).status, 404);
        assert_eq!(send(&st, "PUT", &path, &[], b"x").status, 404, "no entity, no file");
        assert_eq!(handle(&st, &req("GET", "/_powder/entity-files/..%2Fx", &[])).status, 404);
    }

    /// A ranked run's ghost (a BEST_GHOST_* entity with its file) is the best's associated_replay, which other
    /// players download; once the game deletes the entity the board says "none" again.
    #[test]
    fn a_ranked_ghost_is_offered_on_the_leaderboard() {
        use crate::nex::rmc::Variant;
        use crate::services::{leaderboards, stats::StatEvent};
        let tmp = tempfile::tempdir().unwrap();
        let (st, pid) = files_state(tmp.path(), None);
        let ghost = body(&send_json(&st, "POST", &format!("/v2/profiles/{pid}/entities"), &[], &json!({
            "type": "BEST_GHOST_CHALLENGE_PUBLIC_ALLTIME", "name": "ScimitarEntity",
            "tags": ["TRAIL", "WORLD_10af88fa9e4", "SANDBOX_A", "BEST_GHOST"],
            "obj": {"leaderboard": "public_challenge_ascending_ranked_ldb", "challenge": "Steep_A09", "score": 58250},
            "extendedStorage": {"provider": "s3", "fileExtension": ""}})));
        let id = ghost["entityId"].as_str().unwrap();
        assert_eq!(send(&st, "PUT", &path_of(ghost["extendedStorage"]["putUrl"].as_str().unwrap()), &[], b"GHOST").status, 200);
        let event = StatEvent { name: "ascending_public_challenge_ranked_score_event".into(), fields: vec![
            ("challenge_id_to_use".into(), Variant::String("Steep_A09".into())),
            ("replay".into(), Variant::String(id.into())), ("score".into(), Variant::String("58250".into()))] };
        st.stats.apply_events(&pid, &[event], "player_profile").unwrap();
        let board = "public_challenge_ascending_ranked_ldb:Steep_A09:infinite";
        let rows = leaderboards::ranks(&st.repos, board, 1, 1).unwrap().rows;
        assert_eq!(rows[0].replay, id);
        let read = body(&handle(&st, &req("GET", &format!("/v2/profiles/entities/{}", rows[0].replay), &[])));
        let r = handle(&st, &req("GET", &path_of(read["extendedStorage"]["getUrl"].as_str().unwrap()), &[]));
        assert_eq!(r.body, b"GHOST");
        handle(&st, &req("DELETE", &format!("/v2/profiles/entities/{id}"), &[]));
        assert_eq!(leaderboards::ranks(&st.repos, board, 1, 1).unwrap().rows[0].replay, "none");
    }

    /// The friends' trails come in one request for several players, filtered by the world's tags.
    #[test]
    fn friends_trails_by_world() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, pid) = files_state(tmp.path(), None);
        for world in ["WORLD_10af88fa9e4", "WORLD_7a954c6bd0"] {
            let e = json!({"type": "FAVORITE_CHALLENGE", "tags": ["TRAIL", world, "SANDBOX_A"], "obj": {"world": world}});
            st.entities.create(e.as_object().unwrap(), &pid).unwrap();
        }
        let q = format!("profileIds={pid}&type=FAVORITE_CHALLENGE&tags=TRAIL,WORLD_7a954c6bd0,SANDBOX_A");
        let found = body(&handle(&st, &req("GET", &format!("/v2/profiles/entities?{q}"), &[])));
        assert_eq!(found["entities"].as_array().unwrap().iter().map(|e| e["obj"]["world"].clone()).collect::<Vec<_>>(),
                   [json!("WORLD_7a954c6bd0")]);
        let all = body(&handle(&st, &req("GET", &format!("/v2/profiles/entities?profileIds={pid}&offset=1"), &[])));
        assert_eq!(all["entities"].as_array().unwrap().len(), 1);
    }

    // ---- cloud saves (tests/test_cloudsaves.py; the links come from the service, as the demux gets them) ------
    const PRODUCT: u64 = 3280;

    fn cloud_name(product: u64) -> String {
        format!("upcCloudSaveUplayId_{product}")
    }

    fn entity_body(revision: Option<i64>, sizes: &[i64]) -> Value {
        let slots: Vec<Value> = sizes.iter().enumerate().map(|(i, n)| json!({
            "id": i + 1, "filename": (i + 1).to_string(), "hash": "0".repeat(32), "size": n,
            "lastModified": "2026-01-01T00:00:00.000Z"})).collect();
        let mut body = json!({"spaceId": SPACE_ID, "name": cloud_name(PRODUCT), "tags": [], "type": "cloudSave",
                              "obj": {"slots": slots, "timestamp": "1700000000", "pc_name": "TEST-PC"}});
        if let Some(r) = revision {
            body["revision"] = json!(r);
        }
        body
    }

    /// The link the demux hands out for one slot (its path on this server).
    fn link(saves: &CloudSaveService, pid: &str, product: u64, slot: &str, md5: Option<&str>, method: &str) -> String {
        format!("/_powder/cloudsaves/{product}/{slot}?t={}", saves.link(pid, product, slot, md5, method).unwrap())
    }

    #[test]
    fn no_cloud_save_metadata() {
        let (st, pid) = with_player();
        let r = handle(&st, &req("GET", &format!("/v1/profiles/{pid}/entities?name={}", cloud_name(PRODUCT)), &[]));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("no-cloud-save", &br#"{"entities": []}"#[..]));
        let r = handle(&st, &req("GET", &format!("/v1/profiles/{pid}/entities?name=other"), &[]));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("entities-v1", &br#"{"entities": []}"#[..]));
    }

    #[test]
    fn a_save_goes_up_and_comes_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, me) = files_state(tmp.path(), None);
        let url = format!("/v1/profiles/{me}/entities?name={}", cloud_name(PRODUCT));
        assert_eq!(body(&handle(&st, &req("GET", &url, &[]))), json!({"entities": []}));
        // game exit: the entity first (creating it), then links, then the files
        let r = send_json(&st, "POST", &format!("/v1/profiles/{me}/entities"), &[], &entity_body(None, &[10, 20]));
        assert_eq!(r.how, "cloud-save-written(rev 1)");
        let created = body(&r);
        assert_eq!((created["name"].as_str(), created["type"].as_str()), (Some(cloud_name(PRODUCT).as_str()), Some("cloudSave")));
        let id = created["entityId"].as_str().unwrap();
        assert_eq!(body(&handle(&st, &req("GET", &url, &[]))), json!({"entities": []}), "files not there yet");
        let files: [(&str, &[u8]); 2] = [("1", b"first slot"), ("2", b"second slot!")];
        for (slot, data) in files {
            let md5 = md5_b64(data);
            let path = link(&st.cloudsaves, &me, PRODUCT, slot, Some(&md5), "PUT");
            let r = send(&st, "PUT", &path, &[("Content-MD5", &md5)], data);
            assert_eq!((r.status, r.how.as_str()), (200, "cloud-save-upload"));
        }
        let offered = body(&handle(&st, &req("GET", &url, &[])));
        assert_eq!(offered["entities"].as_array().unwrap().len(), 1);
        assert_eq!(offered["entities"][0]["entityId"], id);
        assert_eq!(offered["entities"][0]["obj"]["slots"][1]["filename"], "2");
        // next exit: PUT with the current revision; seen live: Connect writes to .../entitites/<id> (sic)
        let put = body(&send_json(&st, "PUT", &format!("/v1/profiles/entities/{id}"), &[], &entity_body(Some(1), &[10, 20])));
        assert_eq!(put["revision"], 2);
        let put = body(&send_json(&st, "PUT", &format!("/v1/profiles/entitites/{id}"), &[], &entity_body(Some(2), &[11, 22])));
        assert_eq!((put["revision"].as_i64(), put["obj"]["slots"][1]["size"].as_i64()), (Some(3), Some(22)));
        // another PC downloads (links for GET)
        for (slot, data) in files {
            let r = handle(&st, &req("GET", &link(&st.cloudsaves, &me, PRODUCT, slot, None, "GET"), &[]));
            assert_eq!((r.status, r.how.as_str(), r.body.as_slice()), (200, "cloud-save-download", data));
            assert_eq!(header(&r, "content-type"), Some("application/octet-stream"));
        }
        let get = link(&st.cloudsaves, &me, PRODUCT, "1", None, "GET");
        assert_eq!(handle(&st, &req("HEAD", &get, &[])).status, 200);
        assert_eq!(send(&st, "PUT", &get, &[], b"x").status, 403, "GET only");
        assert_eq!(handle(&st, &req("GET", &link(&st.cloudsaves, &me, PRODUCT, "3", None, "GET"), &[])).status, 404);
        // deleting
        let r = handle(&st, &req("DELETE", &link(&st.cloudsaves, &me, PRODUCT, "2", None, "DELETE"), &[]));
        assert_eq!((r.status, r.how.as_str()), (204, "cloud-save-delete"));
        assert!(!st.cloudsaves.path(&me, PRODUCT, "2").unwrap().exists());
    }

    /// Ubisoft Connect version (3279) and Steam version (3280): one cloud save, under the name asked for.
    #[test]
    fn steeps_two_versions_share_one_save() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, me) = files_state(tmp.path(), None);
        let mut b = entity_body(None, &[10, 20]);
        b["name"] = json!(cloud_name(3279));
        let created = body(&send_json(&st, "POST", &format!("/v1/profiles/{me}/entities"), &[], &b));
        assert_eq!(created["name"], cloud_name(3279));
        let id = created["entityId"].as_str().unwrap();
        assert_eq!(st.entities.get(id).unwrap().unwrap()["name"], cloud_name(PRODUCT), "kept under 3280");
        for slot in ["1", "2"] {
            let token = st.cloudsaves.link(&me, 3279, slot, None, "PUT").unwrap();
            let l = st.cloudsaves.resolve(&token, 3279, slot, None).unwrap();
            st.cloudsaves.store(&l, format!("slot {slot}").as_bytes(), None).unwrap();
        }
        let steam = body(&handle(&st, &req("GET", &format!("/v1/profiles/{me}/entities?name={}", cloud_name(PRODUCT)), &[])));
        assert_eq!((&steam["entities"][0]["entityId"], &steam["entities"][0]["name"]), (&json!(id), &json!(cloud_name(PRODUCT))));
        let r = handle(&st, &req("GET", &link(&st.cloudsaves, &me, PRODUCT, "1", None, "GET"), &[]));
        assert_eq!(r.body, b"slot 1");
        // creating it again (another PC's first exit) writes the same entity
        let again = body(&send_json(&st, "POST", &format!("/v1/profiles/{me}/entities"), &[], &entity_body(None, &[1, 2])));
        assert_eq!((again["entityId"].as_str(), again["revision"].as_i64()), (Some(id), Some(2)));
    }

    #[test]
    fn a_damaged_upload_never_replaces_a_stored_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, me) = files_state(tmp.path(), None);
        let good = link(&st.cloudsaves, &me, PRODUCT, "1", Some(&md5_b64(b"good data")), "PUT");
        assert_eq!(send(&st, "PUT", &good, &[], b"good data").status, 200, "MD5 from the link request");
        let newer = link(&st.cloudsaves, &me, PRODUCT, "1", Some(&md5_b64(b"newer data")), "PUT");
        assert_eq!(send(&st, "PUT", &newer, &[], b"newer da").status, 400, "cut off");
        assert_eq!(std::fs::read(st.cloudsaves.path(&me, PRODUCT, "1").unwrap()).unwrap(), b"good data");
    }

    #[test]
    fn links_and_entities_belong_to_one_player() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, host) = files_state(tmp.path(), None);
        register(&st, "host@example.com", "Host"); // takes over the default player
        let other = st.accounts.register("guest@example.com", PASSWORD, "Guest").unwrap().profile_id;
        let auth = auth_for(&st, &host);
        let auth = [("Authorization", auth.as_str())];
        let token = st.cloudsaves.link(&other, PRODUCT, "1", None, "PUT").unwrap();
        let r = send(&st, "PUT", &format!("/_powder/cloudsaves/{PRODUCT}/2?t={token}"), &[], b"x");
        assert_eq!((r.status, r.how.as_str(), r.body.len()), (403, "?", 0), "another slot");
        assert_eq!(handle(&st, &req("GET", &format!("/_powder/cloudsaves/{PRODUCT}/1?t=made-up"), &[])).status, 403);
        assert_eq!(handle(&st, &req("GET", &format!("/_powder/cloudsaves/-1/1?t={token}"), &[])).status, 403);
        let mut b = entity_body(None, &[10, 20]);
        b["type"] = json!("cloudSave");
        st.entities.put("00000000-0000-4000-8000-0000000000aa", b.as_object().unwrap(), Some(&other)).unwrap();
        let r = send_json(&st, "PUT", "/v1/profiles/entities/00000000-0000-4000-8000-0000000000aa", &auth,
                          &entity_body(Some(1), &[10, 20]));
        assert_eq!((r.status, r.how.as_str()), (403, "forbidden"), "not the requester's");
        let r = handle(&st, &req("GET", &format!("/v1/profiles/{other}/entities?name={}", cloud_name(PRODUCT)), &auth));
        assert_eq!(body(&r), json!({"entities": []}));
        let r = send_json(&st, "POST", &format!("/v1/profiles/{other}/entities"), &auth, &json!({"name": "x"}));
        assert_eq!((r.status, body(&r)["message"].as_str()), (400, Some("not a cloud save")));
        // creating one in another player's name, or anything without a session (accounts exist)
        let r = send_json(&st, "POST", &format!("/v1/profiles/{other}/entities"), &auth, &entity_body(None, &[1]));
        assert_eq!((r.status, body(&r)["message"].as_str()), (403, Some("not your profile")));
        assert!(st.cloudsaves.entity(&other, PRODUCT).unwrap().unwrap()["profileId"] == other.as_str());
        let r = send_json(&st, "PUT", "/v1/profiles/entities/00000000-0000-4000-8000-0000000000aa", &[],
                          &entity_body(Some(1), &[10, 20]));
        assert_eq!((r.status, r.how.as_str(), body(&r)["httpCode"].as_i64()), (401, "no-session", Some(401)));
        let r = handle(&st, &req("GET", &format!("/v1/profiles/{other}/entities?name={}", cloud_name(PRODUCT)), &[]));
        assert_eq!(r.status, 401);
    }

    /// Once accounts exist: no default player for requests without a session, and a player writes only its own
    /// inventory and entities (reads of others' stay allowed).
    #[test]
    fn accounts_keep_players_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, victim) = files_state(tmp.path(), None);
        register(&st, "host@example.com", "Host"); // the victim: the default player, taken over
        let attacker = st.accounts.register("guest@example.com", PASSWORD, "Guest").unwrap().profile_id;
        let (mine, theirs) = (auth_for(&st, &attacker), auth_for(&st, &victim));
        let (mine, theirs) = ([("Authorization", mine.as_str())], [("Authorization", theirs.as_str())]);
        let offer = json!({"spaceId": SPACE_ID, "offerId": SKI_A, "offerRevision": 6});
        let inv = format!("/v1/profiles/{victim}/inventory");
        // inventory: no ticket -> 401 (was: the default player); someone else's -> 403; unknown ids get no record
        let r = send_json(&st, "POST", &inv, &[], &offer);
        assert_eq!((r.status, r.how.as_str()), (401, "no-session"));
        assert_eq!(body(&r)["errorContext"], "Profiles Client Facade Service");
        let r = send_json(&st, "POST", &inv, &mine, &offer);
        assert_eq!((r.status, body(&r)["message"].as_str()), (403, Some("not your inventory")));
        let r = send_json(&st, "POST", &format!("/v1/profiles/{FRIEND}/inventory"), &mine, &offer);
        assert_eq!(r.status, 403);
        assert!(st.repos.player(FRIEND).unwrap().is_none(), "no player record for an unknown id");
        assert_eq!(send_json(&st, "POST", &inv, &theirs, &offer).how, "economy", "one's own: fine");
        assert_eq!(handle(&st, &req("GET", &inv, &mine)).how, "inventory", "reading another's: fine");
        assert_eq!(handle(&st, &req("GET", &inv, &[])).status, 401);

        // entities: the victim's card can be read, not changed, deleted or made in the victim's name
        let card = body(&send_json(&st, "POST", &format!("/v2/profiles/{victim}/entities"), &theirs,
                                   &json!({"type": "PROFILE", "obj": {"title": 1}})));
        let card_id = card["entityId"].as_str().unwrap().to_string();
        let path = format!("/v2/profiles/entities/{card_id}");
        let r = send_json(&st, "PUT", &path, &mine, &json!({"obj": {"title": 666}}));
        assert_eq!((r.status, body(&r)["message"].as_str()), (403, Some("not your entity")));
        assert_eq!(send_json(&st, "PUT", &path, &[], &json!({"obj": {}})).status, 401);
        assert_eq!(handle(&st, &req("DELETE", &path, &mine)).status, 403);
        assert_eq!(handle(&st, &req("DELETE", &path, &[])).status, 401);
        assert_eq!(st.entities.get(&card_id).unwrap().unwrap()["obj"]["title"], 1, "untouched");
        assert_eq!(body(&handle(&st, &req("GET", &path, &mine)))["obj"]["title"], 1);
        let r = send_json(&st, "POST", &format!("/v2/profiles/{victim}/entities"), &mine, &json!({"type": "X"}));
        assert_eq!((r.status, body(&r)["message"].as_str()), (403, Some("not your profile")));
        let r = send_json(&st, "POST", "/v2/profiles/entities", &mine, &json!({"type": "X", "profileId": victim}));
        assert_eq!(r.status, 403, "the body naming another owner");
        let r = send_json(&st, "PUT", "/v2/profiles/entities/00000000-0000-4000-8000-0000000000bb", &mine,
                          &json!({"type": "X", "profileId": victim}));
        assert_eq!(r.status, 403);
        assert_eq!(send_json(&st, "POST", "/v2/profiles/entities", &[], &json!({"type": "X"})).status, 401);
        // the cloud save entity through /v2 too
        let save = st.entities.put("00000000-0000-4000-8000-0000000000cc",
                                   entity_body(None, &[1]).as_object().unwrap(), Some(&victim)).unwrap();
        let r = send_json(&st, "PUT", &format!("/v2/profiles/entities/{}", save["entityId"].as_str().unwrap()), &mine,
                          &json!({"obj": {}}));
        assert_eq!(r.status, 403);
        // listings need a session
        let batch = format!("/v2/profiles/entities?profileIds={victim}");
        assert_eq!(handle(&st, &req("GET", &batch, &[])).status, 401);
        assert_eq!(handle(&st, &req("GET", &batch, &mine)).how, "entities(2)");
        assert_eq!(handle(&st, &req("GET", &format!("/v2/profiles/{victim}/entities"), &[])).status, 401);
        assert_eq!(handle(&st, &req("GET", &path, &[])).status, 401);
        // the own entity works as before
        let r = send_json(&st, "POST", &format!("/v2/profiles/{attacker}/entities"), &mine, &json!({"type": "X"}));
        assert_eq!((r.status, body(&r)["profileId"].as_str()), (200, Some(attacker.as_str())));
    }

    /// An entity's file: once accounts are required, writes need the putUrl's token (the game may upload without
    /// its ticket, as to S3) or the owner's session; downloads stay open.
    #[test]
    fn entity_files_are_written_by_their_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, victim) = files_state(tmp.path(), None);
        register(&st, "host@example.com", "Host");
        let attacker = st.accounts.register("guest@example.com", PASSWORD, "Guest").unwrap().profile_id;
        let (mine, theirs) = (auth_for(&st, &attacker), auth_for(&st, &victim));
        let (mine, theirs) = ([("Authorization", mine.as_str())], [("Authorization", theirs.as_str())]);
        let ghost = body(&send_json(&st, "POST", &format!("/v2/profiles/{victim}/entities"), &theirs, &json!({
            "type": "BEST_GHOST_CHALLENGE_PUBLIC_ALLTIME", "extendedStorage": {"provider": "s3", "fileExtension": ""}})));
        let storage = &ghost["extendedStorage"];
        let put = path_of(storage["putUrl"].as_str().unwrap());
        let (plain, token) = put.split_once("?t=").expect("a signed upload link");
        assert_eq!(storage["deleteUrl"], storage["putUrl"]);
        assert_eq!(path_of(storage["getUrl"].as_str().unwrap()), plain, "the read links are not signed");
        assert_eq!(send(&st, "PUT", &put, &[], b"GHOST").status, 200, "the link's token");
        assert_eq!(send(&st, "PUT", plain, &theirs, b"GHOST2").status, 200, "the owner's session");
        for (path, auth) in [(plain.to_string(), &mine[..]), (plain.to_string(), &[][..]),
                             (format!("{plain}?t=1.{}", &token[token.len() - 64..]), &[][..]),
                             (format!("{plain}?t={}", &token[..token.len() - 1]), &mine[..])] {
            assert_eq!(send(&st, "PUT", &path, auth, b"EVIL").status, 403, "{path}");
            assert_eq!(handle(&st, &req("DELETE", &path, auth)).status, 403, "{path}");
        }
        // another entity's token does not fit
        let other = body(&send_json(&st, "POST", "/v2/profiles/entities", &mine, &json!({
            "type": "FAVORITE_REPLAY", "extendedStorage": {"provider": "s3"}})));
        let other_put = path_of(other["extendedStorage"]["putUrl"].as_str().unwrap());
        let other_token = other_put.split_once("?t=").unwrap().1;
        assert_eq!(send(&st, "PUT", &format!("{plain}?t={other_token}"), &mine, b"EVIL").status, 403);
        let r = handle(&st, &req("GET", plain, &[]));
        assert_eq!((r.status, r.body.as_slice()), (200, &b"GHOST2"[..]), "downloads: open");
        assert!(super::file_token_ok(ghost["entityId"].as_str().unwrap(), token));
        assert!(!super::file_token_ok(ghost["entityId"].as_str().unwrap(),
                                      &super::file_token(ghost["entityId"].as_str().unwrap(), 1)), "expired");
    }

    /// RFC 4231 test case 2.
    #[test]
    fn hmac_sha256_vector() {
        assert_eq!(hex::encode(super::hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
                   "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    /// Without a cloud save, Connect lists the old system's slots first: empty.
    #[test]
    fn nothing_to_migrate_from_the_old_cloud_saves() {
        let (st, _) = with_player();
        let r = handle(&st, &req("GET", &format!("/_powder/cloudsaves/legacy-list/{PRODUCT}"), &[]));
        assert_eq!((r.status, r.how.as_str()), (200, "cloud-save-legacy-list"));
        assert_eq!(header(&r, "content-type"), Some("application/xml; charset=utf-8"));
        assert_eq!(String::from_utf8(r.body).unwrap(), "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult \
            xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>savegames</Name><Prefix></Prefix><KeyCount>0\
            </KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated></ListBucketResult>");
    }

    #[test]
    fn python_slices() {
        let v = || vec![1, 2, 3, 4];
        assert_eq!(super::py_slice(v(), 1, 2), [2, 3]);
        assert_eq!(super::py_slice(v(), -1, 100), [4]);
        assert_eq!(super::py_slice(v(), -3, 1), [2]);
        assert!(super::py_slice(v(), 2, -1).is_empty() && super::py_slice(v(), 9, 1).is_empty());
        assert_eq!(super::py_slice(v(), 0, -1), [1, 2, 3]);
    }
}
