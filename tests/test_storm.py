"""Storm (peer-to-peer helpers): NAT detection and traversal, and the configuration that points the game at
them. Message layouts as recorded from Ubisoft's servers; addresses and ids here are made up."""
import asyncio
import dataclasses
import json
import socket
from types import SimpleNamespace

from aiohttp.test_utils import TestClient, TestServer

import synthetic as syn
from powder.server.constants import APP_ID
from powder.server.transport import storm
from powder.server.transport.https.app import build_app, storm_settings, with_storm

GAME_A, GAME_A_SOCKET = ("192.0.2.10", 49627), ("192.0.2.10", 49625)
GAME_B = ("192.0.2.20", 50001)


def make():
    sent = []
    s = storm.Storm(lambda data, addr, port: sent.append((data, addr, port)), clock=lambda: 0.0)
    return s, sent


def register(s, sent, addr):
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("1040000101010300"), addr)
    data, to, port = sent[-1]
    assert to == addr and port == storm.TRAVERSAL_PORT and data[:4] == bytes.fromhex("12410001")
    peer_id, key = data[4:20], data[20:28]
    assert data[28:] == storm.address_item(addr)
    return peer_id, key


def test_detection_tells_the_address_it_sees():
    s, sent = make()
    s.received(0, bytes.fromhex("10010000"), ("192.0.2.10", 54210))
    assert sent[-1] == (bytes.fromhex("13010000 0204 d3c2 c000020a".replace(" ", "")), ("192.0.2.10", 54210), 0)
    # from the game socket: the request has a trailer, the answer the peer-packet header
    s.received(3, bytes.fromhex("1001000b") + storm.TRAILER, GAME_A_SOCKET)
    assert sent[-1] == (storm.HEADER + bytes.fromhex("1301000b") + storm.address_item(GAME_A_SOCKET), GAME_A_SOCKET, 3)
    n = len(sent)
    s.received(0, bytes.fromhex("10020007"), ("192.0.2.10", 54210))      # other port / server: unanswered,
    s.received(0, bytes.fromhex("10030002"), ("192.0.2.10", 54210))      # as from Ubisoft's (game behind NAT)
    assert len(sent) == n


def test_registration_and_keepalives():
    s, sent = make()
    peer_id, key = register(s, sent, GAME_A)
    assert peer_id[:4] == bytes.fromhex("04030103") and key[:4] == bytes.fromhex("02020000")
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("11410002") + peer_id + key, GAME_A)
    assert sent[-1][0] == bytes.fromhex("13400002") + key + peer_id
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10430003") + peer_id + key, GAME_A)
    assert sent[-1][0] == bytes.fromhex("13430003") + peer_id + key
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10430001") + peer_id + key + storm.TRAILER, GAME_A_SOCKET)
    assert sent[-1] == (storm.HEADER + bytes.fromhex("13430001") + peer_id + key + storm.address_item(GAME_A_SOCKET),
                        GAME_A_SOCKET, storm.TRAVERSAL_PORT)
    assert s.peers[peer_id].addr == GAME_A and s.peers[peer_id].game_addr == GAME_A_SOCKET
    assert register(s, sent, GAME_A)[0] == peer_id                         # the same socket again: same peer


def test_connect_request_is_passed_to_the_target():
    s, sent = make()
    a_id, a_key = register(s, sent, GAME_A)
    b_id, _ = register(s, sent, GAME_B)
    addresses = storm.address_item(GAME_A_SOCKET) + storm.address_item(("10.0.0.5", 49625)) + \
        bytes.fromhex("0206007000000000")
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10440005") + a_id + a_key + b_id + addresses, GAME_A)
    assert sent[-1] == (bytes.fromhex("12440005") + a_id + b_id + addresses, GAME_B, storm.TRAVERSAL_PORT)
    n = len(sent)
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10440006") + a_id + a_key + bytes(16) + addresses, GAME_A)
    assert len(sent) == n                                                   # an unknown target: nothing


def test_a_peer_from_before_a_restart_is_taken_as_it_says():
    s, sent = make()
    old_id, old_key = bytes.fromhex("04030103") + bytes(12), bytes.fromhex("0202000011223344")
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10430009") + old_id + old_key, GAME_B)
    assert sent[-1][0] == bytes.fromhex("13430009") + old_id + old_key and s.peers[old_id].addr == GAME_B


