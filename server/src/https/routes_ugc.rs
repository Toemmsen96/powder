//! Steep's UGC service (steep-prod-pc.steep.olympus.ubi.com/ugc/v1), from the Python server's routes_ugc.py:
//! player-made content; the game uses it for photos (typeId 2). Kept for the server's players (the `ugc` table,
//! repos.rs), files in data/ugc/. Trails, saved replays and challenge ghosts are profile entities with a file
//! instead (routes_ubi.rs).
//!
//! From recordings of Ubisoft's service: a list (GET /ugc/v1/?algo=Steep&type_id=&world_id=&profile_id=...) of
//! items {id, typeId, worldId, profileId, pubDate, metadata, likeCounter, downloadCounter, liked, tags, links
//! {creationTimestamp, expirationTimestamp, urls {Thumbnail, Data}}, ...}; PUT/DELETE {id}/like and PUT
//! {id}/download answer 204. The files were on Amazon S3 behind signed links; here they are on this server
//! (/_powder/ugc/<id>[_thumbnail]).
//!
//! Seen live: a photo is created with POST /ugc/v1/ {typeId 2, worldId, metadata (world position, region, time
//! of day, camera settings, ...), tags, datas: the JPEG, Thumbnail: a small JPEG, both base64}; the two pictures
//! are kept as the item's files (Data, Thumbnail), not in the item. The JPEGs themselves carry no metadata.
//! Deleting is DELETE {id} followed by PUT {id}/hide. {id}/url comes from the game's code.

use super::routes_ubi::{accounts_required, acting, empty, file_reply, forbidden, host, no_session, owner, query,
                        remove_file, Actor};
use super::{Ctx, Reply, Route};
use crate::repos::UgcItem;
use anyhow::{Context as _, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static FILE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f-]{36}(_thumbnail)?$").unwrap());
const LINK_LIFETIME_S: i64 = 3600;
/// Inline pictures of a created item -> their file's suffix.
const FILE_FIELDS: [(&str, &str); 2] = [("datas", ""), ("Thumbnail", "_thumbnail")];

/// In routes_ugc.py's registration order.
pub fn routes() -> Vec<Route> {
    vec![
        Route::new("GET", "/ugc/v1/", ugc_list),
        Route::new("POST", "/ugc/v1/", ugc_create),
        Route::new("POST", "/ugc/v1", ugc_create),
        Route::new("*", "/ugc/v1/{ugc_id}/{action}", ugc_action),
        Route::new("*", "/ugc/v1/{ugc_id}", ugc_item),
        Route::new("*", "/_powder/ugc/{name}", ugc_file),
    ]
}

/// Who is asking: the session's player; without one the default player while accounts are not required, else
/// refused (Err: the 401; routes_ubi.rs' "who may act").
fn me(c: &Ctx) -> std::result::Result<Actor, Reply> {
    acting(c)
}

fn files(c: &Ctx) -> PathBuf {
    c.state.config.data_dir().join("ugc")
}

fn links(c: &Ctx, ugc_id: &str) -> Value {
    let now = chrono::Utc::now().timestamp();
    let base = format!("https://{}/_powder/ugc/{ugc_id}", host(c.req));
    json!({"creationTimestamp": now, "expirationTimestamp": now + LINK_LIFETIME_S,
           "urls": {"Thumbnail": format!("{base}_thumbnail"), "Data": base}})
}

/// An item as the game gets it: the fields it sent that we do not know first, then ours.
fn out(c: &Ctx, item: &UgcItem, me: &str) -> Value {
    let mut m: Map<String, Value> = item.extra.iter()
        .filter(|(k, _)| !FILE_FIELDS.iter().any(|(f, _)| f == k)) // files, not part of the item
        .map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in [
        ("id", json!(item.ugc_id)), ("typeId", json!(item.type_id)), ("worldId", json!(item.world_id)),
        ("profileId", json!(item.profile_id)), ("pubDate", json!(item.pub_date)), ("metadata", item.metadata.clone()),
        ("likeCounter", json!(item.liked_by.len())), ("downloadCounter", json!(item.downloads)),
        ("liked", json!(item.liked_by.contains(&me.to_lowercase()))), ("tags", item.tags.clone()),
        ("links", links(c, &item.ugc_id)), ("showcase", Value::Null), ("reportCounter", json!(0)),
        ("isBanned", json!(false)), ("isBanProtected", json!(false)), ("platform", json!("uplay")),
        ("version", json!(1)),
    ] {
        m.insert(k.into(), v);
    }
    Value::Object(m)
}

