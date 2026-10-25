//! Inventory and store (UbiServices). Credits are inventory item CREDITS_ITEM.
//!
//! Buying in the shop AND receiving challenge rewards are the same call:
//!     POST /v1/profiles/{id}/inventory  {"spaceId", "offerId", "offerRevision"}
//! The offer (from the recorded catalogue) lists items with SIGNED quantities, e.g. a shop offer +1 skis /
//! -12000 credits, a gold-medal reward +600 credits / +1 item. Applying an offer adds each delta to the stored
//! inventory; the reply reports quantityBefore and the new total `quantity` (shape copied from real replies).

use crate::assets::Assets;
use crate::config::Config;
use crate::constants::{CREDITS_ITEM, SPACE_ID};
use crate::repos::Repos;
use crate::util::iso_now;
use serde_json::{json, Value};
use std::sync::Arc;

/// A refused change, with what UbiServices answers for it.
#[derive(Debug, PartialEq)]
pub struct EconomyError {
    pub status: u16,
    pub error_code: i64,
    pub message: String,
}

impl EconomyError {
    fn new(status: u16, error_code: i64, message: String) -> Self {
        EconomyError { status, error_code, message }
    }

    /// UbiServices-style error body.
    pub fn body(&self) -> Value {
        json!({"errorCode": self.error_code, "message": self.message, "httpCode": self.status})
    }
}

impl std::fmt::Display for EconomyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EconomyError {}

#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub item_id: String,
    pub delta: i64,
    pub before: i64,
    pub after: i64,
}

pub struct EconomyService {
    config: Arc<Config>,
    repos: Arc<Repos>,
    assets: Arc<Assets>,
}

impl EconomyService {
    pub fn new(config: Arc<Config>, repos: Arc<Repos>, assets: Arc<Assets>) -> Self {
        EconomyService { config, repos, assets }
    }

    /// GET /v1/profiles/{id}/inventory body (items sorted by itemId, descending, like the real one).
    pub fn inventory_reply(&self, profile_id: &str) -> anyhow::Result<Value> {
        let items: Vec<Value> = self.repos.inventory(profile_id)?.into_iter().map(|r| json!({
            "itemId": r.item_id, "quantity": r.quantity, "expirationDate": r.expiration_date,
            "lastModified": r.last_modified, "expirationDetails": null,
        })).collect();
        Ok(json!({"items": items}))
    }

    pub fn credits(&self, profile_id: &str) -> anyhow::Result<i64> {
        self.repos.quantity(profile_id, CREDITS_ITEM)
    }

    /// Purchase / reward. -> (reply body, changes).
    pub fn apply_offer(&self, profile_id: &str, offer_id: &str, revision: Option<i64>)
                       -> anyhow::Result<Result<(Value, Vec<Change>), EconomyError>> {
        let Some(offer) = self.assets.catalog.get(offer_id) else {
            return Ok(Err(EconomyError::new(404, 1001, format!("offer {offer_id} not found"))));
        };
        let offer_revision = offer.get("revision").and_then(Value::as_i64);
        if revision.is_some() && revision != offer_revision {
            log::warn!(target: "powder.economy", "offer {offer_id}: client revision {}, catalogue revision {} \
                       (accepted)", revision.unwrap_or_default(),
                       offer_revision.map(|r| r.to_string()).unwrap_or_else(|| "None".into()));
        }
        let kind = offer.get("type").and_then(Value::as_str);
        let toggles = self.config.toggles(Some(profile_id));
        // (item, requested delta, applied delta)
        let mut deltas: Vec<(String, i64, i64)> = vec![];
        for item in offer.get("items").and_then(Value::as_array).into_iter().flatten() {
            let item_id = item.get("itemId").and_then(Value::as_str).unwrap_or_default();
            let requested = item.get("quantity").and_then(Value::as_i64).unwrap_or_default();
            let mut applied = requested;
            if item_id == CREDITS_ITEM {
                if requested < 0 && toggles.free_purchases {
                    applied = 0;
                } else if requested > 0 && kind == Some("progression") {
                    // Python's round(): halves to even
                    applied = (requested as f64 * toggles.reward_credit_multiplier).round_ties_even() as i64;
                }
            }
            deltas.push((item_id.to_lowercase(), requested, applied));
        }

        let reason = match kind {
            Some("shop") => "purchase",
            Some(k) if !k.is_empty() => k,
            _ => "offer",
        };
        let now = iso_now();
        let changes = self.repos.db.tx(|| {
            let mut changes = vec![];
            for (item_id, _, applied) in &deltas {
                let before = self.repos.quantity(profile_id, item_id)?;
                let after = before + applied;
                if after < 0 {
                    return Ok(Err(EconomyError::new(400, 1002, format!("not enough of item {item_id} (have {before}, \
                                                                       need {})", -applied))));
                }
                changes.push(Change { item_id: item_id.clone(), delta: *applied, before, after });
            }
            for c in &changes {
                self.repos.set_inventory(profile_id, &c.item_id, c.after, Some(&now), None)?;
                self.repos.add_ledger(profile_id, &c.item_id, c.delta, c.after, reason, Some(offer_id))?;
            }
            Ok(Ok(changes))
        })?;
        let changes = match changes {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };

        let items: Vec<Value> = changes.iter().map(|c| json!({
            "quantityBefore": c.before, "spaceId": SPACE_ID, "profileId": profile_id, "itemId": c.item_id,
            "quantity": c.after, "expirationDate": null, "lastModified": now, "expirationDetails": [],
        })).collect();
        let details: Vec<Value> = deltas.iter().map(|(item_id, requested, applied)| json!({
            "receivedItemId": item_id, "requestedItemId": item_id, "receivedQuantity": applied,
            "requestedQuantity": requested, "code": null, "description": null,
        })).collect();
        let reply = json!({
            "primaryStoreItemQuantity": 0,
            "primaryStoreItemIsConsumable": false,
            "items": items,
            "additionalDetails": details,
            "transactionId": uuid::Uuid::new_v4().to_string(),
        });
        let summary: Vec<String> = changes.iter()
            .map(|c| format!("{} {}->{}", &c.item_id[..c.item_id.len().min(8)], c.before, c.after)).collect();
        log::info!(target: "powder.economy", "{} {reason} offer {offer_id} ({}): {}", &profile_id[..profile_id.len().min(8)],
                   offer.get("nameId").and_then(Value::as_str).unwrap_or("None"), summary.join(", "));
        Ok(Ok((reply, changes)))
    }

