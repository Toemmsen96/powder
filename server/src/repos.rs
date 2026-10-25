//! Thin repositories over the shared database. Game logic lives in services/, not here. Ids are stored
//! lower-case.

use crate::db::Db;
use crate::util::iso_now;
use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerRow {
    pub profile_id: String,
    pub user_id: String,
    pub name: String,
    pub created_at: String,
    pub settings: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InventoryRow {
    pub item_id: String,
    pub quantity: i64,
    pub expiration_date: Option<String>,
    pub last_modified: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatRow {
    pub key: String,
    pub kind: u8,
    /// raw 8 bytes as on the wire
    pub value: [u8; 8],
}

#[derive(Clone, Debug, PartialEq)]
pub struct AccountRow {
    pub account_id: String,
    pub email: String,
    pub password_hash: String,
    pub profile_id: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScoreRow {
    pub profile_id: String,
    pub challenge_id: String,
    pub direction: String,
    pub best_score: f64,
    pub replay: Option<String>,
    pub updated: String,
}

#[derive(Clone, Debug)]
pub struct LedgerRow {
    pub ts: String,
    pub offer_id: Option<String>,
    pub item_id: String,
    pub delta: i64,
    pub balance: i64,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct StatEventRow {
    pub ts: String,
    pub name: String,
    pub fields_json: String,
}

/// A player-made content item (photo, ...).
#[derive(Clone, Debug)]
pub struct UgcItem {
    pub ugc_id: String,
    pub type_id: i64,
    pub world_id: Option<i64>,
    pub profile_id: String,
    pub pub_date: String,
    pub metadata: Value,
    pub tags: Value,
    pub extra: Map<String, Value>,
    pub hidden: bool,
    pub downloads: i64,
    pub liked_by: BTreeSet<String>,
}

fn lc(s: &str) -> String {
    s.to_lowercase()
}

fn parse_json(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

pub struct Repos {
    pub db: Arc<Db>,
}

impl Repos {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    // ---- players ------------------------------------------------------------------------------------
    fn player_row(r: &Row) -> rusqlite::Result<PlayerRow> {
        Ok(PlayerRow { profile_id: r.get("profile_id")?, user_id: r.get("user_id")?, name: r.get("name")?,
                       created_at: r.get("created_at")?, settings: parse_json(r.get("settings_json")?) })
    }

    pub fn player(&self, profile_id: &str) -> Result<Option<PlayerRow>> {
        self.db.query_one("SELECT * FROM players WHERE profile_id = ?", [lc(profile_id)], Self::player_row)
    }

    pub fn players(&self) -> Result<Vec<PlayerRow>> {
        self.db.query("SELECT * FROM players ORDER BY created_at", [], Self::player_row)
    }

    pub fn create_player(&self, profile_id: &str, user_id: &str, name: &str) -> Result<()> {
        self.db.execute("INSERT INTO players(profile_id, user_id, name, created_at) VALUES (?,?,?,?)",
                        params![lc(profile_id), lc(user_id), name, iso_now()])?;
        Ok(())
    }

    pub fn delete_player(&self, profile_id: &str) -> Result<()> {
        self.db.execute("DELETE FROM players WHERE profile_id = ?", [lc(profile_id)])?;
        Ok(())
    }

    pub fn rename_player(&self, profile_id: &str, name: &str) -> Result<()> {
        self.db.execute("UPDATE players SET name = ? WHERE profile_id = ?", params![name, lc(profile_id)])?;
        Ok(())
    }

    pub fn player_by_name(&self, name: &str) -> Result<Option<PlayerRow>> {
        self.db.query_one("SELECT * FROM players WHERE name = ? COLLATE NOCASE", [name], Self::player_row)
    }

    // ---- inventory ----------------------------------------------------------------------------------
    fn inventory_row(r: &Row) -> rusqlite::Result<InventoryRow> {
        Ok(InventoryRow { item_id: r.get("item_id")?, quantity: r.get("quantity")?,
                          expiration_date: r.get("expiration_date")?, last_modified: r.get("last_modified")? })
    }

    pub fn inventory(&self, profile_id: &str) -> Result<Vec<InventoryRow>> {
        self.db.query("SELECT * FROM inventory WHERE profile_id = ? ORDER BY item_id DESC", [lc(profile_id)],
                      Self::inventory_row)
    }

    pub fn quantity(&self, profile_id: &str, item_id: &str) -> Result<i64> {
        Ok(self.db.query_one("SELECT quantity FROM inventory WHERE profile_id = ? AND item_id = ?",
                             [lc(profile_id), lc(item_id)], |r| r.get(0))?.unwrap_or(0))
    }

    pub fn set_inventory(&self, profile_id: &str, item_id: &str, quantity: i64, last_modified: Option<&str>,
                         expiration_date: Option<&str>) -> Result<()> {
        self.db.execute(
            "INSERT INTO inventory(profile_id, item_id, quantity, expiration_date, last_modified) VALUES (?,?,?,?,?) \
             ON CONFLICT(profile_id, item_id) DO UPDATE SET quantity = excluded.quantity, \
             expiration_date = excluded.expiration_date, last_modified = excluded.last_modified",
            params![lc(profile_id), lc(item_id), quantity, expiration_date,
                    last_modified.map(str::to_string).unwrap_or_else(iso_now)])?;
        Ok(())
    }

    // ---- stats --------------------------------------------------------------------------------------
    fn stat_row(r: &Row) -> rusqlite::Result<StatRow> {
        let raw: Vec<u8> = r.get("value")?;
        let mut value = [0u8; 8];
        value.copy_from_slice(&raw[..8.min(raw.len())]);
        Ok(StatRow { key: r.get("key")?, kind: r.get("type")?, value })
    }

    pub fn stat_group(&self, profile_id: &str, group: &str) -> Result<Vec<StatRow>> {
        self.db.query("SELECT key, type, value FROM stats WHERE profile_id = ? AND group_name = ? ORDER BY rowid",
                      params![lc(profile_id), group], Self::stat_row)
    }

    pub fn stat(&self, profile_id: &str, group: &str, key: &str) -> Result<Option<StatRow>> {
        self.db.query_one("SELECT key, type, value FROM stats WHERE profile_id = ? AND group_name = ? AND key = ?",
                          params![lc(profile_id), group, key], Self::stat_row)
    }

    pub fn set_stat(&self, profile_id: &str, group: &str, key: &str, kind: u8, value: [u8; 8]) -> Result<()> {
        self.db.execute(
            "INSERT INTO stats(profile_id, group_name, key, type, value) VALUES (?,?,?,?,?) \
             ON CONFLICT(profile_id, group_name, key) DO UPDATE SET type = excluded.type, value = excluded.value",
            params![lc(profile_id), group, key, kind, value.to_vec()])?;
        Ok(())
    }

    // ---- stat events, ledger, kv ----------------------------------------------------------------------
    pub fn add_stat_event(&self, profile_id: &str, name: &str, fields: &Value) -> Result<()> {
        self.db.execute("INSERT INTO stat_events(profile_id, ts, name, fields_json) VALUES (?,?,?,?)",
                        params![lc(profile_id), iso_now(), name, crate::util::dumps(fields)])?;
        Ok(())
    }

    pub fn recent_stat_events(&self, profile_id: &str, limit: i64) -> Result<Vec<StatEventRow>> {
        self.db.query("SELECT ts, name, fields_json FROM stat_events WHERE profile_id = ? ORDER BY id DESC LIMIT ?",
                      params![lc(profile_id), limit],
                      |r| Ok(StatEventRow { ts: r.get(0)?, name: r.get(1)?, fields_json: r.get(2)? }))
    }

    pub fn add_ledger(&self, profile_id: &str, item_id: &str, delta: i64, balance: i64, reason: &str,
                      offer_id: Option<&str>) -> Result<()> {
        self.db.execute("INSERT INTO ledger(profile_id, ts, offer_id, item_id, delta, balance, reason) \
                         VALUES (?,?,?,?,?,?,?)",
                        params![lc(profile_id), iso_now(), offer_id, lc(item_id), delta, balance, reason])?;
        Ok(())
    }

    pub fn recent_ledger(&self, profile_id: &str, limit: i64) -> Result<Vec<LedgerRow>> {
        self.db.query("SELECT ts, offer_id, item_id, delta, balance, reason FROM ledger WHERE profile_id = ? \
                       ORDER BY id DESC LIMIT ?", params![lc(profile_id), limit],
                      |r| Ok(LedgerRow { ts: r.get(0)?, offer_id: r.get(1)?, item_id: r.get(2)?, delta: r.get(3)?,
                                         balance: r.get(4)?, reason: r.get(5)? }))
    }

    pub fn kv_get(&self, scope: &str, key: &str) -> Result<Option<Value>> {
        Ok(self.db.query_one("SELECT value_json FROM kv WHERE scope = ? AND key = ?", [scope, key],
                             |r| r.get::<_, String>(0))?.map(parse_json))
    }

    pub fn kv_set(&self, scope: &str, key: &str, value: &Value) -> Result<()> {
        self.db.execute("INSERT INTO kv(scope, key, value_json) VALUES (?,?,?) ON CONFLICT(scope, key) \
                         DO UPDATE SET value_json = excluded.value_json",
                        params![scope, key, crate::util::dumps(value)])?;
        Ok(())
    }

    // ---- achievements -------------------------------------------------------------------------------
    /// Record {achievement id: unix time}; keeps the earliest time. Returns how many were new.
    pub fn earn_achievements(&self, profile_id: &str, product: &str, earned: &BTreeMap<i64, i64>) -> Result<usize> {
        let before = self.achievements(profile_id, product)?.len();
        self.db.tx(|| {
            for (id, ts) in earned {
                self.db.execute(
                    "INSERT INTO achievements(profile_id, product, achievement_id, earned_at) VALUES (?,?,?,?) \
                     ON CONFLICT(profile_id, product, achievement_id) DO UPDATE SET \
                     earned_at = MIN(earned_at, excluded.earned_at)",
                    params![lc(profile_id), product, id, ts])?;
            }
            Ok(())
        })?;
        Ok(self.achievements(profile_id, product)?.len() - before)
    }

    /// (achievement id, unix time), earliest first.
    pub fn achievements(&self, profile_id: &str, product: &str) -> Result<Vec<(i64, i64)>> {
        self.db.query("SELECT achievement_id, earned_at FROM achievements WHERE profile_id = ? AND product = ? \
                       ORDER BY earned_at, achievement_id", params![lc(profile_id), product],
                      |r| Ok((r.get(0)?, r.get(1)?)))
    }

    pub fn achievement_products(&self, profile_id: &str) -> Result<Vec<(String, i64)>> {
        self.db.query("SELECT product, COUNT(*) FROM achievements WHERE profile_id = ? GROUP BY product",
                      [lc(profile_id)], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    pub fn clear_achievements(&self, profile_id: &str) -> Result<()> {
        self.db.execute("DELETE FROM achievements WHERE profile_id = ?", [lc(profile_id)])?;
        Ok(())
    }

    // ---- challenge scores ---------------------------------------------------------------------------
    fn score_row(r: &Row) -> rusqlite::Result<ScoreRow> {
        Ok(ScoreRow { profile_id: r.get("profile_id")?, challenge_id: r.get("challenge_id")?,
                      direction: r.get("direction")?, best_score: r.get("best_score")?, replay: r.get("replay")?,
                      updated: r.get("updated")? })
    }

    pub fn score(&self, profile_id: &str, challenge_id: &str) -> Result<Option<ScoreRow>> {
        self.db.query_one("SELECT * FROM challenge_scores WHERE profile_id = ? AND challenge_id = ?",
                          params![lc(profile_id), challenge_id], Self::score_row)
    }

    pub fn set_score(&self, profile_id: &str, challenge_id: &str, direction: &str, score: f64,
                     replay: Option<&str>) -> Result<()> {
        self.db.execute(
            "INSERT INTO challenge_scores(profile_id, challenge_id, direction, best_score, replay, updated) \
             VALUES (?,?,?,?,?,?) ON CONFLICT(profile_id, challenge_id) DO UPDATE SET direction = excluded.direction, \
             best_score = excluded.best_score, replay = excluded.replay, updated = excluded.updated",
            params![lc(profile_id), challenge_id, direction, score, replay, iso_now()])?;
        Ok(())
    }

    pub fn scores(&self, profile_id: &str) -> Result<Vec<ScoreRow>> {
        self.db.query("SELECT * FROM challenge_scores WHERE profile_id = ? ORDER BY challenge_id",
                      [lc(profile_id)], Self::score_row)
    }

    /// Every player's best on one challenge (for its leaderboard).
    pub fn scores_for_challenge(&self, challenge_id: &str) -> Result<Vec<ScoreRow>> {
        self.db.query("SELECT * FROM challenge_scores WHERE challenge_id = ?", [challenge_id], Self::score_row)
    }

    // ---- profile entities ---------------------------------------------------------------------------
    /// Row -> UbiServices entity (entityId, profileId, spaceId, type, editable, name, tags, obj,
    /// lastModified, revision[, extendedStorage]).
    fn entity(r: &Row) -> rusqlite::Result<Map<String, Value>> {
        let mut e = Map::new();
        e.insert("entityId".into(), json!(r.get::<_, String>("entity_id")?));
        e.insert("profileId".into(), json!(r.get::<_, String>("profile_id")?));
        e.insert("spaceId".into(), json!(r.get::<_, String>("space_id")?));
        e.insert("type".into(), json!(r.get::<_, String>("type")?));
        e.insert("editable".into(), json!(r.get::<_, i64>("editable")? != 0));
        e.insert("name".into(), json!(r.get::<_, String>("name")?));
        e.insert("tags".into(), parse_json(r.get("tags_json")?));
        e.insert("obj".into(), parse_json(r.get("obj_json")?));
        e.insert("lastModified".into(), json!(r.get::<_, String>("last_modified")?));
        e.insert("revision".into(), json!(r.get::<_, i64>("revision")?));
        if let Some(ext) = r.get::<_, Option<String>>("extended_json")? {
            e.insert("extendedStorage".into(), parse_json(ext));
        }
        Ok(e)
    }

    pub fn entity_get(&self, entity_id: &str) -> Result<Option<Map<String, Value>>> {
        self.db.query_one("SELECT * FROM profile_entities WHERE entity_id = ?", [lc(entity_id)], Self::entity)
    }

    pub fn entities_of(&self, profile_id: &str, kind: Option<&str>) -> Result<Vec<Map<String, Value>>> {
        match kind {
            Some(t) => self.db.query("SELECT * FROM profile_entities WHERE profile_id = ? AND type = ? \
                                      ORDER BY last_modified", params![lc(profile_id), t], Self::entity),
            None => self.db.query("SELECT * FROM profile_entities WHERE profile_id = ? ORDER BY last_modified",
                                  [lc(profile_id)], Self::entity),
        }
    }

    pub fn entity_put(&self, e: &Map<String, Value>) -> Result<()> {
        let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let tags = e.get("tags").filter(|v| !v.is_null()).cloned().unwrap_or(json!([]));
        let obj = e.get("obj").filter(|v| !v.is_null()).cloned().unwrap_or(json!({}));
        let editable = e.get("editable").map(truthy).unwrap_or(true);
        let revision = e.get("revision").and_then(Value::as_i64).filter(|r| *r != 0).unwrap_or(1);
        let modified = e.get("lastModified").and_then(Value::as_str).filter(|s| !s.is_empty())
            .map(str::to_string).unwrap_or_else(iso_now);
        self.db.execute(
            "INSERT INTO profile_entities(entity_id, profile_id, space_id, type, name, editable, tags_json, \
             obj_json, extended_json, revision, last_modified) VALUES (?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(entity_id) DO UPDATE SET profile_id = excluded.profile_id, space_id = excluded.space_id, \
             type = excluded.type, name = excluded.name, editable = excluded.editable, tags_json = excluded.tags_json, \
             obj_json = excluded.obj_json, extended_json = excluded.extended_json, revision = excluded.revision, \
             last_modified = excluded.last_modified",
            params![lc(&s("entityId")), lc(&s("profileId")), s("spaceId"), s("type"), s("name"), editable as i64,
                    crate::util::dumps(&tags), crate::util::dumps(&obj),
                    e.get("extendedStorage").map(crate::util::dumps), revision, modified])?;
        Ok(())
    }

    pub fn entity_delete(&self, entity_id: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM profile_entities WHERE entity_id = ?", [lc(entity_id)])? > 0)
    }

    pub fn clear_entities(&self, profile_id: &str) -> Result<()> {
        self.db.execute("DELETE FROM profile_entities WHERE profile_id = ?", [lc(profile_id)])?;
        Ok(())
    }

    // ---- accounts -----------------------------------------------------------------------------------
    fn account_row(r: &Row) -> rusqlite::Result<AccountRow> {
        Ok(AccountRow { account_id: r.get("account_id")?, email: r.get("email")?, password_hash: r.get("password_hash")?,
                        profile_id: r.get("profile_id")?, created_at: r.get("created_at")?,
                        updated_at: r.get("updated_at")? })
    }

    pub fn account(&self, account_id: &str) -> Result<Option<AccountRow>> {
        self.db.query_one("SELECT * FROM accounts WHERE account_id = ?", [lc(account_id)], Self::account_row)
    }

    pub fn account_by_email(&self, email: &str) -> Result<Option<AccountRow>> {
        self.db.query_one("SELECT * FROM accounts WHERE email = ?", [email.trim()], Self::account_row)
    }

    pub fn account_by_profile(&self, profile_id: &str) -> Result<Option<AccountRow>> {
        self.db.query_one("SELECT * FROM accounts WHERE profile_id = ?", [lc(profile_id)], Self::account_row)
    }

    pub fn accounts(&self) -> Result<Vec<AccountRow>> {
        self.db.query("SELECT * FROM accounts ORDER BY created_at", [], Self::account_row)
    }

    pub fn account_count(&self) -> Result<i64> {
        Ok(self.db.query_one("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?.unwrap_or(0))
    }

    pub fn create_account(&self, account_id: &str, email: &str, password_hash: &str, profile_id: &str) -> Result<()> {
        let now = iso_now();
        self.db.execute("INSERT INTO accounts(account_id, email, password_hash, profile_id, created_at, updated_at) \
                         VALUES (?,?,?,?,?,?)",
                        params![lc(account_id), email.trim(), password_hash, lc(profile_id), now, now])?;
        Ok(())
    }

    /// Change one of email, password_hash, profile_id.
    pub fn update_account(&self, account_id: &str, field: AccountField, value: &str) -> Result<()> {
        let column = match field {
            AccountField::Email => "email",
            AccountField::PasswordHash => "password_hash",
            AccountField::ProfileId => "profile_id",
        };
        let value = if field == AccountField::ProfileId { lc(value) } else { value.to_string() };
        self.db.execute(&format!("UPDATE accounts SET {column} = ?, updated_at = ? WHERE account_id = ?"),
                        params![value, iso_now(), lc(account_id)])?;
        Ok(())
    }

    pub fn delete_account(&self, account_id: &str) -> Result<()> {
        self.db.execute("DELETE FROM accounts WHERE account_id = ?", [lc(account_id)])?;
        Ok(())
    }

    // ---- remember-me tickets ------------------------------------------------------------------------
    /// Per account the newest ones are kept (one per Connect sign-in).
    pub const KEEP_TICKETS: i64 = 20;

    pub fn add_remember_ticket(&self, ticket: &str, account_id: &str) -> Result<()> {
        let aid = lc(account_id);
        self.db.tx(|| {
            self.db.execute("INSERT OR REPLACE INTO remember_tickets(ticket, account_id, created_at) VALUES (?,?,?)",
                            params![ticket, aid, iso_now()])?;
            self.db.execute("DELETE FROM remember_tickets WHERE account_id = ? AND rowid NOT IN (SELECT rowid \
                             FROM remember_tickets WHERE account_id = ? ORDER BY rowid DESC LIMIT ?)",
                            params![aid, aid, Self::KEEP_TICKETS])?;
            Ok(())
        })
    }

    pub fn remember_ticket_account(&self, ticket: &str) -> Result<Option<String>> {
        self.db.query_one("SELECT account_id FROM remember_tickets WHERE ticket = ?", [ticket], |r| r.get(0))
    }

    pub fn revoke_remember_tickets(&self, account_id: &str) -> Result<usize> {
        self.db.execute("DELETE FROM remember_tickets WHERE account_id = ?", [lc(account_id)])
    }

    // ---- player-made content ------------------------------------------------------------------------
    fn ugc_item(&self, r: &Row) -> rusqlite::Result<UgcItem> {
        Ok(UgcItem {
            ugc_id: r.get("ugc_id")?, type_id: r.get("type_id")?, world_id: r.get("world_id")?,
            profile_id: r.get("profile_id")?, pub_date: r.get("pub_date")?,
            metadata: parse_json(r.get("metadata_json")?), tags: parse_json(r.get("tags_json")?),
            extra: match parse_json(r.get("extra_json")?) { Value::Object(m) => m, _ => Map::new() },
            hidden: r.get::<_, i64>("hidden")? != 0, downloads: r.get("downloads")?, liked_by: BTreeSet::new(),
        })
    }

    fn with_likes(&self, mut item: UgcItem) -> Result<UgcItem> {
        item.liked_by = self.db.query("SELECT profile_id FROM ugc_likes WHERE ugc_id = ?", [&item.ugc_id],
                                      |r| r.get::<_, String>(0))?.into_iter().collect();
        Ok(item)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ugc_create(&self, ugc_id: &str, type_id: i64, world_id: Option<i64>, profile_id: &str, metadata: &Value,
                      tags: &Value, extra: &Map<String, Value>) -> Result<UgcItem> {
        self.db.execute(
            "INSERT INTO ugc(ugc_id, type_id, world_id, profile_id, pub_date, metadata_json, tags_json, extra_json) \
             VALUES (?,?,?,?,?,?,?,?)",
            params![ugc_id, type_id, world_id, lc(profile_id), iso_now(), crate::util::dumps(metadata),
                    crate::util::dumps(tags), crate::util::dumps(&Value::Object(extra.clone()))])?;
        Ok(self.ugc_get(ugc_id)?.expect("just created"))
    }

    pub fn ugc_get(&self, ugc_id: &str) -> Result<Option<UgcItem>> {
        let item = self.db.with(|c| {
            c.query_row("SELECT * FROM ugc WHERE ugc_id = ?", [ugc_id], |r| self.ugc_item(r)).optional()
        })?;
        item.map(|i| self.with_likes(i)).transpose()
    }

    pub fn ugc_find(&self, type_id: Option<i64>, world_id: Option<i64>, profile_ids: &[String]) -> Result<Vec<UgcItem>> {
        let mut sql = String::from("SELECT * FROM ugc WHERE hidden = 0");
        let mut args: Vec<rusqlite::types::Value> = vec![];
        if let Some(t) = type_id {
            sql += " AND type_id = ?";
            args.push(t.into());
        }
        if let Some(w) = world_id {
            sql += " AND world_id = ?";
            args.push(w.into());
        }
        if !profile_ids.is_empty() {
            sql += &format!(" AND profile_id IN ({})", vec!["?"; profile_ids.len()].join(","));
            args.extend(profile_ids.iter().map(|p| lc(p).into()));
        }
        sql += " ORDER BY pub_date DESC";
        let items = self.db.with(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| self.ugc_item(r))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        items.into_iter().map(|i| self.with_likes(i)).collect()
    }

    pub fn ugc_delete(&self, ugc_id: &str) -> Result<()> {
        self.db.execute("DELETE FROM ugc WHERE ugc_id = ?", [ugc_id])?;
        Ok(())
    }

    pub fn ugc_hide(&self, ugc_id: &str) -> Result<()> {
        self.db.execute("UPDATE ugc SET hidden = 1 WHERE ugc_id = ?", [ugc_id])?;
        Ok(())
    }

    pub fn ugc_downloaded(&self, ugc_id: &str) -> Result<()> {
        self.db.execute("UPDATE ugc SET downloads = downloads + 1 WHERE ugc_id = ?", [ugc_id])?;
        Ok(())
    }

    pub fn ugc_like(&self, ugc_id: &str, profile_id: &str, liked: bool) -> Result<()> {
        if liked {
            self.db.execute("INSERT OR IGNORE INTO ugc_likes(ugc_id, profile_id) VALUES (?,?)",
                            params![ugc_id, lc(profile_id)])?;
        } else {
            self.db.execute("DELETE FROM ugc_likes WHERE ugc_id = ? AND profile_id = ?", params![ugc_id, lc(profile_id)])?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AccountField {
    Email,
    PasswordHash,
    ProfileId,
}

/// Python-style truthiness of a JSON value (for flags the game may send as bool or number).
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
