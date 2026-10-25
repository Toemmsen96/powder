//! HTTPS for every UbiServices/Olympus hostname (the hosts file points them all at us), on one TLS listener
//! routed by the name the client asked for (SNI): Ubisoft Connect's demux (dmx.upc.ubisoft.com) is not HTTP,
//! everything else is.
//!
//! Before the pipeline, limits.rs: connections (in all, per address), TLS handshake and header timeouts, request
//! body sizes by path (413) and the bodies' memory budget.
//!
//! Request pipeline (outermost first), as in the Python server:
//!   access log      one line per request; uploaded remotelog/events bodies are kept (they explain client errors;
//!                   cut to 256 KiB each, the newest 200)
//!   CORS            Ubisoft Connect's web UI calls UbiServices from its embedded browser: preflights, Allow-Origin
//!                   (Ubisoft's origins only; other origins may only GET / HEAD, see connect_origin)
//!   mods            `api.before_http` hooks may answer a request themselves
//!   session         `Authorization: Ubi_v1 t=<ticket>` -> the player's session (handed to the routes)
//!   routes          method + path pattern -> handler, in the Python server's registration order:
//!                   routes_powder.rs (/_powder/: status, accounts, CA, overlay part, client mods),
//!                   routes_profile.rs (profile, friends, notifications, GraphQL, chat channels...),
//!                   routes_ubi.rs (sign-in, NEX discovery, inventory + store, profile entities + their files,
//!                   Connect's internet check and cloud saves), routes_ugc.rs (photos), websocket.rs (the handshake)
//!   fallback        the game-data pack's recorded reply, else the server's built-in one (builtin/), else `{}` +
//!                   an UNMATCHED warning (Connect's cached UI settings: 304 to keep its copy, 503 without one)
//!
//! Routes match on the path only (the host matters only for the pack); a route whose path matches but whose
//! method does not falls through to the next one and finally to the pack, as with aiohttp's catch-all route.
//! A websocket handshake the route accepted (101) is answered by hyper, then the connection is websocket.rs's.

pub mod limits;
pub mod routes_powder;
pub mod routes_profile;
pub mod routes_ubi;
pub mod routes_ugc;
pub mod websocket;

use crate::builtin::http as builtin_http;
use crate::constants::{APP_ID, DEMUX_HOST};
use crate::core::identity::parse_authorization;
use crate::core::sessions::PlayerSession;
use crate::services::Services;
use anyhow::{Context, Result};
use limits::{BodyError, ConnGuard, Gate, Limits};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use tokio::net::TcpListener;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

pub const JSON: &str = "application/json; charset=utf-8";
const SAVED_UPLOADS: [&str; 2] = ["/remotelog", "/profiles/me/events"];
/// Kept uploads (data/bodies/): each cut to this much (a remotelog or events batch is a few KB), and only the
/// newest so many (the oldest are deleted), so they cannot fill the disk.
const KEPT_BODY_MAX: usize = 256 << 10;
const KEPT_BODIES: usize = 200;
const SAVED_KINDS: [&str; 2] = ["remotelog", "events"];
const CORS_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS";
const CORS_EXPOSE: &str = "X-Total-Count, Ubi-TransactionId";

static GAME_CONFIG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)^/v\d+/applications/{APP_ID}/configuration$")).unwrap());

/// What every request needs; shared by all connections. The services are reached through it
/// (`state.accounts`, `state.economy`, ...).
pub struct HttpState {
    pub services: Arc<Services>,
    /// The key that signs the NEX handshake, as the rendezvous configuration publishes it.
    pub nex_public_key: String,
    /// Ubisoft Connect's demux, on the same TLS port (None: its connections are closed).
    pub demux: Option<Arc<crate::demux::DemuxDeps>>,
}

impl std::ops::Deref for HttpState {
    type Target = Services;

    fn deref(&self) -> &Services {
        &self.services
    }
}

/// One request, read in full.
pub struct Req {
    pub method: String,
    /// The hostname asked for (Host header, else SNI), lower case, no port.
    pub host: String,
    /// Path with the query string, as sent.
    pub path_qs: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub peer: SocketAddr,
}

impl Req {
    pub fn path(&self) -> &str {
        self.path_qs.split('?').next().unwrap_or("")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// The caller's address (Python's `client_ip`).
    pub fn client_ip(&self) -> String {
        self.peer.ip().to_string()
    }
}

/// A reply, plus how it was made ("mod", "gamedata:exact", "UNMATCHED", ...) for the access log.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub how: String,
}

impl Reply {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>, how: &str) -> Reply {
        Reply { status, headers: vec![("Content-Type".into(), content_type.into())], body, how: how.into() }
    }

    /// JSON as Python's `json.dumps` writes it (the routes' `json_response`).
    pub fn json(data: &Value, how: &str, status: u16) -> Reply {
        Reply::new(status, JSON, crate::util::dumps(data).into_bytes(), how)
    }

    /// What aiohttp answers when a handler fails.
    pub fn internal_error(e: &dyn std::fmt::Display) -> Reply {
        log::error!(target: "powder.https", "request failed: {e:#}");
        Reply::new(500, "text/plain; charset=utf-8", b"500 Internal Server Error\n\nServer got itself in trouble".to_vec(),
                   "error")
    }
}

// ---- routes -------------------------------------------------------------------------------------------------
/// What a route handler gets: the request, the path's `{name}` parts and the caller's session (if its ticket
/// is one of ours, or the lenient fallback found one).
pub struct Ctx<'a> {
    pub state: &'a HttpState,
    pub req: &'a Req,
    pub params: HashMap<String, String>,
    pub session: Option<Arc<PlayerSession>>,
}

impl Ctx<'_> {
    /// A `{name}` part of the path ("" if the pattern has none).
    pub fn param(&self, name: &str) -> &str {
        self.params.get(name).map(String::as_str).unwrap_or("")
    }
}

pub type Handler = fn(&Ctx) -> Reply;

pub struct Route {
    /// "GET" (also answers HEAD, as aiohttp's add_get), "POST", ... or "*".
    method: &'static str,
    pattern: Regex,
    handler: Handler,
}

impl Route {
    /// `pattern` in aiohttp's syntax: `{name}` matches one path segment, `{name:regex}` the regex.
    pub fn new(method: &'static str, pattern: &str, handler: Handler) -> Route {
        let mut re = String::from("^");
        let mut rest = pattern;
        while let Some(start) = rest.find('{') {
            re.push_str(&regex::escape(&rest[..start]));
            let end = start + rest[start..].find('}').expect("unclosed { in a route pattern");
            let (name, part) = rest[start + 1..end].split_once(':').unwrap_or((&rest[start + 1..end], "[^{}/]+"));
            re.push_str(&format!("(?P<{name}>{part})"));
            rest = &rest[end + 1..];
        }
        re.push_str(&regex::escape(rest));
        re.push('$');
        Route { method, pattern: Regex::new(&re).expect("route pattern"), handler }
    }

