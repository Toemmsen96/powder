//! One PRUDP connection (one game client): reliable, ordered, fragmented, encrypted message stream.
//!
//! Reliability rules (from the prototype, which the game accepted live):
//!   * every client packet with NEED_ACK is acked (DATA acks echo the fragment id)
//!   * client DATA is decrypted, buffered by sequence id and delivered in order; fragments are joined until
//!     fragment_id == 0 (the last fragment)
//!   * our DATA is sent RELIABLE|NEED_ACK in FRAGMENT_SIZE chunks and retransmitted until acked
//!
//! 16-bit sequence ids compare wrap-safe; a connection that stops answering (retransmits exhausted) or goes
//! silent (IDLE_TIMEOUT_S) is closed. Times are seconds on the server's monotonic clock.
//!
//! Bounded (unlike the Python server): DATA more than REORDER_WINDOW ids ahead of the next one to deliver, or
//! that does not decrypt, is dropped without an ack (the game sends it again); a connection whose buffered
//! message passes MAX_MESSAGE_BYTES is closed.

use crate::config::CheckSignatures;
use super::prudp::{Packet, CONNECT, DATA, FLAG_ACK, FLAG_NEED_ACK, FLAG_RELIABLE};
use super::{rmc, secure};
use crate::core::sessions::{NexChannel, PlayerSession};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Plaintext bytes per DATA fragment (the game sends ~730-780 B).
pub const FRAGMENT_SIZE: usize = 700;
pub const RETRANSMIT_S: f64 = 0.5;
pub const MAX_TRIES: u32 = 12;
/// The game PINGs every few seconds while connected.
pub const IDLE_TIMEOUT_S: f64 = 60.0;
/// Out-of-order DATA buffered ahead of the next sequence id to deliver (in-order DATA is never held back; this
/// only bounds what a lost packet makes us keep: 256 of the game's fragments are ~180 KB).
pub const REORDER_WINDOW: u16 = 256;
/// Largest client message (all fragments of one RMC request, plus what is buffered out of order). The game's
/// requests are a few KB (LoginWithToken ~3 KB, PostEvents batches); files go over HTTPS.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Sends one datagram (non-blocking; UDP may drop it anyway).
pub type SendFn = Arc<dyn Fn(&[u8], SocketAddr) + Send + Sync>;

/// True if 16-bit sequence id `a` comes before `b` (wrap-safe).
pub fn seq_before(a: u16, b: u16) -> bool {
    a.wrapping_sub(b) > 0x8000
}

type QueuedRequest = (String, String, Vec<u8>, u32);

/// A connection's server-initiated requests, queued by anyone (a player's session holds it as its NexChannel) without
/// taking the server's lock; the server sends them on its next pass (`Connection::flush_outbox`), which `wake`
/// asks for at once.
pub struct Outbox {
    /// (protocol, method, params, call id)
    queue: Mutex<Vec<QueuedRequest>>,
    next_call_id: AtomicU32,
    closed: AtomicBool,
    wake: Arc<Notify>,
}

impl Outbox {
    pub fn new(wake: Arc<Notify>) -> Outbox {
        Outbox { queue: Mutex::default(), next_call_id: AtomicU32::new(0x10000), closed: AtomicBool::new(false), wake }
    }

