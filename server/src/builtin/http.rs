//! HTTPS replies the server builds itself when no game-data pack answers (see builtin/mod.rs): a route table
//! of (method, path pattern, builder), tried in order by https.rs's fallback.

use super::{connect_ui, sdk};
use crate::constants::CONNECT_APP_ID;
use regex::{Captures, Regex};
use serde_json::{json, Value};
use std::sync::LazyLock;

/// Version of the space's item list (GET items/all?fields=lastModified,sha256,md5). The game compares it with
/// the list it ships with: when it matches, the game uses its own copy; otherwise it downloads the list and,
/// with anything else than Ubisoft's list, logs out again right after going online (as with {}). These are the
/// fingerprint of that shipped list (no list content); the full list is never needed.
pub fn items_version() -> Value {
    json!({"lastModified": "2025-03-24T15:53:59.194Z",
           "sha256": "F8F0FBB89CC430475115C1B54243D3B3779C29032C7F00EA670DD4E078C85A2C",
           "md5": "AAA894E0E01921DF01C683B64B10DF87"})
}

/// A parsed query string: (name, value) pairs in order, percent- and '+'-decoded (as aiohttp's request.query).
pub struct Query(pub Vec<(String, String)>);

impl Query {
    pub fn parse(qs: &str) -> Query {
        let pairs = qs
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (unquote_plus(k), unquote_plus(v))
            })
            .collect();
        Query(pairs)
    }

    /// The first value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

fn unquote_plus(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'+', _, _) => out.push(b' '),
            (b'%', Some(hi), Some(lo)) => {
                out.push(hi << 4 | lo);
                i += 2;
            }
            (c, _, _) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

type Build = fn(&Captures, &Query) -> Option<Value>;

const UUID: &str = "[0-9a-fA-F-]{36}";

/// (method, path pattern, builder); a builder's None: not ours after all (the next route may answer).
static ROUTES: LazyLock<Vec<(&'static str, Regex, Build)>> = LazyLock::new(|| {
    let re = |p: String| Regex::new(&format!("^(?:{p})$")).unwrap();
    vec![
        ("GET", re(format!(r"/v\d+/applications/({UUID})/configuration")), |m, _| sdk::configuration(&m[1])),
        ("GET", re(format!(r"/v1/spaces/{UUID}/items/all")), |_, q| q.contains("fields").then(items_version)),
        ("GET", re(r"/v2/spaces/global/ubiconnect/challenges/api/legacy/empty".into()), |_, _| Some(json!([]))),
        // event types the server wants reported: none needed (the game still sends its events)
        ("GET", re(format!(r"/v4/spaces/{UUID}/configs/events")), |_, _| Some(json!({"types": []}))),
        // which items each owned DLC grants: none without a pack ({} makes the game log out)
        ("GET", re(format!(r"/v1/spaces/{UUID}/configs/primarystore")), |_, _| Some(json!({"mappings": []}))),
        ("GET", re(format!(r"/v1/applications/{CONNECT_APP_ID}/parameters")),
         |_, _| Some(connect_ui::client_parameters())),
        ("GET", re(format!(r"/v1/spaces/{UUID}/global/ubiconnect/configcache/api/connecturls")),
         |_, _| Some(connect_ui::connect_urls())),
        ("GET", re(r"/v1/spaces/global/ubiconnect/games/api".into()),
         |_, q| connect_ui::game_cards(q.get("spaceIds").unwrap_or(""))),
        ("GET", re(format!(r"/v1/spaces/({UUID})/global/ubiconnect/games/api")), |m, _| connect_ui::game_card(&m[1])),
        // parameters asked without a cached copy (Connect itself, the overlay): empty groups, as Connect waits on
        // an error; a browser with a cached copy got 304 before this (see keep_cached)
        ("GET", re(format!(r"/v1/spaces/{UUID}/parameters")),
         |_, q| Some(connect_ui::native_parameters(q.get("parameterGroups").unwrap_or("")))),
        // Connect's in-game overlay settings, when the overlay has no cached copy (it never has)
        ("GET", re(r"/v4/spaces/global/ubiconnect/configcache/api/postauth".into()),
         |_, q| connect_ui::postauth(q.get("applications").unwrap_or(""))),
    ]
});

/// Ubisoft Connect's UI settings (which UI parts to load from Ubisoft's CDN, feature flags). Connect's embedded
/// browser caches them, and Ubisoft often answers "304 Not Modified" (the pack has no body for those either).
/// Without a pack: 304 when the browser has a copy, else an error. Never an empty 200: the browser would cache
/// it and the UI then fails on every start (dolphin-032) until Connect is online again.
static CONNECT_UI_SETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/v\d+/spaces/[^/]+/(?:global/)?(?:ubiconnect/configcache/.*|parameters)$").unwrap()
});

pub fn keep_cached(path: &str) -> bool {
    CONNECT_UI_SETTINGS.is_match(path)
}

/// The server's own reply to `method path?query`, if it has one.
pub fn lookup(method: &str, path: &str, query: &Query) -> Option<Value> {
    ROUTES
        .iter()
        .filter(|(want, _, _)| *want == method)
        .find_map(|(_, pattern, build)| build(&pattern.captures(path)?, query))
}

