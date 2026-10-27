//! Bring a player's own recorded progress into the database (once): `powder-server admin import <recordings dir>
//! [--replace]`.
//!
//! From the recordings of their own sessions it takes:
//!   * identity      the recorded login reply (profile id, user id, name)
//!   * inventory     the recorded GET /v1/profiles/{id}/inventory snapshot, then every recorded purchase/reward
//!     reply after it (inventory-updates.json): the state at the END of the capture
//!   * statistics    the latest recorded NEX GetStatisticGroup_V1 reply (level, XP, medals)
//!   * entities      the player's recorded profile entities (rider card, favorites)
//!   * achievements  the earned achievements Ubisoft returned to Connect (demux ach_frontend)
//!
//! and makes that player the default one. The server never reads the recordings while running.

use super::stats::PROFILE_GROUP;
use super::Services;
use crate::assets::{HttpFixtures, RmcFixtures};
use crate::constants::CREDITS_ITEM;
use crate::demux::generated::{earned_from, product_name};
use crate::demux::pb::{self, F, V};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// A refused import (no recorded login, the player exists): printed as is by `admin import`.
#[derive(Debug)]
pub struct ImportError(pub String);

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImportError {}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportResult {
    pub profile_id: String,
    pub name: String,
    pub items: usize,
    pub credits: i64,
    pub stats: usize,
    pub entities: usize,
    pub achievements: usize,
}

/// The entities in a recorded reply body: {"entities": [...]} or one entity.
fn entities_in(body: &[u8]) -> Vec<Map<String, Value>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(d)) => match d.get("entities") {
            Some(Value::Array(list)) => list.iter().filter_map(|e| e.as_object().cloned()).collect(),
            _ if d.contains_key("entityId") => vec![d],
            _ => vec![],
        },
        _ => vec![],
    }
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("{}", path.display()))
}

/// Errors: [`ImportError`] (downcast) when the import is refused, anything else when the recordings are unreadable.
pub fn import_recordings(services: &Services, recordings: &Path, replace: bool) -> Result<ImportResult> {
    let http = HttpFixtures::load(&recordings.join("fixtures.json"))?;
    let rmc = RmcFixtures::load(&recordings.join("fixtures-rmc.json"))?;
    let login = match http.find("POST", r"/v3/profiles/sessions$").first() {
        Some(e) => e.json().context("the recorded login reply is not JSON")?,
        None => Value::Null,
    };
    let login = match login {
        Value::Object(m) if !m.is_empty() => m,
        _ => return Err(ImportError(format!("no recorded login in {}", recordings.join("fixtures.json").display())).into()),
    };
    let text = |k: &str| login.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let pid = text("profileId").context("the recorded login has no profileId")?.to_lowercase();
    let repos = &services.repos;
    if repos.player(&pid)?.is_some() {
        if !replace {
            return Err(ImportError(format!("player {} ({pid}) exists; use --replace to overwrite",
                                           text("nameOnPlatform").unwrap_or(""))).into());
        }
        repos.db.tx(|| {
            repos.delete_player(&pid)?;
            repos.clear_entities(&pid)?;
            repos.clear_achievements(&pid)
        })?;
    }

    let (credits, n_entities) = repos.db.tx(|| -> Result<(i64, usize)> {
        let short: String = pid.chars().take(4).collect();
        let name = text("nameOnPlatform").map(str::to_string).unwrap_or_else(|| format!("Player-{short}"));
        repos.create_player(&pid, text("userId").unwrap_or(&pid), &name)?;
        // inventory: snapshot (prefer the space-scoped one the game requests), then recorded updates
        let mut snaps = http.find("GET", &format!("/v1/profiles/{}/inventory$", regex::escape(&pid)));
        snaps.sort_by_key(|e| !e.path.contains("spaceId="));
        if let Some(snap) = snaps.first() {
            let body = snap.json().context("the recorded inventory is not JSON")?;
            for it in body.get("items").and_then(Value::as_array).context("the recorded inventory has no items")? {
                let s = |k: &str| it.get(k).and_then(Value::as_str);
                repos.set_inventory(&pid, s("itemId").context("inventory item without itemId")?,
                                    it.get("quantity").and_then(Value::as_i64).unwrap_or(0), s("lastModified"),
                                    s("expirationDate"))?;
            }
        }
        let updates = recordings.join("inventory-updates.json");
        if updates.exists() {
            for u in read_json(&updates)?.as_array().into_iter().flatten() {
                if u.get("profileId").and_then(Value::as_str).unwrap_or("").to_lowercase() != pid {
                    continue;
                }
                for it in u.pointer("/reply/items").and_then(Value::as_array).into_iter().flatten() {
                    let s = |k: &str| it.get(k).and_then(Value::as_str);
                    repos.set_inventory(&pid, s("itemId").context("inventory update without itemId")?,
                                        it.get("quantity").and_then(Value::as_i64).unwrap_or(0), s("lastModified"),
                                        None)?;
                }
            }
        }
        let credits = repos.quantity(&pid, CREDITS_ITEM)?;
        repos.add_ledger(&pid, CREDITS_ITEM, credits, credits, "import", None)?;
        let mut n_entities = 0;
        for e in &http.entries {
            if !(e.method == "GET" || e.method == "PUT") || !e.path.starts_with("/v2/profiles")
                || !e.path.contains("/entities") {
                continue;
            }
            for ent in entities_in(&e.body) {
                let owner = ent.get("profileId").and_then(Value::as_str).unwrap_or("").to_lowercase();
                let Some(id) = ent.get("entityId").and_then(Value::as_str) else { continue };
                if owner == pid && repos.entity_get(id)?.is_none() {
                    repos.entity_put(&ent)?;
                    n_entities += 1;
                }
            }
        }
        repos.kv_set("server", "default_player", &json!(pid))?;
        Ok((credits, n_entities))
    })?;
    let n_stats = match rmc.latest("PlayerStats2Protocol::GetStatisticGroup_V1").filter(|b| !b.is_empty()) {
        Some(body) => services.stats.import_group(&pid, PROFILE_GROUP, &body)?,
        None => 0,
    };
    let mut n_ach = 0;
    for (product, earned) in recorded_achievements(&recordings.join("demux.json"), &pid)? {
        n_ach += repos.earn_achievements(&pid, &product, &earned)?;
    }
    let result = ImportResult {
        profile_id: pid.clone(), name: text("nameOnPlatform").unwrap_or("").to_string(),
        items: repos.inventory(&pid)?.len(), credits, stats: n_stats, entities: n_entities, achievements: n_ach,
    };
    log::info!(target: "powder.import", "imported {} ({pid}): {} items, {credits} credits, {n_stats} stats, \
               {n_entities} entities, {n_ach} achievements", result.name, result.items);
    Ok(result)
}

