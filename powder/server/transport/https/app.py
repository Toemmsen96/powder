"""HTTPS server for every UbiServices/Olympus hostname (the hosts file points them all at us).

Request pipeline (outermost first):
  limits_middleware   reads the body within its path's limit (413), time (408) and the memory budget (limits.py)
  log_middleware      access log + keeps uploaded remotelog/events bodies (they explain client errors; cut to
                      256 KiB each, the newest 200)
  cors_middleware     CORS for Ubisoft Connect's web UI (preflights, Allow-Origin); Ubisoft's origins only,
                      other origins may only GET / HEAD (see connect_origin)
  plugin_middleware   plugins.before_http hooks may answer a request themselves
  session_middleware  `Authorization: Ubi_v1 t=<ticket>` -> request[SESSION]; answers a route's Refused
  routes              routes_ubi.py (real logic), routes_profile.py (player-related, generated),
                      routes_powder.py (/_powder/), websocket.py
  fallback            game-data pack reply (configs) or `{}` + UNMATCHED warning
Routes match on the path only; the host matters only for the pack.
"""
from __future__ import annotations

import asyncio
import datetime
import json
import logging
import re
from pathlib import Path

from aiohttp import StreamReader, web

from ... import tls
from ...builtin import http as builtin_http
from ...constants import APP_ID
from ...core.identity import parse_authorization
from . import routes_profile, routes_powder, routes_ubi, routes_ugc
from . import limits as body_limits
from .keys import BUDGETS, DATA_DIR, FALLBACK, HOW, LIMITS, NEX_PUBLIC_KEY, PLUGINS, SERVICES, SESSION
from .routes_ubi import Refused, client_ip
from .websocket import websocket

log = logging.getLogger("powder.https")
SAVED_UPLOADS = ("/remotelog", "/profiles/me/events")
# Kept uploads (data/bodies/): each cut to this much (a remotelog or events batch is a few KB), and only the newest
# so many (the oldest are deleted), so they cannot fill the disk.
KEPT_BODY_MAX = 256 << 10
KEPT_BODIES = 200
SAVED_KINDS = ("remotelog", "events")


def host_of(request: web.Request) -> str:
    return (request.host or "").split(":")[0].lower()


def access_log(request: web.Request, how: str, status: int, size) -> None:
    line = f"{request.method} {host_of(request)}{request.path_qs[:160]} -> {how} {status} ({size}B)"
    if how == "UNMATCHED":
        log.warning(line)
    elif request.path.startswith("/_powder/"):        # the launcher polls these every few seconds
        log.debug(line)
    else:
        log.info(line)


async def refused(request: web.Request, status: int, message: str, how: str) -> web.Response:
    """A request refused before the pipeline (its body): UbiServices' error shape, sent, and the connection closed
    right after (its body was not read to the end; aiohttp would otherwise read on for up to 10 s)."""
    body = json.dumps({"errorCode": 1, "message": message, "httpCode": status}).encode()
    response = web.Response(status=status, body=body, headers={"Content-Type": routes_ubi.JSON})
    response.force_close()
    log.warning("%s %s%s from %s: %s", request.method, host_of(request), request.path, request.remote, message)
    access_log(request, how, status, len(body))
    await response.prepare(request)
    await response.write_eof()
    if request.protocol is not None:
        request.protocol.force_close()                  # closes the transport once the reply is out
    return response


@web.middleware
async def limits_middleware(request: web.Request, handler):
    """The body, read in full before anything else, within its path's limit and time, counted against the
    bodies' memory budget until the reply is made (handlers then get it from request.read() as before)."""
    if not request.body_exists:
        return await handler(request)
    limits = request.app[LIMITS]
    limit, kind = body_limits.body_rules(request.path)
    timeout = limits.body_timeout(kind)
    budget = request.app[BUDGETS][kind]
    try:
        async with asyncio.timeout(timeout):
            body, held = await body_limits.read_body(request.content, request.content_length, limit, budget)
    except body_limits.BodyTooLarge:
        return await refused(request, 413, f"Request body too large (at most {limit} bytes).", "body-too-large")
    except TimeoutError:
        return await refused(request, 408, f"Request body not received within {int(timeout)} s.", "body-timeout")
    replay(request, body)
    try:
        return await handler(request)
    finally:
        budget.release(held)


