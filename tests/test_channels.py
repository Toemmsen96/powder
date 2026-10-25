"""Ubisoft Connect's chat channels, which carry game invites (services/channels.py). Made-up ids."""
from powder.server.services.channels import ChannelService

A, B = "11111111-1111-1111-1111-111111111111", "22222222-2222-2222-2222-222222222222"
SPACE = "c0539e9c-0000-0000-0000-000000000000"


def make():
    pushed = []
    return ChannelService(lambda pid, kind, content: pushed.append((pid, kind, content))), pushed


def test_a_game_invite_reaches_the_friend():
    channels, pushed = make()
    dm = channels.create(A, SPACE, {"channel": {"type": "DM"}, "members": [B]})          # as Connect sends it
    assert dm["type"] == "DM" and {m["profileId"] for m in dm["memberships"]} == {A, B}
    assert all(m["membershipType"] == "ACTIVE" for m in dm["memberships"])
    assert pushed == [(B, "upc_channel_created", {"channelId": dm["id"], "channel": dm})]
    assert channels.create(A, SPACE, {"channel": {"type": "DM"}, "members": [B.upper()]})["id"] == dm["id"]
    invite = "eyJnYW1lU2Vzc2lvbklkIjoxMjN9"                                           # {"gameSessionId":123}
    msg = channels.post(A, dm["id"], {"type": "GAME_INVITE", "textMessage": invite})
    assert msg["authorId"] == A and msg["type"] == "GAME_INVITE" and msg["textMessage"] == invite
    assert pushed[-1] == (B, "upc_channel_message_created", {"channelId": dm["id"], "message": msg})
    assert channels.of(B, SPACE) == [dm] and dm["lastMessage"] == msg
    assert channels.messages(B, dm["id"]) == [msg]


def test_strangers_see_nothing():
    channels, _ = make()
    dm = channels.create(A, SPACE, {"channel": {"type": "DM"}, "members": [B]})
    other = "33333333-3333-3333-3333-333333333333"
    assert channels.of(other) == [] and channels.messages(other, dm["id"]) is None
    assert channels.post(other, dm["id"], {"type": "TEXT_MESSAGE"}) is None


def test_bounded_members_messages_and_channels():
    from powder.server.services.channels import (MAX_CHANNELS, MAX_CHANNELS_PER_CREATOR, MAX_MEMBERS,
                                                 MAX_MESSAGE_BYTES, MAX_MESSAGES, json_size)
    channels, _ = make()
    many = [f"p{i:04}" for i in range(MAX_MEMBERS + 20)]
    group = channels.create(A, SPACE, {"members": many})
    assert len(group["memberships"]) == MAX_MEMBERS
    assert group["memberships"][0]["profileId"] == A, "the creator is always in"
    # messages: the newest MAX_MESSAGES, and no more than MAX_MESSAGE_BYTES of them
    dm = channels.create(A, SPACE, {"channel": {"type": "DM"}, "members": [B]})
    for i in range(MAX_MESSAGES + 5):
        channels.post(A, dm["id"], {"type": "TEXT_MESSAGE", "n": i})
    kept = channels.messages(B, dm["id"])
    assert (len(kept), kept[0]["n"], kept[-1]["n"]) == (MAX_MESSAGES, MAX_MESSAGES + 4, 5)
    for _ in range(10):
        channels.post(B, dm["id"], {"type": "TEXT_MESSAGE", "text": "x" * (10 << 10)})
    kept = channels.messages(A, dm["id"])
    assert len(kept) < 10 and sum(map(json_size, kept)) <= MAX_MESSAGE_BYTES
    channels.post(B, dm["id"], {"text": "y" * MAX_MESSAGE_BYTES})
    assert len(channels.messages(A, dm["id"])) == 1, "a single message over the size stays alone"
    # channels: a player's oldest of theirs goes when they create one too many
    first = channels.create(B, SPACE, {"members": ["c0", "c1"]})
    for i in range(1, MAX_CHANNELS_PER_CREATOR):
        channels.create(B, SPACE, {"members": [f"c{i}", "x"]})
    assert any(c["id"] == first["id"] for c in channels.of(B))
    channels.create(B, SPACE, {"members": ["new", "x"]})
    of_b = channels.of(B)
    assert not any(c["id"] == first["id"] for c in of_b), "B's oldest forgotten"
    assert any(c["id"] == dm["id"] for c in of_b), "A's DM with B stays"
    # and the server's oldest when all are taken
    for i in range(MAX_CHANNELS - len(channels.channels)):
        channels.create(f"{i:08}-0000-0000-0000-000000000000", SPACE, {"members": [A]})
    assert len(channels.channels) == MAX_CHANNELS
    channels.create(A, SPACE, {"members": ["one", "more"]})
    assert len(channels.channels) == MAX_CHANNELS
    assert channels.messages(A, dm["id"]) is not None and channels.messages(A, group["id"]) is None
