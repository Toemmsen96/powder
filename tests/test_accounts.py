"""Accounts: registered in the launcher (/_powder/account/*), signed in with in Ubisoft Connect (Basic,
then rm_v1), and the game following the player Connect signed in as on the same PC."""
import asyncio
import base64
import dataclasses
import logging
import threading
import time

import pytest
from aiohttp.test_utils import TestClient, TestServer

import synthetic as syn
from powder.server.config import Config
from powder.server.core.identity import LoginRequest
from powder.server.core.ratelimit import SWEEP_EVERY_S, FailureLimiter, address_key
from powder.server.services.accounts import (MAX_CONCURRENT_HASHES, AccountError, AuthError, check_password,
                                             hash_password, may_take_over, off_loop)
from powder.server.transport.https import routes_powder
from powder.server.transport.https.app import build_app

CONNECT_APP = "f68a4bb5-608a-4ff2-8123-be8ef797e0a6"
PASSWORD = "correct horse"
UBI = {"Host": syn.UBI}


def basic(email: str, password: str) -> str:
    return "Basic " + base64.b64encode(f"{email}:{password}".encode()).decode()


@pytest.fixture
async def client(services, player):            # the made-up player, imported: the default player
    c = TestClient(TestServer(build_app(services, nex_public_key="TESTKEY")))
    await c.start_server()
    yield c
    await c.close()


async def connect_login(client, auth: str):
    return await client.post("/v3/profiles/sessions", json={"rememberMe": True},
                             headers={"Ubi-AppId": CONNECT_APP, "Authorization": auth})


async def game_login(client):
    return await client.post("/v3/profiles/sessions", json={},
                             headers={**UBI, "Authorization": "uplaypc_v1 t=connect-made-ticket"})


async def register(client, email="rider@example.com", password=PASSWORD, name="Rider"):
    return await client.post("/_powder/account/register", json={"email": email, "password": password, "name": name})


def test_password_hash():
    h = hash_password(PASSWORD)
    assert PASSWORD not in h and check_password(PASSWORD, h)
    assert not check_password("wrong", h) and not check_password(PASSWORD, "garbage")
    assert hash_password(PASSWORD) != h                     # salted


def test_first_account_takes_over_the_default_player(services, player):
    assert services.accounts.claimable_player_name() == syn.PLAYER_NAME
    first = services.accounts.register("Host@Example.com", PASSWORD, "")
    assert first.profile_id == syn.PLAYER                  # keeps the imported progress
    assert services.accounts.claimable_player_name() is None
    second = services.accounts.register("guest@example.com", PASSWORD, "Guest")
    assert second.profile_id != syn.PLAYER and services.repos.players.get(second.profile_id).name == "Guest"


def test_register_refuses(services, player):
    services.accounts.register("host@example.com", PASSWORD, "")
    for email, password, name in [("HOST@example.com", PASSWORD, "Other"),     # email taken (any case)
                                  ("not-an-email", PASSWORD, "Other"),
                                  ("new@example.com", "short", "Other"),
                                  ("new@example.com", PASSWORD, syn.PLAYER_NAME.upper()),   # name taken
                                  ("new@example.com", PASSWORD, "x")]:                      # name too short
        with pytest.raises(AccountError):
            services.accounts.register(email, password, name)


def test_change_email_and_password(services, player):
    acc = services.accounts.register("host@example.com", PASSWORD, "")
    acc = services.accounts.change_email(acc, "new@example.com")
    with pytest.raises(AuthError):
        services.accounts.authenticate("host@example.com", PASSWORD)
    assert services.accounts.authenticate("NEW@example.com", PASSWORD).account_id == acc.account_id
    services.repos.remember_tickets.add("rm-ticket", acc.account_id)
    services.accounts.change_password(acc, "another password")
    assert services.repos.remember_tickets.account("rm-ticket") is None      # Connect must sign in again
    with pytest.raises(AuthError):
        services.accounts.authenticate("new@example.com", PASSWORD)
    services.accounts.authenticate("new@example.com", "another password")


async def test_launcher_account_api(client, services):
    r = await client.get("/_powder/account/new")
    assert (await r.json())["takes_over"] == syn.PLAYER_NAME
    r = await register(client, name="")
    assert r.status == 200 and (await r.json())["profile_id"] == syn.PLAYER
    r = await register(client, email="RIDER@example.com", name="Other")
    assert r.status == 400 and "already exists" in (await r.json())["error"]
    r = await client.post("/_powder/account/login", json={"email": "rider@example.com", "password": "nope"})
    assert r.status == 401
    r = await client.post("/_powder/account/name", json={"email": "rider@example.com", "password": PASSWORD,
                                                         "name": "NewName"})
    assert r.status == 200 and (await r.json())["name"] == "NewName"
    r = await client.post("/_powder/account/email", json={"email": "rider@example.com", "password": PASSWORD,
                                                          "new_email": "moved@example.com"})
    assert (await r.json())["email"] == "moved@example.com"
    r = await client.post("/_powder/account/password", json={"email": "moved@example.com", "password": PASSWORD,
                                                             "new_password": "x"})
    assert r.status == 400


