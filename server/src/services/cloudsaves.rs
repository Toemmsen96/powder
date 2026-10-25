//! Cloud saves: Ubisoft Connect keeps a game's own save files (Steep: savegames\<profile id>\3280\N.save) on the
//! server, so they follow the account to another PC.
//!
//! How Connect does it (recorded): at game start it reads the "cloudSave" entity upcCloudSaveUplayId_<product>
//! (GET /v1/profiles/<pid>/entities?name=...) and compares its slots with the local files; at exit it writes the
//! entity (PUT /v1/profiles/entities/<id>: obj {slots [{id, filename, hash, size, lastModified}], timestamp,
//! pc_name}) and then asks the demux cloudsave_service for upload links, one per slot, and uploads each file there.
//! The files are stored as they come (Connect transforms them; it reads them back itself). The link request names
//! a method, Put / Get / Delete, so downloads and deletions ask for links the same way.
//!
//! The demux hands out the links ([`CloudSaveService::link`]); the HTTPS side redeems them ([`CloudSaveService::resolve`]
//! then store / read / remove), so both must share one instance.
//!
//! Safety: an entity is only shown while every slot it lists has its file here, so Connect never tries to download
//! a save that is not complete; an upload replaces a stored file only when it arrived whole (MD5 as announced) and
//! is written to a temporary file first.

use crate::repos::Repos;
use crate::util::{b64, token_urlsafe, unix_now};
use anyhow::Result;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

pub const TYPE: &str = "cloudSave";
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^upcCloudSaveUplayId_(\d{1,10})$").unwrap());
static SLOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,32}$").unwrap());
const LINK_LIFETIME_S: f64 = 3600.0;

/// The product a cloud save is kept under. Steep's Ubisoft Connect version (3279) and Steam version (3280) keep
/// their saves in the same format (same header and slot layout), so a player keeps one save across both.
pub fn shared(product: u64) -> u64 {
    if product == 3279 { 3280 } else { product }
}

pub fn entity_name(product: u64) -> String {
    format!("upcCloudSaveUplayId_{product}")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub profile_id: String,
    pub product: u64,
    pub slot: String,
    pub expires: f64,
    /// What the link allows: PUT (upload), GET (download), DELETE.
    pub method: String,
    /// Base64 MD5 announced for an upload.
    pub md5: Option<String>,
}

/// An upload or download that can't be done (unknown link, bad slot, MD5 mismatch).
#[derive(Debug, Clone, PartialEq)]
pub struct CloudSaveError(pub String);

impl std::fmt::Display for CloudSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CloudSaveError {}

fn refuse<T>(msg: String) -> std::result::Result<T, CloudSaveError> {
    Err(CloudSaveError(msg))
}

/// One cloud save in the admin listing.
#[derive(Clone, Debug)]
pub struct CloudSaveItem {
    pub profile_id: String,
    pub product: u64,
    pub entity: Map<String, Value>,
    pub complete: bool,
}

pub struct CloudSaveService {
    repos: Arc<Repos>,
    pub root: PathBuf,
    links: Mutex<HashMap<String, Link>>,
}

impl CloudSaveService {
    pub fn new(repos: Arc<Repos>, data_dir: &Path) -> Self {
        Self { repos, root: data_dir.join("cloudsaves"), links: Mutex::new(HashMap::new()) }
    }

    // ---- the entity ------------------------------------------------------------------------------------------
    pub fn entity(&self, profile_id: &str, product: u64) -> Result<Option<Map<String, Value>>> {
        let name = entity_name(shared(product));
        Ok(self.repos.entities_of(&profile_id.to_lowercase(), Some(TYPE))?.into_iter()
            .find(|e| e.get("name").and_then(Value::as_str) == Some(name.as_str())))
    }

    /// The slot file names an entity lists (Python's str() of each filename / id).
    pub fn slots(entity: &Map<String, Value>) -> Vec<String> {
        let py_str = |v: &Value| match v {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            Value::Bool(b) => if *b { "True" } else { "False" }.into(),
            other => other.to_string(),
        };
        let slots = entity.get("obj").and_then(Value::as_object).and_then(|o| o.get("slots"))
            .and_then(Value::as_array).cloned().unwrap_or_default();
        slots.iter().filter_map(|s| {
            let s = s.as_object()?;
            let name = s.get("filename").or_else(|| s.get("id")).map(py_str).unwrap_or_default();
            SLOT.is_match(&name).then_some(name)
        }).collect()
    }

