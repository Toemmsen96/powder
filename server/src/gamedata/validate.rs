//! What a game-data pack may contain, checked again whenever one is installed, loaded or zipped (validate.py).
//!
//! A pack may come from someone else (a .zip passed around), and its manifest only proves that its files were not
//! changed since whoever made it wrote it. So the builder's allow-lists (build.rs) are applied again here, and
//! what would go to the game or to Ubisoft Connect as it is gets a strict check:
//!
//! - http.json / connect/http.json: only GET replies for the allow-listed paths, on the redirected hostnames, with
//!   a JSON body; besides those Connect's achievement definitions (a zip of '<lang>_loc.txt' texts and '<id>.png'
//!   icons, size-capped) and its free-games list (UTF-8 text). They are served as JSON / zip / plain text whatever
//!   content type the pack names, so a pack cannot serve a page or a script.
//! - rmc.json: only the allow-listed NEX methods, bodies hex.
//! - connect/demux.json: only the ids of Steep's products are used (each must be one of builtin/product_table.rs'):
//!   the product configurations (YAML: executables, working directory, registry paths) Connect gets are the
//!   server's own (builtin/products.rs), never the pack's. The patch info keeps only its numbers and release
//!   name (the patch URL is the server's).
//! - offers.json / actions.json: the shapes the server reads; actions without player state.
//!
//! A pack that fails is refused as a whole (an error "the game-data pack at X is refused: <what>"): the server then
//! runs on its built-in replies, as without a pack.

use super::build::{allowed_path, ACTION_PLAYER_FIELDS, CONNECT_ALLOW, GAME_ALLOW, RMC_ALLOW};
use crate::assets::HttpFixture;
use crate::builtin::product_table::KINDS;
use crate::constants::redirect_hosts;
use crate::demux::connection::PATCH_BASE_URL;
use crate::demux::pb::{self, F, V};
use anyhow::{anyhow, bail, Result};
use regex::bytes::Regex as BytesRegex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::LazyLock;

/// Connect's achievement definitions (the one zip a pack may hold) and its free-games list (text).
static ACHIEVEMENTS_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/orbit/uplay_launcher_3_0/assets/[0-9a-f]+\.zip$").unwrap());
static TEXT_PATHS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/orbit/uplay_launcher_14_0/free_games/latest\.txt$").unwrap());
static ACHIEVEMENT_MEMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[A-Za-z]{2,3}(-[A-Za-z0-9]{2,4})?_loc\.txt|\d{1,6}\.png)$").unwrap());
static RELEASE_NAME: LazyLock<BytesRegex> = LazyLock::new(|| BytesRegex::new(r"^(?-u)[\w.\- ]{0,64}$").unwrap());
pub const MAX_ZIP_MEMBERS: usize = 4096;
/// Uncompressed, all members (Steep's: a few MB of icons).
pub const MAX_ZIP_BYTES: u64 = 64 << 20;
const JSON_TYPES: &[&str] = &["application/json", "text/json", "text/plain"];
const JSON_TYPE: &str = "application/json";

/// The checked contents of a pack, as the server uses them.
#[derive(Default, Debug)]
pub struct ValidPack {
    pub http: Vec<HttpFixture>,
    pub connect_http: Vec<HttpFixture>,
    pub rmc: Map<String, Value>,
    pub offers: Vec<Value>,
    pub actions: Vec<Value>,
    pub patch_info: Option<Vec<u8>>,
    pub product_ids: Vec<u64>,
}

/// The members of a zip (achievement definitions) by name, capped in count and uncompressed size, only names of
/// '<lang>_loc.txt' texts and '<id>.png' icons; an error otherwise. No member is inflated beyond its declared size.
pub fn read_zip(data: &[u8]) -> Result<HashMap<String, Vec<u8>>> {
    read_zip_capped(data, MAX_ZIP_MEMBERS, MAX_ZIP_BYTES)
}

