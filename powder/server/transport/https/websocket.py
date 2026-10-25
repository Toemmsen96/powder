"""UbiServices notification WebSocket (public-ws-ubiservices.ubi.com/v2/websocket, the game's, and
/v2/websocket/server?SpaceIds=...&NotificationTypes=..., Ubisoft Connect's).

The client's JobInitiateConnection waits for US_CONNECTION_ESTABLISHED with a connectionId; without
it NEX Register fails ("Failed to get the connection Id ..."). Envelope format from FUN_7ff661c884e0.
Open sockets are registered on the player's session, so services can push notifications later
(SessionRegistry.push_ws): into the socket's queue (core/sessions.WsQueue), sent from there by a task of its
own, so a client that does not read holds up nobody else.

Clients send nothing the server uses (what arrives is only logged): a message over MAX_INCOMING ends the socket
(aiohttp closes it with 1009, message too big; logged). Our pushes are not limited.
"""
from __future__ import annotations

import asyncio
import logging
import secrets
import uuid

from aiohttp import WSMsgType, web

from ...core.identity import parse_authorization
from ...core.sessions import WsQueue
from ...util import iso_now
from .keys import HOW, SERVICES
from .routes_ubi import client_ip

log = logging.getLogger("powder.https")
MAX_INCOMING = 64 << 10          # the largest message a client may send (aiohttp's default was 4 MiB)


async def websocket(request: web.Request) -> web.StreamResponse:
    services = request.app[SERVICES]
    offered = request.headers.get("Sec-WebSocket-Protocol", "")
    first = offered.split(",")[0].strip()
    ws = web.WebSocketResponse(protocols=(first,) if first else (), heartbeat=None, autoping=True,
                               max_msg_size=MAX_INCOMING)
    check = ws.can_prepare(request)
    if not check.ok:
        # header names only: the values carry the player's ticket (Authorization)
        log.warning("websocket handshake not acceptable; headers: %s", ", ".join(request.headers.keys()))
        request[HOW] = "ws-rejected"
        return web.Response(status=400)
    await ws.prepare(request)
    request[HOW] = "websocket"

    _, ticket = parse_authorization(request.headers.get("Authorization"))
    session = services.accounts.session_for_ticket(ticket, client_ip(request))
    connection_id = str(uuid.uuid4())
    # contactUrl is where the client PUTs which notification types it wants (its JobUpdateConnection), as
    # Ubisoft's: https://public-im-.../v2/connections/<node>/<port>/<connection id>/notifications (answered
    # by routes_profile). A wss:// address there fails in the game ("Unsupported protocol").
    contact = f"https://public-im-ubiservices.ubi.com/v2/connections/{secrets.token_hex(9)[:17]}/8180/" \
              f"{connection_id}/notifications"
    await ws.send_json({"notificationType": "US_CONNECTION_ESTABLISHED",
                        "content": {"connectionId": connection_id, "contactUrl": contact},
                        "datePosted": iso_now()})
    pushes = WsQueue()
    sender = asyncio.ensure_future(send_pushes(ws, pushes))
    if session is not None:
        session.websockets.add(pushes)
        await services.events.emit("ws_connected", session=session)
    log.info("websocket open (%s) for %s", connection_id, session.player.name if session else "unknown")
    try:
        async for msg in ws:
            if msg.type in (WSMsgType.TEXT, WSMsgType.BINARY):
                log.info("websocket <- %r", msg.data[:200])
            elif msg.type == WSMsgType.ERROR:
                log.warning("websocket from %s: %s", client_ip(request), ws.exception())
    finally:
        pushes.close()
        sender.cancel()
        if session is not None:
            session.websockets.discard(pushes)
        log.info("websocket closed (%s)", connection_id)
    return ws


async def send_pushes(ws: web.WebSocketResponse, pushes: WsQueue) -> None:
    """Send what is pushed to this socket, in order, as fast as the client takes it."""
    try:
        while True:
            await ws.send_json(await pushes.queue.get())
    except asyncio.CancelledError:
        raise
    except Exception as e:  # noqa: BLE001 - the socket is gone: push_ws forgets it
        log.debug("websocket push failed: %s", e)
        pushes.close()
