//! Game logic over the repositories: accounts and logins (accounts), Connect's chat channels (channels), cloud
//! saves (cloudsaves), inventory and store (economy), profile entities (entities), friends (friends), challenge
//! leaderboards (leaderboards), new players (seed), player statistics (stats).
//!
//! [`Services`] is one server's set of them (the Python server's `Services`): the HTTPS routes, the NEX handlers,
//! the demux and the admin CLI share it.

pub mod accounts;
pub mod channels;
pub mod cloudsaves;
pub mod economy;
pub mod entities;
pub mod friends;
pub mod leaderboards;
pub mod seed;
pub mod stats;

use crate::assets::Assets;
use accounts::AccountService;
use crate::config::Config;
use crate::core::ratelimit::FailureLimiter;
use crate::core::sessions::SessionRegistry;
use crate::demux::ConnectClients;
use crate::mods::ModHost;
use crate::repos::Repos;
use parking_lot::Mutex;
use serde_json::Map;
use std::sync::Arc;

pub struct Services {
    pub config: Arc<Config>,
    pub assets: Arc<Assets>,
    pub repos: Arc<Repos>,
    /// One registry for every transport: HTTPS logins issue the tickets NEX's LoginWithToken looks up.
    pub sessions: Arc<SessionRegistry>,
    pub accounts: Arc<AccountService>,
    pub economy: Arc<economy::EconomyService>,
    pub stats: Arc<stats::StatsService>,
    pub entities: Arc<entities::EntityService>,
    /// Connect's demux hands out cloud-save links the HTTPS routes redeem: one instance for both.
    pub cloudsaves: Arc<cloudsaves::CloudSaveService>,
    pub channels: Arc<channels::ChannelService>,
    /// Live Ubisoft Connect demux connections (/_powder/status, friends' online state).
    pub connect_clients: Arc<ConnectClients>,
    /// Failed sign-ins per client address: UbiServices' and the account API's.
    pub login_failures: Arc<FailureLimiter>,
    pub mods: Option<Arc<Mutex<ModHost>>>,
}

impl Services {
    pub fn new(config: Arc<Config>, assets: Arc<Assets>, repos: Arc<Repos>, mods: Option<Arc<Mutex<ModHost>>>)
               -> Services {
        let sessions = Arc::new(SessionRegistry::new());
        let accounts = Arc::new(AccountService::new(config.clone(), repos.clone(), sessions.clone()));
        let push = sessions.clone();
        let channels = channels::ChannelService::new(Arc::new(move |pid: &str, kind: &str, content| {
            if push.push_ws(pid, kind, content, Map::new()) == 0 {
                log::info!(target: "powder", "{kind}: {pid} has no websocket open, not delivered");
            }
        }));
        Services {
            economy: Arc::new(economy::EconomyService::new(config.clone(), repos.clone(), assets.clone())),
            stats: Arc::new(stats::StatsService::new(repos.clone())),
            entities: Arc::new(entities::EntityService::new(repos.clone())),
            cloudsaves: Arc::new(cloudsaves::CloudSaveService::new(repos.clone(), config.data_dir())),
            channels: Arc::new(channels),
            connect_clients: Arc::new(ConnectClients::new()),
            login_failures: Arc::new(FailureLimiter::default()),
            config, assets, repos, sessions, accounts, mods,
        }
    }

    /// Over an in-memory database, without a pack or mods (tests).
    #[cfg(test)]
    pub fn memory(config: Config) -> Services {
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        Services::new(Arc::new(config), Arc::new(Assets::default()), Arc::new(Repos::new(db)), None)
    }
}
