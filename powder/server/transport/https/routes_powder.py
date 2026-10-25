"""The server's own endpoints (not Ubisoft's), under /_powder/. Used by the launcher; the web admin will
build on them. The status needs no sign-in; it shows the addresses of connected games and Connect clients only
to the server's own PC (loopback) and to each caller for its own address (others: names only; on a [server]
public server not at all). The account calls carry the account's email and password.

  GET /_powder/status   who is connected: Ubisoft Connect (demux) clients and game (NEX) sessions, and
                        the caller's address as the server sees it ("you": a launcher finds its own PC)
  GET /_powder/account/new        whether registering takes over the existing player
  POST /_powder/account/register|login|email|password|name   accounts, for the launcher (JSON; each
                        call carries email and password, failed ones are rate-limited per address)
  GET /_powder/ca.pem   the local CA certificate (public part), for clients that must trust it
  GET /_powder/overlay/powder.js   the Powder part of Ubisoft Connect's in-game overlay
"""
from __future__ import annotations

import inspect
import ipaddress

from aiohttp import web

from ....version import __version__
from ...builtin import connect_ui
from ...services.accounts import AccountError, AuthError
from .keys import CA_PEM, HOW, SERVICES
from .routes_ubi import client_ip


REGISTER_KEY = "#register"


def _peer(nex) -> str:
    addr = getattr(nex, "addr", None)
    return f"{addr[0]}:{addr[1]}" if addr else ""


def _ip(address: str | None):
    """An address as an ipaddress object (IPv4-mapped IPv6 as the IPv4 address), else None."""
    try:
        ip = ipaddress.ip_address((address or "").strip("[]"))
    except ValueError:
        return None
    return ip.ipv4_mapped or ip if ip.version == 6 else ip


def _shown(request: web.Request, you, address: str | None, entry: dict) -> dict | None:
    """Who sees which entries of the status: the server's own PC (loopback) everything; anyone else its own
    entries (same address) as they are, the others' without their address, and on a public server not at all.
    The launcher uses its own entries ("connect", "game" with the peer it is seen from) and the names in
    "game"."""
    if you is not None and (you.is_loopback or _ip(address) == you):
        return entry
    if request.app[SERVICES].config.server.public:
        return None
    return {k: v for k, v in entry.items() if k != "peer"}


async def status(request: web.Request) -> web.Response:
    services = request.app[SERVICES]
    you = _ip(client_ip(request))
    connect = [_shown(request, you, getattr(c, "client_ip", None),
                      {"player": c.player_name, "peer": c.peer, "services": sorted(set(c.channels.values()))})
               for c in services.connect_clients if c.profile_id]
    game = [_shown(request, you, (getattr(s.nex, "addr", None) or (s.client_ip,))[0],
                   {"player": s.player.name, "nex": s.nex is not None, "peer": _peer(s.nex)})
            for s in services.sessions.by_player.values() if s.nex is not None]
    connect, game = [e for e in connect if e is not None], [e for e in game if e is not None]
    request[HOW] = "powder-status"
    return web.json_response({"server": "powder", "version": __version__, "you": client_ip(request),
                              "public_host": services.config.server.public_host,
                              "accounts": services.accounts.required,
                              "players": len(services.repos.players.all()),
                              "gamedata": not services.assets.empty,
                              "connect": connect, "game": game})


async def ca_pem(request: web.Request) -> web.StreamResponse:
    path = request.app.get(CA_PEM)
    request[HOW] = "powder-ca"
    if path is None or not path.exists():
        return web.Response(status=404, text="no CA")
    return web.Response(body=path.read_bytes(), content_type="application/x-pem-file")


async def overlay_part(request: web.Request) -> web.Response:
    """The Powder part of Ubisoft Connect's in-game overlay (builtin/connect_ui.py); the overlay loads it
    cross-origin, like Ubisoft's parts from their CDN."""
    request[HOW] = "powder-overlay"
    return web.Response(text=connect_ui.POWDER_PART_JS, content_type="application/javascript",
                        headers={"Access-Control-Allow-Origin": "*"})


# ---- accounts (the launcher registers and manages them) ---------------------------------------
# JSON bodies; every call that reads or changes an account carries its email and current password, so
# the launcher needs no session of its own. Bodies are never logged (passwords).
async def _account_call(request: web.Request, change) -> web.Response:
    services = request.app[SERVICES]
    limiter = services.login_failures
    ip = client_ip(request)
    request[HOW] = "powder-account"
    try:
        body = await request.json()
        if not isinstance(body, dict):
            raise ValueError
    except ValueError:
        return web.json_response({"error": "bad request"}, status=400)
    email = str(body.get("email", ""))
    key = REGISTER_KEY if change is None else email      # registrations count like failures (hashing is slow)
    # counted before hashing (off the event loop: concurrent calls can't all pass); a registration always
    # counts, a call whose password is right takes its count back
    if not limiter.attempt(ip, key):
        return web.json_response({"error": "Too many attempts, try again in a few minutes."}, status=429)
    accounts = services.accounts
    try:
        if change is None:
            account = await accounts.register_async(email, str(body.get("password", "")), str(body.get("name", "")),
                                                    ip)
        else:
            account = await accounts.authenticate_async(email, str(body.get("password", "")))
            limiter.succeeded(ip, email)
            changed = change(accounts, account, body)
            account = (await changed if inspect.isawaitable(changed) else changed) or account
    except AuthError:
        return web.json_response({"error": "Wrong email or password."}, status=401)
    except AccountError as e:
        return web.json_response({"error": str(e)}, status=400)
    return web.json_response(accounts.info(account))


async def account_register(request: web.Request) -> web.Response:
    return await _account_call(request, None)


async def account_login(request: web.Request) -> web.Response:
    return await _account_call(request, lambda accounts, account, body: None)


async def account_email(request: web.Request) -> web.Response:
    return await _account_call(request, lambda accounts, account, body:
                               accounts.change_email(account, str(body.get("new_email", ""))))


async def account_password(request: web.Request) -> web.Response:
    return await _account_call(request, lambda accounts, account, body:
                               accounts.change_password_async(account, str(body.get("new_password", ""))))


async def account_name(request: web.Request) -> web.Response:
    return await _account_call(request, lambda accounts, account, body:
                               accounts.rename(account, str(body.get("name", ""))))


async def account_new(request: web.Request) -> web.Response:
    """What registering would do here: take over an existing player (the first account on a server, registered
    from the server's own PC; services/accounts.py register_from)."""
    request[HOW] = "powder-account"
    return web.json_response({"takes_over": request.app[SERVICES].accounts.claimable_player_name_for(
        client_ip(request))})


def add_routes(app: web.Application) -> None:
    app.router.add_get("/_powder/status", status)
    app.router.add_get("/_powder/account/new", account_new)
    app.router.add_post("/_powder/account/register", account_register)
    app.router.add_post("/_powder/account/login", account_login)
    app.router.add_post("/_powder/account/email", account_email)
    app.router.add_post("/_powder/account/password", account_password)
    app.router.add_post("/_powder/account/name", account_name)
    app.router.add_get("/_powder/ca.pem", ca_pem)
    app.router.add_get(connect_ui.POWDER_PART_PATH, overlay_part)
