//! Matchmaking (NEX MatchmakingProtocol): puts players into one session, so their games meet peer-to-peer.
//!
//! How Steep uses it (from recordings of Ubisoft's server, confirmed live with two PCs):
//!   1. Every game reserves a session id and creates its own small session (type 1, its group), with its station
//!      URLs (where other games reach it).
//!   2. It opens a match ticket (type 2 = the shared world) with search criteria (world_id, worldcell, ...) and
//!      refreshes it every ~10 s (UpdateTicket); a new ticket whenever its player rides into another cell.
//!   3. Once two tickets match, the server makes a type-2 session and picks a host: the host gets code 1, asks
//!      GetSession, then reports its station URLs (ReportHostSuccess). Every other member then gets code 2 with the
//!      host's URLs, connects to the host and reports ReportJoinSuccess (or ReportConnectionFailure, then a new
//!      ticket excluding that session).
//!   4. A ticket that matches an existing session (with its host's URLs known) joins it right away (code 2).
//!   5. When the host leaves, another member is asked to host (code 12); it answers MigrateHost with its URLs and
//!      everyone gets code 10 with the new host.
//!
//! A ticket is used up by a match; the game opens a new one after LeaveSession.
//!
//! Policy here: players meet when their tickets name the same world_id and worldcell (a game leaves a session of
//! another cell right after GetSession), at most `max_size` players per session (what the ticket asks, 32). The
//! game only acts on code 1 while it is searching (its SessionsFlowMachine is in InMatchmaking.SearchingSession),
//! so a code 1 or 2 without an answer is sent again; after MAX_ASKS tries the session is given up and its members'
//! tickets go back into the pool.
//!
//! Transport-free: notifications go through `notify(profile_id, code, params)`, which [`super::Services`] wires to
//! NEX pushes. They are sent after the state lock is released. Criteria and attributes keep the variants the game
//! sent and compare as Python values do (1 == 1.0 == True, a missing key == None).

use crate::nex::rmc::Variant;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Session types: the player's own group, the shared world.
pub const SESSION_GROUP: u32 = 1;
pub const SESSION_WORLD: u32 = 2;
/// Member states (GetSession).
pub const PENDING: u32 = 1;
pub const JOINED: u32 = 2;
pub const CODE_HOST: i64 = 1;
pub const CODE_JOIN: i64 = 2;
/// Answers to JoinSession / JoinPlayer (recorded; the game's onDirectJoinSucceed / onDirectJoinFailed, then it
/// retries).
pub const CODE_DIRECT_JOIN: i64 = 4;
pub const CODE_DIRECT_JOIN_FAILED: i64 = 5;
pub const CODE_MIGRATE_DONE: i64 = 10;
pub const CODE_BECOME_HOST: i64 = 12;
/// Code 1 / 2 again when the game did not react within this time.
pub const RESEND_AFTER_S: f64 = 15.0;
/// Tries per code 1 / 2; then the session is given up / the member left alone.
pub const MAX_ASKS: u32 = 3;
const ATTR_ORDER: [&str; 6] = ["worldcell", "language", "country", "connectivity_nat", "version", "world_id"];

/// (profile id, code, params): a "Matchmaking" notification to the player's game.
pub type Notify = Arc<dyn Fn(&str, i64, Map<String, Value>) + Send + Sync>;
/// Monotonic seconds.
pub type Clock = Box<dyn Fn() -> f64 + Send + Sync>;

/// Python's `==` between two plain values (numbers across int / float / bool).
fn py_eq(a: &Variant, b: &Variant) -> bool {
    match (a, b) {
        (Variant::String(x), Variant::String(y)) => x == y,
        (Variant::None, Variant::None) => true,
        (Variant::Double(_), _) | (_, Variant::Double(_)) => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
        _ => matches!((a.as_i128(), b.as_i128()), (Some(x), Some(y)) if x == y),
    }
}

/// Python's `int(v)` (None: it would raise).
pub fn py_int(v: &Variant) -> Option<i64> {
    match v {
        Variant::Double(f) if f.is_finite() => Some(f.trunc() as i64),
        Variant::String(s) => s.trim().parse().ok(),
        other => other.as_i128().map(|i| i as i64),
    }
}

fn get<'a>(map: &'a [(String, Variant)], key: &str) -> &'a Variant {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(&Variant::None)
}

/// dict.update: existing keys keep their position.
fn update(map: &mut Vec<(String, Variant)>, key: &str, value: Variant) {
    match map.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => map.push((key.into(), value)),
    }
}

fn same_place(a: &(Variant, Variant), b: &(Variant, Variant)) -> bool {
    py_eq(&a.0, &b.0) && py_eq(&a.1, &b.1)
}

#[derive(Clone, Debug)]
pub struct Ticket {
    /// Increasing: a lower id is an older ticket.
    pub id: u64,
    pub profile_id: String,
    pub own_session: u32,
    pub session_type: u32,
    pub criteria: Vec<(String, Variant)>,
    pub excluded: BTreeSet<u32>,
    pub max_size: u32,
    pub created: DateTime<Utc>,
}

impl Ticket {
    /// Where the player is: world and world cell. Only tickets of one place are matched.
    pub fn place(&self) -> (Variant, Variant) {
        (get(&self.criteria, "world_id").clone(), get(&self.criteria, "worldcell").clone())
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub profile_id: String,
    pub state: u32,
    pub urls: Vec<String>,
    /// The ticket that brought the member in (world sessions).
    pub ticket: Option<Ticket>,
    /// Code 1 / 2 last sent, while no answer came (clock).
    pub asked_at: Option<f64>,
    /// How often.
    pub asked: u32,
}

impl Member {
    fn new(profile_id: &str, state: u32) -> Member {
        Member { profile_id: profile_id.into(), state, urls: vec![], ticket: None, asked_at: None, asked: 0 }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: u32,
    pub kind: u32,
    /// Profile id, None until the host reported its URLs.
    pub host: Option<String>,
    pub attrs: Vec<(String, Variant)>,
    /// In joining order.
    pub members: Vec<Member>,
    pub max_size: u32,
    pub created: DateTime<Utc>,
    pub host_urls: Vec<String>,
    /// Asked to host (code 1 / 12), not confirmed yet.
    pub host_candidate: Option<String>,
}

impl Session {
    pub fn free(&self) -> u32 {
        self.max_size.saturating_sub(self.members.len() as u32)
    }