fn read_zip_capped(data: &[u8], max_members: usize, max_bytes: u64) -> Result<HashMap<String, Vec<u8>>> {
    let invalid = |e: &dyn std::fmt::Display| anyhow!("the achievements zip is not a valid zip ({e})");
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| invalid(&e))?;
    let mut infos = vec![]; // (index, name, declared size)
    for i in 0..z.len() {
        let f = z.by_index_raw(i).map_err(|e| invalid(&e))?;
        if !f.is_dir() {
            infos.push((i, f.name().to_string(), f.size()));
        }
    }
    if infos.len() > max_members {
        bail!("the achievements zip has {} files (at most {max_members})", infos.len());
    }
    let total: u64 = infos.iter().map(|i| i.2).sum();
    if total > max_bytes {
        bail!("the achievements zip unpacks to {total} bytes (at most {max_bytes})");
    }
    if let Some((_, odd, _)) = infos.iter().find(|i| !ACHIEVEMENT_MEMBER.is_match(&i.1)) {
        bail!("unexpected file in the achievements zip: {}", cut(odd, 80));
    }
    let mut out = HashMap::new();
    for (i, name, size) in infos {
        let mut f = z.by_index(i).map_err(|e| invalid(&e))?;
        let mut content = vec![];
        (&mut f).take(size + 1).read_to_end(&mut content).map_err(|e| invalid(&e))?;
        if content.len() as u64 != size {
            return Err(invalid(&format!("{name}: not its declared size")));
        }
        out.insert(name, content);
    }
    Ok(out)
}

/// The first `n` characters (Python's `s[:n]`).
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A JSON value as Python's `str()` of it writes it (enough for the messages).
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(other) => other.to_string(),
    }
}

/// ... and as its `repr()`.
fn py_repr(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => format!("'{s}'"),
        other => py_str(other),
    }
}

fn bytes_repr(b: &[u8]) -> String {
    format!("b'{}'", b.escape_ascii())
}

/// Base64 as Python's `b64decode(..., validate=True)` reads it: only the alphabet and padding.
fn strict_b64(text: &str) -> Option<Vec<u8>> {
    use base64::engine::{general_purpose::GeneralPurpose, GeneralPurposeConfig};
    use base64::Engine;
    static ENGINE: LazyLock<GeneralPurpose> = LazyLock::new(|| GeneralPurpose::new(&base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true)));
    ENGINE.decode(text).ok()
}

/// Whether a body is JSON (as Python's `json.loads` of bytes: a UTF-8 BOM is allowed).
fn is_json(body: &[u8]) -> bool {
    let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    serde_json::from_slice::<serde::de::IgnoredAny>(body).is_ok()
}

fn media_type(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_lowercase()
}

fn http(raw: &Value, rules: &[(&str, Regex)], name: &str, connect: bool) -> Result<Vec<HttpFixture>> {
    let Some(raw) = raw.as_array() else { bail!("{name}: not a list of replies") };
    let hosts = redirect_hosts();
    let mut out = vec![];
    for (n, e) in raw.iter().enumerate() {
        let place = format!("{name} entry {n}");
        let Some(e) = e.as_object() else { bail!("{place}: not an object") };
        let (method, path, status) = (e.get("method"), e.get("path"), e.get("status"));
        let host = e.get("host").map_or_else(String::new, |h| py_str(Some(h))).to_lowercase();
        let allowed = match (method.and_then(Value::as_str), path.and_then(Value::as_str)) {
            (Some(m), Some(p)) => allowed_path(rules, m, p),
            _ => false,
        };
        let Some(path) = path.and_then(Value::as_str).filter(|_| allowed) else {
            bail!("{place}: {} {} is not one of the replies a pack may hold", py_str(method), cut(&py_str(path), 80));
        };
        if !hosts.contains(&host.as_str()) {
            bail!("{place}: host {} is not one this server answers for", cut(&host, 60));
        }
        let Some(code) = status.and_then(Value::as_u64).filter(|s| (200..=599).contains(s)) else {
            bail!("{place}: status {}", py_repr(status));
        };
        let body = match e.get("body_b64") {
            None => Some(vec![]),
            Some(Value::String(s)) => strict_b64(s),
            Some(_) => None,
        };
        let Some(body) = body else { bail!("{place}: body is not base64") };
        let base = path.split('?').next().unwrap_or("");
        let content_type = if connect && ACHIEVEMENTS_PATH.is_match(base) {
            read_zip(&body)?;
            "application/zip".to_string()
        } else if connect && TEXT_PATHS.is_match(base) {
            if std::str::from_utf8(&body).is_err() {
                bail!("{place}: {base} is not text");
            }
            "text/plain; charset=utf-8".to_string()
        } else {
            if !is_json(&body) {
                bail!("{place}: {} does not reply JSON", cut(base, 80));
            }
            match e.get("content_type").and_then(Value::as_str) {
                Some(recorded) if JSON_TYPES.contains(&media_type(recorded).as_str()) => recorded.to_string(),
                _ => JSON_TYPE.to_string(),
            }
        };
        out.push(HttpFixture { method: "GET".into(), host, path: path.to_string(), status: code as u16, content_type,
                               body });
    }
    Ok(out)
}