def test_peers_without_keepalives_are_forgotten():
    now = [0.0]
    s = storm.Storm(clock=lambda: now[0])
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("1040000101010300"), GAME_A)
    now[0] = storm.PEER_TTL_S + 1
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("1040000101010300"), GAME_B)
    assert [p.addr for p in s.peers.values()] == [GAME_B]


def register_confirmed(s, sent, addr):
    """REGISTER, then the game's 0x1141 with the key."""
    peer_id, key = register(s, sent, addr)
    n = len(sent)
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("11410002") + peer_id + key, addr)
    assert len(sent) == n + 1 and s.peers[peer_id].confirmed
    return peer_id, key


def test_a_known_peer_needs_its_key():
    s, sent = make()
    a_id, a_key = register_confirmed(s, sent, GAME_A)
    b_id, b_key = register(s, sent, GAME_B)
    attacker = ("198.51.100.9", 4000)
    wrong = a_key[:4] + bytes(4)
    n = len(sent)
    # a keep-alive in A's name: no answer, A's address stays (also from a game socket)
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10430003") + a_id + wrong, attacker)
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10430001") + a_id + wrong + storm.TRAILER, attacker)
    # a connect request in A's name is not passed on to B
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10440005") + a_id + wrong + b_id + storm.address_item(attacker),
               attacker)
    assert len(sent) == n
    assert (s.peers[a_id].addr, s.peers[a_id].game_addr) == (GAME_A, None)
    # with the key, A may move (its NAT gave it a new port): a REGISTER from there finds it
    moved = ("192.0.2.10", 50000)
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("10430003") + a_id + a_key, moved)
    assert len(sent) == n + 1 and s.peers[a_id].addr == moved
    assert register(s, sent, moved)[0] == a_id
    assert register(s, sent, GAME_A)[0] != a_id, "its old socket is no longer it"
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("11410002") + b_id + b_key, GAME_B)   # B's own key still works
    assert sent[-1][0] == bytes.fromhex("13400002") + b_key + b_id


def test_peers_are_capped_and_confirmed_ones_kept():
    now = [0.0]
    sent = []
    s = storm.Storm(lambda data, addr, port: sent.append((data, addr, port)), clock=lambda: now[0])
    a_id, _ = register_confirmed(s, sent, GAME_A)
    # spoofed REGISTERs from A's IP, each from another port: the oldest unconfirmed make room, A stays
    for port in range(200):
        now[0] = port * 0.1
        s.received(storm.TRAVERSAL_PORT, bytes.fromhex("1040000101010300"), ("192.0.2.10", 1000 + port))
    assert s.per_ip["192.0.2.10"] == storm.MAX_PEERS_PER_IP and a_id in s.peers
    assert len(s.by_addr) == len(s.peers)
    # unconfirmed peers go after UNCONFIRMED_TTL_S, confirmed ones after PEER_TTL_S
    now[0] = 20.0 + storm.UNCONFIRMED_TTL_S + 1
    s.received(0, bytes.fromhex("10010000"), GAME_B)
    assert list(s.peers) == [a_id] and (len(s.by_addr), len(s.per_ip)) == (1, 1)


def test_peers_are_capped_in_all(monkeypatch):
    monkeypatch.setattr(storm, "MAX_PEERS", 3)
    s, sent = make()
    a_id, _ = register_confirmed(s, sent, GAME_A)
    b_id, _ = register_confirmed(s, sent, GAME_B)
    register(s, sent, ("192.0.2.30", 1))
    register(s, sent, ("192.0.2.31", 1))                                  # the unconfirmed one made room
    assert len(s.peers) == 3 and {a_id, b_id} <= set(s.peers)
    assert ("192.0.2.30", 1) not in s.by_addr
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("11410002") + s.by_addr[("192.0.2.31", 1)] +
               s.peers[s.by_addr[("192.0.2.31", 1)]].key, ("192.0.2.31", 1))
    n = len(sent)
    s.received(storm.TRAVERSAL_PORT, bytes.fromhex("1040000101010300"), ("192.0.2.32", 1))
    assert len(sent) == n and len(s.peers) == 3, "all confirmed: no answer"


def test_what_we_send_to_one_ip_is_rate_limited():
    now = [0.0]
    sent = []
    s = storm.Storm(lambda data, addr, port: sent.append((data, addr, port)), clock=lambda: now[0])
    victim = ("203.0.113.5", 3074)

    def count(n):
        before = len(sent)
        for _ in range(n):
            s.received(0, bytes.fromhex("10010000"), victim)
        return len(sent) - before
    assert count(100) == storm.REPLY_BURST
    now[0] = 1.0
    assert count(100) == storm.REPLIES_PER_S
    s.received(0, bytes.fromhex("10010000"), GAME_A)                      # another IP has its own budget
    assert sent[-1][1] == GAME_A


