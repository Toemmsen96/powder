//! Login sessions and live connections per player.
//!
//! A PlayerSession is created at UbiServices login and carries our own ticket. The game then sends
//! `Authorization: Ubi_v1 t=<ticket>` on HTTPS calls and the same ticket as the NEX LoginWithToken token, so both
//! transports find the same session. The SessionRegistry knows each player's open WebSockets and NEX connection,
//! which is what lets the server push messages to a player.
//!
//! Sessions are shared as `Arc<PlayerSession>`; the registry's maps sit behind one lock, and pushes copy the
//! channel out before sending, so nothing is called (or awaited) while it is held. A WebSocket is an unbounded
//! channel to its connection's task (sending never blocks); a NEX connection is anything implementing NexChannel.

use crate::core::identity::Player;
use crate::util::{iso_now, token_urlsafe, unix_now};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub const SESSION_TTL_S: f64 = 3.0 * 3600.0;

/// A ticket shaped like Ubisoft's (base64 JSON header . payload . MAC), for Ubisoft Connect, in case it reads the
/// header. The payload is random: only this server ever validates tickets.
pub fn ubi_shaped_ticket(app_id: &str, session_id: &str) -> String {
    let header = json!({"ver": "1", "aid": app_id, "env": "Prod", "sid": session_id, "typ": "JWE", "enc": "A128CBC",
                        "iv": token_urlsafe(16), "int": "HS256", "kid": uuid::Uuid::new_v4().to_string()});
    // serde's pretty form is Python's json.dumps(indent=2) for these (ASCII) values
    let header = serde_json::to_string_pretty(&header).expect("plain JSON");
    [crate::util::b64(header.as_bytes()), token_urlsafe(96), token_urlsafe(32)].join(".")
}

/// The game's NEX connection, for server-initiated RMC requests.
pub trait NexChannel: Send + Sync {
    /// -> the call id used.
    fn send_request(&self, protocol: &str, method: &str, params: &[u8], call_id: Option<u32>) -> u32;
}

/// A player's live connections; kept across a re-login (the new session shares them).
#[derive(Default)]
pub struct Channels {
    websockets: Mutex<Vec<(u64, UnboundedSender<Value>)>>,
    nex: Mutex<Option<Arc<dyn NexChannel>>>,
}

static NEXT_WS: AtomicU64 = AtomicU64::new(1);

impl Channels {
    /// Register a WebSocket (its task forwards what arrives on the channel); -> id for `remove_websocket`.
    pub fn add_websocket(&self, tx: UnboundedSender<Value>) -> u64 {
        let id = NEXT_WS.fetch_add(1, Ordering::Relaxed);
        self.websockets.lock().push((id, tx));
        id
    }

    pub fn remove_websocket(&self, id: u64) {
        self.websockets.lock().retain(|(i, _)| *i != id);
    }

    pub fn websocket_count(&self) -> usize {
        self.websockets.lock().len()
    }

    pub fn set_nex(&self, nex: Option<Arc<dyn NexChannel>>) {
        *self.nex.lock() = nex;
    }

    pub fn nex(&self) -> Option<Arc<dyn NexChannel>> {
        self.nex.lock().clone()
    }
}

pub struct PlayerSession {
    pub player: Player,
    pub ticket: String,
    pub session_id: String,
    pub client_ip: String,
    /// Ubisoft Connect's session (not the game's).
    pub connect: bool,
    /// Unix times.
    pub created: f64,
    pub expires: f64,
    pub channels: Arc<Channels>,
    /// Per-session scratch (services, mods).
    pub data: Mutex<Map<String, Value>>,
}

impl PlayerSession {
    pub fn profile_id(&self) -> &str {
        &self.player.profile_id
    }
}

impl std::fmt::Debug for PlayerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayerSession").field("player", &self.player).field("session_id", &self.session_id)
            .field("client_ip", &self.client_ip).field("connect", &self.connect).finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Maps {
    by_ticket: HashMap<String, Arc<PlayerSession>>,
    by_session_id: HashMap<String, Arc<PlayerSession>>,
    /// Most recent session per player.
    by_player: HashMap<String, Arc<PlayerSession>>,
}

#[derive(Default)]
pub struct SessionRegistry {
    maps: Mutex<Maps>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new session (and ticket) for a player. `app_id` (Ubisoft Connect's) makes it a Connect session with a
    /// Ubisoft-shaped ticket; otherwise the ticket is "powder.<random>".
    pub fn issue(&self, player: Player, client_ip: &str, app_id: &str) -> Arc<PlayerSession> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let ticket = if app_id.is_empty() {
            format!("powder.{}", token_urlsafe(48))
        } else {
            ubi_shaped_ticket(app_id, &session_id)
        };
        let now = unix_now();
        let mut m = self.maps.lock();
        // keep live channels across a re-login
        let channels = m.by_player.get(&player.profile_id).map(|old| old.channels.clone()).unwrap_or_default();
        let s = Arc::new(PlayerSession { player, ticket, session_id, client_ip: client_ip.into(),
                                         connect: !app_id.is_empty(), created: now, expires: now + SESSION_TTL_S,
                                         channels, data: Mutex::new(Map::new()) });
        m.by_ticket.insert(s.ticket.clone(), s.clone());
        m.by_session_id.insert(s.session_id.clone(), s.clone());
        m.by_player.insert(s.player.profile_id.clone(), s.clone());
        let expired: Vec<(String, String)> = m.by_ticket.values().filter(|s| s.expires < now)
            .map(|s| (s.ticket.clone(), s.session_id.clone())).collect();
        for (ticket, sid) in expired {
            m.by_ticket.remove(&ticket);
            m.by_session_id.remove(&sid);
        }
        s
    }

