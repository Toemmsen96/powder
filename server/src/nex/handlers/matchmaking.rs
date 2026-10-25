//! MatchmakingProtocol: the game's side of matchmaking (logic in services/matchmaking.rs).
//!
//! Params start with a type map; layouts from recordings of Ubisoft's server (types: u32 session id,
//! Map<String,Variant>, List<String> station URLs, SessionMember = GUID profile + u32 state).
//!
//! [`attach`] gives the service what the NEX server drives: a closed game connection leaves its sessions, and a
//! 1 s tick resends unanswered codes.

use super::{HandlerDeps, HandlerFn, Result};
use crate::nex::dispatch::{Ctx, HandlerError};
use crate::nex::rmc::{parse_guid, quazal_datetime, Reader, Variant, Writer};
use crate::nex::server::NexServer;
use crate::services::matchmaking::{py_int, MatchmakingService, SESSION_WORLD};
use anyhow::anyhow;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const HANDLERS: &[(&str, HandlerFn)] = &[
    ("MatchmakingProtocol::ReserveSessionId_V1", reserve_session_id),
    ("MatchmakingProtocol::CreateSessionWithReservedId_V1", create_session_with_reserved_id),
    ("MatchmakingProtocol::CreateMatchTicket_V1", create_match_ticket),
    ("MatchmakingProtocol::UpdateTicket_V1", update_ticket),
    ("MatchmakingProtocol::CancelAllTicket_V1", cancel_all_ticket),
    ("MatchmakingProtocol::GetSession_V1", get_session),
    ("MatchmakingProtocol::JoinSession_V1", join_session),
    ("MatchmakingProtocol::JoinPlayer_V1", join_player),
    ("MatchmakingProtocol::RemoveMembers_V1", remove_members),
    ("MatchmakingProtocol::LeaveSession_V1", leave_session),
    ("MatchmakingProtocol::ReportHostSuccess_V1", report_host_success),
    ("MatchmakingProtocol::ReportJoinSuccess_V1", report_join_success),
    ("MatchmakingProtocol::ReportConnectionFailure_V1", report_connection_failure),
    ("MatchmakingProtocol::MigrateHost_V1", migrate_host),
    ("MatchmakingProtocol::UpdateSession_V1", update_session),
];

/// Wire the NEX server to the service: disconnects and the 1 s matchmaking tick (ripe tickets, retries).
pub fn attach(server: &Arc<Mutex<NexServer>>, mm: Arc<MatchmakingService>) {
    let on_disconnect = mm.clone();
    server.lock().on_disconnect = Some(Box::new(move |pid| on_disconnect.on_disconnect(pid)));
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        timer.tick().await;
        loop {
            timer.tick().await;
            mm.tick();
        }
    });
}

fn mm(deps: &HandlerDeps) -> &MatchmakingService {
    &deps.services.matchmaking
}

/// The calling player; a call before LoginWithToken is not ours to answer.
fn pid(ctx: &Ctx) -> std::result::Result<String, HandlerError> {
    ctx.conn.profile_id.clone().ok_or(HandlerError::Fallthrough)
}

fn sid(r: &mut Reader) -> anyhow::Result<u32> {
    r.type_map()?;
    r.u32()
}

/// outputs (reader FUN_7ff661945900): u32 session id. The game retries every 5 s until it gets one.
pub fn reserve_session_id(deps: &HandlerDeps, ctx: &mut Ctx, _: &mut Reader) -> Result {
    Ok(Writer::new().u32(mm(deps).reserve_session_id(&pid(ctx)?)).take())
}

/// params: type map, u32 reserved id, u32 session type (1: the player's group), Map attributes, List<String> station
/// URLs, List<SessionMember>, u32 max size. outputs: u32 = the id, echoed back.
pub fn create_session_with_reserved_id(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let parsed = (|| -> anyhow::Result<_> {
        let (sid, kind, attrs, urls) = (r.u32()?, r.u32()?, r.variant_map()?, r.strings()?);
        for _ in 0..r.u32()? {
            r.guid()?;
            r.u32()?;
        }
        Ok((sid, kind, attrs, urls, r.u32()?))
    })();
    let pid = pid(ctx)?;
    let sid = match parsed {
        Ok((sid, kind, attrs, urls, max_size)) => {
            mm(deps).create_session_with_reserved_id(&pid, sid, kind, attrs, urls, max_size)
        }
        Err(_) => mm(deps).create_default_session(&pid), // short params: just the id
    };
    Ok(Writer::new().u32(sid).take())
}

