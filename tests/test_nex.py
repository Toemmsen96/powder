"""NEX transport: reliability details and a full session over real UDP with a simulated game client."""
import asyncio
import ipaddress
import os
import uuid

import pytest
from cryptography.hazmat.primitives.asymmetric import ec

from powder.server.core.identity import LoginRequest
from powder.server.services.stats import decode_stat_group
from powder.server.transport.nex import rmc_codec as rmc
from powder.server.transport.nex import secure
from powder.server.transport.nex import server as nex_server
from powder.server.transport.nex.connection import MAX_MESSAGE_BYTES, REORDER_WINDOW, PrudpConnection, seq_before
from powder.server.transport.nex.dispatch import RmcDispatcher
from powder.server.transport.nex.prudp import (CONNECT, DATA, DISCONNECT, FLAG_ACK, FLAG_KEY_EXCHANGE,
                                             FLAG_NEED_ACK, FLAG_RELIABLE, PING, SYN, Packet, checksum)


def test_seq_before_is_wrap_safe():
    assert seq_before(1, 2) and not seq_before(2, 1)
    assert seq_before(65535, 0) and seq_before(65530, 5) and not seq_before(5, 65530)


def test_in_order_delivery_across_sequence_wrap():
    sent = []
    c = PrudpConnection(("127.0.0.1", 1), lambda raw, addr: sent.append(raw), expected_seq=65534)
    pkt = lambda seq, data: Packet(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id=seq, payload=data)
    assert c.receive_data(pkt(65534, b"a")) == [b"a"]
    assert c.receive_data(pkt(0, b"c")) == []                 # early: buffered until 65535 arrives
    assert c.receive_data(pkt(65535, b"b")) == [b"b", b"c"]
    assert c.receive_data(pkt(65535, b"b")) == []             # duplicate after wrap
    assert c.receive_data(pkt(1, b"d")) == [b"d"]
    assert len(sent) == 5                                     # all 5 NEED_ACK packets acked (dup too)


class GameClient(asyncio.DatagramProtocol):
    """Speaks PRUDP + the secure handshake like the game does."""

    def __init__(self):
        self.q: asyncio.Queue = asyncio.Queue()
        self.sess, self.my_sig, self.seq = 0xF7, os.urandom(4), 2

    def datagram_received(self, data, addr):
        self.q.put_nowait(Packet.decode(data))

    async def recv(self, want) -> Packet:
        while True:
            p = await asyncio.wait_for(self.q.get(), 3)
            if want(p):
                return p

    def send(self, p: Packet):
        self.transport.sendto(p.encode())

    def connection_made(self, transport):
        self.transport = transport

    async def connect(self, cfg_pub):
        self.send(Packet(type=SYN, flags=FLAG_NEED_ACK))
        self.srv_sig = (await self.recv(lambda p: p.type == SYN)).connection_signature
        eph = ec.generate_private_key(ec.SECP256R1())
        self.send(Packet(type=CONNECT, flags=FLAG_RELIABLE | FLAG_NEED_ACK | FLAG_KEY_EXCHANGE,
                         session_id=self.sess, signature=self.srv_sig, sequence_id=1,
                         connection_signature=self.my_sig, key_blob=secure.to_blob(eph.public_key(), secure.ECK1)))
        ack = await self.recv(lambda p: p.type == CONNECT)
        assert ack.type_flags == 0x89 and ack.signature == self.my_sig
        assert secure.verify_blob(cfg_pub, ack.key_blob, ack.key_signature), "game would reject the signature"
        self.key = secure.session_key(eph, ack.key_blob)

    def call(self, protocol, call_id, method, params=b""):
        req = rmc.build_request(protocol, call_id, method, params)
        chunks = [req[i:i + 730] for i in range(0, len(req), 730)]
        for i, chunk in enumerate(chunks):
            self.send(Packet(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, session_id=self.sess,
                             signature=self.srv_sig, sequence_id=self.seq,
                             fragment_id=0 if i == len(chunks) - 1 else i + 1,
                             payload=secure.encrypt_payload(self.key, self.seq, chunk)))
            self.seq += 1

    async def read(self) -> rmc.Message:
        parts = []
        while True:
            p = await self.recv(lambda p: p.type == DATA and not p.flags & FLAG_ACK)
            self.send(Packet(type=DATA, flags=FLAG_ACK, session_id=self.sess, signature=self.srv_sig,
                             sequence_id=p.sequence_id, fragment_id=p.fragment_id))
            parts.append(secure.decrypt_payload(self.key, p.payload, p.sequence_id))
            if p.fragment_id == 0:
                return rmc.parse(b"".join(parts))


