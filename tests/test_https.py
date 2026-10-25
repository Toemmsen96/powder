"""HTTPS (UbiServices) app through aiohttp's test client, with the game's real hostnames."""
import asyncio
import base64
import json

import pytest
from aiohttp.test_utils import TestClient, TestServer

import synthetic as syn
from powder.server.constants import APP_ID, CREDITS_ITEM, SPACE_ID
from powder.server.transport.https.app import build_app

UBI = {"Host": syn.UBI}


@pytest.fixture
async def client(services, player):            # the made-up player, imported: the default player
    c = TestClient(TestServer(build_app(services, nex_public_key="TESTKEY")))
    await c.start_server()
    yield c
    await c.close()


def auth_for(services, pid: str) -> dict:
    """The headers of a new session of this player (the game's, from this PC)."""
    ticket = services.sessions.issue(services.accounts.player(pid), "127.0.0.1").ticket
    return {**UBI, "Authorization": f"Ubi_v1 t={ticket}"}


async def login(client) -> dict:
    r = await client.post("/v3/profiles/sessions", headers={**UBI, "Authorization": "uplaypc_v1 t=connect-jwe"},
                          json={})
    assert r.status == 200
    return await r.json()


async def test_login_issues_our_ticket_for_the_default_player(client, services):
    body = await login(client)
    assert body["profileId"] == syn.PLAYER and body["nameOnPlatform"] == syn.PLAYER_NAME
    assert body["spaceId"] == SPACE_ID and body["ticket"] != "recorded-ticket"
    assert body["ticket"].startswith("powder.") and services.sessions.by_token(body["ticket"])


CONNECT_APP = "f68a4bb5-608a-4ff2-8123-be8ef797e0a6"


@pytest.mark.parametrize("auth", ["rm_v1 t=remember-me", "Basic dXNlckBleGFtcGxlLmNvbTpzZWNyZXQ=", "ubi_v1 t=x"])
async def test_ubisoft_connect_login(client, services, auth):
    """Connect signs in with a remember-me ticket, email:password or a refresh; all are accepted, and it
    gets a Ubisoft-shaped ticket plus a new remember-me ticket."""
    r = await client.post("/v3/profiles/sessions", json={"rememberMe": True},
                          headers={"Ubi-AppId": CONNECT_APP, "Authorization": auth})
    assert r.status == 200
    body = await r.json()
    assert body["profileId"] == syn.PLAYER
    header = json.loads(base64.b64decode(body["ticket"].split(".")[0]))
    assert header["aid"] == CONNECT_APP and header["sid"] == body["sessionId"]
    assert body["rememberMeTicket"] and body["twoFactorAuthenticationTicket"] is None
    assert services.sessions.by_token(body["ticket"]).profile_id == syn.PLAYER


async def test_cors_for_connects_web_ui(client):
    origin = "https://connect.cdn.ubisoft.com"
    r = await client.options("/v3/profiles/sessions", headers={
        "Origin": origin, "Access-Control-Request-Method": "POST",
        "Access-Control-Request-Headers": "ubi-appid,authorization"})
    assert r.status == 200
    assert r.headers["Access-Control-Allow-Origin"] == origin
    assert r.headers["Access-Control-Allow-Headers"] == "ubi-appid,authorization"
    r = await client.get("/v3/users/me", headers={"Origin": origin})
    assert r.headers["Access-Control-Allow-Origin"] == origin
    r = await client.get("/v3/users/me")                     # the game sends no Origin: untouched
    assert "Access-Control-Allow-Origin" not in r.headers


async def test_no_cloud_save_metadata(client):
    """Connect must never be offered a replayed (stale) cloud save."""
    r = await client.get(f"/v1/profiles/{syn.PLAYER}/entities?name=upcCloudSaveUplayId_3280")
    assert (await r.json()) == {"entities": []}


async def test_connect_internet_check(client):
    for path in ("/health_check/test_internet.txt?cacheBuster=1", "/orbit/uplay_launcher_2_0/test_internet.txt"):
        r = await client.get(path, headers={"Host": "static3.cdn.ubi.com"})
        assert r.status == 200 and await r.text() == "I AM ALIVE!"


async def test_rendezvous_points_at_our_nex_server(client):
    r = await client.get(f"/v1/spaces/{SPACE_ID}/sandboxes/STEEP_PC_TU7_A/rendezvous/configuration",
                         headers={"Host": "msr-public-ubiservices.ubi.com"})
    body = await r.json()
    assert body["public_key"] == "TESTKEY" and body["prudp_loadbalancer_url"].startswith("prudp:/address=")