/// The reply's bytes: JSON as Python's json.dumps writes it.
pub fn body(reply: &Value) -> Vec<u8> {
    crate::util::dumps(reply).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn get(path_qs: &str) -> Option<Vec<u8>> {
        let (path, qs) = path_qs.split_once('?').unwrap_or((path_qs, ""));
        lookup("GET", path, &Query::parse(qs)).map(|r| body(&r))
    }

    fn sha(b: &[u8]) -> String {
        hex::encode(Sha256::digest(b))
    }

    /// Lengths and sha256 of the Python server's bodies (builtin/*.py on main) for the same requests.
    #[test]
    fn bodies_match_the_python_server() {
        let cases = [
            ("/v1/applications/b65780e8-0875-4685-995a-6e952cc605a8/configuration", 11557,
             "6acf1308805317fc6a1de0398a9d530d870a55e3ca726a2e9c739e794ecc873a"),
            ("/v1/applications/f68a4bb5-608a-4ff2-8123-be8ef797e0a6/configuration", 1187,
             "d5ccfa24a852fa741cadd00345b7b70b741c1617b2b6d78f17ed2bd05fdc2bf4"),
            ("/v1/spaces/e17be87d-2996-4f3b-97c4-19bb2dae2933/global/ubiconnect/configcache/api/connecturls", 2631,
             "c7258837552542ed517f2571a2ea23f1bf977e98bb596df07d5eb923b7ba72e8"),
            ("/v4/spaces/global/ubiconnect/configcache/api/postauth?applications=a,,b,a", 4150,
             "326f6793bc5d5761f2aa399ccdb64beea3128a5793eb2ee6c689a0bf5bb483d1"),
        ];
        for (path, len, hash) in cases {
            let b = get(path).unwrap();
            assert_eq!((b.len(), sha(&b).as_str()), (len, hash), "{path}");
        }
        let items = get("/v1/spaces/a20f75db-9175-4df6-b1b3-ba42e291166c/items/all?fields=lastModified,sha256,md5");
        assert_eq!(String::from_utf8(items.unwrap()).unwrap(),
                   concat!(r#"{"lastModified": "2025-03-24T15:53:59.194Z", "#,
                           r#""sha256": "F8F0FBB89CC430475115C1B54243D3B3779C29032C7F00EA670DD4E078C85A2C", "#,
                           r#""md5": "AAA894E0E01921DF01C683B64B10DF87"}"#));
        let space = "/v1/spaces/e17be87d-2996-4f3b-97c4-19bb2dae2933/parameters";
        let params = get(&format!("{space}?parameterGroups=foo%2Cconnect-clientConfigs")).unwrap();
        assert!(String::from_utf8(params).unwrap().starts_with(
            r#"{"parameters": {"connect-clientConfigs": {"fields": {"shellUrl": "https://{env}connect.cdn.ubisoft"#));
        let empty = get(&format!("{space}?parameterGroups=foo")).unwrap();
        assert_eq!(empty, br#"{"parameters": {}}"#);
        assert_eq!(get("/v2/spaces/global/ubiconnect/challenges/api/legacy/empty").unwrap(), b"[]");
    }

    #[test]
    fn not_ours() {
        assert!(get("/v1/applications/00000000-0875-4685-995a-6e952cc605a8/configuration").is_none());
        assert!(get("/v1/spaces/a20f75db-9175-4df6-b1b3-ba42e291166c/items/all").is_none()); // the full list
        assert!(get("/v1/spaces/global/ubiconnect/games/api?spaceIds=x").is_none());
        assert!(get("/v4/spaces/global/ubiconnect/configcache/api/postauth?applications=").is_none());
        let legacy = "/v2/spaces/global/ubiconnect/challenges/api/legacy/empty";
        assert!(lookup("POST", legacy, &Query::parse("")).is_none());
        // upper-case app id: still the game's (written lower case)
        let upper = get("/v3/applications/B65780E8-0875-4685-995A-6E952CC605A8/configuration").unwrap();
        assert_eq!(sha(&upper), "6acf1308805317fc6a1de0398a9d530d870a55e3ca726a2e9c739e794ecc873a");
    }

    #[test]
    fn connect_ui_settings_are_kept_cached() {
        assert!(keep_cached("/v1/spaces/x/parameters"));
        assert!(keep_cached("/v2/spaces/global/ubiconnect/configcache/api/anything/else"));
        assert!(keep_cached("/v4/spaces/x/global/ubiconnect/configcache/api/postauth"));
        assert!(!keep_cached("/v1/spaces/x/y/parameters"));
        assert!(!keep_cached("/v1/applications/x/parameters"));
    }

    #[test]
    fn query_strings() {
        let q = Query::parse("a=1&b=x%2Cy+z&a=2&flag&bad=%zz%4");
        assert_eq!((q.get("a"), q.get("b"), q.get("flag"), q.get("bad")),
                   (Some("1"), Some("x,y z"), Some(""), Some("%zz%4")));
        assert!(q.contains("flag") && !q.contains("c"));
    }
}
