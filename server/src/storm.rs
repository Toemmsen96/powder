//! Storm: Steep's peer-to-peer helpers, NAT detection and traversal (UDP), so the games find each other.
//!
//! The game's configuration names them ("storm"/"punch": detectUrl1, detectUrl2, traversalUrl; see
//! https.rs with_storm). Without them the game switches its multiplayer off. Layout from recordings of
//! Ubisoft's servers:
//!
//! ```text
//! message   u8 kind (0x10 request, 0x11 the game's answer, 0x12 the server's own, 0x13 the server's answer),
//!           u8 type, u16 sequence (big endian; an answer repeats the request's), then items:
//! peer id   16 bytes, handed out by the traversal server ("04 03 01 03", u32 le, u32 le, u32 be counter)
//! key       8 bytes "02 02 00 00" + 4 random bytes, handed out with the id
//! address   "02 04" + u16 port + IPv4 (big endian); "02 06 ..." (IPv6, empty) passed on as it is
//! ```
//!
//! The game talks from small sockets of their own, and from its game socket (the one the other games reach).
//! From the game socket a message carries a fixed 32-byte trailer, and answers to it start with a fixed
//! 27-byte header (the frame of the game's peer packets, all zero for these).
//!
//! Detection (detectUrl1, detectUrl2 and each one's port + 1): 0x1001 "which address do you see?" -> 0x1301
//! with it. 0x1002 / 0x1003 ask for an answer from the other port / other server; Ubisoft's never arrived (the
//! PC was behind NAT) and the game then worked, so they stay unanswered here as well.
//!
//! Traversal (traversalUrl):
//!
//! ```text
//! 0x1040 register      -> 0x1241 peer id, key, address seen    then 0x1141 (id, key) -> 0x1340 (key, id)
//! 0x1043 keep-alive    -> 0x1343 id, key (+ address seen, from the game socket); every ~8 s
//! 0x1044 "connect me"  (id, key, target id, the requester's addresses) -> 0x1244 to the target (requester id,
//!                      target id, the addresses): both then punch (0x1045/0x1145 directly between the games)
//! ```
//!
//! The relay ("routerProvisioningUrl") was never asked for.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;

/// Before answers to the game socket.
pub const HEADER: [u8; 27] = {
    let mut h = [0u8; 27];
    h[22] = 0x80;
    h[26] = 0x20;
    h
};
/// After the game socket's requests.
pub const TRAILER: [u8; 32] = {
    let mut t = [0u8; 32];
    t[0] = 0x08;
    t[1] = 0x07;
    t[2] = 0x1b;
    t[25] = 0x80;
    t[29] = 0x20;
    t
};
const REQUEST: u8 = 0x10;
const ANSWER: u8 = 0x11;
const PUSH: u8 = 0x12;
const REPLY: u8 = 0x13;
const DETECT: u8 = 0x01;
const REGISTER: u8 = 0x40;
const REGISTERED: u8 = 0x41;
const KEEPALIVE: u8 = 0x43;
const CONNECT: u8 = 0x44;
const CONFIRM: u8 = 0x40; // 0x1340: registration confirmed
const ID_LEN: usize = 16;
const KEY_LEN: usize = 8;
/// Forgotten without keep-alives.
pub const PEER_TTL_S: f64 = 120.0;

/// Ports, as offsets from the base port (11000): detect 1 and its other port, detect 2 and its other port.
pub const DETECT_PORTS: [u8; 4] = [0, 1, 2, 3];
pub const TRAVERSAL_PORT: u8 = 5;