async def test_purchase_round_trip(client):
    s = await login(client)
    auth = {**UBI, "Authorization": f"Ubi_v1 t={s['ticket']}", "Ubi-SessionId": s["sessionId"]}
    inv_url = f"/v1/profiles/{s['profileId']}/inventory"

    async def credits():
        items = (await (await client.get(inv_url + f"?spaceId={SPACE_ID}", headers=auth)).json())["items"]
        return next(i for i in items if i["itemId"] == CREDITS_ITEM)["quantity"]

    assert await credits() == syn.SEEDED_CREDITS
    r = await client.post(inv_url, headers=auth, json={"spaceId": SPACE_ID, "offerId": syn.SKI_A, "offerRevision": 6})
    assert r.status == 200
    reply = await r.json()
    assert reply["transactionId"]
    assert await credits() == syn.SEEDED_CREDITS - syn.SKI_A_PRICE

    r = await client.post(inv_url, headers=auth, json={"spaceId": SPACE_ID, "offerId": "nope"})
    assert r.status == 404 and (await r.json())["errorCode"]


async def test_offers_from_the_pack_catalogue(client, services):
    r = await client.get(syn.SKI_PAGE, headers=UBI)
    assert {o["offerId"] for o in (await r.json())["offers"]} == {syn.SKI_A, syn.SKI_B}
    r = await client.get(f"/v1/spaces/{SPACE_ID}/offers?tags=SKI,DELUXE&offset=0&limit=5", headers=UBI)
    assert [o["offerId"] for o in (await r.json())["offers"]] == [syn.SKI_B]


async def test_pack_fallback_and_unmatched(client):
    r = await client.get(syn.CONFIG_PATH, headers=UBI)
    assert r.status == 200 and await r.json() == {"recorded": True}
    r = await client.get("/v9/nothing/here", headers=UBI)
    assert r.status == 200 and await r.json() == {}


async def test_connect_ui_settings_never_overwrite_the_browser_cache(client):
    """Without a pack reply: 304 if Connect's browser has a copy, else an error (never an empty 200)."""
    path = "/v4/spaces/global/ubiconnect/configcache/api/preauth?bundleEnv=prod"
    r = await client.get(path, headers={**UBI, "If-None-Match": '"abc"'})
    assert r.status == 304
    r = await client.get(path, headers=UBI)
    assert r.status == 503
    r = await client.get("/v1/spaces/x/parameters?parameterGroups=ubiConnect-deeplinkConfig", headers=UBI)
    assert r.status == 503


async def test_builtin_sdk_configuration_for_the_game(client):
    r = await client.get(f"/v1/applications/{APP_ID}/configuration", headers=UBI)
    config = (await r.json())["configuration"]
    assert config["platformConfig"]["applicationId"] == APP_ID and config["platformConfig"]["spaceId"] == SPACE_ID


async def test_websocket_hello_and_server_push(client, services):
    s = await login(client)
    ws = await client.ws_connect("/v2/websocket?NotificationTypes=US_CONNECTION_ESTABLISHED",
                                 headers={"Host": "public-ws-ubiservices.ubi.com",
                                          "Authorization": f"Ubi_v1 t={s['ticket']}"})
    hello = json.loads((await ws.receive()).data)
    assert hello["notificationType"] == "US_CONNECTION_ESTABLISHED" and hello["content"]["connectionId"]
    # the client PUTs its notification types to contactUrl: an https address we answer
    contact = hello["content"]["contactUrl"]
    assert contact.startswith("https://public-im-ubiservices.ubi.com/v2/connections/")
    assert contact.endswith(f"/{hello['content']['connectionId']}/notifications")
    r = await client.put(contact.split(".com", 1)[1], headers={"Host": "public-im-ubiservices.ubi.com"},
                         json={"notificationType": ["STEEP_SPECTATOR"]})
    assert r.status == 200 and await r.json() == {}
    assert await services.sessions.push_ws(s["profileId"], "TEST_NOTIFICATION", {"x": 1}) == 1
    pushed = json.loads((await ws.receive()).data)
    assert pushed["notificationType"] == "TEST_NOTIFICATION"
    await ws.close()


async def test_connects_websocket_server_path(client):
    """Ubisoft Connect opens /v2/websocket/server (not /v2/websocket); it gets the same hello."""
    s = await login(client)
    ws = await client.ws_connect("/v2/websocket/server?SpaceIds=x&NotificationTypes=US_CONNECTION_ESTABLISHED",
                                 headers={"Host": "public-ws-ubiservices.ubi.com",
                                          "Authorization": f"Ubi_v1 t={s['ticket']}"})
    hello = json.loads((await ws.receive()).data)
    assert hello["notificationType"] == "US_CONNECTION_ESTABLISHED" and hello["content"]["connectionId"]
    await ws.close()


