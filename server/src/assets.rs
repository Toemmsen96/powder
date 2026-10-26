//! The game-data pack (gamedata_dir): non-personal server data the game needs, read-only. Every part is
//! optional: without a pack the server runs, but the store is empty and some Connect pages are missing.
//!
//!   http.json, connect/http.json   recorded HTTPS replies {method, host, path, status, content_type, body_b64}
//!   rmc.json                       recorded NEX replies {"Proto::Method_V1": [{params_sha1, params_len, body}]}
//!   offers.json                    the store catalogue {"offers": [...]}
//!   actions.json                   Ubisoft Club action definitions (no player state)
//!   connect/demux.json             Steep's owned-games entries for Ubisoft Connect, and its patch info
//!
//! What is used of a pack is what gamedata/validate.rs lets through (a pack that fails is not used at all).

use crate::gamedata::{pack, validate};
use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{Map, Value};
use sha1::{Digest, Sha1};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{LazyLock, OnceLock};

static GUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}").unwrap()
});
/// Paths where only an exact (query-included) match is acceptable: replaying another page of a paged or
/// filtered list would be silently wrong.
static QUERY_SENSITIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/v1/spaces/[^/]+/offers$").unwrap());

pub fn normalize(path: &str) -> String {
    GUID.replace_all(path, "{id}").into_owned()
}