/// The player's earned achievements from Ubisoft's recorded replies to Connect (ach_frontend read), per product in
/// recorded order.
fn recorded_achievements(demux_json: &Path, pid: &str) -> Result<Vec<(String, BTreeMap<i64, i64>)>> {
    if !demux_json.exists() {
        return Ok(vec![]);
    }
    let demux = read_json(demux_json)?;
    let replies = demux.pointer("/services/ach_frontend/3").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut result: Vec<(String, BTreeMap<i64, i64>)> = vec![];
    for b64 in replies.iter().filter_map(Value::as_str) {
        let raw = crate::util::unb64(b64).with_context(|| format!("{}: bad base64", demux_json.display()))?;
        let Some(fields) = pb::try_decode(&raw).and_then(|f| pb::sub(&f, 3)) else { continue };
        if fields.is_empty() || pb::text(&fields, 1).to_lowercase() != pid {
            continue;
        }
        for F(num, products) in pb::try_decode(pb::bytes(&fields, 2).unwrap_or_default()).unwrap_or_default() {
            let (1, V::Bytes(products)) = (num, products) else { continue };
            let Some(entry) = pb::try_decode(&products).filter(|e| !e.is_empty()) else { continue };
            let Some(name) = pb::bytes(&entry, 1).and_then(product_name) else { continue };
            let earned = earned_from(pb::bytes(&entry, 2).unwrap_or_default());
            match result.iter_mut().find(|(n, _)| *n == name) {
                Some((_, all)) => all.extend(earned),
                None => result.push((name, earned)),
            }
        }
    }
    Ok(result)
}

/// A small, entirely made-up recordings set (tests/synthetic.py on main: the parts the importer reads).
#[cfg(test)]
pub(crate) mod synthetic {
    use super::*;
    use crate::constants::SPACE_ID;
    use crate::repos::StatRow;
    use crate::services::stats::{encode_stat_group, Stat};

    pub const PLAYER: &str = "00000000-0000-4000-8000-000000000001";
    pub const PLAYER_NAME: &str = "TestPlayer";
    pub const START_CREDITS: i64 = 50_000;
    pub const SEEDED_CREDITS: i64 = 50_600;
    pub const LEVEL: i64 = 41;
    pub const BADGE_ITEM: &str = "0000000a-0000-4000-8000-00000000c001";
    pub const OWNED_ITEM: &str = "0000000a-0000-4000-8000-0000000000ff";
    pub const FRIEND: &str = "00000000-0000-4000-8000-000000000002";
    pub const CARD: &str = "0000000c-0000-4000-8000-000000000001";
    pub const FRIEND_CARD: &str = "0000000c-0000-4000-8000-000000000002";
    pub const ACHIEVEMENTS: [(i64, i64); 3] = [(1, 1_500_000_000), (4, 1_500_000_100), (15, 1_500_000_200)];
    const UBI: &str = "public-ubiservices.ubi.com";

