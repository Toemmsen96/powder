//! Player-related UbiServices endpoints answered from the player record (no recordings), from the Python
//! server's routes_profile.py: the game's and Ubisoft Connect's profile, social, news and telemetry calls. Most
//! only need a well-formed answer: empty lists for news and other people's content; neutral values for account
//! details. Friends: every other player of the server (services/friends.rs). Connect's achievement pages
//! (GraphQL) come from the database and the pack; its chat channels from services/channels.rs. "me" is the
//! session's player; without a session the default player, until accounts are required (then a 401; routes_ubi.rs,
//! "who may act").

use super::routes_ubi::{acting, reader};
use super::{Ctx, Reply, Route};
use crate::builtin::http::Query;
use crate::constants::{APP_ID, SPACE_ID};
use crate::repos::truthy;
use crate::services::friends;
use crate::util::{dumps, iso_in, iso_now};
use anyhow::{bail, Result};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::LazyLock;

/// In routes_profile.py's registration order.
pub fn routes() -> Vec<Route> {
    let pid = "{profile_id}";
    let p = |tail: &str| tail.replace("{pid}", pid);
    let mut r = vec![
        // game
        Route::new("GET", "/v1/profiles/me/populations", |c| done(populations(c))),
        Route::new("PUT", "/v1/profiles/me/populations/data", |c| done(populations(c))),
        Route::new("GET", &p("/v1/profiles/{pid}/actions"), |c| done(actions(c))),
        Route::new("GET", &p("/v1/profiles/{pid}/rewards"), |_| ok(json!({"rewards": []}), "rewards")),
        Route::new("GET", "/v2/profiles/me/friends", |c| done(game_friends(c))),
        Route::new("GET", "/v3/profiles", |c| done(profiles_lookup(c))),
        Route::new("GET", "/v1/profiles/me/news", |_| ok(json!({"news": []}), "news")),
        Route::new("GET", "/v1/profiles/connections", |c| done(profile_connections(c))),
        Route::new("POST", "/v1/profiles/me/remotelog", remotelog),
        Route::new("POST", "/v3/profiles/me/events", |_| ok(json!({}), "events")),
        Route::new("POST", "/v5/spaces/{space}/serviceEvents", |_| ok(json!({}), "service-events")),
        Route::new("GET", "/v2/spaces/{space}/entities", space_entities),
        Route::new("*", "/v2/connections/{tail:.*}", |_| ok(json!({}), "notifications")),
        Route::new("POST", &p("/v2/profiles/{pid}/notifications"), |c| done(send_notification(c))),
        // Ubisoft Connect
        Route::new("GET", "/v3/users/me", |c| done(users_me(c))),
        Route::new("GET", "/v3/users/me/profiles", |c| done(users_me_profiles(c))),
        Route::new("GET", "/v3/users/{user_id}", user_info), // after /v3/users/me
        Route::new("GET", "/v3/users/{user_id}/policies", |_| ok(json!({
            "isAcceptanceMandatory": false, "displayValues": {"reacceptanceReason": null},
            "hasAcceptedLegalOptins": true}), "policies")),
        Route::new("GET", "/v1/users/{user_id}/subscriptions", |_| ok(json!({"subscriptions": []}), "subscriptions")),
        Route::new("GET", "/v1/users/onlineStatuses", |c| done(online_statuses(c))),
        Route::new("GET", "/v2/profiles/me/iplocation", iplocation),
        Route::new("GET", &p("/v3/profiles/{pid}/friends"), |c| done(connect_friends(c))),
        Route::new("GET", &p("/v3/profiles/{pid}/global/harboursocial/friends/core/enriched"),
                   |c| done(connect_friends(c))),
        Route::new("GET", &p("/v1/profiles/{pid}/blocks"),
                   |_| ok(json!({"groupId": uuid::Uuid::new_v4().to_string(), "profiles": []}), "blocks")),
        Route::new("GET", &p("/v1/profiles/{pid}/connections"), |_| ok(json!({"connections": []}), "connections")),
        Route::new("GET", &p("/v1/profiles/{pid}/avatars"),
                   |_| ok(json!({"type": "default", "imageVariations": []}), "avatars")),
        Route::new("GET", "/v1/profiles/me/gamesplayed", |_| ok(json!({"gamesPlayed": []}), "games-played")),
        Route::new("POST", "/v1/profiles/me/uplay/graphql", |c| done(graphql(c))),
        Route::new("GET", "/_powder/achievements/{ach_id}.png", achievement_icon),
        Route::new("GET", &p("/v1/profiles/{pid}/gamerecommendations"),
                   |_| ok(json!({"gameRecommendations": []}), "recommendations")),
        Route::new("GET", &p("/v1/profiles/{pid}/statsCard"), |_| ok(json!({"Statscards": []}), "stats-card")),
        Route::new("GET", &p("/v2/profiles/{pid}/news"), |_| ok(json!({
            "pagination": {"total": 0, "count": 0, "offset": 0, "limit": 20}, "news": []}), "news")),
        Route::new("GET", "/v1/profiles/me/channels", |c| done(channels_mine(c))),
        Route::new("POST", "/v1/spaces/{space}/channels", |c| done(channel_create(c))),
        Route::new("*", "/v1/spaces/{space}/channels/{channel}/messages", |c| done(channel_messages(c))),
        Route::new("GET", "/v1/profiles/me/global/ubiconnect/entitlement/api/entitlements",
                   |_| entitlements()),
        Route::new("GET", &p("/v1/profiles/{pid}/richpresences/subscriptions"), |_| ok(json!({}), "richpresence")),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/challenges/api"),
                   |_| ok(json!({"challenges": []}), "connect-challenges")),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/rewards/api"),
                   |_| ok(json!({"rewards": []}), "connect-rewards")),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/economy/api/units"),
                   |c| for_player(c, "units", |pid| json!({"profileId": pid, "units": 0}))),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/economy/api/metaprogression"),
                   |c| for_player(c, "metaprogression", |pid| json!({"profileId": pid, "level": 1, "xp": 0}))),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/economy/api/ubiplussubscriptions"),
                   |_| ok(json!({"ubiPlusSubscriptions": []}), "ubiplus")),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/economy/api/ubiplusprogressions"),
                   |c| for_player(c, "ubiplus", |pid| json!({"profileId": pid, "level": "0", "levelUpAt": null,
                                                             "subscriptionDurationInMonths": 0}))),
        Route::new("GET", &p("/v1/profiles/{pid}/global/ubiconnect/contentGating/api/eligibility"),
                   |c| for_player(c, "eligibility", |pid| json!({"profileId": pid, "isEligible": true}))),
        Route::new("GET", &p("/v2/profiles/{pid}/club/progression/title"), |c| done(club_title(c))),
        Route::new("*", &p("/v1/profiles/{pid}/global/ubiconnect/playsession/api/sessions"), playsession),
        Route::new("POST", &p("/v1/profiles/{pid}/playerAchievements/registrations/steam"),
                   |c| for_player(c, "achievements-registration",
                                  |pid| json!({"spaceId": SPACE_ID, "profileId": pid, "status": "notapplicable"}))),
    ];
    for path in ["/v2/applications/{app}/global/sre/opentelemetry/api/{kind}",
                 "/v1/applications/{app}/global/sre/opentelemetry/anon/{kind}",
                 "/v1/spaces/{space}/global/sre/opentelemetry/api/{kind}"] {
        r.push(Route::new("POST", path, |_| ok(json!({"partialSuccess": {}}), "telemetry")));
    }
    r
}