    fn matches(&self, method: &str, path: &str) -> Option<HashMap<String, String>> {
        let method_ok = self.method == "*" || self.method == method || (self.method == "GET" && method == "HEAD");
        if !method_ok {
            return None;
        }
        let caps = self.pattern.captures(path)?;
        Some(self.pattern.capture_names().flatten()
            .filter_map(|n| Some((n.to_string(), caps.name(n)?.as_str().to_string()))).collect())
    }
}

/// Every route, in the Python server's registration order (routes_powder, routes_profile, routes_ubi,
/// routes_ugc, the websocket).
static ROUTES: LazyLock<Vec<Route>> = LazyLock::new(|| {
    routes_powder::routes().into_iter()
        .chain(routes_profile::routes())
        .chain(routes_ubi::routes())
        .chain(routes_ugc::routes())
        .chain(websocket::routes())
        .collect()
});

/// `Authorization: Ubi_v1 t=<ticket>` -> the session it belongs to (Python's session_middleware).
pub fn session_of(state: &HttpState, req: &Req) -> anyhow::Result<Option<Arc<PlayerSession>>> {
    let (scheme, ticket) = parse_authorization(req.header("Authorization"));
    if scheme.eq_ignore_ascii_case("ubi_v1") && !ticket.is_empty() {
        return state.accounts.session_for_ticket(&ticket, &req.client_ip());
    }
    Ok(None)
}

/// After the mods: the session, then the first matching route, else the fallback.
fn routed(state: &HttpState, req: &Req, routes: &[Route]) -> Reply {
    let session = match session_of(state, req) {
        Ok(s) => s,
        Err(e) => return Reply::internal_error(&e),
    };
    for route in routes {
        if let Some(params) = route.matches(&req.method, req.path()) {
            return (route.handler)(&Ctx { state, req, params, session });
        }
    }
    fallback(state, req)
}

/// The whole pipeline for one request.
pub fn handle(state: &HttpState, req: &Req) -> Reply {
    if req.method == "POST" && SAVED_UPLOADS.iter().any(|s| req.path().ends_with(s)) {
        keep_upload(state, req);
    }
    let origin = req.header("Origin").map(str::to_string);
    if let Some(foreign) = origin.as_deref().filter(|o| !connect_origin(o) && !same_origin(req, o)) {
        if !matches!(req.method.as_str(), "GET" | "HEAD") {
            // a web page elsewhere (the hosts file sends the player's browser here too): no preflight, no writes
            log::warn!(target: "powder.https", "{} {} from {} refused: origin {foreign:?} is not Ubisoft Connect's",
                       req.method, req.path(), req.client_ip());
            let reply = Reply::json(&json!({"errorCode": 1, "message": "origin not allowed", "httpCode": 403}),
                                    "cors-refused", 403);
            access_log(req, &reply.how, reply.status, &reply.body.len().to_string());
            return reply;
        }
    }
    let origin = origin.filter(|o| connect_origin(o));
    let mut reply = match &origin {
        Some(_) if req.method == "OPTIONS" && req.headers.contains_key("Access-Control-Request-Method") => {
            let mut r = Reply { status: 200, headers: vec![], body: vec![], how: "cors-preflight".into() };
            r.headers.push(("Access-Control-Allow-Methods".into(), CORS_METHODS.into()));
            let asked = req.header("Access-Control-Request-Headers").unwrap_or("");
            r.headers.push(("Access-Control-Allow-Headers".into(), asked.into()));
            r
        }
        _ => from_mods(state, req).unwrap_or_else(|| routed(state, req, &ROUTES)),
    };
    if reply.how == websocket::HOW && reply.status == 101 {
        return reply; // the upgrade: no CORS (as aiohttp's for a prepared reply), logged when the socket closes
    }
    if let Some(origin) = origin {
        // set, not added: a route's own ("*" on the overlay part) is replaced, as aiohttp's headers[...] = does
        let cors = [("Access-Control-Allow-Origin", origin.as_str()), ("Access-Control-Allow-Credentials", "true"),
                    ("Access-Control-Expose-Headers", CORS_EXPOSE)];
        for (name, value) in cors {
            reply.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
            reply.headers.push((name.into(), value.into()));
        }
    }
    access_log(req, &reply.how, reply.status, &reply.body.len().to_string());
    reply
}

/// Whether a request's Origin is Ubisoft Connect's embedded browser: its UI and overlay come from Ubisoft's CDN
/// (https://connect.cdn.ubisoft.com, https://<env>connect.cdn.ubisoft.com, ...), so https and a ubisoft.com /
/// ubi.com host. Only these get CORS headers (with credentials); a foreign origin's GET / HEAD is answered
/// without them (the browser hides the reply), anything else from one is refused. The game, the launcher and
/// Connect's own client send no Origin.
fn connect_origin(origin: &str) -> bool {
    let Some(rest) = origin.get(..8).filter(|s| s.eq_ignore_ascii_case("https://")).map(|_| &origin[8..]) else {
        return false;
    };
    let host = rest.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
        .map_or(rest, |(h, _)| h).to_ascii_lowercase();
    let valid = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    valid && ["ubisoft.com", "ubi.com"].iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// A page this server served itself (e.g. a mod's, such as the admin page) calling it back: the browser sets
/// Origin to the page's own scheme + host, which no other site can claim. Not cross-origin: no CORS headers
/// needed, and not refused.
fn same_origin(req: &Req, origin: &str) -> bool {
    req.header("host").is_some_and(|host| !host.is_empty() && origin.eq_ignore_ascii_case(&format!("https://{host}")))
}

/// One line per request (the size is "-" for a websocket, as aiohttp's content_length is None).
pub fn access_log(req: &Req, how: &str, status: u16, size: &str) {
    let qs: String = req.path_qs.chars().take(160).collect();
    let line = format!("{} {}{qs} -> {how} {status} ({size}B)", req.method, req.host);
    if how == "UNMATCHED" {
        log::warn!(target: "powder.https", "{line}");
    } else if req.path().starts_with("/_powder/") {
        log::debug!(target: "powder.https", "{line}"); // the launcher polls these every few seconds
    } else {
        log::info!(target: "powder.https", "{line}");
    }
}

fn keep_upload(state: &HttpState, req: &Req) {
    let kind = if req.path().ends_with("/remotelog") { "remotelog" } else { "events" };
    let dir: PathBuf = state.config.data_dir().join("bodies");
    let name = chrono::Local::now().format(&format!("{kind}-%Y%m%d-%H%M%S-%6f.txt")).to_string();
    let body = &req.body[..req.body.len().min(KEPT_BODY_MAX)];
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(name), body)) {
        log::warn!(target: "powder.https", "keeping a {kind} upload: {e}");
    }
    if body.len() < req.body.len() {
        log::debug!(target: "powder.https", "kept {kind} upload cut to {} of {} bytes", body.len(), req.body.len());
    }
    prune_kept(&dir, KEPT_BODIES);
}

/// Delete the oldest kept uploads beyond the newest `keep` (names sort by time after their kind).
fn prune_kept(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut kept: Vec<(String, PathBuf)> = entries.filter_map(Result::ok).filter_map(|e| {
        let name = e.file_name().into_string().ok()?;
        let (kind, stamp) = name.split_once('-')?;
        (SAVED_KINDS.contains(&kind) && name.ends_with(".txt")).then(|| (stamp.to_string(), e.path()))
    }).collect();
    if kept.len() <= keep {
        return;
    }
    kept.sort();
    for (_, path) in &kept[..kept.len() - keep] {
        let _ = std::fs::remove_file(path); // another request may have deleted it already
    }
}

