//! NEX / Quazal RendezVous transport: PRUDP (classic V0) over UDP with the secure handshake.
//!
//! Demultiplexes datagrams by client address into Connections, performs SYN / CONNECT (ECDH key exchange
//! signed with our ECDSA key, whose public half the HTTPS server publishes in /rendezvous/configuration), and
//! hands complete RMC requests to the Dispatcher. Transport-free like storm.rs: `received` and `tick` are
//! driven by [`start`]'s socket loop, or by tests.
//!
//! Hardened against spoofed source addresses and floods (unlike the Python server): a SYN only makes a half-open
//! connection (kept aside, so it can't replace a live one), answered with a SYN-ACK no larger than the SYN; a
//! CONNECT must follow a SYN and carry the signature its SYN-ACK gave (see [server] nex_check_signatures) and a key
//! exchange (Steep always sends one; a plaintext channel is refused); key exchanges are rate-limited per source
//! IP; connections and half-open ones are capped in all and per IP. Everything we send reliably (and so
//! retransmit) goes to a connection whose key exchange completed, which a spoofed source never gets to.

use super::connection::{Connection, Outbox, SendFn};
use crate::config::CheckSignatures;
use super::dispatch::{Ctx, Dispatcher};
use super::prudp::{Packet, CONNECT, DATA, DISCONNECT, FLAG_ACK, FLAG_KEY_EXCHANGE, FLAG_NEED_ACK, PING, SYN};
use super::{rmc, secure};
use anyhow::{Context, Result};
use p256::ecdsa::SigningKey;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::Notify;

pub const TICK_S: f64 = 0.25;
/// A half-open connection (SYN answered, no CONNECT yet) is forgotten after this; the game CONNECTs at once.
pub const HALF_OPEN_TIMEOUT_S: f64 = 15.0;
/// Half-open connections kept, in all (beyond it the oldest is forgotten) and per source IP.
pub const MAX_HALF_OPEN: usize = 256;
pub const MAX_HALF_OPEN_PER_IP: usize = 8;
/// Connections at most, in all and per source IP (one per game; a LAN party shares one public IP).
pub const MAX_CONNECTIONS: usize = 1024;
pub const MAX_CONNECTIONS_PER_IP: usize = 64;
/// Key exchanges (an ECDH and an ECDSA signature each) per source IP: a burst of HANDSHAKE_BURST, then one per
/// HANDSHAKE_REFILL_S. A game makes one per session (a repeated CONNECT is answered from the cache).
pub const HANDSHAKE_BURST: f64 = 10.0;
pub const HANDSHAKE_REFILL_S: f64 = 6.0;

/// Told the profile id of a logged-in game whose connection closed.
pub type DisconnectFn = Box<dyn Fn(&str) + Send + Sync>;

