"""What one client may use of the TLS listener: connections (in all and per address), time to finish the TLS
handshake and to send a request's headers, and request bodies (per request, by path, and all of them in memory
at once). Generous for real use (a few players, Ubisoft Connect's many parallel connections, MB-sized cloud saves
and photos), small enough that nobody can make the server run out of memory or connections without signing in.
The same limits as the Rust server's (https/limits.rs); before, aiohttp's 64 MiB body limit for every request
was all there was.

A request over its body limit gets a 413, one that does not send its body in time a 408 (UbiServices' error
shape, `Connection: close`: app.py's limits_middleware); a connection over a connection limit is closed right
after it is accepted (tls_router.py).
"""
from __future__ import annotations

import asyncio
import logging
import re
import time
from dataclasses import dataclass

log = logging.getLogger("powder.tls")

# Request bodies are read in full before the handler runs. Most calls send small JSON (the game's telemetry
# batches and Connect's remotelog lines are KBs, entities and GraphQL queries less): at most this much.
MAX_BODY = 2 << 20
# Uploaded files: photos with their thumbnails (UGC create carries them inline, base64), ghosts and trails
# (entity files), cloud saves. 64 MiB is what the server allowed for every request before.
MAX_UPLOAD = 64 << 20
# Connect's chat channels (create one, post a message): a channel or a message is a few hundred bytes.
MAX_CHANNEL_BODY = 16 << 10
UPLOAD_PREFIXES = ("/_powder/entity-files/", "/_powder/ugc/", "/_powder/cloudsaves/")
UPLOAD_PATHS = ("/ugc/v1", "/ugc/v1/")
CHANNEL_PATH = re.compile(r"/v1/spaces/[^/]+/channels(/[^/]+/messages)?")
SMALL, UPLOAD = "small", "upload"
KIB = 1024                                      # the budgets count KiB
REFUSED_LOG_EVERY_S = 10                        # refused connections are logged at most this often


def body_rules(path: str) -> tuple[int, str]:
    """The body limit for a request to this path, and the budget (and timeout) it counts against."""
    if path.startswith(UPLOAD_PREFIXES) or path in UPLOAD_PATHS:
        return MAX_UPLOAD, UPLOAD
    if CHANNEL_PATH.fullmatch(path):
        return MAX_CHANNEL_BODY, SMALL
    return MAX_BODY, SMALL


@dataclass
class Limits:
    # Open connections in all (HTTP, websockets, the demux, handshakes in progress). Ubisoft Connect alone keeps
    # a few dozen open (its embedded browser, the demux, the websocket); a few players fit many times over.
    max_connections: int = 512
    max_per_ip: int = 128                       # from one address (one PC, or a household behind one NAT address)
    tls_handshake: float = 20.0                 # from accepting the TCP connection to the end of the handshake
    # To send a request's headers, and how long an idle keep-alive connection stays open: aiohttp's keep-alive
    # timeout, which in recent aiohttp (3.14, as locked) also runs from the start of a connection and while headers
    # trickle in (older ones start it only after the first reply).
    header_read: float = 75.0
    body: float = 60.0                          # to send a small body once its headers are in (2 MiB at 64 KB/s)
    upload: float = 600.0                       # ... and an upload (64 MiB at about 1 Mbit/s)
    # Request bodies in memory at once (bytes), uploads and everything else apart (so uploads cannot hold up the
    # small calls). A request whose body does not fit waits for room (within its body timeout).
    small_budget: int = 64 << 20
    upload_budget: int = 256 << 20

    def body_timeout(self, kind: str) -> float:
        return self.upload if kind == UPLOAD else self.body


class BodyTooLarge(Exception):
    pass


class Budget:
    """KiB of request bodies in memory at once. acquire waits for room; release gives it back (sync)."""

    def __init__(self, nbytes: int):
        self.free = nbytes // KIB
        self._waiters: list[asyncio.Future] = []

    async def acquire(self, kib: int) -> None:
        while self.free < kib:
            waiter = asyncio.get_running_loop().create_future()
            self._waiters.append(waiter)
            try:
                await waiter
            finally:
                self._waiters.remove(waiter)
        self.free -= kib

    def release(self, kib: int) -> None:
        self.free += kib
        for waiter in self._waiters:
            if not waiter.done():
                waiter.set_result(None)


async def read_body(content, length: int | None, limit: int, budget: Budget) -> tuple[bytes, int]:
    """A request body in full (`content`: aiohttp's StreamReader), at most `limit` bytes, counted against `budget`
    as it arrives (so holding budget takes sending the bytes); -> the bytes and the KiB they hold (the caller
    releases them). Over the limit: BodyTooLarge, from the Content-Length before anything is read."""
    if length is not None and length > limit:
        raise BodyTooLarge
    buf, held = bytearray(), 0
    try:
        async for chunk in content.iter_any():
            size = len(buf) + len(chunk)
            if size > limit:
                raise BodyTooLarge
            need = -(-size // KIB)
            if need > held:
                await budget.acquire(need - held)
                held = need
            buf += chunk
    except BaseException:
        budget.release(held)
        raise
    return bytes(buf), held


class ConnectionGate:
    """Who is connected to one listener: at most `max_connections` in all and `max_per_ip` per address. A
    connection counts from its accept until its TCP transport closes (a websocket's or the demux's too); closed
    ones are found (transport.is_closing()) when the next connection arrives, so nothing needs to tell the gate."""

    def __init__(self, limits: Limits):
        self.limits = limits
        self.open: dict[str, list] = {}
        self._last_refusal_log = float("-inf")

    def _prune(self) -> None:
        for ip in list(self.open):
            alive = [t for t in self.open[ip] if not t.is_closing()]
            if alive:
                self.open[ip] = alive
            else:
                del self.open[ip]

    def open_from(self, ip: str) -> int:
        self._prune()
        return len(self.open.get(ip, ()))

    def admit(self, ip: str, transport) -> bool:
        """A new connection from `ip`, if the limits allow it (the caller closes it otherwise)."""
        self._prune()
        if sum(map(len, self.open.values())) >= self.limits.max_connections:
            self._refused(ip, f"{self.limits.max_connections} connections open")
            return False
        if len(self.open.get(ip, ())) >= self.limits.max_per_ip:
            self._refused(ip, f"{self.limits.max_per_ip} connections open from there")
            return False
        self.open.setdefault(ip, []).append(transport)
        return True

    def _refused(self, ip: str, why: str) -> None:
        now = time.monotonic()
        if now >= self._last_refusal_log + REFUSED_LOG_EVERY_S:
            self._last_refusal_log = now
            log.warning("connection from %s refused: %s (further refusals in the next %d s are not logged)",
                        ip, why, REFUSED_LOG_EVERY_S)