/// The request as a mod sees it: {method, host, path, query, headers (lower-case names), body (text), peer}.
pub fn mod_request(req: &Req) -> Value {
    let headers: Map<String, Value> = req
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), json!(v.to_str().ok()?))))
        .collect();
    json!({
        "method": req.method,
        "host": req.host,
        "path": req.path(),
        "query": req.path_qs.split_once('?').map(|(_, q)| q).unwrap_or(""),
        "headers": headers,
        "body": String::from_utf8_lossy(&req.body),
        "peer": req.peer.ip().to_string(),
    })
}

/// A mod's answer, if one gave it: {status = 200, body = text or a table (sent as JSON), headers = {},
/// content_type}.
fn from_mods(state: &HttpState, req: &Req) -> Option<Reply> {
    let mods = state.mods.as_ref()?;
    let answer = mods.lock().before_http(&req.method, req.path(), &mod_request(req))?;
    let status = answer.get("status").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()).unwrap_or(200);
    let (body, default_type) = match answer.get("body") {
        None | Some(Value::Null) => (vec![], "text/plain; charset=utf-8"),
        Some(Value::String(s)) => (s.clone().into_bytes(), "text/plain; charset=utf-8"),
        Some(v) => (crate::util::dumps(v).into_bytes(), JSON),
    };
    let content_type = answer.get("content_type").and_then(Value::as_str).unwrap_or(default_type);
    let mut reply = Reply::new(status, content_type, body, "mod");
    if let Some(Value::Object(h)) = answer.get("headers") {
        for (k, v) in h {
            let v = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            reply.headers.push((k.clone(), v));
        }
    }
    Some(reply)
}

/// Game-wide data from the game-data pack for anything without a route. Requests from Ubisoft Connect (an app
/// id other than the game's) try Connect's part of the pack first. Without a pack reply: the built-in replies.
fn fallback(state: &HttpState, req: &Req) -> Reply {
    let assets = &state.assets;
    let app_id = req.header("Ubi-AppId").unwrap_or("").to_lowercase();
    let order = if !app_id.is_empty() && app_id != APP_ID {
        [&assets.connect_http, &assets.http]
    } else {
        [&assets.http, &assets.connect_http]
    };
    let hit = order.iter().find_map(|f| f.lookup(&req.method, &req.host, &req.path_qs));
    if let Some((fixture, tier)) = hit {
        let body = storm_config(state, req.path(), &fixture.body);
        return Reply::new(fixture.status, &fixture.content_type, body, &format!("gamedata:{tier}"));
    }
    let cached_ui = builtin_http::keep_cached(req.path()); // Connect's UI settings: never overwrite its cache
    let has = |name| req.header(name).is_some_and(|v| !v.is_empty());
    if cached_ui && (has("If-None-Match") || has("If-Modified-Since")) {
        return Reply { status: 304, headers: vec![], body: vec![], how: "not-modified".into() };
    }
    let query = builtin_http::Query::parse(req.path_qs.split_once('?').map(|(_, q)| q).unwrap_or(""));
    if let Some(built) = builtin_http::lookup(&req.method, req.path(), &query) {
        // no pack (or not in it): the server's own reply
        let body = storm_config(state, req.path(), &builtin_http::body(&built));
        return Reply::new(200, JSON, body, "builtin");
    }
    Reply::new(if cached_ui { 503 } else { 200 }, JSON, b"{}".to_vec(), "UNMATCHED")
}

/// The game's SDK configuration names its peer-to-peer helpers (Storm): this server, whether the reply comes
/// from the pack (recorded: Ubisoft's hosts) or not.
fn storm_config(state: &HttpState, path: &str, body: &[u8]) -> Vec<u8> {
    if !GAME_CONFIG.is_match(path) {
        return body.to_vec();
    }
    let cfg = &state.config;
    let settings = (cfg.multiplayer.storm == "powder").then(|| storm_settings(&cfg.server.public_host,
                                                                                cfg.server.storm_port));
    with_storm(body, settings.as_ref())
}

/// Where the games reach Storm: the address they reach the game server at (an address, not a hostname, so no
/// hosts entries are needed).
pub fn storm_settings(host: &str, port: u16) -> Value {
    json!({
        "detectUrl1": format!("{host}:{port}"),
        "detectUrl2": format!("{host}:{}", port + 2),
        "traversalUrl": format!("{host}:{}", port + 5),
        "routerProvisioningUrl": "https://public-ubiservices.ubi.com/v1/storm/router", // relay: never asked
    })
}

/// The configuration with these Storm settings ("storm" and "punch"); None: none (multiplayer off).
pub fn with_storm(body: &[u8], settings: Option<&Value>) -> Vec<u8> {
    let Ok(mut data) = serde_json::from_slice::<Value>(body) else { return body.to_vec() };
    let Some(cfg) = data.get_mut("configuration").and_then(Value::as_object_mut) else { return body.to_vec() };
    cfg.insert("storm".into(), settings.cloned().unwrap_or_else(|| json!({})));
    match settings {
        Some(s) => {
            cfg.insert("punch".into(), s.clone());
        }
        None => {
            cfg.shift_remove("punch");
        }
    }
    crate::util::dumps(&data).into_bytes()
}

fn to_response(reply: Reply) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(reply.body)));
    *resp.status_mut() = hyper::StatusCode::from_u16(reply.status).unwrap_or(hyper::StatusCode::OK);
    for (k, v) in reply.headers {
        if let (Ok(name), Ok(value)) = (hyper::header::HeaderName::try_from(k), HeaderValue::try_from(v)) {
            resp.headers_mut().append(name, value);
        }
    }
    resp
}

/// A request refused before the pipeline (its body): UbiServices' error shape, and the connection closed (its
/// body was not read to the end).
fn refused(req: &Req, status: u16, message: &str, how: &str) -> Response<Full<Bytes>> {
    let mut reply = Reply::json(&json!({"errorCode": 1, "message": message, "httpCode": status}), how, status);
    reply.headers.push(("Connection".into(), "close".into()));
    log::warn!(target: "powder.https", "{} {}{} from {}: {message}", req.method, req.host, req.path(), req.peer);
    access_log(req, how, status, &reply.body.len().to_string());
    to_response(reply)
}