    fn call_id(&self, call_id: Option<u32>) -> u32 {
        call_id.unwrap_or_else(|| self.next_call_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The connection is gone: later requests are dropped.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.queue.lock().clear();
    }
}

impl NexChannel for Outbox {
    fn send_request(&self, protocol: &str, method: &str, params: &[u8], call_id: Option<u32>) -> u32 {
        let call_id = self.call_id(call_id);
        if !self.closed.load(Ordering::Relaxed) {
            self.queue.lock().push((protocol.into(), method.into(), params.to_vec(), call_id));
            self.wake.notify_one();
        }
        call_id
    }
}

pub struct Connection {
    pub addr: SocketAddr,
    send_fn: SendFn,
    /// Our id for this connection (Register's connection id).
    pub pid: u32,
    /// StationURL DID, never 0.
    pub did: u32,
    pub client_stream: u8,
    pub server_stream: u8,
    pub server_sig: [u8; 4],
    pub session_id: u8,
    pub client_sig: [u8; 4],
    pub key: Option<[u8; 16]>,
    /// Cached, resent if the client repeats CONNECT.
    pub connect_ack: Option<Vec<u8>>,
    /// Next client DATA sequence id to deliver.
    pub expected_seq: u16,
    pending: HashMap<u16, (u8, Vec<u8>)>,
    fragments: Vec<u8>,
    /// Bytes in `pending` and `fragments`.
    buffered: usize,
    /// A packet with a wrong signature was logged (the first one is a warning).
    warned_signature: bool,
    logged_match: bool,
    next_seq: u16,
    /// seq -> (raw, last sent, tries)
    unacked: HashMap<u16, (Vec<u8>, f64, u32)>,
    pub last_seen: f64,
    pub closed: bool,
    /// The player, after LoginWithToken.
    pub profile_id: Option<String>,
    /// Their session, after LoginWithToken.
    pub session: Option<Arc<PlayerSession>>,
    /// Server-initiated requests from outside the server (the session's NexChannel).
    pub outbox: Arc<Outbox>,
}

impl Connection {
    pub fn new(addr: SocketAddr, send_fn: SendFn, pid: u32, client_stream: u8, server_stream: u8, now: f64)
               -> Connection {
        let random: [u8; 9] = rand::random();
        Connection {
            addr, send_fn, pid,
            did: 1 + rand::random::<u32>() % 0x7FFF_FFFF,
            client_stream, server_stream,
            server_sig: random[..4].try_into().unwrap(),
            session_id: if random[4] == 0 { 1 } else { random[4] },
            client_sig: [0; 4],
            key: None, connect_ack: None, expected_seq: 0,
            pending: HashMap::new(), fragments: vec![], buffered: 0, warned_signature: false, logged_match: false, next_seq: 1,
            unacked: HashMap::new(),
            last_seen: now, closed: false, profile_id: None, session: None,
            outbox: Arc::new(Outbox::new(Arc::new(Notify::new()))),
        }
    }

    /// A packet from us to this client, with the connection's session id and signature.
    pub fn reply(&self, kind: u8, flags: u8, sequence_id: u16) -> Packet {
        Packet { source: self.server_stream, dest: self.client_stream, kind, flags, sequence_id,
                 session_id: self.session_id, signature: self.client_sig, ..Default::default() }
    }

    /// Whether a client packet carries the signature our SYN-ACK gave the client (see `[server] nex_check_signatures`).
    /// `mode` Log takes it anyway (and says so once per connection, either way), Off does not look.
    pub fn signed(&mut self, p: &Packet, mode: CheckSignatures) -> bool {
        if mode == CheckSignatures::Off {
            return true;
        }
        if p.signature == self.server_sig {
            if mode == CheckSignatures::Log && p.kind != CONNECT && !self.logged_match {
                self.logged_match = true;
                log::info!(target: "powder.nex", "{}: the game's packets carry our signature (type {}; \
                           nex_check_signatures)", self.addr, p.kind);
            }
            return true;
        }
        let what = if mode == CheckSignatures::Enforce { "ignored" } else { "taken anyway" };
        if self.warned_signature {
            log::debug!(target: "powder.nex", "{}: type {} with a wrong signature {what}", self.addr, p.kind);
        } else {
            self.warned_signature = true;
            log::warn!(target: "powder.nex", "{}: type {} with a wrong signature ({}, ours {}) {what} (spoofed, or \
                       the game signs differently: see [server] nex_check_signatures)", self.addr, p.kind,
                       hex::encode(p.signature), hex::encode(self.server_sig));
        }
        mode != CheckSignatures::Enforce
    }

    pub fn send_raw(&self, raw: &[u8]) {
        (self.send_fn)(raw, self.addr);
    }

    pub fn send(&mut self, p: &Packet, reliable_at: Option<f64>) {
        let raw = p.encode();
        self.send_raw(&raw);
        if let Some(now) = reliable_at {
            self.unacked.insert(p.sequence_id, (raw, now, 1));
        }
    }

    pub fn on_ack(&mut self, p: &Packet) {
        self.unacked.remove(&p.sequence_id);
    }