/// Python's `int(value)`, None where it raises (TypeError / ValueError).
fn py_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn ugc_list(c: &Ctx) -> Reply {
    let q = query(c.req);
    let param = |name: &str| py_int(q.get(name).map(|v| json!(v)).as_ref());
    let profiles: Vec<String> = q.0.iter().filter(|(k, _)| k == "profile_id").map(|(_, v)| v.clone()).collect();
    let me = match me(c) {
        Ok(me) => me.pid,
        Err(refused) => return refused,
    };
    let listed = c.state.repos.ugc_find(param("type_id"), param("world_id"), &profiles)
        .map(|items| items.iter().map(|i| out(c, i, &me)).collect::<Vec<_>>());
    match listed {
        Ok(items) => {
            let how = format!("ugc({})", items.len());
            Reply::json(&Value::Array(items), &how, 200)
        }
        Err(e) => Reply::internal_error(&e),
    }
}

/// Python's lenient `base64.b64decode`: characters outside the alphabet are skipped; None if it raises.
fn b64decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    if !text.is_ascii() {
        return None;
    }
    let kept: String = text.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')).collect();
    base64::engine::general_purpose::STANDARD.decode(kept).ok()
}

/// A base64 picture sent inside the item -> its file.
fn save_inline(dir: &Path, name: &str, value: Option<&Value>) -> Result<()> {
    let Some(data) = value.and_then(Value::as_str).and_then(b64decode).filter(|d| !d.is_empty()) else {
        return Ok(());
    };
    std::fs::create_dir_all(dir).and_then(|_| std::fs::write(dir.join(name), data))
        .with_context(|| format!("writing {}", dir.join(name).display()))
}

fn ugc_create(c: &Ctx) -> Reply {
    let me = match me(c) {
        Ok(me) => me.pid,
        Err(refused) => return refused,
    };
    match create(c, &me) {
        Ok(reply) => reply,
        Err(e) => Reply::internal_error(&e),
    }
}

fn create(c: &Ctx, me: &str) -> Result<Reply> {
    let raw = &c.req.body;
    let mut body = match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let pictures: Vec<(&str, Option<Value>)> = FILE_FIELDS.iter().map(|(f, s)| (*s, body.shift_remove(*f))).collect();
    let text = crate::util::dumps(&Value::Object(body.clone())); // ASCII: any cut is a char boundary
    log::info!(target: "powder.https", "ugc create from {me} ({} B): {}", raw.len(), &text[..text.len().min(1500)]);
    let metadata = body.get("metadata").filter(|m| m.is_object()).cloned().unwrap_or_else(|| json!({}));
    // body.get("typeId", body.get("type_id")): a key that is there counts, even with null
    let either = |a: &str, b: &str| if body.contains_key(a) { body.get(a) } else { body.get(b) };
    let type_id = py_int(either("typeId", "type_id")).unwrap_or(0);
    let world_id = py_int(either("worldId", "world_id").or_else(|| metadata.get("worldId")));
    let tags = body.get("tags").filter(|t| t.is_array()).cloned().unwrap_or_else(|| json!([]));
    const KNOWN: [&str; 8] = ["metadata", "typeId", "type_id", "worldId", "world_id", "tags", "id", "profileId"];
    let extra: Map<String, Value> = body.iter().filter(|(k, _)| !KNOWN.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone())).collect();
    let id = uuid::Uuid::new_v4().to_string();
    let item = c.state.repos.ugc_create(&id, type_id, world_id, me, &metadata, &tags, &extra)?;
    for (suffix, picture) in &pictures {
        save_inline(&files(c), &format!("{id}{suffix}"), picture.as_ref())?;
    }
    log::info!(target: "powder.https", "ugc {id} created (type {type_id}, world {}) by {me}",
               world_id.map(|w| w.to_string()).unwrap_or_else(|| "None".into()));
    Ok(Reply::json(&out(c, &item, me), "ugc-created", 200))
}