async fn serve_request(state: Arc<HttpState>, gate: Arc<Gate>, conn: Arc<ConnGuard>, sni: String, peer: SocketAddr,
                       mut req: Request<Incoming>) -> Result<Response<Full<Bytes>>> {
    // kept for the websocket route; hyper hands over the connection once its 101 is sent
    let upgrade = websocket::is_upgrade(req.headers()).then(|| hyper::upgrade::on(&mut req));
    let (parts, body) = req.into_parts();
    let host = parts.headers.get("host").and_then(|h| h.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or("").to_lowercase()).filter(|h| !h.is_empty()).unwrap_or(sni);
    let path_qs = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let mut req = Req { method: parts.method.to_string(), host, path_qs, headers: parts.headers, body: Bytes::new(),
                        peer };
    let (limit, class) = limits::body_rules(req.path());
    let timeout = gate.limits.body_timeout(class);
    // held until the reply is made: the body counts against the budget while it is in memory
    let _budget = match tokio::time::timeout(timeout, limits::read_body(body, limit, gate.budget(class))).await {
        Ok(Ok((bytes, held))) => {
            req.body = bytes;
            held
        }
        Ok(Err(BodyError::TooLarge)) => {
            let message = format!("Request body too large (at most {limit} bytes).");
            return Ok(refused(&req, 413, &message, "body-too-large"));
        }
        Ok(Err(BodyError::Failed(e))) => anyhow::bail!("{e}"),
        Err(_) => {
            let message = format!("Request body not received within {} s.", timeout.as_secs());
            return Ok(refused(&req, 408, &message, "body-timeout"));
        }
    };
    // the pipeline blocks (SQLite, scrypt for password sign-ins, the mods' lock): off the async workers
    let shared = state.clone();
    let (reply, req) = tokio::task::spawn_blocking(move || (handle(&shared, &req), req)).await?;
    if let Some(upgrade) = upgrade.filter(|_| reply.status == 101 && reply.how == websocket::HOW) {
        tokio::spawn(async move {
            websocket::run(state, req, upgrade).await;
            drop(conn); // the websocket is this connection: it counts until it closes
        });
    }
    Ok(to_response(reply))
}

/// Accept TLS connections on `addr` forever. HTTP ones go through [`handle`]; the demux is not ported yet, so
/// its connections are closed (logged).
pub async fn serve(addr: SocketAddr, tls: Arc<ServerConfig>, state: Arc<HttpState>) -> Result<()> {
    let listener = TcpListener::bind(addr).await.with_context(|| format!("HTTPS on {addr}"))?;
    log::info!(target: "powder.https", "listening on {addr}");
    serve_on(listener, tls, state).await
}

/// [`serve`] on a listener that is already bound.
pub async fn serve_on(listener: TcpListener, tls: Arc<ServerConfig>, state: Arc<HttpState>) -> Result<()> {
    serve_limited(listener, tls, state, Limits::default()).await
}

/// [`serve_on`] with these limits.
pub async fn serve_limited(listener: TcpListener, tls: Arc<ServerConfig>, state: Arc<HttpState>, limits: Limits)
                           -> Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    let gate = Gate::new(limits);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                log::warn!(target: "powder.tls", "accept: {e}");
                continue;
            }
        };
        let Some(conn) = gate.admit(peer.ip()) else { continue }; // dropped: closed
        let conn = Arc::new(conn);
        let acceptor = acceptor.clone();
        let (state, gate) = (state.clone(), gate.clone());
        tokio::spawn(async move {
            let stream = match tokio::time::timeout(gate.limits.tls_handshake, acceptor.accept(tcp)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    log::debug!(target: "powder.tls", "handshake with {peer} failed: {e}");
                    return;
                }
                Err(_) => {
                    log::debug!(target: "powder.tls", "handshake with {peer} timed out");
                    return;
                }
            };
            let sni = stream.get_ref().1.server_name().unwrap_or("").to_lowercase();
            if sni.is_empty() {
                log::debug!(target: "powder.tls", "tls ok {peer} for (no SNI)"); // the launcher's status checks
            } else {
                log::info!(target: "powder.tls", "tls ok {peer} for {sni}");
            }
            if sni == DEMUX_HOST {
                match state.demux.clone() {
                    Some(deps) => {
                        if let Err(e) = crate::demux::run(stream, peer, deps).await {
                            log::warn!(target: "powder.demux", "demux connection {peer} failed: {e:#}");
                        }
                    }
                    None => log::warn!(target: "powder.tls", "demux connection from {peer} closed: no demux"),
                }
                return;
            }
            let header_read = gate.limits.header_read;
            let service = service_fn(move |req| serve_request(state.clone(), gate.clone(), conn.clone(), sni.clone(),
                                                              peer, req));
            let conn = http1_builder(header_read).serve_connection(TokioIo::new(stream), service).with_upgrades();
            if let Err(e) = conn.await {
                log::debug!(target: "powder.https", "connection from {peer}: {e}");
            }
        });
    }
}

/// hyper's HTTP/1 with a timer, so that a client gets `header_read` to send its headers (and an idle keep-alive
/// connection is closed after it).
fn http1_builder(header_read: std::time::Duration) -> http1::Builder {
    let mut builder = http1::Builder::new();
    builder.timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(header_read);
    builder
}