def replay(request: web.Request, body: bytes) -> None:
    """Make the body read by limits_middleware the request's again: request.read() / .json() return it,
    request.content streams it and can_read_body says whether there is one, as if nothing had read it."""
    stream = StreamReader(request.protocol, max(len(body), 1 << 16) + 1, loop=asyncio.get_running_loop())
    if body:
        stream.feed_data(body)
    stream.feed_eof()
    request._payload = stream                           # aiohttp's own attributes: there is no public setter
    request._read_bytes = body


def keep_upload(data_dir: Path, path: str, body: bytes) -> None:
    """Keep an uploaded remotelog / events body in data/bodies/ (cut to KEPT_BODY_MAX), then only the newest
    KEPT_BODIES of them."""
    kind = "remotelog" if path.endswith("/remotelog") else "events"
    bodies = data_dir / "bodies"
    try:
        bodies.mkdir(parents=True, exist_ok=True)
        (bodies / datetime.datetime.now().strftime(f"{kind}-%Y%m%d-%H%M%S-%f.txt")).write_bytes(body[:KEPT_BODY_MAX])
    except OSError as e:
        log.warning("keeping a %s upload: %s", kind, e)
    if len(body) > KEPT_BODY_MAX:
        log.debug("kept %s upload cut to %d of %d bytes", kind, KEPT_BODY_MAX, len(body))
    prune_kept(bodies, KEPT_BODIES)


def prune_kept(bodies: Path, keep: int) -> None:
    """Delete the oldest kept uploads beyond the newest `keep` (names sort by time after their kind)."""
    try:
        kept = sorted((p.name.split("-", 1)[1], p) for p in bodies.iterdir()
                      if "-" in p.name and p.name.split("-", 1)[0] in SAVED_KINDS and p.name.endswith(".txt"))
    except OSError:
        return
    for _, p in kept[:max(0, len(kept) - keep)]:
        p.unlink(missing_ok=True)                       # another request may have deleted it already


@web.middleware
async def log_middleware(request: web.Request, handler):
    path = request.path
    if request.method == "POST" and path.endswith(SAVED_UPLOADS) and request.can_read_body:
        keep_upload(request.app[DATA_DIR], path, await request.read())
    response = await handler(request)
    size = response.content_length if response.content_length is not None else "-"
    access_log(request, request.get(HOW, "?"), response.status, size)
    return response


CORS_METHODS = "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"
CORS_EXPOSE = "X-Total-Count, Ubi-TransactionId"
CONNECT_DOMAINS = ("ubisoft.com", "ubi.com")
HOSTNAME = re.compile(r"[A-Za-z0-9.-]+")


def connect_origin(origin: str) -> bool:
    """Whether a request's Origin is Ubisoft Connect's embedded browser: its UI and overlay come from Ubisoft's
    CDN (https://connect.cdn.ubisoft.com, https://<env>connect.cdn.ubisoft.com, ...), so https and a
    ubisoft.com / ubi.com host (a port allowed). The game, the launcher and Connect's own client send no
    Origin."""
    if origin[:8].lower() != "https://":
        return False
    host, sep, port = origin[8:].rpartition(":")
    if not sep or not all(c in "0123456789" for c in port):
        host = origin[8:]
    host = host.lower()
    return bool(HOSTNAME.fullmatch(host)) and any(host == d or host.endswith("." + d) for d in CONNECT_DOMAINS)


def same_origin(request: web.Request, origin: str) -> bool:
    """A page this server served itself (e.g. a plugin's) calling it back: the browser sets Origin to the page's
    own scheme + host, which no other site can claim."""
    host = request.headers.get("Host", "")
    return bool(host) and origin.lower() == f"https://{host}".lower()