    /// Admin/mod: add (or remove) a quantity of an item.
    pub fn grant(&self, profile_id: &str, item_id: &str, delta: i64, reason: &str)
                 -> anyhow::Result<Result<Change, EconomyError>> {
        let item_id = item_id.to_lowercase();
        self.repos.db.tx(|| {
            let before = self.repos.quantity(profile_id, &item_id)?;
            let after = before + delta;
            if after < 0 {
                return Ok(Err(EconomyError::new(400, 1002, format!("would go negative ({before} {delta:+})"))));
            }
            self.repos.set_inventory(profile_id, &item_id, after, None, None)?;
            self.repos.add_ledger(profile_id, &item_id, delta, after, reason, None)?;
            Ok(Ok(Change { item_id: item_id.clone(), delta, before, after }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const P: &str = "11111111-2222-3333-4444-555555555555";
    const SKIS: &str = "aaaaaaaa-0000-0000-0000-000000000001";

    fn service(gameplay: &str) -> EconomyService {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let config = Config::parse(gameplay, std::path::Path::new("/x/config.toml")).unwrap();
        let catalog = crate::assets::OfferCatalog::new(vec![
            json!({"offerId": "shop-skis", "type": "shop", "revision": 3, "nameId": "Skis",
                   "items": [{"itemId": SKIS.to_uppercase(), "quantity": 1}, {"itemId": CREDITS_ITEM, "quantity": -500}]}),
            json!({"offerId": "gold", "type": "progression", "items": [{"itemId": CREDITS_ITEM, "quantity": 25}]}),
        ]);
        let repos = Repos::new(db);
        repos.create_player(P, P, "Tester").unwrap();
        EconomyService::new(Arc::new(config), Arc::new(repos), Arc::new(Assets { catalog, ..Default::default() }))
    }

    #[test]
    fn purchase_spends_credits_and_logs_the_ledger() {
        let s = service("");
        assert!(s.apply_offer(P, "shop-skis", Some(3)).unwrap().unwrap_err().message.starts_with("not enough"));
        assert_eq!(s.grant(P, CREDITS_ITEM, 600, "admin").unwrap().unwrap().after, 600);
        let (reply, changes) = s.apply_offer(P, "shop-skis", Some(3)).unwrap().unwrap();
        assert_eq!(changes.iter().map(|c| (c.before, c.after)).collect::<Vec<_>>(), vec![(0, 1), (600, 100)]);
        assert_eq!(reply["items"][0]["itemId"], SKIS);
        assert_eq!(reply["additionalDetails"][1]["requestedQuantity"], -500);
        assert_eq!(s.credits(P).unwrap(), 100);
        let ledger = s.repos.recent_ledger(P, 10).unwrap();
        assert_eq!((ledger[0].reason.as_str(), ledger[0].offer_id.as_deref()), ("purchase", Some("shop-skis")));
        assert_eq!(s.apply_offer(P, "nope", None).unwrap().unwrap_err().status, 404);
        assert_eq!(s.inventory_reply(P).unwrap()["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn toggles_free_purchases_and_reward_multiplier_round_like_python() {
        let s = service("[gameplay]\nfree_purchases = true\nreward_credit_multiplier = 1.5\n");
        let (_, changes) = s.apply_offer(P, "shop-skis", None).unwrap().unwrap();
        assert_eq!(changes[1].delta, 0);
        let (_, changes) = s.apply_offer(P, "gold", None).unwrap().unwrap();
        assert_eq!(changes[0].delta, 38); // 37.5 -> 38 (half to even)
        let s = service("[gameplay]\nreward_credit_multiplier = 1.1\n");
        let (_, changes) = s.apply_offer(P, "gold", None).unwrap().unwrap();
        assert_eq!(changes[0].delta, 28); // 27.500000000000004 -> 28
        assert_eq!(s.grant(P, CREDITS_ITEM, -100, "admin").unwrap().unwrap_err().status, 400);
    }
}
