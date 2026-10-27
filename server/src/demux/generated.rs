//! Ubisoft Connect demux replies built by the server (formats from the real service's replies).
//!
//!   friends_service   initialize -> every other player of the server; presence updates and acks -> success
//!   playtime_service  -> success
//!   ownership_service Steam proof-of-purchase registration -> success; a game's Uplay ticket, ownership token and
//!                     signature -> fresh opaque values (Connect only passes them on; this server is the only
//!                     validator)
//!   ach_frontend      earned achievements per player: stored when Connect writes them, returned when it reads
//!   cloudsave_service links for uploading, downloading and deleting cloud saves on this server
//!                     (services/cloudsaves.rs)
//!
//! Owned games (ownership_service initialize) are in ownership.rs.

use super::pb::{self, F, V};
use super::services::{make_reply, Handler, ServiceContext, ServiceRequest};
use crate::services::friends::friends_of;
use crate::util::{random_bytes, token_urlsafe, unix_now};
use std::collections::BTreeMap;

pub fn handler(service: &str, kind: u32) -> Option<Handler> {
    let f: Handler = match (service, kind) {
        ("friends_service", 22) => friends_initialize,
        ("friends_service", 13) => |_, req| success(req, 13),
        ("friends_service", 19) => |_, req| success(req, 19),
        ("cloudsave_service", 2) => cloudsave_links,
        ("cloudsave_service", 3) => cloudsave_legacy,
        ("ownership_service", 28) => ownership_token,
        ("ownership_service", 26) => ownership_signature,
        ("ownership_service", 19) => |_, req| success(req, 18),
        ("ownership_service", 14) => uplay_pc_ticket,
        ("playtime_service", 2) => playtime,
        ("ach_frontend", 4) => ach_write,
        ("ach_frontend", 3) => ach_earned,
        _ => return None,
    };
    Some(f)
}

fn success(req: &ServiceRequest, reply_kind: u32) -> Option<Vec<u8>> {
    Some(make_reply(req, reply_kind, V::Msg(vec![F::int(1, 1)])))
}

// ---- friends: 22 initialize (reply 22: {1 result, 2 friend...}), 13 / 19 presence and status updates ----------
/// The friends list: every other player of the server (services/friends.rs). An entry as recorded:
/// {1: 0, 2: {1 profile id, 2 name, 3 empty, 4 0}, 3: 3} (a friend; type 1 entries without a name, state 0, are
/// another relationship and not used).
fn friends_initialize(ctx: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    let friends = friends_of(ctx.deps.repos(), ctx.profile_id).unwrap_or_else(|e| {
        log::warn!(target: "powder.demux", "friends list for {:?}: {e:#}", ctx.profile_id);
        vec![]
    });
    let mut body = vec![F::int(1, 1)];
    body.extend(friends.iter().map(|f| {
        F::msg(2, vec![F::int(1, 0),
                       F::msg(2, vec![F::str(1, &f.profile_id), F::str(2, &f.name), F::bytes(3, b""), F::int(4, 0)]),
                       F::int(3, 3)])
    }));
    log::info!(target: "powder.demux", "friends list for {}: {} friend(s)", ctx.profile_id.unwrap_or("None"),
               friends.len());
    Some(make_reply(req, 22, V::Msg(body)))
}

// ---- cloud saves (mg.protocol.cloudsave_service, definitions from Connect itself) -----------------------------
// 2 = CloudsaveUrlReq {1 ownership token, 2 uplay id (product), 3 method (1 Put, 2 Get, 3 Delete), 4*: Item {1 name,
//   2: {1 write length, 2 md5 base64}}} -> 2 CloudsaveUrlRsp {1 status (1 Ok, 2 InternalError, 3 Denied), 2*: {1
//   header text, 2 url}}. Recorded for Put: header "Content-Length: N\nContent-MD5: x\n", url a presigned S3 link;
//   here links on this server.
pub const CLOUD_FILES: &str = "https://public-ubiservices.ubi.com/_powder/cloudsaves";
const CLOUD_OK: u64 = 1;
const CLOUD_ERROR: u64 = 2;
const CLOUD_DENIED: u64 = 3;

fn cloud_method(n: Option<u64>) -> Option<&'static str> {
    match n? {
        1 => Some("PUT"),
        2 => Some("GET"),
        3 => Some("DELETE"),
        _ => None,
    }
}

fn cloud_status(req: &ServiceRequest, status: u64) -> Option<Vec<u8>> {
    Some(make_reply(req, 2, V::Msg(vec![F::int(1, status)])))
}