@web.middleware
async def cors_middleware(request: web.Request, handler):
    """Ubisoft Connect's UI is a web page (origin https://connect.cdn.ubisoft.com) calling UbiServices
    from its embedded browser, which needs CORS: answer preflights and allow the origin, like Ubisoft. Only
    its origins get CORS headers (with credentials). Another origin's GET / HEAD is answered without them
    (the browser hides the reply), and so is a page of this server's own; anything else from a foreign
    origin is refused: the hosts file sends the player's browser here too, so a web page could otherwise post
    to the server (e.g. register the first account on a server without accounts)."""
    origin = request.headers.get("Origin")
    if not origin:
        return await handler(request)
    if not connect_origin(origin):
        if request.method in ("GET", "HEAD") or same_origin(request, origin):
            return await handler(request)
        log.warning("%s %s from %s refused: origin %r is not Ubisoft Connect's", request.method, request.path,
                    client_ip(request), origin)
        return routes_ubi.json_response(request, {"errorCode": 1, "message": "origin not allowed", "httpCode": 403},
                                        "cors-refused", 403)
    if request.method == "OPTIONS" and "Access-Control-Request-Method" in request.headers:
        request[HOW] = "cors-preflight"
        response = web.Response(status=200)
        response.headers["Access-Control-Allow-Methods"] = CORS_METHODS
        response.headers["Access-Control-Allow-Headers"] = request.headers.get("Access-Control-Request-Headers", "")
    else:
        response = await handler(request)
        if response.prepared:                            # websocket upgrade: headers already sent
            return response
    response.headers["Access-Control-Allow-Origin"] = origin
    response.headers["Access-Control-Allow-Credentials"] = "true"
    response.headers["Access-Control-Expose-Headers"] = CORS_EXPOSE
    return response


@web.middleware
async def plugin_middleware(request: web.Request, handler):
    plugins = request.app.get(PLUGINS)
    if plugins is not None:
        response = await plugins.run_before_http(request)
        if response is not None:
            request[HOW] = "plugin"
            return response
    return await handler(request)


@web.middleware
async def session_middleware(request: web.Request, handler):
    scheme, ticket = parse_authorization(request.headers.get("Authorization"))
    if scheme.lower() == "ubi_v1" and ticket:
        request[SESSION] = request.app[SERVICES].accounts.session_for_ticket(ticket, client_ip(request))
    try:
        return await handler(request)
    except Refused as refused:                          # no session, or someone else's data (routes_ubi.py)
        return refused.response


async def fallback(request: web.Request) -> web.Response:
    """Game-wide data from the game-data pack for anything without a route. Requests from Ubisoft
    Connect (any app id other than the game's) try Connect's part of the pack first."""
    assets = request.app[SERVICES].assets
    app_id = request.headers.get("Ubi-AppId", "").lower()
    order = (assets.connect_http, assets.http) if app_id and app_id != APP_ID else (assets.http, assets.connect_http)
    hit = None
    for fixtures in order:
        hit = fixtures.lookup(request.method, host_of(request), request.path_qs)
        if hit:
            break
    if hit is None:
        cached_ui = builtin_http.keep_cached(request.path)     # Connect's UI settings: never overwrite its cache
        if cached_ui and (request.headers.get("If-None-Match") or request.headers.get("If-Modified-Since")):
            request[HOW] = "not-modified"
            return web.Response(status=304)
        built = builtin_http.lookup(request.method, request.path, request.query)
        if built is not None:                           # no pack (or not in it): the server's own reply
            request[HOW] = "builtin"
            body = storm_config(request, builtin_http.body(built))
            return web.Response(body=body, headers={"Content-Type": routes_ubi.JSON})
        request[HOW] = "UNMATCHED"
        if cached_ui:
            return web.Response(status=503, body=b"{}", headers={"Content-Type": routes_ubi.JSON})
        return web.Response(body=b"{}", headers={"Content-Type": routes_ubi.JSON})
    fixture, tier = hit
    request[HOW] = f"gamedata:{tier}"
    body = storm_config(request, fixture.body)
    return web.Response(body=body, status=fixture.status, headers={"Content-Type": fixture.content_type})


