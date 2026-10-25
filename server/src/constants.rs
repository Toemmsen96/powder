//! Fixed identifiers of Steep's online services (the same for every player), and the Ubisoft hostnames the
//! game and Ubisoft Connect must reach on this server.

pub const APP_ID: &str = "b65780e8-0875-4685-995a-6e952cc605a8"; // Ubi-AppId the game sends
pub const SPACE_ID: &str = "a20f75db-9175-4df6-b1b3-ba42e291166c"; // Steep's UbiServices space
pub const SANDBOX: &str = "STEEP_PC_TU7_A";
pub const CONNECT_APP_ID: &str = "f68a4bb5-608a-4ff2-8123-be8ef797e0a6"; // Ubisoft Connect (PC client)
pub const CONNECT_SPACE_ID: &str = "abda351c-f251-4a70-8fb1-32bd42089262"; // its UbiServices space

/// Inventory item that holds the player's credits (the in-game currency).
pub const CREDITS_ITEM: &str = "e3570d44-da4d-40bd-980e-5896bc539d08";

/// The hostnames that must all resolve to the server's address (hosts file or a DNS server); the launcher
/// checks exactly this list.
pub const GAME_HOSTS: &[&str] = &[
    "public-ubiservices.ubi.com",
    "msr-public-ubiservices.ubi.com",
    "public-im-ubiservices.ubi.com",
    "public-ws-ubiservices.ubi.com",
    "steep-prod-pc.steep.olympus.ubi.com",
];
pub const CONNECT_HOSTS: &[&str] = &[
    "dmx.upc.ubisoft.com",         // demux: Connect's login / owned games / play sessions
    "channel-service.upc.ubi.com",
    "static3.cdn.ubi.com",         // Connect's internet check
];

pub fn redirect_hosts() -> Vec<&'static str> {
    GAME_HOSTS.iter().chain(CONNECT_HOSTS).copied().collect()
}

pub const DEMUX_HOST: &str = "dmx.upc.ubisoft.com";
pub const CRL_HOST: &str = "public-ubiservices.ubi.com"; // serves the local CA's CRL over plain HTTP (port 80)

/// What the server certificate covers: the redirected names plus wildcards for hosts not redirected yet
/// but answered if a user does.
pub fn cert_names() -> Vec<&'static str> {
    let mut names = redirect_hosts();
    names.extend(["*.ubiservices.ubi.com", "*.olympus.ubi.com", "*.upc.ubi.com", "*.upc.ubisoft.com",
                  "*.cdn.ubi.com", "*.ubi.com", "*.ubisoft.com"]);
    names
}

/// Powder's version: the release build sets POWDER_VERSION from the tag.
pub fn version() -> &'static str {
    option_env!("POWDER_VERSION").unwrap_or("0.0.0+dev")
}