fn cloudsave_links(ctx: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    let body = req.fields().unwrap_or_default();
    let (product, method) = (pb::int(&body, 2), cloud_method(pb::int(&body, 3)));
    let (Some(pid), Some(product), Some(method)) = (ctx.profile_id, product, method) else {
        return cloud_status(req, CLOUD_DENIED);
    };
    let saves = &ctx.deps.cloudsaves;
    let mut entries = vec![];
    for f in body.iter().filter(|f| f.0 == 4) {
        let item = match &f.1 {
            V::Bytes(raw) => pb::try_decode(raw).unwrap_or_default(),
            _ => vec![],
        };
        let slot = pb::text(&item, 1);
        let meta = pb::try_decode(pb::bytes(&item, 2).unwrap_or_default()).unwrap_or_default();
        let (length, md5) = (pb::int(&meta, 1), pb::text(&meta, 2));
        let link = saves.path(pid, product, &slot).and_then(|path| {
            if method == "GET" && !path.exists() {
                log::warn!(target: "powder.cloudsaves", "cloud save of {pid}: slot {slot} of product {product} \
                           asked for but not stored");
                return Ok(None);
            }
            saves.link(pid, product, &slot, Some(md5.as_str()).filter(|m| !m.is_empty()), method).map(Some)
        });
        let token = match link {
            Ok(Some(token)) => token,
            Ok(None) => return cloud_status(req, CLOUD_ERROR),
            Err(e) => {
                log::warn!(target: "powder.cloudsaves", "cloud save links for {pid}: {e}");
                return cloud_status(req, CLOUD_ERROR);
            }
        };
        let mut headers = String::new();
        if method == "PUT" {
            if let Some(n) = length {
                headers += &format!("Content-Length: {n}\n");
            }
            if !md5.is_empty() {
                headers += &format!("Content-MD5: {md5}\n");
            }
        }
        entries.push(F::msg(2, vec![F::str(1, &headers), F::str(2, &format!("{CLOUD_FILES}/{product}/{slot}?t={token}"))]));
    }
    log::info!(target: "powder.cloudsaves", "cloud save of {pid}: {} {method} link(s) for product {product}",
               entries.len());
    let mut out = vec![F::int(1, CLOUD_OK)];
    out.extend(entries);
    Some(make_reply(req, 2, V::Msg(out)))
}

// 3 = CloudsaveReq (Ubisoft's old cloud save system) {1 uplay id, 2 put / 3 get / 4 delete item, 5 list items,
//   6 username, 7 ownership token} -> 3 CloudsaveRsp {1 status (0 Ok, 1 InternalError, 2 Denied), 2 method, 3 path,
//   4 host, 5 header}: an HTTP request for Connect to make itself (an S3 request, in the old system). Connect lists
//   the old slots when the player has no cloud save yet ("Migrate to new cloud save system"): the listing here is
//   always empty, so there is nothing to migrate.
const LEGACY_OK: u64 = 0;
const LEGACY_ERROR: u64 = 1;

