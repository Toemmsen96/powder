//! Player statistics (NEX PlayerStats2): level, XP per sport, medals, unlocks.
//!
//! Wire format of a GetStatisticGroup_V1 reply, verified against a recorded 4525-byte reply that the game
//! consumes exactly:
//!     String name (the profile id), u32 n, n x (String key, u8 type, 8-byte value)
//!     type 1 = int64 LE, type 2 = float64 LE.   e.g. "level_stat:6:infinite" = overall level.
//! Values are stored exactly as sent, so seeded data round-trips byte for byte.
//!
//! The game computes progress itself and uploads it as stat events (PostEvents_V1 / PostEvent_V1);
//! EVENT_RULES maps each event to the stat it updates. The rules were derived by diffing recorded stat
//! snapshots against the events uploaded between them (every change explained). They are monotonic on
//! purpose: a game given stale stats uploads regressed values (seen live: levels 2-3, playtime 0), and
//! max / add / bitwise-or make such uploads harmless.

use crate::nex::rmc::{self, Reader, Variant, Writer, V_DOUBLE};
use crate::repos::{Repos, StatRow};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

pub const STAT_INT: u8 = 1;
pub const STAT_DOUBLE: u8 = 2;
pub const PROFILE_GROUP: &str = "player_profile";
pub const LEVEL_KEY: &str = "level_stat:6:infinite";

pub fn decode_stat_group(body: &[u8]) -> Result<(String, Vec<StatRow>)> {
    let mut r = Reader::new(body);
    let name = r.string()?;
    let n = r.u32()?;
    let mut rows = vec![];
    for _ in 0..n {
        let key = r.string()?;
        let kind = r.u8()?;
        let value: [u8; 8] = r.take(8)?.try_into()?;
        rows.push(StatRow { key, kind, value });
    }
    if r.remaining() > 0 {
        bail!("{} trailing bytes after stat group", r.remaining());
    }
    Ok((name, rows))
}

pub fn encode_stat_group(name: &str, rows: &[StatRow]) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(name).u32(rows.len() as u32);
    for row in rows {
        w.string(&row.key).u8(row.kind).raw(&row.value);
    }
    w.take()
}

/// A stat's value: an integer or a float.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stat {
    Int(i64),
    Double(f64),
}

impl std::fmt::Display for Stat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stat::Int(i) => write!(f, "{i}"),
            Stat::Double(d) => f.write_str(&crate::util::py_float(*d)),
        }
    }
}

impl Stat {
    fn as_f64(self) -> f64 {
        match self {
            Stat::Int(i) => i as f64,
            Stat::Double(d) => d,
        }
    }

    pub fn encode(self) -> (u8, [u8; 8]) {
        match self {
            Stat::Int(i) => (STAT_INT, i.to_le_bytes()),
            Stat::Double(d) => (STAT_DOUBLE, d.to_le_bytes()),
        }
    }
}

pub fn stat_value(row: &StatRow) -> Stat {
    if row.kind == STAT_DOUBLE { Stat::Double(f64::from_le_bytes(row.value)) } else { Stat::Int(i64::from_le_bytes(row.value)) }
}

/// Wrap to signed 64 bits (bitfields use all 64 bits; uint64 inputs may exceed int64).
fn to_int64(v: i128) -> i64 {
    v as u64 as i64
}

/// One uploaded event: name + fields {key: variant}.
#[derive(Clone, Debug)]
pub struct StatEvent {
    pub name: String,
    pub fields: Vec<(String, Variant)>,
}

impl StatEvent {
    pub fn field(&self, key: &str) -> Option<&Variant> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn plain(&self) -> Map<String, Value> {
        rmc::variants_to_json(&self.fields)
    }

