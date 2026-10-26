//! Game logic over the repositories: bans and the whitelist (access), accounts and logins (accounts), Connect's
//! chat channels (channels), cloud saves (cloudsaves), inventory and store (economy), profile entities (entities),
//! friends (friends), challenge leaderboards (leaderboards), multiplayer sessions (matchmaking), new players
//! (seed), player statistics (stats).
//!
//! [`Services`] is one server's set of them (the Python server's `Services`): the HTTPS routes, the NEX handlers,
//! the demux and the admin CLI share it.

pub mod access;
pub mod accounts;
pub mod channels;
pub mod cloudsaves;
pub mod economy;
pub mod entities;
pub mod friends;
pub mod importer;
pub mod leaderboards;
pub mod matchmaking;
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
use std::time::Duration;

/// Matchmaking notifications go out this much later, so they arrive after the reply to the call that caused them
/// (as with Ubisoft's server).
pub const MATCHMAKING_PUSH_DELAY: Duration = Duration::from_millis(300);

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
    pub matchmaking: Arc<matchmaking::MatchmakingService>,
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
        let push = sessions.clone();
        let matchmaking = matchmaking::MatchmakingService::new(Arc::new(move |pid: &str, code, params| {
            let (sessions, pid) = (push.clone(), pid.to_string());
            let send = move || {
                let event = crate::nex::rmc::notification_event("Matchmaking", code, params);
                if !sessions.push_nex(&pid, "NotificationProtocol", crate::nex::rmc::NOTIFICATION_METHOD, &event, Some(0)) {
                    log::warn!(target: "powder", "matchmaking: {pid} has no game connected, code {code} not delivered");
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => drop(rt.spawn(async move {
                    tokio::time::sleep(MATCHMAKING_PUSH_DELAY).await;
                    send();
                })),
                Err(_) => send(), // no runtime (admin CLI, tests): send now
            }
        }));
        Services {
            economy: Arc::new(economy::EconomyService::new(config.clone(), repos.clone(), assets.clone())),
            stats: Arc::new(stats::StatsService::new(repos.clone())),
            entities: Arc::new(entities::EntityService::new(repos.clone())),
            cloudsaves: Arc::new(cloudsaves::CloudSaveService::new(repos.clone(), config.data_dir())),
            channels: Arc::new(channels),
            matchmaking: Arc::new(matchmaking),
            connect_clients: Arc::new(ConnectClients::new()),
            login_failures: Arc::new(FailureLimiter::default()),
            config, assets, repos, sessions, accounts, mods,
        }
    }

    /// Kick whoever is online but no longer allowed, and the kicks the admin CLI asked for (see
    /// access::AccessService::sweep); -> the profile ids kicked.
    pub fn enforce_access(&self) -> anyhow::Result<Vec<String>> {
        let connect: Vec<(String, String)> = self.connect_clients.snapshot().into_iter()
            .filter_map(|c| {
                let ip = c.peer.rsplit_once(':').map(|(ip, _)| ip.to_string()).unwrap_or_default();
                Some((c.profile_id?, ip))
            }).collect();
        self.accounts.access.sweep(&connect)
    }

    /// Over an in-memory database, without a pack or mods (tests).
    #[cfg(test)]
    pub fn memory(config: Config) -> Services {
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        Services::new(Arc::new(config), Arc::new(Assets::default()), Arc::new(Repos::new(db)), None)
    }
}

// ---- admin: reset, the database file ------------------------------------------------------------------------------
impl Services {
    pub const DB_NAME: &'static str = "powder.db";
    /// The database's name before the rename to Powder.
    pub const LEGACY_DB_NAME: &'static str = "steep.db";

    /// data/powder.db; if only a database from before the rename exists, move it (and its WAL files) there.
    pub fn move_legacy_db(data_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
        use anyhow::Context;
        let (db, old) = (data_dir.join(Self::DB_NAME), data_dir.join(Self::LEGACY_DB_NAME));
        if !db.exists() && old.exists() {
            for suffix in ["", "-wal", "-shm"] {
                let src = data_dir.join(format!("{}{suffix}", Self::LEGACY_DB_NAME));
                let dst = data_dir.join(format!("{}{suffix}", Self::DB_NAME));
                if src.exists() {
                    std::fs::rename(&src, &dst)
                        .with_context(|| format!("moving {} to {}", src.display(), dst.display()))?;
                }
            }
            log::info!(target: "powder", "moved the database {} to {}", old.display(), db.display());
        }
        Ok(db)
    }

    /// Wipe a player; they start again as a new player on their next login. An account that owns the player keeps
    /// it (same profile id and name; deleting the row would delete the account too).
    pub fn reset(&self, profile_id: &str) -> anyhow::Result<()> {
        let repos = &self.repos;
        repos.db.tx(|| {
            let account = repos.account_by_profile(profile_id)?;
            let player = repos.player(profile_id)?;
            repos.delete_player(profile_id)?;
            repos.clear_entities(profile_id)?;
            repos.clear_achievements(profile_id)?;
            if let (Some(account), Some(player)) = (account, player) {
                seed::create_player(&self.config, repos, profile_id, Some(&player.name), None)?;
                repos.create_account(&account.account_id, &account.email, &account.password_hash, profile_id)?;
            }
            Ok(())
        })
    }
}
