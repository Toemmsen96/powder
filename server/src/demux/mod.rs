//! Ubisoft Connect's demux protocol (dmx.upc.ubisoft.com:443): Connect's login, owned games, play sessions.
//! Without it Connect stays in offline mode and tells the game it is offline.
//!
//! The TLS listener is shared with HTTPS (https.rs routes by the name the client asked for); [`run`] serves one
//! connection once it is handed over.
//!
//!   connection.rs  the outer stream: login, service connections, patch info, pushes
//!   services.rs    messages inside a service connection: request styles, the handler table
//!   ownership.rs   owned games (ownership_service initialize)
//!   generated.rs   the other replies the server builds (friends, cloud saves, tokens, achievements, ...)

pub mod connection;
pub mod generated;
pub mod ownership;
pub mod pb;
pub mod services;
#[cfg(test)]
mod tests;

pub use connection::run;

use crate::assets::Assets;
use crate::config::Config;
use crate::core::sessions::SessionRegistry;
use crate::mods::ModHost;
use crate::repos::Repos;
use crate::services::accounts::AccountService;
use crate::services::cloudsaves::CloudSaveService;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// What a demux connection needs; one per server, shared by all connections.
pub struct DemuxDeps {
    pub config: Arc<Config>,
    /// `assets.products`: Connect's recorded demux replies (owned games, patch info) from the game-data pack.
    pub assets: Arc<Assets>,
    /// Tickets -> sessions (and through it the repositories and the session registry).
    pub accounts: Arc<AccountService>,
    /// The upload/download links handed out here are redeemed by the HTTPS side: share one instance.
    pub cloudsaves: Arc<CloudSaveService>,
    /// Live Connect clients: shared with whoever shows or uses them (/_powder/status, friends' online state).
    pub clients: Arc<ConnectClients>,
    /// `connect_product_started` / `connect_product_ended` events go to the mods.
    pub mods: Option<Arc<Mutex<ModHost>>>,
}

impl DemuxDeps {
    pub fn repos(&self) -> &Arc<Repos> {
        &self.accounts.repos
    }

    pub fn sessions(&self) -> &Arc<SessionRegistry> {
        &self.accounts.sessions
    }
}

/// One live Ubisoft Connect demux connection, as others see it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectClient {
    /// "ip:port"
    pub peer: String,
    /// Set once Connect signed in (and the ticket was accepted).
    pub profile_id: Option<String>,
    pub player_name: Option<String>,
    /// Names of the service connections open now (sorted, unique).
    pub services: Vec<String>,
}

/// The live Connect clients (Python: services.connect_clients) and the Connect version each PC reported last
/// (Python: services.connect_versions).
#[derive(Default)]
pub struct ConnectClients {
    next: AtomicU64,
    live: Mutex<BTreeMap<u64, ConnectClient>>,
    versions: Mutex<HashMap<String, u64>>,
}

impl ConnectClients {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection; it is forgotten when the returned guard drops.
    pub fn add(self: &Arc<Self>, peer: &str) -> ClientGuard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.live.lock().insert(id, ConnectClient { peer: peer.to_string(), ..Default::default() });
        ClientGuard { clients: self.clone(), id }
    }

    /// Every live connection, oldest first.
    pub fn snapshot(&self) -> Vec<ConnectClient> {
        self.live.lock().values().cloned().collect()
    }

    /// Profile ids of the signed-in Connect clients.
    pub fn profile_ids(&self) -> Vec<String> {
        self.live.lock().values().filter_map(|c| c.profile_id.clone()).collect()
    }

    /// The Connect version this address reported last (this run).
    pub fn version_from(&self, client_ip: &str) -> Option<u64> {
        self.versions.lock().get(client_ip).copied()
    }

    pub fn set_version(&self, client_ip: &str, version: u64) {
        self.versions.lock().insert(client_ip.to_string(), version);
    }
}

/// A connection's entry in [`ConnectClients`]; removes it on drop (also when the task is cancelled).
pub struct ClientGuard {
    clients: Arc<ConnectClients>,
    id: u64,
}

impl ClientGuard {
    pub fn update(&self, f: impl FnOnce(&mut ConnectClient)) {
        if let Some(c) = self.clients.live.lock().get_mut(&self.id) {
            f(c);
        }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.clients.live.lock().remove(&self.id);
    }
}