    /// A key template like "level_stat:{expertise_id}:infinite" filled from the fields (None: one is missing).
    fn fill(&self, template: &str) -> Option<String> {
        let mut out = String::new();
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            let end = start + rest[start..].find('}')?;
            out.push_str(&rest[..start]);
            out.push_str(&self.field(&rest[start + 1..end])?.to_text());
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        Some(out)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Set,
    Max,
    Add,
    Or,
}

/// event -> (stat key template filled from the event's fields, combine mode, field holding the value)
const EVENT_RULES: &[(&str, &str, Mode, &str)] = &[
    ("update_level", "level_stat:{expertise_id}:infinite", Mode::Max, "level_expertise"),
    ("update_xp", "xp_ratio_stat:{expertise_id}:infinite", Mode::Set, "xp_ratio_expertise"),
    ("update_player_playtime", "player_playtime:infinite", Mode::Max, "playtime"),
    ("complete_memorable_repeatable", "memorable_repeatable:{memorable_id}:infinite", Mode::Add, "count"),
    ("complete_memorable_exclusive", "memorable_exclusive:{type_id}:{mapping_id}:infinite", Mode::Or, "bitfield"),
    ("unlock_exploration", "unlockable_exploration:{type_id}:{mapping_id}:infinite", Mode::Or, "bitfield"),
    ("unlock_challenge", "unlockable_challenge:{mapping_id}:infinite", Mode::Or, "bitfield"),
];

/// Challenge results feed leaderboards, not player_profile: kept as a personal best per challenge. Only ranked
/// challenges have ghosts (as on Ubisoft's servers): their result comes with the id of the BEST_GHOST_* entity
/// the game made of the run, or "none" when it made none (the run beat no record). Player-made challenges
/// (id "<creator profile>_<savename>") have their own: *_challenge_score_event.
/// -> the direction ("descending": higher is better), for a score event.
pub fn score_direction(event: &str) -> Option<&'static str> {
    let rest = event.strip_suffix("_score_event")?;
    let (direction, kind) = if let Some(k) = rest.strip_prefix("descending_") {
        ("descending", k)
    } else {
        ("ascending", rest.strip_prefix("ascending_")?)
    };
    ["public_challenge", "public_challenge_ranked", "challenge", "challenge_ranked"].contains(&kind).then_some(direction)
}
pub const NO_REPLAY: &str = "none";

pub struct StatsService {
    repos: Arc<Repos>,
    unknown_logged: Mutex<HashSet<String>>,
}

impl StatsService {
    pub fn new(repos: Arc<Repos>) -> Self {
        StatsService { repos, unknown_logged: Mutex::default() }
    }

    pub fn group(&self, profile_id: &str, group: &str) -> Result<Vec<StatRow>> {
        self.repos.stat_group(profile_id, group)
    }

    /// GetStatisticGroup_V1 outputs. The reply names the group after the player's profile id.
    pub fn group_reply(&self, profile_id: &str, group: &str) -> Result<Vec<u8>> {
        Ok(encode_stat_group(profile_id, &self.group(profile_id, group)?))
    }

    pub fn get(&self, profile_id: &str, key: &str, group: &str) -> Result<Option<Stat>> {
        Ok(self.repos.stat(profile_id, group, key)?.map(|r| stat_value(&r)))
    }

    pub fn set(&self, profile_id: &str, key: &str, value: Stat, group: &str) -> Result<()> {
        let (kind, raw) = value.encode();
        self.repos.set_stat(profile_id, group, key, kind, raw)
    }

    /// Store a recorded GetStatisticGroup reply for a player; returns the number of stats.
    pub fn import_group(&self, profile_id: &str, group: &str, reply_body: &[u8]) -> Result<usize> {
        let (_, rows) = decode_stat_group(reply_body)?;
        self.repos.db.tx(|| {
            for row in &rows {
                self.repos.set_stat(profile_id, group, &row.key, row.kind, row.value)?;
            }
            Ok(rows.len())
        })
    }

    /// Record and apply uploaded events; returns how many stats changed.
    pub fn apply_events(&self, profile_id: &str, events: &[StatEvent], group: &str) -> Result<usize> {
        let stale = self.stale_expertises(profile_id, events, group)?;
        self.repos.db.tx(|| {
            let mut changed = 0;
            for ev in events {
                self.repos.add_stat_event(profile_id, &ev.name, &Value::Object(ev.plain()))?;
                if let Some(direction) = score_direction(&ev.name) {
                    self.record_score(profile_id, ev, direction)?;
                    continue;
                }
                let Some(&(_, template, mode, value_field)) = EVENT_RULES.iter().find(|r| r.0 == ev.name) else {
                    if self.unknown_logged.lock().insert(ev.name.clone()) {
                        log::warn!(target: "powder.stats", "unknown stat event {:?} {} (stored, not applied)", ev.name,
                                   Value::Object(ev.plain()));
                    }
                    continue;
                };
                let (Some(key), Some(value)) = (ev.fill(template), ev.field(value_field)) else {
                    log::warn!(target: "powder.stats", "stat event {} without the expected fields: {}", ev.name,
                               Value::Object(ev.plain()));
                    continue;
                };
                if ev.name == "update_xp" && ev.field("expertise_id").is_some_and(|e| stale.contains(&e.to_text())) {
                    continue;
                }
                changed += self.combine(profile_id, group, &key, mode, value)?;
            }
            Ok(changed)
        })
    }

