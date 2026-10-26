//! Who may play on the server: bans (of players and of addresses), the whitelist, and kicks.
//!
//! The rules live in the database (`access_rules`, migration 0006), so the admin CLI (another process) and the
//! admin page change the same list. The server keeps a copy and reads the table again at most every
//! [`RELOAD_S`]; [`AccessService::sweep`] (every few seconds, from main) also carries out the kicks the CLI asked
//! for and kicks whoever is online but no longer allowed (a new ban, the whitelist switched on).
//!
//! - A banned player (or anyone from a banned address) can't sign in, register, or use a ticket they already
//!   have; a ban may expire.
//! - While the whitelist is on (kv access/whitelist), only its players and the server's admins get in.
//! - A kick ends the player's sessions and closes their game's, websockets' and Ubisoft Connect's connections;
//!   unless they are banned, they can sign in again at once.
//!
//! Admins ([server] admins) are never kept out by the whitelist or an address ban, and can't be banned: the
//! host would lock themselves out of the admin page.

use crate::config::Config;
use crate::core::sessions::SessionRegistry;
use crate::repos::{truthy, AccessRow, Repos};
use crate::util::unix_now;
use anyhow::Result;
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;

pub const BAN_PLAYER: &str = "ban_player";
pub const BAN_IP: &str = "ban_ip";
pub const ALLOW_PLAYER: &str = "allow_player";
/// The server's copy of the rules is this old at most (the CLI changes the database from outside).
pub const RELOAD_S: f64 = 2.0;
pub const MAX_REASON: usize = 200;
const KV_SCOPE: &str = "access";
const KV_WHITELIST: &str = "whitelist";

// ---- addresses -----------------------------------------------------------------------------------------------
/// An address or a range of them ("192.0.2.7", "192.0.2.0/24", "2001:db8::/32"). IPv4-mapped IPv6 addresses
/// count as IPv4.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

fn bits(ip: IpAddr) -> (u128, u8) {
    match ip {
        IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
        IpAddr::V6(v6) => (u128::from(v6), 128),
    }
}

fn mask(width: u8, prefix: u8) -> u128 {
    if prefix == 0 { 0 } else { (u128::MAX << (width - prefix)) & (u128::MAX >> (128 - width)) }
}

impl Net {
    pub fn parse(text: &str) -> Option<Net> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p.parse::<u8>().ok()?)),
            None => (text, None),
        };
        let addr = plain(addr.parse::<IpAddr>().ok()?);
        let (value, width) = bits(addr);
        let prefix = prefix.unwrap_or(width);
        if prefix > width {
            return None;
        }
        let value = value & mask(width, prefix);
        let addr = match addr {
            IpAddr::V4(_) => IpAddr::V4((value as u32).into()),
            IpAddr::V6(_) => IpAddr::V6(value.into()),
        };
        Some(Net { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = plain(ip);
        let ((net, width), (value, ip_width)) = (bits(self.addr), bits(ip));
        width == ip_width && value & mask(width, self.prefix) == net
    }
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.prefix == bits(self.addr).1 { write!(f, "{}", self.addr) } else { write!(f, "{}/{}", self.addr, self.prefix) }
    }
}

// ---- durations, texts ----------------------------------------------------------------------------------------
/// "30m", "12h", "7d", "2w", "90s" -> Some(Some(seconds)); "", "permanent", "forever" -> Some(None); else None.
pub fn parse_duration(text: &str) -> Option<Option<i64>> {
    let text = text.trim().to_lowercase();
    if text.is_empty() || text == "permanent" || text == "forever" {
        return Some(None);
    }
    let unit = text.chars().last()?;
    let n: i64 = text[..text.len() - unit.len_utf8()].trim().parse().ok().filter(|n| *n > 0)?;
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        'w' => 7 * 86400,
        _ => return None,
    };
    Some(Some(n.checked_mul(scale)?))
}

/// "2026-11-02 14:00 UTC"
pub fn format_time(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| unix.to_string())
}