/// Owned downloadable content, so the game does not show DLC worlds (Japan, Alaska, ...) as "to buy".
/// Best-effort: Powder grants every DLC/ULC product it knows (product_table). The exact field set Steep reads
/// is from its live service and not recorded here; this is a plausible shape and is tuned against the game.
fn entitlements() -> Reply {
    let list: Vec<Value> = crate::builtin::product_table::KINDS.iter()
        .filter(|(_, kind)| *kind == "dlc" || *kind == "ulc")
        .map(|(id, kind)| json!({
            "entitlementId": format!("{SPACE_ID}:{id}"),
            "productId": id,
            "spaceId": SPACE_ID,
            "type": if *kind == "dlc" { "DOWNLOADABLE_CONTENT" } else { "UNLOCKABLE_CONTENT" },
            "obtentionDate": "2016-12-02T00:00:00.000Z",
            "active": true,
        }))
        .collect();
    ok(json!({"entitlements": list}), "entitlements")
}

fn ok(data: Value, how: &str) -> Reply {
    Reply::json(&data, how, 200)
}

fn done(r: Result<Reply>) -> Reply {
    r.unwrap_or_else(|e| match e.downcast::<Refused>() {
        Ok(Refused(reply)) => reply,
        Err(e) => Reply::internal_error(&e),
    })
}

/// A request refused before it was answered (no session where accounts are required); `done` sends its reply.
struct Refused(Reply);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused ({} {})", self.0.status, self.0.how)
    }
}

impl std::fmt::Debug for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for Refused {}

/// The requesting player's profile id: the ticket's; without one the default player while accounts are not
/// required, else refused (routes_ubi.rs' `acting`).
fn me(c: &Ctx) -> Result<String> {
    acting(c).map(|a| a.pid).map_err(|r| Refused(r).into())
}

/// Reads of player data: refused without a session once accounts are required (routes_ubi.rs' `reader`).
fn signed_in(c: &Ctx) -> Result<()> {
    reader(c).map(|_| ()).map_err(|r| Refused(r).into())
}

/// The {profile_id} in the path ("me" = the requester).
fn pid(c: &Ctx) -> Result<String> {
    match c.params.get("profile_id").map(String::as_str) {
        None | Some("me") => me(c),
        Some(p) => Ok(p.to_lowercase()),
    }
}

/// Answers build(profile id) for the {profile_id} in the path.
fn for_player(c: &Ctx, how: &str, build: fn(&str) -> Value) -> Reply {
    done(pid(c).map(|p| ok(build(&p), how)))
}

fn query(c: &Ctx) -> Query {
    Query::parse(c.req.path_qs.split_once('?').map(|(_, q)| q).unwrap_or(""))
}

/// The request body as JSON (None: none or not JSON).
fn json_body(c: &Ctx) -> Option<Value> {
    serde_json::from_slice(&c.req.body).ok()
}

/// The body if it is a JSON object, else {}.
fn object_body(c: &Ctx) -> Value {
    json_body(c).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

/// json.dumps(body)[:2000], for the log.
fn logged(body: &Value) -> String {
    dumps(body).chars().take(2000).collect()
}

/// Game-wide entities: none. Ubisoft's featured trails (TITLE_CHALLENGE "Specials" with rewards, asked for with
/// tags=TRAIL,ACTIVATED,OUTOFCLUSTER; the last ones ended in 2019) are not served; the players' own trails are
/// profile entities (FAVORITE_CHALLENGE / FAVORITE_REPLAY). The world's matchmaking map (TITLE_MATCHMAKING_MAP)
/// stays empty on purpose: with no entity the game matchmakes without a map (MatchmakingCellsService goes on),
/// while an entity whose map file it can't download switches its matchmaking off for
/// RETRY_MATCHMAKING_MAP_DOWNLOAD_SECONDS (30 min). The map files are not ours.
fn space_entities(_: &Ctx) -> Reply {
    ok(json!({"entities": []}), "space-entities")
}

// ---- Connect's GraphQL ----------------------------------------------------------------------------------------
/// The product key Connect uses for Steep's achievements.
const STEEP_ACHIEVEMENTS: &str = "STP:PC";

/// Each operation's full query text is logged once (by operation name).
static LOGGED_QUERIES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Ubisoft Connect's UI queries (library, game pages, achievements) go to one GraphQL endpoint, as a batch: one
/// result per operation, in order. Achievement queries are answered from the database and the pack; the others
/// with no games. Each operation's full query is logged once.
fn graphql(c: &Ctx) -> Result<Reply> {
    let batch = json_body(c).unwrap_or_else(|| json!([]));
    let ops = match &batch {
        Value::Array(ops) => ops.clone(),
        other => vec![other.clone()],
    };
    let mut results = vec![];
    for op in ops {
        let op = op.as_object().cloned().unwrap_or_default();
        let name = op.get("operationName").cloned().unwrap_or(Value::Null);
        let text = match op.get("query") {
            Some(Value::String(q)) => q.split_whitespace().collect::<Vec<_>>().join(" "),
            Some(v) => dumps(v),
            None => String::new(),
        };
        let variables = match op.get("variables") {
            Some(Value::Object(v)) => v.clone(),
            Some(v) if truthy(v) => bail!("graphql: variables are not an object"),
            _ => Map::new(),
        };
        let mut keys: Vec<&String> = variables.keys().collect();
        keys.sort();
        let keys: Vec<String> = keys.iter().map(|k| format!("'{k}'")).collect();
        let shown = match &name {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            v => dumps(v),
        };
        let first = LOGGED_QUERIES.lock().insert(dumps(&name));
        log::info!(target: "powder.https", "graphql {shown} vars=[{}] query={}", keys.join(", "),
                   if first { text.as_str() } else { "(as above)" });
        results.push(match name.as_str() {
            Some("GetAchievementsCount") => gql_achievements_count(c, &variables)?,
            Some("GetAchievements") => gql_achievements(c, &variables)?,
            _ => json!({"data": {"games": []}}),
        });
    }
    let data = if batch.is_array() { Value::Array(results) } else { results.swap_remove(0) };
    Ok(ok(data, "graphql"))
}

fn space_of(variables: &Map<String, Value>) -> &str {
    variables.get("spaceId").and_then(Value::as_str).unwrap_or("")
}

/// {data: {game: {id, viewer: {meta: {id, achievements: connection}}}}} for Steep's space.
fn achievement_game(c: &Ctx, variables: &Map<String, Value>, connection: Value) -> Result<Value> {
    let (space, me) = (space_of(variables), me(c)?);
    let mut achievements = connection.as_object().cloned().unwrap_or_default();
    achievements.insert("__typename".into(), json!("AchievementConnection"));
    Ok(json!({"data": {"game": {
        "id": space, "__typename": "Game",
        "viewer": {"__typename": "GameUserEdge", "meta": {
            "id": format!("{me}-{space}"), "__typename": "UserGameMeta", "achievements": achievements}}}}}))
}

/// (names, earned) of the player's achievements in this game; empty for games other than Steep.
#[allow(clippy::type_complexity)]
fn achievements(c: &Ctx, variables: &Map<String, Value>) -> Result<(Vec<(i64, String, String)>, Vec<(i64, i64)>)> {
    if space_of(variables).to_lowercase() != SPACE_ID {
        return Ok((vec![], vec![]));
    }
    let earned = c.state.repos.achievements(&me(c)?, STEEP_ACHIEVEMENTS)?;
    Ok((c.state.assets.achievement_names("en-US"), earned))
}

fn gql_achievements_count(c: &Ctx, variables: &Map<String, Value>) -> Result<Value> {
    let (names, _) = achievements(c, variables)?;
    achievement_game(c, variables, json!({"totalCount": names.len()}))
}

/// Every achievement with its title, description, icon (served by this server) and whether and when the player
/// earned it.
fn gql_achievements(c: &Ctx, variables: &Map<String, Value>) -> Result<Value> {
    let (names, earned) = achievements(c, variables)?;
    let (space, me) = (space_of(variables), me(c)?);
    let host = c.req.header("Host").filter(|h| !h.is_empty()).unwrap_or(&c.req.host);
    let mut nodes = vec![];
    let mut completed = 0;
    for (id, title, description) in names {
        let ts = earned.iter().find(|(e, _)| *e == id).map(|(_, ts)| *ts);
        completed += usize::from(ts.is_some());
        let date = ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| t.format("%Y-%m-%dT%H:%M:%S.000Z").to_string());
        nodes.push(json!({
            "id": format!("{space}-{id}"), "achievementId": id, "title": title, "description": description,
            "icon": format!("https://{host}/_powder/achievements/{id}.png"), "__typename": "Achievement",
            "viewer": {"__typename": "AchievementUserEdge", "meta": {
                "id": format!("{me}-{space}-{id}"), "__typename": "UserAchievementMeta", "isCompleted": ts.is_some(),
                "completionDate": date}}}));
    }
    achievement_game(c, variables, json!({"totalCount": nodes.len(), "completedCount": completed, "nodes": nodes}))
}

