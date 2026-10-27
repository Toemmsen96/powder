//! PlayerStats2Protocol: statistics live in the database (a new player has none; `admin import` brings in recorded
//! ones) and the game's uploads are applied to them (rules in services/stats.rs).

use super::{HandlerDeps, HandlerFn, Result};
use crate::nex::dispatch::{Ctx, HandlerError};
use crate::nex::rmc::Reader;
use crate::services::stats::{StatEvent, PROFILE_GROUP};
use serde_json::{json, Value};

pub const HANDLERS: &[(&str, HandlerFn)] = &[
    ("PlayerStats2Protocol::GetStatisticGroup_V1", get_statistic_group),
    ("PlayerStats2Protocol::PostEvents_V1", post_events),
    ("PlayerStats2Protocol::PostEvent_V1", post_event),
];

/// params: type map {StatisticsResult: 1}, String group name (e.g. "player_profile").
/// outputs: see services/stats.rs.
pub fn get_statistic_group(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    r.type_map()?;
    let group = r.string()?;
    let Some(pid) = &ctx.conn.profile_id else {
        return Err(HandlerError::Rmc("Authentication".into(), 1)); // not logged in: nobody's statistics
    };
    Ok(deps.services.stats.group_reply(pid, &group)?)
}

/// params: type map {StatisticEvent: 1}, then u32 n events (PostEvents) or one event (PostEvent).
/// event = String name, u32 m, m x (String key, Variant value). A repeated key keeps its first position and its
/// last value, as Python's dict does.
pub fn decode_events(r: &mut Reader, single: bool) -> anyhow::Result<Vec<StatEvent>> {
    r.type_map()?;
    let n = if single { 1 } else { r.u32()? };
    let mut events = vec![];
    for _ in 0..n {
        let name = r.string()?;
        let mut fields: Vec<(String, _)> = vec![];
        for _ in 0..r.u32()? {
            let (key, value) = (r.string()?, r.variant()?);
            match fields.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => fields.push((key, value)),
            }
        }
        events.push(StatEvent { name, fields });
    }
    Ok(events)
}

fn post(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader, single: bool) -> Result {
    let events = decode_events(r, single)?;
    let Some(pid) = &ctx.conn.profile_id else {
        return Ok(vec![]); // not logged in: nothing to attribute the progress to
    };
    deps.services.stats.apply_events(pid, &events, PROFILE_GROUP)?;
    if let Some(mods) = &deps.mods {
        let events: Vec<Value> = events.iter().map(|e| json!({"name": e.name, "fields": e.plain()})).collect();
        mods.lock().emit("stats_event", &json!({"profile_id": pid, "events": events}));
    }
    Ok(vec![]) // the client reads no outputs
}

pub fn post_events(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    post(deps, ctx, r, false)
}

pub fn post_event(deps: &HandlerDeps, ctx: &mut Ctx, r: &mut Reader) -> Result {
    post(deps, ctx, r, true)
}

#[cfg(test)]
mod tests {
    use super::super::testing;
    use super::*;
    use crate::config::Config;
    use crate::nex::connection::{Connection, SendFn};
    use crate::nex::rmc::{self, Variant, Writer};
    use crate::services::stats::{decode_stat_group, stat_value, Stat, LEVEL_KEY};
    use std::sync::Arc;

    const P: &str = "11111111-2222-3333-4444-555555555555";

    fn call(deps: &Arc<HandlerDeps>, profile_id: Option<&str>, method: &str, params: &[u8]) -> rmc::Message {
        let d = testing::dispatcher(deps);
        let send_fn: SendFn = Arc::new(|_, _| {});
        let mut conn = Connection::new("192.0.2.9:1".parse().unwrap(), send_fn, 1, 0x3f, 0x31, 0.0);
        conn.profile_id = profile_id.map(String::from);
        let mut ctx = Ctx { conn: &mut conn, station_url: String::new(), now: 0.0 };
        let req = rmc::parse(&rmc::build_request("PlayerStats2Protocol", 6, method, params)).unwrap();
        rmc::parse(&d.dispatch(&mut ctx, &req)).unwrap()
    }

    /// PostEvents/PostEvent params the way the game builds them (test_progression.py's encode_events).
    fn encode_events(events: &[(&str, &[(&str, Variant)])], single: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(1).string("StatisticEvent").u16(1);
        if !single {
            w.u32(events.len() as u32);
        }
        for (name, fields) in events {
            w.string(name).u32(fields.len() as u32);
            for (key, value) in *fields {
                w.string(key).variant(value);
            }
        }
        w.take()
    }