async def test_big_client_messages_close_the_websocket(client, services):
    """A client message over MAX_INCOMING ends the socket (1009) and unregisters it; smaller ones are only logged."""
    from aiohttp import WSMsgType
    from powder.server.transport.https.websocket import MAX_INCOMING
    s = await login(client)
    ws = await client.ws_connect("/v2/websocket", headers={"Host": "public-ws-ubiservices.ubi.com",
                                                           "Authorization": f"Ubi_v1 t={s['ticket']}"})
    await ws.receive()
    await ws.send_str("x" * MAX_INCOMING)
    assert await services.sessions.push_ws(s["profileId"], "STILL_OPEN", {}) == 1
    assert json.loads((await ws.receive()).data)["notificationType"] == "STILL_OPEN"
    await ws.send_bytes(b"\0" * (MAX_INCOMING + 1))
    msg = await ws.receive()
    assert msg.type in (WSMsgType.CLOSE, WSMsgType.CLOSED, WSMsgType.CLOSING) and ws.close_code == 1009
    await ws.close()
    for _ in range(50):
        if not services.sessions.for_player(s["profileId"]).websockets:
            break
        await asyncio.sleep(0.01)
    assert await services.sessions.push_ws(s["profileId"], "GONE", {}) == 0


async def test_a_full_websocket_queue_drops_pushes(services):
    """A websocket that does not keep up: pushes beyond its queue are dropped, it stays registered, and gets the
    next ones once it reads again; a closed one is forgotten."""
    from powder.server.core.sessions import WS_QUEUE, WsQueue
    a = services.sessions.issue(services.accounts.player(services.default_player_id()), "10.0.0.1")
    q = WsQueue()
    a.websockets.add(q)
    for _ in range(WS_QUEUE):
        assert await services.sessions.push_ws(a.profile_id, "TEST", {}) == 1
    assert await services.sessions.push_ws(a.profile_id, "LOST", {}) == 0
    assert a.websockets == {q}
    assert q.queue.get_nowait()["notificationType"] == "TEST"
    assert await services.sessions.push_ws(a.profile_id, "AFTER", {}) == 1
    rest = [q.queue.get_nowait() for _ in range(q.queue.qsize())]
    assert (len(rest), rest[-1]["notificationType"]) == (WS_QUEUE, "AFTER")
    q.close()
    assert await services.sessions.push_ws(a.profile_id, "TEST", {}) == 0
    assert not a.websockets, "closed: forgotten"


async def test_a_slow_websocket_does_not_hold_up_the_others(services):
    """push_ws never waits for a socket: one whose client does not read cannot delay another's pushes."""
    from powder.server.core.sessions import WsQueue
    a = services.sessions.issue(services.accounts.player(services.default_player_id()), "10.0.0.1")
    stuck, fine = WsQueue(), WsQueue()
    a.websockets.update((stuck, fine))
    await asyncio.wait_for(services.sessions.push_ws(a.profile_id, "TEST", {}), 0.5)
    assert fine.queue.qsize() == stuck.queue.qsize() == 1


async def test_personal_endpoints_are_generated_not_replayed(client):
    """Account details, friends and Club actions come from the player record: nothing of the recorded
    account (email, friends, action progress) is ever served."""
    me = await (await client.get("/v3/users/me", headers=UBI)).json()
    assert me["nameOnPlatform"] == syn.PLAYER_NAME and me["email"].endswith("@powder.invalid")
    assert me["email"] != syn.PLAYER_EMAIL and me["phone"]["number"] == ""
    friends = await (await client.get(f"/v3/profiles/{syn.PLAYER}/friends", headers=UBI)).json()
    assert friends["friends"] == []
    assert (await (await client.get("/v2/profiles/me/friends", headers=UBI)).json()) == {"friends": []}
    actions = (await (await client.get(f"/v1/profiles/{syn.PLAYER}/actions", headers=UBI)).json())["actions"]
    assert [a["id"] for a in actions] == [syn.ACTION["id"]] and actions[0]["value"] == 0
    looked_up = await (await client.get(f"/v3/profiles?profileIds={syn.PLAYER},{syn.FRIEND}", headers=UBI)).json()
    assert [p["profileId"] for p in looked_up["profiles"]] == [syn.PLAYER]     # only players we know
    assert (await (await client.get("/ugc/v1/?algo=Steep", headers={"Host": "steep-prod-pc.steep.olympus.ubi.com"})).json()) == []


