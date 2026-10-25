//! Accounts, login and session lookup: game and Ubisoft Connect requests -> Player -> PlayerSession.
//!
//! An account (email + password, registered in the launcher) owns one player. Ubisoft Connect signs in with the
//! email and password (`Basic`), later with the remember-me ticket we gave it (`rm_v1`, kept in the database so
//! it outlives a server restart). The game logs in with a ticket Ubisoft Connect made (`uplaypc_v1`); it is the
//! player Connect signed in as on the same PC.
//!
//! Before the first account exists (and unless [server] public is set), a server works as it did before
//! accounts: every credential is accepted and the AccountResolver ([players.bind], else the default player)
//! decides who it is.
//!
//! Lenient mode (default, [server] strict_tickets = false): a ticket we do not know (e.g. the server was
//! restarted while the game kept running) is not rejected; the request is attributed to the latest session from
//! the same address (or, without accounts, via the AccountResolver) and a warning is logged. Strict mode refuses
//! it.
//!
//! Passwords are hashed with scrypt (`scrypt$N$r$p$<salt hex>$<key hex>`, the Python server's format) and never
//! logged or stored in clear. Hashing takes tens of milliseconds: async callers should run register,
//! authenticate, change_password and login with `Basic` credentials on a blocking thread.

use crate::config::{Config, Registration};
use crate::constants::APP_ID;
use crate::core::identity::{AccountResolver, DefaultResolver, LoginRequest, Player};
use crate::core::sessions::{PlayerSession, SessionRegistry};
use crate::repos::{AccountField, AccountRow, Repos};
use crate::services::seed::create_player;
use crate::util::{random_bytes, token_urlsafe};
use base64::Engine;
use parking_lot::RwLock;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::{Arc, LazyLock};

pub const MIN_PASSWORD: usize = 8;
/// Like Ubisoft names: 3-15 letters, digits, . _ -
static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]{3,15}$").unwrap());
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").unwrap());
const SCRYPT_LOG_N: u8 = 14;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

#[derive(Debug)]
pub enum Error {
    /// Login refused (wrong credentials, unknown remember-me ticket, ...).
    Auth(String),
    /// An account request that cannot be done (email taken, password too short, ...); the message is meant for
    /// the player.
    Account(String),
    /// The database failed.
    Internal(anyhow::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Auth(m) | Error::Account(m) => f.write_str(m),
            Error::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Internal(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn account_err<T>(msg: &str) -> Result<T> {
    Err(Error::Account(msg.to_string()))
}

// ---- passwords ----------------------------------------------------------------------------------------------
pub fn hash_password(password: &str) -> String {
    let salt = random_bytes(16);
    let params = scrypt::Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, 32).expect("valid scrypt parameters");
    let mut key = [0u8; 32];
    scrypt::scrypt(password.as_bytes(), &salt, &params, &mut key).expect("32-byte output");
    format!("scrypt${}${SCRYPT_R}${SCRYPT_P}${}${}", 1u32 << SCRYPT_LOG_N, hex::encode(salt), hex::encode(key))
}

pub fn check_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [algo, n, r, p, salt, key] = parts[..] else { return false };
    if algo != "scrypt" {
        return false;
    }
    let (Ok(n), Ok(r), Ok(p), Ok(salt)) = (n.parse::<u64>(), r.parse::<u32>(), p.parse::<u32>(), hex::decode(salt))
    else { return false };
    if !n.is_power_of_two() || n < 2 {
        return false;
    }
    let Ok(params) = scrypt::Params::new(n.trailing_zeros() as u8, r, p, key.len() / 2) else { return false };
    let mut got = vec![0u8; key.len() / 2];
    if scrypt::scrypt(password.as_bytes(), &salt, &params, &mut got).is_err() {
        return false;
    }
    // constant time, on the hex texts (as Python's hmac.compare_digest(got.hex(), key))
    let got = hex::encode(got);
    got.len() == key.len() && got.bytes().zip(key.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Checked for unknown emails (see authenticate).
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| hash_password(&token_urlsafe(16)));

/// `Basic` value -> (email, password), or None if it holds none (the game sends empty ones).
pub fn basic_credentials(value: &str) -> Option<(String, String)> {
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD, base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true));
    let raw = String::from_utf8(engine.decode(value).ok()?).ok()?;
    let (email, password) = raw.split_once(':')?;
    let email = email.trim();
    (!email.is_empty()).then(|| (email.to_string(), password.to_string()))
}

