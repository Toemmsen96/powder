//! ownership_service initialize: the games Ubisoft Connect thinks the player owns.
//!
//! The reply lists products, each with its configuration as YAML (field 17). They come from the game-data pack
//! (connect/demux.json: Steep's Steam entry 3279, product 3280 and its add-ons), else from the server's own
//! entries (builtin/products.rs). The YAML includes `cloud_saves: yes|no`; unless [server] connect_cloud_saves
//! is on, it is switched off: Connect then skips syncing instead of failing (and asking) at launch and exit.
//!
//! Reply layout: {1: {1 request_id, 2 initialize {1 result, 2 {1 product (repeated)}, 5 0, 6 0}}}

use super::pb::{self, F, V};
use super::services::{make_reply, Handler, ServiceContext, ServiceRequest};
use crate::builtin::products as builtin_products;

pub const SERVICE: &str = "ownership_service";
pub const INITIALIZE: u32 = 2;
pub const PRODUCT_CONFIG: u32 = 17;

pub fn handler(service: &str, kind: u32) -> Option<Handler> {
    (service == SERVICE && kind == INITIALIZE).then_some(initialize as Handler)
}

fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Products with `cloud_saves: yes` turned off in their configuration -> (products, number changed).
pub fn without_cloud_saves(products: &[Vec<u8>]) -> (Vec<Vec<u8>>, usize) {
    let mut changed = 0;
    let out = products.iter().map(|value| {
        let Ok(fields) = pb::decode(value) else { return value.clone() };
        match pb::first(&fields, PRODUCT_CONFIG) {
            Some(V::Bytes(config)) if contains(config, b"cloud_saves: yes") => {
                let config = replace_all(config, b"cloud_saves: yes", b"cloud_saves: no");
                changed += 1;
                pb::encode(&pb::replace(&fields, PRODUCT_CONFIG, V::Bytes(config)))
            }
            _ => value.clone(),
        }
    }).collect();
    (out, changed)
}

pub fn initialize_body(products: &[Vec<u8>]) -> V {
    V::Msg(vec![F::int(1, 1), F::msg(2, products.iter().map(|p| F::bytes(1, p)).collect()), F::int(5, 0),
                F::int(6, 0)])
}

fn initialize(ctx: &ServiceContext, req: &ServiceRequest) -> Option<Vec<u8>> {
    let cloud_saves = ctx.deps.config.server.connect_cloud_saves;
    let recorded = &ctx.deps.assets.products.products;
    if recorded.is_empty() {
        // no game-data pack: the server's own entries
        let products = builtin_products::entries(cloud_saves, ctx.deps.assets.achievements_file().as_deref());
        log::info!(target: "powder.demux", "demux owned games: {} products (built in, no game-data pack)",
                   products.len());
        return Some(make_reply(req, INITIALIZE, initialize_body(&products)));
    }
    if cloud_saves {
        return Some(make_reply(req, INITIALIZE, initialize_body(recorded)));
    }
    let (products, changed) = without_cloud_saves(recorded);
    log::info!(target: "powder.demux", "demux owned games: {} products, cloud saves switched off for {changed}",
               products.len());
    Some(make_reply(req, INITIALIZE, initialize_body(&products)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(pid: u64, cloud: &str) -> Vec<u8> {
        pb::encode(&[F::int(1, pid), F::str(17, &format!("version: 2.0\r\nroot:\r\n  name: l1\r\n  cloud_saves: {cloud}\r\n"))])
    }

    #[test]
    fn owned_games_without_cloud_saves() {
        let (out, changed) = without_cloud_saves(&[product(3280, "yes"), product(9999, "no"), pb::encode(&[F::int(1, 4242)])]);
        assert_eq!(changed, 1);
        // lengths re-encoded
        assert_eq!(out, vec![product(3280, "no"), product(9999, "no"), pb::encode(&[F::int(1, 4242)])]);
    }

    #[test]
    fn initialize_layout() {
        let body = pb::encode(&[F(2, initialize_body(&[vec![0x08, 0x01]]))]);
        assert_eq!(hex::encode(body), "120c080112040a02080128003000"); // made by the Python server's codec
    }
}
