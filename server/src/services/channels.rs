//! Ubisoft Connect's chat channels (channel-service.upc.ubi.com), which also carry game invites.
//!
//! When Steep invites a friend, Ubisoft Connect gets (or creates) the direct-message channel with that friend and
//! posts a GAME_INVITE message into it; the friend's Connect learns about both through its notification websocket
//! (upc_channel_created, upc_channel_message_created) and hands an accepted invite to its game.
//!
//! What Connect sends (seen live):
//!   create   POST /v1/spaces/{space}/channels {"channel": {"type": "DM"}, "members": [friend's profile id]}
//!   invite   POST .../channels/{id}/messages {"type": "GAME_INVITE", "textMessage": base64 JSON {multiplayerId,
//!            gameSessionId (the inviter's group session on the game server), isJoinable, productName, userName,
//!            spaceId, ...}}
//! What it gets back, from Connect's own UI code (no recordings): a channel has id, type (DM / GROUP),
//! memberships [{profileId, membershipType ACTIVE / PENDING, addedBy}], lastMessage, lastMessageTime,
//! unreadCount; a message id, channelId, authorId, type, createTime (plus what Connect sent).
//!
//! In memory: channels last until the server stops (invites are only useful while both are playing).

use crate::util::iso_now;
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

pub const DM: &str = "DM";
pub const GROUP: &str = "GROUP";

/// (profile id, notification type, content): a notification over the player's websocket(s).
pub type Push = Arc<dyn Fn(&str, &str, Value) + Send + Sync>;

struct Channel {
    /// The channel object as Connect gets it.
    data: Map<String, Value>,
    messages: Vec<Value>,
}

impl Channel {
    fn id(&self) -> &str {
        self.data.get("id").and_then(Value::as_str).unwrap_or("")
    }

    fn members(&self) -> Vec<String> {
        let memberships = self.data.get("memberships").and_then(Value::as_array);
        memberships.into_iter().flatten()
            .map(|m| m.get("profileId").and_then(Value::as_str).unwrap_or("").to_lowercase()).collect()
    }
}

/// Python's truthiness, for `x or default`.
fn truthy(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    })
}

pub struct ChannelService {
    push: Push,
    /// In creation order.
    channels: Mutex<Vec<Channel>>,
}

impl ChannelService {
    pub fn new(push: Push) -> Self {
        ChannelService { push, channels: Mutex::new(vec![]) }
    }

    /// The channels a player is a member of (of this space, or of none).
    pub fn of(&self, profile_id: &str, space_id: Option<&str>) -> Vec<Value> {
        let pid = profile_id.to_lowercase();
        let in_space = |c: &Channel| match (space_id, c.data.get("spaceId")) {
            (None | Some(""), _) | (_, None) => true,
            (Some(want), Some(v)) => v.as_str() == Some(want),
        };
        self.channels.lock().iter().filter(|c| c.members().contains(&pid) && in_space(c))
            .map(|c| Value::Object(c.data.clone())).collect()
    }

    /// POST /v1/spaces/{space}/channels: an existing DM with the same members is returned instead.
    pub fn create(&self, profile_id: &str, space_id: &str, body: &Value) -> Value {
        let me = profile_id.to_lowercase();
        let mut wanted: BTreeSet<String> = body.get("members").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_lowercase).collect();
        wanted.insert(me.clone());
        let requested = body.get("channel").and_then(Value::as_object).cloned().unwrap_or_default();
        let ctype = truthy(requested.get("type")).cloned()
            .unwrap_or_else(|| json!(if wanted.len() == 2 { DM } else { GROUP }));
        let others: Vec<String> = wanted.iter().filter(|p| **p != me).cloned().collect();
        let data = {
            let mut channels = self.channels.lock();
            let existing = channels.iter().find(|c| {
                c.data.get("type").and_then(Value::as_str) == Some(DM) && ctype == DM
                    && c.members().into_iter().collect::<BTreeSet<_>>() == wanted
            });
            if let Some(c) = existing {
                return Value::Object(c.data.clone());
            }
            let now = iso_now();
            let memberships: Vec<Value> = std::iter::once(&me).chain(&others)
                .map(|p| json!({"profileId": p, "membershipType": "ACTIVE", "addedBy": me, "joinTime": now}))
                .collect();
            let mut data = requested;
            for (k, v) in [("id", json!(uuid::Uuid::new_v4().to_string())), ("type", ctype.clone()),
                           ("spaceId", json!(space_id)), ("memberships", json!(memberships)),
                           ("createTime", json!(now)), ("updateTime", json!(now)), ("lastMessage", Value::Null),
                           ("lastMessageTime", json!(now)), ("unreadCount", json!(0))] {
                data.insert(k.into(), v);
            }
            channels.push(Channel { data: data.clone(), messages: vec![] });
            data
        };
        let id = data["id"].as_str().unwrap_or("").to_string();
        log::info!(target: "powder.channels", "channel {id} ({}) created by {me} for {others:?}",
                   ctype.as_str().unwrap_or(""));
        let data = Value::Object(data);
        for p in &others {
            (self.push)(p, "upc_channel_created", json!({"channelId": id, "channel": data}));
        }
        data
    }

    /// POST .../channels/{id}/messages (a GAME_INVITE when a game invites); None: no such channel of theirs.
    pub fn post(&self, profile_id: &str, channel_id: &str, body: &Value) -> Option<Value> {
        let me = profile_id.to_lowercase();
        let (message, members) = {
            let mut channels = self.channels.lock();
            let channel = channels.iter_mut().find(|c| c.id() == channel_id)?;
            let members = channel.members();
            if !members.contains(&me) {
                return None;
            }
            let now = iso_now();
            let mut message = body.as_object().cloned().unwrap_or_default();
            let kind = truthy(body.get("type")).cloned().unwrap_or_else(|| json!("TEXT_MESSAGE"));
            for (k, v) in [("id", json!(uuid::Uuid::new_v4().to_string())), ("channelId", json!(channel_id)),
                           ("authorId", json!(me)), ("type", kind), ("createTime", json!(now))] {
                message.insert(k.into(), v);
            }
            let message = Value::Object(message);
            channel.messages.insert(0, message.clone());
            channel.data.insert("lastMessage".into(), message.clone());
            channel.data.insert("lastMessageTime".into(), json!(now));
            (message, members)
        };
        log::info!(target: "powder.channels", "channel {channel_id}: {} message from {me}",
                   message["type"].as_str().unwrap_or(""));
        for p in members.iter().filter(|p| **p != me) {
            (self.push)(p, "upc_channel_message_created", json!({"channelId": channel_id, "message": message}));
        }
        Some(message)
    }

    /// A channel's messages, newest first; None: no such channel of theirs.
    pub fn messages(&self, profile_id: &str, channel_id: &str) -> Option<Vec<Value>> {
        let channels = self.channels.lock();
        let channel = channels.iter().find(|c| c.id() == channel_id)?;
        channel.members().contains(&profile_id.to_lowercase()).then(|| channel.messages.clone())
    }
}

