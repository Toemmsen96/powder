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

use super::prudp::{Packet, DATA, FLAG_ACK, FLAG_NEED_ACK, FLAG_RELIABLE};
use super::{rmc, secure};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Plaintext bytes per DATA fragment (the game sends ~730-780 B).
pub const FRAGMENT_SIZE: usize = 700;
pub const RETRANSMIT_S: f64 = 0.5;
pub const MAX_TRIES: u32 = 12;
/// The game PINGs every few seconds while connected.
pub const IDLE_TIMEOUT_S: f64 = 60.0;

/// Sends one datagram (non-blocking; UDP may drop it anyway).
pub type SendFn = Arc<dyn Fn(&[u8], SocketAddr) + Send + Sync>;

/// True if 16-bit sequence id `a` comes before `b` (wrap-safe).
pub fn seq_before(a: u16, b: u16) -> bool {
    a.wrapping_sub(b) > 0x8000
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
    next_seq: u16,
    /// seq -> (raw, last sent, tries)
    unacked: HashMap<u16, (Vec<u8>, f64, u32)>,
    pub last_seen: f64,
    pub closed: bool,
    /// The player, after LoginWithToken.
    pub profile_id: Option<String>,
    next_call_id: u32,
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
            pending: HashMap::new(), fragments: vec![], next_seq: 1, unacked: HashMap::new(),
            last_seen: now, closed: false, profile_id: None, next_call_id: 0x10000,
        }
    }

    /// A packet from us to this client, with the connection's session id and signature.
    pub fn reply(&self, kind: u8, flags: u8, sequence_id: u16) -> Packet {
        Packet { source: self.server_stream, dest: self.client_stream, kind, flags, sequence_id,
                 session_id: self.session_id, signature: self.client_sig, ..Default::default() }
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

    /// Handle a client DATA packet; returns the complete messages it finishes (in order).
    pub fn receive_data(&mut self, p: &Packet) -> Vec<Vec<u8>> {
        if p.has(FLAG_NEED_ACK) {
            let mut ack = self.reply(DATA, FLAG_ACK, p.sequence_id);
            ack.fragment_id = p.fragment_id;
            self.send(&ack, None);
        }
        if seq_before(p.sequence_id, self.expected_seq) || self.pending.contains_key(&p.sequence_id) {
            return vec![]; // duplicate
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
        self.pending.insert(p.sequence_id, (p.fragment_id, plain));
        let mut messages = vec![];
        while let Some((frag_id, chunk)) = self.pending.remove(&self.expected_seq) {
            self.expected_seq = self.expected_seq.wrapping_add(1);
            self.fragments.extend_from_slice(&chunk);
            if frag_id == 0 {
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
        let call_id = call_id.unwrap_or_else(|| {
            self.next_call_id += 1;
            self.next_call_id - 1
        });
        self.send_message(&rmc::build_request(protocol, call_id, method, params), now);
        call_id
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