/// params: type map {SearchCriteria}, u32 own session id, u32 session type (2: the shared world), Map criteria
/// (connectivity_nat, country, language, request, version, world_id, worldcell), List<u32> excluded session ids,
/// u32 max size. outputs: u64 ticket id.
pub fn create_match_ticket(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let parsed = (|| -> anyhow::Result<_> {
        let (own, kind, criteria) = (r.u32()?, r.u32()?, r.variant_map()?);
        let excluded = (0..r.u32()?).map(|_| r.u32()).collect::<anyhow::Result<Vec<u32>>>()?;
        Ok((own, kind, criteria, excluded, r.u32()?))
    })();
    let pid = pid(ctx)?;
    let tid = match parsed {
        Ok((own, kind, criteria, excluded, max_size)) => {
            mm(deps).create_match_ticket(&pid, own, kind, criteria, &excluded, max_size)
        }
        Err(_) => mm(deps).create_default_ticket(&pid),
    };
    Ok(Writer::new().u64(tid).take())
}

/// params: type map, u32 ticket id, Map criteria.
pub fn update_ticket(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    let tid = sid(r)?;
    mm(deps).update_ticket(&pid, tid as u64, r.variant_map()?);
    Ok(vec![])
}

pub fn cancel_all_ticket(deps: &HandlerDeps, ctx: &mut Ctx, _: &mut Reader) -> Result {
    mm(deps).cancel_all_tickets(&pid(ctx)?);
    Ok(vec![])
}

/// params: type map {MatchmakingSession, SessionMember}, u32 session id.
/// outputs: u32 id, u32 type, DateTime created, Map attributes (int64), GUID host (zero until it reported its
/// URLs), u32 host state, List<String> host URLs, List<SessionMember>, u32 max size, u32 free slots, bool (false for
/// world sessions).
pub fn get_session(deps: &HandlerDeps, _: &mut Ctx, r: &mut Reader) -> Result {
    // not ours (e.g. from before a restart): the recorded reply
    let s = mm(deps).get_session(sid(r)?).ok_or(HandlerError::Fallthrough)?;
    let mut w = Writer::new();
    w.u32(s.id).u32(s.kind).u64(quazal_datetime(s.created)).u32(s.attrs.len() as u32);
    for (k, v) in &s.attrs {
        let v = py_int(v).ok_or_else(|| anyhow!("session {} attribute {k} is not a number: {v:?}", s.id))?;
        w.string(k).variant(&Variant::Int(v));
    }
    let host = s.host.as_deref().and_then(|h| s.member(h));
    match host {
        Some(h) => w.guid(&parse_guid(&h.profile_id)?).u32(h.state).strings(&s.host_urls),
        None => w.guid(&Uuid::nil()).u32(0).u32(0),
    };
    w.u32(s.members.len() as u32);
    for m in &s.members {
        w.guid(&parse_guid(&m.profile_id)?).u32(m.state);
    }
    Ok(w.u32(s.max_size).u32(s.free()).bool(s.kind != SESSION_WORLD).take())
}

pub fn join_session(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    mm(deps).join_session(&pid, sid(r)?);
    Ok(vec![])
}

pub fn leave_session(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    mm(deps).leave(&pid, sid(r)?);
    Ok(vec![])
}

/// params: type map, u32 session id, List<String> station URLs, u32 (0).
pub fn report_host_success(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    let sid = sid(r)?;
    mm(deps).report_host_success(&pid, sid, r.strings()?);
    Ok(vec![])
}

pub fn report_join_success(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    mm(deps).report_join_success(&pid, sid(r)?);
    Ok(vec![])
}

/// params: type map, u32 session id, u32 reason (0x0a and 0x1a seen), 3 more bytes.
pub fn report_connection_failure(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    let sid = sid(r)?;
    let reason = if r.remaining() >= 4 { Some(r.u32()?) } else { None };
    mm(deps).report_connection_failure(&pid, sid, reason);
    Ok(vec![])
}

/// params: type map, u32 session id, List<String> station URLs (answer to code 12).
pub fn migrate_host(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    let sid = sid(r)?;
    mm(deps).migrate_host(&pid, sid, r.strings()?);
    Ok(vec![])
}

/// params: type map, u32 session id, List<GUID> players (the host removes them).
pub fn remove_members(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    let sid = sid(r)?;
    let members = (0..r.u32()?).map(|_| Ok(r.guid()?.to_string())).collect::<anyhow::Result<Vec<_>>>()?;
    mm(deps).remove_members(&pid, sid, &members);
    Ok(vec![])
}

