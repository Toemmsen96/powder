//! Privacy check for a game-data pack: nothing that identifies a person may end up in it.
//!
//! Markers are collected from the *personal* parts of the source recordings (everything the builder did not
//! allow-list): the values of identifying JSON keys (profile/user ids, names, emails, tickets, ip), every GUID
//! in friend lists, profile lookups and leaderboards, and Connect's friends. Then every file of the pack
//! (including base64 bodies, decoded, and what is inside zip / gzip bodies) is searched for them. Any hit fails
//! the check. Markers are at least 6 characters (shorter ids would match by chance), except names: a player name
//! can be 3 characters, so names of 3-5 are searched as whole words (case-sensitive).
//!
//! Besides, a product entry's field 21 (40 hex digits) must be Steep's known value (builtin/product_table.rs):
//! another value could be per account. And [`self_check`] looks for account data without the recordings (a pack
//! being zipped for someone else): values of identifying keys, e-mail addresses, field 21.

use crate::builtin::product_table::{P, PRODUCTS};
use crate::constants::{APP_ID, CONNECT_APP_ID, CONNECT_SPACE_ID, SPACE_ID};
use crate::demux::pb;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

static GUID: LazyLock<BytesRegex> = LazyLock::new(|| {
    BytesRegex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}").unwrap()
});
const IDENTIFYING_KEYS: &[&str] = &["profileid", "userid", "idonplatform", "nameonplatform", "username", "email",
    "emailaddress", "ticket", "sessionkey", "sessionid", "remembermeticket", "clientip", "firstname", "lastname",
    "dateofbirth", "friendid", "pid"];
/// Replies that are lists of people: every GUID in them is a marker. (Rider cards and inventories are not
/// here: they also hold game item ids that legitimately appear in the store catalogue.)
static PEOPLE_PATHS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/friends|/v3/profiles$|/onlineStatuses|/blocks|harboursocial").unwrap());
static B64: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9+/=]+$").unwrap());
const NAME_KEYS: &[&str] = &["nameonplatform", "username", "firstname", "lastname"];
const MIN_MARKER: usize = 6; // shorter ids would match by chance
const MIN_NAME: usize = 3; // a player name can be this short
pub const PRODUCT_HASH: u32 = 21; // a product entry's 40-hex field
pub static HEX40: LazyLock<BytesRegex> = LazyLock::new(|| BytesRegex::new(r"^[0-9A-Fa-f]{40}$").unwrap());
static EMAIL: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"(?-u)[\w.+-]{1,64}@[\w-]{1,63}(\.[\w-]{1,63})+").unwrap());
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\{.*\}|0)$").unwrap());
/// What is unpacked from one zip / gzip body to search it.
const MAX_INNER: u64 = 64 << 20;

/// Something that identifies a person: an id-like value (searched as a substring, ignoring ASCII case, from
/// MIN_MARKER characters) or a name (also from MIN_NAME characters; below MIN_MARKER as a whole word, exact case).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Marker {
    Value(String),
    Name(String),
}

impl Marker {
    pub fn text(&self) -> &str {
        match self {
            Marker::Value(t) | Marker::Name(t) => t,
        }
    }
}

fn guids(raw: &[u8]) -> impl Iterator<Item = Marker> + '_ {
    GUID.find_iter(raw).map(|m| Marker::Value(String::from_utf8_lossy(m.as_bytes()).to_lowercase()))
}

/// A JSON string or integer as Python's `str()` writes it.
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        _ => None,
    }
}

fn walk(value: &Value, out: &mut HashSet<Marker>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = k.to_lowercase();
                let text = IDENTIFYING_KEYS.contains(&key.as_str()).then(|| scalar(v)).flatten();
                if let Some(t) = text.filter(|t| t.chars().count() >= MIN_MARKER) {
                    out.insert(Marker::Value(t));
                } else if let Value::String(s) = v {
                    if NAME_KEYS.contains(&key.as_str()) && s.trim().chars().count() >= MIN_NAME {
                        out.insert(Marker::Name(s.trim().to_string()));
                    }
                }
                walk(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
        _ => {}
    }
}

/// Markers from recorded HTTPS replies that were NOT copied into the pack (`kept` = their indexes).
pub fn markers_from_http(entries: &[Value], kept: &HashSet<usize>) -> HashSet<Marker> {
    let mut out = HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        if kept.contains(&i) {
            continue;
        }
        let body = crate::util::unb64(e.get("body_b64").and_then(Value::as_str).unwrap_or("")).unwrap_or_default();
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            walk(&v, &mut out);
        }
        if PEOPLE_PATHS.is_match(e.get("path").and_then(Value::as_str).unwrap_or("")) {
            out.extend(guids(&body));
        }
    }
    out
}

/// Every GUID in NEX replies that were not kept (leaderboards, profiles, matchmaking).
pub fn markers_from_rmc(rmc: &Map<String, Value>, kept: &[&str]) -> HashSet<Marker> {
    let mut out = HashSet::new();
    for (method, rows) in rmc {
        if kept.contains(&method.as_str()) {
            continue;
        }
        for row in rows.as_array().into_iter().flatten() {
            let body = hex::decode(row.get("body").and_then(Value::as_str).unwrap_or("")).unwrap_or_default();
            out.extend(guids(&body));
        }
    }
    out
}