pub fn message(kind: u8, mtype: u8, seq: u16, items: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![kind, mtype];
    out.extend_from_slice(&seq.to_be_bytes());
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

pub fn address_item(addr: SocketAddrV4) -> Vec<u8> {
    let mut out = vec![0x02, 0x04];
    out.extend_from_slice(&addr.port().to_be_bytes());
    out.extend_from_slice(&addr.ip().octets());
    out
}

pub fn parse_address(item: &[u8]) -> Option<SocketAddrV4> {
    (item.len() == 8 && item[..2] == [0x02, 0x04]).then(|| {
        SocketAddrV4::new(Ipv4Addr::new(item[4], item[5], item[6], item[7]), u16::from_be_bytes([item[2], item[3]]))
    })
}

/// -> (message, from the game socket).
pub fn unframe(data: &[u8]) -> (&[u8], bool) {
    if data.len() > TRAILER.len() && data.ends_with(&TRAILER) {
        (&data[..data.len() - TRAILER.len()], true)
    } else {
        (data, false)
    }
}

pub struct Peer {
    pub id: [u8; ID_LEN],
    pub key: [u8; KEY_LEN],
    /// The socket it registered from (pushes go there).
    pub addr: SocketAddrV4,
    /// Its game socket, once seen.
    pub game_addr: Option<SocketAddrV4>,
    seen: f64,
    /// Target id -> when its request was last logged.
    connects: HashMap<[u8; ID_LEN], f64>,
}

/// A datagram to send: (bytes, to, from which of our ports).
pub type Out = (Vec<u8>, SocketAddrV4, u8);

/// Detection and traversal state, transport-free: `received` returns what to send. Times are seconds on any
/// monotonic clock.
pub struct Storm {
    pub peers: HashMap<[u8; ID_LEN], Peer>,
    next_id: u32,
}

impl Default for Storm {
    fn default() -> Self {
        Self::new()
    }
}

impl Storm {
    pub fn new() -> Storm {
        // unlike ids games kept over a restart
        Storm { peers: HashMap::new(), next_id: 1 + rand::random::<u32>() % (1 << 30) }
    }

    fn new_id(&mut self) -> [u8; ID_LEN] {
        let mut id = [0u8; ID_LEN];
        id[..4].copy_from_slice(&[0x04, 0x03, 0x01, 0x03]);
        id[4..8].copy_from_slice(&1u32.to_le_bytes());
        id[8..12].copy_from_slice(&1u32.to_le_bytes());
        id[12..].copy_from_slice(&self.next_id.to_be_bytes());
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// A datagram on one of our ports (port: offset from the base port).
    pub fn received(&mut self, now: f64, port: u8, data: &[u8], addr: SocketAddrV4) -> Vec<Out> {
        let (msg, framed) = unframe(data);
        if msg.len() < 4 || !(msg[0] == REQUEST || msg[0] == ANSWER) {
            log::debug!(target: "powder.storm", "storm: {} bytes from {addr} on +{port} ignored", data.len());
            return vec![];
        }
        let (kind, mtype, seq) = (msg[0], msg[1], u16::from_be_bytes([msg[2], msg[3]]));
        let body = &msg[4..];
        let reply = |kind: u8, mtype: u8, parts: &[&[u8]]| -> Vec<Out> {
            let out = message(kind, mtype, seq, parts);
            let out = if framed { [HEADER.as_slice(), &out].concat() } else { out };
            vec![(out, addr, port)]
        };

        if DETECT_PORTS.contains(&port) {
            if kind == REQUEST && mtype == DETECT {
                return reply(REPLY, mtype, &[&address_item(addr)]);
            }
            return vec![];
        }
        if port != TRAVERSAL_PORT {
            return vec![];
        }
        self.peers.retain(|_, p| now - p.seen <= PEER_TTL_S);
        if kind == REQUEST && mtype == REGISTER {
            let existing = self.peers.values().find(|p| p.addr == addr).map(|p| p.id);
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = self.new_id();
                    let mut key = [0x02, 0x02, 0, 0, 0, 0, 0, 0];
                    key[4..].copy_from_slice(&rand::random::<[u8; 4]>());
                    self.peers.insert(id, Peer { id, key, addr, game_addr: None, seen: now, connects: HashMap::new() });
                    log::info!(target: "powder.storm", "storm: {} registered as peer {}", addr.ip(), hex::encode(&id[12..]));
                    id
                }
            };
            let peer = self.peers.get_mut(&id).expect("just found or inserted");
            peer.seen = now;
            // the game acks (0x1141)
            return reply(PUSH, REGISTERED, &[&peer.id, &peer.key, &address_item(addr)]);
        }
        if body.len() < ID_LEN + KEY_LEN {
            return vec![];
        }
        let peer = self.peer(now, &body[..ID_LEN], &body[ID_LEN..ID_LEN + KEY_LEN], addr, framed);
        let (id, key) = (peer.id, peer.key);
        if kind == ANSWER && mtype == REGISTERED {
            reply(REPLY, CONFIRM, &[&key, &id])
        } else if kind == REQUEST && mtype == KEEPALIVE {
            let seen = address_item(addr);
            if framed { reply(REPLY, mtype, &[&id, &key, &seen]) } else { reply(REPLY, mtype, &[&id, &key]) }
        } else if kind == REQUEST && mtype == CONNECT && body.len() >= 2 * ID_LEN + KEY_LEN {
            let target_id: [u8; ID_LEN] = body[ID_LEN + KEY_LEN..2 * ID_LEN + KEY_LEN].try_into().unwrap();
            let addresses = &body[2 * ID_LEN + KEY_LEN..];
            let Some(target_addr) = self.peers.get(&target_id).map(|t| t.addr) else {
                log::warn!(target: "powder.storm", "storm: peer {} asks for unknown peer {}", hex::encode(&id[12..]),
                           hex::encode(&target_id[12..]));
                return vec![];
            };
            let peer = self.peers.get_mut(&id).expect("registered above");
            // not every retry (about two a second)
            if now - peer.connects.get(&target_id).copied().unwrap_or(-60.0) >= 30.0 {
                peer.connects.insert(target_id, now);
                log::info!(target: "powder.storm", "storm: peer {} ({}) connects to peer {} ({})",
                           hex::encode(&id[12..]), addr.ip(), hex::encode(&target_id[12..]), target_addr.ip());
            }
            vec![(message(PUSH, CONNECT, seq, &[&id, &target_id, addresses]), target_addr, TRAVERSAL_PORT)]
        } else {
            log::debug!(target: "powder.storm", "storm: message {kind:02x}{mtype:02x} from {addr} ignored");
            vec![]
        }
    }

    /// The registered peer; one we don't know (e.g. the server restarted) is taken as it says.
    fn peer(&mut self, now: f64, id: &[u8], key: &[u8], addr: SocketAddrV4, framed: bool) -> &mut Peer {
        let id: [u8; ID_LEN] = id.try_into().unwrap();
        let peer = self.peers.entry(id).or_insert_with(|| {
            log::info!(target: "powder.storm", "storm: {} continues as peer {}", addr.ip(), hex::encode(&id[12..]));
            Peer { id, key: key.try_into().unwrap(), addr, game_addr: None, seen: now, connects: HashMap::new() }
        });
        if framed {
            peer.game_addr = Some(addr);
        } else {
            peer.addr = addr;
        }
        peer.seen = now;
        peer
    }
}

