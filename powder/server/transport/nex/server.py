"""NEX / Quazal RendezVous transport: PRUDP (classic V0) over UDP with the secure handshake.

Demultiplexes datagrams by client address into PrudpConnections, performs SYN / CONNECT (ECDH key
exchange signed with our ECDSA key, whose public half the HTTPS server publishes in
/rendezvous/configuration), and hands complete RMC requests to the RmcDispatcher.
See notes/nex-recon.md for the protocol.

Hardened against spoofed source addresses and floods: a SYN only makes a half-open connection (kept in
`syns`, so it can't replace a live one), answered with a SYN-ACK no larger than the SYN; a CONNECT must
follow a SYN, carry the signature its SYN-ACK gave (`[server] nex_check_signatures`, see connection.py) and
a key exchange (Steep always sends one; a plaintext channel is refused); key exchanges are rate-limited per
source IP; connections and half-open ones are capped in all and per IP (IPv6 per /64). Everything we send
reliably (and so retransmit) goes to a connection whose key exchange completed. An exception while handling
one datagram, a message or a timer step is logged and NEX carries on.
"""
from __future__ import annotations

import asyncio
import ipaddress
import itertools
import logging
import time

from cryptography.hazmat.primitives.asymmetric import ec

from . import rmc_codec, secure
from .connection import CHECK_MODES, PrudpConnection
from .dispatch import NexContext, RmcDispatcher
from .prudp import (CONNECT, DATA, DISCONNECT, FLAG_ACK, FLAG_KEY_EXCHANGE, FLAG_NEED_ACK, PING, SYN,
                    Packet)

log = logging.getLogger("powder.nex")
TICK_S = 0.25
SYN_SIZE = 18                   # header + connection signature + checksum: our SYN-ACK's size too
HALF_OPEN_TIMEOUT_S = 15.0      # SYN answered, no CONNECT yet: forgotten after this (the game CONNECTs at once)
MAX_HALF_OPEN = 256             # in all (beyond it the oldest is forgotten) ...
MAX_HALF_OPEN_PER_IP = 8        # ... and per source IP (beyond it a SYN is ignored)
MAX_CONNECTIONS = 1024          # one per game; a LAN party shares one public IP
MAX_CONNECTIONS_PER_IP = 64
HANDSHAKE_BURST = 10.0          # key exchanges (an ECDH + an ECDSA signature each) per source IP: a burst,
HANDSHAKE_REFILL_S = 6.0        # then one per HANDSHAKE_REFILL_S (a repeated CONNECT is answered from the cache)


def ip_key(addr) -> str:
    """What caps and rate limits count by: the source IP, an IPv6 host's whole /64."""
    try:
        ip = ipaddress.ip_address(addr[0].split("%", 1)[0])
    except ValueError:
        return addr[0]
    if ip.version == 6:
        if ip.ipv4_mapped is not None:
            return str(ip.ipv4_mapped)
        return str(ipaddress.IPv6Network((ip, 64), strict=False))
    return str(ip)