    pub fn complete(&self, profile_id: &str, product: u64, entity: &Map<String, Value>) -> bool {
        let slots = Self::slots(entity);
        !slots.is_empty() && slots.iter().all(|s| self.path(profile_id, product, s).is_ok_and(|p| p.exists()))
    }

    /// The cloud save Connect gets at game start: only a complete one, under the name Connect asked for.
    pub fn offered(&self, profile_id: &str, product: u64) -> Result<Option<Map<String, Value>>> {
        let Some(mut e) = self.entity(profile_id, product)? else { return Ok(None) };
        if !self.complete(profile_id, product, &e) {
            log::warn!(target: "powder.cloudsaves", "cloud save of {profile_id} (product {product}) is incomplete \
                       (files missing): not offered");
            return Ok(None);
        }
        e.insert("name".into(), Value::String(entity_name(product)));
        Ok(Some(e))
    }

    // ---- files -----------------------------------------------------------------------------------------------
    pub fn path(&self, profile_id: &str, product: u64, slot: &str) -> std::result::Result<PathBuf, CloudSaveError> {
        if !SLOT.is_match(slot) {
            return refuse(format!("bad slot name {slot:?}"));
        }
        Ok(self.root.join(profile_id.to_lowercase()).join(shared(product).to_string()).join(format!("{slot}.savegame")))
    }

    /// A one-hour token for one slot of this player's save: uploading (PUT), downloading (GET), DELETE.
    pub fn link(&self, profile_id: &str, product: u64, slot: &str, md5: Option<&str>, method: &str)
                -> std::result::Result<String, CloudSaveError> {
        self.path(profile_id, product, slot)?; // validates the slot name
        let now = unix_now();
        let mut links = self.links.lock();
        links.retain(|_, l| l.expires >= now);
        let token = token_urlsafe(24);
        links.insert(token.clone(), Link { profile_id: profile_id.to_lowercase(), product, slot: slot.to_string(),
                                           expires: now + LINK_LIFETIME_S, method: method.to_string(),
                                           md5: md5.map(str::to_string) });
        Ok(token)
    }

    pub fn resolve(&self, token: &str, product: u64, slot: &str, method: Option<&str>)
                   -> std::result::Result<Link, CloudSaveError> {
        let link = self.links.lock().get(token).cloned();
        let Some(link) = link.filter(|l| l.expires >= unix_now() && l.product == product && l.slot == slot) else {
            return refuse("unknown or expired link".into());
        };
        if let Some(m) = method {
            let m = if m == "HEAD" { "GET" } else { m };
            if m != link.method {
                return refuse(format!("link is for {}, not {m}", link.method));
            }
        }
        Ok(link)
    }

    pub fn remove(&self, link: &Link) -> Result<()> {
        let path = self.path(&link.profile_id, link.product, &link.slot)?;
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        log::info!(target: "powder.cloudsaves", "cloud save of {}: slot {} of product {} deleted", link.profile_id,
                   link.slot, link.product);
        Ok(())
    }