fn ugc_item(c: &Ctx) -> Reply {
    let id = c.param("ugc_id");
    let me = match me(c) {
        Ok(me) => me.pid,
        Err(refused) => return refused,
    };
    let item = match c.state.repos.ugc_get(id) {
        Ok(Some(item)) => item,
        Ok(None) => return Reply::json(&json!({"error": "not found"}), "?", 404),
        Err(e) => return Reply::internal_error(&e),
    };
    if c.req.method != "DELETE" {
        return Reply::json(&out(c, &item, &me), "ugc-item", 200);
    }
    if item.profile_id == me.to_lowercase() {
        if let Err(e) = c.state.repos.ugc_delete(id) {
            return Reply::internal_error(&e);
        }
        for name in [id.to_string(), format!("{id}_thumbnail")] {
            remove_file(&files(c).join(name));
        }
        log::info!(target: "powder.https", "ugc {id} deleted");
    }
    empty(204, "ugc-deleted")
}

fn ugc_action(c: &Ctx) -> Reply {
    let (id, action) = (c.param("ugc_id"), c.param("action"));
    let how = format!("ugc-{action}");
    let repos = &c.state.repos;
    let me = match me(c) {
        Ok(me) => me.pid,
        Err(refused) => return refused,
    };
    let done = (|| -> Result<Option<Reply>> {
        let Some(item) = repos.ugc_get(id)? else { return Ok(None) }; // e.g. the hide the game sends after deleting
        match action {
            "like" => repos.ugc_like(id, &me, c.req.method != "DELETE")?,
            "download" => repos.ugc_downloaded(id)?,
            "hide" if item.profile_id == me.to_lowercase() => repos.ugc_hide(id)?,
            "hide" => {}
            "url" => return Ok(Some(Reply::json(&links(c, id), "ugc-url", 200))),
            _ => log::info!(target: "powder.https", "ugc {} {id}/{action}: not known, acknowledged", c.req.method),
        }
        Ok(None)
    })();
    match done {
        Ok(Some(reply)) => reply,
        Ok(None) => empty(204, &how),
        Err(e) => Reply::internal_error(&e),
    }
}

/// The files behind an item's links: the game uploads (PUT/POST) and downloads (GET) them here. Downloads are
/// open (the links are in every listing); once accounts are required, only the item's owner (its session)
/// uploads.
fn ugc_file(c: &Ctx) -> Reply {
    match file(c) {
        Ok(reply) => reply,
        Err(e) => Reply::internal_error(&e),
    }
}