/// A reason as stored: one line, no control characters, at most MAX_REASON characters.
pub fn clean_reason(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).take(MAX_REASON).collect::<String>().trim().to_string()
}

// ---- refusals --------------------------------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    Banned { reason: String, expires: Option<i64> },
    AddressBanned { reason: String, expires: Option<i64> },
    NotWhitelisted,
}

impl Refusal {
    /// For the player (the launcher shows it; Ubisoft Connect only says the sign-in failed).
    pub fn message(&self) -> String {
        let ban = |what: &str, reason: &str, expires: &Option<i64>| {
            let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
            let until = expires.map(|t| format!(" (until {})", format_time(t))).unwrap_or_default();
            format!("{what} banned from this server{until}{reason}")
        };
        match self {
            Refusal::Banned { reason, expires } => ban("You are", reason, expires),
            Refusal::AddressBanned { reason, expires } => ban("Your address is", reason, expires),
            Refusal::NotWhitelisted => "This server only lets in the players on its whitelist; ask its host to add you."
                .into(),
        }
    }
}

// ---- the service -----------------------------------------------------------------------------------------------
#[derive(Default)]
struct Rules {
    loaded: f64,
    whitelist: bool,
    banned: HashMap<String, AccessRow>,
    nets: Vec<(Net, AccessRow)>,
    allowed: HashSet<String>,
}

pub struct AccessService {
    config: Arc<Config>,
    repos: Arc<Repos>,
    sessions: Arc<SessionRegistry>,
    rules: RwLock<Option<Arc<Rules>>>,
}

fn live(row: &AccessRow, now: i64) -> bool {
    row.expires.is_none_or(|t| t > now)
}

impl AccessService {
    pub fn new(config: Arc<Config>, repos: Arc<Repos>, sessions: Arc<SessionRegistry>) -> Self {
        Self { config, repos, sessions, rules: RwLock::new(None) }
    }

    fn load(&self) -> Result<Rules> {
        let now = unix_now() as i64;
        let mut rules = Rules { loaded: unix_now(), ..Default::default() };
        rules.whitelist = self.repos.kv_get(KV_SCOPE, KV_WHITELIST)?.as_ref().is_some_and(truthy);
        for row in self.repos.access_rules()?.into_iter().filter(|r| live(r, now)) {
            match row.kind.as_str() {
                BAN_PLAYER => drop(rules.banned.insert(row.value.to_lowercase(), row)),
                BAN_IP => match Net::parse(&row.value) {
                    Some(net) => rules.nets.push((net, row)),
                    None => log::warn!(target: "powder.access", "ignoring the address ban {:?}: not an address",
                                       row.value),
                },
                ALLOW_PLAYER => drop(rules.allowed.insert(row.value.to_lowercase())),
                _ => {}
            }
        }
        Ok(rules)
    }

    /// The rules, read again if the copy is older than RELOAD_S.
    fn rules(&self) -> Result<Arc<Rules>> {
        if let Some(r) = self.rules.read().as_ref().filter(|r| unix_now() - r.loaded < RELOAD_S) {
            return Ok(r.clone());
        }
        let fresh = Arc::new(self.load()?);
        *self.rules.write() = Some(fresh.clone());
        Ok(fresh)
    }

    /// Read the database again at the next check (after a change).
    fn invalidate(&self) {
        *self.rules.write() = None;
    }

    pub fn is_admin(&self, profile_id: &str) -> bool {
        self.config.server.admins.iter().any(|a| a.trim().eq_ignore_ascii_case(profile_id.trim()))
    }

    fn address_ban(rules: &Rules, client_ip: &str) -> Option<Refusal> {
        let ip = client_ip.trim().parse::<IpAddr>().ok()?;
        rules.nets.iter().find(|(net, _)| net.contains(ip))
            .map(|(_, r)| Refusal::AddressBanned { reason: r.reason.clone(), expires: r.expires })
    }