@pytest.fixture
async def nex(services):
    signing = ec.generate_private_key(ec.SECP256R1())
    server, transport = await nex_server.start(services, RmcDispatcher(services), signing,
                                               "127.0.0.1", "127.0.0.1", 0)
    timers = asyncio.ensure_future(server.run_timers())
    port = transport.get_extra_info("sockname")[1]
    loop = asyncio.get_running_loop()
    ct, client = await loop.create_datagram_endpoint(GameClient, remote_addr=("127.0.0.1", port))
    await client.connect(secure.from_blob(__import__("base64").b64decode(secure.config_public_key(signing))))
    yield server, client
    timers.cancel()
    ct.close()
    transport.close()


async def test_full_session(services, player, nex):
    server, client = nex
    session = services.accounts.login(LoginRequest("127.0.0.1"))

    # LoginWithToken with our HTTPS ticket (3 KB-ish params -> several fragments, like the game)
    params = (rmc.Writer().u32(1).string("ClientVersionInfo").u16(1).string(session.ticket)
              .u16(1).u16(0).u16(0).u32(0).string("x" * 2500).bytes())
    client.call("LoginProtocol", 5, "LoginProtocol::LoginWithToken_V1", params)
    resp = await client.read()
    assert resp.success and resp.method == "LoginProtocol::LoginWithToken_V1*"
    out = rmc.Reader(resp.body)
    assert out.guid() == uuid.UUID(player)
    url = out.string()
    assert url.startswith("prudp:/address=127.0.0.1;") and out.bool() is False
    # the game drops every server notification unless the station URL carries a non-zero DID
    did = dict(kv.split("=", 1) for kv in url.split(":/", 1)[1].split(";"))["DID"]
    assert int(did) > 0
    assert session.nex is not None                           # connection registered for server push

    # player statistics come from the database: level 41
    client.call("PlayerStats2Protocol", 6, "PlayerStats2Protocol::GetStatisticGroup_V1",
                rmc.Writer().u32(1).string("StatisticsResult").u16(1).string("player_profile").bytes())
    _, rows = decode_stat_group((await client.read()).body)
    assert next(r for r in rows if r.key == "level_stat:6:infinite").value[0] == 41

    # a matchmaking call (empty reply)
    client.call("MatchmakingProtocol", 7, "MatchmakingProtocol::LeaveSession_V1", rmc.Writer().u32(0).u32(1).bytes())
    assert (await client.read()).success

    # unknown call -> RMC error, connection survives
    client.call("FooProtocol", 8, "FooProtocol::Bar_V1")
    err = await client.read()
    assert not err.success and err.error == ("Core", 2) and err.call_id == 8

    # server push: a request initiated by the server reaches the client
    assert services.sessions.push_nex(player, "NotificationProtocol",
                                      "NotificationProtocol::ProcessNotificationEvent_V1", b"\x00", call_id=0)
    pushed = await client.read()
    assert pushed.is_request and pushed.call_id == 0 and pushed.method.endswith("ProcessNotificationEvent_V1")

    await asyncio.sleep(0.3)
    conn = next(iter(server.conns.values()))
    assert not conn.unacked, "every server DATA packet must have been acknowledged"