    pub fn place(&self) -> (Variant, Variant) {
        (get(&self.attrs, "world_id").clone(), get(&self.attrs, "worldcell").clone())
    }

    pub fn member(&self, profile_id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.profile_id == profile_id)
    }

    fn member_mut(&mut self, profile_id: &str) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| m.profile_id == profile_id)
    }

    pub fn has(&self, profile_id: &str) -> bool {
        self.member(profile_id).is_some()
    }
}

/// Station URLs other PCs can use: loopback addresses dropped (unless that is all there is).
pub fn reachable_urls(urls: &[String]) -> Vec<String> {
    let loopback = |url: &str| {
        let body = url.split_once(":/").map_or(url, |(_, b)| b);
        let body = body.split('#').next().unwrap_or("");
        body.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == "address").then(|| v.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
        }).unwrap_or(false)
    };
    let good: Vec<String> = urls.iter().filter(|u| !loopback(u)).cloned().collect();
    if good.is_empty() { urls.to_vec() } else { good }
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
}

fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn text(v: &Variant) -> String {
    v.to_text()
}

fn opt(v: &Option<String>) -> Value {
    v.as_ref().map_or(Value::Null, |s| json!(s))
}

fn dumps(v: Value) -> String {
    crate::util::dumps(&v)
}

struct State {
    next_session_id: u32,
    next_ticket_id: u64,
    /// profile id -> last reserved session id
    reserved: HashMap<String, u32>,
    /// In creation order (Python's dict).
    sessions: Vec<Session>,
    tickets: BTreeMap<u64, Ticket>,
}

/// One operation on the state: the notifications it makes go out once the lock is released.
struct Op<'a> {
    st: &'a mut State,
    now: f64,
    out: Vec<(String, i64, Map<String, Value>)>,
}

pub struct MatchmakingService {
    state: Mutex<State>,
    notify: Notify,
    clock: Clock,
    resend_after: f64,
}

impl MatchmakingService {
    pub fn new(notify: Notify) -> Self {
        Self::with_clock(notify, Box::new(monotonic), RESEND_AFTER_S)
    }

    pub fn with_clock(notify: Notify, clock: Clock, resend_after: f64) -> Self {
        let state = State { next_session_id: 0x196001, next_ticket_id: 0x7D0000, reserved: HashMap::new(),
                            sessions: vec![], tickets: BTreeMap::new() };
        MatchmakingService { state: Mutex::new(state), notify, clock, resend_after }
    }

    fn op<R>(&self, f: impl FnOnce(&mut Op) -> R) -> R {
        let now = (self.clock)();
        let (result, out) = {
            let mut st = self.state.lock();
            let mut op = Op { st: &mut st, now, out: vec![] };
            let result = f(&mut op);
            (result, op.out)
        };
        for (pid, code, params) in out {
            (self.notify)(&pid, code, params);
        }
        result
    }

    // ---- the player's own session (type 1) ----

    pub fn reserve_session_id(&self, profile_id: &str) -> u32 {
        self.op(|op| op.reserve_session_id(profile_id))
    }

    pub fn create_session_with_reserved_id(&self, profile_id: &str, sid: u32, session_type: u32,
                                           attrs: Vec<(String, Variant)>, urls: Vec<String>, max_size: u32) -> u32 {
        self.op(|op| {
            let sid = match sid {
                0 => match op.st.reserved.get(profile_id) {
                    Some(&r) if r != 0 => r,
                    _ => op.reserve_session_id(profile_id),
                },
                sid => sid,
            };
            let mut s = Session { id: sid, kind: session_type, host: Some(profile_id.into()), attrs, members: vec![],
                                  max_size, created: Utc::now(), host_urls: urls.clone(), host_candidate: None };
            s.members.push(Member { urls, ..Member::new(profile_id, JOINED) });
            match op.st.sessions.iter_mut().find(|x| x.id == sid) {
                Some(slot) => *slot = s,
                None => op.st.sessions.push(s),
            }
            sid
        })
    }

    /// CreateSessionWithReservedId with short params: the player's group of 4, nothing known.
    pub fn create_default_session(&self, profile_id: &str) -> u32 {
        self.create_session_with_reserved_id(profile_id, 0, SESSION_GROUP, vec![], vec![], 4)
    }

    // ---- tickets ----

    pub fn create_match_ticket(&self, profile_id: &str, own_session: u32, session_type: u32,
                               criteria: Vec<(String, Variant)>, excluded: &[u32], max_size: u32) -> u64 {
        self.op(|op| {
            let id = op.st.next_ticket_id;
            op.st.next_ticket_id += 0x1234;
            let t = Ticket { id, profile_id: profile_id.into(), own_session, session_type, criteria,
                             excluded: excluded.iter().copied().collect(), max_size, created: Utc::now() };
            if session_type == SESSION_WORLD {
                // one world session per player: a new search ends the old
                for sid in op.sessions_of(profile_id, |s| s.kind == SESSION_WORLD) {
                    op.leave(profile_id, sid);
                }
            }
            let (world, cell) = t.place();
            let excl = if t.excluded.is_empty() { String::new() }
                       else { format!(", excluding sessions {:?}", t.excluded.iter().collect::<Vec<_>>()) };
            log::info!(target: "powder.matchmaking", "ticket {id} from {profile_id}: world {}, cell {}{excl}",
                       text(&world), text(&cell));
            op.st.tickets.insert(id, t);
            op.match_ticket(id);
            id
        })
    }