    fn group_params() -> Vec<u8> {
        Writer::new().u32(1).string("StatisticsResult").u16(1).string("player_profile").take()
    }

    fn level(body: &[u8]) -> Stat {
        let (name, rows) = decode_stat_group(body).unwrap();
        assert_eq!(name, P);
        stat_value(rows.iter().find(|r| r.key == LEVEL_KEY).unwrap())
    }

    #[test]
    fn decode_events_multi_and_single() {
        let body = encode_events(&[("update_xp", &[("expertise_id", Variant::UInt(2)), ("xp_earned", Variant::UInt(500)),
                                                  ("xp_ratio_expertise", Variant::Double(0.5))])], false);
        assert_eq!(hex::encode(&body), "010000000f005374617469737469634576656e74000100010000000a007570646174655f787000\
            030000000d006578706572746973655f6964000602000000000000000a0078705f6561726e65640006f401000000000000130078\
            705f726174696f5f6578706572746973650002000000000000e03f"); // as the Python encode_events writes it
        let events = decode_events(&mut Reader::new(&body), false).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "update_xp");
        assert_eq!(Value::Object(events[0].plain()), json!({"expertise_id": 2, "xp_earned": 500,
                                                            "xp_ratio_expertise": 0.5}));
        let body = encode_events(&[("descending_public_challenge_score_event",
                                    &[("challenge_id_to_use", Variant::String("C1".into())),
                                      ("score", Variant::String("100".into()))])], true);
        assert_eq!(decode_events(&mut Reader::new(&body), true).unwrap()[0].plain()["score"], json!("100"));
        // a repeated key: first position, last value
        let body = encode_events(&[("e", &[("a", Variant::Int(1)), ("b", Variant::Int(2)), ("a", Variant::Int(3))])],
                                 true);
        assert_eq!(dumps_plain(&decode_events(&mut Reader::new(&body), true).unwrap()[0]), r#"{"a": 3, "b": 2}"#);
    }

    fn dumps_plain(e: &StatEvent) -> String {
        crate::util::dumps(&Value::Object(e.plain()))
    }

    #[test]
    fn statistic_group_comes_from_the_database() {
        let deps = testing::deps(Config::default());
        deps.repos.create_player(P, P, "Tester").unwrap();
        deps.services.stats.set(P, LEVEL_KEY, Stat::Int(41), PROFILE_GROUP).unwrap();
        let out = call(&deps, Some(P), "PlayerStats2Protocol::GetStatisticGroup_V1", &group_params());
        assert_eq!((out.error, out.method.as_str()), (None, "PlayerStats2Protocol::GetStatisticGroup_V1*"));
        assert_eq!(level(&out.body), Stat::Int(41));
        let mut expected = Writer::new();
        expected.string(P).u32(1).string(LEVEL_KEY).u8(1).i64(41);
        assert_eq!(out.body, expected.take());
        // not logged in: an Authentication error
        let out = call(&deps, None, "PlayerStats2Protocol::GetStatisticGroup_V1", &group_params());
        assert_eq!(out.error, Some(("Authentication".into(), 1)));
    }

    #[test]
    fn uploaded_progress_shows_in_the_next_stat_read() {
        let deps = testing::deps(Config::default());
        deps.repos.create_player(P, P, "Tester").unwrap();
        let params = encode_events(&[("update_level", &[("expertise_id", Variant::UInt(6)),
                                                        ("level_expertise", Variant::UInt(50))])], false);
        let out = call(&deps, Some(P), "PlayerStats2Protocol::PostEvents_V1", &params);
        assert_eq!((out.error, out.body), (None, vec![]));
        let out = call(&deps, Some(P), "PlayerStats2Protocol::GetStatisticGroup_V1", &group_params());
        assert_eq!(level(&out.body), Stat::Int(50));
        // PostEvent_V1: one event, no count
        let params = encode_events(&[("update_level", &[("expertise_id", Variant::UInt(6)),
                                                        ("level_expertise", Variant::UInt(51))])], true);
        assert_eq!(call(&deps, Some(P), "PlayerStats2Protocol::PostEvent_V1", &params).body, b"");
        assert_eq!(deps.services.stats.get(P, LEVEL_KEY, PROFILE_GROUP).unwrap(), Some(Stat::Int(51)));
        // not logged in: accepted, applied to nobody
        let out = call(&deps, None, "PlayerStats2Protocol::PostEvent_V1", &params);
        assert_eq!((out.error, out.body), (None, vec![]));
        assert_eq!(deps.repos.recent_stat_events(P, 100).unwrap().len(), 2);
    }
}
