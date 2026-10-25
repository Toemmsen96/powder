//! NEX / Quazal RendezVous transport: PRUDP (classic V0) over UDP with the secure handshake.
//!
//! Demultiplexes datagrams by client address into Connections, performs SYN / CONNECT (ECDH key exchange
//! signed with our ECDSA key, whose public half the HTTPS server publishes in /rendezvous/configuration), and
//! hands complete RMC requests to the Dispatcher. Transport-free like storm.rs: `received` and `tick` are
//! driven by [`start`]'s socket loop, or by tests.

use super::connection::{Connection, SendFn};
use super::dispatch::{Ctx, Dispatcher};
use super::prudp::{Packet, CONNECT, DATA, DISCONNECT, FLAG_ACK, FLAG_KEY_EXCHANGE, FLAG_NEED_ACK, PING, SYN};
use super::{rmc, secure};
use anyhow::{Context, Result};
use p256::ecdsa::SigningKey;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

pub const TICK_S: f64 = 0.25;

pub struct NexServer {
    pub conns: HashMap<SocketAddr, Connection>,
    pub dispatcher: Dispatcher,
    signing_key: SigningKey,
    public_host: String,
    port: u16,
    send_fn: SendFn,
    /// Never reused.
    next_pid: u32,
}

impl NexServer {
    pub fn new(dispatcher: Dispatcher, signing_key: SigningKey, public_host: &str, port: u16, send_fn: SendFn)
               -> NexServer {
        NexServer { conns: HashMap::new(), dispatcher, signing_key, public_host: public_host.into(), port, send_fn,
                    next_pid: 0x1001 }
    }

    fn new_conn(&mut self, addr: SocketAddr, p: &Packet, now: f64) -> Connection {
        self.next_pid += 1;
        Connection::new(addr, self.send_fn.clone(), self.next_pid - 1, p.source, p.dest, now)
    }

    pub fn received(&mut self, now: f64, data: &[u8], addr: SocketAddr) {
        let p = match Packet::decode(data) {
            Ok(p) => p,
            Err(e) => {
                log::debug!(target: "powder.nex", "{addr}: undecodable datagram ({e})");
                return;
            }
        };
        if let Some(c) = self.conns.get_mut(&addr) {
            c.last_seen = now;
        }
        if p.kind == SYN && !p.has(FLAG_ACK) {
            return self.on_syn(addr, &p, now);
        }
        if p.kind == CONNECT && !p.has(FLAG_ACK) {
            return self.on_connect(addr, &p, now);
        }
        let Some(c) = self.conns.get_mut(&addr) else {
            log::debug!(target: "powder.nex", "{addr}: type {} for unknown connection ignored", p.kind);
            return;
        };
        if p.has(FLAG_ACK) {
            c.on_ack(&p);
        } else if p.kind == DATA {
            for message in c.receive_data(&p) {
                self.on_message(addr, &message, now);
            }
        } else if p.kind == PING {
            if p.has(FLAG_NEED_ACK) {
                let ack = c.reply(PING, FLAG_ACK, p.sequence_id);
                c.send(&ack, None);
            }
        } else if p.kind == DISCONNECT {
            let ack = c.reply(DISCONNECT, FLAG_ACK, p.sequence_id);
            for _ in 0..3 {
                c.send(&ack, None);
            }
            self.drop_conn(addr, "client disconnected");
        }
    }

    fn on_syn(&mut self, addr: SocketAddr, p: &Packet, now: f64) {
        if self.conns.contains_key(&addr) {
            self.drop_conn(addr, "new SYN from the same address");
        }
        let mut c = self.new_conn(addr, p, now);
        let mut ack = c.reply(SYN, FLAG_ACK, 0);
        ack.session_id = 0;
        ack.signature = [0; 4];
        ack.connection_signature = c.server_sig;
        c.send(&ack, None);
        log::info!(target: "powder.nex", "{addr} SYN -> connection pid 0x{:x}", c.pid);
        self.conns.insert(addr, c);
    }