class NexServer(asyncio.DatagramProtocol):
    def __init__(self, services, dispatcher: RmcDispatcher, signing_key: ec.EllipticCurvePrivateKey,
                 public_host: str, port: int, check_signatures: str | None = None):
        self.services, self.dispatcher, self.signing_key = services, dispatcher, signing_key
        self.public_host, self.port = public_host, port
        if check_signatures is None:
            server_cfg = getattr(getattr(services, "config", None), "server", None)
            check_signatures = getattr(server_cfg, "nex_check_signatures", "log")
        if check_signatures not in CHECK_MODES:
            raise ValueError(f"nex_check_signatures must be one of {CHECK_MODES}, not {check_signatures!r}")
        self.check = check_signatures
        self.conns: dict[tuple, PrudpConnection] = {}       # CONNECT completed
        self.syns: dict[tuple, PrudpConnection] = {}        # half-open: SYN answered, waiting for the CONNECT
        self.handshakes: dict[str, tuple[float, float]] = {}  # ip_key -> (key exchanges left, when counted)
        self.transport: asyncio.DatagramTransport | None = None
        self._pids = itertools.count(0x1001)        # never reused, unlike the prototype's len(conns)
        self._mm_tick = 0.0
        self._tasks: set[asyncio.Task] = set()

    # ---- asyncio plumbing ------------------------------------------------------------------------
    def connection_made(self, transport):
        self.transport = transport

    def _sendto(self, raw: bytes, addr) -> None:
        if self.transport is not None:
            self.transport.sendto(raw, addr)

    def datagram_received(self, data: bytes, addr) -> None:
        try:
            self.received(data, addr, time.monotonic())
        except Exception:  # noqa: BLE001 - one bad datagram must not stop NEX
            log.exception("%s: error handling a datagram; NEX carries on", addr)

    def error_received(self, exc) -> None:                              # e.g. ICMP port unreachable
        log.debug("nex: %s", exc)

    def received(self, data: bytes, addr, now: float) -> None:
        try:
            p = Packet.decode(data)
        except Exception as e:  # noqa: BLE001 - ValueError, or a truncated field
            log.debug("%s: undecodable datagram (%s)", addr, e)
            return
        if p.type == SYN and not p.flags & FLAG_ACK:
            if len(data) >= SYN_SIZE:                   # our answer is no larger than the request
                self.on_syn(addr, p, now)
            return
        if p.type == CONNECT and not p.flags & FLAG_ACK:
            return self.on_connect(addr, p, now)
        c = self.conns.get(addr)
        if c is None:
            log.debug("%s: %s for unknown connection ignored", addr, p.type)
            return
        if not c.signed(p, self.check):
            return
        c.last_seen = now
        if p.flags & FLAG_ACK:
            c.on_ack(p)
        elif p.type == DATA:
            messages = c.receive_data(p)
            if c.closed:
                return self.drop(c, "message too large")
            for message in messages:
                self._spawn(self.on_message(c, message))
        elif p.type == PING:
            if p.flags & FLAG_NEED_ACK:
                c.send(c.reply(type=PING, flags=FLAG_ACK, sequence_id=p.sequence_id))
        elif p.type == DISCONNECT:
            for _ in range(3):
                c.send(c.reply(type=DISCONNECT, flags=FLAG_ACK, sequence_id=p.sequence_id))
            self.drop(c, "client disconnected")

    def _spawn(self, coro) -> None:
        task = asyncio.ensure_future(coro)
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)

    # ---- handshake ------------------------------------------------------------------------------
    @staticmethod
    def _syn_ack(c: PrudpConnection) -> Packet:
        return c.reply(type=SYN, flags=FLAG_ACK, session_id=0, signature=b"\x00" * 4, sequence_id=0,
                       connection_signature=c.server_sig)

    def on_syn(self, addr, p: Packet, now: float) -> None:
        """A half-open connection, kept aside: a live connection from the same address stays until the new
        one's CONNECT completes (a spoofed SYN can't cut a game off)."""
        c = self.syns.get(addr)
        if c is not None:
            return c.send(self._syn_ack(c))              # our SYN-ACK was lost: the same answer
        ip = ip_key(addr)
        if sum(1 for a in self.syns if ip_key(a) == ip) >= MAX_HALF_OPEN_PER_IP:
            log.debug("%s: SYN ignored (%d half-open from %s)", addr, MAX_HALF_OPEN_PER_IP, ip)
            return
        if len(self.syns) >= MAX_HALF_OPEN:
            del self.syns[min(self.syns.values(), key=lambda h: h.last_seen).addr]
        c = PrudpConnection(addr, self._sendto, client_stream=p.source, server_stream=p.dest, last_seen=now)
        c.send(self._syn_ack(c))
        self.syns[addr] = c
        log.debug("%s:%d SYN", addr[0], addr[1])

    def _take_handshake(self, ip: str, now: float) -> bool:
        """One key exchange from `ip`, if its budget allows."""
        left, at = self.handshakes.get(ip, (HANDSHAKE_BURST, now))
        left = min(HANDSHAKE_BURST, left + max(0.0, now - at) / HANDSHAKE_REFILL_S)
        ok = left >= 1.0
        self.handshakes[ip] = (left - 1.0 if ok else left, now)
        return ok

    def on_connect(self, addr, p: Packet, now: float) -> None:
        half, live = self.syns.get(addr), self.conns.get(addr)
        # for the half-open connection or the live one, by the signature it carries
        if half is not None and p.signature == half.server_sig:
            c = half
        elif live is not None and p.signature == live.server_sig:
            c = live
        else:
            c = half or live
        if c is None:
            log.debug("%s: CONNECT without a SYN before it ignored", addr)
            return
        if not c.signed(p, self.check):
            return
        if c is live:
            c.last_seen = now
            if c.connect_ack is not None:                 # retransmitted CONNECT -> same answer
                self._sendto(c.connect_ack, addr)
            return
        if not p.flags & FLAG_KEY_EXCHANGE or not p.key_blob:
            log.warning("%s: CONNECT without key exchange refused (Steep always sends one)", addr)
            del self.syns[addr]
            return
        ip = ip_key(addr)
        if live is None:
            from_ip = sum(1 for a in self.conns if ip_key(a) == ip)
            if len(self.conns) >= MAX_CONNECTIONS or from_ip >= MAX_CONNECTIONS_PER_IP:
                log.warning("%s: CONNECT refused (%d connections, %d from %s; at most %d, %d per IP)", addr,
                            len(self.conns), from_ip, ip, MAX_CONNECTIONS, MAX_CONNECTIONS_PER_IP)
                del self.syns[addr]
                return
        if not self._take_handshake(ip, now):
            # the half-open connection stays: the game's next CONNECT may fit the budget
            log.debug("%s: CONNECT refused for now (key exchanges from %s rate-limited)", addr, ip)
            return
        del self.syns[addr]
        try:
            blob, sig, key = secure.server_handshake(self.signing_key, p.key_blob)
        except Exception as e:  # noqa: BLE001 - not a P-256 point, a bad blob
            log.warning("%s: CONNECT with a bad key exchange (%s)", addr, e)
            return
        c.pid = next(self._pids)
        c.client_sig = p.connection_signature
        c.expected_seq = (p.sequence_id + 1) & 0xFFFF
        c.last_seen = now
        ack = c.reply(type=CONNECT, flags=FLAG_ACK | FLAG_KEY_EXCHANGE, sequence_id=p.sequence_id,
                      connection_signature=b"\x00" * 4, key_blob=blob, key_signature=sig,
                      payload=secure.encrypt_payload(key, p.sequence_id, b""))
        c.key = key
        c.connect_ack = ack.encode()
        self._sendto(c.connect_ack, addr)
        self.conns[addr] = c
        if live is not None:
            self.drop(live, "a new connection from the same address")
        log.info("%s:%d CONNECT -> connection pid 0x%x, secure channel up", addr[0], addr[1], c.pid)

    def station_url(self, c: PrudpConnection) -> str:
        """The server's station URL in the LoginWithToken reply, as Ubisoft's: CID=1 and a DID that differs
        per connection. The game keeps DID with the connection (FUN_7ff661b572df) and drops every
        server-sent notification (ProcessNotificationEvent, e.g. matchmaking) while it is 0."""
        return (f"prudp:/address={self.public_host};port={self.port};CID=1;DID={c.did};"
                f"sid=1;stream=3;type=2")

    # ---- messages -------------------------------------------------------------------------------
    async def on_message(self, c: PrudpConnection, data: bytes) -> None:
        try:
            await self._on_message(c, data)
        except Exception:  # noqa: BLE001 - e.g. a failing mod hook: the connection carries on
            log.exception("%s: error handling a message; NEX carries on", c.addr)

    async def _on_message(self, c: PrudpConnection, data: bytes) -> None:
        try:
            msg = rmc_codec.parse(data)
        except ValueError as e:
            log.warning("%s: unparseable RMC (%s): %s", c.addr, e, data[:64].hex())
            return
        if not msg.is_request:
            log.info("%s: client answered our call #%d %s", c.addr, msg.call_id, msg.method or msg.error)
            return
        ctx = NexContext(self.services, c, self.station_url(c))
        reply = await self.dispatcher.dispatch(ctx, msg)
        c.send_message(reply)

    # ---- lifecycle ------------------------------------------------------------------------------
    def drop(self, c: PrudpConnection, why: str) -> None:
        c.closed = True
        if self.conns.get(c.addr) is c:
            del self.conns[c.addr]
        s = c.session
        if s is not None:
            # a later login of the same player (e.g. Ubisoft Connect refreshing its ticket) carries this
            # connection over to its own session object: clear it there too
            gone = False
            for sess in {s, self.services.sessions.for_player(s.profile_id)} - {None}:
                if getattr(sess, "nex", None) is c:
                    sess.nex = None
                    gone = True
            if gone:
                self.services.matchmaking.on_disconnect(s.profile_id)   # the game is gone: out of its sessions
        log.info("%s:%d connection closed (%s)", c.addr[0], c.addr[1], why)

    def tick(self, now: float) -> None:
        for c in list(self.conns.values()):
            try:
                if not c.tick(now):
                    self.drop(c, "timeout")
            except Exception:  # noqa: BLE001 - must not stop the timers (nor fail again every tick)
                log.exception("%s: timer failed, connection forgotten; NEX carries on", c.addr)
                c.closed = True
                if self.conns.get(c.addr) is c:
                    del self.conns[c.addr]
        for addr in [a for a, h in self.syns.items() if now - h.last_seen > HALF_OPEN_TIMEOUT_S]:
            del self.syns[addr]
        for ip in [ip for ip, (left, at) in self.handshakes.items()   # a full budget needs no entry
                   if left + max(0.0, now - at) / HANDSHAKE_REFILL_S >= HANDSHAKE_BURST]:
            del self.handshakes[ip]
        if now - self._mm_tick >= 1.0:                 # matchmaking: ripe tickets, retries
            self._mm_tick = now
            try:
                self.services.matchmaking.tick()
            except Exception:  # noqa: BLE001 - must not stop the timers
                log.exception("matchmaking tick failed")

    async def run_timers(self) -> None:
        while True:
            await asyncio.sleep(TICK_S)
            try:
                self.tick(time.monotonic())
            except Exception:  # noqa: BLE001 - must not stop the timers
                log.exception("NEX timers failed; NEX carries on")


async def start(services, dispatcher: RmcDispatcher, signing_key, bind: str, public_host: str,
                port: int, check_signatures: str | None = None) -> tuple[NexServer, asyncio.DatagramTransport]:
    loop = asyncio.get_running_loop()
    server = NexServer(services, dispatcher, signing_key, public_host, port, check_signatures)
    transport, _ = await loop.create_datagram_endpoint(lambda: server, local_addr=(bind, port))
    return server, transport