fn rmc(raw: Value) -> Result<Map<String, Value>> {
    let Value::Object(raw) = raw else { bail!("rmc.json: not an object") };
    for (method, rows) in &raw {
        if !RMC_ALLOW.contains(&method.as_str()) {
            bail!("rmc.json: {} is not one of the NEX replies a pack may hold", cut(method, 80));
        }
        let rows = rows.as_array().filter(|rows| rows.iter().all(|r| {
            r.get("params_sha1").is_some_and(Value::is_string) && r.get("body").is_some_and(Value::is_string)
        }));
        let Some(rows) = rows else { bail!("rmc.json: {method}: not a list of recorded replies") };
        for r in rows {
            // bytes.fromhex: whitespace between the digits is allowed
            let digits: String = r["body"].as_str().unwrap_or("").chars().filter(|c| !c.is_ascii_whitespace()).collect();
            if hex::decode(digits).is_err() {
                bail!("rmc.json: {method}: a body is not hex");
            }
        }
    }
    Ok(raw)
}

fn falsy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

/// Connect's patch info {1 ok, 2 release name, 3, 4 patch base URL, 5 latest version, 6, 7}, rebuilt from the
/// pack's: its numbers and a plain release name; the patch URL is always the server's own (Connect never patches
/// from it, the latest version is always its own, but a pack must not name where it would).
fn patch_info(b64: Option<&Value>) -> Result<Option<Vec<u8>>> {
    if falsy(b64) {
        return Ok(None);
    }
    let fields = b64.and_then(Value::as_str).and_then(strict_b64).and_then(|b| pb::decode(&b).ok())
        .ok_or_else(|| anyhow!("connect/demux.json: patch_info is not a patch info message"))?;
    let mut out = vec![];
    for F(number, value) in fields {
        match (number, value) {
            (1 | 3 | 5 | 6 | 7, V::Int(v)) => out.push(F(number, V::Int(v))),
            (2, value) => {
                let raw = match &value {
                    V::Bytes(b) => Some(b.as_slice()),
                    V::Fixed32(b) => Some(&b[..]),
                    V::Fixed64(b) => Some(&b[..]),
                    _ => None,
                };
                out.push(match raw.filter(|r| RELEASE_NAME.is_match(r)) {
                    Some(r) => F::bytes(2, r),
                    None => F::str(2, "0"),
                });
            }
            (4, _) => out.push(F::str(4, PATCH_BASE_URL)),
            _ => {}
        }
    }
    Ok(Some(pb::encode(&out)))
}

