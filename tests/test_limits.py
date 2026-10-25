"""What one client may use of the server: body sizes by path (413), body timeouts (408), the bodies' memory budget,
connections in all and per address, TLS handshake and header timeouts, kept uploads (as the Rust server)."""
import asyncio
import json
import ssl
import time

import pytest
from aiohttp import web

from powder.server import tls
from powder.server.transport import tls_router
from powder.server.transport.demux import server as demux_server
from powder.server.transport.https import app as https_app
from powder.server.transport.https import limits
from powder.server.transport.https.app import build_app
from powder.server.transport.https.limits import (MAX_BODY, MAX_CHANNEL_BODY, MAX_UPLOAD, SMALL, UPLOAD, Budget,
                                                  BodyTooLarge, ConnectionGate, Limits, body_rules, read_body)

HOST = "public-ubiservices.ubi.com"


def test_body_limits_by_path():
    for path in ("/v3/profiles/me/events", "/v1/profiles/me/remotelog", "/v2/profiles/entities", "/ugc/v1/x/like",
                 "/v1/profiles/x/entities", "/_powder/account/login", "/v1/spaces/s/channels/c"):
        assert body_rules(path) == (MAX_BODY, SMALL), path
    for path in ("/_powder/entity-files/abc", "/_powder/ugc/abc_thumbnail", "/_powder/cloudsaves/3280/1",
                 "/ugc/v1", "/ugc/v1/"):
        assert body_rules(path) == (MAX_UPLOAD, UPLOAD), path
    for path in ("/v1/spaces/abc/channels", "/v1/spaces/abc/channels/c-1/messages"):
        assert body_rules(path)[0] == MAX_CHANNEL_BODY, path


class _Chunks:
    """Stands in for aiohttp's StreamReader."""
    def __init__(self, chunks):
        self.chunks = chunks

    async def iter_any(self):
        for c in self.chunks:
            yield bytes(c)


async def test_bodies_within_their_limit_and_the_budget():
    budget = Budget(8 << 10)                                    # 8 KiB
    body, held = await read_body(_Chunks([b"\1" * 3000]), 3000, 4000, budget)
    assert (len(body), budget.free) == (3000, 5)
    budget.release(held)
    assert budget.free == 8
    # Content-Length over the limit: refused at once; no length (chunked): once the bytes pass it
    with pytest.raises(BodyTooLarge):
        await read_body(_Chunks([]), 4001, 4000, budget)
    with pytest.raises(BodyTooLarge):
        await read_body(_Chunks([b"\0" * 3000, b"\0" * 1001]), None, 4000, budget)
    assert budget.free == 8, "a refused body gives its budget back"
    body, held = await read_body(_Chunks([b"\0"] * 2000), None, 4000, budget)
    assert (len(body), budget.free) == (2000, 6), "counted in KiB, not per chunk"
    # no room: the body waits until there is
    pending = asyncio.ensure_future(read_body(_Chunks([b"\0" * 7 * 1024]), None, 8 << 10, budget))
    await asyncio.sleep(0.05)
    assert not pending.done()
    budget.release(held)
    body, held = await asyncio.wait_for(pending, 1)
    assert len(body) == 7 * 1024 and budget.free == 1


class _Transport:
    def __init__(self):
        self.closed = False

    def is_closing(self):
        return self.closed


def test_connections_per_address_and_in_all():
    gate = ConnectionGate(Limits(max_connections=3, max_per_ip=2))
    a1, a2, a3, b1, b2, b3 = (_Transport() for _ in range(6))
    assert gate.admit("10.0.0.1", a1) and gate.admit("10.0.0.1", a2)
    assert not gate.admit("10.0.0.1", a3), "per address"
    assert gate.admit("10.0.0.2", b1)
    assert not gate.admit("10.0.0.2", b2), "in all"
    a1.closed = True
    assert gate.open_from("10.0.0.1") == 1
    assert gate.admit("10.0.0.2", b2)
    for t in (a2, b1, b2):
        t.closed = True
    assert (gate.open_from("10.0.0.1"), gate.open_from("10.0.0.2")) == (0, 0) and gate.open == {}
    assert gate.admit("10.0.0.2", b3)


@pytest.fixture
async def listener(services, tmp_path):
    """The real TLS listener with these limits: -> start(limits) -> (port, ca file)."""
    started = []

    async def start(lim: Limits):
        pki = tls.ensure_pki(tmp_path / "tls")
        runner = web.AppRunner(build_app(services, nex_public_key="TESTKEY", limits=lim),
                               keepalive_timeout=lim.header_read)
        await runner.setup()
        server = await tls_router.start(runner, demux_server.handler(services), "127.0.0.1", 0, pki.server_pem, lim)
        started.append((server, runner))
        return server.sockets[0].getsockname()[1], pki.ca_pem
    yield start
    for server, runner in started:
        server.close()
        await runner.cleanup()


async def _connect(port, ca, host=HOST):
    ctx = ssl.create_default_context(cafile=str(ca))
    return await asyncio.wait_for(asyncio.open_connection("127.0.0.1", port, ssl=ctx, server_hostname=host), 5)


async def _request(port, ca, method, path, length, sent: bytes) -> bytes:
    reader, writer = await _connect(port, ca)
    writer.write(f"{method} {path} HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: {length}\r\n"
                 f"Connection: close\r\n\r\n".encode() + sent)
    try:
        return await asyncio.wait_for(reader.read(), 10)
    except ConnectionError:
        return b""
    finally:
        writer.close()