fn cloudsave_legacy(ctx: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    let body = req.fields().unwrap_or_default();
    if pb::first(&body, 5).is_none() {
        // only listing is asked for (migration)
        let kinds: std::collections::BTreeSet<u32> = body.iter().map(|f| f.0).collect();
        log::warn!(target: "powder.cloudsaves", "cloud save (old system) request not supported: {kinds:?}");
        return Some(make_reply(req, 3, V::Msg(vec![F::int(1, LEGACY_ERROR)])));
    }
    let product = match pb::first(&body, 1) {
        Some(V::Int(n)) => n.to_string(),
        Some(V::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => "None".into(),
    };
    log::info!(target: "powder.cloudsaves", "cloud save of {}: old-system listing for product {product} (always empty)",
               ctx.profile_id.unwrap_or("None"));
    Some(make_reply(req, 3, V::Msg(vec![F::int(1, LEGACY_OK), F::str(2, "GET"),
                                        F::str(3, &format!("/_powder/cloudsaves/legacy-list/{product}")),
                                        F::str(4, "public-ubiservices.ubi.com"), F::str(5, "")])))
}

// ---- ownership tokens and signatures (mg.protocol.ownership, definitions from Connect) ------------------------
// 28 OwnershipTokenReq {1 product} -> 27 OwnershipTokenRsp {1 success, 2 token, 3 expiration}; 26 SignOwnershipReq
// {1 product} -> 25 SignOwnershipRsp {1 success, 2 branch id, 3 expiration, 4 signature}. Connect needs them e.g.
// before a cloud save sync; it only passes them on to this server, which accepts any.
const OWNERSHIP_LIFETIME_S: u64 = 24 * 3600;

fn expiration() -> u64 {
    unix_now() as u64 + OWNERSHIP_LIFETIME_S
}

fn ownership_token(_: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    Some(make_reply(req, 27, V::Msg(vec![F::int(1, 1), F::str(2, &token_urlsafe(48)), F::int(3, expiration())])))
}

fn ownership_signature(_: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    Some(make_reply(req, 25, V::Msg(vec![F::int(1, 1), F::int(2, 0), F::int(3, expiration()),
                                         F::bytes(4, &random_bytes(64))])))
}

// ---- playtime: 2 = playtime report (reply 2: {1 result}) -------------------------------------------------------
fn playtime(_: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    Some(make_reply(req, 2, V::Msg(vec![F::int(1, 0)])))
}

// ---- ownership: 19 = Steam proof-of-purchase registration (reply 18), 14 = GetUplayPCTicketReq (reply 13) ------
fn uplay_pc_ticket(_: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    Some(make_reply(req, 13, V::Msg(vec![F::int(1, 1), F::str(2, &token_urlsafe(420))])))
}

// ---- achievements (flat style) ----------------------------------------------------------------------------------
// Connect keeps the earned achievements locally and syncs them:
//   4 = write: {1: {1: {1: {1 product code, 2 platform, 3 space id}, 2: {1: {1 id, 2 unix time}...}}...}}
//       (its whole known list for each product; reply 4: empty)
//   3 = read:  {2: {1 product code, 2 platform}} -> the stored list, same shape (see ach_earned)

/// "STP:PC" from a product key message {1 code, 2 platform, ...}.
pub fn product_name(key: &[u8]) -> Option<String> {
    let fields = pb::try_decode(key)?;
    let (code, platform) = (pb::text(&fields, 1), pb::text(&fields, 2));
    (!code.is_empty() && !platform.is_empty()).then(|| format!("{code}:{platform}"))
}

/// {achievement id: unix time} from {1: {1 id, 2 time}, 1: ...}.
pub fn earned_from(entries: &[u8]) -> BTreeMap<i64, i64> {
    let mut out = BTreeMap::new();
    for f in pb::try_decode(entries).unwrap_or_default() {
        let F(1, V::Bytes(value)) = f else { continue };
        let Some(e) = pb::try_decode(&value) else { continue };
        if let Some(id) = pb::int(&e, 1) {
            out.insert(id as i64, pb::int(&e, 2).unwrap_or(0) as i64);
        }
    }
    out
}

/// The products and their achievements in a write (kind 4) request, in request order.
pub fn earned_by_product(body: &[u8]) -> Vec<(String, BTreeMap<i64, i64>)> {
    let mut result: Vec<(String, BTreeMap<i64, i64>)> = vec![];
    for f in pb::try_decode(body).unwrap_or_default() {
        let F(1, V::Bytes(products)) = f else { continue };
        for entry in pb::try_decode(&products).unwrap_or_default() {
            let F(1, V::Bytes(entry)) = entry else { continue };
            let Some(fields) = pb::try_decode(&entry) else { continue };
            let Some(name) = pb::bytes(&fields, 1).and_then(product_name) else { continue };
            let earned = earned_from(pb::bytes(&fields, 2).unwrap_or_default());
            match result.iter_mut().find(|(n, _)| *n == name) {
                Some((_, all)) => all.extend(earned),
                None => result.push((name, earned)),
            }
        }
    }
    result
}

fn ach_write(ctx: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    if let Some(pid) = ctx.profile_id {
        let repos = ctx.deps.repos();
        for (product, earned) in earned_by_product(req.body_bytes()) {
            match repos.earn_achievements(pid, &product, &earned) {
                Ok(0) => {}
                Ok(new) => {
                    let total = repos.achievements(pid, &product).map(|a| a.len()).unwrap_or(0);
                    log::info!(target: "powder.demux", "achievements: {new} new for {pid} ({product}), {total} in total");
                }
                Err(e) => log::warn!(target: "powder.demux", "achievements of {pid} ({product}): {e:#}"),
            }
        }
    }
    Some(make_reply(req, 4, V::Bytes(vec![])))
}

/// {3: {1 profile id, 2: {1: {1: {1 product code, 2 platform}, 2: {1: {1 achievement id, 2 time}...}}}}}: the
/// product key is echoed from the request, with the player's stored achievements. The earned-list message (2) must
/// be present even when empty, or Connect reports "Achievements is missing for product" and fails to sync.
fn ach_earned(ctx: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    // request {2: {1 product code, 2 platform}}
    let key = req.fields().and_then(|f| pb::bytes(&f, 2).map(<[u8]>::to_vec))
        .unwrap_or_else(|| pb::encode(&[F::str(1, "STP"), F::str(2, "PC")]));
    let earned = match (ctx.profile_id, product_name(&key)) {
        (Some(pid), Some(name)) => ctx.deps.repos().achievements(pid, &name).unwrap_or_else(|e| {
            log::warn!(target: "powder.demux", "achievements of {pid} ({name}): {e:#}");
            vec![]
        }),
        _ => vec![],
    };
    let entries = pb::encode(&earned.iter().map(|(id, ts)| F::msg(1, vec![F::int(1, *id), F::int(2, *ts)]))
        .collect::<Vec<_>>());
    Some(make_reply(req, 3, V::Msg(vec![
        F::str(1, ctx.profile_id.unwrap_or("")),
        F::msg(2, vec![F::msg(1, vec![F(1, V::Bytes(key)), F(2, V::Bytes(entries))])]),
    ])))
}