fn product_ids(raw: &[Value]) -> Result<Vec<u64>> {
    let mut ids = vec![];
    for (n, b64) in raw.iter().enumerate() {
        let Some(fields) = b64.as_str().and_then(strict_b64).and_then(|b| pb::decode(&b).ok()) else {
            bail!("connect/demux.json: product {n} is not a product entry");
        };
        match pb::first(&fields, 1) {
            Some(V::Int(pid)) if KINDS.iter().any(|(id, _)| id == pid) => ids.push(*pid),
            other => {
                let shown = match other {
                    None => "None".to_string(),
                    Some(V::Int(i)) => i.to_string(),
                    Some(V::Bytes(b)) => bytes_repr(b),
                    Some(V::Fixed32(b)) => bytes_repr(b),
                    Some(V::Fixed64(b)) => bytes_repr(b),
                    Some(V::Msg(m)) => bytes_repr(&pb::encode(m)),
                };
                bail!("connect/demux.json: product {shown} is not one of Steep's");
            }
        }
    }
    Ok(ids)
}

fn load(root: &Path, name: &str, default: Value) -> Result<Value> {
    let path = root.join(name);
    if !path.exists() {
        return Ok(default);
    }
    if path.is_symlink() || !path.is_file() {
        bail!("{name} is not a plain file");
    }
    let raw = std::fs::read(&path).map_err(|e| anyhow!("{name}: {e}"))?;
    std::str::from_utf8(&raw).ok().and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| anyhow!("{name} is not JSON"))
}

/// The pack at `root`, checked (see the module doc); an error says what is wrong.
pub fn validated(root: &Path) -> Result<ValidPack> {
    check_all(root).map_err(|e| anyhow!("the game-data pack at {} is refused: {e}", root.display()))
}

fn check_all(root: &Path) -> Result<ValidPack> {
    let demux = load(root, "connect/demux.json", json!({}))?;
    let offers = load(root, "offers.json", json!({"offers": []}))?;
    let actions = load(root, "actions.json", json!({"actions": []}))?;
    let (Some(demux), Some(offers), Some(actions)) = (demux.as_object(), offers.as_object(), actions.as_object())
    else {
        bail!("connect/demux.json, offers.json and actions.json must be objects");
    };
    let offer_list = offers.get("offers").cloned().unwrap_or_else(|| json!([]));
    let action_list = actions.get("actions").cloned().unwrap_or_else(|| json!([]));
    let Some(offer_list) = offer_list.as_array().filter(|offers| offers.iter().all(|o| {
        o.as_object().is_some_and(|o| o.get("offerId").is_some_and(Value::is_string))
    })) else {
        bail!("offers.json: not a list of offers");
    };
    let Some(action_list) = action_list.as_array().filter(|actions| actions.iter().all(Value::is_object)) else {
        bail!("actions.json: not a list of actions");
    };
    if action_list.iter().any(|a| ACTION_PLAYER_FIELDS.iter().any(|k| a.get(k).is_some())) {
        bail!("actions.json: holds player state (profileId / value)");
    }
    let products = demux.get("products").cloned().unwrap_or_else(|| json!([]));
    let Some(products) = products.as_array() else { bail!("connect/demux.json: products is not a list") };
    Ok(ValidPack {
        http: http(&load(root, "http.json", json!([]))?, &GAME_ALLOW, "http.json", false)?,
        connect_http: http(&load(root, "connect/http.json", json!([]))?, &CONNECT_ALLOW, "connect/http.json", true)?,
        rmc: rmc(load(root, "rmc.json", json!({}))?)?,
        offers: offer_list.clone(),
        actions: action_list.clone(),
        patch_info: patch_info(demux.get("patch_info"))?,
        product_ids: product_ids(products)?,
    })
}