async def test_every_other_player_is_a_friend(client, services):
    """The game's friends list (social tab) and Connect's: the server's other players, with online status."""
    services.accounts.register("host@example.com", "correct horse", "")          # takes over syn.PLAYER
    guest = services.accounts.register("guest@example.com", "correct horse", "Guest")
    auth = auth_for(services, syn.PLAYER)
    game = (await (await client.get("/v2/profiles/me/friends", headers=auth)).json())["friends"]
    assert [(f["pid"], f["nameOnPlatform"], f["state"]) for f in game] == [(guest.profile_id, "Guest", "Friends")]
    plain = (await (await client.get(f"/v3/profiles/{syn.PLAYER}/friends", headers=auth)).json())["friends"]
    assert [f["pid"] for f in plain] == [guest.profile_id] and "friendshipDate" in plain[0]
    url = f"/v3/profiles/{syn.PLAYER}/global/harboursocial/friends/core/enriched"
    assert (await (await client.get(url, headers=auth)).json())["friends"][0]["onlineStatus"] == "offline"
    services.connect_clients.add(type("Conn", (), {"profile_id": guest.profile_id})())   # Guest's Connect is on
    assert (await (await client.get(url, headers=auth)).json())["friends"][0]["onlineStatus"] == "online"
    user = services.repos.players.get(guest.profile_id).user_id
    statuses = (await (await client.get(f"/v1/users/onlineStatuses?UserIds={user}", headers=auth)).json())
    assert statuses["onlineStatuses"][0]["onlineStatus"] == "online"


async def test_a_friend_in_steep_has_a_connection(client, services):
    """The game asks which friends are in Steep (and only offers to invite those)."""
    import dataclasses
    guest = services.accounts.register("guest@example.com", "correct horse", "Guest")
    auth = auth_for(services, syn.PLAYER)
    url = f"/v1/profiles/connections?limit=24&offset=0&profileIds={guest.profile_id}&applications={APP_ID}"
    assert (await (await client.get(url, headers=auth)).json()) == {"connections": []}
    session = services.sessions.issue(services.accounts.player(guest.profile_id), "192.0.2.20")
    session.nex = object()                                                       # Guest's game is connected
    (conn,) = (await (await client.get(url, headers=auth)).json())["connections"]
    assert conn["profileId"] == guest.profile_id and conn["applicationId"] == APP_ID
    other_app = url.replace(APP_ID, "f68a4bb5-608a-4ff2-8123-be8ef797e0a6")
    assert (await (await client.get(other_app, headers=auth)).json()) == {"connections": []}
    session.nex = None
    services.config = dataclasses.replace(services.config, multiplayer=dataclasses.replace(
        services.config.multiplayer, pretend_online=("guest",)))                 # testing alone
    assert len((await (await client.get(url, headers=auth)).json())["connections"]) == 1


async def test_a_notification_reaches_the_other_players_game(client, services):
    """POST /v2/profiles/{id}/notifications (Steep's Spectate asks the friend this way)."""
    guest = services.accounts.register("guest@example.com", "correct horse", "Guest")
    got = []

    class FakeWs:
        async def send_json(self, data):
            got.append(data)
    services.sessions.issue(services.accounts.player(guest.profile_id), "192.0.2.20").websockets.add(FakeWs())
    r = await client.post(f"/v2/profiles/{guest.profile_id}/notifications", headers=auth_for(services, syn.PLAYER),
                          json={"notificationType": "STEEP_SPECTATOR", "content": {"ask": 1}, "spaceId": SPACE_ID})
    assert r.status == 200
    (n,) = got
    assert (n["notificationType"], n["content"], n["spaceId"]) == ("STEEP_SPECTATOR", {"ask": 1}, SPACE_ID)
    assert n["source"]["profileId"] == services.default_player_id()


async def test_game_invites_are_refused_chat_channels_stay(client, services):
    """Invites can't be accepted (Connect's pages are Ubisoft's): refused, so the inviting game reports it."""
    guest = services.accounts.register("guest@example.com", "correct horse", "Guest")
    space = "c0539e9c-90ab-4af8-a2d6-ef7b3883f430"
    host = {**auth_for(services, syn.PLAYER), "Host": "channel-service.upc.ubi.com"}
    dm = await (await client.post(f"/v1/spaces/{space}/channels", headers=host,
                                  json={"channel": {"type": "DM"}, "members": [guest.profile_id]})).json()
    r = await client.post(f"/v1/spaces/{space}/channels/{dm['id']}/messages", headers=host,
                          json={"type": "GAME_INVITE", "textMessage": "e30="})
    assert r.status == 403 and (await r.json())["httpCode"] == 403
    assert services.channels.messages(guest.profile_id, dm["id"]) == []         # nothing delivered