    /// Sports whose uploaded level is below the stored one: the batch was computed from stale stats, so its xp
    /// ratios for those sports must not overwrite the real ones.
    fn stale_expertises(&self, profile_id: &str, events: &[StatEvent], group: &str) -> Result<HashSet<String>> {
        let mut stale = HashSet::new();
        for ev in events.iter().filter(|e| e.name == "update_level") {
            let (Some(e), Some(level)) = (ev.field("expertise_id"), ev.field("level_expertise").and_then(Variant::as_f64))
            else { continue };
            if let Some(row) = self.repos.stat(profile_id, group, &format!("level_stat:{}:infinite", e.to_text()))?
                && level < stat_value(&row).as_f64() {
                    stale.insert(e.to_text());
                }
        }
        Ok(stale)
    }

    fn combine(&self, profile_id: &str, group: &str, key: &str, mode: Mode, value: &Variant) -> Result<usize> {
        let row = self.repos.stat(profile_id, group, key)?;
        let stat_type = row.as_ref().map(|r| r.kind)
            .unwrap_or(if value.kind() == V_DOUBLE { STAT_DOUBLE } else { STAT_INT });
        let old = row.as_ref().map(stat_value);
        let new = if stat_type == STAT_DOUBLE {
            let Some(v) = value.as_f64() else { return Ok(0) };
            let old = old.map(Stat::as_f64);
            Stat::Double(match (old, mode) {
                (None, _) | (_, Mode::Set) => v,
                (Some(o), Mode::Max) => o.max(v),
                (Some(o), Mode::Add) => o + v,
                (Some(o), Mode::Or) => to_int64(o as i128 | v as i128) as f64,
            })
        } else {
            // int(value): floats truncate toward zero, as in Python
            let Some(v) = value.as_i128().or_else(|| value.as_f64().map(|f| f.trunc() as i128)) else { return Ok(0) };
            let v = to_int64(v);
            let old = old.map(|o| match o {
                Stat::Int(i) => i,
                Stat::Double(d) => d as i64,
            });
            Stat::Int(match (old, mode) {
                (None, _) | (_, Mode::Set) => v,
                (Some(o), Mode::Max) => o.max(v),
                (Some(o), Mode::Add) => o.wrapping_add(v),
                (Some(o), Mode::Or) => o | v,
            })
        };
        if row.is_some() && Some(new) == old {
            return Ok(0);
        }
        let (_, raw) = new.encode();
        self.repos.set_stat(profile_id, group, key, stat_type, raw)?;
        Ok(1)
    }