    /// CreateMatchTicket with short params: a world ticket without criteria.
    pub fn create_default_ticket(&self, profile_id: &str) -> u64 {
        self.create_match_ticket(profile_id, 0, SESSION_WORLD, vec![], &[], 32)
    }

    pub fn update_ticket(&self, profile_id: &str, ticket_id: u64, criteria: Vec<(String, Variant)>) {
        self.op(|op| {
            let Some(t) = op.st.tickets.get_mut(&ticket_id).filter(|t| t.profile_id == profile_id) else { return };
            let before = t.place();
            for (k, v) in criteria {
                update(&mut t.criteria, &k, v);
            }
            let after = t.place();
            if !same_place(&before, &after) {
                log::info!(target: "powder.matchmaking", "ticket {ticket_id} from {profile_id}: now world {}, cell {}",
                           text(&after.0), text(&after.1));
            }
            op.match_ticket(ticket_id);
        })
    }

    pub fn cancel_all_tickets(&self, profile_id: &str) {
        self.state.lock().tickets.retain(|_, t| t.profile_id != profile_id);
    }

    // ---- reports from the games ----

    pub fn report_host_success(&self, profile_id: &str, sid: u32, urls: Vec<String>) {
        self.op(|op| op.report_host_success(profile_id, sid, urls))
    }

    pub fn report_join_success(&self, profile_id: &str, sid: u32) {
        let mut st = self.state.lock();
        let Some(s) = st.sessions.iter_mut().find(|s| s.id == sid) else { return };
        let host = s.host.clone().unwrap_or_else(|| "None".into());
        if let Some(m) = s.member_mut(profile_id) {
            (m.state, m.asked_at) = (JOINED, None);
            log::info!(target: "powder.matchmaking", "session {sid}: {profile_id} connected to host {host}");
        }
    }

    pub fn report_connection_failure(&self, profile_id: &str, sid: u32, reason: Option<u32>) {
        self.op(|op| {
            let host = match op.st.sessions.iter().find(|s| s.id == sid) {
                Some(s) => s.host.clone().unwrap_or_else(|| "None".into()),
                None => "?".into(),
            };
            let reason = reason.map_or("None".into(), |r| r.to_string());
            log::warn!(target: "powder.matchmaking", "session {sid}: {profile_id} could NOT connect to host {host} \
                                                      (reason {reason})");
            op.leave(profile_id, sid);
        })
    }

    /// JoinSession, e.g. accepting a friend's invite (the session id came in the invite): code 4 with the host's
    /// URLs, or code 5 when the session is gone, full or has no host yet (the game then gives up or asks again).
    pub fn join_session(&self, profile_id: &str, sid: u32) {
        self.op(|op| {
            let s = op.session(sid);
            if s.is_some_and(|s| s.host.is_some() && !s.host_urls.is_empty() && (s.has(profile_id) || s.free() > 0)) {
                op.join_directly(profile_id, sid);
                return;
            }
            log::warn!(target: "powder.matchmaking", "JoinSession from {profile_id}: session {sid} {}",
                       if s.is_none() { "does not exist" } else { "is full or has no host" });
            let kind = s.map_or(SESSION_GROUP, |s| s.kind);
            op.notify(profile_id, CODE_DIRECT_JOIN_FAILED, json!({"session_id": sid, "session_type": kind}));
        })
    }

    /// RemoveMembers: the host takes players out of its session.
    pub fn remove_members(&self, profile_id: &str, sid: u32, members: &[String]) {
        self.op(|op| {
            if op.session(sid).is_none_or(|s| s.host.as_deref() != Some(profile_id)) {
                return;
            }
            for pid in members.iter().map(|p| p.to_lowercase()) {
                if pid != profile_id {
                    op.leave(&pid, sid);
                }
            }
        })
    }

    /// JoinPlayer ("Join group" on a friend): into the friend's group session (type 1, the one it hosts), then
    /// code 4 with its host's URLs. False: the friend has no such session.
    pub fn join_player(&self, profile_id: &str, target: &str) -> bool {
        let target = target.to_lowercase();
        self.op(|op| {
            let s = op.st.sessions.iter().find(|s| s.kind == SESSION_GROUP && s.host.as_deref() == Some(&target)
                                                   && !s.host_urls.is_empty());
            let Some(s) = s.filter(|s| s.has(profile_id) || s.free() > 0) else {
                log::warn!(target: "powder.matchmaking", "JoinPlayer from {profile_id}: {target} has no group session \
                                                          to join{}", if s.is_none() { "" } else { " (full)" });
                return false;
            };
            let sid = s.id;
            log::info!(target: "powder.matchmaking", "session {sid}: {profile_id} joins {target}'s group");
            op.join_directly(profile_id, sid);
            true
        })
    }

    /// UpdateSession: the host changes its session's attributes (integers only).
    pub fn update_session(&self, profile_id: &str, sid: u32, attrs: Vec<(String, Variant)>) {
        let mut st = self.state.lock();
        let Some(s) = st.sessions.iter_mut().find(|s| s.id == sid && s.host.as_deref() == Some(profile_id)) else {
            return;
        };
        for (k, v) in attrs {
            if matches!(v, Variant::Int(_) | Variant::UInt(_) | Variant::DateTime(_) | Variant::Bool(_)) {
                update(&mut s.attrs, &k, Variant::Int(py_int(&v).unwrap_or_default()));
            }
        }
    }

