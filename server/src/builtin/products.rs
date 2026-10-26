//! Steep's owned-games entries for Ubisoft Connect, without a game-data pack and with one: a pack only says which
//! of these products to list (gamedata/validate.rs), so what Connect is told to run is always this.
//!
//! Connect only starts a game it finds in the player's owned games. The entries' ids and numbers come from
//! product_table.rs; their product configuration (YAML, field 17) is written here, with only what Connect
//! needs to start Steep: how to launch it (Steam, or Steep.exe from Connect's install folder), its app and
//! space ids, cloud saves on or off, and the achievement definitions file if the server has one. Names, images
//! and links of the real configuration are left out (Connect's library then shows less).

use super::product_table::{P, KINDS, PRODUCTS};
use crate::constants::{APP_ID, SPACE_ID};
use crate::demux::pb::{self, F, V};
use std::collections::BTreeSet;

pub const PRODUCT_CONFIG: u32 = 17;
pub const STEAM_APP_ID: u32 = 460920;

fn common(cloud_saves: bool, achievements: &str) -> String {
    let cloud_saves = if cloud_saves { "yes" } else { "no" };
    format!("  installer:\n    game_identifier: Steep\n  digital_distribution:\n    version: 0\n  \
             cloud_saves: {cloud_saves}\n{achievements}  app_id: {APP_ID}\n  space_id: {SPACE_ID}\n\
             localizations: {{}}\n")
}

fn connect_game(product_id: u64) -> String {
    format!("version: 2.0\nroot:\n  name: Steep\n  start_game:\n    online:\n      overlay_required: yes\n      \
             overlay_supported: yes\n      executables:\n      - shortcut_name: Steep\n        path:\n          \
             relative: steep.exe\n        working_directory:\n          register: \
             HKEY_LOCAL_MACHINE\\SOFTWARE\\Ubisoft\\Launcher\\Installs\\{product_id}\\InstallDir\n")
}

fn steam_game() -> String {
    let id = STEAM_APP_ID;
    format!("version: 2.0\nroot:\n  name: Steep\n  start_game:\n    steam:\n      overlay_required: yes\n      \
             overlay_supported: yes\n      steam_app_id: {id}\n      steam_installation_status_register: \
             HKEY_LOCAL_MACHINE\\SOFTWARE\\Valve\\Steam\\InstallPath\n      game_installation_status_register: \
             HKEY_CURRENT_USER\\SOFTWARE\\Valve\\Steam\\Apps\\{id}\\Installed\n  third_party_platform:\n    \
             name: Steam\n")
}

fn addon(name: &str, visible: &str, ulc: &str) -> String {
    format!("version: 2.0\nroot:\n  name: {name}\n  is_visible: {visible}\n{ulc}localizations: {{}}\n")
}

fn kind(product_id: u64) -> &'static str {
    KINDS.iter().find(|(id, _)| *id == product_id).map(|(_, k)| *k).unwrap_or("none")
}

/// The product configuration (CRLF line ends) of one entry; empty for products without one.
fn yaml(product_id: u64, fields: &[F], cloud_saves: bool, achievements_file: Option<&str>) -> Vec<u8> {
    let text = match kind(product_id) {
        "game" => {
            let steam = pb::int(fields, 7) == Some(3); // target partner: Steam
            let achievements = achievements_file
                .map(|f| format!("  uplay:\n    achievements: {f}\n    achievements_sync_id: STP\n"))
                .unwrap_or_default();
            let start = if steam { steam_game() } else { connect_game(product_id) };
            start + &common(cloud_saves, &achievements)
        }
        "dlc" => addon(&format!("Steep add-on {product_id}"), "yes", ""),
        "ulc" => addon("", "no", "  is_ulc: yes\n"),
        _ => return vec![],
    };
    text.replace('\n', "\r\n").into_bytes()
}

pub(crate) fn to_fields(table: &[(u32, P)]) -> Vec<F> {
    table
        .iter()
        .map(|(n, v)| match v {
            P::I(i) => F(*n, V::Int(*i)),
            P::S(s) => F::str(*n, s),
            P::M(sub) => F::msg(*n, to_fields(sub)),
        })
        .collect()
}

