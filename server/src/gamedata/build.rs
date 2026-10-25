//! Build a game-data pack from recordings of real sessions (your own captures):
//!
//! ```text
//! powder-server gamedata build <recordings dir> <pack dir>
//! ```
//!
//! Only allow-listed, game-wide replies are copied; Ubisoft Club actions are rewritten to drop player state.
//! The privacy check (privacy.rs) must then find no identifying value from the rest of the recordings in the
//! pack, or the build fails and nothing is written.

use super::{pack, privacy};
use crate::constants::{APP_ID, SANDBOX, SPACE_ID};
use crate::demux::pb;
use anyhow::{bail, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::sync::LazyLock;

const GUID: &str = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}";

fn id() -> String {
    format!("(?:{GUID}|global|all)")
}

/// (method, path regex without query): game-wide configuration only, nothing under a profile or user.
pub(crate) static GAME_ALLOW: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        format!("/v2/applications/{GUID}/configuration"),
        format!("/v1/spaces/{GUID}/configs/primarystore"),
        format!("/v4/spaces/{GUID}/configs/events"),
        format!("/v1/spaces/{GUID}/items/all"),
        r"/v2/spaces/global/ubiconnect/challenges/api/legacy/empty".into(),
    ].into_iter().map(|p| ("GET", Regex::new(&format!("^(?:{p})$")).unwrap())).collect()
});

pub(crate) static CONNECT_ALLOW: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let id = id();
    [
        format!("/v1/applications/{id}/configuration"),
        format!("/v1/applications/{id}/parameters"),
        r"/v1/applications/global/webservices/ubisoftplus/vault/products".into(),
        r"/v1/mdm/products".into(),
        format!("/v1/spaces/{id}/parameters"),
        format!("/v1/spaces/{id}/global/ubiconnect/games/api"),
        r"/v1/spaces/global/ubiconnect/games/api".into(),
        format!("/v1/spaces/{id}/global/ubiconnect/configcache/api/connecturls"),
        format!("/v1/spaces/{id}/global/ubiconnect/challenges/api"),
        format!("/v1/spaces/{id}/global/ubiconnect/rewards/api"),
        r"/v1/spaces/global/ubiconnect/economy/api/metaprogression".into(),
        r"/v4/spaces/global/ubiconnect/configcache/api/(preauth|postauth)".into(),
        format!("/v4/spaces/{GUID}/configs/events"),
        format!("/v2/spaces/{GUID}/configs/news/tags"),
        r"/orbit/uplay_launcher_14_0/free_games/latest\.txt".into(),
        r"/orbit/uplay_launcher_3_0/assets/[0-9a-f]+\.zip".into(), // achievement definitions (Connect)
    ].into_iter().map(|p| ("GET", Regex::new(&format!("^(?:{p})$")).unwrap())).collect()
});

pub const RMC_ALLOW: &[&str] = &["GameStorageProtocol::GetGameParamaters_V1", "SteepTournamentProtocol::FetchSeasonInfo_V1"];
const STEEP_PRODUCTS: &[u64] = &[3279, 3280]; // Steam entry + Ubisoft product; their add-ons follow
static OFFERS_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("/v1/spaces/{GUID}/offers$")).unwrap());
static ACTIONS_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("/v1/profiles/{GUID}/actions$")).unwrap());
pub(crate) const ACTION_PLAYER_FIELDS: &[&str] = &["profileId", "value"]; // per-player state in an action
static ANY_GUID: LazyLock<Regex> = LazyLock::new(|| Regex::new(GUID).unwrap());

#[derive(Default, Debug)]
pub struct Report {
    pub kept: BTreeMap<String, usize>,
    pub findings: Vec<String>,
}

fn base(e: &Value) -> &str {
    e.get("path").and_then(Value::as_str).unwrap_or("").split('?').next().unwrap_or("")
}

fn method(e: &Value) -> &str {
    e.get("method").and_then(Value::as_str).unwrap_or("")
}

fn allowed(rules: &[(&str, Regex)], e: &Value) -> bool {
    rules.iter().any(|(m, rx)| *m == method(e) && rx.is_match(base(e)))
}