    fn on_connect(&mut self, addr: SocketAddr, p: &Packet, now: f64) {
        if !self.conns.contains_key(&addr) {
            let c = self.new_conn(addr, p, now);
            self.conns.insert(addr, c);
        }
        let c = self.conns.get_mut(&addr).expect("inserted above");
        if let Some(raw) = &c.connect_ack {
            c.send_raw(raw); // retransmitted CONNECT -> same answer
            return;
        }
        c.client_sig = p.connection_signature;
        c.expected_seq = p.sequence_id.wrapping_add(1);
        let mut ack = c.reply(CONNECT, FLAG_ACK | FLAG_KEY_EXCHANGE, p.sequence_id);
        ack.connection_signature = [0; 4];
        let handshake = if p.has(FLAG_KEY_EXCHANGE) && !p.key_blob.is_empty() {
            Some(secure::server_handshake(&self.signing_key, &p.key_blob))
        } else {
            None
        };
        match handshake {
            Some(Ok((blob, sig, key))) => {
                ack.key_blob = blob;
                ack.key_signature = sig;
                ack.payload = secure::encrypt_payload(&key, p.sequence_id, b"", false);
                c.key = Some(key);
            }
            Some(Err(e)) => {
                log::warn!(target: "powder.nex", "{addr}: CONNECT with a bad key exchange ({e:#})");
                return;
            }
            None => {
                log::warn!(target: "powder.nex", "{addr}: CONNECT without key exchange (unexpected for Steep)");
                ack.flags = FLAG_ACK;
            }
        }
        let raw = ack.encode();
        c.send_raw(&raw);
        c.connect_ack = Some(raw);
        log::info!(target: "powder.nex", "{addr} CONNECT, secure channel up");
    }

    /// The server's station URL in the LoginWithToken reply, as Ubisoft's: CID=1 and a DID that differs per
    /// connection. The game keeps DID with the connection and drops every server-sent notification (e.g.
    /// matchmaking) while it is 0.
    pub fn station_url(&self, c: &Connection) -> String {
        format!("prudp:/address={};port={};CID=1;DID={};sid=1;stream=3;type=2", self.public_host, self.port, c.did)
    }

    fn on_message(&mut self, addr: SocketAddr, data: &[u8], now: f64) {
        let msg = match rmc::parse(data) {
            Ok(m) => m,
            Err(e) => {
                log::warn!(target: "powder.nex", "{addr}: unparseable RMC ({e}): {}",
                           hex::encode(&data[..data.len().min(64)]));
                return;
            }
        };
        if !msg.is_request {
            log::info!(target: "powder.nex", "{addr}: client answered our call #{} {}", msg.call_id,
                       msg.error.as_ref().map(|(n, c)| format!("{n}/{c}")).unwrap_or(msg.method.clone()));
            return;
        }
        let Some(conn) = self.conns.get(&addr) else { return };
        let station_url = self.station_url(conn);
        let conn = self.conns.get_mut(&addr).expect("checked above");
        let out = self.dispatcher.dispatch(&mut Ctx { conn, station_url, now }, &msg);
        conn.send_message(&out, now);
    }

    pub fn drop_conn(&mut self, addr: SocketAddr, why: &str) {
        if let Some(mut c) = self.conns.remove(&addr) {
            c.closed = true;
            log::info!(target: "powder.nex", "{addr} connection closed ({why})");
        }
    }

    /// Retransmits and timeouts; call every TICK_S.
    pub fn tick(&mut self, now: f64) {
        let dead: Vec<SocketAddr> = self.conns.iter_mut().filter_map(|(a, c)| (!c.tick(now)).then_some(*a)).collect();
        for addr in dead {
            self.drop_conn(addr, "timeout");
        }
    }
}