/// Names in a friends_service initialize reply: {1: {1 id, 22: {2: {2: {1 profile id, 2 name}}...}}}.
fn friend_names(raw: &[u8]) -> HashSet<Marker> {
    let mut names = HashSet::new();
    let Some(top) = pb::try_decode(raw) else { return names };
    let Some(outer) = pb::sub(&top, 1) else { return names };
    let Some(body) = pb::sub(&outer, 22) else { return names };
    for f in &body {
        if let (2, pb::V::Bytes(entry)) = (f.0, &f.1) {
            let inner = pb::try_decode(entry).and_then(|e| pb::sub(&e, 2)).unwrap_or_default();
            if let Some(name) = pb::bytes(&inner, 2).filter(|n| n.trim_ascii().len() >= MIN_NAME) {
                names.insert(Marker::Name(String::from_utf8_lossy(name).trim().to_string()));
            }
        }
    }
    names
}

/// Friends (ids, names) and the player's id from Connect's demux replies.
pub fn markers_from_demux(demux: &Value) -> HashSet<Marker> {
    let mut out = HashSet::new();
    let Some(services) = demux.get("services").and_then(Value::as_object) else { return out };
    for (svc, kinds) in services {
        if svc == "ownership_service" {
            continue; // product data, handled by the allow-list
        }
        for (kind, replies) in kinds.as_object().into_iter().flatten() {
            for b64 in replies.as_array().into_iter().flatten() {
                let raw = crate::util::unb64(b64.as_str().unwrap_or("")).unwrap_or_default();
                out.extend(guids(&raw));
                if svc == "friends_service" && kind == "22" {
                    out.extend(friend_names(&raw));
                }
            }
        }
    }
    out
}

fn b64_strings(value: &Value, out: &mut Vec<Vec<u8>>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                match v {
                    Value::String(s) if k.ends_with("b64") || k == "patch_info" => {
                        out.extend(crate::util::unb64(s));
                    }
                    _ => b64_strings(v, out),
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                match v {
                    Value::String(s) if s.len() > 40 && B64.is_match(s) => out.extend(crate::util::unb64(s)),
                    _ => b64_strings(v, out),
                }
            }
        }
        _ => {}
    }
}

/// What a zip or gzip body holds (capped: MAX_INNER in all), to be searched too -> (member name, content).
fn inner(blob: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![];
    if blob.starts_with(b"PK\x03\x04") {
        let Ok(mut z) = zip::ZipArchive::new(std::io::Cursor::new(blob)) else { return out };
        let mut left = MAX_INNER;
        for i in 0..z.len() {
            let Ok(mut f) = z.by_index(i) else { return out };
            if f.is_dir() || f.size() > left {
                continue;
            }
            let (size, name, mut data) = (f.size(), f.name().to_string(), vec![]);
            left -= size;
            if (&mut f).take(size).read_to_end(&mut data).is_err() {
                return out;
            }
            out.push((name, data));
        }
    } else if blob.starts_with(b"\x1f\x8b") {
        let mut data = vec![];
        if flate2::read::MultiGzDecoder::new(blob).take(MAX_INNER).read_to_end(&mut data).is_ok() {
            out.push(("gzip".into(), data));
        }
    }
    out
}

fn json_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            json_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "json") {
            out.push(p);
        }
    }
}

/// Every JSON file of the pack, its base64 strings decoded, and what zip / gzip bodies among those hold.
fn pack_blobs(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![];
    json_files(root, &mut files);
    let mut blobs = vec![];
    for path in files {
        let name = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        let raw = std::fs::read(&path).unwrap_or_default();
        let parsed = serde_json::from_slice::<Value>(&raw).ok();
        blobs.push((name.clone(), raw));
        let Some(v) = parsed else { continue };
        let mut decoded = vec![];
        b64_strings(&v, &mut decoded);
        for b in decoded {
            let inside = inner(&b);
            blobs.push((format!("{name} (decoded)"), b));
            for (member, content) in inside {
                let member: String = member.chars().take(40).collect();
                blobs.push((format!("{name} (decoded, {member})"), content));
            }
        }
    }
    blobs
}

/// Field 21 of a product in builtin/product_table.rs (None: empty or no such product).
pub fn known_product_hash(product_id: Option<u64>) -> Option<pb::V> {
    let table = PRODUCTS.iter().find(|t| matches!(t.first(), Some((1, P::I(id))) if Some(*id) == product_id))?;
    match &table.iter().rev().find(|(n, _)| *n == PRODUCT_HASH)?.1 {
        P::S(s) if !s.is_empty() => Some(pb::V::Bytes(s.as_bytes().to_vec())),
        P::M(sub) if !sub.is_empty() => Some(pb::V::Bytes(pb::encode(&crate::builtin::products::to_fields(sub)))),
        P::I(i) if *i != 0 => Some(pb::V::Int(*i)),
        _ => None,
    }
}