GAME_CONFIG = re.compile(rf"/v\d+/applications/{APP_ID}/configuration", re.IGNORECASE)


def storm_config(request: web.Request, body: bytes) -> bytes:
    """The game's SDK configuration names its peer-to-peer helpers (Storm, transport/storm.py): this server,
    whether the reply comes from the pack (recorded: Ubisoft's hosts) or is the built-in one."""
    if not GAME_CONFIG.fullmatch(request.path):
        return body
    cfg = request.app[SERVICES].config
    return with_storm(body, storm_settings(cfg.server) if cfg.multiplayer.storm == "powder" else None)


def storm_settings(server) -> dict:
    """Where the games reach Storm: the address they reach the game server at (an address, not a hostname, so
    no hosts entries are needed; on the hosting PC its network address, as the others see it)."""
    host, port = server.public_host, server.storm_port
    return {"detectUrl1": f"{host}:{port}", "detectUrl2": f"{host}:{port + 2}",
            "traversalUrl": f"{host}:{port + 5}",
            "routerProvisioningUrl": "https://public-ubiservices.ubi.com/v1/storm/router"}   # relay: never asked


def with_storm(body: bytes, settings: dict | None) -> bytes:
    """The configuration with these Storm settings ("storm" and "punch"); None: none (multiplayer off)."""
    try:
        data = json.loads(body)
        cfg = data.get("configuration")
    except (ValueError, AttributeError):
        return body
    if not isinstance(cfg, dict):                       # not an SDK configuration
        return body
    cfg["storm"] = dict(settings or {})
    if settings:
        cfg["punch"] = dict(settings)
    else:
        cfg.pop("punch", None)
    return json.dumps(data).encode()


def build_app(services, plugins=None, *, nex_public_key: str = "",
              limits: body_limits.Limits | None = None) -> web.Application:
    # limits_middleware reads every body first, by path (limits.py); this is only aiohttp's own ceiling
    app = web.Application(middlewares=[limits_middleware, log_middleware, cors_middleware, plugin_middleware,
                                       session_middleware], client_max_size=body_limits.MAX_UPLOAD + 1)
    limits = limits or body_limits.Limits()
    app[LIMITS] = limits
    app[BUDGETS] = {body_limits.SMALL: body_limits.Budget(limits.small_budget),
                    body_limits.UPLOAD: body_limits.Budget(limits.upload_budget)}
    app[SERVICES] = services
    app[PLUGINS] = plugins
    app[NEX_PUBLIC_KEY] = nex_public_key
    app[DATA_DIR] = Path(services.config.data_dir)
    app[FALLBACK] = fallback
    routes_powder.add_routes(app)
    routes_profile.add_routes(app)
    routes_ubi.add_routes(app)
    routes_ugc.add_routes(app)
    app.router.add_get("/v2/websocket", websocket)
    app.router.add_get("/v2/websocket/server", websocket)     # Ubisoft Connect's ("websocket/server" in the SDK config)
    app.router.add_route("*", "/{tail:.*}", fallback)      # last: everything else
    return app


async def start_crl(crl, host: str, port: int, limits: body_limits.Limits | None = None):
    """Plain HTTP: the local CA's revocation list (Windows checks it for Ubisoft Connect), reached as
    http://public-ubiservices.ubi.com/powder-ca.crl. `crl` is a callable returning the DER bytes. With its own
    connection limits; nothing there takes a body. -> (runner, listener)."""
    from .. import tls_router
    limits = limits or body_limits.Limits()

    async def serve(request: web.Request) -> web.Response:
        return web.Response(body=crl(), content_type="application/pkix-crl")
    app = web.Application(client_max_size=body_limits.MAX_BODY)
    for path in (tls.CRL_PATH, *tls.LEGACY_CRL_PATHS):
        app.router.add_get(path, serve)
    runner = web.AppRunner(app, access_log=None, keepalive_timeout=limits.header_read)
    await runner.setup()
    try:
        listener = await tls_router.start_plain(runner, host, port, limits)
    except BaseException:
        await runner.cleanup()
        raise
    return runner, listener