    /// Why this player, from this address, may not play; None: they may.
    pub fn check(&self, profile_id: &str, client_ip: &str) -> Result<Option<Refusal>> {
        let rules = self.rules()?;
        let pid = profile_id.trim().to_lowercase();
        if let Some(r) = rules.banned.get(&pid) {
            return Ok(Some(Refusal::Banned { reason: r.reason.clone(), expires: r.expires }));
        }
        if self.is_admin(&pid) {
            return Ok(None);
        }
        if let Some(refusal) = Self::address_ban(&rules, client_ip) {
            return Ok(Some(refusal));
        }
        Ok((rules.whitelist && !rules.allowed.contains(&pid)).then_some(Refusal::NotWhitelisted))
    }

    /// Before we know who it is (a registration): only address bans.
    pub fn check_address(&self, client_ip: &str) -> Result<Option<Refusal>> {
        let rules = self.rules()?;
        Ok(Self::address_ban(&rules, client_ip))
    }

    // ---- changes ---------------------------------------------------------------------------------------------
    /// Ok(Err(why)): refused (an admin).
    pub fn ban_player(&self, profile_id: &str, reason: &str, seconds: Option<i64>, by: &str)
                      -> Result<std::result::Result<(), String>> {
        let pid = profile_id.trim().to_lowercase();
        if self.is_admin(&pid) {
            return Ok(Err("an admin can't be banned (take them out of [server] admins first)".into()));
        }
        let expires = seconds.map(|s| unix_now() as i64 + s);
        self.repos.set_access_rule(BAN_PLAYER, &pid, &clean_reason(reason), expires, by)?;
        self.invalidate();
        log::warn!(target: "powder.access", "{by} banned player {pid}{}", expires.map(|t| format!(" until {}", format_time(t)))
                   .unwrap_or_default());
        Ok(Ok(()))
    }

    pub fn unban_player(&self, profile_id: &str, by: &str) -> Result<bool> {
        let pid = profile_id.trim().to_lowercase();
        let had = self.repos.delete_access_rule(BAN_PLAYER, &pid)?;
        self.invalidate();
        if had {
            log::info!(target: "powder.access", "{by} lifted the ban of player {pid}");
        }
        Ok(had)
    }

    /// -> the range as stored ("192.0.2.0/24"), or Err(why).
    pub fn ban_address(&self, address: &str, reason: &str, seconds: Option<i64>, by: &str)
                       -> Result<std::result::Result<String, String>> {
        let Some(net) = Net::parse(address) else {
            return Ok(Err(format!("not an address or range: {address:?}")));
        };
        if net.prefix == 0 {
            return Ok(Err("that range is every address".into()));
        }
        let expires = seconds.map(|s| unix_now() as i64 + s);
        let value = net.to_string();
        self.repos.set_access_rule(BAN_IP, &value, &clean_reason(reason), expires, by)?;
        self.invalidate();
        log::warn!(target: "powder.access", "{by} banned the address {value}{}", expires
                   .map(|t| format!(" until {}", format_time(t))).unwrap_or_default());
        Ok(Ok(value))
    }

    pub fn unban_address(&self, address: &str, by: &str) -> Result<std::result::Result<bool, String>> {
        let Some(net) = Net::parse(address) else {
            return Ok(Err(format!("not an address or range: {address:?}")));
        };
        let had = self.repos.delete_access_rule(BAN_IP, &net.to_string())?;
        self.invalidate();
        if had {
            log::info!(target: "powder.access", "{by} lifted the ban of the address {net}");
        }
        Ok(Ok(had))
    }

    pub fn whitelist_enabled(&self) -> Result<bool> {
        Ok(self.rules()?.whitelist)
    }

    pub fn set_whitelist(&self, on: bool, by: &str) -> Result<()> {
        self.repos.kv_set(KV_SCOPE, KV_WHITELIST, &json!(on))?;
        self.invalidate();
        log::warn!(target: "powder.access", "{by} switched the whitelist {}", if on { "on" } else { "off" });
        Ok(())
    }