    fn entity(id: &str, profile_id: &str, title: i64) -> Value {
        json!({"entityId": id, "profileId": profile_id, "spaceId": SPACE_ID, "type": "PROFILE", "editable": true,
               "name": "profile", "tags": ["STEEP"], "obj": {"RIDER_CARD": {"title": title}, "GLOBAL_VERSION": 7},
               "lastModified": "2026-01-01T00:00:00.000Z", "revision": 3})
    }

    fn http(method: &str, path: &str, body: Value) -> Value {
        json!({"method": method, "host": UBI, "path": path, "status": 200, "content_type": "application/json",
               "body_b64": crate::util::b64(crate::util::dumps(&body).as_bytes())})
    }

    pub fn stat_reply() -> Vec<u8> {
        let row = |key: &str, v: Stat| {
            let (kind, value) = v.encode();
            StatRow { key: key.into(), kind, value }
        };
        encode_stat_group(PLAYER, &[row("level_stat:6:infinite", Stat::Int(LEVEL)),
                                    row("xp_ratio_stat:0:infinite", Stat::Double(0.5))])
    }

    fn achievements_reply() -> Vec<u8> {
        let entries = ACHIEVEMENTS.iter().map(|&(i, t)| F::msg(1, vec![F::int(1, i), F::int(2, t)])).collect();
        let product = F::msg(1, vec![F::msg(1, vec![F::str(1, "STP"), F::str(2, "PC")]), F::msg(2, entries)]);
        pb::encode(&[F::int(1, 2), F::msg(3, vec![F::str(1, PLAYER), F::msg(2, vec![product])])])
    }