async def test_connect_signs_in_with_email_and_password_then_remember_me(client, services):
    await register(client)
    r = await connect_login(client, basic("rider@example.com", "wrong password"))
    assert r.status == 401 and (await r.json())["errorCode"] == 1
    r = await connect_login(client, basic("rider@example.com", PASSWORD))
    assert r.status == 200
    body = await r.json()
    assert body["profileId"] == syn.PLAYER
    r = await connect_login(client, f"rm_v1 t={body['rememberMeTicket']}")
    assert r.status == 200
    services.sessions.by_ticket.clear()                     # a server restart: remember-me still works
    services.sessions.by_player.clear()
    assert (await connect_login(client, f"rm_v1 t={body['rememberMeTicket']}")).status == 200
    assert (await connect_login(client, "rm_v1 t=made-up")).status == 401


async def test_game_follows_connect_on_the_same_pc(client, services):
    await register(client)
    assert (await game_login(client)).status == 401          # nobody signed in to Connect here yet
    await connect_login(client, basic("rider@example.com", PASSWORD))
    r = await game_login(client)
    assert r.status == 200 and (await r.json())["profileId"] == syn.PLAYER


def test_game_login_from_two_pcs(services, player):
    a = services.accounts.register("a@example.com", PASSWORD, "")
    b = services.accounts.register("b@example.com", PASSWORD, "PlayerB")
    for email, ip in (("a@example.com", "127.0.0.1"), ("b@example.com", "192.0.2.20")):
        services.accounts.login(LoginRequest(ip, "Basic", basic(email, PASSWORD).split()[1],
                                             {"Ubi-AppId": CONNECT_APP}))
    assert services.accounts.login(LoginRequest("127.0.0.1", "uplaypc_v1", "x")).profile_id == a.profile_id
    assert services.accounts.login(LoginRequest("192.0.2.20", "uplaypc_v1", "x")).profile_id == b.profile_id
    with pytest.raises(AuthError):
        services.accounts.login(LoginRequest("192.0.2.30", "uplaypc_v1", "x"))


async def test_without_accounts_every_credential_still_works(client, services):
    """A server from before accounts: until the first registration, logins work as they did."""
    assert (await connect_login(client, basic("anyone@example.com", "anything"))).status == 200
    assert (await game_login(client)).status == 200


async def test_failed_sign_ins_are_rate_limited(client, services):
    await register(client)
    services.login_failures.max_failures = 3
    for _ in range(3):
        assert (await connect_login(client, basic("rider@example.com", "wrong password"))).status == 401
    r = await connect_login(client, basic("rider@example.com", PASSWORD))
    assert r.status == 401 and "Too many" in (await r.json())["message"]


def test_failure_limiter_window():
    now = [0.0]
    lim = FailureLimiter(max_failures=3, max_per_address=5, window_s=300, clock=lambda: now[0])
    for _ in range(3):
        assert not lim.blocked("ip", "A@x.org") and lim.attempt("ip", "A@x.org")
    assert lim.blocked("ip", "a@x.org")                     # emails count case-insensitively
    assert not lim.attempt("ip", "a@x.org")                 # a blocked try is not counted
    assert not lim.blocked("ip", "b@x.org") and lim.attempt("ip", "b@x.org") and lim.attempt("ip", "c@x.org")
    assert lim.blocked("ip", "d@x.org")                     # per address: 5 failures
    assert not lim.blocked("other", "a@x.org")
    now[0] = 301
    assert not lim.blocked("ip", "a@x.org") and lim.attempt("ip", "a@x.org")
    lim.succeeded("ip", "a@x.org")
    assert not lim.blocked("ip", "a@x.org")


def test_a_success_is_not_a_failure():
    """A sign-in that works takes its own try back: the address's count is the failures only."""
    lim = FailureLimiter(max_failures=3, max_per_address=2, clock=lambda: 0.0)
    for _ in range(5):
        assert lim.attempt("ip", "me@x.org")
        lim.succeeded("ip", "me@x.org")
    assert lim.attempt("ip", "other@x.org") and lim.attempt("ip", "other@x.org")
    assert not lim.attempt("ip", "me@x.org")                # two failures from this address