/// GET /_powder/achievements/{id}.png: an achievement's icon from the pack.
fn achievement_icon(c: &Ctx) -> Reply {
    let raw = c.param("ach_id");
    let icon = (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
        .then(|| raw.parse::<i64>().ok()).flatten().and_then(|id| c.state.assets.achievement_icon(id));
    match icon {
        Some(png) => {
            let mut r = Reply::new(200, "image/png", png, "achievement-icon");
            r.headers.push(("Cache-Control".into(), "max-age=86400".into()));
            r
        }
        None => Reply::new(404, "text/plain; charset=utf-8", b"404: Not Found".to_vec(), "?"),
    }
}

// ---- game ---------------------------------------------------------------------------------------------------
/// GET /v1/profiles/me/populations and PUT .../populations/data (the game reports its populations).
fn populations(c: &Ctx) -> Result<Reply> {
    let (repos, pid) = (&c.state.repos, me(c)?);
    if c.req.method == "PUT" {
        if let Some(body) = json_body(c) {
            repos.kv_set(&pid, "populations_data", &body)?;
        }
    }
    let stored = repos.kv_get(&pid, "populations_data")?;
    let pops = stored.as_ref().and_then(|s| s.get("populations")).filter(|p| truthy(p)).cloned();
    Ok(ok(json!({"populations": pops.unwrap_or_else(|| json!([]))}), "populations"))
}

/// Ubisoft Club actions: the definitions from the game-data pack, for this player (no progress).
fn actions(c: &Ctx) -> Result<Reply> {
    let pid = pid(c)?;
    let defs = &c.state.assets.actions;
    let out: Vec<Value> = defs.iter().map(|a| {
        let mut a = a.as_object().cloned().unwrap_or_default();
        a.insert("profileId".into(), json!(pid));
        a.insert("value".into(), json!(0));
        Value::Object(a)
    }).collect();
    Ok(ok(json!({"actions": out}), &format!("actions({})", defs.len())))
}

/// GET /v3/profiles?profileIds=|userIds=|idOnPlatform=&platformType=: players this server knows.
fn profiles_lookup(c: &Ctx) -> Result<Reply> {
    signed_in(c)?;
    let q = query(c);
    let wanted: HashSet<String> = ["profileIds", "userIds", "idOnPlatform", "idsOnPlatform", "nameOnPlatform"].iter()
        .flat_map(|k| q.get(k).unwrap_or("").split(',').filter(|x| !x.is_empty()).map(str::to_lowercase)
            .collect::<Vec<_>>())
        .collect();
    let mut out = vec![];
    for p in c.state.repos.players()? {
        let keys = [p.profile_id.clone(), p.user_id.to_lowercase(), p.name.to_lowercase()];
        if wanted.is_empty() || keys.iter().any(|k| wanted.contains(k)) {
            out.push(json!({"profileId": p.profile_id, "userId": p.user_id, "platformType": "uplay",
                            "idOnPlatform": p.user_id, "nameOnPlatform": p.name}));
        }
    }
    let how = format!("profiles({})", out.len());
    Ok(ok(json!({"profiles": out}), &how))
}

/// GET /v3/users/{id}: country and language only.
fn user_info(c: &Ctx) -> Reply {
    ok(json!({"country": c.state.config.server.country, "preferredLanguage": "en"}), "user")
}

/// The game's error reports (the pipeline keeps the body); acknowledge like the real service.
fn remotelog(_: &Ctx) -> Reply {
    ok(json!({"transactionId": uuid::Uuid::new_v4().to_string(), "status": "OK", "statusMessage": "Success"}),
       "remotelog")
}

fn no_content() -> Reply {
    Reply { status: 204, headers: vec![], body: vec![], how: "ok".into() }
}

// ---- Ubisoft Connect ----------------------------------------------------------------------------------------
fn users_me(c: &Ctx) -> Result<Reply> {
    let me = me(c)?;
    let p = c.state.repos.player(&me)?;
    let name = p.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "Player".into());
    // Every field of the real reply must be present (Ubisoft Connect drops the session otherwise), with neutral
    // values: no real email (reserved .invalid domain), no phone number, no names.
    Ok(ok(json!({
        "userId": p.as_ref().map(|p| p.user_id.clone()).unwrap_or(me), "username": name, "nameOnPlatform": name,
        "accountIssues": null, "communicationOptIn": false, "communicationThirdPartyOptIn": false,
        "country": c.state.config.server.country, "region": null,
        "dateCreated": p.as_ref().map(|p| p.created_at.clone()).unwrap_or_else(iso_now),
        "dateAnonymized": null, "dateOfBirth": "1990-01-01T00:00:00.0000000Z",
        "email": format!("{}@powder.invalid", name.to_lowercase()), "firstName": "", "gender": "", "lastName": "",
        "hasAcceptedLegalOptins": true, "preferredLanguage": "en",
        "status": {"ageVerification": "notVerified", "autoGeneratedUsername": false, "changeEmailPending": false,
                   "dateOfBirthApproximated": false, "generalStatus": "activated", "inactiveAccount": false,
                   "invalidEmail": false, "invalidPhone": false, "locked": false, "minor": false,
                   "missingRequiredInformation": false, "passwordUpdateRequired": false,
                   "pendingDeactivation": false, "phoneActivated": false, "phoneSanctioned": false,
                   "recoveringPassword": false, "reserved": false, "stolenAccount": false,
                   "suspiciousActivity": false, "targetDeactivationDate": null, "testAccount": false},
        "phone": {"countryCallingCode": "", "number": ""}, "accountType": "Ubisoft", "ageGroup": "adult"}),
       "users-me"))
}

fn users_me_profiles(c: &Ctx) -> Result<Reply> {
    let profiles: Vec<Value> = c.state.repos.player(&me(c)?)?.into_iter()
        .map(|p| json!({"profileId": p.profile_id, "userId": p.user_id, "platformType": "uplay",
                        "idOnPlatform": p.user_id, "nameOnPlatform": p.name}))
        .collect();
    Ok(ok(json!({"profiles": profiles}), "users-me-profiles"))
}

fn iplocation(c: &Ctx) -> Reply {
    let country = &c.state.config.server.country;
    ok(json!({
        "city": null, "countryCode": country, "countryName": country, "ip": c.req.client_ip(), "isp": "",
        "latitude": "0.000000", "longitude": "0.000000", "organization": "", "regionCode": "", "regionName": "",
        "timeZone": "UTC", "zipCode": null}), "iplocation")
}

fn online_profiles(c: &Ctx) -> Result<std::collections::BTreeSet<String>> {
    let s = c.state;
    friends::online_profiles(&s.config, &s.repos, &s.sessions, &s.connect_clients)
}