/// Bind the NEX port and serve it until the process ends (one receive loop and the timers).
pub async fn start(dispatcher: Dispatcher, signing_key: SigningKey, bind: &str, public_host: &str, port: u16)
                   -> Result<Arc<Mutex<NexServer>>> {
    let sock = Arc::new(UdpSocket::bind(format!("{bind}:{port}")).await
        .with_context(|| format!("NEX on udp {bind}:{port}"))?);
    let out = sock.clone();
    let send_fn: SendFn = Arc::new(move |data, to| {
        let _ = out.try_send_to(data, to);
    });
    let server = Arc::new(Mutex::new(NexServer::new(dispatcher, signing_key, public_host, port, send_fn)));
    let started = Instant::now();
    let (srv, rx) = (server.clone(), sock);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match rx.recv_from(&mut buf).await {
                Ok((n, from)) => srv.lock().received(started.elapsed().as_secs_f64(), &buf[..n], from),
                Err(e) => log::debug!(target: "powder.nex", "recv: {e}"),
            }
        }
    });
    let srv = server.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs_f64(TICK_S));
        loop {
            timer.tick().await;
            srv.lock().tick(started.elapsed().as_secs_f64());
        }
    });
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Assets;
    use crate::nex::prudp::FLAG_RELIABLE;

    type Sent = Arc<Mutex<Vec<(Vec<u8>, SocketAddr)>>>;

    fn server(dispatcher: Dispatcher) -> (NexServer, Sent) {
        let sent: Sent = Arc::default();
        let s = sent.clone();
        let send_fn: SendFn = Arc::new(move |d, a| s.lock().push((d.to_vec(), a)));
        let key = SigningKey::random(&mut rand::thread_rng());
        (NexServer::new(dispatcher, key, "192.0.2.1", 6005, send_fn), sent)
    }

    fn last(sent: &Sent) -> Packet {
        Packet::decode(&sent.lock().last().unwrap().0).unwrap()
    }

    #[test]
    fn handshake_then_an_encrypted_call() {
        let mut d = Dispatcher::new(Arc::new(Assets::default()), None);
        d.handle("TestProtocol::Echo_V1", Box::new(|ctx, r| {
            ctx.conn.profile_id = Some("p1".into());
            let mut w = rmc::Writer::new();
            w.string(&format!("hi {}", r.string()?));
            Ok(w.take())
        }));
        let (mut srv, sent) = server(d);
        let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();

        srv.received(0.0, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), game);
        let syn_ack = last(&sent);
        assert_eq!((syn_ack.kind, syn_ack.has(FLAG_ACK)), (SYN, true));

        let (blob, eph) = secure::client_key();
        let connect = Packet { kind: CONNECT, flags: FLAG_NEED_ACK | FLAG_RELIABLE | FLAG_KEY_EXCHANGE, sequence_id: 1,
                               connection_signature: [1, 2, 3, 4], key_blob: blob, ..Default::default() };
        srv.received(0.1, &connect.encode(), game);
        let ack = last(&sent);
        assert!(ack.has(FLAG_KEY_EXCHANGE) && !ack.key_blob.is_empty());
        let key = secure::client_session_key(&eph, &ack.key_blob).unwrap();
        srv.received(0.2, &connect.encode(), game); // a repeated CONNECT gets the same answer
        assert_eq!(sent.lock().last().unwrap().0, ack.encode());

        let mut params = rmc::Writer::new();
        params.string("game");
        let call = rmc::build_request("TestProtocol", 7, "TestProtocol::Echo_V1", &params.take());
        let data = Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: 2,
                            payload: secure::encrypt_payload(&key, 2, &call, false), ..Default::default() };
        srv.received(0.3, &data.encode(), game);
        let replies: Vec<Packet> = sent.lock().iter().map(|(d, _)| Packet::decode(d).unwrap()).collect();
        let n = replies.len();
        assert!(replies[n - 2].kind == DATA && replies[n - 2].has(FLAG_ACK)); // our ack of the call
        let resp = &replies[n - 1];
        let msg = rmc::parse(&secure::decrypt_payload(&key, &resp.payload, Some(resp.sequence_id)).unwrap()).unwrap();
        assert_eq!((msg.is_request, msg.call_id, msg.method.as_str()), (false, 7, "TestProtocol::Echo_V1*"));
        assert_eq!(rmc::Reader::new(&msg.body).string().unwrap(), "hi game");
        assert_eq!(srv.conns[&game].profile_id.as_deref(), Some("p1"));

        // unacked, our reply is retransmitted; after MAX_TRIES the connection is dropped
        let before = sent.lock().len();
        srv.tick(1.0);
        assert_eq!(sent.lock().len(), before + 1);
        for i in 0..20 {
            srv.tick(2.0 + i as f64);
        }
        assert!(srv.conns.is_empty());
    }

    #[test]
    fn unknown_calls_are_core_2_and_mods_answer_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.lua"), r#"return function(api)
            api.before_rmc("P::Modded_V1", function(ctx, params) return "M" .. ctx.peer end)
        end"#).unwrap();
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let mut config = crate::config::Config::default();
        config.mods.dir = dir.path().to_path_buf();
        let mods = crate::mods::ModHost::load(&config, Arc::new(crate::repos::Repos::new(db)));
        let d = Dispatcher::new(Arc::new(Assets::default()), Some(Arc::new(Mutex::new(mods))));
        let (srv, _) = server(d);
        let mut conn = Connection::new("192.0.2.9:1".parse().unwrap(), srv.send_fn.clone(), 1, 0x3f, 0x31, 0.0);
        let call = |m: &str| rmc::parse(&rmc::build_request("P", 3, m, b"")).unwrap();
        let mut ctx = Ctx { conn: &mut conn, station_url: String::new(), now: 0.0 };
        let out = rmc::parse(&srv.dispatcher.dispatch(&mut ctx, &call("P::Nothing_V1"))).unwrap();
        assert_eq!(out.error, Some(("Core".into(), 2)));
        let out = rmc::parse(&srv.dispatcher.dispatch(&mut ctx, &call("P::Modded_V1"))).unwrap();
        assert_eq!(out.body, b"M192.0.2.9");
    }
}