async def test_connect_achievement_view(client, services):
    """Ubisoft Connect's UI asks for achievements over GraphQL (batched): titles, icons and the player's
    unlocks come from the pack and the database."""
    ticket = (await login(client))["ticket"]
    auth = {**UBI, "Authorization": f"Ubi_v1 t={ticket}"}
    batch = [{"operationName": "GetAchievementsCount", "variables": {"spaceId": SPACE_ID, "productId": 3279}},
             {"operationName": "GetAchievements", "variables": {"spaceId": SPACE_ID, "productId": 3279}},
             {"operationName": "GetFriends", "variables": {"spaceId": SPACE_ID}}]
    r = await client.post("/v1/profiles/me/uplay/graphql", headers=auth, json=batch)
    count, full, other = await r.json()
    assert count["data"]["game"]["viewer"]["meta"]["achievements"]["totalCount"] == len(syn.ACHIEVEMENT_NAMES)
    ach = full["data"]["game"]["viewer"]["meta"]["achievements"]
    assert ach["totalCount"] == 4 and ach["completedCount"] == 3              # 1, 4, 15 imported; 30 open
    by_id = {n["achievementId"]: n for n in ach["nodes"]}
    assert by_id[1]["title"] == "First steps" and by_id[1]["viewer"]["meta"]["isCompleted"]
    assert by_id[1]["viewer"]["meta"]["completionDate"] == "2017-07-14T02:40:00.000Z"
    assert not by_id[30]["viewer"]["meta"]["isCompleted"] and by_id[30]["viewer"]["meta"]["completionDate"] is None
    assert other == {"data": {"games": []}}                                   # one result per operation
    icon = await client.get(by_id[1]["icon"].split(syn.UBI, 1)[1], headers=UBI)
    assert icon.status == 200 and await icon.read() == syn.ICON
    # another game: nothing; a single (unbatched) operation gets a single result
    r = await client.post("/v1/profiles/me/uplay/graphql", headers=auth, json={
        "operationName": "GetAchievements", "variables": {"spaceId": "00000000-0000-0000-0000-000000000000"}})
    assert (await r.json())["data"]["game"]["viewer"]["meta"]["achievements"]["nodes"] == []


async def test_no_matchmaking_map(client):
    """An empty list lets the game matchmake without a map (a map entity it can't download disables it)."""
    r = await client.get(f"/v2/spaces/{SPACE_ID}/entities?type=TITLE_MATCHMAKING_MAP&tags=WORLD_10af88fa9e4"
                         "&offset=0&limit=10", headers=UBI)
    assert r.status == 200 and (await r.json())["entities"] == []


async def refused(r, status=401):
    body = await r.json()
    assert r.status == status, body
    if status == 401:
        assert list(body) == ["message", "errorCode", "httpCode", "errorContext", "moreInfo", "transactionTime",
                              "transactionId"]
        assert (body["message"], body["httpCode"]) == ("Not signed in (no valid ticket)", 401)
    return body


async def test_without_a_session_nobody_acts_for_the_default_player(client, services):
    """Once accounts exist, a request without a session is nobody: no default player to read as, overwrite,
    notify or chat as (401); with one, the session's player."""
    services.accounts.register("host@example.com", "correct horse", "")          # takes over syn.PLAYER
    guest = services.accounts.register("guest@example.com", "correct horse", "Guest").profile_id
    got = []

    class FakeWs:
        async def send_json(self, data):
            got.append(data)
    services.sessions.issue(services.accounts.player(guest), "192.0.2.20").websockets.add(FakeWs())
    await refused(await client.put("/v1/profiles/me/populations/data", headers=UBI, json={"populations": [9]}))
    assert services.repos.kv.get(syn.PLAYER, "populations_data") is None
    await refused(await client.post(f"/v2/profiles/{guest}/notifications", headers=UBI,
                                    json={"type": "STEEP_SPECTATOR"}))
    assert got == []                                                              # nothing delivered
    await refused(await client.post("/v1/spaces/s/channels", headers=UBI, json={"members": [guest]}))
    for path in ("/v3/profiles", "/v3/users/me", "/v3/users/me/profiles", "/v2/profiles/me/friends",
                 "/v1/profiles/me/channels", "/v1/profiles/me/populations", "/v1/profiles/me/actions",
                 "/v1/profiles/me/global/ubiconnect/economy/api/units", f"/v3/profiles/{syn.PLAYER}/friends",
                 f"/v1/users/onlineStatuses?UserIds={guest}", f"/v1/profiles/connections?profileIds={guest}",
                 "/ugc/v1/?type_id=2"):
        await refused(await client.get(path, headers=UBI))
    await refused(await client.post("/v1/profiles/me/uplay/graphql", headers=UBI, json=[
        {"operationName": "GetAchievements", "variables": {"spaceId": SPACE_ID}}]))
    await refused(await client.get("/v3/profiles", headers={**UBI, "Authorization": "Ubi_v1 t=made-up"}))
    # game-wide answers need nobody
    assert (await client.get("/v1/profiles/me/news", headers=UBI)).status == 200
    assert (await client.get(f"/v1/profiles/{guest}/global/ubiconnect/economy/api/units", headers=UBI)).status == 200
    assert (await client.get(syn.SKI_PAGE, headers=UBI)).status == 200
    # with a session: that player
    auth = auth_for(services, guest)
    assert (await client.put("/v1/profiles/me/populations/data", headers=auth, json={"populations": [1]})).status == 200
    assert services.repos.kv.get(guest, "populations_data") == {"populations": [1]}
    assert (await (await client.get("/v3/users/me", headers=auth)).json())["nameOnPlatform"] == "Guest"
    assert len((await (await client.get("/v3/profiles", headers=auth)).json())["profiles"]) == 2


