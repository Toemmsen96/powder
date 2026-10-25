//! Privacy check for a game-data pack: nothing that identifies a person may end up in it.
//!
//! Markers are collected from the *personal* parts of the source recordings (everything the builder did not
//! allow-list): the values of identifying JSON keys (profile/user ids, names, emails, tickets, ip), every GUID
//! in friend lists, profile lookups and leaderboards, and Connect's friends. Then every file of the pack
//! (including base64 bodies, decoded) is searched for them. Any hit fails the check.

use crate::demux::pb;
use regex::bytes::Regex as BytesRegex;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashSet};
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

fn guids(raw: &[u8]) -> impl Iterator<Item = String> + '_ {
    GUID.find_iter(raw).map(|m| String::from_utf8_lossy(m.as_bytes()).to_lowercase())
}

fn walk(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if IDENTIFYING_KEYS.contains(&k.to_lowercase().as_str()) {
                    let text = match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
                        _ => None,
                    };
                    if let Some(t) = text.filter(|t| t.chars().count() >= 6) {
                        out.insert(t);
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
pub fn markers_from_http(entries: &[Value], kept: &HashSet<usize>) -> HashSet<String> {
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
pub fn markers_from_rmc(rmc: &Map<String, Value>, kept: &[&str]) -> HashSet<String> {
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
fn friend_names(raw: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(top) = pb::try_decode(raw) else { return names };
    let Some(outer) = pb::sub(&top, 1) else { return names };
    let Some(body) = pb::sub(&outer, 22) else { return names };
    for f in &body {
        if let (2, pb::V::Bytes(entry)) = (f.0, &f.1) {
            let inner = pb::try_decode(entry).and_then(|e| pb::sub(&e, 2)).unwrap_or_default();
            if let Some(name) = pb::bytes(&inner, 2).filter(|n| n.len() >= 4) {
                names.insert(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    names
}

/// Friends (ids, names) and the player's id from Connect's demux replies.
pub fn markers_from_demux(demux: &Value) -> HashSet<String> {
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

fn json_files(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            json_files(root, &p, out);
        } else if p.extension().is_some_and(|e| e == "json") {
            out.push(p);
        }
    }
}

/// -> findings "file: marker" (empty = clean). `allowed` = markers known to be game-wide (e.g. the game's own
/// app/space ids, which also appear inside personal replies).
pub fn check(root: &Path, markers: &HashSet<String>, allowed: &HashSet<String>) -> Vec<String> {
    let needles: Vec<(String, Vec<u8>)> = markers.iter()
        .filter(|m| !allowed.contains(*m) && m.chars().count() >= 6)
        .map(|m| (m.clone(), m.to_lowercase().into_bytes())).collect();
    let mut files = vec![];
    json_files(root, root, &mut files);
    let mut findings = BTreeSet::new();
    for path in files {
        let name = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        let raw = std::fs::read(&path).unwrap_or_default();
        let mut blobs = vec![(name.clone(), raw.clone())];
        if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
            let mut decoded = vec![];
            b64_strings(&v, &mut decoded);
            blobs.extend(decoded.into_iter().map(|b| (format!("{name} (decoded)"), b)));
        }
        for (label, blob) in blobs {
            let low = blob.to_ascii_lowercase();
            for (m, n) in &needles {
                if !n.is_empty() && low.windows(n.len()).any(|w| w == n.as_slice()) {
                    let short: String = m.chars().take(12).collect();
                    findings.insert(format!("{label}: {short}..."));
                }
            }
        }
    }
    findings.into_iter().collect()
}
