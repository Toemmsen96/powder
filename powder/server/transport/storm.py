"""Storm: Steep's peer-to-peer helpers, NAT detection and traversal (UDP), so the games find each other.

The game's configuration names them ("storm"/"punch": detectUrl1, detectUrl2, traversalUrl; see
https/app.py with_storm). Without them the game switches its multiplayer off. Layout from recordings
of Ubisoft's servers:

  message   u8 kind (0x10 request, 0x11 the game's answer, 0x12 the server's own, 0x13 the server's answer),
            u8 type, u16 sequence (big endian; an answer repeats the request's), then items:
  peer id   16 bytes, handed out by the traversal server ("04 03 01 03", u32 le, u32 le, u32 be counter)
  key       8 bytes "02 02 00 00" + 4 random bytes, handed out with the id
  address   "02 04" + u16 port + IPv4 (big endian); "02 06 ..." (IPv6, empty) passed on as it is

The game talks from small sockets of their own, and from its game socket (the one the other games
reach). From the game socket a message carries a fixed 32-byte trailer, and answers to it start with a
fixed 27-byte header (the frame of the game's peer packets, all zero for these).

Detection (detectUrl1, detectUrl2 and each one's port + 1): 0x1001 "which address do you see?" ->
0x1301 with it. 0x1002 / 0x1003 ask for an answer from the other port / other server; Ubisoft's never
arrived (the PC was behind NAT) and the game then worked, so they stay unanswered here as well.

Traversal (traversalUrl):
  0x1040 register        -> 0x1241 peer id, key, address seen    then 0x1141 (id, key) -> 0x1340 (key, id)
  0x1043 keep-alive      -> 0x1343 id, key (+ address seen, from the game socket); every ~8 s
  0x1044 "connect me"    (id, key, target id, the requester's addresses) -> 0x1244 to the target (requester id,
                         target id, the addresses): both then punch (0x1045/0x1145 directly between the games)
The relay ("routerProvisioningUrl") was never asked for.

Bounded: a message naming a known peer must carry its key (else it is ignored, so nobody can move another
game's address or speak for it); peers are capped in all and per IP, unconfirmed ones (no answer with the key
yet) expire sooner and make room first; we send at most REPLIES_PER_S datagrams a second to one IP, whatever
a spoofed source asks for (REGISTER's answer is ~5x its request); forgotten peers are swept every SWEEP_S and
found by address through an index.
"""
from __future__ import annotations

import asyncio
import itertools
import logging
import secrets
import socket
import struct
import time
from dataclasses import dataclass, field

log = logging.getLogger("powder.storm")

HEADER = bytes(22) + bytes.fromhex("8000000020")                 # before answers to the game socket
TRAILER = bytes.fromhex("08071b") + bytes(22) + bytes.fromhex("80000000200000")   # after its requests
REQUEST, ANSWER, PUSH, REPLY = 0x10, 0x11, 0x12, 0x13
DETECT, DETECT_OTHER_PORT, DETECT_OTHER_SERVER = 0x01, 0x02, 0x03
REGISTER, REGISTERED, KEEPALIVE, CONNECT = 0x40, 0x41, 0x43, 0x44
CONFIRM = 0x40                                                    # 0x1340: registration confirmed
ID_LEN, KEY_LEN = 16, 8
PEER_TTL_S = 120.0                  # forgotten without keep-alives (the game sends one every ~8 s)
UNCONFIRMED_TTL_S = 30.0            # the same for a peer that has not answered with its key
MAX_PEERS = 4096                    # in all and per IP (a game registers from one socket or two; a LAN party
MAX_PEERS_PER_IP = 32               # shares an IP); when full the oldest unconfirmed peer makes room
REPLY_BURST = 40.0                  # datagrams we send to one IP: a burst, then REPLIES_PER_S (the game's own:
REPLIES_PER_S = 20.0                # a keep-alive every ~8 s per socket, ~2 connect requests a second joining)
MAX_CONNECT_ADDRESSES = 128         # bytes of addresses a connect request may pass on (the game sends 24)
SWEEP_S = 1.0                       # how often forgotten peers are swept

# ports, from the base port (11000): detect 1 and its other port, detect 2 and its other port, traversal
DETECT_PORTS = (0, 1, 2, 3)
TRAVERSAL_PORT = 5

Address = tuple[str, int]


def message(kind: int, mtype: int, seq: int, *items: bytes) -> bytes:
    return struct.pack(">BBH", kind, mtype, seq) + b"".join(items)


def address_item(addr: Address) -> bytes:
    return b"\x02\x04" + struct.pack(">H", addr[1]) + socket.inet_aton(addr[0])


def parse_address(item: bytes) -> Address | None:
    if len(item) == 8 and item[:2] == b"\x02\x04":
        return socket.inet_ntoa(item[4:8]), struct.unpack(">H", item[2:4])[0]
    return None


def unframe(data: bytes) -> tuple[bytes, bool]:
    """-> (message, from the game socket)."""
    if len(data) > len(TRAILER) and data.endswith(TRAILER):
        return data[:-len(TRAILER)], True
    return data, False