async def test_without_accounts_the_default_player_acts(client, services):
    """Without accounts, everything works without a ticket, as the default player."""
    assert (await (await client.get("/v3/users/me", headers=UBI)).json())["userId"] == \
        services.repos.players.get(syn.PLAYER).user_id
    assert (await client.get("/v3/profiles", headers=UBI)).status == 200
    assert (await client.put("/v1/profiles/me/populations/data", headers=UBI, json={})).status == 200
    card = await (await client.post("/v2/profiles/entities", headers=UBI, json={
        "type": "FAVORITE_REPLAY", "extendedStorage": {"provider": "s3"}})).json()
    assert card["profileId"] == syn.PLAYER and "?t=" not in card["extendedStorage"]["putUrl"]   # unsigned
    r = await client.post("/v2/profiles/not-a-profile-id/entities", headers=UBI, json={"type": "X"})
    assert r.status == 400 and (await r.json())["message"] == "bad profile id"


async def test_accounts_keep_players_apart(client, services):
    """Once accounts exist: no default player for requests without a session, and a player writes only its own
    inventory and entities (reads of others' stay allowed)."""
    victim = services.accounts.register("host@example.com", "correct horse", "").profile_id   # the default player
    attacker = services.accounts.register("guest@example.com", "correct horse", "Guest").profile_id
    mine, theirs = auth_for(services, attacker), auth_for(services, victim)
    offer = {"spaceId": SPACE_ID, "offerId": syn.SKI_A, "offerRevision": 6}
    inv = f"/v1/profiles/{victim}/inventory"
    # inventory: no ticket -> 401 (was: the default player); someone else's -> 403; unknown ids get no record
    body = await refused(await client.post(inv, headers=UBI, json=offer))
    assert body["errorContext"] == "Profiles Client Facade Service"
    r = await client.post(inv, headers=mine, json=offer)
    assert r.status == 403 and (await r.json()) == {"errorCode": 1, "message": "not your inventory", "httpCode": 403}
    assert (await client.post(f"/v1/profiles/{syn.FRIEND}/inventory", headers=mine, json=offer)).status == 403
    assert services.repos.players.get(syn.FRIEND) is None                         # no player record for it
    assert (await client.post(inv, headers=theirs, json=offer)).status == 200       # one's own: fine
    assert (await client.get(inv, headers=mine)).status == 200                     # reading another's: fine
    await refused(await client.get(inv, headers=UBI))

    # entities: the victim's card can be read, not changed, deleted or made in the victim's name
    card = await (await client.post(f"/v2/profiles/{victim}/entities", headers=theirs,
                                    json={"type": "PROFILE", "obj": {"title": 1}})).json()
    path = f"/v2/profiles/entities/{card['entityId']}"
    r = await client.put(path, headers=mine, json={"obj": {"title": 666}})
    assert r.status == 403 and (await r.json())["message"] == "not your entity"
    await refused(await client.put(path, headers=UBI, json={"obj": {}}))
    assert (await client.delete(path, headers=mine)).status == 403
    await refused(await client.delete(path, headers=UBI))
    assert services.entities.get(card["entityId"])["obj"] == {"title": 1}          # untouched
    assert (await (await client.get(path, headers=mine)).json())["obj"] == {"title": 1}
    r = await client.post(f"/v2/profiles/{victim}/entities", headers=mine, json={"type": "X"})
    assert r.status == 403 and (await r.json())["message"] == "not your profile"
    r = await client.post("/v2/profiles/entities", headers=mine, json={"type": "X", "profileId": victim})
    assert r.status == 403                                                         # the body naming another owner
    r = await client.put("/v2/profiles/entities/00000000-0000-4000-8000-0000000000bb", headers=mine,
                         json={"type": "X", "profileId": victim})
    assert r.status == 403 and services.entities.get("00000000-0000-4000-8000-0000000000bb") is None
    await refused(await client.post("/v2/profiles/entities", headers=UBI, json={"type": "X"}))
    # the cloud save entity through /v2 too
    save = services.entities.put("00000000-0000-4000-8000-0000000000cc", {"type": "cloudSave", "obj": {}}, victim)
    assert (await client.put(f"/v2/profiles/entities/{save['entityId']}", headers=mine, json={"obj": {}})).status == 403
    # listings need a session
    batch = f"/v2/profiles/entities?profileIds={victim}"
    await refused(await client.get(batch, headers=UBI))
    assert len((await (await client.get(batch, headers=mine)).json())["entities"]) >= 2
    await refused(await client.get(f"/v2/profiles/{victim}/entities", headers=UBI))
    await refused(await client.get(path, headers=UBI))
    # the own entity works as before
    r = await client.post(f"/v2/profiles/{attacker}/entities", headers=mine, json={"type": "X"})
    assert r.status == 200 and (await r.json())["profileId"] == attacker