fn online_statuses(c: &Ctx) -> Result<Reply> {
    signed_in(c)?;
    let q = query(c);
    let raw = q.get("UserIds").or_else(|| q.get("userIds")).unwrap_or("");
    let now = online_profiles(c)?;
    let online: HashSet<String> = c.state.repos.players()?.into_iter().filter(|p| now.contains(&p.profile_id))
        .map(|p| p.user_id.to_lowercase()).collect();
    let statuses: Vec<Value> = raw.split(',').filter(|u| !u.is_empty())
        .map(|u| json!({"connections": [], "onlineStatus": if online.contains(&u.to_lowercase()) { "online" }
                        else { "offline" }, "userId": u}))
        .collect();
    Ok(ok(json!({"onlineStatuses": statuses}), "online-statuses"))
}

/// The game's friends list (its social tab): every other player of the server (services/friends.rs).
fn game_friends(c: &Ctx) -> Result<Reply> {
    let since = iso_now();
    let list: Vec<Value> = friends::friends_of(&c.state.repos, Some(&me(c)?))?.into_iter()
        .map(|f| json!({"pid": f.profile_id, "nameOnPlatform": f.name, "state": "Friends", "lastModified": since}))
        .collect();
    Ok(ok(json!({"friends": list}), "friends"))
}

/// Ubisoft Connect's friends list; the enriched one (harboursocial) also has each one's online status.
fn connect_friends(c: &Ctx) -> Result<Reply> {
    signed_in(c)?;
    let (since, enriched) = (iso_now(), c.req.path().ends_with("/enriched"));
    let online = if enriched { online_profiles(c)? } else { Default::default() };
    let mut out = vec![];
    for f in friends::friends_of(&c.state.repos, Some(&pid(c)?))? {
        let mut entry = json!({"isFavorite": false, "lastModified": since, "nickname": "", "pid": f.profile_id,
                               "state": "Friends"});
        let fields = entry.as_object_mut().expect("an object");
        if enriched {
            let status = if online.contains(&f.profile_id) { "online" } else { "offline" };
            fields.insert("onlineStatus".into(), json!(status));
        } else {
            fields.insert("friendshipDate".into(), json!(since));
        }
        out.push(entry);
    }
    Ok(ok(json!({"groupId": uuid::Uuid::new_v4().to_string(), "friends": out}), "friends"))
}

/// What the game's connection asks to be told (as recorded); the game only checks that a friend has one.
const STEEP_MESSAGE_TYPES: [&str; 13] = [
    "STEEP_LIVEREPLAY", "STEEP_SPECTATOR", "STEEP_MAINTENANCE", "US_APPLICATION_CONFIGURATION_UPDATE",
    "US_NOTIFICATION_MAINTENANCE", "US_CLIENT_SECONDARY_STORE_UPDATE", "CLUB_BADGE_ACQUIRED",
    "CLUB_CHALLENGE_COMPLETED", "CLUB_CHALLENGE_BANKED", "CLUB_CHALLENGE_PARTICIPATION",
    "CLUB_CHALLENGE_THRESHOLD_REACHED", "CLUB_PROGRESSION_REACHED", "CLUB_REWARD_PURCHASED"];

const IM_HOST: &str = "public-im-ubiservices.ubi.com";

/// GET /v1/profiles/connections?profileIds=..&applications=<Steep>: which friends are in Steep now (the game asks
/// every minute; only a friend with a connection is "online" in its social tab and can be invited).
fn profile_connections(c: &Ctx) -> Result<Reply> {
    signed_in(c)?;
    let q = query(c);
    let list = |k: &str| -> Vec<String> {
        q.get(k).unwrap_or("").split(',').filter(|x| !x.is_empty()).map(str::to_lowercase).collect()
    };
    let (wanted, apps) = (list("profileIds"), list("applications"));
    if !apps.is_empty() && !apps.iter().any(|a| a == APP_ID) {
        return Ok(ok(json!({"connections": []}), "connections"));
    }
    let s = c.state;
    let (playing, now) = (friends::in_game(&s.config, &s.repos, &s.sessions)?, iso_now());
    let connections: Vec<Value> = wanted.iter().filter(|p| playing.contains(*p)).map(|pid| {
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, format!("powder-connection/{pid}").as_bytes());
        json!({"profileId": pid, "connectionId": id.to_string(), "applicationId": APP_ID,
               "contactProtocol": "HTTP Post",
               "contactUrl": format!("https://{IM_HOST}/v2/connections/powder/8180/{pid}/notifications"),
               "messageTypes": STEEP_MESSAGE_TYPES, "spaceIndependentMessageTypes": [], "obj": {}, "createdDate": now,
               "lastModifiedDate": now, "processId": "powder", "spaceIds": [SPACE_ID], "stagingSpaceId": SPACE_ID,
               "ubiLocaleCode": "en-US"})
    }).collect();
    Ok(ok(json!({"connections": connections}), "connections"))
}

/// Ubisoft Connect's chat channels (services/channels.rs), which also carry game invites.
fn channels_mine(c: &Ctx) -> Result<Reply> {
    let q = query(c);
    let found = c.state.channels.of(&me(c)?, q.get("spaceId"));
    Ok(ok(json!({"channels": found, "nextToken": ""}), "channels"))
}

fn channel_create(c: &Ctx) -> Result<Reply> {
    let (body, me) = (object_body(c), me(c)?);
    log::info!(target: "powder.https", "channel create request from {me}: {}", logged(&body));
    Ok(ok(c.state.channels.create(&me, c.param("space"), &body), "channel-created"))
}

fn channel_messages(c: &Ctx) -> Result<Reply> {
    let (channels, cid, me) = (&c.state.channels, c.param("channel"), me(c)?);
    if c.req.method != "POST" {
        let found = channels.messages(&me, cid).unwrap_or_default();
        return Ok(ok(json!({"messages": found, "nextToken": ""}), "channel-messages"));
    }
    let body = object_body(c);
    log::info!(target: "powder.https", "channel {cid} message from {me}: {}", logged(&body));
    if body.get("type").and_then(Value::as_str) == Some("GAME_INVITE") {
        // Accepting needs Ubisoft Connect's own pages (pop-up, chat), which can't be shown here: refuse, so the
        // inviting game says the invite failed instead of nothing happening for the friend.
        log::info!(target: "powder.https", "game invite from {me} refused (invites can't be accepted on this server)");
        return Ok(Reply::json(&json!({"errorCode": 1003, "httpCode": 403,
                                      "message": "Game invites are not available on this server."}),
                              "game-invite-refused", 403));
    }
    Ok(match channels.post(&me, cid, &body) {
        Some(message) => ok(message, "channel-message"),
        None => Reply::json(&json!({"errorCode": 1, "message": "no such channel"}), "?", 404),
    })
}

/// POST /v2/profiles/{profile_id}/notifications: one game's message to another player's game (Steep's
/// "Spectate" asks the friend this way, STEEP_SPECTATOR). Delivered over the target's notification websocket; the
/// request is logged, as no recording of one exists.
fn send_notification(c: &Ctx) -> Result<Reply> {
    let (body, sender, target) = (object_body(c), me(c)?, pid(c)?);
    log::info!(target: "powder.https", "notification from {sender} to {target}: {}", logged(&body));
    let kind = [body.get("notificationType"), body.get("type")].into_iter().flatten().find(|v| truthy(v))
        .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| dumps(v))).unwrap_or_default();
    let content = body.get("content").cloned().unwrap_or_else(|| body.clone());
    let mut extra = Map::new();
    extra.insert("source".into(), json!({"profileId": sender}));
    extra.insert("spaceId".into(), body.get("spaceId").cloned().unwrap_or_else(|| json!(SPACE_ID)));
    if c.state.sessions.push_ws(&target, &kind, content, extra) == 0 {
        log::info!(target: "powder.https", "notification {kind} to {target}: no websocket open, not delivered");
    }
    Ok(ok(json!({}), "notification-sent"))
}