    pub fn allow(&self, profile_id: &str, by: &str) -> Result<()> {
        self.repos.set_access_rule(ALLOW_PLAYER, &profile_id.trim().to_lowercase(), "", None, by)?;
        self.invalidate();
        Ok(())
    }

    pub fn disallow(&self, profile_id: &str) -> Result<bool> {
        let had = self.repos.delete_access_rule(ALLOW_PLAYER, &profile_id.trim().to_lowercase())?;
        self.invalidate();
        Ok(had)
    }

    /// The rules in force (expired ones left out), oldest first.
    pub fn list(&self) -> Result<Vec<AccessRow>> {
        let now = unix_now() as i64;
        Ok(self.repos.access_rules()?.into_iter().filter(|r| live(r, now)).collect())
    }

    /// {whitelist, rules: [{kind, value, name, reason, created_at, expires, by}]} (name: the player's, if any).
    pub fn summary(&self) -> Result<Value> {
        let mut rules = vec![];
        for r in self.list()? {
            let name = match r.kind.as_str() {
                BAN_IP => None,
                _ => self.repos.player(&r.value)?.map(|p| p.name),
            };
            rules.push(json!({"kind": r.kind, "value": r.value, "name": name, "reason": r.reason,
                              "created_at": r.created_at, "expires": r.expires, "by": r.by}));
        }
        Ok(json!({"whitelist": self.whitelist_enabled()?, "rules": rules}))
    }

    /// A player by profile id, account email or player name (as typed by an admin).
    pub fn find_player(&self, text: &str) -> Result<Option<String>> {
        let text = text.trim();
        if uuid::Uuid::parse_str(text).is_ok() {
            return Ok(self.repos.player(&text.to_lowercase())?.map(|p| p.profile_id));
        }
        if text.contains('@') {
            return Ok(self.repos.account_by_email(text)?.map(|a| a.profile_id));
        }
        Ok(self.repos.player_by_name(text)?.map(|p| p.profile_id))
    }

    // ---- kicks -----------------------------------------------------------------------------------------------
    /// End the player's sessions and connections now (this process); -> whether they had any.
    pub fn kick(&self, profile_id: &str, why: &str) -> bool {
        let pid = profile_id.trim().to_lowercase();
        let had = self.sessions.kick(&pid);
        if had {
            log::warn!(target: "powder.access", "kicked {pid} ({why})");
        }
        had
    }

    /// From another process (the admin CLI): the running server kicks at its next sweep.
    pub fn request_kick(&self, profile_id: &str, reason: &str) -> Result<()> {
        self.repos.add_kick_request(profile_id, &clean_reason(reason))
    }