async def test_entity_files_are_written_by_their_owner_only(client, services):
    """An entity's file: once accounts are required, writes need the putUrl's token (the game may upload
    without its ticket, as to S3) or the owner's session; downloads stay open."""
    from powder.server.transport.https.routes_ubi import file_token, file_token_ok
    victim = services.accounts.register("host@example.com", "correct horse", "").profile_id
    attacker = services.accounts.register("guest@example.com", "correct horse", "Guest").profile_id
    mine, theirs = auth_for(services, attacker), auth_for(services, victim)
    ghost = await (await client.post(f"/v2/profiles/{victim}/entities", headers=theirs, json={
        "type": "BEST_GHOST_CHALLENGE_PUBLIC_ALLTIME",
        "extendedStorage": {"provider": "s3", "fileExtension": ""}})).json()
    storage = ghost["extendedStorage"]
    put = storage["putUrl"].split(".com", 1)[1]
    plain, token = put.split("?t=")
    assert storage["deleteUrl"] == storage["putUrl"]
    assert storage["getUrl"].split(".com", 1)[1] == plain                          # the read links are not signed
    assert (await client.put(put, headers=UBI, data=b"GHOST")).status == 200        # the link's token
    assert (await client.put(plain, headers=theirs, data=b"GHOST2")).status == 200  # the owner's session
    for path, auth in [(plain, mine), (plain, UBI), (f"{plain}?t=1.{token[-64:]}", UBI),
                       (f"{plain}?t={token[:-1]}", mine)]:
        assert (await client.put(path, headers=auth, data=b"EVIL")).status == 403, path
        assert (await client.delete(path, headers=auth)).status == 403, path
    # another entity's token does not fit
    other = await (await client.post("/v2/profiles/entities", headers=mine, json={
        "type": "FAVORITE_REPLAY", "extendedStorage": {"provider": "s3"}})).json()
    other_token = other["extendedStorage"]["putUrl"].split("?t=")[1]
    assert (await client.put(f"{plain}?t={other_token}", headers=mine, data=b"EVIL")).status == 403
    r = await client.get(plain, headers=UBI)
    assert r.status == 200 and await r.read() == b"GHOST2"                          # downloads: open
    assert file_token_ok(ghost["entityId"], token) and not file_token_ok(ghost["entityId"],
                                                                         file_token(ghost["entityId"], 1))


