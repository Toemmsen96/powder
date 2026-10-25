//! NEX method handlers, one module per protocol, each with a `register` that adds its methods to the Dispatcher.
//!
//! A handler is `fn(&HandlerDeps, &mut Ctx, &mut Reader) -> Result<outputs, HandlerError>`; HandlerError::Fallthrough
//! lets the recording answer. Ported so far: login, gamestorage, stats, profile (the Python server's matchmaking
//! handlers wait for their service).

pub mod gamestorage;
pub mod login;
pub mod profile;
pub mod stats;

use super::dispatch::{Ctx, Dispatcher, HandlerError};
use super::rmc::Reader;
use crate::config::Config;
use crate::core::sessions::SessionRegistry;
use crate::mods::ModHost;
use crate::repos::Repos;
use crate::services::accounts::AccountService;
use crate::services::Services;
use parking_lot::Mutex;
use std::sync::Arc;

/// What the handlers need besides the call.
pub struct HandlerDeps {
    /// Everything else (stats, ...).
    pub services: Arc<Services>,
    pub config: Arc<Config>,
    pub accounts: Arc<AccountService>,
    pub sessions: Arc<SessionRegistry>,
    pub repos: Arc<Repos>,
    /// For the "nex_connected" event.
    pub mods: Option<Arc<Mutex<ModHost>>>,
}

impl HandlerDeps {
    pub fn new(services: Arc<Services>) -> Arc<HandlerDeps> {
        Arc::new(HandlerDeps { config: services.config.clone(), accounts: services.accounts.clone(),
                               sessions: services.sessions.clone(), repos: services.repos.clone(),
                               mods: services.mods.clone(), services })
    }
}

pub type Result = std::result::Result<Vec<u8>, HandlerError>;
pub type HandlerFn = fn(&HandlerDeps, &mut Ctx, &mut Reader) -> Result;

/// Add every ported handler to the dispatcher.
pub fn register(d: &mut Dispatcher, deps: Arc<HandlerDeps>) {
    for (method, f) in login::HANDLERS.iter().chain(gamestorage::HANDLERS).chain(stats::HANDLERS)
        .chain(profile::HANDLERS) {
        let deps = deps.clone();
        d.handle(method, Box::new(move |ctx, r| f(&deps, ctx, r)));
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::assets::Assets;

    pub fn deps(config: Config) -> Arc<HandlerDeps> {
        HandlerDeps::new(Arc::new(Services::memory(config)))
    }

    pub fn dispatcher(deps: &Arc<HandlerDeps>) -> Dispatcher {
        let mut d = Dispatcher::new(Arc::new(Assets::default()), None);
        register(&mut d, deps.clone());
        d
    }
}