/// Products in connect/demux.json whose field 21 (40 hex digits) is not Steep's known one: maybe per account.
pub fn product_hash_findings(root: &Path) -> Vec<String> {
    let path = root.join("connect").join("demux.json");
    let Some(raw) = std::fs::read(&path).ok().and_then(|r| serde_json::from_slice::<Value>(&r).ok()) else {
        return vec![];
    };
    let mut findings = vec![];
    for b64 in raw.get("products").and_then(Value::as_array).into_iter().flatten() {
        let Some(fields) = b64.as_str().and_then(crate::util::unb64).and_then(|b| pb::try_decode(&b)) else {
            continue;
        };
        let pid = pb::int(&fields, 1);
        let Some(value) = pb::bytes(&fields, PRODUCT_HASH).filter(|v| HEX40.is_match(v)) else { continue };
        if !matches!(known_product_hash(pid), Some(pb::V::Bytes(k)) if k == value.to_ascii_uppercase()) {
            findings.push(format!("connect/demux.json: product {} field {PRODUCT_HASH} {}... (not Steep's known value: \
                                   maybe per account)", pid.map_or("None".into(), |p| p.to_string()),
                                  String::from_utf8_lossy(&value[..8])));
        }
    }
    findings
}

/// Whether `needle` occurs in `hay` with no ASCII word character right before or after it.
fn contains_word(hay: &[u8], needle: &[u8]) -> bool {
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    !needle.is_empty() && hay.windows(needle.len()).enumerate().any(|(i, w)| {
        w == needle && !word(i.checked_sub(1).and_then(|j| hay.get(j))) && !word(hay.get(i + needle.len()))
    })
}

/// -> findings "file: marker" (empty = clean). `allowed` = markers known to be game-wide (e.g. the game's own
/// app/space ids, which also appear inside personal replies).
pub fn check(root: &Path, markers: &HashSet<Marker>, allowed: &HashSet<String>) -> Vec<String> {
    let markers: Vec<&Marker> = markers.iter().filter(|m| !allowed.contains(m.text())).collect();
    let lowered: Vec<(&str, Vec<u8>)> = markers.iter().filter(|m| m.text().chars().count() >= MIN_MARKER)
        .map(|m| (m.text(), m.text().as_bytes().to_ascii_lowercase())).collect();
    let names: Vec<&str> = markers.iter().filter_map(|m| match m {
        Marker::Name(t) if (MIN_NAME..MIN_MARKER).contains(&t.chars().count()) => Some(t.as_str()),
        _ => None,
    }).collect();
    let short = |m: &str| m.chars().take(12).collect::<String>();
    let mut findings: BTreeSet<String> = product_hash_findings(root).into_iter().collect();
    for (label, blob) in pack_blobs(root) {
        let low = blob.to_ascii_lowercase();
        for (m, n) in &lowered {
            if !n.is_empty() && low.windows(n.len()).any(|w| w == n.as_slice()) {
                findings.insert(format!("{label}: {}...", short(m)));
            }
        }
        for m in &names {
            if contains_word(&blob, m.as_bytes()) {
                findings.insert(format!("{label}: {}...", short(m)));
            }
        }
    }
    findings.into_iter().collect()
}

fn identifying_values(value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if let Some(text) = scalar(v).filter(|_| IDENTIFYING_KEYS.contains(&k.to_lowercase().as_str())) {
                    if !text.trim().is_empty() && !PLACEHOLDER.is_match(&text) {
                        out.push((k.clone(), text));
                    }
                }
                identifying_values(v, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| identifying_values(v, out)),
        _ => {}
    }
}

/// The checks that need no recordings (a pack zipped for someone else): values of identifying keys (not the
/// game's own ids), e-mail addresses, product field 21. -> findings.
pub fn self_check(root: &Path) -> Vec<String> {
    let game_ids = [APP_ID, SPACE_ID, CONNECT_APP_ID, CONNECT_SPACE_ID];
    let mut findings: BTreeSet<String> = product_hash_findings(root).into_iter().collect();
    for (name, blob) in pack_blobs(root) {
        if let Some(m) = EMAIL.find(&blob) {
            findings.insert(format!("{name}: e-mail address {}...", String::from_utf8_lossy(&m.as_bytes()[..3])));
        }
        let Ok(data) = serde_json::from_slice::<Value>(&blob) else { continue };
        let mut values = vec![];
        identifying_values(&data, &mut values);
        for (k, v) in values.into_iter().filter(|(_, v)| !game_ids.contains(&v.as_str())) {
            findings.insert(format!("{name}: {k}={}...", v.chars().take(12).collect::<String>()));
        }
    }
    findings.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_of_three_characters_are_markers() {
        let mut seen = HashSet::new();
        walk(&serde_json::json!({"nameOnPlatform": "Zed", "userId": "abc", "username": "  Al  "}), &mut seen);
        assert_eq!(seen, [Marker::Name("Zed".into()), Marker::Value("  Al  ".into())].into());
        assert!(contains_word(b"(Zed)", b"Zed") && !contains_word(b"Zed_1", b"Zed") && !contains_word(b"aZed", b"Zed"));
    }
}