/// Caps and rate limits count per IP; an IPv6 host usually has a whole /64.
fn ip_key(addr: SocketAddr) -> IpAddr {
    match addr.ip().to_canonical() {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

pub struct NexServer {
    /// Connections whose CONNECT completed.
    pub conns: HashMap<SocketAddr, Connection>,
    /// Half-open connections: SYN answered, waiting for the CONNECT.
    pub syns: HashMap<SocketAddr, Connection>,
    pub dispatcher: Dispatcher,
    signing_key: SigningKey,
    public_host: String,
    port: u16,
    send_fn: SendFn,
    /// Never reused.
    next_pid: u32,
    /// Source IP -> (key exchanges left, when counted).
    handshakes: HashMap<IpAddr, (f64, f64)>,
    /// Notified when a connection's outbox has requests to send (see [`start`]).
    pub wake: Arc<Notify>,
    /// Called with the player's profile id when a logged-in game's connection closes (e.g. to take it out of
    /// its matchmaking sessions).
    pub on_disconnect: Option<DisconnectFn>,
    /// `[server] nex_check_signatures` (Enforce unless the server's config says otherwise).
    pub check: CheckSignatures,
}

impl NexServer {
    pub fn new(dispatcher: Dispatcher, signing_key: SigningKey, public_host: &str, port: u16, send_fn: SendFn)
               -> NexServer {
        NexServer { conns: HashMap::new(), syns: HashMap::new(), dispatcher, signing_key,
                    public_host: public_host.into(), port, send_fn, next_pid: 0x1001, handshakes: HashMap::new(),
                    wake: Arc::new(Notify::new()), on_disconnect: None,
                    check: CheckSignatures::Enforce }
    }

    pub fn received(&mut self, now: f64, data: &[u8], addr: SocketAddr) {
        self.receive(now, data, addr);
        self.flush(now);
    }

    /// Send what was queued on the connections' outboxes (server-initiated requests).
    pub fn flush(&mut self, now: f64) {
        for c in self.conns.values_mut() {
            c.flush_outbox(now);
        }
    }

    fn receive(&mut self, now: f64, data: &[u8], addr: SocketAddr) {
        let p = match Packet::decode(data) {
            Ok(p) => p,
            Err(e) => {
                log::debug!(target: "powder.nex", "{addr}: undecodable datagram ({e})");
                return;
            }
        };
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
        if !c.signed(&p, self.check) {
            return;
        }
        c.last_seen = now;
        if p.has(FLAG_ACK) {
            c.on_ack(&p);
        } else if p.kind == DATA {
            let messages = c.receive_data(&p);
            if c.closed {
                return self.drop_conn(addr, "message too large");
            }
            for message in messages {
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

    fn syn_ack(c: &Connection) -> Packet {
        let mut ack = c.reply(SYN, FLAG_ACK, 0);
        ack.session_id = 0;
        ack.signature = [0; 4];
        ack.connection_signature = c.server_sig;
        ack
    }

    /// A half-open connection, kept aside: a live connection from the same address stays until the new one's
    /// CONNECT arrives (a spoofed SYN can't cut a game off).
    fn on_syn(&mut self, addr: SocketAddr, p: &Packet, now: f64) {
        if let Some(c) = self.syns.get(&addr) {
            return c.send_raw(&Self::syn_ack(c).encode()); // our SYN-ACK was lost: the same answer
        }
        let ip = ip_key(addr);
        if self.syns.keys().filter(|a| ip_key(**a) == ip).count() >= MAX_HALF_OPEN_PER_IP {
            log::debug!(target: "powder.nex", "{addr}: SYN ignored ({MAX_HALF_OPEN_PER_IP} half-open from {ip})");
            return;
        }
        if self.syns.len() >= MAX_HALF_OPEN {
            let oldest = self.syns.values().min_by(|a, b| a.last_seen.total_cmp(&b.last_seen)).map(|c| c.addr);
            if let Some(oldest) = oldest {
                self.syns.remove(&oldest);
            }
        }
        let mut c = Connection::new(addr, self.send_fn.clone(), 0, p.source, p.dest, now);
        c.outbox = Arc::new(Outbox::new(self.wake.clone()));
        c.send(&Self::syn_ack(&c), None);
        log::debug!(target: "powder.nex", "{addr} SYN");
        self.syns.insert(addr, c);
    }

    /// One key exchange from `ip`, if its budget allows.
    fn take_handshake(&mut self, ip: IpAddr, now: f64) -> bool {
        let (left, at) = self.handshakes.entry(ip).or_insert((HANDSHAKE_BURST, now));
        *left = (*left + (now - *at).max(0.0) / HANDSHAKE_REFILL_S).min(HANDSHAKE_BURST);
        *at = now;
        if *left < 1.0 {
            return false;
        }
        *left -= 1.0;
        true
    }

    fn on_connect(&mut self, addr: SocketAddr, p: &Packet, now: f64) {
        // for the half-open connection or the live one, by the signature it carries
        let sig = |c: Option<&Connection>| c.map(|c| c.server_sig);
        let (half_open, live) = (sig(self.syns.get(&addr)), sig(self.conns.get(&addr)));
        let to_half_open = if half_open == Some(p.signature) {
            true
        } else if live == Some(p.signature) {
            false
        } else {
            // a wrong signature: logged, and taken anyway unless the check is enforced
            let check = self.check;
            let taken = match (self.syns.get_mut(&addr), self.conns.get_mut(&addr)) {
                (Some(c), _) => c.signed(p, check).then_some(true),
                (None, Some(c)) => c.signed(p, check).then_some(false),
                (None, None) => {
                    log::debug!(target: "powder.nex", "{addr}: CONNECT without a SYN before it ignored");
                    None
                }
            };
            match taken {
                Some(to_half_open) => to_half_open,
                None => return,
            }
        };
        if !to_half_open {
            let c = self.conns.get_mut(&addr).expect("live");
            c.last_seen = now;
            if let Some(raw) = &c.connect_ack {
                c.send_raw(raw); // a retransmitted CONNECT: the same answer
            }
            return;
        }
        if !p.has(FLAG_KEY_EXCHANGE) || p.key_blob.is_empty() {
            log::warn!(target: "powder.nex", "{addr}: CONNECT without key exchange refused (Steep always sends one)");
            self.syns.remove(&addr);
            return;
        }
        let ip = ip_key(addr);
        if !self.conns.contains_key(&addr) {
            let from_ip = self.conns.keys().filter(|a| ip_key(**a) == ip).count();
            if self.conns.len() >= MAX_CONNECTIONS || from_ip >= MAX_CONNECTIONS_PER_IP {
                log::warn!(target: "powder.nex", "{addr}: CONNECT refused ({} connections, {from_ip} from {ip}; at most \
                           {MAX_CONNECTIONS}, {MAX_CONNECTIONS_PER_IP} per IP)", self.conns.len());
                self.syns.remove(&addr);
                return;
            }
        }
        if !self.take_handshake(ip, now) {
            // the half-open connection stays: the game's next CONNECT may fit the budget
            log::debug!(target: "powder.nex", "{addr}: CONNECT refused for now (key exchanges from {ip} rate-limited)");
            return;
        }
        let mut c = self.syns.remove(&addr).expect("checked above");
        let (blob, sig, key) = match secure::server_handshake(&self.signing_key, &p.key_blob) {
            Ok(h) => h,
            Err(e) => {
                log::warn!(target: "powder.nex", "{addr}: CONNECT with a bad key exchange ({e:#})");
                return;
            }
        };
        c.pid = self.next_pid;
        self.next_pid = self.next_pid.wrapping_add(1);
        c.client_sig = p.connection_signature;
        c.expected_seq = p.sequence_id.wrapping_add(1);
        c.last_seen = now;
        let mut ack = c.reply(CONNECT, FLAG_ACK | FLAG_KEY_EXCHANGE, p.sequence_id);
        ack.connection_signature = [0; 4];
        ack.key_blob = blob;
        ack.key_signature = sig;
        ack.payload = secure::encrypt_payload(&key, p.sequence_id, b"", false);
        c.key = Some(key);
        let raw = ack.encode();
        c.send_raw(&raw);
        c.connect_ack = Some(raw);
        self.drop_conn(addr, "a new connection from the same address");
        log::info!(target: "powder.nex", "{addr} CONNECT -> connection pid 0x{:x}, secure channel up", c.pid);
        self.conns.insert(addr, c);
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
        conn.flush_outbox(now); // requests the handler made to its own game go first, as they would in Python
        conn.send_message(&out, now);
    }

    pub fn drop_conn(&mut self, addr: SocketAddr, why: &str) {
        if let Some(mut c) = self.conns.remove(&addr) {
            c.closed = true;
            c.outbox.close();
            // a later login of the same player shares the session's channels: clear the connection there, unless
            // it was already replaced by a newer one
            let channel: Arc<dyn crate::core::sessions::NexChannel> = c.outbox.clone();
            if let Some(s) = c.session.take().filter(|s| s.channels.clear_nex_if(&channel)) {
                if let Some(f) = &self.on_disconnect {
                    f(s.profile_id());
                }
            }
            log::info!(target: "powder.nex", "{addr} connection closed ({why})");
        }
    }

    /// Retransmits and timeouts; call every TICK_S.
    pub fn tick(&mut self, now: f64) {
        let dead: Vec<SocketAddr> = self.conns.iter_mut().filter_map(|(a, c)| (!c.tick(now)).then_some(*a)).collect();
        for addr in dead {
            self.drop_conn(addr, "timeout");
        }
        self.syns.retain(|_, c| now - c.last_seen <= HALF_OPEN_TIMEOUT_S);
        // a full budget needs no entry
        self.handshakes.retain(|_, (left, at)| *left + (now - *at).max(0.0) / HANDSHAKE_REFILL_S < HANDSHAKE_BURST);
        self.flush(now);
    }
}

/// Run one step of the server's loops; a panic is logged instead of ending the loop (one bad datagram must not
/// stop NEX for good). The server's lock is released while unwinding (parking_lot locks don't poison).
fn guarded(what: &str, step: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(step)) {
        let why = panic.downcast_ref::<&str>().map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
        log::error!(target: "powder.nex", "{what} panicked ({why}); NEX carries on");
    }
}

/// Bind the NEX port and serve it until the process ends (one receive loop and the timers).
pub async fn start(dispatcher: Dispatcher, signing_key: SigningKey, bind: &str, public_host: &str, port: u16,
                   check: CheckSignatures) -> Result<Arc<Mutex<NexServer>>> {
    let sock = Arc::new(UdpSocket::bind(format!("{bind}:{port}")).await
        .with_context(|| format!("NEX on udp {bind}:{port}"))?);
    let out = sock.clone();
    let send_fn: SendFn = Arc::new(move |data, to| {
        let _ = out.try_send_to(data, to);
    });
    let mut nex = NexServer::new(dispatcher, signing_key, public_host, port, send_fn);
    nex.check = check;
    let server = Arc::new(Mutex::new(nex));
    let started = Instant::now();
    let (srv, rx) = (server.clone(), sock);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match rx.recv_from(&mut buf).await {
                Ok((n, from)) => guarded(&format!("a datagram from {from}"), || {
                    srv.lock().received(started.elapsed().as_secs_f64(), &buf[..n], from)
                }),
                Err(e) => log::debug!(target: "powder.nex", "recv: {e}"),
            }
        }
    });
    let srv = server.clone();
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs_f64(TICK_S));
        loop {
            timer.tick().await;
            guarded("the timer", || srv.lock().tick(started.elapsed().as_secs_f64()));
        }
    });
    let (srv, wake) = (server.clone(), server.lock().wake.clone());
    tokio::spawn(async move {
        loop {
            wake.notified().await;
            guarded("sending queued requests", || srv.lock().flush(started.elapsed().as_secs_f64()));
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

    fn echo_dispatcher() -> Dispatcher {
        let mut d = Dispatcher::new(Arc::new(Assets::default()), None);
        d.handle("TestProtocol::Echo_V1", Box::new(|ctx, r| {
            ctx.conn.profile_id = Some("p1".into());
            let mut w = rmc::Writer::new();
            w.string(&format!("hi {}", r.string()?));
            Ok(w.take())
        }));
        d
    }

    fn connect_packet(server_sig: [u8; 4], blob: Vec<u8>) -> Packet {
        Packet { kind: CONNECT, flags: FLAG_NEED_ACK | FLAG_RELIABLE | FLAG_KEY_EXCHANGE, sequence_id: 1,
                 signature: server_sig, connection_signature: [1, 2, 3, 4], key_blob: blob, ..Default::default() }
    }

    /// The game's side of the handshake: SYN, CONNECT signed with the SYN-ACK's signature -> (signature, key).
    fn handshake(srv: &mut NexServer, sent: &Sent, game: SocketAddr, now: f64) -> ([u8; 4], [u8; 16]) {
        srv.received(now, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), game);
        let syn_ack = last(sent);
        assert_eq!((syn_ack.kind, syn_ack.has(FLAG_ACK)), (SYN, true));
        let (blob, eph) = secure::client_key();
        srv.received(now, &connect_packet(syn_ack.connection_signature, blob).encode(), game);
        let ack = last(sent);
        assert!(ack.kind == CONNECT && ack.has(FLAG_KEY_EXCHANGE) && !ack.key_blob.is_empty());
        (syn_ack.connection_signature, secure::client_session_key(&eph, &ack.key_blob).unwrap())
    }

    fn echo_call(sig: [u8; 4], key: &[u8; 16], seq: u16, call_id: u32) -> Packet {
        let mut params = rmc::Writer::new();
        params.string("game");
        let call = rmc::build_request("TestProtocol", call_id, "TestProtocol::Echo_V1", &params.take());
        Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: seq, signature: sig,
                 payload: secure::encrypt_payload(key, seq, &call, false), ..Default::default() }
    }

    #[test]
    fn handshake_then_an_encrypted_call() {
        let (mut srv, sent) = server(echo_dispatcher());
        let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();

        srv.received(0.0, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), game);
        let syn_ack = last(&sent);
        assert_eq!((syn_ack.kind, syn_ack.has(FLAG_ACK)), (SYN, true));
        assert_eq!(syn_ack.encode().len(), 18, "no larger than the SYN");
        srv.received(0.05, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), game);
        assert_eq!(last(&sent), syn_ack, "a repeated SYN gets the same answer");
        let sig = syn_ack.connection_signature;

        let (blob, eph) = secure::client_key();
        let connect = connect_packet(sig, blob);
        srv.received(0.1, &connect.encode(), game);
        let ack = last(&sent);
        assert!(ack.has(FLAG_KEY_EXCHANGE) && !ack.key_blob.is_empty());
        let key = secure::client_session_key(&eph, &ack.key_blob).unwrap();
        srv.received(0.2, &connect.encode(), game); // a repeated CONNECT gets the same answer
        assert_eq!(sent.lock().last().unwrap().0, ack.encode());

        srv.received(0.3, &echo_call(sig, &key, 2, 7).encode(), game);
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
    fn a_connect_needs_the_syn_acks_signature_and_a_key_exchange() {
        let (mut srv, sent) = server(echo_dispatcher());
        let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();
        // no SYN before it (a spoofed source never sees our SYN-ACK): no answer, no ECDH
        srv.received(0.0, &connect_packet([0; 4], secure::client_key().0).encode(), game);
        assert!(sent.lock().is_empty() && srv.conns.is_empty() && srv.syns.is_empty());

        srv.received(0.0, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), game);
        let sig = last(&sent).connection_signature;
        let n = sent.lock().len();
        let wrong = [sig[0] ^ 1, sig[1], sig[2], sig[3]];
        srv.received(0.1, &connect_packet(wrong, secure::client_key().0).encode(), game);
        assert_eq!(sent.lock().len(), n, "a wrong signature: no answer");
        // without key exchange (a plaintext channel): refused
        let mut plain = connect_packet(sig, vec![]);
        plain.flags &= !FLAG_KEY_EXCHANGE;
        srv.received(0.2, &plain.encode(), game);
        assert!(sent.lock().len() == n && srv.conns.is_empty() && srv.syns.is_empty());
    }

    #[test]
    fn packets_without_our_signature_are_ignored() {
        let (mut srv, sent) = server(echo_dispatcher());
        let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();
        let (sig, key) = handshake(&mut srv, &sent, game, 0.0);
        let n = sent.lock().len();
        let spoofed = [0u8; 4];
        // a call (no ack, no reply), a PING, a DISCONNECT (the connection stays)
        srv.received(0.1, &echo_call(spoofed, &key, 2, 7).encode(), game);
        srv.received(0.1, &Packet { kind: PING, flags: FLAG_NEED_ACK, signature: spoofed, ..Default::default() }.encode(),
                     game);
        srv.received(0.1, &Packet { kind: DISCONNECT, signature: spoofed, ..Default::default() }.encode(), game);
        assert_eq!(sent.lock().len(), n);
        assert_eq!(srv.conns[&game].last_seen, 0.0, "not kept alive either");
        // the game's own (signed) call still works
        srv.received(0.2, &echo_call(sig, &key, 2, 7).encode(), game);
        assert_eq!(sent.lock().len(), n + 2, "ack + reply");
    }

    /// The default setting: a packet without our signature is logged and taken anyway (the game is not confirmed
    /// to sign as our vectors do); "off" does not look at it.
    #[test]
    fn log_and_off_take_unsigned_packets() {
        for check in [CheckSignatures::Log, CheckSignatures::Off] {
            let (mut srv, sent) = server(echo_dispatcher());
            srv.check = check;
            let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();
            let (_, key) = handshake(&mut srv, &sent, game, 0.0);
            let n = sent.lock().len();
            srv.received(0.1, &echo_call([0u8; 4], &key, 2, 7).encode(), game);
            assert_eq!(sent.lock().len(), n + 2, "{check:?}: ack + reply");
        }
    }

    #[test]
    fn a_new_syn_does_not_cut_off_the_live_connection() {
        let (mut srv, sent) = server(echo_dispatcher());
        let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();
        let (sig, key) = handshake(&mut srv, &sent, game, 0.0);
        let pid = srv.conns[&game].pid;
        srv.received(1.0, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), game);
        srv.received(1.1, &echo_call(sig, &key, 2, 7).encode(), game);
        assert_eq!(srv.conns[&game].profile_id.as_deref(), Some("p1"), "still served");
        // the half-open one is forgotten after a while
        srv.tick(1.0 + HALF_OPEN_TIMEOUT_S + 0.5);
        assert!(srv.syns.is_empty());
        // a game that really reconnects (SYN + CONNECT) replaces it
        let (sig2, _) = handshake(&mut srv, &sent, game, 20.0);
        assert!(sig2 != sig && srv.conns[&game].pid != pid && srv.conns[&game].profile_id.is_none());
    }

    #[test]
    fn key_exchanges_are_rate_limited_per_ip() {
        let (mut srv, sent) = server(echo_dispatcher());
        let at = |port: u16| SocketAddr::from(([192, 0, 2, 50], port));
        for port in 0..HANDSHAKE_BURST as u16 {
            handshake(&mut srv, &sent, at(1000 + port), 0.0);
        }
        // over the budget: the SYN is answered, the CONNECT not (yet)
        srv.received(0.0, &Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode(), at(2000));
        let sig = last(&sent).connection_signature;
        let (blob, _) = secure::client_key();
        let n = sent.lock().len();
        srv.received(0.0, &connect_packet(sig, blob.clone()).encode(), at(2000));
        assert_eq!(sent.lock().len(), n);
        // another IP is not affected
        handshake(&mut srv, &sent, "192.0.2.51:1000".parse().unwrap(), 0.0);
        // the game's CONNECT retransmit passes once the budget refilled
        srv.received(HANDSHAKE_REFILL_S, &connect_packet(sig, blob).encode(), at(2000));
        assert_eq!(last(&sent).kind, CONNECT);
        assert!(srv.conns.contains_key(&at(2000)));
    }

    #[test]
    fn half_open_connections_are_capped() {
        let (mut srv, _) = server(echo_dispatcher());
        let syn = Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode();
        for port in 0..100 {
            srv.received(0.0, &syn, SocketAddr::from(([192, 0, 2, 50], port)));
        }
        assert_eq!(srv.syns.len(), MAX_HALF_OPEN_PER_IP);
        for i in 0..(MAX_HALF_OPEN as u32 + 50) {
            let ip = std::net::Ipv4Addr::from(0x0a00_0000 + i);
            srv.received(1.0 + i as f64 * 1e-6, &syn, SocketAddr::from((ip, 1)));
        }
        assert_eq!(srv.syns.len(), MAX_HALF_OPEN);
        assert!(!srv.syns.contains_key(&SocketAddr::from(([10, 0, 0, 0], 1))), "the oldest went first");
        // an IPv6 host's /64 counts as one
        for i in 0..20u16 {
            let ip = Ipv6Addr::new(0x2001, 0xdb8, 0, 1, i, 0, 0, 1);
            srv.received(2.0, &syn, SocketAddr::from((ip, 1)));
        }
        let v6 = srv.syns.keys().filter(|a| a.is_ipv6()).count();
        assert_eq!(v6, MAX_HALF_OPEN_PER_IP);
    }

    #[test]
    fn an_oversized_message_closes_the_connection() {
        let (mut srv, sent) = server(echo_dispatcher());
        let game: SocketAddr = "192.0.2.50:3074".parse().unwrap();
        let (sig, key) = handshake(&mut srv, &sent, game, 0.0);
        let chunk = vec![0u8; 60_000];
        for seq in 2..(2 + (crate::nex::connection::MAX_MESSAGE_BYTES / chunk.len()) as u16 + 1) {
            assert!(srv.conns.contains_key(&game));
            let p = Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: seq, fragment_id: 1,
                             signature: sig, payload: secure::encrypt_payload(&key, seq, &chunk, false),
                             ..Default::default() };
            srv.received(0.1, &p.encode(), game);
        }
        assert!(!srv.conns.contains_key(&game));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panic_or_garbage_does_not_stop_the_receive_loop() {
        let mut d = echo_dispatcher();
        d.handle("TestProtocol::Panic_V1", Box::new(|_, _| panic!("a bug in a handler")));
        let port = 34000 + (std::process::id() % 8000) as u16;
        let key = SigningKey::random(&mut rand::thread_rng());
        start(d, key, "127.0.0.1", "127.0.0.1", port, CheckSignatures::Enforce).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.connect(("127.0.0.1", port)).await.unwrap();
        let recv = |want: u8| {
            let sock = &sock;
            async move {
                let mut buf = vec![0u8; 2048];
                loop {
                    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf)).await
                        .expect("an answer").unwrap();
                    let p = Packet::decode(&buf[..n]).unwrap();
                    if p.kind == want && (want != DATA || !p.has(FLAG_ACK)) {
                        return p;
                    }
                }
            }
        };
        for garbage in [&b""[..], b"\x00", &[0xff; 13], &[0x3f; 600]] {
            sock.send(garbage).await.unwrap();
        }
        sock.send(&Packet { kind: SYN, flags: FLAG_NEED_ACK, ..Default::default() }.encode()).await.unwrap();
        let sig = recv(SYN).await.connection_signature;
        let (blob, eph) = secure::client_key();
        sock.send(&connect_packet(sig, blob).encode()).await.unwrap();
        let key = secure::client_session_key(&eph, &recv(CONNECT).await.key_blob).unwrap();
        let call = rmc::build_request("TestProtocol", 1, "TestProtocol::Panic_V1", b"");
        sock.send(&Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: 2, signature: sig,
                            payload: secure::encrypt_payload(&key, 2, &call, false), ..Default::default() }.encode())
            .await.unwrap();
        sock.send(&echo_call(sig, &key, 3, 2).encode()).await.unwrap();
        let resp = recv(DATA).await;
        let msg = rmc::parse(&secure::decrypt_payload(&key, &resp.payload, Some(resp.sequence_id)).unwrap()).unwrap();
        assert_eq!((msg.call_id, rmc::Reader::new(&msg.body).string().unwrap()), (2, "hi game".to_string()));
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