    pub fn migrate_host(&self, profile_id: &str, sid: u32, urls: Vec<String>) {
        self.op(|op| {
            let Some(s) = op.session_mut(sid).filter(|s| s.has(profile_id)) else { return };
            (s.host, s.host_urls, s.host_candidate) = (Some(profile_id.into()), urls.clone(), None);
            s.member_mut(profile_id).expect("checked above").urls = urls;
            let members: Vec<String> = s.members.iter().map(|m| m.profile_id.clone()).collect();
            log::info!(target: "powder.matchmaking", "session {sid}: {profile_id} is the new host");
            for pid in members {
                op.notify(&pid, CODE_MIGRATE_DONE, json!({"session_id": sid, "host_profile_id": profile_id}));
            }
        })
    }

    pub fn leave(&self, profile_id: &str, sid: u32) {
        self.op(|op| op.leave(profile_id, sid))
    }

    /// The player's game is gone (NEX connection closed): out of every session, tickets cancelled.
    pub fn on_disconnect(&self, profile_id: &str) {
        self.cancel_all_tickets(profile_id);
        self.op(|op| {
            for sid in op.sessions_of(profile_id, |_| true) {
                op.leave(profile_id, sid);
            }
            op.st.reserved.remove(profile_id);
        })
    }

    /// Every second (NEX server timers): ask again where a game did not react to code 1 or 2.
    pub fn tick(&self) {
        let resend_after = self.resend_after;
        self.op(|op| {
            let sids: Vec<u32> = op.st.sessions.iter().filter(|s| s.kind == SESSION_WORLD).map(|s| s.id).collect();
            for sid in sids {
                let Some(s) = op.session(sid) else { continue };
                let pids: Vec<String> = s.members.iter().map(|m| m.profile_id.clone()).collect();
                for pid in pids {
                    let Some(s) = op.session(sid) else { break };
                    let Some(m) = s.member(&pid) else { continue };
                    if m.asked_at.is_none_or(|at| op.now - at < resend_after) {
                        continue;
                    }
                    if s.host_candidate.as_deref() == Some(&pid) && s.host.is_none() {
                        if m.asked < MAX_ASKS {
                            op.ask_host(sid, &pid);
                        } else {
                            op.give_up(sid);
                            break;
                        }
                    } else if s.host.is_some() && m.state == PENDING {
                        if m.asked < MAX_ASKS {
                            op.send_join(sid, &pid);
                        } else {
                            log::warn!(target: "powder.matchmaking", "session {sid}: {pid} never reported joining");
                            op.session_mut(sid).and_then(|s| s.member_mut(&pid)).expect("checked above").asked_at = None;
                        }
                    }
                }
            }
        })
    }

    // ---- reading ----

    pub fn get_session(&self, sid: u32) -> Option<Session> {
        self.state.lock().sessions.iter().find(|s| s.id == sid).cloned()
    }

    /// The tickets waiting for a match, by id.
    pub fn tickets(&self) -> Vec<Ticket> {
        self.state.lock().tickets.values().cloned().collect()
    }
}