def test_ipv6_counts_per_64():
    lim = FailureLimiter(max_failures=2, max_per_address=3, clock=lambda: 0.0)
    assert lim.attempt("2001:db8:1:2::1", "a@x.org") and lim.attempt("2001:db8:1:2:ffff::9", "a@x.org")
    assert lim.blocked("2001:db8:1:2:abcd::1", "A@x.org")           # the same /64
    assert not lim.blocked("2001:db8:1:3::1", "a@x.org")             # another /64
    assert lim.attempt("::ffff:10.0.0.1", "a@x.org") and lim.attempt("10.0.0.1", "a@x.org")
    assert lim.blocked("10.0.0.1", "a@x.org")                        # IPv4-mapped is the IPv4 address
    assert address_key("2001:DB8:0:0:1::1") == "2001:db8:0:0::/64"


def test_old_entries_are_swept_and_the_map_is_capped():
    """Entries go once their failures left the window (no need to ask for them again), and there are never
    more than max_keys."""
    now = [0.0]
    lim = FailureLimiter(max_failures=3, max_per_address=1000, clock=lambda: now[0])
    for i in range(50):
        assert lim.attempt(f"10.0.0.{i}", "a@x.org")
    assert len(lim._failures) == 100
    now[0] = 301 + SWEEP_EVERY_S
    assert not lim.blocked("10.9.9.9", "b@x.org")
    assert len(lim._failures) == 0                          # swept without being asked for
    lim.max_keys = 10
    for i in range(50):
        now[0] = 1000 + i
        assert lim.attempt("10.1.1.1", f"guess{i}@x.org")
        assert len(lim._failures) <= 10
    assert ("10.1.1.1", "guess49@x.org") in lim._failures   # the newest are kept
    assert len(lim._failures[("10.1.1.1",)]) == 50          # the address's count too


async def test_concurrent_guesses_are_limited_too(client, services):
    """Guesses sent at once: each is counted before its password is hashed (off the event loop), so only
    max_failures get hashed."""
    await register(client)
    services.login_failures.max_failures = 3
    replies = await asyncio.gather(*(connect_login(client, basic("rider@example.com", "wrong password"))
                                     for _ in range(12)))
    messages = [(await r.json())["message"] for r in replies]
    assert messages.count("Invalid credentials") == 3 and sum("Too many" in m for m in messages) == 9


async def test_hashing_is_limited_to_a_few_at_once():
    """At most MAX_CONCURRENT_HASHES scrypt runs at once (16 MiB each), however many sign-ins come in; the event
    loop goes on meanwhile."""
    lock, now, peak = threading.Lock(), [0], [0]

    def work():
        with lock:
            now[0] += 1
            peak[0] = max(peak[0], now[0])
        time.sleep(0.02)
        with lock:
            now[0] -= 1
    ticks = 0

    async def ticking():
        nonlocal ticks
        while True:
            ticks += 1
            await asyncio.sleep(0.001)
    ticker = asyncio.create_task(ticking())
    await asyncio.gather(*(off_loop(work) for _ in range(12)))
    ticker.cancel()
    assert peak[0] == MAX_CONCURRENT_HASHES and now[0] == 0 and ticks > 10
    assert check_password(PASSWORD, await off_loop(hash_password, PASSWORD))


async def test_passwords_never_logged(client, services, caplog):
    caplog.set_level(logging.DEBUG)
    await register(client)
    await connect_login(client, basic("rider@example.com", PASSWORD))
    await connect_login(client, basic("rider@example.com", "wrong password"))
    await client.post("/_powder/account/password", json={"email": "rider@example.com", "password": PASSWORD,
                                                         "new_password": "brand new secret"})
    assert PASSWORD not in caplog.text and "wrong password" not in caplog.text
    assert "brand new secret" not in caplog.text


def test_resetting_a_player_keeps_its_account(services, player):
    acc = services.accounts.register("host@example.com", PASSWORD, "")
    services.reset(acc.profile_id)
    assert services.accounts.authenticate("host@example.com", PASSWORD).profile_id == acc.profile_id
    assert services.repos.players.get(acc.profile_id).name == syn.PLAYER_NAME and services.accounts.required


async def test_signing_in_to_ones_own_account_does_not_reset_the_guess_count(client, services):
    await register(client)
    await register(client, email="attacker@example.com", name="Attacker")
    services.login_failures.max_failures = 3
    for _ in range(2):
        await connect_login(client, basic("rider@example.com", "wrong password"))
        assert (await connect_login(client, basic("attacker@example.com", PASSWORD))).status == 200
    await connect_login(client, basic("rider@example.com", "wrong password"))
    r = await connect_login(client, basic("rider@example.com", PASSWORD))
    assert r.status == 401 and "Too many" in (await r.json())["message"]