    /// Store an upload; a [`CloudSaveError`] (in the anyhow error) when it arrived damaged.
    pub fn store(&self, link: &Link, data: &[u8], content_md5: Option<&str>) -> Result<()> {
        if let Some(expected) = content_md5.filter(|m| !m.is_empty()).or(link.md5.as_deref()).filter(|m| !m.is_empty()) {
            let got = b64(&Md5::digest(data));
            if got != expected {
                return Err(CloudSaveError(format!("upload of slot {} is damaged (MD5 {got}, expected {expected})",
                                                  link.slot)).into());
            }
        }
        let path = self.path(&link.profile_id, link.product, &link.slot)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, data)?;
        if path.exists() {
            // the previous upload stays, as <slot>.prev
            std::fs::rename(&path, path.with_extension("prev"))?;
        }
        std::fs::rename(&tmp, &path)?;
        log::info!(target: "powder.cloudsaves", "cloud save of {}: slot {} of product {} stored ({} B)",
                   link.profile_id, link.slot, link.product, data.len());
        Ok(())
    }

    pub fn read(&self, link: &Link) -> Result<Vec<u8>> {
        let path = self.path(&link.profile_id, link.product, &link.slot)?;
        if !path.exists() {
            return Err(CloudSaveError(format!("slot {} not stored", link.slot)).into());
        }
        let data = std::fs::read(&path)?;
        log::info!(target: "powder.cloudsaves", "cloud save of {}: slot {} of product {} downloaded ({} B)",
                   link.profile_id, link.slot, link.product, data.len());
        Ok(data)
    }

    // ---- admin -----------------------------------------------------------------------------------------------
    pub fn all(&self, profile_id: Option<&str>) -> Result<Vec<CloudSaveItem>> {
        let players = match profile_id {
            Some(p) => vec![p.to_lowercase()],
            None => self.repos.players()?.into_iter().map(|p| p.profile_id).collect(),
        };
        let mut out = vec![];
        for pid in players {
            for e in self.repos.entities_of(&pid, Some(TYPE))? {
                let name = e.get("name").and_then(Value::as_str).unwrap_or("");
                let Some(product) = NAME.captures(name).and_then(|c| c[1].parse::<u64>().ok()) else { continue };
                let complete = self.complete(&pid, product, &e);
                out.push(CloudSaveItem { profile_id: pid.clone(), product, entity: e, complete });
            }
        }
        Ok(out)
    }

    pub fn delete(&self, profile_id: &str, product: Option<u64>) -> Result<usize> {
        let mut n = 0;
        for item in self.all(Some(profile_id))? {
            if product.is_some_and(|p| p != item.product) {
                continue;
            }
            if let Some(id) = item.entity.get("entityId").and_then(Value::as_str) {
                self.repos.entity_delete(id)?;
            }
            let folder = self.root.join(profile_id.to_lowercase()).join(item.product.to_string());
            if folder.exists() {
                for f in std::fs::read_dir(&folder)? {
                    std::fs::remove_file(f?.path())?;
                }
                std::fs::remove_dir(&folder)?;
            }
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use serde_json::json;

    fn service(dir: &Path) -> CloudSaveService {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        CloudSaveService::new(Arc::new(Repos::new(db)), dir)
    }

    #[test]
    fn links_store_and_admin() {
        let tmp = tempfile::tempdir().unwrap();
        let saves = service(tmp.path());
        let me = "00000000-0000-4000-8000-0000000000aa";
        assert!(saves.link(me, 3280, "../x", None, "PUT").is_err(), "bad slot");
        let token = saves.link(me, 3279, "1", Some("bad"), "PUT").unwrap();
        assert!(saves.resolve(&token, 3280, "1", Some("PUT")).is_err(), "other product");
        assert!(saves.resolve(&token, 3279, "1", Some("GET")).is_err(), "other method");
        let link = saves.resolve(&token, 3279, "1", Some("PUT")).unwrap();
        let err = saves.store(&link, b"12345", None).unwrap_err();
        assert!(err.downcast_ref::<CloudSaveError>().is_some(), "damaged upload refused");
        saves.store(&link, b"12345", Some(&b64(&Md5::digest(b"12345")))).unwrap();
        assert!(saves.path(me, 3280, "1").unwrap().exists(), "3279 kept under 3280");

        let entity = json!({"entityId": "00000000-0000-4000-8000-0000000000bb", "profileId": me, "type": TYPE,
                            "name": entity_name(3280), "obj": {"slots": [{"id": 1, "filename": "1"}]}});
        saves.repos.entity_put(entity.as_object().unwrap()).unwrap();
        assert_eq!(saves.offered(me, 3279).unwrap().unwrap()["name"], json!(entity_name(3279)));
        let items = saves.all(Some(me)).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].product == 3280 && items[0].complete);
        assert_eq!(saves.delete(me, None).unwrap(), 1);
        assert!(saves.all(Some(me)).unwrap().is_empty() && !saves.path(me, 3280, "1").unwrap().exists());
    }
}