impl Op<'_> {
    fn notify(&mut self, profile_id: &str, code: i64, params: Value) {
        let Value::Object(params) = params else { unreachable!("params are an object") };
        self.out.push((profile_id.into(), code, params));
    }

    fn session(&self, sid: u32) -> Option<&Session> {
        self.st.sessions.iter().find(|s| s.id == sid)
    }

    fn session_mut(&mut self, sid: u32) -> Option<&mut Session> {
        self.st.sessions.iter_mut().find(|s| s.id == sid)
    }

    fn remove_session(&mut self, sid: u32) -> Option<Session> {
        let i = self.st.sessions.iter().position(|s| s.id == sid)?;
        Some(self.st.sessions.remove(i))
    }

    /// Ids of the sessions the player is in that pass the filter, in creation order.
    fn sessions_of(&self, profile_id: &str, filter: impl Fn(&Session) -> bool) -> Vec<u32> {
        self.st.sessions.iter().filter(|s| s.has(profile_id) && filter(s)).map(|s| s.id).collect()
    }

    fn next_session_id(&mut self) -> u32 {
        self.st.next_session_id += 1;
        self.st.next_session_id - 1
    }

    fn reserve_session_id(&mut self, profile_id: &str) -> u32 {
        let sid = self.next_session_id();
        self.st.reserved.insert(profile_id.into(), sid);
        sid
    }

    fn match_ticket(&mut self, tid: u64) {
        let Some(t) = self.st.tickets.get(&tid).filter(|t| t.session_type == SESSION_WORLD).cloned() else { return };
        let place = t.place();
        // an existing session with a known host
        let mut sessions: Vec<&Session> = self.st.sessions.iter().collect();
        sessions.sort_by_key(|s| s.id);
        let found = sessions.iter().find(|s| {
            s.kind == SESSION_WORLD && s.host.is_some() && !s.host_urls.is_empty() && s.free() > 0
                && !t.excluded.contains(&s.id) && !s.has(&t.profile_id) && same_place(&s.place(), &place)
        }).map(|s| s.id);
        if let Some(sid) = found {
            self.add(sid, &t);
            self.send_join(sid, &t.profile_id);
            return;
        }
        // another waiting ticket: a new session, the older ticket's player hosts
        let other = self.st.tickets.values().find(|o| {
            o.id != t.id && o.profile_id != t.profile_id && o.session_type == SESSION_WORLD
                && same_place(&o.place(), &place)
        }).cloned();
        if let Some(other) = other {
            let (host, joiner) = if other.id < t.id { (other, t) } else { (t, other) };
            self.new_session(host, joiner);
        }
    }

    fn add(&mut self, sid: u32, t: &Ticket) {
        let s = self.session_mut(sid).expect("a live session");
        s.members.push(Member { ticket: Some(t.clone()), ..Member::new(&t.profile_id, PENDING) });
        self.st.tickets.remove(&t.id);
    }

    fn new_session(&mut self, host: Ticket, joiner: Ticket) {
        let mut attrs = vec![];
        for k in ATTR_ORDER {
            let Some((_, v)) = host.criteria.iter().find(|(c, _)| c == k) else { continue };
            match py_int(v) {
                Some(i) => attrs.push((k.to_string(), Variant::Int(i))),
                None => log::warn!(target: "powder.matchmaking", "ticket {}: criterion {k} is not a number: {}",
                                   host.id, text(v)),
            }
        }
        attrs.push(("reputation".into(), Variant::Int(1)));
        let sid = self.next_session_id();
        self.st.sessions.push(Session { id: sid, kind: SESSION_WORLD, host: None, attrs, members: vec![],
                                        max_size: host.max_size, created: Utc::now(), host_urls: vec![],
                                        host_candidate: None });
        self.add(sid, &host);
        self.add(sid, &joiner);
        let (world, cell) = self.session(sid).expect("just made").place();
        log::info!(target: "powder.matchmaking", "session {sid}: {} hosts, {} joins (world {}, cell {})",
                   host.profile_id, joiner.profile_id, text(&world), text(&cell));
        self.ask_host(sid, &host.profile_id);
    }

    /// Code 1: the member is to host the session (sent again by tick until it reports back).
    fn ask_host(&mut self, sid: u32, pid: &str) {
        let now = self.now;
        let s = self.session_mut(sid).expect("a live session");
        s.host_candidate = Some(pid.into());
        let (kind, s_created) = (s.kind, s.created);
        let m = s.member_mut(pid).expect("a member");
        (m.asked_at, m.asked) = (Some(now), m.asked + 1);
        if m.asked > 1 {
            log::info!(target: "powder.matchmaking", "session {sid}: asking {pid} to host again (try {})", m.asked);
        }
        let created = m.ticket.as_ref().map_or(s_created, |t| t.created);
        let ticket_id = m.ticket.as_ref().map_or(Value::Null, |t| json!(t.id));
        self.notify(pid, CODE_HOST, json!({"session_type": kind, "ticket_id": ticket_id, "session_id": sid,
                                           "ticket_creation_dates": dumps(json!([iso(created)]))}));
    }

    fn send_join(&mut self, sid: u32, pid: &str) {
        let now = self.now;
        let s = self.session_mut(sid).expect("a live session");
        let m = s.member_mut(pid).expect("a member");
        (m.asked_at, m.asked) = (Some(now), m.asked + 1);
        let (asked, ticket_created) = (m.asked, m.ticket.as_ref().map(|t| t.created));
        let host = opt(&s.host);
        log::info!(target: "powder.matchmaking", "session {sid}: {pid} joins host {}{}",
                   s.host.as_deref().unwrap_or("None"), if asked > 1 { format!(" (try {asked})") } else { String::new() });
        let tickets: Map<String, Value> = s.members.iter()
            .filter_map(|x| x.ticket.as_ref().map(|t| (x.profile_id.clone(), json!(t.id)))).collect();
        let params = json!({
            "session_type": s.kind,
            "ticket_creation_dates": dumps(json!([iso(ticket_created.unwrap_or(s.created))])),
            "ticket_ids": dumps(Value::Object(tickets)), "session_id": sid, "host_profile_id": host,
            "host_station_urls": dumps(json!(reachable_urls(&s.host_urls)))});
        self.notify(pid, CODE_JOIN, params);
    }

    /// Nobody took the session: dissolve it and put its members' tickets back, so they are matched anew.
    fn give_up(&mut self, sid: u32) {
        log::warn!(target: "powder.matchmaking", "session {sid}: no member reported as host, giving it up");
        let Some(s) = self.remove_session(sid) else { return };
        for t in s.members.into_iter().filter_map(|m| m.ticket) {
            self.st.tickets.insert(t.id, t);
        }
    }

    fn report_host_success(&mut self, profile_id: &str, sid: u32, urls: Vec<String>) {
        let Some(s) = self.session_mut(sid).filter(|s| s.has(profile_id)) else {
            log::warn!(target: "powder.matchmaking", "ReportHostSuccess from {profile_id} for unknown session {sid}");
            return;
        };
        (s.host, s.host_urls, s.host_candidate) = (Some(profile_id.into()), urls.clone(), None);
        let host = s.member_mut(profile_id).expect("checked above");
        (host.state, host.urls, host.asked_at) = (JOINED, urls, None);
        log::info!(target: "powder.matchmaking", "session {sid}: {profile_id} hosts ({} station URLs)", s.host_urls.len());
        let pending: Vec<String> = s.members.iter_mut().filter(|m| m.profile_id != profile_id && m.state == PENDING)
            .map(|m| {
                m.asked = 0;
                m.profile_id.clone()
            }).collect();
        for pid in pending {
            self.send_join(sid, &pid);
        }
        let tids: Vec<u64> = self.st.tickets.keys().copied().collect();
        for tid in tids {
            self.match_ticket(tid);
        }
    }

    /// Code 4: where the session's host is (recorded after JoinSession; the game then connects to the host and
    /// reports ReportJoinSuccess).
    fn send_direct_join(&mut self, sid: u32, pid: &str) {
        let s = self.session(sid).expect("a live session");
        log::info!(target: "powder.matchmaking", "session {sid}: {pid} joins host {} directly",
                   s.host.as_deref().unwrap_or("None"));
        let params = json!({"session_type": s.kind, "session_id": sid, "host_profile_id": opt(&s.host),
                            "host_station_urls": dumps(json!(reachable_urls(&s.host_urls)))});
        self.notify(pid, CODE_DIRECT_JOIN, params);
    }

    fn join_directly(&mut self, profile_id: &str, sid: u32) {
        if self.session(sid).is_some_and(|s| s.kind == SESSION_GROUP) {
            // one group at a time
            for other in self.sessions_of(profile_id, |o| o.kind == SESSION_GROUP && o.id != sid) {
                self.leave(profile_id, other);
            }
        }
        let s = self.session_mut(sid).expect("a live session");
        if !s.has(profile_id) {
            s.members.push(Member::new(profile_id, PENDING));
        }
        self.send_direct_join(sid, profile_id);
    }

    fn leave(&mut self, profile_id: &str, sid: u32) {
        let Some(s) = self.session_mut(sid) else { return };
        let Some(i) = s.members.iter().position(|m| m.profile_id == profile_id) else { return };
        s.members.remove(i);
        log::info!(target: "powder.matchmaking", "session {sid}: {profile_id} left ({} remain)", s.members.len());
        if s.members.is_empty() || (s.kind != SESSION_WORLD && s.host.as_deref() == Some(profile_id)) {
            self.remove_session(sid);
            return;
        }
        if s.kind != SESSION_WORLD {
            return;
        }
        if s.host.as_deref() == Some(profile_id) {
            // a running session: another member takes over
            (s.host, s.host_urls) = (None, vec![]);
            let i = s.members.iter().position(|m| m.state == JOINED).unwrap_or(0);
            let new = &mut s.members[i];
            new.asked_at = None;
            let new = new.profile_id.clone();
            s.host_candidate = Some(new.clone());
            let kind = s.kind;
            log::info!(target: "powder.matchmaking", "session {sid}: host left, asking {new} to host");
            self.notify(&new, CODE_BECOME_HOST, json!({"host_profile_id": new, "session_id": sid, "session_type": kind}));
        } else if s.host_candidate.as_deref() == Some(profile_id) && s.host.is_none() {
            // it never hosted: ask the next one
            s.members[0].asked = 0;
            let new = s.members[0].profile_id.clone();
            self.ask_host(sid, &new);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const A: &str = "11111111-1111-1111-1111-111111111111";
    pub const B: &str = "22222222-2222-2222-2222-222222222222";
    pub const C: &str = "33333333-3333-3333-3333-333333333333";
    pub const WORLD: i64 = 123456789;

    type Sent = Arc<Mutex<Vec<(String, i64, Map<String, Value>)>>>;

    pub fn crit(world: i64) -> Vec<(String, Variant)> {
        [("connectivity_nat", 2), ("country", 1), ("language", 2), ("request", 1), ("version", 3),
         ("world_id", world), ("worldcell", 18)].iter().map(|(k, v)| (k.to_string(), Variant::Int(*v))).collect()
    }

    pub fn urls(host: &str) -> Vec<String> {
        let tail = "#Nat=2;PunchGUID=v(1)key(1.0)u(1)s(1)pt(3);peerguidhigh=1;peerguidlow=2;sessionguid=0;versionguid=4";
        vec![format!("prudp:/address={host};port=3074;type=2{tail}"), format!("prudp:/address={host};port=3074;type=1{tail}")]
    }

    struct Mm {
        svc: MatchmakingService,
        sent: Sent,
        now: Arc<Mutex<f64>>,
    }

    impl Mm {
        fn new() -> Mm {
            let (sent, now): (Sent, _) = (Arc::default(), Arc::new(Mutex::new(1000.0)));
            let (s, n) = (sent.clone(), now.clone());
            let svc = MatchmakingService::with_clock(Arc::new(move |pid: &str, code, params| {
                s.lock().push((pid.into(), code, params))
            }), Box::new(move || *n.lock()), 15.0);
            Mm { svc, sent, now }
        }

        fn ticket(&self, pid: &str, world: i64, excluded: &[u32]) -> u64 {
            let own = self.svc.create_session_with_reserved_id(pid, self.svc.reserve_session_id(pid), 1,
                                                               vec![("version".into(), Variant::Int(3))],
                                                               urls("192.0.2.1"), 4);
            self.svc.create_match_ticket(pid, own, 2, crit(world), excluded, 32)
        }

        fn sent(&self) -> Vec<(String, i64, Map<String, Value>)> {
            self.sent.lock().clone()
        }

        fn last(&self) -> (String, i64, Map<String, Value>) {
            self.sent.lock().last().cloned().unwrap()
        }

        fn codes(&self) -> Vec<(String, i64)> {
            self.sent.lock().iter().map(|(p, c, _)| (p.clone(), *c)).collect()
        }

        fn first_sid(&self) -> u32 {
            self.sent.lock()[0].2["session_id"].as_u64().unwrap() as u32
        }

        fn advance(&self, s: f64) {
            *self.now.lock() += s;
        }
    }

    fn parsed(v: &Value) -> Value {
        serde_json::from_str(v.as_str().unwrap()).unwrap()
    }

    #[test]
    fn a_lone_player_waits() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        assert!(mm.sent().is_empty());
    }

    #[test]
    fn two_players_meet() {
        let mm = Mm::new();
        let ta = mm.ticket(A, WORLD, &[]);
        let tb = mm.ticket(B, WORLD, &[]);
        // the older ticket's player hosts
        let [(pid, code, params)]: [_; 1] = mm.sent().try_into().unwrap();
        assert_eq!((pid.as_str(), code, &params["ticket_id"]), (A, CODE_HOST, &json!(ta)));
        let keys: Vec<&str> = params.keys().map(String::as_str).collect();
        assert_eq!(keys, ["session_type", "ticket_id", "session_id", "ticket_creation_dates"]);
        let sid = mm.first_sid();
        let dates = parsed(&params["ticket_creation_dates"]);
        assert_eq!(dates[0].as_str().unwrap().matches('T').count(), 1);
        assert_eq!(dates[0].as_str().unwrap().len(), 26); // 2026-10-25T12:00:00.123456
        let s = mm.svc.get_session(sid).unwrap();
        assert!(s.host.is_none() && s.members.iter().all(|m| m.state == PENDING) && s.free() == 30);
        assert_eq!((get(&s.attrs, "world_id"), get(&s.attrs, "reputation")), (&Variant::Int(WORLD), &Variant::Int(1)));
        let keys: Vec<&str> = s.attrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["worldcell", "language", "country", "connectivity_nat", "version", "world_id", "reputation"]);
        // the host reports where it can be reached; then the other player is told to join
        mm.svc.report_host_success(A, sid, [urls("192.0.2.10"), urls("127.0.0.1")].concat());
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code, &params["host_profile_id"], &params["session_id"]),
                   (B, CODE_JOIN, &json!(A), &json!(sid)));
        let keys: Vec<&str> = params.keys().map(String::as_str).collect();
        assert_eq!(keys, ["session_type", "ticket_creation_dates", "ticket_ids", "session_id", "host_profile_id",
                          "host_station_urls"]);
        assert_eq!(params["ticket_ids"], json!(format!(r#"{{"{A}": {ta}, "{B}": {tb}}}"#)));
        assert_eq!(parsed(&params["host_station_urls"]), json!(urls("192.0.2.10"))); // loopback dropped
        mm.svc.report_join_success(B, sid);
        let s = mm.svc.get_session(sid).unwrap();
        assert!(s.members.iter().all(|m| m.state == JOINED));
        assert!(mm.svc.tickets().is_empty()); // used up by the match
    }

    #[test]
    fn a_third_player_joins_the_running_session() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        mm.ticket(B, WORLD, &[]);
        let sid = mm.first_sid();
        mm.svc.report_host_success(A, sid, urls("192.0.2.10"));
        mm.ticket(C, WORLD, &[]);
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code, &params["session_id"]), (C, CODE_JOIN, &json!(sid)));
    }

    #[test]
    fn other_worlds_and_excluded_sessions() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        mm.ticket(B, WORLD + 1, &[]);
        assert!(mm.sent().is_empty()); // different worlds never meet
        mm.svc.on_disconnect(B);
        mm.ticket(B, WORLD, &[]);
        let sid = mm.last().2["session_id"].as_u64().unwrap() as u32;
        mm.svc.report_host_success(A, sid, urls("192.0.2.10"));
        mm.svc.report_connection_failure(B, sid, Some(0x0A));
        assert!(!mm.svc.get_session(sid).unwrap().has(B));
        let n = mm.sent().len();
        mm.ticket(B, WORLD, &[sid]); // the game avoids the session it failed with
        assert_eq!(mm.sent().len(), n);
    }

    #[test]
    fn host_leaves_and_another_member_hosts() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        mm.ticket(B, WORLD, &[]);
        let sid = mm.first_sid();
        mm.svc.report_host_success(A, sid, urls("192.0.2.10"));
        mm.svc.report_join_success(B, sid);
        mm.svc.on_disconnect(A);
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code), (B, CODE_BECOME_HOST));
        assert_eq!(crate::util::dumps(&Value::Object(params)),
                   format!(r#"{{"host_profile_id": "{B}", "session_id": {sid}, "session_type": 2}}"#));
        mm.svc.migrate_host(B, sid, urls("192.0.2.20"));
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code), (B, CODE_MIGRATE_DONE));
        assert_eq!(crate::util::dumps(&Value::Object(params)),
                   format!(r#"{{"session_id": {sid}, "host_profile_id": "{B}"}}"#));
        assert_eq!(mm.svc.get_session(sid).unwrap().host_urls, urls("192.0.2.20"));
        mm.svc.leave(B, sid);
        assert!(mm.svc.get_session(sid).is_none());
    }

    #[test]
    fn only_players_in_the_same_cell_meet() {
        let mm = Mm::new();
        let cell = |c: i64| {
            let mut v = crit(WORLD);
            update(&mut v, "worldcell", Variant::Int(c));
            v
        };
        let own = mm.svc.create_session_with_reserved_id(A, mm.svc.reserve_session_id(A), 1, vec![], urls("192.0.2.1"), 4);
        mm.svc.create_match_ticket(A, own, 2, cell(0), &[], 32);
        let own = mm.svc.create_session_with_reserved_id(B, mm.svc.reserve_session_id(B), 1, vec![], urls("192.0.2.2"), 4);
        let tb = mm.svc.create_match_ticket(B, own, 2, cell(1001), &[], 32);
        assert!(mm.sent().is_empty());
        mm.svc.update_ticket(B, tb, vec![("worldcell".into(), Variant::UInt(0))]); // B rides into A's cell
        let [(pid, code, params)]: [_; 1] = mm.sent().try_into().unwrap();
        assert_eq!((pid.as_str(), code), (A, CODE_HOST));
        let s = mm.svc.get_session(params["session_id"].as_u64().unwrap() as u32).unwrap();
        assert_eq!(get(&s.attrs, "worldcell"), &Variant::Int(0));
    }

    #[test]
    fn code_1_is_sent_again_then_the_session_is_given_up() {
        let mm = Mm::new();
        let (ta, tb) = (mm.ticket(A, WORLD, &[]), mm.ticket(B, WORLD, &[]));
        let sid = mm.first_sid();
        for _ in 0..2 {
            // A's game does not react: asked again
            mm.advance(15.0);
            mm.svc.tick();
        }
        assert_eq!(mm.codes(), vec![(A.to_string(), CODE_HOST); 3]);
        mm.advance(15.0);
        mm.svc.tick();
        assert!(mm.svc.get_session(sid).is_none());
        assert_eq!(mm.svc.tickets().iter().map(|t| t.id).collect::<Vec<_>>(), [ta, tb]);
        mm.svc.update_ticket(B, tb, vec![]); // the next ticket refresh matches them again
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code), (A, CODE_HOST));
        assert_ne!(params["session_id"], json!(sid));
    }

    #[test]
    fn host_candidate_leaving_before_hosting_hands_code_1_on() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        mm.ticket(B, WORLD, &[]);
        let sid = mm.first_sid();
        mm.svc.leave(A, sid); // A never reported as host
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code, &params["session_id"]), (B, CODE_HOST, &json!(sid)));
    }

    #[test]
    fn a_new_world_ticket_leaves_the_old_world_session() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        mm.ticket(B, WORLD, &[]);
        let sid = mm.first_sid();
        mm.svc.report_host_success(A, sid, urls("192.0.2.10"));
        let tb2 = mm.ticket(B, WORLD, &[]); // B searches again without leaving first
        let s = mm.svc.get_session(sid).unwrap();
        let b = s.member(B).unwrap();
        assert_eq!((b.ticket.as_ref().unwrap().id, b.state), (tb2, PENDING)); // left, joins again (code 2)
        assert_eq!(mm.last().1, CODE_JOIN);
        assert_eq!(mm.last().0, B);
    }

    #[test]
    fn code_2_is_sent_again_until_the_joiner_reports() {
        let mm = Mm::new();
        mm.ticket(A, WORLD, &[]);
        mm.ticket(B, WORLD, &[]);
        let sid = mm.first_sid();
        mm.svc.report_host_success(A, sid, urls("192.0.2.10"));
        mm.advance(15.0);
        mm.svc.tick();
        assert_eq!(mm.codes(), [(A.into(), CODE_HOST), (B.into(), CODE_JOIN), (B.into(), CODE_JOIN)]);
        mm.svc.report_join_success(B, sid);
        mm.advance(60.0);
        mm.svc.tick();
        assert_eq!(mm.sent().len(), 3); // joined: nothing more
    }

    #[test]
    fn join_group_on_a_friend() {
        let mm = Mm::new();
        let group_b = mm.svc.create_session_with_reserved_id(B, mm.svc.reserve_session_id(B), 1, vec![],
                                                             urls("192.0.2.20"), 4);
        let own_a = mm.svc.create_session_with_reserved_id(A, mm.svc.reserve_session_id(A), 1, vec![],
                                                           urls("192.0.2.10"), 4);
        assert!(mm.svc.join_player(A, &B.to_uppercase()));
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code, &params["session_id"], &params["session_type"]),
                   (A, CODE_DIRECT_JOIN, &json!(group_b), &json!(1)));
        assert!(!params.contains_key("ticket_ids")); // as recorded: code 4 has no ticket fields
        assert_eq!(parsed(&params["host_station_urls"]), json!(urls("192.0.2.20")));
        assert_eq!(params["host_profile_id"], json!(B));
        assert!(mm.svc.get_session(own_a).is_none());
        assert_eq!(mm.svc.get_session(group_b).unwrap().member(A).unwrap().state, PENDING);
        mm.svc.report_join_success(A, group_b);
        assert_eq!(mm.svc.get_session(group_b).unwrap().member(A).unwrap().state, JOINED);
        assert!(!mm.svc.join_player(B, C)); // C has no group session: the call fails
        mm.svc.remove_members(B, group_b, &[A.to_uppercase()]); // the host takes A out again
        assert!(!mm.svc.get_session(group_b).unwrap().has(A));
    }

    #[test]
    fn accepting_an_invite_joins_the_inviters_group() {
        let mm = Mm::new();
        let group_a = mm.svc.create_session_with_reserved_id(A, mm.svc.reserve_session_id(A), 1, vec![],
                                                             urls("192.0.2.10"), 4);
        mm.svc.create_session_with_reserved_id(B, mm.svc.reserve_session_id(B), 1, vec![], urls("192.0.2.20"), 4);
        mm.svc.join_session(B, group_a);
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code, &params["session_id"], &params["host_profile_id"]),
                   (B, CODE_DIRECT_JOIN, &json!(group_a), &json!(A)));
        mm.svc.leave(A, group_a); // the inviter's group ends
        mm.svc.join_session(C, group_a);
        let (pid, code, params) = mm.last();
        assert_eq!((pid.as_str(), code), (C, CODE_DIRECT_JOIN_FAILED));
        assert_eq!(crate::util::dumps(&Value::Object(params)),
                   format!(r#"{{"session_id": {group_a}, "session_type": 1}}"#));
    }

    #[test]
    fn reachable_urls_drop_loopback_unless_alone() {
        assert_eq!(reachable_urls(&[urls("127.0.0.1"), urls("192.0.2.5")].concat()), urls("192.0.2.5"));
        assert_eq!(reachable_urls(&urls("127.0.0.1")), urls("127.0.0.1"));
    }

    #[test]
    fn ids_and_python_values() {
        let mm = Mm::new();
        assert_eq!(mm.svc.reserve_session_id(A), 0x196001);
        assert_eq!(mm.svc.create_default_session(A), 0x196001); // the reserved id
        assert_eq!(mm.svc.create_default_ticket(A), 0x7D0000);
        assert_eq!(mm.svc.create_default_ticket(B), 0x7D1234); // no criteria on either: one place (None, None)
        assert_eq!(mm.last().2["session_id"], json!(0x196002));
        assert!(py_eq(&Variant::UInt(5), &Variant::Double(5.0)) && py_eq(&Variant::Bool(true), &Variant::Int(1)));
        assert!(!py_eq(&Variant::String("5".into()), &Variant::Int(5)));
        assert_eq!((py_int(&Variant::Double(-2.7)), py_int(&Variant::String(" 7 ".into())), py_int(&Variant::None)),
                   (Some(-2), Some(7), None));
        assert_eq!(iso(DateTime::from_timestamp(1_700_000_000, 5_000).unwrap()), "2023-11-14T22:13:20.000005");
    }
}