fn club_title(c: &Ctx) -> Result<Reply> {
    let space = c.params.get("space").map(String::as_str).unwrap_or(SPACE_ID);
    Ok(ok(json!({
        "spaceId": space, "profileId": pid(c)?, "owned": false,
        "units": {"owned": 0, "available": 0}, "actions": {"owned": 0, "available": 0},
        "badges": {"owned": 0, "available": 0}, "rewards": {"new": 0, "owned": 0, "available": 0},
        "gameXp": {"owned": 0, "threshold": 6000, "breakdown": {"actions": [], "challenges": 0}},
        "challengesCompleted": 0}), "club-title"))
}

/// Connect's play session for a running game: just acknowledge (POST start, PUT refresh, DELETE end).
fn playsession(c: &Ctx) -> Reply {
    let (stamp, status) = match c.req.method.as_str() {
        "DELETE" => return no_content(),
        "POST" => ("createdAt", 201),
        "PUT" => ("updatedAt", 200),
        // the Python handler fails on any other method (KeyError)
        m => return Reply::internal_error(&format!("playsession: no answer for {m}")),
    };
    let mut body = Map::new();
    body.insert("expiresAt".into(), json!(iso_in(330)));
    body.insert(stamp.into(), json!(iso_now()));
    Reply::json(&Value::Object(body), "playsession", status)
}

#[cfg(test)]
mod tests {
    //! The Python tests/test_https.py (personal endpoints, friends, notifications, channels, GraphQL) and the
    //! populations part of tests/test_progression.py. Made-up ids and names (tests/synthetic.py).
    use super::super::testing::*;
    use super::super::{handle, HttpState, Req};
    use super::*;
    use crate::assets::{Assets, HttpFixture, HttpFixtures};
    use crate::config::Config;
    use crate::core::sessions::NexChannel;
    use crate::repos::Repos;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::Arc;

    const PLAYER_NAME: &str = "TestPlayer";
    const FRIEND: &str = "00000000-0000-4000-8000-000000000002";
    const ICON: &[u8] = b"\x89PNG\r\n\x1a\nmade-up";
    const ACHIEVEMENT_NAMES: [(i64, &str, &str); 4] = [
        (1, "First steps", "Do the first thing"), (4, "Four", "Do a fourth thing"),
        (15, "Fifteen", "Do it fifteen times"), (30, "Thirty", "Not earned yet")];

