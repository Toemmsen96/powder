//! One Ubisoft Connect demux connection (TLS to dmx.upc.ubisoft.com:443).
//!
//! Every message is a 4-byte big-endian length + protobuf. Client -> server ("upstream"):
//!   1 request  {1 request_id, 2 authenticate {1 {3 ticket}, 2 "uplay_pc"},
//!               3 open_connection {2 service name}, 5 patch_info, 6 service_request {1 name, 2 data}}
//!   2 push     {1 data {1 connection_id, 2 framed service message}, 2 connection_closed {1 id},
//!               3 keep_alive, 4 client_version {1 version}, 6 product_started {1 product_id},
//!               7 product_ended {1 product_id}}
//! Server -> client ("downstream"):
//!   1 response {1 request_id, 2 authenticate {1 success}, 3 open_connection {1 connection_id, 2 success},
//!               5 patch_info {...}, 6 service_request {1 success, 2 data}}
//!   2 push     {1 data {1 connection_id, 2 framed service message}}
//! Messages inside a service connection: services.rs. Field names follow what the messages carry; the numbers are
//! what matters.

use super::pb::{self, F, V};
use super::services::{answer, ServiceContext};
use super::{ClientGuard, DemuxDeps};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest upstream message accepted; a longer one closes the connection. Connect's own are small (the
/// largest, the sign-in with its ticket, is a few KB; cloud saves go over HTTPS links), so 1 MiB is generous.
/// (The big replies, owned games and product configurations, go the other way and are not limited.)
pub const MAX_MESSAGE: usize = 1 << 20;
/// A message's buffer grows as its bytes arrive, from this much: a length header alone allocates little.
const READ_CHUNK: usize = 64 << 10;
/// Never fetched: there is no newer version.
pub const PATCH_BASE_URL: &str = "https://static3.cdn.ubi.com/orbit/releases/0/patches/";

/// Serve one demux connection (already through TLS) until the client closes it.
pub async fn run<S: AsyncRead + AsyncWrite + Unpin + Send>(mut stream: S, peer: SocketAddr, deps: Arc<DemuxDeps>)
                                                             -> anyhow::Result<()> {
    let mut conn = Connection::new(deps, peer);
    log::info!(target: "powder.demux", "demux connection from {}", conn.peer);
    let result = conn.serve(&mut stream).await;
    let open: Vec<&str> = conn.channels.values().map(String::as_str).collect();
    log::info!(target: "powder.demux", "demux {} closed ({})", conn.peer,
               if open.is_empty() { "no services".to_string() } else { open.join(", ") });
    let _ = stream.shutdown().await;
    match result {
        Err(e) if matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset
                                     | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe) => Ok(()),
        other => Ok(other?),
    }
}

/// The next upstream message (unframed); None: its length is over [`MAX_MESSAGE`] (logged; the connection is
/// closed). Read as it arrives, so a length header with nothing behind it costs no more than what was sent.
async fn read_message<S: AsyncRead + Unpin>(stream: &mut S, peer: &str) -> std::io::Result<Option<Vec<u8>>> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let n = u32::from_be_bytes(head) as usize;
    if n > MAX_MESSAGE {
        log::warn!(target: "powder.demux", "{peer}: message of {n} bytes (at most {MAX_MESSAGE}), closing");
        return Ok(None);
    }
    let mut msg = Vec::with_capacity(n.min(READ_CHUNK));
    (&mut *stream).take(n as u64).read_to_end(&mut msg).await?;
    if msg.len() < n {
        return Err(ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(msg))
}

/// The state of one connection; [`Connection::handle`] turns one upstream message into the downstream ones.
pub struct Connection {
    deps: Arc<DemuxDeps>,
    pub peer: String,
    pub client_ip: String,
    pub profile_id: Option<String>,
    pub player_name: Option<String>,
    /// connection id -> service name
    pub channels: BTreeMap<u64, String>,
    next_channel: u64,
    pub client_version: u64,
    entry: ClientGuard,
}