/// Whether a reply (`path` with or without its query) is one of `rules` (validate.rs applies them to packs).
pub(crate) fn allowed_path(rules: &[(&str, Regex)], method: &str, path: &str) -> bool {
    let base = path.split('?').next().unwrap_or("");
    rules.iter().any(|(m, rx)| *m == method && rx.is_match(base))
}

fn body(e: &Value) -> Value {
    crate::util::unb64(e.get("body_b64").and_then(Value::as_str).unwrap_or(""))
        .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

fn load(path: &Path, default: Value) -> Result<Value> {
    if !path.exists() {
        return Ok(default);
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn write(path: &Path, data: &Value) -> Result<()> {
    std::fs::write(path, crate::util::dumps_compact(data))?;
    Ok(())
}

fn offers(game: &[Value]) -> (Vec<Value>, HashSet<usize>) {
    let (mut out, mut seen, mut used) = (vec![], HashSet::new(), HashSet::new());
    for (i, e) in game.iter().enumerate() {
        if method(e) == "GET" && OFFERS_PATH.is_match(base(e)) {
            used.insert(i);
            for o in body(e).get("offers").and_then(Value::as_array).into_iter().flatten() {
                let id = o.get("offerId").and_then(Value::as_str).unwrap_or("").to_string();
                if seen.insert(id) {
                    out.push(o.clone());
                }
            }
        }
    }
    (out, used)
}

/// Ubisoft Club action definitions, without whose they are and how far that player got.
fn actions(game: &[Value]) -> (Vec<Value>, HashSet<usize>) {
    let (mut out, mut seen, mut used): (Vec<Value>, HashSet<String>, HashSet<usize>) = (vec![], HashSet::new(), HashSet::new());
    for (i, e) in game.iter().enumerate() {
        if method(e) == "GET" && ACTIONS_PATH.is_match(base(e)) {
            used.insert(i);
            for a in body(e).get("actions").and_then(Value::as_array).into_iter().flatten() {
                let id = a.get("id").map(|v| v.to_string()).unwrap_or_default();
                if seen.insert(id) {
                    let clean: Map<String, Value> = a.as_object().into_iter().flatten()
                        .filter(|(k, _)| !ACTION_PLAYER_FIELDS.contains(&k.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone())).collect();
                    out.push(Value::Object(clean));
                }
            }
        }
    }
    (out, used)
}

/// Steep's entries (and its add-ons) from the recorded owned-games reply; the rest of the library stays out.
fn products(demux: &Value) -> Result<Vec<Vec<u8>>> {
    let replies = demux.pointer("/services/ownership_service/2").and_then(Value::as_array);
    let Some(first) = replies.and_then(|r| r.first()).and_then(Value::as_str) else { return Ok(vec![]) };
    let reply = crate::util::unb64(first).unwrap_or_default();
    let outer = pb::decode(&reply)?;
    let msg = pb::decode(pb::bytes(&outer, 1).unwrap_or(&[]))?;
    let init = pb::decode(pb::bytes(&msg, 2).unwrap_or(&[]))?;
    let list = pb::decode(pb::bytes(&init, 2).unwrap_or(&[]))?;
    let mut decoded: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for f in &list {
        if let pb::V::Bytes(v) = &f.1 {
            if let Some(pid) = pb::int(&pb::decode(v)?, 1) {
                decoded.insert(pid, v.clone());
            }
        }
    }
    let mut wanted: BTreeSet<u64> = STEEP_PRODUCTS.iter().copied().collect();
    for pid in STEEP_PRODUCTS {
        if let Some(v) = decoded.get(pid) {
            wanted.extend(pb::decode(v)?.iter().filter(|f| f.0 == 16).filter_map(|f| match f.1 {
                pb::V::Int(x) => Some(x),
                _ => None,
            }));
        }
    }
    wanted.iter().filter_map(|p| decoded.get(p)).map(|e| known_hash(e)).collect()
}

/// A product entry with its field 21 (40 hex digits) as Steep's known one (builtin/product_table.rs), or without
/// it: another value could be per account (privacy::product_hash_findings).
fn known_hash(entry: &[u8]) -> Result<Vec<u8>> {
    let fields = pb::decode(entry)?;
    let Some(value) = pb::bytes(&fields, privacy::PRODUCT_HASH).filter(|v| privacy::HEX40.is_match(v)) else {
        return Ok(entry.to_vec());
    };
    let known = privacy::known_product_hash(pb::int(&fields, 1));
    if matches!(&known, Some(pb::V::Bytes(k)) if *k == value.to_ascii_uppercase()) {
        return Ok(entry.to_vec());
    }
    Ok(pb::encode(&match known {
        Some(k) => pb::replace(&fields, privacy::PRODUCT_HASH, k),
        None => fields.into_iter().filter(|f| f.0 != privacy::PRODUCT_HASH).collect(),
    }))
}

pub fn build(recordings: &Path, out: &Path) -> Result<Report> {
    let game: Vec<Value> = load(&recordings.join("fixtures.json"), json!([]))?.as_array().cloned().unwrap_or_default();
    let rmc = load(&recordings.join("fixtures-rmc.json"), json!({}))?.as_object().cloned().unwrap_or_default();
    let connect: Vec<Value> = load(&recordings.join("connect-fixtures.json"), json!([]))?.as_array().cloned()
        .unwrap_or_default();
    let demux = load(&recordings.join("demux.json"), json!({}))?;
    if game.is_empty() {
        bail!("no recordings in {} (fixtures.json missing)", recordings.display());
    }
    let mut report = Report::default();
    let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = out.with_file_name(format!("{name}.building"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("connect"))?;

    let (offers, offer_idx) = offers(&game);
    let (actions, _) = actions(&game);
    let game_kept: BTreeSet<usize> = (0..game.len()).filter(|&i| allowed(&GAME_ALLOW, &game[i])).collect();
    write(&tmp.join("http.json"), &Value::Array(game_kept.iter().map(|&i| game[i].clone()).collect()))?;
    write(&tmp.join("offers.json"), &json!({"offers": offers}))?;
    write(&tmp.join("actions.json"), &json!({"actions": actions}))?;
    let rmc_kept: Map<String, Value> = rmc.iter().filter(|(m, _)| RMC_ALLOW.contains(&m.as_str()))
        .map(|(k, v)| (k.clone(), v.clone())).collect();
    write(&tmp.join("rmc.json"), &Value::Object(rmc_kept.clone()))?;
    let connect_kept: BTreeSet<usize> = (0..connect.len()).filter(|&i| allowed(&CONNECT_ALLOW, &connect[i])).collect();
    write(&tmp.join("connect").join("http.json"),
          &Value::Array(connect_kept.iter().map(|&i| connect[i].clone()).collect()))?;
    let products = products(&demux)?;
    write(&tmp.join("connect").join("demux.json"), &json!({
        "patch_info": demux.get("patch_info").cloned().unwrap_or(Value::Null),
        "products": products.iter().map(|p| crate::util::b64(p)).collect::<Vec<_>>()}))?;
    for (k, v) in [("http.json", game_kept.len()), ("offers.json", offers.len()), ("actions.json", actions.len()),
                   ("rmc.json", rmc_kept.len()), ("connect/http.json", connect_kept.len()),
                   ("connect/products", products.len())] {
        report.kept.insert(k.into(), v);
    }

    // privacy: identifying values from everything that was NOT kept must not appear in the pack
    let game_used: HashSet<usize> = game_kept.iter().copied().chain(offer_idx).collect();
    let connect_used: HashSet<usize> = connect_kept.iter().copied().collect();
    let mut markers = privacy::markers_from_http(&game, &game_used);
    markers.extend(privacy::markers_from_http(&connect, &connect_used));
    markers.extend(privacy::markers_from_rmc(&rmc, RMC_ALLOW));
    markers.extend(privacy::markers_from_demux(&demux));
    let mut game_ids: HashSet<String> = [APP_ID.to_string(), SPACE_ID.to_string()].into();
    for (idx, entries) in [(&game_kept, &game), (&connect_kept, &connect)] {
        for &i in idx {
            let path = entries[i].get("path").and_then(Value::as_str).unwrap_or("");
            game_ids.extend(ANY_GUID.find_iter(path).map(|m| m.as_str().to_lowercase()));
        }
    }
    report.findings = privacy::check(&tmp, &markers, &game_ids);
    if !report.findings.is_empty() {
        std::fs::remove_dir_all(&tmp)?;
        return Ok(report);
    }
    let mut info = Map::new();
    info.insert("built".into(), json!(chrono::Local::now().format("%Y-%m-%d").to_string()));
    info.insert("sandbox".into(), json!(SANDBOX));
    info.insert("app_id".into(), json!(APP_ID));
    info.insert("space_id".into(), json!(SPACE_ID));
    info.insert("contents".into(), serde_json::to_value(&report.kept)?);
    pack::write_manifest(&tmp, info)?;
    if out.exists() {
        std::fs::remove_dir_all(out)?;
    }
    std::fs::rename(&tmp, out)?;
    Ok(report)
}

/// A small, entirely made-up recordings set (tests/synthetic.py on main: the parts the builder reads), and the
/// pack the real builder makes of it.
#[cfg(test)]
pub(crate) mod synthetic {
    use super::*;
    use crate::demux::pb::F;
    use std::io::Write;
    use std::path::PathBuf;

    pub const PLAYER: &str = "00000000-0000-4000-8000-000000000001";
    pub const PLAYER_NAME: &str = "TestPlayer";
    pub const FRIEND: &str = "00000000-0000-4000-8000-000000000002";
    pub const FRIEND_NAME: &str = "SecretFriendName"; // personal: must never reach a pack
    pub const PLAYER_EMAIL: &str = "player@example.invalid"; // personal
    pub const UBI: &str = "public-ubiservices.ubi.com";
    pub const STEEP_STEAM: u64 = 3279;
    pub const STEEP: u64 = 3280;
    pub const ADDON: u64 = 4001;
    pub const OTHER_GAME: u64 = 1234;
    pub const ICON: &[u8] = b"\x89PNG\r\n\x1a\nmade-up";
    pub const ACHIEVEMENT_NAMES: [(i64, &str, &str); 4] = [(1, "First steps", "Do the first thing"),
        (4, "Four", "Do a fourth thing"), (15, "Fifteen", "Do it fifteen times"), (30, "Thirty", "Not earned yet")];

    pub fn entry(host: &str, path: &str, body: &[u8], content_type: &str) -> Value {
        json!({"method": "GET", "host": host, "path": path, "status": 200, "content_type": content_type,
               "body_b64": crate::util::b64(body)})
    }

    fn http(method: &str, path: &str, body: Value) -> Value {
        json!({"method": method, "host": UBI, "path": path, "status": 200, "content_type": "application/json",
               "body_b64": crate::util::b64(crate::util::dumps(&body).as_bytes())})
    }

    pub fn achievement_zip() -> Vec<u8> {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("en-US_loc.txt", opts).unwrap();
        let text: String = ACHIEVEMENT_NAMES.iter().map(|(i, n, d)| format!("{i}\t{n}\t{d}\n")).collect();
        z.write_all(format!("\u{feff}{text}").as_bytes()).unwrap();
        for (i, _, _) in ACHIEVEMENT_NAMES {
            z.start_file(format!("{i}.png"), opts).unwrap();
            z.write_all(ICON).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    pub fn product(pid: u64, addons: &[u64], extra: Vec<F>) -> Vec<u8> {
        let mut fields = vec![F::int(1, pid)];
        fields.extend(addons.iter().map(|a| F::int(16, *a)));
        fields.push(F::str(17, "version: 2.0\r\nroot:\r\n  name: l1\r\n  cloud_saves: yes\r\n"));
        fields.extend(extra);
        pb::encode(&fields)
    }

    pub fn owned_games_reply(products: &[Vec<u8>]) -> Vec<u8> {
        pb::encode(&[F::msg(1, vec![F::int(1, 1), F::msg(2, vec![F::int(1, 1),
            F::msg(2, products.iter().map(|p| F::bytes(1, p)).collect()), F::int(5, 0), F::int(6, 0)])])])
    }

    /// Steep's Steam entry (with its add-on), Steep, the add-on, and another game (the rest of the library).
    pub fn products(extra: impl Fn() -> Vec<F>) -> Vec<Vec<u8>> {
        vec![product(STEEP_STEAM, &[ADDON], extra()), product(STEEP, &[], extra()), product(ADDON, &[], extra()),
             product(OTHER_GAME, &[], extra())]
    }

    fn friends_reply() -> Vec<u8> {
        let entry = vec![F::int(1, 0), F::msg(2, vec![F::str(1, FRIEND), F::str(2, FRIEND_NAME), F::bytes(3, b""),
                                                      F::int(4, 0)]), F::int(3, 3)];
        pb::encode(&[F::msg(1, vec![F::int(1, 2), F::msg(22, vec![F::int(1, 1), F::msg(2, entry)])])])
    }

    pub fn recordings(folder: &Path, products: &[Vec<u8>]) -> PathBuf {
        std::fs::create_dir_all(folder).unwrap();
        let offer = |id: &str, kind: &str| json!({"offerId": id, "spaceId": SPACE_ID, "type": kind, "tags": ["STEEP"],
                                                   "revision": 6, "items": []});
        let login = json!({"platformType": "uplay", "ticket": "recorded-ticket", "profileId": PLAYER, "userId": PLAYER,
                           "nameOnPlatform": PLAYER_NAME, "environment": "Prod", "spaceId": SPACE_ID});
        let action = json!({"spaceId": SPACE_ID, "profileId": PLAYER, "id": "1000", "value": 3,
                            "name": "The legend starts here", "xp": 20, "description": "Play", "isBadge": false});
        let fixtures = json!([
            http("POST", "/v3/profiles/sessions", login),
            http("GET", &format!("/v1/spaces/{SPACE_ID}/offers?tags=SKI&offset=0&limit=100"),
                 json!({"offers": [offer("0000000b-0000-4000-8000-0000000000a1", "shop")]})),
            http("GET", &format!("/v2/applications/{APP_ID}/configuration"), json!({"recorded": true})),
            http("GET", &format!("/v1/profiles/{PLAYER}/actions?spaceId={SPACE_ID}"), json!({"actions": [action]})),
            http("GET", "/v2/profiles/me/friends", json!({"friends": [
                {"pid": FRIEND, "nameOnPlatform": FRIEND_NAME, "state": "Friends", "lastModified": "x"}]})),
        ]);
        std::fs::write(folder.join("fixtures.json"), crate::util::dumps(&fixtures)).unwrap();
        let connect = json!([
            http("GET", "/v3/users/me", json!({"userId": PLAYER, "nameOnPlatform": PLAYER_NAME, "email": PLAYER_EMAIL})),
            http("GET", &format!("/v1/spaces/{SPACE_ID}/parameters?parameterGroups=x"), json!({"parameters": {"a": 1}})),
            entry("static3.cdn.ubi.com", "/orbit/uplay_launcher_3_0/assets/0123abcd.zip", &achievement_zip(),
                  "application/zip"),
        ]);
        std::fs::write(folder.join("connect-fixtures.json"), crate::util::dumps(&connect)).unwrap();
        let demux = json!({"client_version": 13368,
            "patch_info": crate::util::b64(&pb::encode(&[F::int(1, 1), F::int(5, 13368)])),
            "services": {"ownership_service": {"2": [crate::util::b64(&owned_games_reply(products))]},
                         "friends_service": {"22": [crate::util::b64(&friends_reply())]}}});
        std::fs::write(folder.join("demux.json"), crate::util::dumps(&demux)).unwrap();
        let rmc = json!({
            "GameStorageProtocol::GetGameParamaters_V1": [{"params_sha1": "00", "params_len": 0, "body": "0100"}],
            "MatchmakingProtocol::LeaveSession_V1": [{"params_sha1": "01", "params_len": 4, "body": ""}]});
        std::fs::write(folder.join("fixtures-rmc.json"), crate::util::dumps(&rmc)).unwrap();
        folder.to_path_buf()
    }

    /// A pack built from the made-up recordings with the real builder, in `dir/gamedata`.
    pub fn pack(dir: &Path) -> PathBuf {
        let out = dir.join("gamedata");
        let report = build(&recordings(&dir.join("recordings"), &products(Vec::new)), &out).unwrap();
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        out
    }
}