/// params: type map, GUID the friend ("Join group" in the social tab). No outputs (the game's reply dispatcher reads
/// none); the game is then told where to go with code 4 (services/matchmaking.rs).
pub fn join_player(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let pid = pid(ctx)?;
    if !mm(deps).join_player(&pid, &r.guid()?.to_string()) {
        return Err(HandlerError::Rmc("Core".into(), 2)); // the game shows "join has failed"
    }
    Ok(vec![])
}

/// params: type map, u32 session id, Map attributes.
pub fn update_session(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    let pid = pid(ctx)?;
    let sid = sid(r)?;
    mm(deps).update_session(&pid, sid, r.variant_map()?);
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use super::*;
    use crate::config::Config;
    use crate::core::identity::Player;
    use crate::core::sessions::NexChannel;
    use crate::nex::connection::{Connection, SendFn};
    use crate::nex::rmc::{self, Message, NOTIFICATION_METHOD, V_INT64};
    use crate::services::matchmaking::{CODE_BECOME_HOST, CODE_DIRECT_JOIN, CODE_HOST, CODE_JOIN, JOINED, PENDING};
    use serde_json::{json, Value};

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const WORLD: u64 = 123456789;

    /// A game's NEX connection as the session registry sees it: the requests pushed to it.
    #[derive(Default)]
    struct Pushed(Mutex<Vec<Vec<u8>>>);

    impl NexChannel for Pushed {
        fn send_request(&self, protocol: &str, method: &str, params: &[u8], call_id: Option<u32>) -> u32 {
            let call_id = call_id.unwrap_or(0x10000);
            self.0.lock().push(rmc::build_request(protocol, call_id, method, params));
            call_id
        }
    }

    struct Game {
        conn: Connection,
        pushed: Arc<Pushed>,
    }

    fn game(deps: &Arc<HandlerDeps>, pid: &str, name: &str) -> Game {
        let player = Player { profile_id: pid.into(), user_id: pid.into(), name: name.into() };
        let session = deps.sessions.issue(player, "127.0.0.1", "");
        let pushed = Arc::new(Pushed::default());
        session.channels.set_nex(Some(pushed.clone()));
        let send_fn: SendFn = Arc::new(|_, _| {});
        let mut conn = Connection::new("192.0.2.9:1".parse().unwrap(), send_fn, 1, 0x3f, 0x31, 0.0);
        conn.profile_id = Some(pid.into());
        Game { conn, pushed }
    }

    impl Game {
        fn call(&mut self, deps: &Arc<HandlerDeps>, method: &str, params: &[u8]) -> Message {
            let d = testing::dispatcher(deps);
            let mut ctx = Ctx { conn: &mut self.conn, station_url: String::new(), now: 0.0 };
            let req = rmc::parse(&rmc::build_request("MatchmakingProtocol", 10, method, params)).unwrap();
            rmc::parse(&d.dispatch(&mut ctx, &req)).unwrap()
        }

        /// The one notification pushed since the last look: its JSON.
        fn notification(&self) -> Value {
            let [raw]: [Vec<u8>; 1] = std::mem::take(&mut *self.pushed.0.lock()).try_into().unwrap();
            let msg = rmc::parse(&raw).unwrap();
            assert!(msg.is_request && msg.call_id == 0 && msg.method == NOTIFICATION_METHOD);
            let mut r = Reader::new(&msg.body);
            assert_eq!(r.type_map().unwrap(), [("NotificationEvent".to_string(), 1)]);
            assert_eq!((r.take(14).unwrap(), r.u16().unwrap()), (&[0u8; 14][..], 2));
            serde_json::from_str(&r.string().unwrap()).unwrap()
        }
    }

    fn urls(host: &str) -> Vec<String> {
        let tail = "#Nat=2;PunchGUID=v(1)key(1.0)u(1)s(1)pt(3);peerguidhigh=1;peerguidlow=2;sessionguid=0;versionguid=4";
        vec![format!("prudp:/address={host};port=3074;type=2{tail}"),
             format!("prudp:/address={host};port=3074;type=1{tail}")]
    }

    /// test_matchmaking.py's ticket_params: world_id is a UInt64, the rest Int64.
    fn ticket_params(own: u32) -> Vec<u8> {
        let crit: [(&str, u64); 7] = [("connectivity_nat", 2), ("country", 1), ("language", 2), ("request", 1),
                                      ("version", 3), ("world_id", WORLD), ("worldcell", 18)];
        let mut w = Writer::new();
        w.u32(1).string("SearchCriteria").u16(1).u32(own).u32(2).u32(crit.len() as u32);
        for (k, v) in crit {
            w.string(k).variant(&if k == "world_id" { Variant::UInt(v) } else { Variant::Int(v as i64) });
        }
        w.u32(0).u32(32).take()
    }

    fn open_ticket(deps: &Arc<HandlerDeps>, g: &mut Game) -> u64 {
        let reply = g.call(deps, "MatchmakingProtocol::ReserveSessionId_V1", &Writer::new().u32(0).take());
        let own = Reader::new(&reply.body).u32().unwrap();
        let u = urls("192.0.2.1");
        let mut w = Writer::new();
        w.u32(1).string("SessionMember").u16(1).u32(own).u32(1).u32(0).u32(2).string(&u[0]).string(&u[1]).u32(0)
            .u32(4);
        let body = g.call(deps, "MatchmakingProtocol::CreateSessionWithReservedId_V1", &w.take()).body;
        assert_eq!(body, own.to_le_bytes());
        let out = g.call(deps, "MatchmakingProtocol::CreateMatchTicket_V1", &ticket_params(own));
        assert_eq!(out.body.len(), 8);
        Reader::new(&out.body).u64().unwrap()
    }

    #[test]
    fn two_games_meet_over_nex() {
        let deps = testing::deps(Config::default());
        let (mut alice, mut bob) = (game(&deps, A, "Alice"), game(&deps, B, "Bob"));
        assert_eq!(open_ticket(&deps, &mut alice), 0x7D0000);
        open_ticket(&deps, &mut bob);
        let host = alice.notification(); // Alice's ticket is older: she hosts
        assert_eq!((&host["code"], &host["facility"]), (&json!(CODE_HOST), &json!("Matchmaking")));
        let sid = host["params"]["session_id"].as_u64().unwrap() as u32;
        assert_eq!(sid, 0x196003); // after the two games' own sessions

        let out = alice.call(&deps, "MatchmakingProtocol::GetSession_V1", &Writer::new().u32(0).u32(sid).take());
        let mut r = Reader::new(&out.body);
        assert_eq!((r.u32().unwrap(), r.u32().unwrap()), (sid, 2));
        r.u64().unwrap();
        let attrs = r.variant_map().unwrap();
        assert!(attrs.iter().all(|(_, v)| v.kind() == V_INT64));
        assert!(attrs.contains(&("world_id".into(), Variant::Int(WORLD as i64))));
        assert_eq!(attrs.last().unwrap(), &("reputation".into(), Variant::Int(1)));
        assert_eq!((r.guid().unwrap(), r.u32().unwrap(), r.strings().unwrap()), (Uuid::nil(), 0, vec![]));
        let members: Vec<(String, u32)> = (0..r.u32().unwrap())
            .map(|_| (r.guid().unwrap().to_string(), r.u32().unwrap())).collect();
        assert_eq!(members, [(A.into(), PENDING), (B.into(), PENDING)]);
        assert_eq!((r.u32().unwrap(), r.u32().unwrap(), r.bool().unwrap(), r.remaining()), (32, 30, false, 0));

        let u = urls("192.0.2.10");
        let params = Writer::new().u32(0).u32(sid).u32(2).string(&u[0]).string(&u[1]).u32(0).take();
        assert_eq!(alice.call(&deps, "MatchmakingProtocol::ReportHostSuccess_V1", &params).error, None);
        let join = bob.notification();
        assert_eq!((&join["code"], &join["params"]["host_profile_id"]), (&json!(CODE_JOIN), &json!(A)));
        assert_eq!(join["params"]["host_station_urls"], json!(crate::util::dumps(&json!(u))));

        let params = Writer::new().u32(0).u32(sid).take();
        assert_eq!(bob.call(&deps, "MatchmakingProtocol::ReportJoinSuccess_V1", &params).body, b"");
        let out = bob.call(&deps, "MatchmakingProtocol::GetSession_V1", &params);
        let mut r = Reader::new(&out.body);
        r.take(16).unwrap();
        r.variant_map().unwrap();
        assert_eq!((r.guid().unwrap().to_string(), r.u32().unwrap(), r.strings().unwrap()), (A.into(), JOINED, u));

        // Alice's game is gone: out of the session, Bob asked to host
        deps.services.matchmaking.on_disconnect(A);
        assert_eq!(bob.notification()["code"], json!(CODE_BECOME_HOST));
        // a session the player is not in: an empty reply all the same
        let out = bob.call(&deps, "MatchmakingProtocol::LeaveSession_V1", &Writer::new().u32(0).u32(1).take());
        assert_eq!((out.error, out.body), (None, vec![]));
    }

    #[test]
    fn get_session_reply_bytes() {
        let deps = testing::deps(Config::default());
        let mut alice = game(&deps, A, "Alice");
        let mm = &deps.services.matchmaking;
        let url = urls("192.0.2.1")[..1].to_vec();
        let sid = mm.create_session_with_reserved_id(A, 0, 1, vec![("version".into(), Variant::UInt(3))], url.clone(), 4);
        let created = mm.get_session(sid).unwrap().created;
        let body = alice.call(&deps, "MatchmakingProtocol::GetSession_V1", &Writer::new().u32(0).u32(sid).take()).body;
        let a = parse_guid(A).unwrap();
        let mut expected = Writer::new();
        expected.u32(sid).u32(1).u64(quazal_datetime(created)).u32(1).string("version").variant(&Variant::Int(3))
            .guid(&a).u32(JOINED).strings(&url).u32(1).guid(&a).u32(JOINED).u32(4).u32(3).bool(true);
        assert_eq!(body, expected.take());
        assert_eq!(hex::encode(&body[..8]), "0160190001000000");
        // not ours: the recording answers (none here: an RMC error)
        let out = alice.call(&deps, "MatchmakingProtocol::GetSession_V1", &Writer::new().u32(0).u32(7).take());
        assert!(out.error.is_some());
    }

    #[test]
    fn join_player_params_and_failure() {
        let deps = testing::deps(Config::default());
        let (mut alice, bob) = (game(&deps, A, "Alice"), game(&deps, B, "Bob"));
        let params = Writer::new().u32(0).guid(&parse_guid(B).unwrap()).take();
        assert_eq!(params.len(), 20);
        // Bob has no group session: "join has failed"
        let out = alice.call(&deps, "MatchmakingProtocol::JoinPlayer_V1", &params);
        assert_eq!(out.error, Some(("Core".into(), 2)));
        let group = deps.services.matchmaking.create_session_with_reserved_id(B, 0, 1, vec![], urls("192.0.2.20"), 4);
        let out = alice.call(&deps, "MatchmakingProtocol::JoinPlayer_V1", &params);
        assert_eq!((out.error, out.body), (None, vec![]));
        let n = alice.notification();
        assert_eq!((&n["code"], &n["params"]["session_id"]), (&json!(CODE_DIRECT_JOIN), &json!(group)));
        assert!(bob.pushed.0.lock().is_empty());
    }

    #[test]
    fn calls_before_login_fall_through_and_short_params_still_answer() {
        let deps = testing::deps(Config::default());
        let mut g = game(&deps, A, "Alice");
        g.conn.profile_id = None;
        let out = g.call(&deps, "MatchmakingProtocol::ReserveSessionId_V1", b"");
        assert_eq!(out.error, Some(("Core".into(), 2))); // no recording behind it here
        g.conn.profile_id = Some(A.into());
        let out = g.call(&deps, "MatchmakingProtocol::CreateSessionWithReservedId_V1", &Writer::new().u32(0).take());
        assert_eq!(out.body, 0x196001u32.to_le_bytes());
        let out = g.call(&deps, "MatchmakingProtocol::CreateMatchTicket_V1", &Writer::new().u32(0).take());
        assert_eq!(out.body, 0x7D0000u64.to_le_bytes());
        let t = &deps.services.matchmaking.tickets()[0];
        assert!(t.session_type == SESSION_WORLD && t.criteria.is_empty() && t.max_size == 32);
    }

    #[tokio::test]
    async fn notifications_go_out_after_the_reply() {
        let deps = testing::deps(Config::default());
        let mut alice = game(&deps, A, "Alice");
        deps.services.matchmaking.create_session_with_reserved_id(B, 0, 1, vec![], urls("192.0.2.20"), 4);
        let params = Writer::new().u32(0).guid(&parse_guid(B).unwrap()).take();
        assert_eq!(alice.call(&deps, "MatchmakingProtocol::JoinPlayer_V1", &params).error, None);
        assert!(alice.pushed.0.lock().is_empty());
        tokio::time::sleep(crate::services::MATCHMAKING_PUSH_DELAY + std::time::Duration::from_millis(100)).await;
        assert_eq!(alice.notification()["code"], json!(CODE_DIRECT_JOIN));
    }
}
