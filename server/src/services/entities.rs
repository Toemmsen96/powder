//! UbiServices profile entities: small JSON documents a player owns, e.g. the rider card (type PROFILE, obj =
//! RIDER_CARD / PLAYER_STATUS / GLOBAL_VERSION) that others see, favorite challenges and replays. They live
//! in the database; players this server does not know have none.

use crate::constants::SPACE_ID;
use crate::repos::Repos;
use crate::util::iso_now;
use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

pub type Entity = Map<String, Value>;

const EDITABLE_FIELDS: [&str; 7] = ["type", "name", "tags", "obj", "spaceId", "editable", "extendedStorage"];

pub struct EntityService {
    repos: Arc<Repos>,
}

/// items[offset:offset + limit], as Python slices.
fn page(items: Vec<Entity>, offset: usize, limit: usize) -> Vec<Entity> {
    items.into_iter().skip(offset).take(limit).collect()
}

impl EntityService {
    pub fn new(repos: Arc<Repos>) -> Self {
        EntityService { repos }
    }

    pub fn is_local(&self, profile_id: &str) -> Result<bool> {
        Ok(self.repos.player(profile_id)?.is_some())
    }

    /// `tags`: comma-separated; an entity must carry all of them.
    pub fn for_profile(&self, profile_id: &str, kind: Option<&str>, tags: Option<&str>, offset: usize, limit: usize)
                       -> Result<Vec<Entity>> {
        let pid = profile_id.to_lowercase();
        let mut items = if self.is_local(&pid)? { self.repos.entities_of(&pid, kind)? } else { vec![] };
        let want: HashSet<&str> = tags.unwrap_or("").split(',').filter(|t| !t.is_empty()).collect();
        if !want.is_empty() {
            items.retain(|e| {
                let have: HashSet<&str> = e.get("tags").and_then(Value::as_array).into_iter().flatten()
                    .filter_map(Value::as_str).collect();
                want.is_subset(&have)
            });
        }
        Ok(page(items, offset, limit))
    }

    /// Several players' entities at once, e.g. the friends' trails (FAVORITE_REPLAY / FAVORITE_CHALLENGE, tags
    /// TRAIL,WORLD_<id>,SANDBOX_A) or rider cards.
    pub fn for_profiles(&self, profile_ids: &[String], kind: Option<&str>, tags: Option<&str>, offset: usize,
                        limit: usize) -> Result<Vec<Entity>> {
        let mut out = vec![];
        for pid in profile_ids {
            out.extend(self.for_profile(pid, kind, tags, 0, 10_000)?);
        }
        Ok(page(out, offset, limit))
    }

    pub fn get(&self, entity_id: &str) -> Result<Option<Entity>> {
        self.repos.entity_get(entity_id)
    }

    /// Create or replace (the game PUTs its whole rider card); revision is bumped.
    pub fn put(&self, entity_id: &str, body: &Entity, profile_id: Option<&str>) -> Result<Entity> {
        let existing = self.repos.entity_get(entity_id)?.unwrap_or_default();
        let mut entity = existing.clone();
        for k in EDITABLE_FIELDS {
            if let Some(v) = body.get(k) {
                entity.insert(k.into(), v.clone());
            }
        }
        let text = |m: &Entity, k: &str| m.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let owner = text(&existing, "profileId").or_else(|| text(body, "profileId"))
            .or_else(|| profile_id.map(str::to_string)).unwrap_or_default().to_lowercase();
        if owner.is_empty() {
            bail!("entity without an owner");
        }
        entity.insert("entityId".into(), json!(entity_id.to_lowercase()));
        entity.insert("profileId".into(), json!(owner));
        entity.entry("spaceId").or_insert(json!(SPACE_ID));
        entity.entry("type").or_insert(json!("UNKNOWN"));
        entity.entry("editable").or_insert(json!(true));
        let revision = existing.get("revision").and_then(Value::as_i64).unwrap_or(0) + 1;
        entity.insert("revision".into(), json!(revision));
        entity.insert("lastModified".into(), json!(iso_now()));
        self.repos.entity_put(&entity)?;
        Ok(self.repos.entity_get(entity_id)?.unwrap_or(entity))
    }

    pub fn create(&self, body: &Entity, profile_id: &str) -> Result<Entity> {
        log::info!(target: "powder.entities", "new {} entity for {profile_id}",
                   body.get("type").and_then(Value::as_str).unwrap_or("None"));
        self.put(&uuid::Uuid::new_v4().to_string(), body, Some(profile_id))
    }

    pub fn delete(&self, entity_id: &str) -> Result<bool> {
        self.repos.entity_delete(entity_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const A: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const B: &str = "bbbbbbbb-0000-0000-0000-000000000002";

    fn obj(v: Value) -> Entity {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn put_bumps_revision_keeps_owner_and_filters_by_tags() {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Arc::new(Repos::new(db));
        repos.create_player(A, A, "A").unwrap();
        repos.create_player(B, B, "B").unwrap();
        let s = EntityService::new(repos);

        let e = s.create(&obj(json!({"type": "FAVORITE_REPLAY", "tags": ["TRAIL", "WORLD_1"], "obj": {"x": 1}})), A)
            .unwrap();
        let id = e["entityId"].as_str().unwrap().to_string();
        assert_eq!((e["revision"].clone(), e["profileId"].clone()), (json!(1), json!(A)));
        // another player's PUT does not take it over; the revision goes up
        let e = s.put(&id.to_uppercase(), &obj(json!({"obj": {"x": 2}, "profileId": B})), Some(B)).unwrap();
        assert_eq!((e["revision"].clone(), e["profileId"].clone(), e["obj"].clone()), (json!(2), json!(A), json!({"x": 2})));
        assert_eq!(e["type"], "FAVORITE_REPLAY");

        s.create(&obj(json!({"type": "FAVORITE_REPLAY", "tags": ["TRAIL"]})), B).unwrap();
        let both = s.for_profiles(&[A.into(), B.into(), "unknown".into()], Some("FAVORITE_REPLAY"), Some("TRAIL"), 0, 100)
            .unwrap();
        assert_eq!(both.len(), 2);
        assert_eq!(s.for_profile(A, None, Some("TRAIL,WORLD_1"), 0, 100).unwrap().len(), 1);
        assert_eq!(s.for_profile(B, None, Some("TRAIL,WORLD_1"), 0, 100).unwrap().len(), 0);
        assert_eq!(s.for_profiles(&[A.into(), B.into()], None, None, 1, 100).unwrap().len(), 1);
        assert!(s.put("x", &obj(json!({})), None).is_err());
        assert!(s.delete(&id).unwrap());
        assert!(s.get(&id).unwrap().is_none());
    }
}
