"""One TLS listener (port 443) for everything, routed by the hostname the client asked for (SNI).

Ubisoft Connect's demux (dmx.upc.ubisoft.com) is not HTTP, everything else is. Routing by name lets every
Ubisoft hostname point at one address, which a hosts file, a DNS server or a remote/docker server can all
provide. After the handshake the connection is handed to:
  demux    -> transport/demux (StreamReader/StreamWriter around the TLS transport)
  anything -> aiohttp's request handler (the HTTPS app), as if aiohttp had accepted it itself
The peer address is kept, so per-IP player binding still works.

Before the handshake, https/limits.py's connection limits: a TCP connection over them (in all, or from one
address) is closed right after it is accepted, and the handshake must finish within `Limits.tls_handshake`.
That is why the listener accepts plain TCP and starts TLS itself (loop.start_tls): asyncio's ssl= listener
creates the protocol before the peer is known.
"""
from __future__ import annotations

import asyncio
import logging
import ssl
from pathlib import Path
from typing import Awaitable, Callable

from aiohttp import web

from ..hostnames import DEMUX_HOST
from .https.limits import ConnectionGate, Limits

log = logging.getLogger("powder.tls")

StreamHandler = Callable[[asyncio.StreamReader, asyncio.StreamWriter], Awaitable[None]]


def tls_context(cert_file: Path) -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    ctx.load_cert_chain(str(cert_file))

    def remember_sni(ssl_object, server_name, _ctx):
        # server-side SSLObject.server_hostname is always None: keep what the client asked for
        ssl_object.steep_sni = (server_name or "").lower()
        # logged before the handshake completes: a hello without a matching "tls ok" = the client gave up
        # no SNI = a client connecting by address (the launcher's status checks): only at debug level
        (log.info if ssl_object.steep_sni else log.debug)("tls hello for %s", ssl_object.steep_sni or "(no SNI)")
    ctx.sni_callback = remember_sni
    return ctx


class _Pending(asyncio.Protocol):
    """A connection between accept and its handover: the TLS handshake runs, what arrives after it (a request
    sent right behind the handshake) is kept for the protocol that takes the connection over."""

    def __init__(self, admitted: Callable[[asyncio.Transport], None]):
        self.admitted = admitted
        self.received: list[bytes] = []
        self.eof = self.lost = False

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.admitted(transport)

    def data_received(self, data: bytes) -> None:
        self.received.append(data)

    def eof_received(self) -> bool:
        self.eof = True
        return False

    def connection_lost(self, exc: BaseException | None) -> None:
        self.lost = True

    def hand_over(self, transport: asyncio.Transport, proto: asyncio.Protocol) -> None:
        transport.set_protocol(proto)
        proto.connection_made(transport)
        for data in self.received:
            proto.data_received(data)
        if self.eof and not proto.eof_received():
            transport.close()


class _Router:
    def __init__(self, http_server: web.Server, demux: StreamHandler, ctx: ssl.SSLContext, gate: ConnectionGate):
        self.http_server, self.demux, self.ctx, self.gate = http_server, demux, ctx, gate

    def protocol(self) -> _Pending:
        pending = _Pending(lambda transport: self.accepted(transport, pending))
        return pending

    def accepted(self, tcp: asyncio.Transport, pending: _Pending) -> None:
        peer = tcp.get_extra_info("peername") or ("?", 0)
        if not self.gate.admit(peer[0], tcp):
            tcp.abort()
            return
        tcp.pause_reading()                             # the handshake's protocol reads (start_tls resumes)
        asyncio.ensure_future(self.handshake(tcp, pending, peer))

    async def handshake(self, tcp: asyncio.Transport, pending: _Pending, peer) -> None:
        loop = asyncio.get_running_loop()
        try:
            transport = await loop.start_tls(tcp, pending, self.ctx, server_side=True,
                                             ssl_handshake_timeout=self.gate.limits.tls_handshake)
        except (OSError, ssl.SSLError, TimeoutError, ConnectionError) as e:
            log.debug("handshake with %s:%s failed: %s", peer[0], peer[1], e or type(e).__name__)
            tcp.abort()
            return
        if transport is None or pending.lost:
            tcp.abort()
            return
        sni = getattr(transport.get_extra_info("ssl_object"), "steep_sni", "")
        (log.info if sni else log.debug)("tls ok %s:%s for %s", peer[0], peer[1], sni or "(no SNI)")
        if sni == DEMUX_HOST:
            reader = asyncio.StreamReader()
            proto = asyncio.StreamReaderProtocol(reader)
            pending.hand_over(transport, proto)
            writer = asyncio.StreamWriter(transport, proto, reader, loop)
            asyncio.ensure_future(self.demux(reader, writer))
        else:
            pending.hand_over(transport, self.http_server())


async def start(runner: web.AppRunner, demux: StreamHandler, host: str, port: int,
                cert_file: Path, limits: Limits | None = None) -> asyncio.AbstractServer:
    """`runner` must be set up (AppRunner.setup()) but not started on a site of its own."""
    http_server = runner.server
    if http_server is None:
        raise RuntimeError("AppRunner.setup() first")
    router = _Router(http_server, demux, tls_context(cert_file), ConnectionGate(limits or Limits()))
    return await asyncio.get_running_loop().create_server(router.protocol, host, port)


async def start_plain(runner: web.AppRunner, host: str, port: int,
                      limits: Limits | None = None) -> asyncio.AbstractServer:
    """Plain HTTP for a set-up `runner` (the CRL), with its own connection limits."""
    http_server, gate = runner.server, ConnectionGate(limits or Limits())
    if http_server is None:
        raise RuntimeError("AppRunner.setup() first")

    def protocol() -> _Pending:
        def admitted(tcp: asyncio.Transport) -> None:
            peer = tcp.get_extra_info("peername") or ("?", 0)
            if gate.admit(peer[0], tcp):
                pending.hand_over(tcp, http_server())
            else:
                tcp.abort()
        pending = _Pending(admitted)
        return pending
    return await asyncio.get_running_loop().create_server(protocol, host, port)