/// Bind the detection ports and the traversal port (base + 0..3, + 5) and serve them until the process ends.
/// Returns once all are bound, so a port in use is reported at startup.
pub async fn start(bind: &str, base_port: u16) -> Result<Arc<Mutex<Storm>>> {
    let storm = Arc::new(Mutex::new(Storm::new()));
    let mut sockets: HashMap<u8, Arc<UdpSocket>> = HashMap::new();
    for offset in DETECT_PORTS.iter().copied().chain([TRAVERSAL_PORT]) {
        let addr = format!("{bind}:{}", base_port + offset as u16);
        let sock = UdpSocket::bind(&addr).await.with_context(|| format!("Storm on udp {addr}"))?;
        sockets.insert(offset, Arc::new(sock));
    }
    let sockets = Arc::new(sockets);
    let started = Instant::now();
    for (&offset, sock) in sockets.iter() {
        let (sock, sockets, storm) = (sock.clone(), sockets.clone(), storm.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let (n, from) = match sock.recv_from(&mut buf).await {
                    Ok(r) => r,
                    Err(e) => {
                        log::debug!(target: "powder.storm", "storm: {e}"); // e.g. ICMP port unreachable
                        continue;
                    }
                };
                let SocketAddr::V4(from) = from else { continue };
                let now = started.elapsed().as_secs_f64();
                let out = storm.lock().received(now, offset, &buf[..n], from);
                for (data, to, via) in out {
                    if let Some(s) = sockets.get(&via) {
                        let _ = s.send_to(&data, to).await;
                    }
                }
            }
        });
    }
    Ok(storm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddrV4 {
        s.parse().unwrap()
    }
    fn h(s: &str) -> Vec<u8> {
        hex::decode(s.replace(' ', "")).unwrap()
    }
    const GAME_A: &str = "192.0.2.10:49627";
    const GAME_A_SOCKET: &str = "192.0.2.10:49625";
    const GAME_B: &str = "192.0.2.20:50001";

    fn register(s: &mut Storm, addr: SocketAddrV4) -> (Vec<u8>, Vec<u8>) {
        let out = s.received(0.0, TRAVERSAL_PORT, &h("1040000101010300"), addr);
        let (data, to, port) = &out[0];
        assert_eq!((*to, *port, &data[..4]), (addr, TRAVERSAL_PORT, &h("12410001")[..]));
        assert_eq!(data[28..], address_item(addr));
        (data[4..20].to_vec(), data[20..28].to_vec())
    }

    #[test]
    fn detection_tells_the_address_it_sees() {
        let mut s = Storm::new();
        let out = s.received(0.0, 0, &h("10010000"), a("192.0.2.10:54210"));
        assert_eq!(out, vec![(h("13010000 0204 d3c2 c000020a"), a("192.0.2.10:54210"), 0)]);
        // from the game socket: the request has a trailer, the answer the peer-packet header
        let out = s.received(0.0, 3, &[h("1001000b"), TRAILER.to_vec()].concat(), a(GAME_A_SOCKET));
        let want = [HEADER.to_vec(), h("1301000b"), address_item(a(GAME_A_SOCKET))].concat();
        assert_eq!(out, vec![(want, a(GAME_A_SOCKET), 3)]);
        // other port / server: unanswered, as from Ubisoft's (game behind NAT)
        assert!(s.received(0.0, 0, &h("10020007"), a("192.0.2.10:54210")).is_empty());
        assert!(s.received(0.0, 0, &h("10030002"), a("192.0.2.10:54210")).is_empty());
    }

    #[test]
    fn registration_and_keepalives() {
        let mut s = Storm::new();
        let (id, key) = register(&mut s, a(GAME_A));
        assert_eq!((&id[..4], &key[..4]), (&h("04030103")[..], &h("02020000")[..]));
        let out = s.received(0.0, TRAVERSAL_PORT, &[h("11410002"), id.clone(), key.clone()].concat(), a(GAME_A));
        assert_eq!(out[0].0, [h("13400002"), key.clone(), id.clone()].concat());
        let out = s.received(0.0, TRAVERSAL_PORT, &[h("10430003"), id.clone(), key.clone()].concat(), a(GAME_A));
        assert_eq!(out[0].0, [h("13430003"), id.clone(), key.clone()].concat());
        let framed = [h("10430001"), id.clone(), key.clone(), TRAILER.to_vec()].concat();
        let out = s.received(0.0, TRAVERSAL_PORT, &framed, a(GAME_A_SOCKET));
        let want = [HEADER.to_vec(), h("13430001"), id.clone(), key.clone(), address_item(a(GAME_A_SOCKET))].concat();
        assert_eq!(out, vec![(want, a(GAME_A_SOCKET), TRAVERSAL_PORT)]);
        let p = &s.peers[&<[u8; 16]>::try_from(id.as_slice()).unwrap()];
        assert_eq!((p.addr, p.game_addr), (a(GAME_A), Some(a(GAME_A_SOCKET))));
        assert_eq!(register(&mut s, a(GAME_A)).0, id); // the same socket again: same peer
    }

    #[test]
    fn connect_request_is_passed_to_the_target() {
        let mut s = Storm::new();
        let (a_id, a_key) = register(&mut s, a(GAME_A));
        let (b_id, _) = register(&mut s, a(GAME_B));
        let addresses = [address_item(a(GAME_A_SOCKET)), address_item(a("10.0.0.5:49625")), h("0206007000000000")]
            .concat();
        let req = [h("10440005"), a_id.clone(), a_key.clone(), b_id.clone(), addresses.clone()].concat();
        let out = s.received(0.0, TRAVERSAL_PORT, &req, a(GAME_A));
        assert_eq!(out, vec![([h("12440005"), a_id.clone(), b_id, addresses.clone()].concat(), a(GAME_B), TRAVERSAL_PORT)]);
        let req = [h("10440006"), a_id, a_key, vec![0; 16], addresses].concat();
        assert!(s.received(0.0, TRAVERSAL_PORT, &req, a(GAME_A)).is_empty()); // an unknown target: nothing
    }

    #[test]
    fn a_peer_from_before_a_restart_is_taken_as_it_says() {
        let mut s = Storm::new();
        let (old_id, old_key) = ([h("04030103"), vec![0; 12]].concat(), h("0202000011223344"));
        let out = s.received(0.0, TRAVERSAL_PORT, &[h("10430009"), old_id.clone(), old_key.clone()].concat(), a(GAME_B));
        assert_eq!(out[0].0, [h("13430009"), old_id.clone(), old_key].concat());
        assert_eq!(s.peers[&<[u8; 16]>::try_from(old_id.as_slice()).unwrap()].addr, a(GAME_B));
    }

    #[test]
    fn peers_without_keepalives_are_forgotten() {
        let mut s = Storm::new();
        s.received(0.0, TRAVERSAL_PORT, &h("1040000101010300"), a(GAME_A));
        s.received(PEER_TTL_S + 1.0, TRAVERSAL_PORT, &h("1040000101010300"), a(GAME_B));
        assert_eq!(s.peers.values().map(|p| p.addr).collect::<Vec<_>>(), vec![a(GAME_B)]);
    }

    #[test]
    fn frames_and_address_items_as_recorded() {
        assert_eq!(hex::encode(TRAILER), format!("08071b{}80000000200000", "00".repeat(22)));
        assert_eq!(hex::encode(HEADER), format!("{}8000000020", "00".repeat(22)));
        assert_eq!(parse_address(&address_item(a("192.0.2.7:11000"))), Some(a("192.0.2.7:11000")));
        assert_eq!(parse_address(&h("0206007000000000")), None);
    }

    #[tokio::test]
    async fn detection_over_udp() {
        let base = 21000 + (std::process::id() % 2000) as u16 * 6;
        start("127.0.0.1", base).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(me) = client.local_addr().unwrap() else { unreachable!() };
        client.send_to(&h("10010007"), ("127.0.0.1", base + 2)).await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut buf)).await.unwrap().unwrap();
        assert_eq!(buf[..n], [h("13010007"), address_item(me)].concat());
    }
}