@dataclass
class Peer:
    id: bytes
    key: bytes
    addr: Address                                   # the socket it registered from (pushes go there)
    game_addr: Address | None = None                # its game socket, once seen
    seen: float = field(default_factory=time.monotonic)
    connects: dict[bytes, float] = field(default_factory=dict)  # target id -> when its request was last logged
    issued: bool = False                            # we handed out its id (REGISTER) ...
    confirmed: bool = False                         # ... and it answered with the key (a spoofed REGISTER never
                                                    # sees the key); unconfirmed peers expire sooner


class Storm:
    """Detection and traversal state; transport-free (send(data, addr, port) is wired by start())."""

    def __init__(self, send=None, clock=time.monotonic):
        self.send = send or (lambda data, addr, port: None)
        self.clock = clock
        self.peers: dict[bytes, Peer] = {}
        self.by_addr: dict[Address, bytes] = {}                         # registration socket -> peer id
        self.per_ip: dict[str, int] = {}                                # peers per registration IP
        self.budgets: dict[str, tuple[float, float]] = {}               # destination IP -> (left, when counted)
        self._last_sweep = float("-inf")
        self._ids = itertools.count(1 + secrets.randbelow(1 << 30))    # unlike ids games kept over a restart

    def new_id(self) -> bytes:
        return bytes.fromhex("04030103") + struct.pack("<II", 1, 1) + struct.pack(">I", next(self._ids))

    def received(self, port: int, data: bytes, addr: Address) -> None:
        """A datagram on one of our ports (port: offset from the base port)."""
        now = self.clock()
        self._sweep(now)
        for out, to, to_port in self._handle(now, port, data, addr):
            if self._take_budget(to[0], now):
                self.send(out, to, to_port)

    def _handle(self, now: float, port: int, data: bytes, addr: Address) -> list[tuple[bytes, Address, int]]:
        msg, framed = unframe(data)
        if len(msg) < 4 or msg[0] not in (REQUEST, ANSWER):
            log.debug("storm: %d bytes from %s on +%d ignored", len(data), addr, port)
            return []
        kind, mtype, seq = struct.unpack(">BBH", msg[:4])
        body = msg[4:]

        def reply(*parts: bytes, kind_: int = REPLY, type_: int = mtype) -> list[tuple[bytes, Address, int]]:
            out = message(kind_, type_, seq, *parts)
            return [(HEADER + out if framed else out, addr, port)]

        if port in DETECT_PORTS:
            return reply(address_item(addr)) if kind == REQUEST and mtype == DETECT else []
        if port != TRAVERSAL_PORT:
            return []
        if kind == REQUEST and mtype == REGISTER:
            peer = self.peers.get(self.by_addr.get(addr, b""))
            if peer is None:
                if not self._make_room(addr[0]):
                    return []
                peer = Peer(self.new_id(), b"\x02\x02\x00\x00" + secrets.token_bytes(4), addr, seen=now, issued=True)
                self._insert(peer)
                log.debug("storm: %s registers as peer %s", addr[0], peer.id[-4:].hex())
            peer.seen = now
            return reply(peer.id, peer.key, address_item(addr), kind_=PUSH, type_=REGISTERED)   # acked by 0x1141
        if len(body) < ID_LEN + KEY_LEN:
            return []
        peer = self._peer(now, body[:ID_LEN], body[ID_LEN:ID_LEN + KEY_LEN], addr, framed)
        if peer is None:
            return []
        if kind == ANSWER and mtype == REGISTERED:
            return reply(peer.key, peer.id, type_=CONFIRM)
        if kind == REQUEST and mtype == KEEPALIVE:
            return reply(peer.id, peer.key, *((address_item(addr),) if framed else ()))
        if kind == REQUEST and mtype == CONNECT and len(body) >= 2 * ID_LEN + KEY_LEN:
            target_id, addresses = body[ID_LEN + KEY_LEN:2 * ID_LEN + KEY_LEN], body[2 * ID_LEN + KEY_LEN:]
            if len(addresses) > MAX_CONNECT_ADDRESSES:
                log.debug("storm: connect request with %d bytes of addresses ignored", len(addresses))
                return []
            target = self.peers.get(target_id)
            if target is None:
                log.warning("storm: peer %s asks for unknown peer %s", peer.id[-4:].hex(), target_id[-4:].hex())
                return []
            if now - peer.connects.get(target_id, -60.0) >= 30:    # not every retry (about two a second)
                peer.connects[target_id] = now
                log.info("storm: peer %s (%s) connects to peer %s (%s)", peer.id[-4:].hex(), addr[0],
                         target_id[-4:].hex(), target.addr[0])
            return [(message(PUSH, CONNECT, seq, peer.id, target_id, addresses), target.addr, TRAVERSAL_PORT)]
        log.debug("storm: message %02x%02x from %s ignored", kind, mtype, addr)
        return []

    def _peer(self, now: float, peer_id: bytes, key: bytes, addr: Address, framed: bool) -> Peer | None:
        """Note a message from a peer: a known one must give its key; one we don't know (e.g. the server
        restarted) is taken as it says. None: ignore the message."""
        peer = self.peers.get(peer_id)
        if peer is not None and peer.key != key:
            log.debug("storm: %s names peer %s with a wrong key, ignored", addr, peer_id[-4:].hex())
            return None
        if peer is None:
            if not self._make_room(addr[0]):
                return None
            peer = Peer(peer_id, key, addr, seen=now)
            self._insert(peer)
            log.info("storm: %s continues as peer %s", addr[0], peer_id[-4:].hex())
        peer.seen = now
        if peer.issued and not peer.confirmed:
            peer.confirmed = True
            log.info("storm: %s registered as peer %s", addr[0], peer_id[-4:].hex())
        if framed:
            peer.game_addr = addr
        elif peer.addr != addr:
            self._unindex(peer)
            peer.addr = addr
            self._insert(peer)
        return peer

    # ---- bookkeeping: the address index, caps, the send budget ---------------------------------------
    def _insert(self, peer: Peer) -> None:
        old = self.peers.get(peer.id)
        if old is not None and old is not peer:
            self._unindex(old)
        self.peers[peer.id] = peer
        self.by_addr[peer.addr] = peer.id
        self.per_ip[peer.addr[0]] = self.per_ip.get(peer.addr[0], 0) + 1

    def _unindex(self, peer: Peer) -> None:
        if self.by_addr.get(peer.addr) == peer.id:
            del self.by_addr[peer.addr]
        n = self.per_ip.get(peer.addr[0], 0) - 1
        if n > 0:
            self.per_ip[peer.addr[0]] = n
        else:
            self.per_ip.pop(peer.addr[0], None)

    def _remove(self, peer_id: bytes) -> None:
        peer = self.peers.pop(peer_id, None)
        if peer is not None:
            self._unindex(peer)

    def _make_room(self, ip: str) -> bool:
        """Room for one more peer from `ip`: under the caps, or after forgetting the oldest unconfirmed peer
        (of that IP when it is the IP that is full)."""
        ip_full = self.per_ip.get(ip, 0) >= MAX_PEERS_PER_IP
        if not ip_full and len(self.peers) < MAX_PEERS:
            return True
        candidates = [p for p in self.peers.values() if not p.confirmed and (not ip_full or p.addr[0] == ip)]
        if not candidates:
            log.warning("storm: no room for a peer from %s (%d peers, at most %d, %d per IP)", ip, len(self.peers),
                        MAX_PEERS, MAX_PEERS_PER_IP)
            return False
        self._remove(min(candidates, key=lambda p: p.seen).id)
        return True

    def _sweep(self, now: float) -> None:
        if now - self._last_sweep < SWEEP_S:
            return
        self._last_sweep = now
        for p in [p for p in self.peers.values() if now - p.seen > (PEER_TTL_S if p.confirmed else UNCONFIRMED_TTL_S)]:
            self._remove(p.id)
        for ip in [ip for ip, (left, at) in self.budgets.items()        # a full budget needs no entry
                   if left + max(0.0, now - at) * REPLIES_PER_S >= REPLY_BURST]:
            del self.budgets[ip]

    def _take_budget(self, ip: str, now: float) -> bool:
        """One datagram to `ip`, if its budget allows."""
        left, at = self.budgets.get(ip, (REPLY_BURST, now))
        left = min(REPLY_BURST, left + max(0.0, now - at) * REPLIES_PER_S)
        ok = left >= 1.0
        self.budgets[ip] = (left - 1.0 if ok else left, now)
        if not ok:
            log.debug("storm: datagram to %s dropped (over %d/s)", ip, REPLIES_PER_S)
        return ok