async def test_unknown_ticket_is_attributed_in_lenient_mode(services, player, nex):
    _, client = nex
    params = rmc.Writer().u32(0).string("emu.not-a-ticket-we-issued").bytes()
    client.call("LoginProtocol", 5, "LoginProtocol::LoginWithToken_V1", params)
    out = rmc.Reader((await client.read()).body)
    assert out.guid() == uuid.UUID(player)


async def test_uploaded_progress_shows_in_the_next_stat_read(services, player, nex):
    from test_progression import encode_events
    from powder.server.transport.nex.rmc_codec import V_UINT64
    _, client = nex
    session = services.accounts.login(LoginRequest("127.0.0.1"))
    client.call("LoginProtocol", 5, "LoginProtocol::LoginWithToken_V1",
                rmc.Writer().u32(0).string(session.ticket).bytes())
    await client.read()
    client.call("PlayerStats2Protocol", 6, "PlayerStats2Protocol::PostEvents_V1", encode_events(
        [("update_level", {"expertise_id": (V_UINT64, 6), "level_expertise": (V_UINT64, 50)})]))
    assert (await client.read()).body == b""
    client.call("PlayerStats2Protocol", 7, "PlayerStats2Protocol::GetStatisticGroup_V1",
                rmc.Writer().u32(1).string("StatisticsResult").u16(1).string("player_profile").bytes())
    _, rows = decode_stat_group((await client.read()).body)
    assert next(r for r in rows if r.key == "level_stat:6:infinite").value[0] == 50


# ---- hardening: bounded buffers, the signed handshake, caps and rate limits ------------------------
def test_data_far_ahead_or_undecryptable_is_dropped_unacked():
    sent, key = [], bytes([3]) * 16
    c = PrudpConnection(("127.0.0.1", 1), lambda raw, addr: sent.append(raw), expected_seq=10, key=key)
    enc = lambda seq, data: Packet(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id=seq,
                                   payload=secure.encrypt_payload(key, seq, data))
    # the window's last id is buffered and acked; one past it is neither (the game sends it again)
    assert c.receive_data(enc(10 + REORDER_WINDOW - 1, b"z")) == []
    assert c.receive_data(enc(10 + REORDER_WINDOW, b"far")) == []
    assert len(sent) == 1 and len(c.pending) == 1
    # garbage in the window: not acked, not buffered; the real packet with that id is then taken
    garbage = Packet(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id=10, payload=bytes(48))
    assert c.receive_data(garbage) == [] and (len(sent), len(c.pending)) == (1, 1)
    assert c.receive_data(enc(10, b"a")) == [b"a"] and len(sent) == 2