def test_a_connect_request_passes_on_a_bounded_address_list():
    s, sent = make()
    a_id, a_key = register(s, sent, GAME_A)
    b_id, _ = register(s, sent, GAME_B)
    req = lambda n: bytes.fromhex("10440005") + a_id + a_key + b_id + bytes([2]) * n
    n = len(sent)
    s.received(storm.TRAVERSAL_PORT, req(storm.MAX_CONNECT_ADDRESSES), GAME_A)
    assert len(sent) == n + 1 and sent[-1][1] == GAME_B
    s.received(storm.TRAVERSAL_PORT, req(storm.MAX_CONNECT_ADDRESSES + 1), GAME_A)
    assert len(sent) == n + 1


def test_game_socket_frames_as_recorded():
    assert storm.TRAILER.hex() == "08071b" + "00" * 22 + "80000000200000" and len(storm.TRAILER) == 32
    assert storm.HEADER.hex() == "00" * 22 + "8000000020" and len(storm.HEADER) == 27


def test_address_items():
    assert storm.parse_address(storm.address_item(("192.0.2.7", 11000))) == ("192.0.2.7", 11000)
    assert storm.parse_address(bytes.fromhex("0206007000000000")) is None


def test_configuration_names_this_server():
    settings = storm_settings(dataclasses.replace(syn_server(), public_host="192.0.2.10"))
    assert settings["detectUrl1"] == "192.0.2.10:11000" and settings["detectUrl2"] == "192.0.2.10:11002"
    assert settings["traversalUrl"] == "192.0.2.10:11005"
    recorded = {"configuration": {"storm": {"detectUrl1": "detect.example.invalid:11000"},
                                  "punch": {"traversalUrl": "traversal.example.invalid:11005"}, "other": 1}}
    cfg = json.loads(with_storm(json.dumps(recorded).encode(), settings))["configuration"]
    assert cfg["storm"] == cfg["punch"] == settings and cfg["other"] == 1
    off = json.loads(with_storm(json.dumps(recorded).encode(), None))["configuration"]
    assert off == {"storm": {}, "other": 1}
    assert with_storm(b"not json", settings) == b"not json"


def syn_server():
    from powder.server.config import ServerConfig
    return ServerConfig()


async def test_the_packs_configuration_gets_powders_storm(services, monkeypatch):
    """The pack's recorded configuration names Ubisoft's hosts; the game gets this server instead."""
    services.config = dataclasses.replace(services.config, server=dataclasses.replace(
        services.config.server, public_host="192.0.2.10"))
    recorded = {"configuration": {"storm": {"traversalUrl": "lb-traversal.ubisoft.com:11005"},
                                  "punch": {"traversalUrl": "lb-traversal.ubisoft.com:11005"}}}
    fixture = SimpleNamespace(body=json.dumps(recorded).encode(), status=200, content_type="application/json")
    monkeypatch.setattr(services.assets.http, "lookup", lambda method, host, path: (fixture, "exact"))
    client = TestClient(TestServer(build_app(services, nex_public_key="TESTKEY")))
    await client.start_server()
    try:
        r = await client.get(f"/v2/applications/{APP_ID}/configuration", headers={"Host": syn.UBI})
        cfg = (await r.json())["configuration"]
        assert cfg["storm"]["traversalUrl"] == "192.0.2.10:11005"
        assert "ubisoft.com" not in json.dumps(cfg.get("storm")) + json.dumps(cfg.get("punch"))
    finally:
        await client.close()


async def test_storm_over_udp():
    _, transports = await _start_free()
    loop = asyncio.get_running_loop()
    try:
        port = transports[0].get_extra_info("sockname")[1]
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setblocking(False)
        sock.bind(("127.0.0.1", 0))
        await loop.sock_sendto(sock, bytes.fromhex("10010000"), ("127.0.0.1", port))
        data = await asyncio.wait_for(loop.sock_recv(sock, 64), 2)
        assert data[:4] == bytes.fromhex("13010000") and storm.parse_address(data[4:]) == sock.getsockname()
        sock.close()
    finally:
        for t in transports:
            t.close()


async def _start_free():
    """storm.start on a free base port (all six ports must be free)."""
    for base in range(41000, 42000, 10):
        try:
            return await storm.start("127.0.0.1", base)
        except OSError:
            continue
    raise RuntimeError("no free ports")