    /// Run every few seconds by the server: drop expired rules, carry out requested kicks, and kick whoever is
    /// connected but no longer allowed. `connect`: the signed-in Ubisoft Connect clients (profile id, address).
    /// -> the profile ids kicked.
    pub fn sweep(&self, connect: &[(String, String)]) -> Result<Vec<String>> {
        self.invalidate();
        self.repos.delete_expired_access_rules(unix_now() as i64)?;
        let mut kicked = vec![];
        for (pid, reason) in self.repos.take_kick_requests()? {
            let why = if reason.is_empty() { "kick from the admin CLI".to_string() } else { reason };
            if self.kick(&pid, &why) {
                kicked.push(pid);
            }
        }
        let mut online: Vec<(String, String)> = self.sessions.players().iter()
            .map(|s| (s.profile_id().to_lowercase(), s.client_ip.clone())).collect();
        online.extend(connect.iter().map(|(p, ip)| (p.to_lowercase(), ip.clone())));
        for (pid, ip) in online {
            if kicked.contains(&pid) {
                continue;
            }
            let Some(refusal) = self.check(&pid, &ip)? else { continue };
            // a Connect client without a session (it signed in before a restart) is closed by the kick notice
            let from_connect = connect.iter().any(|(p, _)| p.eq_ignore_ascii_case(&pid));
            if self.kick(&pid, &refusal.message()) || from_connect {
                if from_connect {
                    log::warn!(target: "powder.access", "closing {pid}'s Ubisoft Connect ({})", refusal.message());
                }
                kicked.push(pid);
            }
        }
        Ok(kicked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RIDER: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const ADMIN: &str = "aaaaaaaa-0000-4000-8000-000000000001";

    fn service() -> AccessService {
        let mut config = Config::default();
        config.server.admins = vec![ADMIN.to_uppercase()];
        let db = Arc::new(crate::db::Db::memory().unwrap());
        db.migrate().unwrap();
        AccessService::new(Arc::new(config), Arc::new(Repos::new(db)), Arc::new(SessionRegistry::new()))
    }

    #[test]
    fn nets() {
        let n = Net::parse("192.0.2.77/24").unwrap();
        assert_eq!(n.to_string(), "192.0.2.0/24");
        assert!(n.contains("192.0.2.1".parse().unwrap()));
        assert!(n.contains("::ffff:192.0.2.200".parse().unwrap()), "IPv4-mapped counts as IPv4");
        assert!(!n.contains("192.0.3.1".parse().unwrap()));
        assert!(!n.contains("2001:db8::1".parse().unwrap()));
        assert_eq!(Net::parse("::ffff:10.0.0.1").unwrap().to_string(), "10.0.0.1");
        let v6 = Net::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1::5".parse().unwrap()) && !v6.contains("2001:db9::".parse().unwrap()));
        assert_eq!(Net::parse("10.0.0.0/0").unwrap().to_string(), "0.0.0.0/0");
        for bad in ["", "x", "10.0.0.1/33", "10.0.0.1/-1", "::/129", "10.0.0.1:443"] {
            assert_eq!(Net::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn durations_and_reasons() {
        assert_eq!(parse_duration("30m"), Some(Some(1800)));
        assert_eq!(parse_duration(" 7d "), Some(Some(7 * 86400)));
        assert_eq!(parse_duration("2w"), Some(Some(14 * 86400)));
        assert_eq!(parse_duration(""), Some(None));
        assert_eq!(parse_duration("permanent"), Some(None));
        for bad in ["0d", "-1h", "h", "7x", "1.5h", "99999999999999999w"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(clean_reason(" griefing\nlines\u{7} "), "griefing lines");
        assert_eq!(clean_reason(&"x".repeat(500)).len(), MAX_REASON);
    }

    #[test]
    fn bans_whitelist_and_admins() {
        let a = service();
        assert_eq!(a.check(RIDER, "192.0.2.5").unwrap(), None);
        a.ban_player(&RIDER.to_uppercase(), "griefing", Some(3600), "cli").unwrap().unwrap();
        let Some(Refusal::Banned { reason, expires: Some(_) }) = a.check(RIDER, "192.0.2.5").unwrap() else {
            panic!("banned")
        };
        assert_eq!(reason, "griefing");
        assert!(a.check(RIDER, "").unwrap().unwrap().message().starts_with("You are banned from this server (until "));
        assert!(a.ban_player(ADMIN, "", None, "cli").unwrap().is_err(), "admins can't be banned");
        assert!(a.unban_player(RIDER, "cli").unwrap());
        assert!(!a.unban_player(RIDER, "cli").unwrap());
        assert_eq!(a.check(RIDER, "192.0.2.5").unwrap(), None);

        assert_eq!(a.ban_address("192.0.2.9/24", "", None, "cli").unwrap().unwrap(), "192.0.2.0/24");
        assert!(a.ban_address("0.0.0.0/0", "", None, "cli").unwrap().is_err());
        assert!(matches!(a.check(RIDER, "192.0.2.5").unwrap(), Some(Refusal::AddressBanned { .. })));
        assert!(matches!(a.check_address("::ffff:192.0.2.5").unwrap(), Some(Refusal::AddressBanned { .. })));
        assert_eq!(a.check(ADMIN, "192.0.2.5").unwrap(), None, "admins pass address bans");
        assert_eq!(a.check(RIDER, "198.51.100.1").unwrap(), None);
        assert_eq!(a.unban_address("192.0.2.1/24", "cli").unwrap(), Ok(true));

        a.set_whitelist(true, "cli").unwrap();
        assert_eq!(a.check(RIDER, "").unwrap(), Some(Refusal::NotWhitelisted));
        assert_eq!(a.check(ADMIN, "").unwrap(), None, "admins pass the whitelist");
        a.allow(RIDER, "cli").unwrap();
        assert_eq!(a.check(RIDER, "").unwrap(), None);
        assert!(a.disallow(RIDER).unwrap());
        assert_eq!(a.check(RIDER, "").unwrap(), Some(Refusal::NotWhitelisted));
        a.set_whitelist(false, "cli").unwrap();
        assert_eq!(a.check(RIDER, "").unwrap(), None);
    }

    #[test]
    fn expired_bans_are_gone() {
        let a = service();
        a.repos.set_access_rule(BAN_PLAYER, RIDER, "", Some(unix_now() as i64 - 1), "cli").unwrap();
        a.invalidate();
        assert_eq!(a.check(RIDER, "").unwrap(), None);
        assert!(a.list().unwrap().is_empty());
        a.sweep(&[]).unwrap();
        assert!(a.repos.access_rules().unwrap().is_empty(), "the sweep deletes them");
    }

    #[test]
    fn changes_from_another_process_are_seen() {
        let a = service();
        assert_eq!(a.check(RIDER, "").unwrap(), None);
        // the CLI writes the table directly; the cached copy is used until it is RELOAD_S old
        a.repos.set_access_rule(BAN_PLAYER, RIDER, "", None, "cli").unwrap();
        assert_eq!(a.check(RIDER, "").unwrap(), None);
        if let Some(r) = a.rules.write().as_mut() {
            Arc::get_mut(r).expect("not shared").loaded -= RELOAD_S;
        }
        assert!(a.check(RIDER, "").unwrap().is_some());
    }

    #[test]
    fn sweep_kicks_requested_and_refused_players() {
        let a = service();
        let player = crate::core::identity::Player { profile_id: RIDER.into(), user_id: RIDER.into(), name: "Rider".into() };
        a.sessions.issue(player.clone(), "192.0.2.5", "");
        a.request_kick(RIDER, "be nice").unwrap();
        assert_eq!(a.sweep(&[]).unwrap(), vec![RIDER.to_string()]);
        assert!(a.sessions.for_player(RIDER).is_none());
        assert!(a.repos.take_kick_requests().unwrap().is_empty(), "done once");
        // back online, then banned: the next sweep kicks them
        a.sessions.issue(player, "192.0.2.5", "");
        assert!(a.sweep(&[]).unwrap().is_empty());
        a.ban_player(RIDER, "", None, "cli").unwrap().unwrap();
        assert_eq!(a.sweep(&[]).unwrap(), vec![RIDER.to_string()]);
        // a Connect client only (no session left): kicked through the registry's kick notice
        let mut kicks = a.sessions.subscribe_kicks();
        assert_eq!(a.sweep(&[(RIDER.into(), "192.0.2.5".into())]).unwrap(), vec![RIDER.to_string()]);
        assert_eq!(kicks.try_recv().unwrap(), RIDER);
    }

    #[test]
    fn find_player() {
        let a = service();
        crate::services::seed::create_player(&a.config, &a.repos, RIDER, Some("Rider"), None).unwrap();
        assert_eq!(a.find_player(&RIDER.to_uppercase()).unwrap().as_deref(), Some(RIDER));
        assert_eq!(a.find_player("Rider").unwrap().as_deref(), Some(RIDER));
        assert_eq!(a.find_player("nobody").unwrap(), None);
        assert_eq!(a.find_player("cccccccc-0000-4000-8000-000000000003").unwrap(), None);
    }
}