#[derive(Clone, Debug)]
pub struct HttpFixture {
    pub method: String,
    pub host: String,
    pub path: String,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl HttpFixture {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Recorded HTTPS replies. Lookup tiers: exact (METHOD host path?query), stripped (query ignored),
/// normalized (GUIDs replaced). The looser tiers are refused for query-sensitive collections.
#[derive(Default)]
pub struct HttpFixtures {
    pub entries: Vec<HttpFixture>,
    exact: HashMap<String, usize>,
    stripped: HashMap<String, usize>,
    normalized: HashMap<String, usize>,
}

impl HttpFixtures {
    pub fn new(entries: Vec<HttpFixture>) -> Self {
        let mut f = HttpFixtures { entries: vec![], ..Default::default() };
        for e in entries {
            f.push(e);
        }
        f
    }

    /// Add an entry; an exact duplicate replaces the earlier one (later sources override).
    pub fn push(&mut self, e: HttpFixture) {
        let i = self.entries.len();
        let base = e.path.split('?').next().unwrap_or("").to_string();
        self.exact.insert(format!("{} {} {}", e.method, e.host, e.path), i);
        self.stripped.entry(format!("{} {} {}", e.method, e.host, base)).or_insert(i);
        self.normalized.entry(format!("{} {} {}", e.method, e.host, normalize(&base))).or_insert(i);
        self.entries.push(e);
    }

    pub fn parse(raw: &[Value]) -> Result<Self> {
        let mut out = vec![];
        for e in raw {
            let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            out.push(HttpFixture {
                method: s("method"), host: s("host").to_lowercase(), path: s("path"),
                status: e.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
                content_type: Some(s("content_type")).filter(|c| !c.is_empty())
                    .unwrap_or_else(|| "application/json".into()),
                body: crate::util::unb64(&s("body_b64")).unwrap_or_default(),
            });
        }
        Ok(Self::new(out))
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw: Vec<Value> = serde_json::from_slice(&std::fs::read(path)?)
            .with_context(|| format!("{}", path.display()))?;
        Self::parse(&raw)
    }

    /// -> (fixture, tier). `path_qs` is the raw path including the query string.
    pub fn lookup(&self, method: &str, host: &str, path_qs: &str) -> Option<(&HttpFixture, &'static str)> {
        let host = host.to_lowercase();
        let base = path_qs.split('?').next().unwrap_or("");
        if let Some(&i) = self.exact.get(&format!("{method} {host} {path_qs}")) {
            return Some((&self.entries[i], "exact"));
        }
        if QUERY_SENSITIVE.is_match(base) {
            return None;
        }
        if let Some(&i) = self.stripped.get(&format!("{method} {host} {base}")) {
            return Some((&self.entries[i], "stripped"));
        }
        self.normalized.get(&format!("{method} {host} {}", normalize(base))).map(|&i| (&self.entries[i], "normalized"))
    }

    /// All recordings whose method matches and whose path (without query) matches the regex.
    pub fn find(&self, method: &str, pattern: &str) -> Vec<&HttpFixture> {
        let rx = Regex::new(pattern).expect("pattern");
        self.entries.iter()
            .filter(|e| e.method == method && rx.is_match(e.path.split('?').next().unwrap_or("")))
            .collect()
    }
}

/// Recorded NEX replies. Lookup: exact params match; else "uniform" when every recording of the method
/// returned the same bytes (usually an empty ack), since the params evidently do not matter then.
#[derive(Default)]
pub struct RmcFixtures {
    pub data: HashMap<String, Vec<RmcFixture>>,
}

#[derive(Clone, Debug)]
pub struct RmcFixture {
    pub params_sha1: String,
    pub body: Vec<u8>,
}

impl RmcFixtures {
    pub fn parse(raw: &Map<String, Value>) -> Self {
        let mut data = HashMap::new();
        for (method, rows) in raw {
            let rows = rows.as_array().map(|rows| rows.iter().map(|r| RmcFixture {
                params_sha1: r.get("params_sha1").and_then(Value::as_str).unwrap_or("").to_string(),
                body: hex::decode(r.get("body").and_then(Value::as_str).unwrap_or("")).unwrap_or_default(),
            }).collect()).unwrap_or_default();
            data.insert(method.clone(), rows);
        }
        Self { data }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw: Map<String, Value> = serde_json::from_slice(&std::fs::read(path)?)
            .with_context(|| format!("{}", path.display()))?;
        Ok(Self::parse(&raw))
    }

    pub fn lookup(&self, method: &str, params: &[u8]) -> Option<(Vec<u8>, &'static str)> {
        let entries = self.data.get(method).filter(|e| !e.is_empty())?;
        let digest = hex::encode(Sha1::digest(params));
        if let Some(e) = entries.iter().find(|e| e.params_sha1 == digest) {
            return Some((e.body.clone(), "exact"));
        }
        if entries.iter().all(|e| e.body == entries[0].body) {
            return Some((entries[0].body.clone(), "uniform"));
        }
        None
    }

    /// Most recent recording of a method (captures are merged oldest -> newest).
    pub fn latest(&self, method: &str) -> Option<Vec<u8>> {
        self.data.get(method).and_then(|e| e.last()).map(|e| e.body.clone())
    }
}

/// The store's offers. Filters (verified against the recorded query families):
///   tags=A,B   offer has ALL listed tags          type=X   offer.type == X
///   itemIds=.. offer grants ANY listed item       offset/limit   paging over first-seen order
/// An offer's items[] carry SIGNED quantities: a shop offer is +1 item / -price credits, a reward offer is
/// +credits (+ item). See services/economy.rs.
#[derive(Default)]
pub struct OfferCatalog {
    offers: Vec<Value>,
    by_id: HashMap<String, usize>,
}

impl OfferCatalog {
    pub fn new(offers: Vec<Value>) -> Self {
        let mut c = Self::default();
        for o in offers {
            c.add(o, false);
        }
        c
    }

    /// Add an offer; with `replace`, one with the same offerId is replaced in place (mods), else the first
    /// one seen wins (recordings).
    pub fn add(&mut self, offer: Value, replace: bool) {
        let Some(id) = offer.get("offerId").and_then(Value::as_str).map(str::to_string) else { return };
        match self.by_id.get(&id) {
            Some(&i) if replace => self.offers[i] = offer,
            Some(_) => {}
            None => {
                self.by_id.insert(id, self.offers.len());
                self.offers.push(offer);
            }
        }
    }

    pub fn remove(&mut self, offer_id: &str) {
        if let Some(i) = self.by_id.remove(offer_id) {
            self.offers.remove(i);
            for v in self.by_id.values_mut() {
                if *v > i {
                    *v -= 1;
                }
            }
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw: Value = serde_json::from_slice(&std::fs::read(path)?).with_context(|| format!("{}", path.display()))?;
        Ok(Self::new(raw.get("offers").and_then(Value::as_array).cloned().unwrap_or_default()))
    }

    pub fn len(&self) -> usize {
        self.offers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offers.is_empty()
    }

    pub fn get(&self, offer_id: &str) -> Option<&Value> {
        self.by_id.get(offer_id).map(|&i| &self.offers[i])
    }

    pub fn all(&self) -> &[Value] {
        &self.offers
    }

    pub fn query(&self, tags: Option<&str>, kind: Option<&str>, item_ids: Option<&str>, offset: usize, limit: usize)
                 -> Vec<Value> {
        let split = |s: Option<&str>| -> Vec<String> {
            s.unwrap_or("").split(',').filter(|x| !x.is_empty()).map(str::to_string).collect()
        };
        let (want_tags, want_items) = (split(tags), split(item_ids));
        self.offers.iter().filter(|o| {
            if let Some(k) = kind.filter(|k| !k.is_empty()) {
                if o.get("type").and_then(Value::as_str) != Some(k) {
                    return false;
                }
            }
            let has_tag = |t: &String| o.get("tags").and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|x| x.as_str() == Some(t)));
            if !want_tags.iter().all(has_tag) {
                return false;
            }
            if !want_items.is_empty() {
                let items = o.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                if !items.iter().any(|i| i.get("itemId").and_then(Value::as_str)
                    .is_some_and(|id| want_items.iter().any(|w| w == id))) {
                    return false;
                }
            }
            true
        }).skip(offset).take(limit).cloned().collect()
    }
}

/// Without a game-data pack the store is empty, so the game cannot acquire anything - not even the rewards it
/// grants itself (after a mission step or a challenge it looks up `offers?itemIds=<reward item>` and buys that
/// offer). For an item no offer grants, a synthetic free offer gives +1 of it. Its id is the item's UUID with every
/// byte XOR 0xA5: still a UUID, and reversible, so a purchase knows the item without stored state.
pub fn synthetic_offer(item_id: &str) -> Option<Value> {
    let offer_id = xor_uuid(item_id)?;
    Some(serde_json::json!({"offerId": offer_id, "type": "progression", "revision": 1, "tags": ["REWARDS"],
                            "items": [{"itemId": item_id, "quantity": 1}]}))
}

/// The item of a synthetic offer id (see synthetic_offer).
pub fn synthetic_item(offer_id: &str) -> Option<String> {
    xor_uuid(offer_id)
}

fn xor_uuid(id: &str) -> Option<String> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) || id.len() != 36 {
        return None;
    }
    let b: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap() ^ 0xA5).collect();
    let h = hex::encode(b);
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Steep's entries in Ubisoft Connect's owned-games reply: the server's own entries (builtin/products.rs) for the
/// products the pack lists, with cloud saves on (demux/ownership.rs switches them off unless configured). Nothing
/// of the pack's product entries but their ids reaches Connect.
#[derive(Default)]
pub struct ConnectProducts {
    pub patch_info: Option<Vec<u8>>,
    /// encoded product messages
    pub products: Vec<Vec<u8>>,
}