    /// The most recent live session from this address (Ubisoft Connect's only, if asked): the game and Ubisoft
    /// Connect run on the same PC, so a game login without a ticket of ours belongs to the player Connect signed
    /// in as there.
    pub fn latest_from(&self, client_ip: &str, connect_only: bool) -> Option<Arc<PlayerSession>> {
        let now = unix_now();
        let m = self.maps.lock();
        m.by_ticket.values().filter(|s| s.client_ip == client_ip && s.expires > now && (s.connect || !connect_only))
            .max_by(|a, b| a.created.total_cmp(&b.created)).cloned()
    }

    /// Forget every session of a player (account deleted or moved to another player).
    pub fn drop_player(&self, profile_id: &str) {
        let mut m = self.maps.lock();
        let gone: Vec<(String, String)> = m.by_ticket.values().filter(|s| s.profile_id() == profile_id)
            .map(|s| (s.ticket.clone(), s.session_id.clone())).collect();
        for (ticket, sid) in gone {
            m.by_ticket.remove(&ticket);
            m.by_session_id.remove(&sid);
        }
        m.by_player.remove(profile_id);
    }

    /// The live session with this ticket.
    pub fn by_token(&self, ticket: &str) -> Option<Arc<PlayerSession>> {
        let m = self.maps.lock();
        m.by_ticket.get(ticket).filter(|s| s.expires > unix_now()).cloned()
    }

    pub fn by_session_id(&self, session_id: &str) -> Option<Arc<PlayerSession>> {
        self.maps.lock().by_session_id.get(session_id).cloned()
    }

    /// The player's most recent session (expired or not).
    pub fn for_player(&self, profile_id: &str) -> Option<Arc<PlayerSession>> {
        self.maps.lock().by_player.get(profile_id).cloned()
    }

    /// Every player's most recent session.
    pub fn players(&self) -> Vec<Arc<PlayerSession>> {
        self.maps.lock().by_player.values().cloned().collect()
    }

    // ---- server push (the seam for notifications and multiplayer) -------------------------------------------
    /// Send a UbiServices notification over the player's WebSocket(s); returns how many got it. `extra`: more
    /// fields of the notification (e.g. its sender). A closed socket is dropped.
    pub fn push_ws(&self, profile_id: &str, notification_type: &str, content: Value, extra: Map<String, Value>)
                   -> usize {
        let Some(s) = self.for_player(profile_id) else { return 0 };
        let mut msg = Map::new();
        msg.insert("notificationType".into(), json!(notification_type));
        msg.insert("content".into(), content);
        msg.insert("datePosted".into(), json!(iso_now()));
        msg.extend(extra);
        let msg = Value::Object(msg);
        let mut sent = 0;
        s.channels.websockets.lock().retain(|(_, tx)| match tx.send(msg.clone()) {
            Ok(()) => {
                sent += 1;
                true
            }
            Err(_) => {
                log::warn!(target: "powder.sessions", "push_ws to {profile_id} failed: websocket closed");
                false
            }
        });
        sent
    }

    /// Send a server-initiated RMC request over the player's NEX connection.
    pub fn push_nex(&self, profile_id: &str, protocol: &str, method: &str, params: &[u8], call_id: Option<u32>)
                    -> bool {
        let Some(nex) = self.for_player(profile_id).and_then(|s| s.channels.nex()) else { return false };
        nex.send_request(protocol, method, params, call_id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(pid: &str) -> Player {
        Player { profile_id: pid.into(), user_id: pid.into(), name: format!("P-{pid}") }
    }

    #[test]
    fn tickets_and_relogin() {
        let reg = SessionRegistry::new();
        let a = reg.issue(player("a"), "10.0.0.1", "");
        assert!(a.ticket.starts_with("powder.") && !a.connect);
        assert_eq!(reg.by_token(&a.ticket).unwrap().session_id, a.session_id);
        assert!(reg.by_token("nope").is_none());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        a.channels.add_websocket(tx);
        // a re-login keeps the websocket; both tickets stay valid
        let a2 = reg.issue(player("a"), "10.0.0.1", "f68a4bb5-608a-4ff2-8123-be8ef797e0a6");
        assert!(a2.connect && a2.ticket.split('.').count() == 3);
        assert!(reg.by_token(&a.ticket).is_some());
        assert_eq!(reg.push_ws("a", "TEST", json!({"x": 1}), Map::new()), 1);
        assert_eq!(rx.try_recv().unwrap()["notificationType"], "TEST");
        assert_eq!(reg.latest_from("10.0.0.1", true).unwrap().session_id, a2.session_id);
        assert!(reg.latest_from("10.0.0.2", false).is_none());
        drop(rx);
        assert_eq!(reg.push_ws("a", "TEST", json!({}), Map::new()), 0);
        assert_eq!(a.channels.websocket_count(), 0);
        reg.drop_player("a");
        assert!(reg.by_token(&a2.ticket).is_none() && reg.for_player("a").is_none());
    }

    #[test]
    fn ubi_shaped_header() {
        let t = ubi_shaped_ticket("app", "sid");
        let header = crate::util::unb64(t.split('.').next().unwrap()).unwrap();
        let header = String::from_utf8(header).unwrap();
        assert!(header.starts_with("{\n  \"ver\": \"1\",\n  \"aid\": \"app\",\n  \"env\": \"Prod\",\n  \"sid\": \"sid\","));
    }
}