/// Plain HTTP serving fixed bytes at a few paths (the local CA's revocation list, which Windows checks for
/// Ubisoft Connect). `body` is called per request; anything else is a 404.
pub async fn serve_plain<F>(addr: SocketAddr, paths: Vec<String>, content_type: &'static str, body: F) -> Result<()>
where
    F: Fn() -> Result<Vec<u8>> + Send + Sync + 'static,
{
    let listener = TcpListener::bind(addr).await.with_context(|| format!("HTTP on {addr}"))?;
    log::info!(target: "powder.https", "plain HTTP on {addr} ({})", paths.join(", "));
    let shared = Arc::new((paths, body));
    let gate = Gate::new(Limits::default());
    loop {
        let Ok((tcp, peer)) = listener.accept().await else { continue };
        let Some(conn) = gate.admit(peer.ip()) else { continue };
        let (shared, header_read) = (shared.clone(), gate.limits.header_read);
        tokio::spawn(async move {
            let _conn = conn;
            let service = service_fn(move |req: Request<Incoming>| {
                let shared = shared.clone();
                async move {
                    let (paths, body) = &*shared;
                    let reply = match paths.iter().any(|p| p == req.uri().path()).then(body) {
                        Some(Ok(bytes)) => Reply::new(200, content_type, bytes, "plain"),
                        Some(Err(e)) => {
                            log::warn!(target: "powder.https", "{}: {e:#}", req.uri().path());
                            Reply::new(500, "text/plain", vec![], "plain")
                        }
                        None => Reply::new(404, "text/plain", vec![], "plain"),
                    };
                    Ok::<_, std::convert::Infallible>(to_response(reply))
                }
            });
            let _ = http1_builder(header_read).serve_connection(TokioIo::new(tcp), service).await;
        });
    }
}

/// Shared by the HTTPS tests: a state over an in-memory database, requests, reply headers.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::assets::{Assets, HttpFixture, HttpFixtures};
    use crate::config::Config;
    use crate::db::Db;
    use crate::mods::ModHost;
    use crate::repos::Repos;
    use parking_lot::Mutex;

    pub fn state_with(config: Config, fixtures: Vec<HttpFixture>, mods: Option<ModHost>) -> HttpState {
        let assets = Assets { http: HttpFixtures::new(fixtures), ..Default::default() };
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let mods = mods.map(|m| Arc::new(Mutex::new(m)));
        let services = Services::new(Arc::new(config), Arc::new(assets), Arc::new(Repos::new(db)), mods);
        HttpState { services: Arc::new(services), nex_public_key: "TESTKEY".into(), demux: None }
    }

    pub fn state(fixtures: Vec<HttpFixture>, mods: Option<ModHost>) -> HttpState {
        state_with(Config::default(), fixtures, mods)
    }

    pub fn req_from(peer: &str, method: &str, path_qs: &str, headers: &[(&str, &str)]) -> Req {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        Req { method: method.into(), host: "public-ubiservices.ubi.com".into(), path_qs: path_qs.into(),
              headers: map, body: Bytes::new(), peer: format!("{peer}:5000").parse().unwrap() }
    }

    pub fn req(method: &str, path_qs: &str, headers: &[(&str, &str)]) -> Req {
        req_from("127.0.0.1", method, path_qs, headers)
    }

    pub fn header<'a>(r: &'a Reply, name: &str) -> Option<&'a str> {
        r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The reply's body as JSON.
    pub fn body(r: &Reply) -> Value {
        serde_json::from_slice(&r.body).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&r.body)))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::assets::HttpFixture;
    use crate::config::Config;
    use crate::mods::ModHost;

    #[test]
    fn unmatched_is_empty_json() {
        let r = handle(&state(vec![], None), &req("GET", "/v1/nothing", &[]));
        assert_eq!((r.status, r.body.as_slice(), r.how.as_str()), (200, &b"{}"[..], "UNMATCHED"));
        assert_eq!(header(&r, "content-type"), Some(JSON));
    }

    #[test]
    fn builtin_replies_without_a_pack() {
        let st = state(vec![], None);
        let path = format!("/v1/applications/{APP_ID}/configuration");
        let r = handle(&st, &req("GET", &path, &[]));
        assert_eq!((r.status, r.how.as_str()), (200, "builtin"));
        assert_eq!(header(&r, "content-type"), Some(JSON));
        // storm_config applies: the default config has Powder's Storm
        let cfg = &st.config;
        let plain = builtin_http::body(&crate::builtin::sdk::configuration(APP_ID).unwrap());
        let settings = storm_settings(&cfg.server.public_host, cfg.server.storm_port);
        assert_eq!(r.body, with_storm(&plain, Some(&settings)));
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["configuration"]["punch"], settings);

        let path = format!("/v1/spaces/global/ubiconnect/games/api?spaceIds=x%2C{}", crate::constants::SPACE_ID);
        let games = handle(&st, &req("GET", &path, &[]));
        assert_eq!(games.how, "builtin");
        let v: Value = serde_json::from_slice(&games.body).unwrap();
        assert_eq!(v["games"][0]["displayName"], "Steep");
    }

    #[test]
    fn the_pack_answers_before_the_builtin_replies() {
        let path = "/v4/spaces/a20f75db-9175-4df6-b1b3-ba42e291166c/configs/events";
        let fx = HttpFixture { method: "GET".into(), host: "public-ubiservices.ubi.com".into(), path: path.into(),
                               status: 200, content_type: JSON.into(), body: br#"{"types": ["x"]}"#.to_vec() };
        let r = handle(&state(vec![fx], None), &req("GET", path, &[]));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("gamedata:exact", &br#"{"types": ["x"]}"#[..]));
        let r = handle(&state(vec![], None), &req("GET", path, &[]));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("builtin", &br#"{"types": []}"#[..]));
    }

    #[test]
    fn connect_ui_settings_keep_their_cache() {
        let st = state(vec![], None);
        let path = "/v1/spaces/e17be87d-2996-4f3b-97c4-19bb2dae2933/global/ubiconnect/configcache/api/other";
        let r = handle(&st, &req("GET", path, &[("if-none-match", "\"abc\"")]));
        assert_eq!((r.status, r.how.as_str(), r.body.len()), (304, "not-modified", 0));
        let r = handle(&st, &req("GET", path, &[("if-modified-since", "Tue, 01 Dec 2016 00:00:00 GMT")]));
        assert_eq!(r.status, 304);
        // no cached copy, nothing built in: an error, never an empty 200 the browser would cache
        let r = handle(&st, &req("GET", path, &[]));
        assert_eq!((r.status, r.how.as_str(), r.body.as_slice()), (503, "UNMATCHED", &b"{}"[..]));
        assert_eq!(header(&r, "content-type"), Some(JSON));
        // a built-in one when there is no cached copy to keep
        let postauth = "/v4/spaces/global/ubiconnect/configcache/api/postauth?applications=a";
        assert_eq!(handle(&st, &req("GET", postauth, &[])).how, "builtin");
        assert_eq!(handle(&st, &req("GET", postauth, &[("If-None-Match", "x")])).status, 304);
    }

    #[test]
    fn pack_reply_and_cors() {
        let fx = HttpFixture { method: "GET".into(), host: "public-ubiservices.ubi.com".into(),
                               path: "/v1/spaces/x".into(), status: 200, content_type: JSON.into(),
                               body: br#"{"a": 1}"#.to_vec() };
        let st = state(vec![fx], None);
        let r = handle(&st, &req("GET", "/v1/spaces/x", &[("origin", "https://connect.cdn.ubisoft.com")]));
        assert_eq!(r.how, "gamedata:exact");
        assert_eq!(r.body, br#"{"a": 1}"#);
        assert_eq!(header(&r, "access-control-allow-origin"), Some("https://connect.cdn.ubisoft.com"));
        let pre = handle(&st, &req("OPTIONS", "/v1/spaces/x", &[("origin", "https://uat-connect.cdn.ubisoft.com"),
                                                              ("access-control-request-method", "POST"),
                                                              ("access-control-request-headers", "ubi-appid")]));
        assert_eq!((pre.status, pre.how.as_str()), (200, "cors-preflight"));
        assert_eq!(header(&pre, "access-control-allow-headers"), Some("ubi-appid"));
    }

    /// Only Ubisoft Connect's origins get CORS (with credentials): a web page elsewhere, which the hosts file
    /// also sends here, can neither read replies nor send anything but GET / HEAD.
    #[test]
    fn a_servers_own_page_may_call_it() {
        let st = state(vec![], None);
        let own = [("Host", "127.0.0.1:18443"), ("Origin", "https://127.0.0.1:18443")];
        let r = handle(&st, &req("POST", "/v1/nothing", &own));
        assert_eq!((r.status, r.how.as_str(), header(&r, "access-control-allow-origin")), (200, "UNMATCHED", None));
        let other = [("Host", "127.0.0.1:18443"), ("Origin", "https://127.0.0.1:9999")];
        assert_eq!(handle(&st, &req("POST", "/v1/nothing", &other)).status, 403);
        assert_eq!(handle(&st, &req("POST", "/v1/nothing", &[("Origin", "https://127.0.0.1:18443")])).status, 403,
                   "no Host: nothing to compare with");
    }

    #[test]
    fn cors_only_for_connects_origins() {
        for ok in ["https://connect.cdn.ubisoft.com", "https://uat-connect.cdn.ubisoft.com", "https://ubisoft.com",
                   "https://store.ubi.com", "HTTPS://Connect.CDN.Ubisoft.com:443"] {
            assert!(connect_origin(ok), "{ok}");
        }
        for foreign in ["null", "http://connect.cdn.ubisoft.com", "https://evil.com", "https://ubisoft.com.evil.com",
                        "https://evilubisoft.com", "https://evil.com/.ubisoft.com", "https://a@ubisoft.com", "",
                        "file://", "https://ubisoft.com:x"] {
            assert!(!connect_origin(foreign), "{foreign}");
        }
        let st = state(vec![], None);
        let evil = [("Origin", "https://evil.example"), ("Access-Control-Request-Method", "POST"),
                    ("Access-Control-Request-Headers", "authorization")];
        let pre = handle(&st, &req("OPTIONS", "/v3/profiles/sessions", &evil));
        assert_eq!((pre.status, pre.how.as_str()), (403, "cors-refused"));
        assert!(header(&pre, "access-control-allow-origin").is_none());
        assert!(header(&pre, "access-control-allow-headers").is_none());
        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let r = handle(&st, &req(method, "/_powder/account/register", &evil[..1]));
            assert_eq!((r.status, body(&r)), (403, json!({"errorCode": 1, "message": "origin not allowed",
                                                            "httpCode": 403})), "{method}");
        }
        let r = handle(&st, &req("GET", "/v1/nothing", &evil[..1]));
        assert_eq!((r.status, r.how.as_str()), (200, "UNMATCHED"), "a GET is answered, without CORS");
        for name in ["access-control-allow-origin", "access-control-allow-credentials", "access-control-expose-headers"] {
            assert!(header(&r, name).is_none(), "{name}");
        }
        let r = handle(&st, &req("GET", "/_powder/overlay/powder.js", &evil[..1]));
        assert_eq!(header(&r, "access-control-allow-origin"), Some("*"), "the overlay part's own stays");
        assert!(header(&r, "access-control-allow-credentials").is_none());
    }

    #[test]
    fn a_mod_answers_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.lua"), r#"return function(api)
            api.before_http("GET", "/v1/spaces/*", function(req)
                return { status = 201, body = { seen = req.path, ip = req.peer }, headers = { ["X-Mod"] = "m" } }
            end)
        end"#).unwrap();
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let mut config = Config::default();
        config.mods.dir = dir.path().to_path_buf();
        let host = ModHost::load(&config, Arc::new(crate::repos::Repos::new(db)));
        let r = handle(&state(vec![], Some(host)), &req("GET", "/v1/spaces/abc?x=1", &[]));
        assert_eq!((r.status, r.how.as_str()), (201, "mod"));
        assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap(), json!({"seen": "/v1/spaces/abc", "ip": "127.0.0.1"}));
        assert_eq!(header(&r, "x-mod"), Some("m"));
        assert_eq!(header(&r, "content-type"), Some(JSON));
    }

    /// A server on a free port with the local CA's certificate; -> its address and the CA (PEM).
    async fn tls_server(state: HttpState, dir: &std::path::Path) -> (SocketAddr, Vec<u8>) {
        tls_server_limited(state, dir, Limits::default()).await
    }

    async fn tls_server_limited(state: HttpState, dir: &std::path::Path, limits: Limits) -> (SocketAddr, Vec<u8>) {
        let pki = crate::tls::ensure_pki(dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_limited(listener, pki.server_config().unwrap(), Arc::new(state), limits));
        (addr, std::fs::read(&pki.ca_pem).unwrap())
    }

    /// A TLS connection to the server, checked against the CA, with this SNI.
    async fn tls_connect(addr: SocketAddr, ca: &[u8], sni: &str)
                         -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
        try_tls_connect(addr, ca, sni).await.unwrap()
    }

    async fn try_tls_connect(addr: SocketAddr, ca: &[u8], sni: &str)
                             -> std::io::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
        use tokio_rustls::rustls::{self, pki_types::ServerName, ClientConfig, RootCertStore};
        let mut roots = RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut &ca[..]) {
            roots.add(cert.unwrap()).unwrap();
        }
        let client = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from(sni.to_string()).unwrap();
        tokio_rustls::TlsConnector::from(Arc::new(client)).connect(name, tcp).await
    }

    /// One request over a real TLS connection (SNI public-ubiservices.ubi.com); -> the raw response.
    async fn tls_request(addr: SocketAddr, ca: &[u8], request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut tls = tls_connect(addr, ca, "public-ubiservices.ubi.com").await;
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut out = vec![];
        let _ = tls.read_to_end(&mut out).await; // a peer closing without close_notify is fine here
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A real TLS connection: the local CA's server certificate, SNI, hyper, the pipeline.
    #[tokio::test]
    async fn end_to_end_over_tls() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, ca) = tls_server(state(vec![], None), dir.path()).await;
        let text = tls_request(addr, &ca, "GET /v1/nothing HTTP/1.1\r\nHost: public-ubiservices.ubi.com\r\n\
                                           Connection: close\r\n\r\n").await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.ends_with("\r\n\r\n{}"), "{text}");
    }

    /// Signing in over TLS with an account's email and password, then using the ticket we gave out.
    #[tokio::test(flavor = "multi_thread")]
    async fn sign_in_over_tls() {
        let dir = tempfile::tempdir().unwrap();
        let st = state(vec![], None);
        let account = st.accounts.register("rider@example.com", "correct horse", "Rider").unwrap();
        let (addr, ca) = tls_server(st, dir.path()).await;
        let post = |auth: String| format!("POST /v3/profiles/sessions HTTP/1.1\r\nHost: public-ubiservices.ubi.com\r\n\
                                           Authorization: {auth}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
        let text = tls_request(addr, &ca, &post(format!("Basic {}", crate::util::b64(b"rider@example.com:correct horse")))).await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        let b: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(b["profileId"], account.profile_id.as_str());
        let ticket = b["ticket"].as_str().unwrap();
        assert!(ticket.starts_with("powder."));
        // a game session (no Connect one from this address): only the ticket itself can sign in again
        let text = tls_request(addr, &ca, &post(format!("Ubi_v1 t={ticket}"))).await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        let again: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(again["profileId"], account.profile_id.as_str());
        assert_ne!(again["ticket"], b["ticket"]);
        let text = tls_request(addr, &ca, &post("Ubi_v1 t=powder.made-up".into())).await;
        assert!(text.starts_with("HTTP/1.1 401"), "{text}");
        let text = tls_request(addr, &ca, &post(format!("Basic {}", crate::util::b64(b"rider@example.com:wrong")))).await;
        assert!(text.starts_with("HTTP/1.1 401") && text.contains(r#""errorCode": 1"#), "{text}");
    }

    /// The notification websocket over TLS, at the game's path and Connect's: the 101 (first protocol echoed), the
    /// hello, a push from the server, a ping answered, and the socket forgotten once closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_over_tls() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let dir = tempfile::tempdir().unwrap();
        // a mod hears ws_connected
        std::fs::write(dir.path().join("m.lua"), r#"return function(api)
            api.on("ws_connected", function(ev) api.save("seen", { ev.profile_id, ev.connection_id }) end)
        end"#).unwrap();
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let mod_repos = Arc::new(crate::repos::Repos::new(db));
        let mut config = Config::default();
        config.mods.dir = dir.path().to_path_buf();
        let st = state(vec![], Some(ModHost::load(&config, mod_repos.clone())));
        let pid = st.accounts.default_profile_id().unwrap();
        let session = st.sessions.issue(st.accounts.player(&pid).unwrap(), "127.0.0.1", "");
        let sessions = st.sessions.clone();
        let (addr, ca) = tls_server(st, dir.path()).await;
        for path in ["/v2/websocket?NotificationTypes=US_CONNECTION_ESTABLISHED",
                     "/v2/websocket/server?SpaceIds=x&NotificationTypes=US_CONNECTION_ESTABLISHED"] {
            let tls = tls_connect(addr, &ca, "public-ws-ubiservices.ubi.com").await;
            let mut request = format!("wss://public-ws-ubiservices.ubi.com{path}").into_client_request().unwrap();
            let headers = request.headers_mut();
            headers.insert("Authorization", HeaderValue::from_str(&format!("Ubi_v1 t={}", session.ticket)).unwrap());
            headers.insert("Sec-WebSocket-Protocol", HeaderValue::from_static("ubiservices, other"));
            let (mut ws, response) = tokio_tungstenite::client_async(request, tls).await.unwrap();
            assert_eq!(response.status(), 101);
            assert_eq!(response.headers()["sec-websocket-protocol"], "ubiservices");
            assert!(response.headers().get("content-length").is_none(), "{response:?}");
            let next = |m: Option<Result<Message, _>>| m.expect("a message").expect("no error");
            let hello: Value = serde_json::from_str(next(ws.next().await).to_text().unwrap()).unwrap();
            let keys: Vec<_> = hello.as_object().unwrap().keys().collect();
            assert_eq!(keys, ["notificationType", "content", "datePosted"]);
            assert_eq!(hello["notificationType"], "US_CONNECTION_ESTABLISHED");
            let id = hello["content"]["connectionId"].as_str().unwrap();
            assert!(uuid::Uuid::parse_str(id).is_ok());
            let mut tries = 0;
            while mod_repos.kv_get("mod:m", "seen").unwrap() != Some(json!([pid, id])) {
                tries += 1;
                assert!(tries < 200, "no ws_connected event");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let contact = hello["content"]["contactUrl"].as_str().unwrap();
            let node = contact.strip_prefix("https://public-im-ubiservices.ubi.com/v2/connections/").unwrap();
            assert_eq!(node, format!("{}/8180/{id}/notifications", &node[..17]));
            assert!(node[..17].bytes().all(|b| b.is_ascii_hexdigit()));
            // registered right after the hello
            let mut tries = 0;
            while sessions.push_ws(&pid, "TEST_NOTIFICATION", json!({"x": 1}), Map::new()) == 0 {
                tries += 1;
                assert!(tries < 200, "the websocket was never registered");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let pushed: Value = serde_json::from_str(next(ws.next().await).to_text().unwrap()).unwrap();
            assert_eq!(pushed["notificationType"], "TEST_NOTIFICATION");
            assert_eq!(pushed["content"], json!({"x": 1}));
            ws.send(Message::text("hi")).await.unwrap();
            ws.send(Message::Ping(b"p".to_vec())).await.unwrap();
            assert_eq!(next(ws.next().await), Message::Pong(b"p".to_vec()));
            ws.close(None).await.unwrap();
            while ws.next().await.is_some() {}
            let mut tries = 0;
            while session.channels.websocket_count() > 0 {
                tries += 1;
                assert!(tries < 200, "the websocket was never removed");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        // a plain GET there is refused
        let text = tls_request(addr, &ca, "GET /v2/websocket HTTP/1.1\r\nHost: public-ws-ubiservices.ubi.com\r\n\
                                           Connection: close\r\n\r\n").await;
        assert!(text.starts_with("HTTP/1.1 400"), "{text}");
    }

    /// The launcher's calls over TLS: the status, and the CA it makes (`run` keeps it in <data>/tls).
    #[tokio::test(flavor = "multi_thread")]
    async fn powder_status_and_ca_over_tls() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.data_dir = dir.path().to_path_buf();
        let (addr, ca) = tls_server(state_with(config, vec![], None), &dir.path().join("tls")).await;
        let get = |path: &str| {
            format!("GET {path} HTTP/1.1\r\nHost: public-ubiservices.ubi.com\r\nConnection: close\r\n\r\n")
        };
        let text = tls_request(addr, &ca, &get("/_powder/status")).await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        let b: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!((&b["connect"], &b["you"]), (&json!([]), &json!("127.0.0.1")));
        let text = tls_request(addr, &ca, &get("/_powder/ca.pem")).await;
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert_eq!(text.split_once("\r\n\r\n").unwrap().1.as_bytes(), ca);
    }

    /// Bodies over their path's limit get a 413 (from the Content-Length, before the body is read), uploads may be
    /// bigger, chat channels less; a body that does not arrive in time gets a 408.
    #[tokio::test(flavor = "multi_thread")]
    async fn body_limits_over_tls() {
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits { body: std::time::Duration::from_millis(300), ..Limits::default() };
        let (addr, ca) = tls_server_limited(state(vec![], None), dir.path(), limits).await;
        let post = |method: &str, path: &str, length: usize, sent: &[u8]| {
            let head = format!("{method} {path} HTTP/1.1\r\nHost: public-ubiservices.ubi.com\r\nContent-Length: \
                                {length}\r\nConnection: close\r\n\r\n");
            [head.as_bytes(), sent].concat()
        };
        let ca = &ca;
        let request = |raw: Vec<u8>| async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut tls = tls_connect(addr, ca, "public-ubiservices.ubi.com").await;
            tls.write_all(&raw).await.unwrap();
            let mut out = vec![];
            let _ = tls.read_to_end(&mut out).await;
            String::from_utf8_lossy(&out).into_owned()
        };
        let too_big = limits::MAX_BODY + 1;
        let text = request(post("POST", "/v1/profiles/me/remotelog", too_big, b"{}")).await;
        assert!(text.starts_with("HTTP/1.1 413"), "{text}");
        assert!(text.to_lowercase().contains("connection: close"), "{text}");
        let body: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body, json!({"errorCode": 1, "message": format!("Request body too large (at most {} bytes).",
                                                                     limits::MAX_BODY), "httpCode": 413}));
        // an upload of that size is read and handed to its route
        let big = vec![b'x'; too_big];
        let text = request(post("PUT", "/_powder/entity-files/abc", big.len(), &big)).await;
        assert!(text.starts_with("HTTP/1.1 ") && !text.starts_with("HTTP/1.1 413"), "{text}");
        let channel = vec![b' '; limits::MAX_CHANNEL_BODY + 1];
        let text = request(post("POST", "/v1/spaces/s/channels", channel.len(), &channel)).await;
        assert!(text.starts_with("HTTP/1.1 413"), "{text}");
        let text = request(post("POST", "/v1/profiles/me/remotelog", 100, b"0123456789")).await;
        assert!(text.starts_with("HTTP/1.1 408"), "{text}");
    }

    /// The connection limits: per address (and the slot freed when one closes), a TLS handshake and a request's
    /// headers that take too long, an idle keep-alive connection.
    #[tokio::test(flavor = "multi_thread")]
    async fn connection_limits_over_tls() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits { max_per_ip: 2, tls_handshake: Duration::from_millis(300),
                              header_read: Duration::from_millis(300), ..Limits::default() };
        let (addr, ca) = tls_server_limited(state(vec![], None), dir.path(), limits).await;
        let host = "public-ubiservices.ubi.com";
        let first = tls_connect(addr, &ca, host).await;
        let _second = tls_connect(addr, &ca, host).await;
        assert!(try_tls_connect(addr, &ca, host).await.is_err(), "a third one from the same address");
        drop(first);
        let mut tries = 0;
        let mut third = loop {
            match try_tls_connect(addr, &ca, host).await {
                Ok(c) => break c,
                Err(_) => {
                    tries += 1;
                    assert!(tries < 100, "the closed connection's slot was never freed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        // headers not finished in time: closed (also how an idle keep-alive connection ends)
        let start = Instant::now();
        third.write_all(b"GET /v1/nothing HTTP/1.1\r\nHost: public-ubiservices.ubi.com\r\n").await.unwrap();
        let mut out = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(5), third.read_to_end(&mut out)).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(3) && !String::from_utf8_lossy(&out).contains("200"));
        drop(_second);
        // no TLS handshake at all: closed
        let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut buf)).await.unwrap();
        assert!(matches!(n, Ok(0) | Err(_)), "{n:?}");
    }

    /// Kept remotelog/events uploads: each cut to KEPT_BODY_MAX, only the newest KEPT_BODIES (other files stay).
    #[test]
    fn kept_uploads_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.data_dir = dir.path().to_path_buf();
        let st = state_with(config, vec![], None);
        let bodies = dir.path().join("bodies");
        std::fs::create_dir_all(&bodies).unwrap();
        std::fs::write(bodies.join("notes.txt"), b"mine").unwrap();
        let post = |path: &str, body: Vec<u8>| {
            let mut r = req("POST", path, &[]);
            r.body = body.into();
            handle(&st, &r)
        };
        assert_eq!(post("/v1/profiles/me/remotelog", vec![b'x'; KEPT_BODY_MAX + 10]).how, "remotelog");
        let only: Vec<_> = std::fs::read_dir(&bodies).unwrap().map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("remotelog-")).collect();
        assert_eq!(std::fs::metadata(&only[0]).unwrap().len(), KEPT_BODY_MAX as u64);
        for i in 0..KEPT_BODIES + 5 {
            let path = if i % 2 == 0 { "/v1/profiles/me/remotelog" } else { "/v3/profiles/me/events" };
            post(path, format!("{i}").into_bytes());
        }
        let names: Vec<String> = std::fs::read_dir(&bodies).unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(names.len(), KEPT_BODIES + 1, "the newest {KEPT_BODIES} and notes.txt");
        assert!(names.contains(&"notes.txt".to_string()));
        let contents: std::collections::BTreeSet<String> = names.iter().filter(|n| *n != "notes.txt")
            .map(|n| std::fs::read_to_string(bodies.join(n)).unwrap()).collect();
        assert!(contents.contains(&format!("{}", KEPT_BODIES + 4)) && !contents.contains("0"), "oldest deleted");
    }

    #[test]
    fn route_patterns() {
        fn nothing(_: &Ctx) -> Reply {
            Reply::new(200, JSON, vec![], "x")
        }
        let r = Route::new("GET", "/v1/spaces/{space}/sandboxes/{sandbox}/x.json", nothing);
        let p = r.matches("GET", "/v1/spaces/a-b/sandboxes/S_1/x.json").unwrap();
        assert_eq!((p["space"].as_str(), p["sandbox"].as_str()), ("a-b", "S_1"));
        assert!(r.matches("HEAD", "/v1/spaces/a/sandboxes/b/x.json").is_some(), "GET routes answer HEAD");
        assert!(r.matches("POST", "/v1/spaces/a/sandboxes/b/x.json").is_none());
        assert!(r.matches("GET", "/v1/spaces/a/b/sandboxes/c/x.json").is_none(), "one segment per {{name}}");
        assert!(r.matches("GET", "/v1/spaces/a/sandboxes/b/xxjson").is_none(), "literal parts are escaped");
        let tail = Route::new("*", "/{prefix:.*}test_internet.txt", nothing);
        assert_eq!(tail.matches("PUT", "/a/b/test_internet.txt").unwrap()["prefix"], "a/b/");
    }

    /// The session middleware: a `Ubi_v1` ticket of ours reaches the routes as its session; others don't.
    #[test]
    fn routes_get_the_session() {
        fn whoami(c: &Ctx) -> Reply {
            Reply::json(&json!({"who": c.session.as_ref().map(|s| s.profile_id().to_string())}), "whoami", 200)
        }
        let mut config = Config::default();
        config.server.strict_tickets = true;
        let st = state_with(config, vec![], None);
        let routes = [Route::new("GET", "/whoami", whoami)];
        let pid = st.accounts.default_profile_id().unwrap();
        let s = st.sessions.issue(st.accounts.player(&pid).unwrap(), "10.0.0.2", "");
        let ask = |auth: &str| body(&routed(&st, &req("GET", "/whoami", &[("Authorization", auth)]), &routes));
        assert_eq!(ask(&format!("Ubi_v1 t={}", s.ticket))["who"], pid.as_str());
        assert_eq!(ask(&format!("ubi_v1 t={}", s.ticket))["who"], pid.as_str(), "the scheme in any case");
        assert!(ask(&format!("Basic {}", s.ticket))["who"].is_null());
        assert!(ask("Ubi_v1 t=stale")["who"].is_null(), "strict tickets");
        // lenient (the default), no accounts: an unknown ticket is the address's player
        let st = state(vec![], None);
        let r = routed(&st, &req("GET", "/whoami", &[("Authorization", "Ubi_v1 t=stale")]), &routes);
        assert_eq!(body(&r)["who"], st.accounts.default_profile_id().unwrap().as_str());
        assert_eq!(routed(&st, &req("GET", "/other", &[]), &routes).how, "UNMATCHED");
    }

    #[test]
    fn storm_settings_go_into_the_sdk_configuration() {
        let body = br#"{"configuration": {"other": 1, "punch": {"old": true}}}"#;
        let on = with_storm(body, Some(&storm_settings("10.0.0.2", 11000)));
        let v: Value = serde_json::from_slice(&on).unwrap();
        assert_eq!(v["configuration"]["storm"]["detectUrl2"], "10.0.0.2:11002");
        assert_eq!(v["configuration"]["punch"]["traversalUrl"], "10.0.0.2:11005");
        let off: Value = serde_json::from_slice(&with_storm(body, None)).unwrap();
        assert_eq!(off["configuration"], json!({"other": 1, "storm": {}}));
        assert_eq!(with_storm(b"not json", None), b"not json");
        assert!(GAME_CONFIG.is_match(&format!("/v1/applications/{}/configuration", APP_ID.to_uppercase())));
    }
}