    fn ack_data(&mut self, p: &Packet) {
        if p.has(FLAG_NEED_ACK) {
            let mut ack = self.reply(DATA, FLAG_ACK, p.sequence_id);
            ack.fragment_id = p.fragment_id;
            self.send(&ack, None);
        }
    }

    /// Handle a client DATA packet; returns the complete messages it finishes (in order). Sets `closed` when the
    /// client's message grows past MAX_MESSAGE_BYTES.
    pub fn receive_data(&mut self, p: &Packet) -> Vec<Vec<u8>> {
        if seq_before(p.sequence_id, self.expected_seq) || self.pending.contains_key(&p.sequence_id) {
            self.ack_data(p); // a duplicate: our ack was lost
            return vec![];
        }
        if p.sequence_id.wrapping_sub(self.expected_seq) >= REORDER_WINDOW {
            log::debug!(target: "powder.nex", "{}: DATA seq {} too far ahead of {}, dropped", self.addr, p.sequence_id,
                        self.expected_seq);
            return vec![];
        }
        let plain = match (&self.key, p.payload.is_empty()) {
            (Some(key), false) => match secure::decrypt_payload(key, &p.payload, Some(p.sequence_id)) {
                Ok(plain) => plain,
                Err(e) => {
                    log::warn!(target: "powder.nex", "{}: undecryptable DATA seq {} ({e})", self.addr, p.sequence_id);
                    return vec![];
                }
            },
            _ => p.payload.clone(),
        };
        self.ack_data(p);
        self.buffered += plain.len();
        if self.buffered > MAX_MESSAGE_BYTES {
            log::warn!(target: "powder.nex", "{}: message over {MAX_MESSAGE_BYTES} bytes, dropping connection",
                       self.addr);
            self.closed = true;
            (self.pending, self.fragments, self.buffered) = (HashMap::new(), vec![], 0);
            return vec![];
        }
        self.pending.insert(p.sequence_id, (p.fragment_id, plain));
        let mut messages = vec![];
        while let Some((frag_id, chunk)) = self.pending.remove(&self.expected_seq) {
            self.expected_seq = self.expected_seq.wrapping_add(1);
            self.fragments.extend_from_slice(&chunk);
            if frag_id == 0 {
                self.buffered -= self.fragments.len();
                messages.push(std::mem::take(&mut self.fragments));
            }
        }
        messages
    }