async def test_body_limits_over_tls(listener):
    """Bodies over their path's limit get a 413 (from the Content-Length, before the body is read), uploads may be
    bigger, chat channels less; a body that does not arrive in time gets a 408."""
    port, ca = await listener(Limits(body=0.3))
    raw = await _request(port, ca, "POST", "/v1/profiles/me/remotelog", MAX_BODY + 1, b"{}")
    head, _, body = raw.partition(b"\r\n\r\n")
    assert head.startswith(b"HTTP/1.1 413") and b"connection: close" in head.lower(), raw
    assert json.loads(body) == {"errorCode": 1, "message": f"Request body too large (at most {MAX_BODY} bytes).",
                                "httpCode": 413}
    big = b"x" * (MAX_BODY + 1)                                 # an upload of that size reaches its route
    raw = await _request(port, ca, "PUT", "/_powder/entity-files/abc", len(big), big)
    assert raw.startswith(b"HTTP/1.1 ") and not raw.startswith(b"HTTP/1.1 413"), raw[:200]
    channel = b" " * (MAX_CHANNEL_BODY + 1)
    raw = await _request(port, ca, "POST", "/v1/spaces/s/channels", len(channel), channel)
    assert raw.startswith(b"HTTP/1.1 413"), raw
    raw = await _request(port, ca, "POST", "/v1/profiles/me/remotelog", 100, b"0123456789")
    assert raw.startswith(b"HTTP/1.1 408"), raw
    assert json.loads(raw.partition(b"\r\n\r\n")[2])["message"] == "Request body not received within 0 s."
    # a normal body still reaches its handler (can_read_body, read(), json() as before)
    body = json.dumps({"populations": [1]}).encode()
    raw = await _request(port, ca, "PUT", "/v1/profiles/me/populations/data", len(body), body)
    assert raw.startswith(b"HTTP/1.1 "), raw


async def test_connection_limits_over_tls(listener):
    """Per address (and the slot freed when one closes), a TLS handshake and a request's headers that take too
    long."""
    port, ca = await listener(Limits(max_per_ip=2, tls_handshake=0.3, header_read=0.3))
    first = await _connect(port, ca)
    second = await _connect(port, ca)
    with pytest.raises((ConnectionError, ssl.SSLError, asyncio.IncompleteReadError, OSError)):
        await _connect(port, ca)                                # a third one from the same address
    first[1].close()
    for _ in range(100):
        try:
            third = await _connect(port, ca)
            break
        except (ConnectionError, ssl.SSLError, OSError):
            await asyncio.sleep(0.02)
    else:
        raise AssertionError("the closed connection's slot was never freed")
    # headers not finished in time: closed (also how an idle keep-alive connection ends)
    start = time.monotonic()
    third[1].write(f"GET /v1/nothing HTTP/1.1\r\nHost: {HOST}\r\n".encode())
    out = await asyncio.wait_for(third[0].read(), 5)
    assert time.monotonic() - start < 3 and b"200" not in out
    second[1].close()
    # no TLS handshake at all: closed
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    try:
        assert await asyncio.wait_for(reader.read(16), 5) == b""
    except ConnectionError:
        pass
    writer.close()


async def test_a_request_right_behind_the_handshake_is_not_lost(listener):
    """What arrives between the handshake and the handover (a pipelined request) reaches the HTTP handler."""
    port, ca = await listener(Limits())
    for _ in range(5):
        reader, writer = await _connect(port, ca)
        writer.write(f"GET /_powder/status HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n".encode())
        assert (await asyncio.wait_for(reader.read(), 5)).startswith(b"HTTP/1.1 200")
        writer.close()


def test_kept_uploads_are_bounded(tmp_path):
    bodies = tmp_path / "bodies"
    bodies.mkdir()
    (bodies / "notes.txt").write_bytes(b"mine")
    https_app.keep_upload(tmp_path, "/v1/profiles/me/remotelog", b"x" * (https_app.KEPT_BODY_MAX + 10))
    [kept] = bodies.glob("remotelog-*")
    assert kept.stat().st_size == https_app.KEPT_BODY_MAX
    for i in range(https_app.KEPT_BODIES + 5):
        path = "/v1/profiles/me/remotelog" if i % 2 == 0 else "/v3/profiles/me/events"
        https_app.keep_upload(tmp_path, path, str(i).encode())
    names = sorted(p.name for p in bodies.iterdir())
    assert len(names) == https_app.KEPT_BODIES + 1 and "notes.txt" in names, "the newest ones and notes.txt"
    contents = {(bodies / n).read_text() for n in names if n != "notes.txt"}
    assert str(https_app.KEPT_BODIES + 4) in contents and "0" not in contents, "oldest deleted"


async def test_crl_listener_has_its_own_caps(tmp_path):
    runner, server = await https_app.start_crl(lambda: b"crl", "127.0.0.1", 0, Limits(max_per_ip=1))
    port = server.sockets[0].getsockname()[1]
    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        writer.write(f"GET {tls.CRL_PATH} HTTP/1.1\r\nHost: x\r\n\r\n".encode())
        assert (await asyncio.wait_for(reader.readuntil(b"crl"), 5)).startswith(b"HTTP/1.1 200")
        r2, w2 = await asyncio.open_connection("127.0.0.1", port)       # a second one from there: closed
        assert await asyncio.wait_for(r2.read(), 5) == b""
        w2.close()
        writer.close()
    finally:
        server.close()
        await runner.cleanup()


def test_limits_defaults_are_the_rust_servers():
    lim = Limits()
    assert (lim.max_connections, lim.max_per_ip, lim.tls_handshake, lim.header_read, lim.body, lim.upload,
            lim.small_budget, lim.upload_budget) == (512, 128, 20, 75, 60, 600, 64 << 20, 256 << 20)
    assert (MAX_BODY, MAX_UPLOAD, MAX_CHANNEL_BODY) == (2 << 20, 64 << 20, 16 << 10)
    assert limits.KIB == 1024