impl Connection {
    pub fn new(deps: Arc<DemuxDeps>, peer: SocketAddr) -> Self {
        let client_ip = peer.ip().to_string();
        let peer = format!("{client_ip}:{}", peer.port());
        let entry = deps.clients.add(&peer);
        Self { deps, peer, client_ip, profile_id: None, player_name: None, channels: BTreeMap::new(), next_channel: 1,
               client_version: 0, entry }
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(&mut self, stream: &mut S) -> std::io::Result<()> {
        let mut kicks = self.deps.accounts.sessions.subscribe_kicks();
        let mut listening = true;
        loop {
            let msg = {
                // a read is not cancelled midway (that would lose bytes of the stream): kick notices are
                // waited for beside it
                let read = read_message(stream, &self.peer);
                tokio::pin!(read);
                loop {
                    tokio::select! {
                        msg = &mut read => break msg?,
                        kick = kicks.recv(), if listening => match kick {
                            Ok(pid) if self.profile_id.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&pid)) => {
                                log::info!(target: "powder.demux", "{}: {} was kicked, closing", self.peer,
                                           self.player_name.as_deref().unwrap_or(&pid));
                                return Ok(());
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                log::warn!(target: "powder.demux", "{}: {n} kick notice(s) missed", self.peer);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => listening = false,
                        },
                    }
                }
            };
            let Some(msg) = msg else { return Ok(()) };
            let out = self.handle(&msg);
            if !out.is_empty() {
                for m in out {
                    stream.write_all(&pb::frame(&m)).await?;
                }
                stream.flush().await?;
            }
        }
    }

    // ---- dispatch ----------------------------------------------------------------------------------------------
    /// One upstream message -> the downstream messages (unframed) to send for it.
    pub fn handle(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        let Some(fields) = pb::try_decode(msg) else {
            log::warn!(target: "powder.demux", "{}: undecodable message {}", self.peer,
                       hex::encode(&msg[..msg.len().min(32)]));
            return vec![];
        };
        let sub = |num| pb::bytes(&fields, num).map(|b| (b, pb::try_decode(b)));
        if let Some((raw, req)) = sub(1) {
            match req {
                Some(req) => self.request(&req).into_iter().collect(),
                None => self.bad(raw),
            }
        } else if let Some((raw, push)) = sub(2) {
            match push {
                Some(push) => self.push(&push),
                None => self.bad(raw),
            }
        } else {
            vec![]
        }
    }

    fn bad(&self, raw: &[u8]) -> Vec<Vec<u8>> {
        log::warn!(target: "powder.demux", "{}: undecodable message {}", self.peer, hex::encode(&raw[..raw.len().min(32)]));
        vec![]
    }

    fn request(&mut self, req: &[F]) -> Option<Vec<u8>> {
        let rid = pb::int(req, 1).unwrap_or(0);
        let kind = req.iter().find(|f| f.0 != 1)?;
        let body = match &kind.1 {
            V::Bytes(b) => pb::try_decode(b).unwrap_or_default(),
            _ => vec![],
        };
        let reply = match kind.0 {
            2 => self.authenticate(&body),
            3 => self.open_connection(&pb::text(&body, 2)),
            5 => self.patch_info(), // "your client is up to date"
            6 => self.service_request(&pb::text(&body, 1), pb::bytes(&body, 2).unwrap_or_default()),
            num => {
                log::warn!(target: "powder.demux", "DEMUX UNHANDLED request kind {num} from {}", self.peer);
                return None;
            }
        };
        Some(pb::encode(&[F::msg(1, vec![F::int(1, rid), F::bytes(kind.0, &reply)])]))
    }

