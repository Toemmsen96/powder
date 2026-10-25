//! UbiServices notification websocket (public-ws-ubiservices.ubi.com/v2/websocket, the game's, and
//! /v2/websocket/server?SpaceIds=...&NotificationTypes=..., Ubisoft Connect's), from websocket.py.
//!
//! The client's JobInitiateConnection waits for US_CONNECTION_ESTABLISHED with a connectionId; without it NEX
//! Register fails ("Failed to get the connection Id ..."). Envelope format from FUN_7ff661c884e0. Open sockets are
//! registered on the player's session, so services can push notifications later (SessionRegistry::push_ws).
//!
//! The handshake is a route like any other (so mods, the session and the route order apply as in Python): it
//! checks the request as aiohttp's WebSocketResponse.can_prepare does and answers 101, or 400 "ws-rejected".
//! The HTTPS server then takes the connection over ([`run`], after hyper sent the 101). Pings are answered
//! (tungstenite queues the pong); permessage-deflate is not offered back (aiohttp would), which the spec allows.

use super::{access_log, Ctx, HttpState, Reply, Req, Route};
use crate::core::identity::parse_authorization;
use crate::util::{dumps, iso_now, token_hex};
use futures_util::{SinkExt, StreamExt};
use hyper::header::HeaderMap;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// The access log's "how" of an accepted upgrade (the server takes the connection over when it sees it).
pub const HOW: &str = "websocket";

pub fn routes() -> Vec<Route> {
    vec![
        Route::new("GET", "/v2/websocket", handshake),
        Route::new("GET", "/v2/websocket/server", handshake), // Connect's ("websocket/server" in the SDK config)
    ]
}

/// Whether hyper should keep the request's upgrade for us (the route decides whether it is one).
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(hyper::header::UPGRADE)
}

fn handshake(c: &Ctx) -> Reply {
    match accept(c.req) {
        Some(headers) => Reply { status: 101, headers, body: vec![], how: HOW.into() },
        None => {
            let headers: Vec<_> = c.req.headers.iter().map(|(k, v)| (k.as_str(), v.to_str().unwrap_or("?"))).collect();
            log::warn!(target: "powder.https", "websocket handshake not acceptable; headers: {headers:?}");
            Reply { status: 400, headers: vec![], body: vec![], how: "ws-rejected".into() }
        }
    }
}

/// aiohttp's handshake checks; -> the 101's headers. The first protocol the client offers is echoed back.
fn accept(req: &Req) -> Option<Vec<(String, String)>> {
    if req.header("Upgrade").unwrap_or("").trim().to_lowercase() != "websocket" {
        return None;
    }
    let connection = req.headers.get_all(hyper::header::CONNECTION).iter().filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',')).any(|t| t.trim().eq_ignore_ascii_case("upgrade"));
    if !connection || !["13", "8", "7"].contains(&req.header("Sec-WebSocket-Version").unwrap_or("")) {
        return None;
    }
    // Python's b64decode skips what is not base64 before decoding
    let key = req.header("Sec-WebSocket-Key").unwrap_or("");
    let clean: String = key.chars().filter(|c| c.is_ascii_alphanumeric() || "+/=".contains(*c)).collect();
    use base64::Engine;
    if key.is_empty() || base64::engine::general_purpose::STANDARD.decode(clean).map(|k| k.len()) != Ok(16) {
        return None;
    }
    let mut headers = vec![("Upgrade".into(), "websocket".into()), ("Connection".into(), "upgrade".into()),
                           ("Sec-WebSocket-Accept".into(), derive_accept_key(key.as_bytes()))];
    let offered = req.header("Sec-WebSocket-Protocol").unwrap_or("");
    let first = offered.split(',').next().unwrap_or("").trim();
    if !first.is_empty() {
        headers.push(("Sec-WebSocket-Protocol".into(), first.into()));
    }
    Some(headers)
}

/// The connection after its 101: hello, then the session's pushes out and the client's messages logged, until
/// it closes. The access-log line is written then (as aiohttp's, once the handler returns).
pub async fn run(state: Arc<HttpState>, req: Req, upgrade: OnUpgrade) {
    match upgrade.await {
        Ok(upgraded) => {
            let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
            if let Err(e) = talk(&state, &req, ws).await {
                log::warn!(target: "powder.https", "websocket from {}: {e:#}", req.peer);
            }
        }
        Err(e) => log::warn!(target: "powder.https", "websocket upgrade from {} failed: {e}", req.peer),
    }
    access_log(&req, HOW, 101, "-");
}