    pub fn build(folder: &Path) {
        let login = json!({"platformType": "uplay", "ticket": "recorded-ticket", "profileId": PLAYER, "userId": PLAYER,
                           "nameOnPlatform": PLAYER_NAME, "environment": "Prod", "spaceId": SPACE_ID,
                           "sessionKey": "c2Vzc2lvbg==", "rememberMeTicket": null});
        let item = |id: &str, q: i64| json!({"itemId": id, "quantity": q, "expirationDate": null,
                                             "lastModified": "2026-01-01T00:00:00.000Z", "expirationDetails": null});
        let fixtures = json!([
            http("POST", "/v3/profiles/sessions", login),
            http("GET", &format!("/v1/profiles/{PLAYER}/inventory?spaceId={SPACE_ID}"),
                 json!({"items": [item(CREDITS_ITEM, START_CREDITS), item(OWNED_ITEM, 1)]})),
            http("GET", &format!("/v2/profiles/{PLAYER}/entities?type=PROFILE&spaceId={SPACE_ID}&offset=0&limit=5"),
                 json!({"entities": [entity(CARD, PLAYER, 1)]})),
            http("GET", &format!("/v2/profiles/entities?profileIds={FRIEND}&type=PROFILE&spaceId={SPACE_ID}"),
                 json!({"entities": [entity(FRIEND_CARD, FRIEND, 2)]})),
            http("PUT", "/v1/profiles/me/populations/data", json!({"recorded": "populations"})),
        ]);
        std::fs::write(folder.join("fixtures.json"), crate::util::dumps(&fixtures)).unwrap();
        let rmc = json!({
            "PlayerStats2Protocol::GetStatisticGroup_V1": [
                {"params_sha1": "00", "params_len": 0, "body": hex::encode(stat_reply())}],
            "MatchmakingProtocol::LeaveSession_V1": [
                {"params_sha1": "01", "params_len": 4, "body": ""}, {"params_sha1": "02", "params_len": 4, "body": ""}],
        });
        std::fs::write(folder.join("fixtures-rmc.json"), crate::util::dumps(&rmc)).unwrap();
        let demux = json!({"client_version": 13368, "services": {
            "ach_frontend": {"3": [crate::util::b64(&achievements_reply())]}}});
        std::fs::write(folder.join("demux.json"), crate::util::dumps(&demux)).unwrap();
        let updates = json!([{"time": 1.0, "profileId": PLAYER,
            "request": {"spaceId": SPACE_ID, "offerId": "0000000b-0000-4000-8000-0000000000c1", "offerRevision": 6},
            "reply": {"items": [
                {"itemId": CREDITS_ITEM, "quantityBefore": START_CREDITS, "quantity": SEEDED_CREDITS,
                 "lastModified": "2026-01-01T01:00:00.000Z"},
                {"itemId": BADGE_ITEM, "quantityBefore": 0, "quantity": 1, "lastModified": "2026-01-01T01:00:00.000Z"}],
                "transactionId": "00000000-0000-4000-8000-0000000000f1"}}]);
        std::fs::write(folder.join("inventory-updates.json"), crate::util::dumps(&updates)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::synthetic as syn;
    use super::*;
    use crate::config::Config;
    use crate::services::stats::{Stat, LEVEL_KEY};

    fn setup() -> (tempfile::TempDir, Services) {
        let dir = tempfile::tempdir().unwrap();
        syn::build(dir.path());
        (dir, Services::memory(Config::default()))
    }

    fn imported() -> (tempfile::TempDir, Services) {
        let (dir, s) = setup();
        import_recordings(&s, dir.path(), false).unwrap();
        (dir, s)
    }

    fn refused(r: Result<ImportResult>) -> String {
        r.unwrap_err().downcast::<ImportError>().expect("an ImportError").0
    }

    #[test]
    fn import_from_recordings() {
        let (dir, s) = setup();
        let r = import_recordings(&s, dir.path(), false).unwrap();
        assert_eq!(r, ImportResult { profile_id: syn::PLAYER.into(), name: syn::PLAYER_NAME.into(), items: 3,
                                     credits: syn::SEEDED_CREDITS, stats: 2, entities: 1, achievements: 3 });
        assert_eq!(s.repos.player(syn::PLAYER).unwrap().unwrap().name, syn::PLAYER_NAME);
        assert_eq!(s.stats.get(syn::PLAYER, LEVEL_KEY, PROFILE_GROUP).unwrap(), Some(Stat::Int(syn::LEVEL)));
        assert_eq!(s.economy.credits(syn::PLAYER).unwrap(), syn::SEEDED_CREDITS); // snapshot + update
        assert_eq!(s.repos.quantity(syn::PLAYER, syn::BADGE_ITEM).unwrap(), 1);
        assert_eq!(s.repos.recent_ledger(syn::PLAYER, 1).unwrap()[0].reason, "import");
        // only the player's own entities; the friend's rider card stays out
        assert!(s.repos.entity_get(syn::CARD).unwrap().is_some());
        assert!(s.repos.entity_get(syn::FRIEND_CARD).unwrap().is_none());
        assert_eq!(s.repos.achievements(syn::PLAYER, "STP:PC").unwrap(), syn::ACHIEVEMENTS.to_vec());
    }

    #[test]
    fn import_makes_the_recorded_player_the_default() {
        let (_dir, s) = imported();
        assert_eq!(s.accounts.default_profile_id().unwrap(), syn::PLAYER);
    }

    #[test]
    fn stat_group_reply_is_byte_identical_to_recording() {
        let (_dir, s) = imported();
        assert_eq!(s.stats.group_reply(syn::PLAYER, PROFILE_GROUP).unwrap(), syn::stat_reply());
    }

    #[test]
    fn reimport_with_replace_restores_recorded_state() {
        let (dir, s) = imported();
        s.economy.grant(syn::PLAYER, CREDITS_ITEM, -12_000, "purchase").unwrap().unwrap();
        s.entities.put(syn::CARD, json!({"obj": {"RIDER_CARD": {"title": 5}}}).as_object().unwrap(),
                       Some(syn::PLAYER)).unwrap();
        assert_eq!(s.entities.get(syn::CARD).unwrap().unwrap()["obj"]["RIDER_CARD"]["title"], 5);
        assert_eq!(refused(import_recordings(&s, dir.path(), false)),
                   format!("player {} ({}) exists; use --replace to overwrite", syn::PLAYER_NAME, syn::PLAYER));
        import_recordings(&s, dir.path(), true).unwrap();
        assert_eq!(s.economy.credits(syn::PLAYER).unwrap(), syn::SEEDED_CREDITS);
        let cards = s.entities.for_profile(syn::PLAYER, None, None, 0, 100).unwrap();
        assert_eq!(cards[0]["obj"]["RIDER_CARD"]["title"], 1); // recorded again
    }

    #[test]
    fn no_recorded_login_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Services::memory(Config::default());
        assert_eq!(refused(import_recordings(&s, dir.path(), false)),
                   format!("no recorded login in {}", dir.path().join("fixtures.json").display()));
    }

    #[test]
    fn entities_in_replies() {
        assert_eq!(entities_in(br#"{"entities": [{"entityId": "a"}, 3]}"#).len(), 1);
        assert_eq!(entities_in(br#"{"entityId": "a", "obj": {}}"#).len(), 1);
        assert!(entities_in(br#"{"entities": {}}"#).is_empty() && entities_in(b"[1]").is_empty());
        assert!(entities_in(b"not json").is_empty());
    }
}