#[cfg(test)]
mod tests {
    //! tests/test_gamedata.py on main: a pack from someone else checked again on install, load and zip, zip bombs,
    //! the privacy check's gaps (short names, product field 21, zip / gzip bodies, zip_pack). Made-up packs only.
    use super::*;
    use crate::assets::Assets;
    use crate::constants::{APP_ID, SPACE_ID};
    use crate::gamedata::build::synthetic as syn;
    use crate::gamedata::{build, pack, privacy};
    use std::collections::HashSet;
    use std::io::Write;
    use std::path::PathBuf;

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &to.join(e.file_name()));
            } else {
                std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }

    /// A built pack in `dir/built`, copied to `dir/copy` (to be tampered with).
    fn pack_copy(dir: &Path) -> PathBuf {
        let built = syn::pack(&dir.join("built"));
        copy_dir(&built, &dir.join("copy"));
        dir.join("copy")
    }

    /// Change one file of the pack and write its manifest again, as someone making a pack would.
    fn edit(root: &Path, name: &str, change: impl FnOnce(Value) -> Value) {
        let path = root.join(name);
        let data = change(serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap());
        std::fs::write(&path, serde_json::to_string(&data).unwrap()).unwrap();
        let mut info = pack::read_manifest(root).unwrap();
        info.retain(|k, _| k != "format" && k != "files");
        pack::write_manifest(root, info).unwrap();
    }

    fn zip_folder(root: &Path, out: &Path) -> PathBuf {
        fn add(z: &mut zip::ZipWriter<std::fs::File>, root: &Path, dir: &Path) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    add(z, root, &p);
                } else {
                    let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    z.start_file(format!("gamedata/{rel}"), zip::write::SimpleFileOptions::default()).unwrap();
                    z.write_all(&std::fs::read(&p).unwrap()).unwrap();
                }
            }
        }
        let mut z = zip::ZipWriter::new(std::fs::File::create(out).unwrap());
        add(&mut z, root, root);
        z.finish().unwrap();
        out.to_path_buf()
    }

    fn push(entry: Value) -> impl FnOnce(Value) -> Value {
        move |mut d| {
            d.as_array_mut().unwrap().push(entry);
            d
        }
    }

    fn yaml(entry: &[u8]) -> String {
        String::from_utf8(pb::bytes(&pb::decode(entry).unwrap(), 17).unwrap_or_default().to_vec()).unwrap()
    }

    #[test]
    fn a_built_pack_passes_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let built = syn::pack(dir.path());
        assert_eq!(pack::check(&built).unwrap()["format"], json!(pack::PACK_FORMAT));
        assert_eq!(privacy::self_check(&built), Vec::<String>::new());
        let out = dir.path().join("pack.zip");
        pack::zip_pack(&built, &out).unwrap();
        let m = pack::install(&out, &dir.path().join("installed")).unwrap();
        assert_eq!(m["files"], pack::read_manifest(&built).unwrap()["files"]);
        let assets = Assets::load(&dir.path().join("installed"));
        assert!(!assets.is_empty());
        assert_eq!(assets.http.entries.len(), 1);
        assert_eq!(assets.connect_http.entries.len(), 2);
        assert_eq!(assets.catalog.len(), 1);
        assert_eq!(assets.rmc.data.len(), 1);
        assert_eq!(assets.actions, vec![json!({"spaceId": SPACE_ID, "id": "1000", "name": "The legend starts here",
                                                "xp": 20, "description": "Play", "isBadge": false})]);
    }

    /// The pack's product YAML (executables, working directory, registry paths) never reaches Connect: only its
    /// product ids are used, the configurations are builtin/products.rs'.
    #[test]
    fn the_product_configuration_is_the_servers_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = pack_copy(dir.path());
        let evil = pb::encode(&[F::int(1, syn::STEEP), F::str(17, "version: 2.0\r\nroot:\r\n  start_game:\r\n    \
            online:\r\n      executables:\r\n      - path:\r\n          relative: ..\\\\evil.exe\r\n")]);
        edit(&root, "connect/demux.json", |mut d| {
            d["products"] = json!([crate::util::b64(&evil)]);
            d
        });
        let assets = Assets::load(&root);
        assert!(!assets.is_empty());
        let [entry] = &assets.products.products[..] else { panic!("one product") };
        assert!(!yaml(entry).contains("evil.exe") && yaml(entry).contains("steam_app_id: 460920")); // the Steam entry
        let only = [syn::STEEP].into();
        assert_eq!(*entry, crate::builtin::products::entries_of(true, assets.achievements_file().as_deref(),
                                                                 Some(&only))[0]);
        assert_eq!(assets.achievements_file().as_deref(), Some("0123abcd.zip"));
        assert!(yaml(entry).contains("achievements: 0123abcd.zip"));
    }

    #[test]
    fn a_pack_outside_the_allow_lists_is_refused() {
        type Change = Box<dyn FnOnce(Value) -> Value>;
        let cases: Vec<(&str, Change, &str)> = vec![
            ("connect/demux.json", Box::new(|mut d: Value| {
                d["products"] = json!([crate::util::b64(&pb::encode(&[F::int(1, 1234)]))]);
                d
            }), "product 1234 is not one of Steep's"),
            ("http.json", Box::new(push(syn::entry(syn::UBI, &format!("/v2/applications/{APP_ID}/configuration?x=1"),
                                                   b"<script>alert(1)</script>", "text/html"))), "does not reply JSON"),
            ("connect/http.json", Box::new(push(syn::entry("static3.cdn.ubi.com", "/orbit/app.js", b"{}",
                                                           "application/json"))),
             "is not one of the replies a pack may hold"),
            ("http.json", Box::new(push(syn::entry("account.ubisoft.com", &format!("/v1/spaces/{SPACE_ID}/items/all"),
                                                   b"{}", "application/json"))), "is not one this server answers for"),
            ("http.json", Box::new(push({
                let mut e = syn::entry(syn::UBI, &format!("/v1/spaces/{SPACE_ID}/items/all"), b"{}", "application/json");
                e["method"] = json!("POST");
                e
            })), "is not one of the replies a pack may hold"),
            ("rmc.json", Box::new(|mut d: Value| {
                d["AccountManagementProtocol::GetName_V1"] = json!([{"params_sha1": "0", "body": ""}]);
                d
            }), "is not one of the NEX replies"),
            ("actions.json", Box::new(|_| json!({"actions": [{"id": "1", "profileId": syn::PLAYER}]})), "player state"),
        ];
        for (n, (name, change, why)) in cases.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let root = pack_copy(dir.path());
            edit(&root, name, change);
            let err = pack::check(&root).unwrap_err().to_string();
            assert!(err.contains(why) && err.contains("is refused: "), "case {n}: {err}");
            let installed = dir.path().join("installed");
            let err = pack::install(&zip_folder(&root, &dir.path().join("p.zip")), &installed).unwrap_err();
            assert!(err.to_string().contains("refused"), "case {n}: {err}");
            assert!(!installed.exists(), "case {n}");
            assert!(Assets::load(&root).is_empty(), "case {n}: the server runs on its built-in replies");
        }
    }

    #[test]
    fn json_is_served_as_json_and_the_patch_url_is_the_servers() {
        let dir = tempfile::tempdir().unwrap();
        let root = pack_copy(dir.path());
        let path = format!("/v1/spaces/{SPACE_ID}/items/all");
        edit(&root, "http.json", push(syn::entry(syn::UBI, &path, b"{\"items\": []}", "text/html")));
        let patch = pb::encode(&[F::int(1, 1), F::str(2, "release"), F::str(4, "https://evil.example/patches/"),
                                 F::int(5, 13368), F::str(9, "x")]);
        edit(&root, "connect/demux.json", |mut d| {
            d["patch_info"] = json!(crate::util::b64(&patch));
            d
        });
        let assets = Assets::load(&root);
        let (fixture, _) = assets.http.lookup("GET", syn::UBI, &path).unwrap();
        assert_eq!(fixture.content_type, "application/json");
        assert_eq!(pb::decode(assets.products.patch_info.as_ref().unwrap()).unwrap(),
                   vec![F::int(1, 1), F::str(2, "release"), F::str(4, PATCH_BASE_URL), F::int(5, 13368)]);
        // a release name that is not plain is "0"; the achievements zip and free-games text keep their own types
        let odd = pb::encode(&[F::str(2, "<b>x</b>"), F::str(1, "not a number")]);
        assert_eq!(patch_info(Some(&json!(crate::util::b64(&odd)))).unwrap().unwrap(), pb::encode(&[F::str(2, "0")]));
        let types: Vec<&str> = assets.connect_http.entries.iter().map(|e| e.content_type.as_str()).collect();
        assert_eq!(types, ["application/json", "application/zip"]);
        let text = syn::entry("static3.cdn.ubi.com", "/orbit/uplay_launcher_14_0/free_games/latest.txt", b"x", "x/y");
        assert_eq!(http(&json!([text]), &CONNECT_ALLOW, "c", true).unwrap()[0].content_type, "text/plain; charset=utf-8");
    }

    #[test]
    fn zip_bombs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed");
        let many = dir.path().join("many.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&many).unwrap());
        for i in 0..=pack::MAX_PACK_MEMBERS {
            z.start_file(format!("gamedata/f{i}"), zip::write::SimpleFileOptions::default()).unwrap();
        }
        z.finish().unwrap();
        let err = pack::install(&many, &installed).unwrap_err().to_string();
        assert!(err.contains("65 files") && err.contains("at most 64 files"), "{err}");
        let big = dir.path().join("big.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&big).unwrap());
        z.start_file("gamedata/http.json", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(&vec![0u8; 1 << 20]).unwrap();
        z.finish().unwrap();
        let err = pack::unzip_capped(&big, &dir.path().join("staging"), 64, 1 << 19).unwrap_err().to_string();
        assert!(err.contains("1048576 bytes unpacked"), "{err}");
        // the achievements zip inside the pack: capped too
        let one = |name: &str, data: &[u8]| {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
            z.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
            z.finish().unwrap().into_inner()
        };
        let err = read_zip_capped(&one("1.png", &vec![0u8; 1 << 20]), 4096, 1 << 19).unwrap_err().to_string();
        assert!(err.contains("unpacks to 1048576 bytes"), "{err}");
        let err = read_zip_capped(&one("1.png", b"x"), 0, 1 << 19).unwrap_err().to_string();
        assert!(err.contains("has 1 files (at most 0)"), "{err}");
        let err = read_zip(&one("../evil.dll", b"MZ")).unwrap_err().to_string();
        assert_eq!(err, "unexpected file in the achievements zip: ../evil.dll");
    }

    #[test]
    fn the_achievements_zip_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut assets = Assets::load(&syn::pack(dir.path()));
        assert_eq!(assets.achievement_icon(1).as_deref(), Some(syn::ICON));
        // read once: the cached files stay when the recorded zip is gone
        assets.connect_http = crate::assets::HttpFixtures::default();
        assert_eq!(assets.achievement_icon(4).as_deref(), Some(syn::ICON));
        let (id, name, description) = syn::ACHIEVEMENT_NAMES[1];
        assert_eq!(assets.achievement_names("en-US")[1], (id, name.to_string(), description.to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_with_links_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = pack_copy(dir.path());
        std::os::unix::fs::symlink(dir.path(), root.join("extra")).unwrap();
        let err = pack::install(&root, &dir.path().join("installed")).unwrap_err().to_string();
        assert_eq!(err, "this game-data pack contains symbolic links; not installed");
    }

    #[test]
    fn short_names_are_found_as_words() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("http.json"), r#"{"text": "Bob rides; Bobsled and SKI"}"#).unwrap();
        let name = |s: &str| privacy::Marker::Name(s.into());
        let markers = [name("Bob"), name("Ski"), name("led"), privacy::Marker::Value("ab12".into())].into();
        assert_eq!(privacy::check(dir.path(), &markers, &HashSet::new()), ["http.json: Bob..."]);
    }

    #[test]
    fn inner_zip_and_gzip_bodies_are_searched() {
        let dir = tempfile::tempdir().unwrap();
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("en-US_loc.txt", opts).unwrap();
        z.write_all(format!("1\t{}\tx\n", syn::FRIEND_NAME).repeat(20).as_bytes()).unwrap();
        let zipped = z.finish().unwrap().into_inner();
        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
        gz.write_all(syn::PLAYER_EMAIL.as_bytes()).unwrap();
        let entries = json!([{"body_b64": crate::util::b64(&zipped)},
                             {"body_b64": crate::util::b64(&gz.finish().unwrap())}]);
        std::fs::write(dir.path().join("http.json"), entries.to_string()).unwrap();
        let markers = [privacy::Marker::Value(syn::FRIEND_NAME.into()),
                       privacy::Marker::Value(syn::PLAYER_EMAIL.into())];
        let found = privacy::check(dir.path(), &markers.into(), &HashSet::new());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.contains("decoded, ")), "{found:?}");
    }

    /// A 40-hex field 21 other than Steep's known one could be per account: the builder writes the known one, the
    /// check (also before zipping) reports any other.
    #[test]
    fn product_field_21_must_be_steeps() {
        let Some(V::Bytes(known)) = privacy::known_product_hash(Some(3279)) else { panic!("3279 has a known hash") };
        assert!(privacy::HEX40.is_match(&known));
        let dir = tempfile::tempdir().unwrap();
        let mine = "A".repeat(40);
        let products = syn::products(|| vec![F::str(21, &mine)]);
        let recordings = syn::recordings(&dir.path().join("recordings"), &products);
        let out = dir.path().join("gamedata");
        assert!(build::build(&recordings, &out).unwrap().findings.is_empty());
        let demux: Value = serde_json::from_slice(&std::fs::read(out.join("connect/demux.json")).unwrap()).unwrap();
        let written: Vec<(u64, Option<Vec<u8>>)> = demux["products"].as_array().unwrap().iter().map(|p| {
            let fields = pb::decode(&crate::util::unb64(p.as_str().unwrap()).unwrap()).unwrap();
            (pb::int(&fields, 1).unwrap(), pb::bytes(&fields, 21).map(<[u8]>::to_vec))
        }).collect();
        assert_eq!(written, [(3279, Some(known)), (3280, None), (4001, None)]);
        assert!(privacy::product_hash_findings(&out).is_empty());
        edit(&out, "connect/demux.json", |mut d| {
            d["products"] = json!([crate::util::b64(&products[0])]);
            d
        });
        let findings = privacy::product_hash_findings(&out);
        assert_eq!(findings, ["connect/demux.json: product 3279 field 21 AAAAAAAA... (not Steep's known value: maybe \
                               per account)"]);
        let err = pack::zip_pack(&out, &dir.path().join("p.zip")).unwrap_err().to_string();
        assert!(err.contains("personal data"), "{err}");
    }

    #[test]
    fn zip_pack_refuses_a_pack_with_account_data() {
        let dir = tempfile::tempdir().unwrap();
        let root = pack_copy(dir.path());
        edit(&root, "offers.json", |mut d| {
            d["offers"].as_array_mut().unwrap().push(json!({"offerId": "x", "note": syn::PLAYER_EMAIL}));
            d
        });
        pack::check(&root).unwrap(); // installable (someone else's data is not a danger to us)
        let err = pack::zip_pack(&root, &dir.path().join("p.zip")).unwrap_err().to_string();
        assert!(err.contains("looks like it holds personal data, not zipped: offers.json: e-mail address pla..."),
                "{err}");
        assert!(!dir.path().join("p.zip").exists());
        // identifying keys too, but not the game's own ids
        edit(&root, "offers.json", |_| json!({"offers": [{"offerId": "x", "profileId": SPACE_ID}]}));
        assert_eq!(privacy::self_check(&root), Vec::<String>::new());
        edit(&root, "offers.json", |_| json!({"offers": [{"offerId": "x", "profileId": syn::PLAYER}]}));
        assert_eq!(privacy::self_check(&root), ["offers.json: profileId=00000000-000..."]);
    }
}