    /// The Ubisoft Connect version of this client. Connect asks for patch info on short connections of its own,
    /// without reporting its version there; it reports it on its main connection, so use what this PC (IP) reported
    /// last, else what any Connect reported last (kept across restarts).
    pub fn known_client_version(&self) -> u64 {
        if self.client_version != 0 {
            return self.client_version;
        }
        if let Some(v) = self.deps.clients.version_from(&self.client_ip).filter(|v| *v != 0) {
            return v;
        }
        match self.deps.repos().kv_get("server", "connect_version") {
            Ok(Some(Value::Number(n))) => n.as_u64().unwrap_or(0),
            Ok(Some(Value::String(s))) => s.parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// The latest Ubisoft Connect version is always the one this client runs: a version from the recording
    /// (field 5) makes a newer Connect try to patch itself to it, fetch a file list the server doesn't have, and
    /// give up (on some PCs closing when a game starts).
    pub fn patch_info(&self) -> Vec<u8> {
        let version = self.known_client_version();
        log::info!(target: "powder.demux", "demux {}: patch info: latest Ubisoft Connect is {}", self.peer,
                   if version == 0 { "as recorded".to_string() } else { version.to_string() });
        let Some(recorded) = self.deps.assets.products.patch_info.as_ref().filter(|r| !r.is_empty()) else {
            // {1 ok, 2 release name, 3 ?, 4 patch base URL, 5 latest version, 6, 7}: without 2 and 4 Connect keeps
            // its start-up connection open and waits 30 s for a timeout
            return pb::encode(&[F::int(1, 1), F::str(2, "0"), F::int(3, 0), F::str(4, PATCH_BASE_URL),
                                F::int(5, version), F::int(6, 0), F::int(7, 0)]);
        };
        if version == 0 {
            return recorded.clone();
        }
        match pb::decode(recorded) {
            Ok(fields) => pb::encode(&pb::replace(&fields, 5, V::Int(version))),
            Err(e) => {
                log::warn!(target: "powder.demux", "recorded patch info is not protobuf ({e}): sent as recorded");
                recorded.clone()
            }
        }
    }

    fn authenticate(&mut self, body: &[F]) -> Vec<u8> {
        let ticket = pb::sub(body, 1).map(|t| pb::text(&t, 3)).unwrap_or_default();
        let session = self.deps.accounts.session_for_ticket(&ticket, &self.client_ip).unwrap_or_else(|e| {
            log::warn!(target: "powder.demux", "demux login from {}: {e:#}", self.peer);
            None
        });
        self.profile_id = session.as_ref().map(|s| s.profile_id().to_string());
        self.player_name = session.as_ref().map(|s| s.player.name.clone());
        let (pid, name) = (self.profile_id.clone(), self.player_name.clone());
        self.entry.update(|c| {
            c.profile_id = pid;
            c.player_name = name;
        });
        let client = pb::text(body, 2);
        log::info!(target: "powder.demux", "demux login {} from {} ({})",
                   self.player_name.as_deref().unwrap_or("REFUSED"), self.peer,
                   if client.is_empty() { "?" } else { &client });
        pb::encode(&[F::int(1, session.is_some() as u64)])
    }

    fn sync_services(&self) {
        let mut services: Vec<String> = self.channels.values().cloned().collect();
        services.sort();
        services.dedup();
        self.entry.update(|c| c.services = services);
    }

    fn open_connection(&mut self, name: &str) -> Vec<u8> {
        let cid = self.next_channel;
        self.next_channel += 1;
        self.channels.insert(cid, name.to_string());
        self.sync_services();
        log::info!(target: "powder.demux", "demux {}: open {name} -> connection {cid}", self.peer);
        pb::encode(&[F::int(1, cid), F::int(2, 1)])
    }

    /// One-off requests outside a service connection: {1 result, 2 data}.
    fn service_request(&self, name: &str, data: &[u8]) -> Vec<u8> {
        let out = match name {
            "utility_service" => {
                // where the player is
                let cfg = &self.deps.config.server;
                pb::encode(&[F::msg(1, vec![F::msg(1, vec![F::str(1, &cfg.country), F::str(2, &cfg.region)])])])
            }
            "client_configuration_service" => data.to_vec(), // the client's version, echoed back
            _ => {
                log::warn!(target: "powder.demux", "DEMUX UNHANDLED service request {name} ({}B)", data.len());
                return pb::encode(&[F::int(1, 0)]);
            }
        };
        log::info!(target: "powder.demux", "demux {}: service request {name}", self.peer);
        pb::encode(&[F::int(1, 1), F::bytes(2, &out)])
    }

    fn push(&mut self, push: &[F]) -> Vec<Vec<u8>> {
        let mut out = vec![];
        for F(num, value) in push {
            let fields = match value {
                V::Bytes(b) => pb::try_decode(b).unwrap_or_default(),
                _ => vec![],
            };
            match num {
                1 => {
                    let cid = pb::int(&fields, 1);
                    let data = pb::bytes(&fields, 2).unwrap_or_default();
                    let Some(service) = cid.and_then(|c| self.channels.get(&c)) else {
                        log::warn!(target: "powder.demux", "demux {}: data for unknown connection {cid:?}", self.peer);
                        continue;
                    };
                    let Ok(msg) = pb::unframe(data) else {
                        log::warn!(target: "powder.demux", "demux {}: bad {service} frame", self.peer);
                        continue;
                    };
                    let ctx = ServiceContext { deps: &self.deps, profile_id: self.profile_id.as_deref() };
                    if let Some(reply) = answer(&ctx, service, msg) {
                        let cid = cid.unwrap_or(0);
                        out.push(pb::encode(&[F::msg(2, vec![F::msg(1, vec![F::int(1, cid),
                                                                             F::bytes(2, &pb::frame(&reply))])])]));
                    }
                }
                2 => {
                    let cid = pb::int(&fields, 1);
                    let closed = cid.and_then(|c| self.channels.remove(&c));
                    self.sync_services();
                    log::info!(target: "powder.demux", "demux {}: {} closed by client", self.peer,
                               closed.unwrap_or_else(|| format!("{cid:?}")));
                }
                4 => {
                    self.client_version = pb::int(&fields, 1).unwrap_or(0);
                    log::info!(target: "powder.demux", "demux {}: client version {}", self.peer, self.client_version);
                    if self.client_version != 0 {
                        // for its patch checks on separate connections
                        self.deps.clients.set_version(&self.client_ip, self.client_version);
                        if let Err(e) = self.deps.repos().kv_set("server", "connect_version", &json!(self.client_version)) {
                            log::warn!(target: "powder.demux", "storing the Connect version: {e:#}");
                        }
                    }
                }
                6 | 7 => {
                    let product = pb::int(&fields, 1);
                    let what = if *num == 6 { "started" } else { "ended" };
                    log::info!(target: "powder.demux", "demux {}: product {} {what}", self.peer,
                               product.map_or("None".to_string(), |p| p.to_string()));
                    if let Some(mods) = &self.deps.mods {
                        mods.lock().emit(&format!("connect_product_{what}"),
                                         &json!({"profile_id": self.profile_id, "product_id": product}));
                    }
                }
                3 => {} // keep-alive
                _ => log::warn!(target: "powder.demux", "DEMUX UNHANDLED push kind {num} from {}", self.peer),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_are_read_within_the_limit() {
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        client.write_all(&pb::frame(b"\x08\x01")).await.unwrap();
        assert_eq!(read_message(&mut server, "p").await.unwrap().unwrap(), b"\x08\x01");
        // over the limit: refused from the header alone
        client.write_all(&((MAX_MESSAGE + 1) as u32).to_be_bytes()).await.unwrap();
        assert!(read_message(&mut server, "p").await.unwrap().is_none());
        // a header announcing much more than follows: an EOF, after buffering only what arrived
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        client.write_all(&(MAX_MESSAGE as u32).to_be_bytes()).await.unwrap();
        client.write_all(b"0123456789").await.unwrap();
        drop(client);
        assert_eq!(read_message(&mut server, "p").await.unwrap_err().kind(), ErrorKind::UnexpectedEof);
        // exactly the limit is fine
        let (mut client, mut server) = tokio::io::duplex(1 << 16);
        let big = vec![7u8; MAX_MESSAGE];
        let writer = tokio::spawn(async move { client.write_all(&pb::frame(&big)).await.unwrap() });
        assert_eq!(read_message(&mut server, "p").await.unwrap().unwrap().len(), MAX_MESSAGE);
        writer.await.unwrap();
    }
}