impl ConnectProducts {
    pub fn from_ids(patch_info: Option<Vec<u8>>, ids: &[u64], achievements_file: Option<&str>) -> Self {
        if ids.is_empty() {
            return Self { patch_info, products: vec![] };
        }
        let only: BTreeSet<u64> = ids.iter().copied().collect();
        Self { patch_info, products: crate::builtin::products::entries_of(true, achievements_file, Some(&only)) }
    }
}

#[derive(Default)]
pub struct Assets {
    pub http: HttpFixtures,
    pub rmc: RmcFixtures,
    pub catalog: OfferCatalog,
    pub actions: Vec<Value>,
    pub connect_http: HttpFixtures,
    pub products: ConnectProducts,
    pub manifest: Option<Map<String, Value>>,
    /// The achievement definitions zip's files, read once (not on every icon or GraphQL request).
    pub(crate) achievements: OnceLock<HashMap<String, Vec<u8>>>,
}

impl Assets {
    /// Synthetic offers (synthetic_offer) are made when there is no game-data pack.
    pub fn synthesizes_offers(&self) -> bool {
        self.manifest.is_none()
    }

    /// The pack at `root`, or empty assets (logged) when there is none or it can't be used.
    pub fn load(root: &Path) -> Assets {
        if !root.join("MANIFEST.json").exists() {
            log::info!(target: "powder", "no game-data pack at {}: built-in replies (empty store, no challenge \
                       rewards, no achievement names in Ubisoft Connect)", root.display());
            return Assets::default();
        }
        match Self::try_load(root) {
            Ok(a) => a,
            Err(e) => {
                log::warn!(target: "powder", "game-data pack at {}: {e:#}", root.display());
                Assets::default()
            }
        }
    }

    fn try_load(root: &Path) -> Result<Assets> {
        let manifest = pack::read_manifest(root)?;
        let changed = pack::verify_files(root, &manifest);
        if !changed.is_empty() {
            log::warn!(target: "powder", "game-data pack files changed since it was built: {}", changed.join(", "));
        }
        let valid = match validate::validated(root) {
            Ok(valid) => valid,
            Err(e) => {
                log::warn!(target: "powder", "{e:#}; running on built-in replies");
                return Ok(Assets::default());
            }
        };
        let mut assets = Assets {
            http: HttpFixtures::new(valid.http),
            rmc: RmcFixtures::parse(&valid.rmc),
            catalog: OfferCatalog::new(valid.offers),
            actions: valid.actions,
            connect_http: HttpFixtures::new(valid.connect_http),
            manifest: Some(manifest),
            ..Assets::default()
        };
        assets.products = ConnectProducts::from_ids(valid.patch_info, &valid.product_ids,
                                                    assets.achievements_file().as_deref());
        Ok(assets)
    }

