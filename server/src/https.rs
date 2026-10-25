//! HTTPS for every UbiServices/Olympus hostname (the hosts file points them all at us), on one TLS listener
//! routed by the name the client asked for (SNI): Ubisoft Connect's demux (dmx.upc.ubisoft.com) is not HTTP,
//! everything else is.
//!
//! Request pipeline (outermost first), as in the Python server:
//!   access log      one line per request; uploaded remotelog/events bodies are kept (they explain client errors)
//!   CORS            Ubisoft Connect's web UI calls UbiServices from its embedded browser: preflights, Allow-Origin
//!   mods            `api.before_http` hooks may answer a request themselves
//!   fallback        the game-data pack's recorded reply, or `{}` + an UNMATCHED warning
//!
//! The UbiServices routes themselves (sessions, profiles, UGC, /_powder/, the websocket) are not ported yet;
//! until they are, everything they would answer falls through to the pack.

use crate::assets::Assets;
use crate::config::Config;
use crate::constants::{APP_ID, DEMUX_HOST};
use crate::mods::ModHost;
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use tokio::net::TcpListener;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

pub const JSON: &str = "application/json; charset=utf-8";
const MAX_BODY: usize = 64 * 1024 * 1024; // UGC files (photos, trails)
const SAVED_UPLOADS: [&str; 2] = ["/remotelog", "/profiles/me/events"];
const CORS_METHODS: &str = "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS";
const CORS_EXPOSE: &str = "X-Total-Count, Ubi-TransactionId";

static GAME_CONFIG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)^/v\d+/applications/{APP_ID}/configuration$")).unwrap());

/// What every request needs; shared by all connections.
pub struct HttpState {
    pub config: Arc<Config>,
    pub assets: Arc<Assets>,
    pub mods: Option<Arc<Mutex<ModHost>>>,
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

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
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
    fn new(status: u16, content_type: &str, body: Vec<u8>, how: &str) -> Reply {
        Reply { status, headers: vec![("Content-Type".into(), content_type.into())], body, how: how.into() }
    }
}

/// The whole pipeline for one request.
pub fn handle(state: &HttpState, req: &Req) -> Reply {
    if req.method == "POST" && SAVED_UPLOADS.iter().any(|s| req.path().ends_with(s)) {
        keep_upload(state, req);
    }
    let origin = req.header("Origin").map(str::to_string);
    let mut reply = match &origin {
        Some(_) if req.method == "OPTIONS" && req.headers.contains_key("Access-Control-Request-Method") => {
            let mut r = Reply { status: 200, headers: vec![], body: vec![], how: "cors-preflight".into() };
            r.headers.push(("Access-Control-Allow-Methods".into(), CORS_METHODS.into()));
            let asked = req.header("Access-Control-Request-Headers").unwrap_or("");
            r.headers.push(("Access-Control-Allow-Headers".into(), asked.into()));
            r
        }
        _ => from_mods(state, req).unwrap_or_else(|| fallback(state, req)),
    };
    if let Some(origin) = origin {
        reply.headers.push(("Access-Control-Allow-Origin".into(), origin));
        reply.headers.push(("Access-Control-Allow-Credentials".into(), "true".into()));
        reply.headers.push(("Access-Control-Expose-Headers".into(), CORS_EXPOSE.into()));
    }
    let qs: String = req.path_qs.chars().take(160).collect();
    let line = format!("{} {}{qs} -> {} {} ({}B)", req.method, req.host, reply.how, reply.status, reply.body.len());
    if reply.how == "UNMATCHED" {
        log::warn!(target: "powder.https", "{line}");
    } else if req.path().starts_with("/_powder/") {
        log::debug!(target: "powder.https", "{line}"); // the launcher polls these every few seconds
    } else {
        log::info!(target: "powder.https", "{line}");
    }
    reply
}