    pub fn send_message(&mut self, data: &[u8], now: f64) {
        if self.closed {
            return;
        }
        let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[]] } else { data.chunks(FRAGMENT_SIZE).collect() };
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let mut p = self.reply(DATA, FLAG_RELIABLE | FLAG_NEED_ACK, seq);
            p.fragment_id = if i == last { 0 } else { (i + 1) as u8 };
            p.payload = match &self.key {
                Some(key) => secure::encrypt_payload(key, seq, chunk, false),
                None => chunk.to_vec(),
            };
            self.send(&p, Some(now));
        }
    }

    /// Server-initiated RMC request (e.g. NotificationProtocol::ProcessNotificationEvent_V1, which the real
    /// server sends with call id 0). Returns the call id used.
    pub fn send_request(&mut self, protocol: &str, method: &str, params: &[u8], call_id: Option<u32>, now: f64) -> u32 {
        let call_id = self.outbox.call_id(call_id);
        self.send_message(&rmc::build_request(protocol, call_id, method, params), now);
        call_id
    }

    /// Send the requests queued on the outbox.
    pub fn flush_outbox(&mut self, now: f64) {
        let queued = std::mem::take(&mut *self.outbox.queue.lock());
        for (protocol, method, params, call_id) in queued {
            self.send_request(&protocol, &method, &params, Some(call_id), now);
        }
    }

    /// Retransmit overdue packets. Returns false when the connection should be dropped.
    pub fn tick(&mut self, now: f64) -> bool {
        if self.closed {
            return false;
        }
        if now - self.last_seen > IDLE_TIMEOUT_S {
            log::info!(target: "powder.nex", "{}: idle for {:.0}s, dropping connection", self.addr, now - self.last_seen);
            return false;
        }
        let mut resend = vec![];
        for (seq, (raw, last, tries)) in self.unacked.iter_mut() {
            if now - *last < RETRANSMIT_S {
                continue;
            }
            if *tries >= MAX_TRIES {
                log::warn!(target: "powder.nex", "{}: seq {seq} never acked after {tries} tries, dropping connection",
                           self.addr);
                return false;
            }
            resend.push(raw.clone());
            *last = now;
            *tries += 1;
        }
        for raw in resend {
            self.send_raw(&raw);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(expected_seq: u16, key: Option<[u8; 16]>) -> (Connection, Arc<Mutex<Vec<Vec<u8>>>>) {
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let s = sent.clone();
        let mut c = Connection::new("127.0.0.1:1".parse().unwrap(), Arc::new(move |d, _| s.lock().push(d.to_vec())), 1,
                                    0x3f, 0x31, 0.0);
        (c.expected_seq, c.key) = (expected_seq, key);
        (c, sent)
    }

    fn pkt(seq: u16, data: &[u8]) -> Packet {
        Packet { kind: DATA, flags: FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id: seq, payload: data.to_vec(),
                 ..Default::default() }
    }

    #[test]
    fn seq_before_is_wrap_safe() {
        assert!(seq_before(1, 2) && !seq_before(2, 1));
        assert!(seq_before(65535, 0) && seq_before(65530, 5) && !seq_before(5, 65530));
    }

    #[test]
    fn in_order_delivery_across_sequence_wrap() {
        let (mut c, sent) = conn(65534, None);
        assert_eq!(c.receive_data(&pkt(65534, b"a")), vec![b"a".to_vec()]);
        assert!(c.receive_data(&pkt(0, b"c")).is_empty()); // early: buffered until 65535 arrives
        assert_eq!(c.receive_data(&pkt(65535, b"b")), vec![b"b".to_vec(), b"c".to_vec()]);
        assert!(c.receive_data(&pkt(65535, b"b")).is_empty()); // duplicate after wrap
        assert_eq!(c.receive_data(&pkt(1, b"d")), vec![b"d".to_vec()]);
        assert_eq!(sent.lock().len(), 5); // all 5 NEED_ACK packets acked (dup too)
    }

    #[test]
    fn data_far_ahead_or_undecryptable_is_dropped_unacked() {
        let key = [3u8; 16];
        let (mut c, sent) = conn(10, Some(key));
        let enc = |seq: u16, data: &[u8]| pkt(seq, &secure::encrypt_payload(&key, seq, data, false));
        // the window's last id is buffered and acked; one past it is neither (the game sends it again)
        assert!(c.receive_data(&enc(10 + REORDER_WINDOW - 1, b"z")).is_empty());
        assert!(c.receive_data(&enc(10 + REORDER_WINDOW, b"far")).is_empty());
        assert_eq!(sent.lock().len(), 1);
        assert_eq!(c.pending.len(), 1);
        // garbage in the window: not acked, not buffered; the real packet with that id is then taken
        assert!(c.receive_data(&pkt(10, &[0u8; 48])).is_empty());
        assert_eq!((sent.lock().len(), c.pending.len()), (1, 1));
        assert_eq!(c.receive_data(&enc(10, b"a")), vec![b"a".to_vec()]);
        assert_eq!(sent.lock().len(), 2);
    }

    #[test]
    fn buffered_bytes_are_counted_back_down() {
        let (mut c, _) = conn(0, None);
        for round in 0..3u16 {
            let chunk = vec![1u8; MAX_MESSAGE_BYTES / 2];
            let mut first = pkt(round * 2, &chunk);
            first.fragment_id = 1;
            assert!(c.receive_data(&first).is_empty());
            assert_eq!(c.receive_data(&pkt(round * 2 + 1, &chunk)).len(), 1);
            assert!(!c.closed && c.buffered == 0, "round {round}");
        }
        let mut first = pkt(6, &vec![1u8; MAX_MESSAGE_BYTES]);
        first.fragment_id = 1;
        assert!(c.receive_data(&first).is_empty() && !c.closed);
        assert!(c.receive_data(&pkt(7, b"x")).is_empty());
        assert!(c.closed);
    }
}