    pub fn is_empty(&self) -> bool {
        self.manifest.is_none()
    }

    fn achievements_entry(&self) -> Option<&HttpFixture> {
        self.connect_http.entries.iter().find(|e| {
            let path = e.path.split('?').next().unwrap_or("");
            path.ends_with(".zip") && path.contains("/assets/")
        })
    }

    /// File name of Ubisoft Connect's achievement definitions in the pack (referenced in Steep's product
    /// configuration; Connect downloads it), or None without one.
    pub fn achievements_file(&self) -> Option<String> {
        self.achievements_entry().map(|e| {
            let path = e.path.split('?').next().unwrap_or("");
            path.rsplit('/').next().unwrap_or(path).to_string()
        })
    }

    /// A file from the achievement definitions (a zip of '<lang>_loc.txt' files of 'id<TAB>name<TAB>
    /// description' lines and '<id>.png' icons). The zip is read once (size-capped, validate::read_zip).
    fn achievement_file(&self, name: &str) -> Option<Vec<u8>> {
        let files = self.achievements.get_or_init(|| {
            for e in self.connect_http.entries.iter()
                .filter(|e| e.path.split('?').next().unwrap_or("").ends_with(".zip") && e.path.contains("/assets/")) {
                match validate::read_zip(&e.body) {
                    Ok(files) => return files,
                    Err(err) => log::warn!(target: "powder", "game-data pack: {err:#}"),
                }
            }
            HashMap::new()
        });
        files.get(name).cloned()
    }

    /// {achievement id: (name, description)} of Steep's achievements, in file order.
    pub fn achievement_names(&self, lang: &str) -> Vec<(i64, String, String)> {
        let Some(raw) = self.achievement_file(&format!("{lang}_loc.txt")) else { return vec![] };
        let text = String::from_utf8_lossy(&raw);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut out: Vec<(i64, String, String)> = vec![];
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 && !parts[0].is_empty() && parts[0].chars().all(|c| c.is_ascii_digit()) {
                let id: i64 = parts[0].parse().unwrap_or(0);
                let entry = (id, parts[1].to_string(), parts[2].to_string());
                match out.iter_mut().find(|x| x.0 == id) {
                    Some(old) => *old = entry,
                    None => out.push(entry),
                }
            }
        }
        out
    }

    pub fn achievement_icon(&self, id: i64) -> Option<Vec<u8>> {
        self.achievement_file(&format!("{id}.png"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_lookup_tiers() {
        let f = HttpFixtures::parse(&[
            json!({"method": "GET", "host": "Public-Ubiservices.ubi.com", "path": "/v1/a?x=1", "status": 200,
                   "content_type": "application/json", "body_b64": crate::util::b64(b"{}")}),
            json!({"method": "GET", "host": "h", "path": "/v1/profiles/0d4f3b4c-1111-2222-3333-444455556666/x",
                   "status": 200, "body_b64": ""}),
            json!({"method": "GET", "host": "h", "path": "/v1/spaces/s/offers?offset=0", "status": 200, "body_b64": ""}),
        ]).unwrap();
        assert_eq!(f.lookup("GET", "public-ubiservices.ubi.com", "/v1/a?x=1").unwrap().1, "exact");
        assert_eq!(f.lookup("GET", "public-ubiservices.ubi.com", "/v1/a?x=2").unwrap().1, "stripped");
        assert_eq!(f.lookup("GET", "h", "/v1/profiles/aaaaaaaa-1111-2222-3333-444455556666/x").unwrap().1, "normalized");
        assert!(f.lookup("GET", "h", "/v1/spaces/s/offers?offset=100").is_none());
        assert_eq!(f.entries[1].content_type, "application/json");
    }

    #[test]
    fn catalog_filters() {
        let c = OfferCatalog::new(vec![
            json!({"offerId": "a", "type": "shop", "tags": ["x", "y"], "items": [{"itemId": "i1", "quantity": 1}]}),
            json!({"offerId": "b", "type": "progression", "tags": ["x"], "items": [{"itemId": "i2", "quantity": 1}]}),
            json!({"offerId": "a", "type": "dup"}),
        ]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.query(Some("x,y"), None, None, 0, 100).len(), 1);
        assert_eq!(c.query(None, Some("progression"), None, 0, 100)[0]["offerId"], "b");
        assert_eq!(c.query(None, None, Some("i2,zz"), 0, 100)[0]["offerId"], "b");
        assert_eq!(c.query(None, None, None, 1, 1)[0]["offerId"], "b");
    }
}