fn keep_upload(state: &HttpState, req: &Req) {
    let kind = if req.path().ends_with("/remotelog") { "remotelog" } else { "events" };
    let dir: PathBuf = state.config.data_dir().join("bodies");
    let name = chrono::Local::now().format(&format!("{kind}-%Y%m%d-%H%M%S-%6f.txt")).to_string();
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(name), &req.body)) {
        log::warn!(target: "powder.https", "keeping a {kind} upload: {e}");
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
/// id other than the game's) try Connect's part of the pack first.
fn fallback(state: &HttpState, req: &Req) -> Reply {
    let assets = &state.assets;
    let app_id = req.header("Ubi-AppId").unwrap_or("").to_lowercase();
    let order = if !app_id.is_empty() && app_id != APP_ID {
        [&assets.connect_http, &assets.http]
    } else {
        [&assets.http, &assets.connect_http]
    };
    let hit = order.iter().find_map(|f| f.lookup(&req.method, &req.host, &req.path_qs));
    match hit {
        Some((fixture, tier)) => {
            let body = storm_config(state, req.path(), &fixture.body);
            Reply::new(fixture.status, &fixture.content_type, body, &format!("gamedata:{tier}"))
        }
        None => Reply::new(200, JSON, b"{}".to_vec(), "UNMATCHED"),
    }
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

async fn serve_request(state: Arc<HttpState>, sni: String, peer: SocketAddr, req: Request<Incoming>)
                       -> Result<Response<Full<Bytes>>> {
    let (parts, body) = req.into_parts();
    let body = Limited::new(body, MAX_BODY).collect().await.map_err(|e| anyhow::anyhow!("{e}"))?.to_bytes();
    let host = parts.headers.get("host").and_then(|h| h.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or("").to_lowercase()).filter(|h| !h.is_empty()).unwrap_or(sni);
    let path_qs = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let req = Req { method: parts.method.to_string(), host, path_qs, headers: parts.headers, body, peer };
    Ok(to_response(handle(&state, &req)))
}

/// Accept TLS connections on `addr` forever. HTTP ones go through [`handle`]; the demux is not ported yet, so
/// its connections are closed (logged).
pub async fn serve(addr: SocketAddr, tls: Arc<ServerConfig>, state: Arc<HttpState>) -> Result<()> {
    let listener = TcpListener::bind(addr).await.with_context(|| format!("HTTPS on {addr}"))?;
    log::info!(target: "powder.https", "listening on {addr}");
    let acceptor = TlsAcceptor::from(tls);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                log::warn!(target: "powder.tls", "accept: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    log::debug!(target: "powder.tls", "handshake with {peer} failed: {e}");
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
                log::warn!(target: "powder.tls", "demux connection from {peer} closed: not ported to Rust yet");
                return;
            }
            let service = service_fn(move |req| serve_request(state.clone(), sni.clone(), peer, req));
            if let Err(e) = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                log::debug!(target: "powder.https", "connection from {peer}: {e}");
            }
        });
    }
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
    loop {
        let Ok((tcp, _)) = listener.accept().await else { continue };
        let shared = shared.clone();
        tokio::spawn(async move {
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
            let _ = http1::Builder::new().serve_connection(TokioIo::new(tcp), service).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{HttpFixture, HttpFixtures};

    fn state(fixtures: Vec<HttpFixture>, mods: Option<ModHost>) -> HttpState {
        let assets = Assets { http: HttpFixtures::new(fixtures), ..Default::default() };
        HttpState { config: Arc::new(Config::default()), assets: Arc::new(assets),
                    mods: mods.map(|m| Arc::new(Mutex::new(m))) }
    }

    fn req(method: &str, path_qs: &str, headers: &[(&str, &str)]) -> Req {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        Req { method: method.into(), host: "public-ubiservices.ubi.com".into(), path_qs: path_qs.into(),
              headers: map, body: Bytes::new(), peer: "127.0.0.1:5000".parse().unwrap() }
    }

    fn header<'a>(r: &'a Reply, name: &str) -> Option<&'a str> {
        r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    #[test]
    fn unmatched_is_empty_json() {
        let r = handle(&state(vec![], None), &req("GET", "/v1/nothing", &[]));
        assert_eq!((r.status, r.body.as_slice(), r.how.as_str()), (200, &b"{}"[..], "UNMATCHED"));
        assert_eq!(header(&r, "content-type"), Some(JSON));
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
        let pre = handle(&st, &req("OPTIONS", "/v1/spaces/x", &[("origin", "https://o"),
                                                              ("access-control-request-method", "POST"),
                                                              ("access-control-request-headers", "ubi-appid")]));
        assert_eq!((pre.status, pre.how.as_str()), (200, "cors-preflight"));
        assert_eq!(header(&pre, "access-control-allow-headers"), Some("ubi-appid"));
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