def test_buffered_bytes_are_counted_back_down():
    c = PrudpConnection(("127.0.0.1", 1), lambda raw, addr: None)
    pkt = lambda seq, data, frag=0: Packet(type=DATA, sequence_id=seq, fragment_id=frag, payload=data)
    half = bytes(MAX_MESSAGE_BYTES // 2)
    for rnd in range(3):
        assert c.receive_data(pkt(rnd * 2, half, frag=1)) == []
        assert len(c.receive_data(pkt(rnd * 2 + 1, half))) == 1
        assert not c.closed and c.buffered == 0, rnd
    assert c.receive_data(pkt(6, bytes(MAX_MESSAGE_BYTES), frag=1)) == [] and not c.closed
    assert c.receive_data(pkt(7, b"x")) == [] and c.closed


class FakeTransport:
    def __init__(self):
        self.sent: list[tuple[bytes, tuple]] = []

    def sendto(self, raw, addr):
        self.sent.append((raw, addr))

    def last(self) -> Packet:
        return Packet.decode(self.sent[-1][0])


def echo(ctx, r):
    ctx.conn.tag = "served"
    return rmc.Writer().string("hi " + r.string()).bytes()


def make_server(services, check="enforce", plugins=None):
    d = RmcDispatcher(services, plugins, handlers={"TestProtocol::Echo_V1": echo})
    srv = nex_server.NexServer(services, d, ec.generate_private_key(ec.SECP256R1()), "127.0.0.1", 6005, check)
    t = FakeTransport()
    srv.connection_made(t)
    return srv, t


SYN_PACKET = Packet(type=SYN, flags=FLAG_NEED_ACK)
GAME = ("192.0.2.50", 3074)


def connect_packet(server_sig: bytes, blob: bytes) -> Packet:
    return Packet(type=CONNECT, flags=FLAG_NEED_ACK | FLAG_RELIABLE | FLAG_KEY_EXCHANGE, sequence_id=1,
                  signature=server_sig, connection_signature=bytes([1, 2, 3, 4]), key_blob=blob)


def plain_connect(server_sig: bytes) -> Packet:
    p = connect_packet(server_sig, b"")
    p.flags &= ~FLAG_KEY_EXCHANGE
    return p


def client_key():
    eph = ec.generate_private_key(ec.SECP256R1())
    return secure.to_blob(eph.public_key(), secure.ECK1), eph


def handshake(srv, t, game, now=0.0) -> tuple[bytes, bytes]:
    """The game's side: SYN, CONNECT signed with the SYN-ACK's signature -> (signature, key)."""
    srv.received(SYN_PACKET.encode(), game, now)
    sig = t.last().connection_signature
    blob, eph = client_key()
    srv.received(connect_packet(sig, blob).encode(), game, now)
    ack = t.last()
    assert ack.type == CONNECT and ack.flags & FLAG_KEY_EXCHANGE and ack.key_blob
    return sig, secure.session_key(eph, ack.key_blob)


def echo_call(sig, key, seq, call_id) -> Packet:
    call = rmc.build_request("TestProtocol", call_id, "TestProtocol::Echo_V1", rmc.Writer().string("game").bytes())
    return Packet(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id=seq, signature=sig,
                  payload=secure.encrypt_payload(key, seq, call))


def ping(sig) -> bytes:
    return Packet(type=PING, flags=FLAG_NEED_ACK, signature=sig).encode()


async def settle():
    for _ in range(5):
        await asyncio.sleep(0)


async def test_handshake_then_an_encrypted_call(services):
    srv, t = make_server(services)
    srv.received(SYN_PACKET.encode(), GAME, 0.0)
    syn_ack = t.last()
    assert syn_ack.type == SYN and syn_ack.flags & FLAG_ACK and len(t.sent[-1][0]) == 18   # no larger than the SYN
    srv.received(SYN_PACKET.encode(), GAME, 0.05)
    assert t.last() == syn_ack, "a repeated SYN gets the same answer"
    assert not srv.conns and GAME in srv.syns
    sig = syn_ack.connection_signature
    blob, eph = client_key()
    connect = connect_packet(sig, blob)
    srv.received(connect.encode(), GAME, 0.1)
    ack_raw = t.sent[-1][0]
    key = secure.session_key(eph, Packet.decode(ack_raw).key_blob)
    assert GAME in srv.conns and not srv.syns and srv.conns[GAME].pid >= 0x1001
    srv.received(connect.encode(), GAME, 0.2)                      # a repeated CONNECT gets the same answer
    assert t.sent[-1][0] == ack_raw
    srv.received(echo_call(sig, key, 2, 7).encode(), GAME, 0.3)
    await settle()
    replies = [Packet.decode(raw) for raw, _ in t.sent[-2:]]
    assert replies[0].type == DATA and replies[0].flags & FLAG_ACK                 # our ack of the call
    msg = rmc.parse(secure.decrypt_payload(key, replies[1].payload, replies[1].sequence_id))
    assert msg.call_id == 7 and rmc.Reader(msg.body).string() == "hi game"


def test_a_connect_needs_a_syn_its_signature_and_a_key_exchange(services):
    srv, t = make_server(services)
    # no SYN before it (a spoofed source never sees our SYN-ACK): no answer, no ECDH
    srv.received(connect_packet(bytes(4), client_key()[0]).encode(), GAME, 0.0)
    assert not t.sent and not srv.conns and not srv.syns
    srv.received(SYN_PACKET.encode(), GAME, 0.0)
    sig = t.last().connection_signature
    n = len(t.sent)
    wrong = bytes([sig[0] ^ 1]) + sig[1:]
    srv.received(connect_packet(wrong, client_key()[0]).encode(), GAME, 0.1)
    assert len(t.sent) == n, "a wrong signature: no answer"
    srv.received(plain_connect(sig).encode(), GAME, 0.2)          # without key exchange: a plaintext channel
    assert len(t.sent) == n and not srv.conns and not srv.syns


async def test_packets_without_our_signature_are_ignored(services):
    srv, t = make_server(services)
    sig, key = handshake(srv, t, GAME)
    n, spoofed = len(t.sent), bytes(4)
    srv.received(echo_call(spoofed, key, 2, 7).encode(), GAME, 0.1)   # a call: no ack, no reply
    srv.received(ping(spoofed), GAME, 0.1)
    srv.received(Packet(type=DISCONNECT, signature=spoofed).encode(), GAME, 0.1)   # the connection stays
    await settle()
    assert len(t.sent) == n and GAME in srv.conns
    assert srv.conns[GAME].last_seen == 0.0, "not kept alive either"
    srv.received(echo_call(sig, key, 2, 7).encode(), GAME, 0.2)       # the game's own (signed) call still works
    await settle()
    assert len(t.sent) == n + 2, "ack + reply"


async def test_signature_checks_only_log_by_default(services, caplog):
    assert services.config.server.nex_check_signatures == "log"
    srv, t = make_server(services, check=None)                     # from the config
    assert srv.check == "log"
    caplog.set_level("DEBUG", logger="powder.nex")
    sig, key = handshake(srv, t, GAME)
    srv.received(ping(sig), GAME, 0.1)
    srv.received(ping(sig), GAME, 0.1)
    matched = [r for r in caplog.records if "carry our signature" in r.message]
    assert len(matched) == 1 and matched[0].levelname == "INFO"
    # a wrong signature is taken, and logged: the first one per connection as a warning
    n = len(t.sent)
    srv.received(echo_call(bytes(4), key, 2, 7).encode(), GAME, 0.2)
    srv.received(ping(bytes(4)), GAME, 0.3)
    await settle()
    assert len(t.sent) == n + 3, "ack + reply + ping ack"
    wrong = [r for r in caplog.records if "wrong signature" in r.message]
    assert [r.levelname for r in wrong] == ["WARNING", "DEBUG"]
    assert srv.conns[GAME].last_seen == 0.3
    # a CONNECT with another signature is taken too (for the half-open connection), with a warning
    other = ("192.0.2.51", 3074)
    srv.received(SYN_PACKET.encode(), other, 1.0)
    srv.received(connect_packet(bytes(4), client_key()[0]).encode(), other, 1.0)
    assert other in srv.conns
    assert sum("wrong signature" in r.message and r.levelname == "WARNING" for r in caplog.records) == 2
    # what stays enforced: a CONNECT only after a SYN, and with a key exchange
    late, plain = ("192.0.2.52", 3074), ("192.0.2.53", 1)
    srv.received(connect_packet(bytes(4), client_key()[0]).encode(), late, 1.0)
    srv.received(SYN_PACKET.encode(), plain, 1.0)
    srv.received(plain_connect(bytes(4)).encode(), plain, 1.0)
    assert late not in srv.conns and plain not in srv.conns


def test_the_signature_check_setting(tmp_path):
    from pathlib import Path
    from powder.server.config import Config
    example = Path(__file__).parent.parent / "config.example.toml"
    assert Config.load(example).server.nex_check_signatures == "log"
    cfg = tmp_path / "config.toml"
    cfg.write_text('[server]\nnex_check_signatures = "enforce"\n')
    assert Config.load(cfg).server.nex_check_signatures == "enforce"
    cfg.write_text('[server]\nnex_check_signatures = "strict"\n')
    with pytest.raises(ValueError, match="nex_check_signatures"):
        Config.load(cfg)


def test_signature_checks_off(services, caplog):
    srv, t = make_server(services, check="off")
    caplog.set_level("DEBUG", logger="powder.nex")
    srv.received(SYN_PACKET.encode(), GAME, 0.0)
    srv.received(connect_packet(bytes(4), client_key()[0]).encode(), GAME, 0.0)
    srv.received(ping(bytes(4)), GAME, 0.1)
    assert GAME in srv.conns and t.last().type == PING
    assert not [r for r in caplog.records if "signature" in r.message]
    with pytest.raises(ValueError):
        make_server(services, check="strict")


async def test_a_new_syn_does_not_cut_off_the_live_connection(services):
    srv, t = make_server(services)
    sig, key = handshake(srv, t, GAME)
    live = srv.conns[GAME]
    srv.received(SYN_PACKET.encode(), GAME, 1.0)
    srv.received(echo_call(sig, key, 2, 7).encode(), GAME, 1.1)
    await settle()
    assert srv.conns[GAME] is live and getattr(live, "tag", None) == "served", "still served"
    srv.tick(1.0 + nex_server.HALF_OPEN_TIMEOUT_S + 0.5)          # the half-open one is forgotten after a while
    assert not srv.syns
    sig2, _ = handshake(srv, t, GAME, 20.0)                        # a game that really reconnects replaces it
    assert sig2 != sig and srv.conns[GAME] is not live and live.closed
    assert srv.conns[GAME].pid != live.pid


def test_key_exchanges_are_rate_limited_per_ip(services):
    srv, t = make_server(services)
    for port in range(int(nex_server.HANDSHAKE_BURST)):
        handshake(srv, t, ("192.0.2.50", 1000 + port))
    # over the budget: the SYN is answered, the CONNECT not (yet)
    srv.received(SYN_PACKET.encode(), ("192.0.2.50", 2000), 0.0)
    sig = t.last().connection_signature
    blob, _ = client_key()
    n = len(t.sent)
    srv.received(connect_packet(sig, blob).encode(), ("192.0.2.50", 2000), 0.0)
    assert len(t.sent) == n
    handshake(srv, t, ("192.0.2.51", 1000))                        # another IP is not affected
    # the game's CONNECT retransmit passes once the budget refilled
    srv.received(connect_packet(sig, blob).encode(), ("192.0.2.50", 2000), nex_server.HANDSHAKE_REFILL_S)
    assert t.last().type == CONNECT and ("192.0.2.50", 2000) in srv.conns


def test_half_open_connections_are_capped(services):
    srv, _ = make_server(services)
    for port in range(100):
        srv.received(SYN_PACKET.encode(), ("192.0.2.50", port), 0.0)
    assert len(srv.syns) == nex_server.MAX_HALF_OPEN_PER_IP
    for i in range(nex_server.MAX_HALF_OPEN + 50):
        srv.received(SYN_PACKET.encode(), (str(ipaddress.IPv4Address(0x0A000000 + i)), 1), 1.0 + i * 1e-6)
    assert len(srv.syns) == nex_server.MAX_HALF_OPEN
    assert ("10.0.0.0", 1) not in srv.syns, "the oldest went first"
    for i in range(20):                                            # an IPv6 host's /64 counts as one
        srv.received(SYN_PACKET.encode(), (f"2001:db8:0:1:{i:x}::1", 1, 0, 0), 2.0)
    assert sum(1 for a in srv.syns if ":" in a[0]) == nex_server.MAX_HALF_OPEN_PER_IP
    short = SYN_PACKET.encode()[:10]
    srv.received(short + checksum(short), ("192.0.2.99", 1), 2.0)   # shorter than our answer: not answered
    assert ("192.0.2.99", 1) not in srv.syns


def test_connections_are_capped_per_ip(services, monkeypatch):
    monkeypatch.setattr(nex_server, "MAX_CONNECTIONS_PER_IP", 2)
    srv, t = make_server(services)
    handshake(srv, t, ("192.0.2.50", 1))
    handshake(srv, t, ("192.0.2.50", 2))
    srv.received(SYN_PACKET.encode(), ("192.0.2.50", 3), 0.0)
    n = len(t.sent)
    srv.received(connect_packet(t.last().connection_signature, client_key()[0]).encode(), ("192.0.2.50", 3), 0.0)
    assert len(t.sent) == n and len(srv.conns) == 2 and not srv.syns
    handshake(srv, t, ("192.0.2.51", 1))
    assert len(srv.conns) == 3


def test_an_oversized_message_closes_the_connection(services):
    srv, t = make_server(services)
    sig, key = handshake(srv, t, GAME)
    chunk = bytes(60_000)
    for seq in range(2, 2 + MAX_MESSAGE_BYTES // len(chunk) + 1):
        assert GAME in srv.conns
        srv.received(Packet(type=DATA, flags=FLAG_RELIABLE | FLAG_NEED_ACK, sequence_id=seq, fragment_id=1,
                            signature=sig, payload=secure.encrypt_payload(key, seq, chunk)).encode(), GAME, 0.1)
    assert GAME not in srv.conns


def test_a_failing_timer_step_does_not_stop_the_timers(services):
    srv, t = make_server(services)
    handshake(srv, t, GAME)
    handshake(srv, t, ("192.0.2.51", 1))
    srv.conns[GAME].tick = lambda now: 1 / 0
    srv.tick(1.0)
    assert list(srv.conns) == [("192.0.2.51", 1)]


CUT_CONNECT = Packet(type=CONNECT, flags=FLAG_KEY_EXCHANGE).encode()[:14]   # no key blob length
CUT_DATA = Packet(type=DATA).encode()[:10]                                  # no fragment id
GARBAGE = (b"", b"\x00", b"\xff" * 13, b"\x3f" * 600, CUT_CONNECT + checksum(CUT_CONNECT),
           CUT_DATA + checksum(CUT_DATA))


def test_undecodable_datagrams_are_dropped(services):
    srv, t = make_server(services)
    handshake(srv, t, GAME)
    n = len(t.sent)
    for garbage in GARBAGE:
        srv.received(garbage, GAME, 0.1)                           # raises nothing (struct.error, IndexError)
    assert len(t.sent) == n and GAME in srv.conns


async def test_garbage_and_failing_hooks_do_not_stop_nex(services, caplog):
    class Plugins:
        async def run_before_rmc(self, method, ctx, params):
            if method == "TestProtocol::Fail_V1":
                raise RuntimeError("a bug in a mod")
            return None
    signing = ec.generate_private_key(ec.SECP256R1())
    dispatcher = RmcDispatcher(services, Plugins(), handlers={"TestProtocol::Echo_V1": echo})
    srv, transport = await nex_server.start(services, dispatcher, signing, "127.0.0.1", "127.0.0.1", 0)
    port = transport.get_extra_info("sockname")[1]
    loop = asyncio.get_running_loop()
    ct, client = await loop.create_datagram_endpoint(GameClient, remote_addr=("127.0.0.1", port))
    try:
        for garbage in GARBAGE:
            client.transport.sendto(garbage)
        await client.connect(signing.public_key())
        client.call("TestProtocol", 1, "TestProtocol::Fail_V1")
        client.call("TestProtocol", 2, "TestProtocol::Echo_V1", rmc.Writer().string("game").bytes())
        msg = await client.read()
        assert msg.call_id == 2 and rmc.Reader(msg.body).string() == "hi game"
        assert any("error handling a message; NEX carries on" in r.message for r in caplog.records)
    finally:
        ct.close()
        transport.close()
