//! Friends: on a Powder server everyone is everyone's friend, so players can see and invite each other.
//!
//! A player's friends are the other players with an account (on a server without accounts yet: every other
//! player). Ubisoft Connect gets them in its friends list (demux friends_service and its web calls) and Steep in
//! its own (UbiServices /v2/profiles/me/friends). Online: Ubisoft Connect or the game is connected now.

use crate::config::Config;
use crate::core::sessions::SessionRegistry;
use crate::demux::ConnectClients;
use crate::repos::Repos;
use anyhow::Result;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Friend {
    pub profile_id: String,
    pub name: String,
    pub user_id: String,
}

/// Everyone but `profile_id`, by name.
pub fn friends_of(repos: &Repos, profile_id: Option<&str>) -> Result<Vec<Friend>> {
    let accounts = repos.accounts()?;
    let mut players = repos.players()?;
    if !accounts.is_empty() {
        let pids: BTreeSet<&str> = accounts.iter().map(|a| a.profile_id.as_str()).collect();
        players.retain(|p| pids.contains(p.profile_id.as_str()));
    }
    let me = profile_id.unwrap_or("").to_lowercase();
    let mut out: Vec<Friend> = players.into_iter().filter(|p| p.profile_id != me)
        .map(|p| Friend { profile_id: p.profile_id, name: p.name, user_id: p.user_id }).collect();
    out.sort_by_key(|f| f.name.to_lowercase());
    Ok(out)
}

/// Profile ids whose game is connected to the game server (NEX) right now, plus the players [multiplayer]
/// pretend_online names (testing invites alone).
pub fn in_game(config: &Config, repos: &Repos, sessions: &SessionRegistry) -> Result<BTreeSet<String>> {
    let mut now: BTreeSet<String> = sessions.players().iter().filter(|s| s.channels.nex().is_some())
        .map(|s| s.profile_id().to_lowercase()).collect();
    let pretend: BTreeSet<String> = config.multiplayer.pretend_online.iter().map(|n| n.to_lowercase()).collect();
    if !pretend.is_empty() {
        now.extend(repos.players()?.into_iter().filter(|p| pretend.contains(&p.name.to_lowercase()))
            .map(|p| p.profile_id));
    }
    Ok(now)
}

/// Profile ids whose Ubisoft Connect (demux) or game (NEX) is connected right now.
pub fn online_profiles(config: &Config, repos: &Repos, sessions: &SessionRegistry, connect: &ConnectClients)
                       -> Result<BTreeSet<String>> {
    let mut out: BTreeSet<String> = connect.profile_ids().iter().map(|p| p.to_lowercase()).collect();
    out.extend(in_game(config, repos, sessions)?);
    Ok(out)
}