#[cfg(test)]
mod tests {
    //! The Python tests/test_channels.py. Made-up ids.
    use super::*;

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const SPACE: &str = "c0539e9c-0000-0000-0000-000000000000";

    type Pushed = Arc<Mutex<Vec<(String, String, Value)>>>;

    fn make() -> (ChannelService, Pushed) {
        let pushed: Pushed = Arc::default();
        let log = pushed.clone();
        (ChannelService::new(Arc::new(move |p, k, c| log.lock().push((p.into(), k.into(), c)))), pushed)
    }

    #[test]
    fn a_game_invite_reaches_the_friend() {
        let (channels, pushed) = make();
        let dm = channels.create(A, SPACE, &json!({"channel": {"type": "DM"}, "members": [B]})); // as Connect sends it
        assert_eq!(dm["type"], "DM");
        let members: BTreeSet<&str> = dm["memberships"].as_array().unwrap().iter()
            .map(|m| m["profileId"].as_str().unwrap()).collect();
        assert_eq!(members, BTreeSet::from([A, B]));
        assert!(dm["memberships"].as_array().unwrap().iter().all(|m| m["membershipType"] == "ACTIVE"));
        assert_eq!(*pushed.lock(), [(B.to_string(), "upc_channel_created".to_string(),
                                     json!({"channelId": dm["id"], "channel": dm}))]);
        let again = channels.create(A, SPACE, &json!({"channel": {"type": "DM"}, "members": [B.to_uppercase()]}));
        assert_eq!(again["id"], dm["id"]);
        let invite = "eyJnYW1lU2Vzc2lvbklkIjoxMjN9"; // {"gameSessionId":123}
        let msg = channels.post(A, dm["id"].as_str().unwrap(), &json!({"type": "GAME_INVITE", "textMessage": invite}))
            .unwrap();
        assert_eq!((msg["authorId"].as_str(), msg["type"].as_str(), msg["textMessage"].as_str()),
                   (Some(A), Some("GAME_INVITE"), Some(invite)));
        assert_eq!(pushed.lock().last().unwrap(), &(B.to_string(), "upc_channel_message_created".to_string(),
                                                     json!({"channelId": dm["id"], "message": msg})));
        let of_b = channels.of(B, Some(SPACE));
        assert_eq!(of_b.len(), 1);
        assert_eq!((&of_b[0]["id"], &of_b[0]["lastMessage"]), (&dm["id"], &msg));
        assert_eq!(channels.messages(B, dm["id"].as_str().unwrap()).unwrap(), [msg]);
    }

    #[test]
    fn strangers_see_nothing() {
        let (channels, _) = make();
        let dm = channels.create(A, SPACE, &json!({"channel": {"type": "DM"}, "members": [B]}));
        let id = dm["id"].as_str().unwrap();
        let other = "33333333-3333-3333-3333-333333333333";
        assert!(channels.of(other, None).is_empty() && channels.messages(other, id).is_none());
        assert!(channels.post(other, id, &json!({"type": "TEXT_MESSAGE"})).is_none());
    }

    #[test]
    fn key_order_is_pythons() {
        let (channels, _) = make();
        let dm = channels.create(A, SPACE, &json!({"channel": {"type": "DM", "name": "x"}, "members": [B]}));
        let keys: Vec<&str> = dm.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["type", "name", "id", "spaceId", "memberships", "createTime", "updateTime", "lastMessage",
                          "lastMessageTime", "unreadCount"]);
        let group = channels.create(A, SPACE, &json!({"members": [B, "33333333-3333-3333-3333-333333333333"]}));
        assert_eq!(group["type"], "GROUP");
    }
}