async def test_cors_only_for_connects_origins(client):
    """Only Ubisoft Connect's origins get CORS (with credentials): a web page elsewhere, which the hosts file
    also sends here, can neither read replies nor send anything but GET / HEAD."""
    from powder.server.transport.https.app import connect_origin
    for ok in ("https://connect.cdn.ubisoft.com", "https://uat-connect.cdn.ubisoft.com", "https://ubisoft.com",
               "https://store.ubi.com", "HTTPS://Connect.CDN.Ubisoft.com:443"):
        assert connect_origin(ok), ok
    for foreign in ("null", "http://connect.cdn.ubisoft.com", "https://evil.com", "https://ubisoft.com.evil.com",
                    "https://evilubisoft.com", "https://evil.com/.ubisoft.com", "https://a@ubisoft.com", "",
                    "file://", "https://ubisoft.com:x"):
        assert not connect_origin(foreign), foreign
    evil = {"Origin": "https://evil.example"}
    pre = await client.options("/v3/profiles/sessions", headers={
        **evil, "Access-Control-Request-Method": "POST", "Access-Control-Request-Headers": "authorization"})
    assert pre.status == 403
    assert "Access-Control-Allow-Origin" not in pre.headers and "Access-Control-Allow-Headers" not in pre.headers
    for method in ("POST", "PUT", "DELETE", "PATCH"):
        r = await client.request(method, "/_powder/account/register", headers=evil, json={})
        assert (r.status, await r.json()) == (403, {"errorCode": 1, "message": "origin not allowed", "httpCode": 403})
    r = await client.get("/v1/nothing", headers={**UBI, **evil})
    assert r.status == 200                                                          # a GET is answered, without CORS
    for name in ("Access-Control-Allow-Origin", "Access-Control-Allow-Credentials", "Access-Control-Expose-Headers"):
        assert name not in r.headers
    r = await client.get("/_powder/overlay/powder.js", headers=evil)
    assert r.headers["Access-Control-Allow-Origin"] == "*"                          # the overlay part's own stays
    assert "Access-Control-Allow-Credentials" not in r.headers


async def test_a_servers_own_page_may_call_it(client):
    own = {"Host": "127.0.0.1:18443", "Origin": "https://127.0.0.1:18443"}
    r = await client.post("/v1/nothing", headers=own)
    assert r.status == 200 and "Access-Control-Allow-Origin" not in r.headers
    other = {"Host": "127.0.0.1:18443", "Origin": "https://127.0.0.1:9999"}
    assert (await client.post("/v1/nothing", headers=other)).status == 403


async def test_status_hides_other_players_addresses(client, services, monkeypatch):
    """GET /_powder/status: the server's own PC sees every address; another PC its own entries, the others'
    without their address; on a public server not at all."""
    import dataclasses
    import types
    from powder.server.transport.https import routes_powder
    services.accounts.register("host@example.com", "correct horse", "")          # takes over syn.PLAYER
    guest = services.accounts.register("guest@example.com", "correct horse", "Guest").profile_id

    class Conn(types.SimpleNamespace):
        __hash__ = object.__hash__
    for name, pid, ip in (("Mine", syn.PLAYER, "192.0.2.10"), ("Guest", guest, "192.0.2.20")):
        services.connect_clients.add(Conn(profile_id=pid, player_name=name, client_ip=ip, peer=f"{ip}:5000",
                                          channels={1: "friends_service"}))
        s = services.sessions.issue(services.accounts.player(pid), ip)
        s.nex = types.SimpleNamespace(addr=(ip, 3074))

    async def status(caller):
        monkeypatch.setattr(routes_powder, "client_ip", lambda request: caller)
        return await (await client.get("/_powder/status", headers=UBI)).json()
    for host_pc in ("127.0.0.1", "::1", "::ffff:127.0.0.1"):
        body = await status(host_pc)
        assert sorted(e["peer"] for e in body["game"]) == ["192.0.2.10:3074", "192.0.2.20:3074"], host_pc
        assert len([e for e in body["connect"] if "peer" in e]) == 2
    body = await status("192.0.2.10")
    assert body["you"] == "192.0.2.10"
    assert sorted(body["game"], key=lambda e: e["player"]) == [
        {"player": "Guest", "nex": True}, {"player": syn.PLAYER_NAME, "nex": True, "peer": "192.0.2.10:3074"}]
    assert sorted(body["connect"], key=lambda e: e["player"]) == [
        {"player": "Guest", "services": ["friends_service"]},
        {"player": "Mine", "peer": "192.0.2.10:5000", "services": ["friends_service"]}]
    services.config = dataclasses.replace(services.config, server=dataclasses.replace(services.config.server,
                                                                                      public=True))
    body = await status("192.0.2.10")
    assert [e["player"] for e in body["game"]] == [syn.PLAYER_NAME]
    assert [e["player"] for e in body["connect"]] == ["Mine"]


async def test_a_refused_websocket_handshake_logs_no_ticket(client, caplog):
    r = await client.get("/v2/websocket", headers={**UBI, "Authorization": "Ubi_v1 t=secret-ticket"})
    assert r.status == 400
    assert "Authorization" in caplog.text and "secret-ticket" not in caplog.text