/// Configured default, else the one stored (imported or created once) and kept. A database from before this was
/// stored keeps its existing player (the first one created).
pub fn default_profile_id(config: &Config, repos: &Repos) -> anyhow::Result<String> {
    if let Some(pid) = config.players.default.as_deref().filter(|p| !p.is_empty()) {
        return Ok(pid.to_lowercase());
    }
    if let Some(pid) = repos.kv_get("server", "default_player")?.as_ref().and_then(Value::as_str) {
        return Ok(pid.to_string());
    }
    let pid = match repos.players()?.first() {
        Some(p) => p.profile_id.clone(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    repos.kv_set("server", "default_player", &json!(pid))?;
    Ok(pid)
}

pub struct AccountService {
    pub config: Arc<Config>,
    pub repos: Arc<Repos>,
    pub sessions: Arc<SessionRegistry>,
    resolver: RwLock<Arc<dyn AccountResolver>>,
}

impl AccountService {
    pub fn new(config: Arc<Config>, repos: Arc<Repos>, sessions: Arc<SessionRegistry>) -> Self {
        let (c, r) = (config.clone(), repos.clone());
        let resolver = DefaultResolver::new(config.players.bind.clone(), Arc::new(move || default_profile_id(&c, &r)));
        Self { config, repos, sessions, resolver: RwLock::new(Arc::new(resolver)) }
    }

    /// Install another resolver (who a login is while there are no accounts).
    pub fn set_resolver(&self, resolver: Arc<dyn AccountResolver>) {
        *self.resolver.write() = resolver;
    }

    pub fn default_profile_id(&self) -> anyhow::Result<String> {
        default_profile_id(&self.config, &self.repos)
    }

    // ---- players ----------------------------------------------------------------------------------------------
    /// The player record, created (and seeded) on first use.
    pub fn player(&self, profile_id: &str) -> anyhow::Result<Player> {
        let row = match self.repos.player(profile_id)? {
            Some(row) => row,
            None => {
                create_player(&self.config, &self.repos, profile_id, None, None)?;
                self.repos.player(profile_id)?.ok_or_else(|| anyhow::anyhow!("player {profile_id} not created"))?
            }
        };
        Ok(Player { profile_id: row.profile_id, user_id: row.user_id, name: row.name })
    }

    /// Accounts are required once the first one exists, and always on a public server.
    pub fn required(&self) -> anyhow::Result<bool> {
        Ok(self.config.server.public || self.repos.account_count()? > 0)
    }

    // ---- accounts ---------------------------------------------------------------------------------------------
    /// A player registering (the launcher): refused while [players] registration = "closed".
    pub fn register(&self, email: &str, password: &str, name: &str) -> Result<AccountRow> {
        if self.config.players.registration == Registration::Closed {
            return account_err("This server does not take registrations; ask its host for an account.");
        }
        self.create_account(email, password, name)
    }

    /// A new account with its own player (the host's `admin account-create`, or `register`). The first account on
    /// a server takes over the existing default player (and its progress) if no account owns it yet.
    pub fn create_account(&self, email: &str, password: &str, name: &str) -> Result<AccountRow> {
        let (email, name) = (email.trim(), name.trim());
        self.check_email(email, None)?;
        check_new_password(password)?;
        let password_hash = hash_password(password);
        let (account_id, profile_id) = self.repos.db.tx(|| -> anyhow::Result<Result<(String, String)>> {
            let profile_id = match self.claimable_player()? {
                None => {
                    if let Err(e) = self.check_name(name, None)? {
                        return Ok(Err(e));
                    }
                    let pid = uuid::Uuid::new_v4().to_string();
                    create_player(&self.config, &self.repos, &pid, Some(name), None)?;
                    pid
                }
                Some(pid) => {
                    let current = self.repos.player(&pid)?.map(|p| p.name).unwrap_or_default();
                    if !name.is_empty() && name != current {
                        if let Err(e) = self.check_name(name, Some(&pid))? {
                            return Ok(Err(e));
                        }
                        self.repos.rename_player(&pid, name)?;
                    }
                    pid
                }
            };
            let account_id = uuid::Uuid::new_v4().to_string();
            self.repos.create_account(&account_id, email, &password_hash, &profile_id)?;
            Ok(Ok((account_id, profile_id)))
        })??;
        let player = self.repos.player(&profile_id)?.map(|p| p.name).unwrap_or_default();
        log::info!(target: "powder.accounts", "account {account_id} registered for player {player} ({profile_id})");
        Ok(self.repos.account(&account_id)?.ok_or_else(|| anyhow::anyhow!("account {account_id} not created"))?)
    }

    /// Name of the player a new account would take over (see create_account), else None.
    pub fn claimable_player_name(&self) -> anyhow::Result<Option<String>> {
        Ok(match self.claimable_player()? {
            Some(pid) => self.repos.player(&pid)?.map(|p| p.name),
            None => None,
        })
    }

    fn claimable_player(&self) -> anyhow::Result<Option<String>> {
        if self.repos.account_count()? > 0 {
            return Ok(None);
        }
        let pid = self.default_profile_id()?;
        Ok(self.repos.player(&pid)?.map(|_| pid))
    }

    pub fn authenticate(&self, email: &str, password: &str) -> Result<AccountRow> {
        let account = self.repos.account_by_email(email)?;
        // an unknown email costs as much as a wrong password, so emails can't be probed by timing
        let stored = account.as_ref().map(|a| a.password_hash.as_str()).unwrap_or(DUMMY_HASH.as_str());
        let ok = check_password(password, stored);
        match account {
            Some(a) if ok => Ok(a),
            _ => Err(Error::Auth("wrong email or password".into())),
        }
    }

    pub fn change_email(&self, account: &AccountRow, new_email: &str) -> Result<AccountRow> {
        let new_email = new_email.trim();
        self.check_email(new_email, Some(&account.account_id))?;
        self.repos.update_account(&account.account_id, AccountField::Email, new_email)?;
        log::info!(target: "powder.accounts", "account {}: email changed", account.account_id);
        self.reload(account)
    }

    /// Ubisoft Connect's remember-me tickets stop working, so its next sign-in needs the new password; a game that
    /// is running now stays connected.
    pub fn change_password(&self, account: &AccountRow, new_password: &str) -> Result<AccountRow> {
        check_new_password(new_password)?;
        self.repos.update_account(&account.account_id, AccountField::PasswordHash, &hash_password(new_password))?;
        let revoked = self.repos.revoke_remember_tickets(&account.account_id)?;
        log::info!(target: "powder.accounts", "account {}: password changed, {revoked} remember-me ticket(s) revoked",
                   account.account_id);
        self.reload(account)
    }

    pub fn rename(&self, account: &AccountRow, name: &str) -> Result<()> {
        let name = name.trim();
        self.check_name(name, Some(&account.profile_id))??;
        self.repos.rename_player(&account.profile_id, name)?;
        log::info!(target: "powder.accounts", "account {}: player renamed to {name}", account.account_id);
        Ok(())
    }

    /// Admin: let an account own another (existing) player.
    pub fn link(&self, account: &AccountRow, profile_id: &str) -> Result<()> {
        let profile_id = profile_id.to_lowercase();
        if self.repos.player(&profile_id)?.is_none() {
            return Err(Error::Account(format!("no player {profile_id}")));
        }
        if let Some(other) = self.repos.account_by_profile(&profile_id)? {
            if other.account_id != account.account_id {
                return Err(Error::Account(format!("player {profile_id} belongs to account {}", other.email)));
            }
        }
        self.repos.update_account(&account.account_id, AccountField::ProfileId, &profile_id)?;
        self.sessions.drop_player(&account.profile_id);
        Ok(())
    }

    /// Admin: remove an account; its player and progress stay (link them to another account).
    pub fn delete(&self, account: &AccountRow) -> Result<()> {
        self.repos.delete_account(&account.account_id)?;
        self.sessions.drop_player(&account.profile_id);
        Ok(())
    }

    /// {account_id, email, profile_id, name}: what the launcher shows.
    pub fn info(&self, account: &AccountRow) -> anyhow::Result<Value> {
        Ok(json!({"account_id": account.account_id, "email": account.email, "profile_id": account.profile_id,
                  "name": self.player(&account.profile_id)?.name}))
    }

    fn reload(&self, account: &AccountRow) -> Result<AccountRow> {
        Ok(self.repos.account(&account.account_id)?
            .ok_or_else(|| anyhow::anyhow!("account {} disappeared", account.account_id))?)
    }

    fn check_email(&self, email: &str, account_id: Option<&str>) -> Result<()> {
        if !EMAIL_RE.is_match(email) {
            return account_err("That doesn't look like an email address.");
        }
        match self.repos.account_by_email(email)? {
            Some(other) if Some(other.account_id.as_str()) != account_id => {
                account_err("An account with this email already exists.")
            }
            _ => Ok(()),
        }
    }

    /// Outer error: the database; inner: the name is refused.
    fn check_name(&self, name: &str, profile_id: Option<&str>) -> anyhow::Result<Result<()>> {
        if !NAME_RE.is_match(name) {
            return Ok(account_err("The name needs 3 to 15 letters, digits, '.', '_' or '-'."));
        }
        Ok(match self.repos.player_by_name(name)? {
            Some(other) if other.profile_id != profile_id.unwrap_or("").to_lowercase() => {
                account_err("Another player already has this name.")
            }
            _ => Ok(()),
        })
    }

    // ---- login ------------------------------------------------------------------------------------------------
    /// POST /v3/profiles/sessions -> a new session of ours, or Error::Auth.
    pub fn login(&self, req: &LoginRequest) -> Result<Arc<PlayerSession>> {
        let app_id = req.header("ubi-appid").unwrap_or("").to_lowercase();
        // Ubisoft Connect and its parts
        let connect = if !app_id.is_empty() && app_id != APP_ID { app_id.as_str() } else { "" };
        let (profile_id, how) = self.identify(req)?;
        let session = self.sessions.issue(self.player(&profile_id)?, &req.client_ip, connect);
        log::info!(target: "powder.accounts", "login {} ({profile_id}) from {}{}: {how}", session.player.name,
                   req.client_ip, if connect.is_empty() { "" } else { " [Ubisoft Connect]" });
        Ok(session)
    }

    /// Keep a remember-me ticket handed to Ubisoft Connect, for its next sign-in (rm_v1).
    pub fn remember(&self, session: &PlayerSession, ticket: &str) -> anyhow::Result<()> {
        if let Some(account) = self.repos.account_by_profile(session.profile_id())? {
            self.repos.add_remember_ticket(ticket, &account.account_id)?;
        }
        Ok(())
    }

    /// -> (profile id, which rule decided). Credentials are never logged.
    fn identify(&self, req: &LoginRequest) -> Result<(String, &'static str)> {
        let scheme = req.auth_scheme.to_lowercase();
        let required = self.required()?;
        if scheme == "basic" {
            if let Some((email, password)) = basic_credentials(&req.auth_value).filter(|_| required) {
                return Ok((self.authenticate(&email, &password)?.profile_id, "email and password"));
            }
        } else if scheme == "rm_v1" && !req.auth_value.is_empty() {
            let account = match self.repos.remember_ticket_account(&req.auth_value)? {
                Some(account_id) => self.repos.account(&account_id)?,
                None => None,
            };
            if let Some(account) = account {
                return Ok((account.profile_id, "remember-me ticket"));
            }
            if required {
                return Err(Error::Auth("unknown or revoked remember-me ticket".into()));
            }
        }
        if !req.auth_value.is_empty() {
            if let Some(s) = self.sessions.by_token(&req.auth_value) {
                return Ok((s.profile_id().to_string(), "ticket of ours"));
            }
        }
        if required {
            if let Some(s) = self.sessions.latest_from(&req.client_ip, true) {
                return Ok((s.profile_id().to_string(), "Ubisoft Connect signed in from the same address"));
            }
            return Err(Error::Auth("no account signed in from this PC (sign in to Ubisoft Connect first)".into()));
        }
        let resolver = self.resolver.read().clone();
        match resolver.resolve(req)? {
            Some(pid) => Ok((pid, "no accounts yet: address/default player")),
            None => Err(Error::Auth("login refused by account resolver".into())),
        }
    }

    /// HTTPS `Ubi_v1 t=` ticket or NEX LoginWithToken token -> session (lenient fallback, see the module doc).
    pub fn session_for_ticket(&self, ticket: &str, client_ip: &str) -> anyhow::Result<Option<Arc<PlayerSession>>> {
        let s = if ticket.is_empty() { None } else { self.sessions.by_token(ticket) };
        if s.is_some() || self.config.server.strict_tickets {
            return Ok(s);
        }
        if self.required()? {
            let s = self.sessions.latest_from(client_ip, false);
            if let Some(s) = s.as_ref().filter(|_| !ticket.is_empty()) {
                log::warn!(target: "powder.accounts", "unknown ticket from {client_ip} -> attributed to {} (lenient mode)",
                           s.player.name);
            }
            return Ok(s);
        }
        let resolver = self.resolver.read().clone();
        let Some(profile_id) = resolver.resolve(&LoginRequest::new(client_ip, "", ""))? else { return Ok(None) };
        let s = match self.sessions.for_player(&profile_id.to_lowercase()) {
            Some(s) => s,
            None => self.sessions.issue(self.player(&profile_id)?, client_ip, ""),
        };
        if !ticket.is_empty() {
            log::warn!(target: "powder.accounts", "unknown ticket from {client_ip} -> attributed to {profile_id} (lenient mode)");
        }
        Ok(Some(s))
    }
}

fn check_new_password(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(Error::Account(format!("The password needs at least {MIN_PASSWORD} characters.")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::CONNECT_APP_ID;
    use crate::db::Db;
    use crate::util::b64;

    /// Made by the Python server's hash_password algorithm (hashlib.scrypt) for "correct horse".
    const PYTHON_HASH: &str = "scrypt$16384$8$1$00112233445566778899aabbccddeeff$\
                               f5206d570fcd120bd1f23a8cd186bd87c04ac1db00e9ac1efca589774ae6ecb8";

    fn service(config: Config) -> AccountService {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        AccountService::new(Arc::new(config), Arc::new(Repos::new(db)), Arc::new(SessionRegistry::new()))
    }

    fn basic(ip: &str, email: &str, password: &str, app_id: &str) -> LoginRequest {
        let mut req = LoginRequest::new(ip, "Basic", &b64(format!("{email}:{password}").as_bytes()));
        req.headers.push(("Ubi-AppId".into(), app_id.into()));
        req
    }

    #[test]
    fn password_hashes() {
        assert!(check_password("correct horse", PYTHON_HASH), "a Python-made hash verifies");
        assert!(!check_password("wrong horse", PYTHON_HASH));
        let h = hash_password("hunter22");
        assert!(h.starts_with("scrypt$16384$8$1$") && h.split('$').count() == 6);
        assert!(check_password("hunter22", &h) && !check_password("hunter23", &h));
        for bad in ["", "bcrypt$16384$8$1$00$00", "scrypt$1000$8$1$00$0011", "scrypt$16384$8$1$zz$00", "a$b"] {
            assert!(!check_password("x", bad), "{bad}");
        }
    }

    #[test]
    fn basic_values() {
        assert_eq!(basic_credentials(&b64(b" a@b.c :p:w")), Some(("a@b.c".into(), "p:w".into())));
        assert_eq!(basic_credentials(&b64(b":pw")), None);
        assert_eq!(basic_credentials(""), None);
        assert_eq!(basic_credentials("not base64!"), None);
    }

    #[test]
    fn before_accounts_logins_use_bindings_and_the_default_player() {
        let mut config = Config::default();
        config.players.bind.insert("10.0.0.2".into(), "BOUND-PLAYER".into());
        let acc = service(config);
        assert!(!acc.required().unwrap());
        let s = acc.login(&LoginRequest::new("10.0.0.2", "uplaypc_v1", "whatever")).unwrap();
        assert_eq!(s.profile_id(), "bound-player");
        let d = acc.login(&LoginRequest::new("10.0.0.3", "Basic", "")).unwrap();
        assert_eq!(d.profile_id(), acc.default_profile_id().unwrap());
        assert_eq!(acc.default_profile_id().unwrap(), d.profile_id(), "the default player is kept");
        // a known ticket logs in as its player
        let again = acc.login(&LoginRequest::new("10.0.0.9", "Ubi_v1", &s.ticket)).unwrap();
        assert_eq!(again.profile_id(), "bound-player");
        // lenient: an unknown ticket goes to the address's player
        assert_eq!(acc.session_for_ticket("stale", "10.0.0.2").unwrap().unwrap().profile_id(), "bound-player");
    }

    #[test]
    fn account_logins_and_tickets() {
        let acc = service(Config::default());
        // the first account takes over the default player
        let d = acc.login(&LoginRequest::new("10.0.0.3", "", "")).unwrap();
        assert_eq!(acc.claimable_player_name().unwrap().as_deref(), Some(d.player.name.as_str()));
        let a = acc.register(" Ann@example.org ", "correct horse", "Ann").unwrap();
        assert_eq!(a.profile_id, d.profile_id());
        assert_eq!(acc.info(&a).unwrap()["name"], "Ann");
        assert!(acc.required().unwrap() && acc.claimable_player_name().unwrap().is_none());
        let b = acc.register("bob@example.org", "correct horse", "Bob").unwrap();
        assert_ne!(b.profile_id, a.profile_id);

        // Ubisoft Connect signs in with email and password, then the game from the same PC with its own ticket
        let c = acc.login(&basic("10.0.0.5", "ANN@example.org", "correct horse", CONNECT_APP_ID)).unwrap();
        assert!(c.connect && c.profile_id() == a.profile_id);
        let g = acc.login(&LoginRequest::new("10.0.0.5", "uplaypc_v1", "jwe-from-connect")).unwrap();
        assert!(!g.connect && g.profile_id() == a.profile_id);
        assert!(acc.login(&LoginRequest::new("10.0.0.6", "uplaypc_v1", "jwe")).is_err(), "no Connect there");

        // ticket -> session, for HTTPS and NEX
        assert_eq!(acc.session_for_ticket(&g.ticket, "elsewhere").unwrap().unwrap().session_id, g.session_id);
        assert_eq!(acc.session_for_ticket("stale", "10.0.0.5").unwrap().unwrap().profile_id(), a.profile_id);
        assert!(acc.session_for_ticket("stale", "10.0.0.7").unwrap().is_none());

        // remember-me ticket, revoked by a password change
        acc.remember(&c, "rm-ticket").unwrap();
        let r = acc.login(&LoginRequest::new("10.0.0.8", "rm_v1", "rm-ticket")).unwrap();
        assert_eq!(r.profile_id(), a.profile_id);
        acc.change_password(&a, "battery staple").unwrap();
        assert!(matches!(acc.login(&LoginRequest::new("10.0.0.8", "rm_v1", "rm-ticket")), Err(Error::Auth(_))));
        assert!(acc.login(&basic("10.0.0.8", "ann@example.org", "battery staple", CONNECT_APP_ID)).is_ok());
    }

    #[test]
    fn strict_tickets() {
        let mut config = Config::default();
        config.server.strict_tickets = true;
        let acc = service(config);
        assert!(acc.session_for_ticket("stale", "10.0.0.2").unwrap().is_none());
        let s = acc.login(&LoginRequest::new("10.0.0.2", "", "")).unwrap();
        assert!(acc.session_for_ticket(&s.ticket, "10.0.0.2").unwrap().is_some());
    }

    #[test]
    fn wrong_passwords_are_refused_and_limited() {
        use crate::core::ratelimit::FailureLimiter;
        let acc = service(Config::default());
        acc.register("ann@example.org", "correct horse", "Ann").unwrap();
        assert!(matches!(acc.authenticate("ann@example.org", "wrong horse"), Err(Error::Auth(_))));
        assert!(matches!(acc.authenticate("nobody@example.org", "correct horse"), Err(Error::Auth(_))));
        assert!(matches!(acc.login(&basic("10.0.0.5", "ann@example.org", "nope", "")), Err(Error::Auth(_))));
        // the route's loop: check, try, count
        let limiter = FailureLimiter::new(3, 30, 300.0);
        let mut refused = 0;
        for _ in 0..5 {
            if limiter.blocked("10.0.0.5", "ann@example.org") {
                refused += 1;
                continue;
            }
            assert!(acc.authenticate("ann@example.org", "guess").is_err());
            limiter.failed("10.0.0.5", "ann@example.org");
        }
        assert_eq!(refused, 2);
    }

    #[test]
    fn registration_rules() {
        let acc = service(Config::default());
        let err = |r: Result<AccountRow>| match r {
            Err(Error::Account(m)) => m,
            other => panic!("{other:?}"),
        };
        assert!(err(acc.register("not-an-email", "correct horse", "Ann")).contains("email"));
        assert!(err(acc.register("ann@example.org", "short", "Ann")).contains("at least 8"));
        let a = acc.register("ann@example.org", "correct horse", "Ann").unwrap();
        assert!(err(acc.register("ANN@example.org", "correct horse", "Ann2")).contains("already exists"));
        assert!(err(acc.register("bob@example.org", "correct horse", "ann")).contains("name"));
        assert!(err(acc.register("bob@example.org", "correct horse", "x")).contains("3 to 15"));
        assert_eq!(acc.repos.account_count().unwrap(), 1, "refused registrations leave nothing behind");
        acc.rename(&a, "Annie").unwrap();
        assert_eq!(acc.info(&a).unwrap()["name"], "Annie");
        assert_eq!(acc.change_email(&a, "annie@example.org").unwrap().email, "annie@example.org");

        let mut closed = Config::default();
        closed.players.registration = Registration::Closed;
        let acc = service(closed);
        assert!(err(acc.register("ann@example.org", "correct horse", "Ann")).contains("registrations"));
        assert!(acc.create_account("ann@example.org", "correct horse", "Ann").is_ok(), "the host still can");
    }

    #[test]
    fn public_servers_need_accounts() {
        let mut config = Config::default();
        config.server.public = true;
        let acc = service(config);
        assert!(acc.required().unwrap());
        assert!(matches!(acc.login(&LoginRequest::new("10.0.0.2", "uplaypc_v1", "jwe")), Err(Error::Auth(_))));
        assert!(acc.session_for_ticket("stale", "10.0.0.2").unwrap().is_none());
    }
}