async fn talk<S>(state: &Arc<HttpState>, req: &Req, mut ws: WebSocketStream<S>) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (_, ticket) = parse_authorization(req.header("Authorization"));
    let (st, ip) = (state.clone(), req.client_ip());
    // session_for_ticket may write (lenient mode issues a session): off the async workers
    let session = tokio::task::spawn_blocking(move || st.accounts.session_for_ticket(&ticket, &ip)).await??;
    let connection_id = uuid::Uuid::new_v4().to_string();
    // contactUrl is where the client PUTs which notification types it wants (its JobUpdateConnection), as
    // Ubisoft's: https://public-im-.../v2/connections/<node>/<port>/<connection id>/notifications (answered by
    // routes_profile). A wss:// address there fails in the game ("Unsupported protocol").
    let node: String = token_hex(9).chars().take(17).collect();
    let contact =
        format!("https://public-im-ubiservices.ubi.com/v2/connections/{node}/8180/{connection_id}/notifications");
    let hello = json!({"notificationType": "US_CONNECTION_ESTABLISHED",
                       "content": {"connectionId": connection_id, "contactUrl": contact}, "datePosted": iso_now()});
    ws.send(Message::text(dumps(&hello))).await?;

    let (tx, mut pushed) = tokio::sync::mpsc::unbounded_channel();
    let registered = session.as_ref().map(|s| s.channels.add_websocket(tx));
    if let (Some(s), Some(mods)) = (&session, &state.mods) {
        let (mods, event) = (mods.clone(), json!({"profile_id": s.profile_id(), "name": s.player.name,
                                                  "session_id": s.session_id, "connection_id": connection_id,
                                                  "peer": req.client_ip()}));
        tokio::task::spawn_blocking(move || mods.lock().emit("ws_connected", &event)).await?;
    }
    let who = session.as_ref().map(|s| s.player.name.as_str()).unwrap_or("unknown");
    log::info!(target: "powder.https", "websocket open ({connection_id}) for {who}");
    let result = async {
        let mut pushing = registered.is_some();
        loop {
            tokio::select! {
                msg = pushed.recv(), if pushing => match msg {
                    Some(msg) => ws.send(Message::text(dumps(&msg))).await?,
                    None => pushing = false,
                },
                incoming = ws.next() => match incoming {
                    Some(Ok(Message::Text(t))) => {
                        let head: String = t.chars().take(200).collect();
                        log::info!(target: "powder.https", "websocket <- {head:?}");
                    }
                    Some(Ok(Message::Binary(b))) => {
                        log::info!(target: "powder.https", "websocket <- {:?}", &b[..b.len().min(200)]);
                    }
                    Some(Ok(_)) => {} // ping (tungstenite answers it), pong, close (answered; the stream ends next)
                    Some(Err(e)) => return Err(anyhow::Error::from(e)),
                    None => return Ok(()),
                },
            }
        }
    }.await;
    if let (Some(s), Some(id)) = (&session, registered) {
        s.channels.remove_websocket(id);
    }
    log::info!(target: "powder.https", "websocket closed ({connection_id})");
    result
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::handle;
    use super::*;

    #[test]
    fn handshake_like_aiohttp() {
        let st = state(vec![], None);
        let ok = [("Upgrade", "websocket"), ("Connection", "keep-alive, Upgrade"), ("Sec-WebSocket-Version", "13"),
                  ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="), ("Sec-WebSocket-Protocol", "a, b"),
                  ("Origin", "https://connect.cdn.ubisoft.com")];
        for path in ["/v2/websocket?NotificationTypes=X", "/v2/websocket/server?SpaceIds=x&NotificationTypes=X"] {
            let r = handle(&st, &req("GET", path, &ok));
            assert_eq!((r.status, r.how.as_str(), r.body.len()), (101, HOW, 0));
            // the RFC 6455 example's accept value; no CORS headers on the upgrade
            let expected = [("Upgrade", "websocket"), ("Connection", "upgrade"),
                            ("Sec-WebSocket-Accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), ("Sec-WebSocket-Protocol", "a")];
            assert_eq!(r.headers, expected.map(|(k, v)| (k.to_string(), v.to_string())));
        }
        let without_protocol: Vec<_> = ok.iter().copied().filter(|(k, _)| *k != "Sec-WebSocket-Protocol").collect();
        let r = handle(&st, &req("GET", "/v2/websocket", &without_protocol));
        assert!(header(&r, "sec-websocket-protocol").is_none());
        // what can_prepare refuses: a plain GET, a wrong version, a bad key, no Connection: upgrade
        let with = |name: &str, value: &str| {
            let mut h: Vec<_> = ok.iter().copied().filter(|(k, _)| *k != name).collect();
            h.push((name, value));
            handle(&st, &req("GET", "/v2/websocket", &h))
        };
        let plain = handle(&st, &req("GET", "/v2/websocket", &[("Origin", "https://connect.cdn.ubisoft.com")]));
        for r in [plain, with("Sec-WebSocket-Version", "12"), with("Sec-WebSocket-Key", "c2hvcnQ="),
                  with("Connection", "keep-alive"), with("Upgrade", "h2c")] {
            let got = (r.status, r.how.as_str(), r.body.len(), r.headers.len());
            assert_eq!(got, (400, "ws-rejected", 0, 3), "no Content-Type, only CORS headers");
        }
    }
}