class _Port(asyncio.DatagramProtocol):
    def __init__(self, storm: Storm, offset: int):
        self.storm, self.offset = storm, offset
        self.transport: asyncio.DatagramTransport | None = None

    def connection_made(self, transport) -> None:
        self.transport = transport

    def datagram_received(self, data: bytes, addr) -> None:
        try:
            self.storm.received(self.offset, data, (addr[0], addr[1]))
        except Exception:                                               # noqa: BLE001 - keep serving
            log.exception("storm: datagram from %s failed", addr)

    def error_received(self, exc) -> None:                              # e.g. ICMP port unreachable
        log.debug("storm: %s", exc)


async def start(bind: str, base_port: int) -> tuple[Storm, list[asyncio.DatagramTransport]]:
    loop = asyncio.get_running_loop()
    transports: dict[int, asyncio.DatagramTransport] = {}

    def send(data: bytes, addr: Address, offset: int) -> None:
        t = transports.get(offset)
        if t is not None:
            t.sendto(data, addr)
    storm = Storm(send)
    try:
        for offset in (*DETECT_PORTS, TRAVERSAL_PORT):
            transport, _ = await loop.create_datagram_endpoint(lambda o=offset: _Port(storm, o),
                                                               local_addr=(bind, base_port + offset))
            transports[offset] = transport
    except OSError:
        for t in transports.values():
            t.close()
        raise
    return storm, list(transports.values())