    fn record_score(&self, profile_id: &str, ev: &StatEvent, direction: &str) -> Result<()> {
        let score = match ev.field("score") {
            Some(Variant::String(s)) => s.trim().parse::<f64>().ok(),
            Some(v) => v.as_f64(),
            None => None,
        };
        let Some(score) = score else {
            log::warn!(target: "powder.stats", "challenge score event without a numeric score: {}",
                       Value::Object(ev.plain()));
            return Ok(());
        };
        let challenge = ev.field("challenge_id_to_use").map(Variant::to_text).unwrap_or_default();
        if challenge.is_empty() || ev.field("challenge_id_to_use") == Some(&Variant::None) {
            return Ok(());
        }
        let replay = ev.field("replay").map(Variant::to_text).filter(|r| !r.is_empty() && r != "None")
            .unwrap_or_else(|| NO_REPLAY.into());
        let best = self.repos.score(profile_id, &challenge)?;
        let better = match &best {
            None => true,
            Some(b) if direction == "descending" => score > b.best_score,
            Some(b) => score < b.best_score,
        };
        let ghost_of_best = best.as_ref().is_some_and(|b| score == b.best_score) && replay != NO_REPLAY;
        if better || ghost_of_best {
            self.repos.set_score(profile_id, &challenge, direction, score, Some(&replay))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const P: &str = "11111111-2222-3333-4444-555555555555";

    fn service() -> StatsService {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Repos::new(db);
        repos.create_player(P, P, "Tester").unwrap();
        StatsService::new(Arc::new(repos))
    }

    fn ev(name: &str, fields: &[(&str, Variant)]) -> StatEvent {
        StatEvent { name: name.into(), fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }
    }

    #[test]
    fn group_codec_round_trip() {
        let rows = vec![StatRow { key: LEVEL_KEY.into(), kind: STAT_INT, value: 7i64.to_le_bytes() },
                        StatRow { key: "xp".into(), kind: STAT_DOUBLE, value: 0.5f64.to_le_bytes() }];
        let body = encode_stat_group(P, &rows);
        assert_eq!(decode_stat_group(&body).unwrap(), (P.to_string(), rows));
        assert!(decode_stat_group(&[body, vec![0]].concat()).is_err());
    }

    #[test]
    fn rules_are_monotonic_and_stale_xp_is_ignored() {
        let s = service();
        let level = |e: i64, l: i64| ev("update_level", &[("expertise_id", Variant::Int(e)), ("level_expertise", Variant::Int(l))]);
        let xp = |e: i64, r: f64| ev("update_xp", &[("expertise_id", Variant::Int(e)), ("xp_ratio_expertise", Variant::Double(r))]);
        assert_eq!(s.apply_events(P, &[level(6, 10), xp(6, 0.25)], PROFILE_GROUP).unwrap(), 2);
        // a stale batch: lower level (kept at max) and its xp ratio not applied
        assert_eq!(s.apply_events(P, &[level(6, 3), xp(6, 0.9)], PROFILE_GROUP).unwrap(), 0);
        assert_eq!(s.get(P, LEVEL_KEY, PROFILE_GROUP).unwrap(), Some(Stat::Int(10)));
        assert_eq!(s.get(P, "xp_ratio_stat:6:infinite", PROFILE_GROUP).unwrap(), Some(Stat::Double(0.25)));
        let bits = |b: u64| ev("unlock_challenge", &[("mapping_id", Variant::Int(4)), ("bitfield", Variant::UInt(b))]);
        s.apply_events(P, &[bits(0b01), bits(0x8000_0000_0000_0000)], PROFILE_GROUP).unwrap();
        assert_eq!(s.get(P, "unlockable_challenge:4:infinite", PROFILE_GROUP).unwrap(),
                   Some(Stat::Int(0x8000_0000_0000_0001u64 as i64)));
        let count = ev("complete_memorable_repeatable", &[("memorable_id", Variant::String("m1".into())), ("count", Variant::Int(2))]);
        s.apply_events(P, &[count.clone(), count], PROFILE_GROUP).unwrap();
        assert_eq!(s.get(P, "memorable_repeatable:m1:infinite", PROFILE_GROUP).unwrap(), Some(Stat::Int(4)));
        // unknown events and ones missing fields are stored but not applied
        assert_eq!(s.apply_events(P, &[ev("mystery", &[]), ev("update_level", &[])], PROFILE_GROUP).unwrap(), 0);
        assert_eq!(s.repos.recent_stat_events(P, 100).unwrap().len(), 10);
    }

    #[test]
    fn challenge_scores_keep_the_best_and_its_ghost() {
        let s = service();
        let run = |score: Variant, replay: &str| ev("descending_challenge_ranked_score_event", &[
            ("challenge_id_to_use", Variant::String("c1".into())), ("score", score), ("replay", Variant::String(replay.into()))]);
        s.apply_events(P, &[run(Variant::Double(100.0), "none")], PROFILE_GROUP).unwrap();
        s.apply_events(P, &[run(Variant::Int(90), "g0")], PROFILE_GROUP).unwrap(); // worse: kept out
        assert_eq!(s.repos.score(P, "c1").unwrap().unwrap().replay.as_deref(), Some("none"));
        s.apply_events(P, &[run(Variant::String("100".into()), "g1")], PROFILE_GROUP).unwrap(); // the best's ghost
        let best = s.repos.score(P, "c1").unwrap().unwrap();
        assert_eq!((best.best_score, best.replay.as_deref(), best.direction.as_str()), (100.0, Some("g1"), "descending"));
        assert_eq!(score_direction("ascending_public_challenge_score_event"), Some("ascending"));
        assert_eq!(score_direction("descending_other_score_event"), None);
    }
}