/// The encoded product messages of Steep, its Steam entry and its add-ons.
pub fn entries(cloud_saves: bool, achievements_file: Option<&str>) -> Vec<Vec<u8>> {
    entries_of(cloud_saves, achievements_file, None)
}

/// [`entries`], `only` those of these product ids (a game-data pack's list), in this table's order.
pub fn entries_of(cloud_saves: bool, achievements_file: Option<&str>, only: Option<&BTreeSet<u64>>) -> Vec<Vec<u8>> {
    PRODUCTS
        .iter()
        .map(|table| to_fields(table))
        .filter(|fields| only.is_none_or(|only| pb::int(fields, 1).is_some_and(|pid| only.contains(&pid))))
        .map(|fields| {
            let pid = pb::int(&fields, 1).unwrap_or(0);
            let config = yaml(pid, &fields, cloud_saves, achievements_file);
            let fields: Vec<F> = fields
                .into_iter()
                .map(|f| if f.0 == PRODUCT_CONFIG { F(f.0, V::Bytes(config.clone())) } else { f })
                .collect();
            // field 20 = owned. The captured table has it on the games only; the DLC worlds lacked it and
            // showed as "to buy", so add it for them. NOT for ulc/none: force-owning cosmetics/consumables
            // makes the store say "already bought" with nothing usable, and blocks heli tickets.
            let fields = if kind(pid) == "dlc" { pb::replace(&fields, 20, V::Int(1)) } else { fields };
            pb::encode(&fields)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// sha256 of the entries, each with a 4-byte length prefix, as the Python server (products.py) writes them
    /// plus field 20 (owned) on the DLC entries.
    fn framed_sha(entries: Vec<Vec<u8>>) -> String {
        let all: Vec<u8> = entries.iter().flat_map(|e| pb::frame(e)).collect();
        hex::encode(Sha256::digest(all))
    }

    #[test]
    fn entries_match_the_python_server() {
        assert_eq!(PRODUCTS.len(), 62);
        assert_eq!(framed_sha(entries(false, None)),
                   "6daad70d0aab4157b044c3812fcea8ab859179d85e74bb145016e3236dcdf61a");
        assert_eq!(framed_sha(entries(true, Some("achievements.xml"))),
                   "6f20ee604adb131d50564d7050e6b19a019ae444479539b24493be251a90d7ee");
    }

    #[test]
    fn dlc_entries_are_owned_and_the_others_as_captured() {
        for (entry, table) in entries(false, None).iter().zip(PRODUCTS) {
            let fields = pb::decode(entry).unwrap();
            let pid = pb::int(&fields, 1).unwrap();
            let owned = pb::int(&fields, 20);
            let captured = if kind(pid) == "dlc" { Some(1) } else { pb::int(&to_fields(table), 20) };
            assert_eq!(owned, captured, "product {pid}");
        }
    }

    #[test]
    fn steam_entry_configuration() {
        let steam = pb::decode(&entries(true, Some("a.xml"))[1]).unwrap();
        let yaml = String::from_utf8(pb::bytes(&steam, PRODUCT_CONFIG).unwrap().to_vec()).unwrap();
        assert_eq!(yaml, "version: 2.0\r\nroot:\r\n  name: Steep\r\n  start_game:\r\n    steam:\r\n      \
            overlay_required: yes\r\n      overlay_supported: yes\r\n      steam_app_id: 460920\r\n      \
            steam_installation_status_register: HKEY_LOCAL_MACHINE\\SOFTWARE\\Valve\\Steam\\InstallPath\r\n      \
            game_installation_status_register: HKEY_CURRENT_USER\\SOFTWARE\\Valve\\Steam\\Apps\\460920\\Installed\
            \r\n  third_party_platform:\r\n    name: Steam\r\n  installer:\r\n    game_identifier: Steep\r\n  \
            digital_distribution:\r\n    version: 0\r\n  cloud_saves: yes\r\n  uplay:\r\n    achievements: a.xml\r\n    \
            achievements_sync_id: STP\r\n  app_id: b65780e8-0875-4685-995a-6e952cc605a8\r\n  \
            space_id: a20f75db-9175-4df6-b1b3-ba42e291166c\r\nlocalizations: {}\r\n");
    }
}