fn file(c: &Ctx) -> Result<Reply> {
    let name = c.param("name");
    let id = name.split('_').next().unwrap_or_default();
    let item = if FILE_NAME.is_match(name) { c.state.repos.ugc_get(id)? } else { None };
    let Some(item) = item else { return Ok(empty(404, "?")) };
    let method = c.req.method.as_str();
    if matches!(method, "PUT" | "POST") {
        match accounts_required(c) {
            Ok(false) => {}
            Ok(true) => match owner(c) {
                None => return Ok(no_session(c)),
                Some(me) if me != item.profile_id => return Ok(forbidden(c, "not your item")),
                Some(_) => {}
            },
            Err(reply) => return Ok(reply),
        }
    }
    let dir = files(c);
    let mut path = dir.join(name);
    for (field, suffix) in FILE_FIELDS {
        // stored inside the item before files were
        if !dir.join(format!("{}{suffix}", item.ugc_id)).exists() {
            save_inline(&dir, &format!("{}{suffix}", item.ugc_id), item.extra.get(field))?;
        }
    }
    if method == "GET" && name.ends_with("_thumbnail") && !path.exists() {
        path = dir.join(id); // a photo is its own thumbnail
    }
    if matches!(method, "PUT" | "POST") {
        std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &c.req.body))
            .with_context(|| format!("writing {}", path.display()))?;
        let kind = c.req.header("Content-Type").and_then(|t| t.split(';').next()).map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty()).unwrap_or_else(|| "application/octet-stream".into());
        log::info!(target: "powder.https", "ugc file {name} uploaded ({} B, {kind})", c.req.body.len());
        return Ok(empty(200, "?"));
    }
    if !path.exists() {
        return Ok(empty(404, "?"));
    }
    Ok(file_reply(&path, "ugc-file"))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::super::{handle, HttpState, Reply};
    use crate::config::Config;
    use crate::util::b64;
    use bytes::Bytes;
    use serde_json::{json, Value};

    const OLYMPUS: (&str, &str) = ("Host", "steep-prod-pc.steep.olympus.ubi.com");
    const WORLD: i64 = 1146631465444;

    fn files_state(dir: &std::path::Path) -> HttpState {
        let mut config = Config::default();
        config.server.data_dir = dir.to_path_buf();
        let st = state_with(config, vec![], None);
        st.accounts.player(&st.accounts.default_profile_id().unwrap()).unwrap();
        st
    }

    fn send(st: &HttpState, method: &str, path: &str, data: &[u8]) -> Reply {
        let mut r = req(method, path, &[OLYMPUS]);
        r.body = Bytes::copy_from_slice(data);
        handle(st, &r)
    }

    fn get(st: &HttpState, path: &str) -> Reply {
        send(st, "GET", path, b"")
    }

    /// As this player (a new session of theirs); `None`: without a ticket.
    fn send_as(st: &HttpState, pid: Option<&str>, method: &str, path: &str, data: &[u8]) -> Reply {
        let auth = pid.map(|p| {
            format!("Ubi_v1 t={}", st.sessions.issue(st.accounts.player(p).unwrap(), "127.0.0.1", "").ticket)
        });
        let mut headers = vec![OLYMPUS];
        if let Some(a) = &auth {
            headers.push(("Authorization", a));
        }
        let mut r = req(method, path, &headers);
        r.body = Bytes::copy_from_slice(data);
        handle(st, &r)
    }

    fn path_of(url: &Value) -> String {
        url.as_str().unwrap().split_once(".com").unwrap().1.to_string()
    }

    #[test]
    fn a_photo_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let st = files_state(tmp.path());
        let created = json!({"typeId": 2, "worldId": WORLD, "tags": ["1080p"], "metadata": {"worldId": WORLD, "TimeOfDay": 11}});
        let r = send(&st, "POST", "/ugc/v1/", created.to_string().as_bytes());
        assert_eq!(r.how, "ugc-created");
        let item = body(&r);
        assert_eq!((item["typeId"].as_i64(), item["worldId"].as_i64(), item["metadata"]["TimeOfDay"].as_i64()),
                   (Some(2), Some(WORLD), Some(11)));
        let keys: Vec<&str> = item.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "typeId", "worldId", "profileId", "pubDate", "metadata", "likeCounter", "downloadCounter",
                          "liked", "tags", "links", "showcase", "reportCounter", "isBanned", "isBanProtected", "platform",
                          "version"]);
        let urls = &item["links"]["urls"];
        let id = item["id"].as_str().unwrap();
        assert_eq!(urls["Data"], format!("https://steep-prod-pc.steep.olympus.ubi.com/_powder/ugc/{id}"));
        assert_eq!(item["links"]["expirationTimestamp"].as_i64().unwrap() - item["links"]["creationTimestamp"].as_i64().unwrap(),
                   3600);
        let path = path_of(&urls["Data"]);
        assert_eq!(send(&st, "PUT", &path, b"JPEGDATA").status, 200);
        assert_eq!(send(&st, "PUT", &format!("{path}_thumbnail"), b"THUMB").status, 200);
        let r = get(&st, &format!("/ugc/v1/?algo=Steep&type_id=2&world_id={WORLD}"));
        assert_eq!(r.how, "ugc(1)");
        assert_eq!(body(&r)[0]["id"], id);
        assert_eq!(body(&get(&st, &format!("/ugc/v1/?algo=Steep&type_id=7&world_id={WORLD}"))), json!([]));
        let pid = item["profileId"].as_str().unwrap();
        assert_eq!(body(&get(&st, &format!("/ugc/v1/?profile_id=x&profile_id={pid}")))[0]["id"], id);
        let r = send(&st, "PUT", &format!("/ugc/v1/{id}/like"), b"");
        assert_eq!((r.status, r.how.as_str()), (204, "ugc-like"));
        assert_eq!(send(&st, "PUT", &format!("/ugc/v1/{id}/download"), b"").status, 204);
        let again = body(&get(&st, &format!("/ugc/v1/{id}")));
        assert_eq!((again["likeCounter"].as_i64(), again["liked"].as_bool(), again["downloadCounter"].as_i64()),
                   (Some(1), Some(true), Some(1)));
        let r = get(&st, &path);
        assert_eq!((r.status, r.how.as_str(), r.body.as_slice()), (200, "ugc-file", &b"JPEGDATA"[..]));
        assert_eq!(get(&st, &format!("{path}_thumbnail")).body, b"THUMB");
        assert_eq!(send(&st, "DELETE", &format!("/ugc/v1/{id}/like"), b"").status, 204);
        assert_eq!(body(&get(&st, &format!("/ugc/v1/{id}")))["likeCounter"], 0);
        assert_eq!(body(&send(&st, "GET", &format!("/ugc/v1/{id}/url"), b""))["urls"], *urls);
        assert_eq!(get(&st, "/_powder/ugc/not-an-item").status, 404);
        let r = get(&st, "/ugc/v1/00000000-0000-4000-8000-00000000ffff");
        assert_eq!((r.status, r.body.as_slice()), (404, &br#"{"error": "not found"}"#[..]));
    }

    /// Seen live: the JPEG and a thumbnail come inline (datas, Thumbnail; base64); they are files, never part of
    /// the item.
    #[test]
    fn a_photo_as_the_game_uploads_it() {
        let tmp = tempfile::tempdir().unwrap();
        let st = files_state(tmp.path());
        let created = json!({"typeId": 2, "worldId": WORLD, "metadata": {"worldId": WORLD}, "tags": ["Ski", "Selfie"],
                             "datas": b64(b"\xff\xd8JPEG"), "Thumbnail": b64(b"\xff\xd8SMALL"), "title": "mine"});
        let item = body(&send(&st, "POST", "/ugc/v1", created.to_string().as_bytes()));
        assert!(item.get("datas").is_none() && item.get("Thumbnail").is_none());
        assert_eq!(item["tags"], json!(["Ski", "Selfie"]));
        assert_eq!(item.as_object().unwrap().keys().next().map(String::as_str), Some("title"), "the game's own fields first");
        let urls = &item["links"]["urls"];
        assert_eq!(get(&st, &path_of(&urls["Data"])).body, b"\xff\xd8JPEG");
        assert_eq!(get(&st, &path_of(&urls["Thumbnail"])).body, b"\xff\xd8SMALL");
        let listed = body(&get(&st, &format!("/ugc/v1/?algo=Steep&type_id=2&world_id={WORLD}")));
        assert!(listed[0].get("datas").is_none() && listed[0].get("Thumbnail").is_none());
        let id = item["id"].as_str().unwrap();
        let r = send(&st, "DELETE", &format!("/ugc/v1/{id}"), b"");
        assert_eq!((r.status, r.how.as_str()), (204, "ugc-deleted"));
        assert_eq!(send(&st, "PUT", &format!("/ugc/v1/{id}/hide"), b"").status, 204, "as the game does");
        assert_eq!(get(&st, &path_of(&urls["Data"])).status, 404);
    }

    /// Items from before the pictures were files (inline in the item) get their files on the first download; a
    /// photo without a thumbnail is its own.
    #[test]
    fn inline_pictures_of_older_items_and_the_thumbnail_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let st = files_state(tmp.path());
        let id = "00000000-0000-4000-8000-000000000002";
        let pid = st.accounts.default_profile_id().unwrap();
        let extra = json!({"datas": b64(b"OLD JPEG")});
        st.repos.ugc_create(id, 2, None, &pid, &json!({}), &json!([]), extra.as_object().unwrap()).unwrap();
        assert_eq!(get(&st, &format!("/_powder/ugc/{id}_thumbnail")).body, b"OLD JPEG");
        assert!(tmp.path().join("ugc").join(id).exists());
        assert_eq!(send(&st, "HEAD", &format!("/_powder/ugc/{id}_thumbnail"), b"").status, 404, "only GET falls back");
        // the world from the metadata, a type that is no number: 0
        let created = json!({"typeId": "photo", "metadata": {"worldId": "12"}});
        let item = body(&send(&st, "POST", "/ugc/v1/", created.to_string().as_bytes()));
        assert_eq!((item["typeId"].as_i64(), item["worldId"].as_i64()), (Some(0), Some(12)));
        let item = body(&send(&st, "POST", "/ugc/v1/", b"not json"));
        assert_eq!((item["typeId"].as_i64(), &item["worldId"], &item["metadata"]), (Some(0), &Value::Null, &json!({})));
    }

    #[test]
    fn only_the_owner_hides_or_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let st = files_state(tmp.path());
        let host = st.accounts.register("host@example.com", "correct horse", "").unwrap().profile_id;
        let other = st.accounts.register("guest@example.com", "correct horse", "Guest").unwrap().profile_id;
        let id = "00000000-0000-4000-8000-000000000001";
        st.repos.ugc_create(id, 2, Some(WORLD), &other, &json!({}), &json!([]), &Default::default()).unwrap();
        assert_eq!(send_as(&st, Some(&host), "PUT", &format!("/ugc/v1/{id}/hide"), b"").status, 204);
        assert_eq!(send_as(&st, Some(&host), "DELETE", &format!("/ugc/v1/{id}"), b"").status, 204);
        assert!(!st.repos.ugc_get(id).unwrap().unwrap().hidden, "not ours: untouched");
    }

    /// Once accounts exist: no default player without a ticket, and only the owner uploads an item's files
    /// (everyone downloads them).
    #[test]
    fn only_the_owner_writes_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let st = files_state(tmp.path());
        let host = st.accounts.register("host@example.com", "correct horse", "").unwrap().profile_id;
        let other = st.accounts.register("guest@example.com", "correct horse", "Guest").unwrap().profile_id;
        let created = json!({"typeId": 2, "worldId": WORLD, "datas": b64(b"MINE")});
        let item = body(&send_as(&st, Some(&other), "POST", "/ugc/v1/", created.to_string().as_bytes()));
        assert_eq!(item["profileId"], other.as_str());
        let data = path_of(&item["links"]["urls"]["Data"]);
        let r = send_as(&st, Some(&host), "PUT", &data, b"EVIL");
        assert_eq!((r.status, body(&r)["message"].as_str()), (403, Some("not your item")));
        assert_eq!(send_as(&st, None, "PUT", &data, b"EVIL").status, 401);
        assert_eq!(get(&st, &data).body, b"MINE", "downloads: open");
        assert_eq!(send_as(&st, Some(&other), "PUT", &data, b"NEWER").status, 200);
        assert_eq!(get(&st, &data).body, b"NEWER");
        // no ticket: no stand-in player for creating, listing or liking
        assert_eq!(send(&st, "POST", "/ugc/v1/", created.to_string().as_bytes()).status, 401);
        assert_eq!(get(&st, "/ugc/v1/?type_id=2").status, 401);
        assert_eq!(send(&st, "PUT", &format!("/ugc/v1/{}/like", item["id"].as_str().unwrap()), b"").status, 401);
        assert_eq!(body(&send_as(&st, Some(&host), "GET", "/ugc/v1/?type_id=2", b""))[0]["id"], item["id"]);
    }
}
