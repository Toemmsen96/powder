//! A new player: what someone gets the first time they log in. Like a new Ubisoft account: no items, level 1,
//! plus the `starting_credits` toggle. (A player's own recorded progress can be brought in with
//! `powder-server admin import`.)

use crate::config::Config;
use crate::constants::CREDITS_ITEM;
use crate::repos::Repos;
use anyhow::{Context, Result};

/// Create the player row and initial state. Assumes the player does not exist yet. Default name:
/// "Player-<first 4 of the profile id>".
pub fn create_player(config: &Config, repos: &Repos, profile_id: &str, name: Option<&str>, user_id: Option<&str>)
                     -> Result<()> {
    let profile_id = profile_id.to_lowercase();
    let name = match name.filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("Player-{}", profile_id.chars().take(4).collect::<String>()),
    };
    let start = config.toggles(Some(&profile_id)).starting_credits;
    repos.db.tx(|| {
        repos.create_player(&profile_id, user_id.filter(|u| !u.is_empty()).unwrap_or(&profile_id), &name)?;
        if start != 0 {
            repos.set_inventory(&profile_id, CREDITS_ITEM, start, None, None)?;
            repos.add_ledger(&profile_id, CREDITS_ITEM, start, start, "seed", None)?;
        }
        Ok(())
    }).with_context(|| format!("creating player {profile_id}"))?;
    log::info!(target: "powder.seed", "created new player {name} ({profile_id}) with {start} credits");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::sync::Arc;

    #[test]
    fn new_player_gets_starting_credits() {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        let repos = Repos::new(db);
        let mut config = Config::default();
        config.gameplay.starting_credits = 500;
        create_player(&config, &repos, "ABCDEF01-0000", None, None).unwrap();
        let p = repos.player("abcdef01-0000").unwrap().unwrap();
        assert_eq!((p.name.as_str(), p.user_id.as_str()), ("Player-abcd", "abcdef01-0000"));
        assert_eq!(repos.quantity(&p.profile_id, CREDITS_ITEM).unwrap(), 500);
        assert_eq!(repos.recent_ledger(&p.profile_id, 5).unwrap()[0].reason, "seed");
    }
}