async def test_empty_game_credentials_are_not_counted_as_guesses(client, services):
    await register(client)
    services.login_failures.max_failures = 2
    empty = "Basic " + base64.b64encode(b":").decode()
    for _ in range(5):                                       # the game without a Connect session
        r = await client.post("/v3/profiles/sessions", json={}, headers={**UBI, "Authorization": empty})
        assert r.status == 401
    assert (await connect_login(client, basic("rider@example.com", PASSWORD))).status == 200


def test_only_the_servers_pc_takes_over_the_default_player(services, player):
    assert services.accounts.claimable_player_name_for("192.168.1.20") is None
    assert services.accounts.claimable_player_name_for("::ffff:127.0.0.1") == syn.PLAYER_NAME
    stranger = services.accounts.register_from("s@example.org", PASSWORD, "Stranger", "192.168.1.20")
    assert stranger.profile_id != syn.PLAYER
    assert services.accounts.claimable_player_name_for("127.0.0.1") is None          # an account exists now
    assert may_take_over("::1") and may_take_over("127.0.0.2") and not may_take_over("10.0.0.1")
    assert not may_take_over("")


def test_the_host_still_takes_over_the_default_player(services, player):
    assert services.accounts.create_account("h@example.org", PASSWORD, "Host").profile_id == syn.PLAYER


async def test_only_the_servers_pc_takes_over_the_player_over_the_route(client, services, monkeypatch):
    """GET /_powder/account/new and register from another PC: a new player of its own, same reply."""
    monkeypatch.setattr(routes_powder, "client_ip", lambda request: "192.168.1.20")
    assert (await (await client.get("/_powder/account/new")).json()) == {"takes_over": None}
    r = await register(client, name="Stranger")
    assert r.status == 200 and (await r.json())["profile_id"] != syn.PLAYER
    assert services.repos.accounts.by_profile(syn.PLAYER) is None                   # the host's, untouched


async def test_ipv6_loopback_takes_over_the_player(client, services, monkeypatch):
    monkeypatch.setattr(routes_powder, "client_ip", lambda request: "::1")
    assert (await (await client.get("/_powder/account/new")).json()) == {"takes_over": syn.PLAYER_NAME}
    assert (await (await register(client, name="")).json())["profile_id"] == syn.PLAYER


def test_closed_registration(make_services, config, player):
    cfg = dataclasses.replace(config, players=dataclasses.replace(config.players, registration="closed"))
    services = make_services(cfg)
    with pytest.raises(AccountError, match="registrations"):
        services.accounts.register("ann@example.org", PASSWORD, "Ann")
    assert services.accounts.create_account("ann@example.org", PASSWORD, "Ann")       # the host still can


def test_public_servers_need_accounts(make_services, config):
    cfg = dataclasses.replace(config, server=dataclasses.replace(config.server, public=True))
    acc = make_services(cfg).accounts
    assert acc.required and acc.strict_tickets
    with pytest.raises(AuthError):
        acc.login(LoginRequest("10.0.0.2", "uplaypc_v1", "jwe"))
    assert acc.session_for_ticket("stale", "10.0.0.2") is None
    acc.create_account("ann@example.org", PASSWORD, "Ann")
    c = acc.login(LoginRequest("10.0.0.2", "Basic", basic("ann@example.org", PASSWORD).split()[1],
                               {"Ubi-AppId": CONNECT_APP}))
    # strict tickets: an unknown ticket is never the latest session from its address (one NAT, two players)
    assert acc.session_for_ticket("stale", "10.0.0.2") is None and acc.session_for_ticket("", "10.0.0.2") is None
    assert acc.session_for_ticket(c.ticket, "10.0.0.9") is c
    # the game's own login still follows Connect on the same address
    assert acc.login(LoginRequest("10.0.0.2", "uplaypc_v1", "jwe")).profile_id == c.profile_id


def test_the_rust_servers_settings_load(tmp_path):
    """One config.toml works with both servers: [server] name, motd, public, admins and [players] registration."""
    path = tmp_path / "config.toml"
    path.write_text('[server]\nname = "Ridge"\nmotd = "be nice"\npublic = true\nadmins = ["ABC"]\n'
                    '[players]\nregistration = "closed"\n', encoding="utf-8")
    cfg = Config.load(path)
    assert (cfg.server.name, cfg.server.motd, cfg.server.public, cfg.server.admins) == ("Ridge", "be nice", True,
                                                                                       ("abc",))
    assert cfg.players.registration == "closed" and Config().server.name == "Powder" and not Config().server.public
    path.write_text('[players]\nregistration = "maybe"\n', encoding="utf-8")
    with pytest.raises(ValueError):
        Config.load(path)