    /// Made-up achievement definitions in Ubisoft Connect's format (texts per language + icons).
    fn achievement_zip() -> Vec<u8> {
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

    fn action() -> Value {
        json!({"spaceId": SPACE_ID, "profileId": "00000000-0000-4000-8000-000000000001", "id": "1000", "value": 3,
               "name": "The legend starts here", "xp": 20, "description": "Play", "isBadge": false})
    }

    /// A state with a small pack (one Club action, the achievement definitions) and the default player, named
    /// like the Python tests' imported one.
    fn with_pack(config: Config) -> (HttpState, String) {
        let zip = HttpFixture { method: "GET".into(), host: "static3.cdn.ubi.com".into(),
                                path: "/orbit/x/assets/achievements.zip".into(), status: 200,
                                content_type: "application/zip".into(), body: achievement_zip() };
        let assets = Assets { actions: vec![action()], connect_http: HttpFixtures::new(vec![zip]), ..Default::default() };
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        let services = crate::services::Services::new(Arc::new(config), Arc::new(assets), Arc::new(Repos::new(db)),
                                                      None);
        let st = HttpState { services: Arc::new(services), nex_public_key: "TESTKEY".into(), demux: None };
        let pid = st.accounts.default_profile_id().unwrap();
        st.accounts.player(&pid).unwrap();
        st.repos.rename_player(&pid, PLAYER_NAME).unwrap();
        (st, pid)
    }

    fn get(st: &HttpState, path: &str) -> Reply {
        handle(st, &req("GET", path, &[]))
    }

    fn with_body(method: &str, path: &str, headers: &[(&str, &str)], body: &Value) -> Req {
        let mut r = req(method, path, headers);
        r.body = dumps(body).into_bytes().into();
        r
    }

    fn post(st: &HttpState, path: &str, body: &Value) -> Reply {
        handle(st, &with_body("POST", path, &[], body))
    }

    fn keys(v: &Value) -> Vec<&str> {
        v.as_object().unwrap().keys().map(String::as_str).collect()
    }

    fn register(st: &HttpState, email: &str, name: &str) -> String {
        st.accounts.register(email, "correct horse", name).unwrap().profile_id
    }

    /// The Authorization header of a new session of this player (the game's, from 127.0.0.1).
    fn auth_of(st: &HttpState, pid: &str) -> String {
        format!("Ubi_v1 t={}", st.sessions.issue(st.accounts.player(pid).unwrap(), "127.0.0.1", "").ticket)
    }

    fn get_as(st: &HttpState, auth: &str, path: &str) -> Reply {
        handle(st, &req("GET", path, &[("Authorization", auth)]))
    }

    /// Account details, friends and Club actions come from the player record: nothing of a recorded account
    /// (email, friends, action progress) is ever served.
    #[test]
    fn personal_endpoints_are_generated_not_replayed() {
        let (st, pid) = with_pack(Config::default());
        let r = get(&st, "/v3/users/me");
        assert_eq!((r.status, r.how.as_str()), (200, "users-me"));
        let me = body(&r);
        assert_eq!(me["nameOnPlatform"], PLAYER_NAME);
        assert_eq!(me["email"], "testplayer@powder.invalid");
        assert_eq!(me["phone"]["number"], "");
        assert_eq!(keys(&me), ["userId", "username", "nameOnPlatform", "accountIssues", "communicationOptIn",
                               "communicationThirdPartyOptIn", "country", "region", "dateCreated", "dateAnonymized",
                               "dateOfBirth", "email", "firstName", "gender", "lastName", "hasAcceptedLegalOptins",
                               "preferredLanguage", "status", "phone", "accountType", "ageGroup"]);
        assert_eq!(keys(&me["status"]).len(), 21);
        // the bytes routes_profile.py writes (made with it, for userId UID and dateCreated CREATED)
        let created = st.repos.player(&pid).unwrap().unwrap().created_at;
        let text = String::from_utf8(r.body).unwrap().replace(&pid, "UID").replace(&created, "CREATED");
        assert_eq!(text, concat!(
            r#"{"userId": "UID", "username": "TestPlayer", "nameOnPlatform": "TestPlayer", "accountIssues": null, "#,
            r#""communicationOptIn": false, "communicationThirdPartyOptIn": false, "country": "US", "region": null, "#,
            r#""dateCreated": "CREATED", "dateAnonymized": null, "dateOfBirth": "1990-01-01T00:00:00.0000000Z", "#,
            r#""email": "testplayer@powder.invalid", "firstName": "", "gender": "", "lastName": "", "#,
            r#""hasAcceptedLegalOptins": true, "preferredLanguage": "en", "status": {"ageVerification": "notVerified", "#,
            r#""autoGeneratedUsername": false, "changeEmailPending": false, "dateOfBirthApproximated": false, "#,
            r#""generalStatus": "activated", "inactiveAccount": false, "invalidEmail": false, "invalidPhone": false, "#,
            r#""locked": false, "minor": false, "missingRequiredInformation": false, "passwordUpdateRequired": false, "#,
            r#""pendingDeactivation": false, "phoneActivated": false, "phoneSanctioned": false, "#,
            r#""recoveringPassword": false, "reserved": false, "stolenAccount": false, "suspiciousActivity": false, "#,
            r#""targetDeactivationDate": null, "testAccount": false}, "phone": {"countryCallingCode": "", "number": ""}, "#,
            r#""accountType": "Ubisoft", "ageGroup": "adult"}"#));
        let friends = body(&get(&st, &format!("/v3/profiles/{pid}/friends")));
        assert_eq!(friends["friends"], json!([]));
        assert_eq!(get(&st, "/v2/profiles/me/friends").body, br#"{"friends": []}"#);
        let r = get(&st, &format!("/v1/profiles/{pid}/actions"));
        assert_eq!(r.how, "actions(1)");
        let actions = body(&r)["actions"].clone();
        assert_eq!((actions[0]["id"].as_str(), actions[0]["value"].as_i64()), (Some("1000"), Some(0)));
        assert_eq!(actions[0]["profileId"], pid.as_str());
        assert_eq!(keys(&actions[0]), keys(&action()), "dict merge keeps the keys' places");
        let looked_up = body(&get(&st, &format!("/v3/profiles?profileIds={pid},{FRIEND}")));
        assert_eq!(looked_up, json!({"profiles": [{"profileId": pid, "userId": pid, "platformType": "uplay",
                                                    "idOnPlatform": pid, "nameOnPlatform": PLAYER_NAME}]}));
        let by_name = body(&get(&st, "/v3/profiles?nameOnPlatform=testplayer"));
        assert_eq!(by_name["profiles"].as_array().unwrap().len(), 1);
        assert_eq!(body(&get(&st, "/v3/users/me/profiles"))["profiles"][0]["nameOnPlatform"], PLAYER_NAME);
    }

    #[test]
    fn cors_for_connects_web_ui() {
        let (st, _) = with_pack(Config::default());
        let origin = "https://connect.cdn.ubisoft.com";
        let r = handle(&st, &req("GET", "/v3/users/me", &[("Origin", origin)]));
        assert_eq!(header(&r, "access-control-allow-origin"), Some(origin));
        let r = handle(&st, &req("GET", "/v3/users/me", &[])); // the game sends no Origin: untouched
        assert_eq!(header(&r, "access-control-allow-origin"), None);
    }

    /// The game's friends list (social tab) and Connect's: the server's other players, with online status.
    #[test]
    fn every_other_player_is_a_friend() {
        let (st, pid) = with_pack(Config::default());
        register(&st, "host@example.com", ""); // takes over the default player
        let guest = register(&st, "guest@example.com", "Guest");
        let auth = auth_of(&st, &pid);
        let game = body(&get_as(&st, &auth, "/v2/profiles/me/friends"))["friends"].clone();
        assert_eq!(game.as_array().unwrap().len(), 1);
        assert_eq!((game[0]["pid"].as_str(), game[0]["nameOnPlatform"].as_str(), game[0]["state"].as_str()),
                   (Some(guest.as_str()), Some("Guest"), Some("Friends")));
        assert_eq!(keys(&game[0]), ["pid", "nameOnPlatform", "state", "lastModified"]);
        let plain = body(&get_as(&st, &auth, &format!("/v3/profiles/{pid}/friends")));
        assert_eq!(keys(&plain), ["groupId", "friends"]);
        assert_eq!(plain["friends"][0]["pid"], guest.as_str());
        assert_eq!(keys(&plain["friends"][0]), ["isFavorite", "lastModified", "nickname", "pid", "state",
                                                "friendshipDate"]);
        let url = format!("/v3/profiles/{pid}/global/harboursocial/friends/core/enriched");
        assert_eq!(body(&get_as(&st, &auth, &url))["friends"][0]["onlineStatus"], "offline");
        let connect = st.connect_clients.add("192.0.2.20:1000"); // Guest's Connect is on
        connect.update(|c| c.profile_id = Some(guest.clone()));
        let enriched = body(&get_as(&st, &auth, &url));
        assert_eq!(enriched["friends"][0]["onlineStatus"], "online");
        assert!(enriched["friends"][0].get("friendshipDate").is_none());
        let user = st.repos.player(&guest).unwrap().unwrap().user_id;
        let statuses = body(&get_as(&st, &auth, &format!("/v1/users/onlineStatuses?UserIds={user},someone-else")));
        assert_eq!(statuses, json!({"onlineStatuses": [
            {"connections": [], "onlineStatus": "online", "userId": user},
            {"connections": [], "onlineStatus": "offline", "userId": "someone-else"}]}));
    }

    struct FakeNex;

    impl NexChannel for FakeNex {
        fn send_request(&self, _: &str, _: &str, _: &[u8], _: Option<u32>) -> u32 {
            0
        }
    }

    /// The game asks which friends are in Steep (and only offers to invite those).
    #[test]
    fn a_friend_in_steep_has_a_connection() {
        let (st, pid) = with_pack(Config::default());
        let guest = register(&st, "guest@example.com", "Guest");
        let auth = auth_of(&st, &pid);
        let url = format!("/v1/profiles/connections?limit=24&offset=0&profileIds={guest}&applications={APP_ID}");
        assert_eq!(get_as(&st, &auth, &url).body, br#"{"connections": []}"#);
        let session = st.sessions.issue(st.accounts.player(&guest).unwrap(), "192.0.2.20", "");
        session.channels.set_nex(Some(Arc::new(FakeNex))); // Guest's game is connected
        let conns = body(&get_as(&st, &auth, &url))["connections"].clone();
        assert_eq!(conns.as_array().unwrap().len(), 1);
        let conn = &conns[0];
        assert_eq!((conn["profileId"].as_str(), conn["applicationId"].as_str()), (Some(guest.as_str()), Some(APP_ID)));
        let want_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, format!("powder-connection/{guest}").as_bytes());
        assert_eq!(conn["connectionId"], want_id.to_string());
        assert_eq!(keys(conn), ["profileId", "connectionId", "applicationId", "contactProtocol", "contactUrl",
                                "messageTypes", "spaceIndependentMessageTypes", "obj", "createdDate",
                                "lastModifiedDate", "processId", "spaceIds", "stagingSpaceId", "ubiLocaleCode"]);
        assert_eq!(conn["messageTypes"].as_array().unwrap().len(), 13);
        let other_app = url.replace(APP_ID, "f68a4bb5-608a-4ff2-8123-be8ef797e0a6");
        assert_eq!(body(&get_as(&st, &auth, &other_app)), json!({"connections": []}));
        session.channels.set_nex(None);
        assert_eq!(body(&get_as(&st, &auth, &url)), json!({"connections": []}));

        let mut config = Config::default();
        config.multiplayer.pretend_online = vec!["guest".into()]; // testing alone
        let (st, pid) = with_pack(config);
        let guest = register(&st, "guest@example.com", "Guest");
        let auth = auth_of(&st, &pid);
        let url = format!("/v1/profiles/connections?profileIds={guest}&applications={APP_ID}");
        assert_eq!(body(&get_as(&st, &auth, &url))["connections"].as_array().unwrap().len(), 1);
    }

    /// uuid5(NAMESPACE_URL, "powder-connection/<pid>") as Python's uuid module makes it.
    #[test]
    fn connection_ids_are_pythons() {
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, b"powder-connection/00000000-0000-4000-8000-000000000002");
        assert_eq!(id.to_string(), "0edcae14-4068-5c91-82e3-365745382862");
    }

    /// POST /v2/profiles/{id}/notifications (Steep's Spectate asks the friend this way).
    #[test]
    fn a_notification_reaches_the_other_players_game() {
        let (st, pid) = with_pack(Config::default());
        let guest = register(&st, "guest@example.com", "Guest");
        let auth = auth_of(&st, &pid);
        let post = |path: &str, body: &Value| {
            handle(&st, &with_body("POST", path, &[("Authorization", &auth)], body))
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        st.sessions.issue(st.accounts.player(&guest).unwrap(), "192.0.2.20", "").channels.add_websocket(tx);
        let r = post(&format!("/v2/profiles/{guest}/notifications"),
                     &json!({"notificationType": "STEEP_SPECTATOR", "content": {"ask": 1}, "spaceId": SPACE_ID}));
        assert_eq!((r.status, r.body.as_slice(), r.how.as_str()), (200, &b"{}"[..], "notification-sent"));
        let n = rx.try_recv().unwrap();
        assert_eq!((n["notificationType"].as_str(), &n["content"], n["spaceId"].as_str()),
                   (Some("STEEP_SPECTATOR"), &json!({"ask": 1}), Some(SPACE_ID)));
        assert_eq!(n["source"]["profileId"], pid.as_str());
        assert_eq!(keys(&n), ["notificationType", "content", "datePosted", "source", "spaceId"]);
        assert!(rx.try_recv().is_err());
        // no "content": the whole body is
        post(&format!("/v2/profiles/{}/notifications", guest.to_uppercase()), &json!({"type": "X", "a": 1}));
        let n = rx.try_recv().unwrap();
        assert_eq!((n["notificationType"].as_str(), &n["content"]), (Some("X"), &json!({"type": "X", "a": 1})));
        assert_eq!(n["spaceId"], SPACE_ID);
    }

    /// Invites can't be accepted (Connect's pages are Ubisoft's): refused, so the inviting game reports it.
    #[test]
    fn game_invites_are_refused_chat_channels_stay() {
        let (st, pid) = with_pack(Config::default());
        let guest = register(&st, "guest@example.com", "Guest");
        let space = "c0539e9c-90ab-4af8-a2d6-ef7b3883f430";
        let auth = auth_of(&st, &pid);
        let host = [("Host", "channel-service.upc.ubi.com"), ("Authorization", auth.as_str())];
        let r = handle(&st, &with_body("POST", &format!("/v1/spaces/{space}/channels"), &host,
                                       &json!({"channel": {"type": "DM"}, "members": [guest]})));
        assert_eq!(r.how, "channel-created");
        let dm = body(&r);
        let id = dm["id"].as_str().unwrap();
        let messages = format!("/v1/spaces/{space}/channels/{id}/messages");
        let r = handle(&st, &with_body("POST", &messages, &host, &json!({"type": "GAME_INVITE", "textMessage": "e30="})));
        assert_eq!((r.status, r.how.as_str()), (403, "game-invite-refused"));
        assert_eq!(String::from_utf8(r.body).unwrap(),
                   r#"{"errorCode": 1003, "httpCode": 403, "message": "Game invites are not available on this server."}"#);
        assert_eq!(st.channels.messages(&guest, id), Some(vec![])); // nothing delivered
        // a chat message goes through; both see it
        let r = handle(&st, &with_body("POST", &messages, &host, &json!({"type": "TEXT_MESSAGE", "textMessage": "hi"})));
        assert_eq!((r.status, r.how.as_str()), (200, "channel-message"));
        let listed = body(&handle(&st, &req("GET", &messages, &host)));
        assert_eq!(keys(&listed), ["messages", "nextToken"]);
        assert_eq!(listed["messages"][0]["authorId"], pid.as_str());
        let mine = body(&get_as(&st, &auth, &format!("/v1/profiles/me/channels?spaceId={space}")));
        assert_eq!(mine["channels"][0]["id"], id);
        assert_eq!(body(&get_as(&st, &auth, "/v1/profiles/me/channels?spaceId=other"))["channels"], json!([]));
        let r = handle(&st, &with_body("POST", &format!("/v1/spaces/{space}/channels/nope/messages"), &host, &json!({})));
        assert_eq!(r.status, 404);
    }

    /// Ubisoft Connect's UI asks for achievements over GraphQL (batched): titles, icons and the player's unlocks
    /// come from the pack and the database.
    #[test]
    fn connect_achievement_view() {
        let (st, pid) = with_pack(Config::default());
        let earned = BTreeMap::from([(1, 1_500_000_000), (4, 1_500_000_100), (15, 1_500_000_200)]);
        st.repos.earn_achievements(&pid, STEEP_ACHIEVEMENTS, &earned).unwrap();
        let login = handle(&st, &req("POST", "/v3/profiles/sessions", &[("Authorization", "uplaypc_v1 t=connect-jwe")]));
        let auth = format!("Ubi_v1 t={}", body(&login)["ticket"].as_str().unwrap());
        let auth = [("Authorization", auth.as_str())];
        let batch = json!([
            {"operationName": "GetAchievementsCount", "variables": {"spaceId": SPACE_ID, "productId": 3279}},
            {"operationName": "GetAchievements", "variables": {"spaceId": SPACE_ID, "productId": 3279},
             "query": "query  GetAchievements {\n  game }"},
            {"operationName": "GetFriends", "variables": {"spaceId": SPACE_ID}}]);
        let r = handle(&st, &with_body("POST", "/v1/profiles/me/uplay/graphql", &auth, &batch));
        assert_eq!(r.how, "graphql");
        let out = body(&r);
        let (count, full, other) = (&out[0], &out[1], &out[2]);
        assert_eq!(count, &json!({"data": {"game": {"id": SPACE_ID, "__typename": "Game", "viewer": {
            "__typename": "GameUserEdge", "meta": {"id": format!("{pid}-{SPACE_ID}"), "__typename": "UserGameMeta",
            "achievements": {"totalCount": 4, "__typename": "AchievementConnection"}}}}}}));
        let ach = &full["data"]["game"]["viewer"]["meta"]["achievements"];
        assert_eq!((ach["totalCount"].as_i64(), ach["completedCount"].as_i64()), (Some(4), Some(3)));
        assert_eq!(keys(ach), ["totalCount", "completedCount", "nodes", "__typename"]);
        let nodes = ach["nodes"].as_array().unwrap();
        let first = &nodes[0];
        assert_eq!(first, &json!({
            "id": format!("{SPACE_ID}-1"), "achievementId": 1, "title": "First steps",
            "description": "Do the first thing",
            "icon": "https://public-ubiservices.ubi.com/_powder/achievements/1.png", "__typename": "Achievement",
            "viewer": {"__typename": "AchievementUserEdge", "meta": {
                "id": format!("{pid}-{SPACE_ID}-1"), "__typename": "UserAchievementMeta", "isCompleted": true,
                "completionDate": "2017-07-14T02:40:00.000Z"}}}));
        assert_eq!(keys(first), ["id", "achievementId", "title", "description", "icon", "__typename", "viewer"]);
        let last = &nodes[3]["viewer"]["meta"];
        assert_eq!((nodes[3]["achievementId"].as_i64(), last["isCompleted"].as_bool()), (Some(30), Some(false)));
        assert!(last["completionDate"].is_null());
        assert_eq!(other, &json!({"data": {"games": []}})); // one result per operation
        let icon = get(&st, "/_powder/achievements/1.png");
        assert_eq!((icon.status, icon.body.as_slice(), icon.how.as_str()), (200, ICON, "achievement-icon"));
        assert_eq!(header(&icon, "content-type"), Some("image/png"));
        assert_eq!(header(&icon, "cache-control"), Some("max-age=86400"));
        assert_eq!(get(&st, "/_powder/achievements/2.png").status, 404);
        assert_eq!(get(&st, "/_powder/achievements/x1.png").status, 404);
        // another game: nothing; a single (unbatched) operation gets a single result
        let r = handle(&st, &with_body("POST", "/v1/profiles/me/uplay/graphql", &auth, &json!({
            "operationName": "GetAchievements", "variables": {"spaceId": "00000000-0000-0000-0000-000000000000"}})));
        assert_eq!(body(&r)["data"]["game"]["viewer"]["meta"]["achievements"]["nodes"], json!([]));
        // not JSON: an empty batch
        assert_eq!(handle(&st, &req("POST", "/v1/profiles/me/uplay/graphql", &auth)).body, b"[]");
    }

    /// An empty list lets the game matchmake without a map (a map entity it can't download disables it).
    #[test]
    fn no_matchmaking_map() {
        let (st, _) = with_pack(Config::default());
        let r = get(&st, &format!("/v2/spaces/{SPACE_ID}/entities?type=TITLE_MATCHMAKING_MAP&tags=WORLD_10af88fa9e4\
                                   &offset=0&limit=10"));
        assert_eq!((r.status, r.body.as_slice()), (200, &br#"{"entities": []}"#[..]));
    }

    /// Once accounts exist, a request without a session is nobody: no default player to read as, overwrite,
    /// notify or chat as (401); with one, the session's player.
    #[test]
    fn without_a_session_nobody_acts_for_the_default_player() {
        let (st, host) = with_pack(Config::default());
        register(&st, "host@example.com", ""); // takes over the default player
        let guest = register(&st, "guest@example.com", "Guest");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        st.sessions.issue(st.accounts.player(&guest).unwrap(), "192.0.2.20", "").channels.add_websocket(tx);
        let refused = |r: Reply| {
            assert_eq!((r.status, r.how.as_str()), (401, "no-session"), "{}", String::from_utf8_lossy(&r.body));
            assert_eq!(keys(&body(&r)), ["message", "errorCode", "httpCode", "errorContext", "moreInfo",
                                         "transactionTime", "transactionId"]);
        };
        refused(handle(&st, &with_body("PUT", "/v1/profiles/me/populations/data", &[], &json!({"populations": [9]}))));
        assert_eq!(st.repos.kv_get(&host, "populations_data").unwrap(), None);
        refused(post(&st, &format!("/v2/profiles/{guest}/notifications"), &json!({"type": "STEEP_SPECTATOR"})));
        assert!(rx.try_recv().is_err(), "nothing delivered");
        refused(post(&st, "/v1/spaces/s/channels", &json!({"members": [guest]})));
        for path in ["/v3/profiles", "/v3/users/me", "/v2/profiles/me/friends", "/v1/profiles/me/channels",
                     "/v1/profiles/me/populations", "/v1/profiles/me/actions",
                     "/v1/profiles/me/global/ubiconnect/economy/api/units"] {
            refused(get(&st, path));
        }
        refused(get(&st, &format!("/v3/profiles/{host}/friends")));
        refused(post(&st, "/v1/profiles/me/uplay/graphql", &json!([{"operationName": "GetAchievements",
                                                                     "variables": {"spaceId": SPACE_ID}}])));
        // game-wide answers need nobody
        assert_eq!(get(&st, "/v1/profiles/me/news").status, 200);
        assert_eq!(get(&st, &format!("/v1/profiles/{guest}/global/ubiconnect/economy/api/units")).status, 200);
        // with a session: that player
        let auth = auth_of(&st, &guest);
        let r = handle(&st, &with_body("PUT", "/v1/profiles/me/populations/data", &[("Authorization", &auth)],
                                       &json!({"populations": [1]})));
        assert_eq!(r.status, 200);
        assert_eq!(st.repos.kv_get(&guest, "populations_data").unwrap(), Some(json!({"populations": [1]})));
        assert_eq!(body(&get_as(&st, &auth, "/v3/users/me"))["nameOnPlatform"], "Guest");
        assert_eq!(body(&get_as(&st, &auth, "/v3/profiles"))["profiles"].as_array().unwrap().len(), 2);
    }

    /// Without accounts, everything works without a ticket, as the default player.
    #[test]
    fn without_accounts_the_default_player_acts() {
        let (st, pid) = with_pack(Config::default());
        assert_eq!(body(&get(&st, "/v3/users/me"))["userId"], pid.as_str());
        assert_eq!(get(&st, "/v3/profiles").status, 200);
        assert_eq!(handle(&st, &with_body("PUT", "/v1/profiles/me/populations/data", &[], &json!({}))).status, 200);
    }

    /// The game's populations data is kept and echoed (tests/test_progression.py).
    #[test]
    fn populations_are_kept() {
        let (st, pid) = with_pack(Config::default());
        assert_eq!(get(&st, "/v1/profiles/me/populations").body, br#"{"populations": []}"#);
        let r = handle(&st, &with_body("PUT", "/v1/profiles/me/populations/data", &[], &json!({"populations": [1]})));
        assert_eq!(r.body, br#"{"populations": [1]}"#);
        assert_eq!(st.repos.kv_get(&pid, "populations_data").unwrap(), Some(json!({"populations": [1]})));
        assert_eq!(get(&st, "/v1/profiles/me/populations").body, br#"{"populations": [1]}"#);
        // not JSON: nothing changes
        let r = handle(&st, &req("PUT", "/v1/profiles/me/populations/data", &[]));
        assert_eq!(r.body, br#"{"populations": [1]}"#);
    }

    /// The fixed and per-player answers, byte for byte as the Python server writes them.
    #[test]
    fn fixed_answers() {
        let (st, pid) = with_pack(Config::default());
        let text = |r: Reply| String::from_utf8(r.body).unwrap();
        assert_eq!(text(get(&st, "/v3/users/someone/policies")),
                   r#"{"isAcceptanceMandatory": false, "displayValues": {"reacceptanceReason": null}, "hasAcceptedLegalOptins": true}"#);
        assert_eq!(text(get(&st, "/v3/users/someone")), r#"{"country": "US", "preferredLanguage": "en"}"#);
        assert_eq!(text(get(&st, "/v1/profiles/me/global/ubiconnect/economy/api/ubiplusprogressions")),
                   format!(r#"{{"profileId": "{pid}", "level": "0", "levelUpAt": null, "subscriptionDurationInMonths": 0}}"#));
        assert_eq!(text(get(&st, &format!("/v1/profiles/{}/global/ubiconnect/economy/api/units", FRIEND.to_uppercase()))),
                   format!(r#"{{"profileId": "{FRIEND}", "units": 0}}"#));
        assert_eq!(text(get(&st, "/v2/profiles/me/club/progression/title")),
                   format!(concat!(
                       r#"{{"spaceId": "{}", "profileId": "{}", "owned": false, "units": {{"owned": 0, "available": 0}}, "#,
                       r#""actions": {{"owned": 0, "available": 0}}, "badges": {{"owned": 0, "available": 0}}, "#,
                       r#""rewards": {{"new": 0, "owned": 0, "available": 0}}, "gameXp": {{"owned": 0, "threshold": 6000, "#,
                       r#""breakdown": {{"actions": [], "challenges": 0}}}}, "challengesCompleted": 0}}"#), SPACE_ID, pid));
        assert_eq!(text(get(&st, "/v2/profiles/me/news")),
                   r#"{"pagination": {"total": 0, "count": 0, "offset": 0, "limit": 20}, "news": []}"#);
        assert_eq!(text(post(&st, &format!("/v1/spaces/{SPACE_ID}/global/sre/opentelemetry/api/traces"), &json!({}))),
                   r#"{"partialSuccess": {}}"#);
        let r = handle(&st, &req("PATCH", "/v2/connections/powder/8180/x/notifications", &[]));
        assert_eq!((r.how.as_str(), r.body.as_slice()), ("notifications", &b"{}"[..]));
        let loc = body(&get(&st, "/v2/profiles/me/iplocation"));
        assert_eq!((loc["ip"].as_str(), loc["countryCode"].as_str()), (Some("127.0.0.1"), Some("US")));
        let remote = body(&post(&st, "/v1/profiles/me/remotelog", &json!({})));
        assert_eq!(keys(&remote), ["transactionId", "status", "statusMessage"]);
        assert_ne!(body(&get(&st, "/v1/profiles/me/blocks"))["groupId"], body(&get(&st, "/v1/profiles/me/blocks"))["groupId"]);
    }

    #[test]
    fn connect_play_session() {
        let (st, _) = with_pack(Config::default());
        let path = "/v1/profiles/me/global/ubiconnect/playsession/api/sessions";
        let r = handle(&st, &req("POST", path, &[]));
        assert_eq!((r.status, keys(&body(&r))), (201, vec!["expiresAt", "createdAt"]));
        let r = handle(&st, &req("PUT", path, &[]));
        assert_eq!((r.status, keys(&body(&r))), (200, vec!["expiresAt", "updatedAt"]));
        let r = handle(&st, &req("DELETE", path, &[]));
        assert_eq!((r.status, r.body.len(), r.how.as_str()), (204, 0, "ok"));
        assert_eq!(handle(&st, &req("GET", path, &[])).status, 500);
    }
}
