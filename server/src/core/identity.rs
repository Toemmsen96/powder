//! Who is logging in? The seam between the game's login and our player records.
//!
//! The game logs in with `Authorization: uplaypc_v1 t=<Ubisoft Connect ticket>` (a JWE we cannot and need not
//! decrypt). An AccountResolver turns the login request into a profile id. The default one binds client IPs to
//! players (config [players.bind]) and otherwise uses the default player, which is enough for "friends on one
//! host" on a LAN/VPN. A mod or a later version can install a smarter resolver (e.g. per-machine tokens).

use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    pub profile_id: String,
    pub user_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct LoginRequest {
    pub client_ip: String,
    /// "uplaypc_v1", "Ubi_v1", "Basic", "rm_v1", ...
    pub auth_scheme: String,
    pub auth_value: String,
    /// Request headers as received (names in any case; see `header`).
    pub headers: Vec<(String, String)>,
}

impl LoginRequest {
    pub fn new(client_ip: &str, auth_scheme: &str, auth_value: &str) -> Self {
        Self { client_ip: client_ip.into(), auth_scheme: auth_scheme.into(), auth_value: auth_value.into(),
               headers: vec![] }
    }

    /// A header's value, name matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub trait AccountResolver: Send + Sync {
    /// -> profile id of the player making this login, or None to refuse it.
    fn resolve(&self, req: &LoginRequest) -> Result<Option<String>>;
}

/// [players.bind] by client address, else the default player.
pub struct DefaultResolver {
    bind: BTreeMap<String, String>,
    default_profile_id: Arc<dyn Fn() -> Result<String> + Send + Sync>,
}

impl DefaultResolver {
    pub fn new(bind: BTreeMap<String, String>, default_profile_id: Arc<dyn Fn() -> Result<String> + Send + Sync>)
               -> Self {
        Self { bind, default_profile_id }
    }
}

impl AccountResolver for DefaultResolver {
    fn resolve(&self, req: &LoginRequest) -> Result<Option<String>> {
        match self.bind.get(&req.client_ip).filter(|p| !p.is_empty()) {
            Some(pid) => Ok(Some(pid.clone())),
            None => Ok(Some((self.default_profile_id)()?).filter(|p| !p.is_empty())),
        }
    }
}

/// 'Ubi_v1 t=abc' -> ('Ubi_v1', 'abc'); 'Basic xyz' -> ('Basic', 'xyz').
pub fn parse_authorization(header: Option<&str>) -> (String, String) {
    let Some(header) = header.filter(|h| !h.is_empty()) else { return (String::new(), String::new()) };
    let header = header.trim();
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    let rest = rest.trim();
    (scheme.to_string(), rest.strip_prefix("t=").unwrap_or(rest).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_headers() {
        let p = |h| parse_authorization(h);
        assert_eq!(p(Some("Ubi_v1 t=abc")), ("Ubi_v1".into(), "abc".into()));
        assert_eq!(p(Some(" Basic  xyz ")), ("Basic".into(), "xyz".into()));
        assert_eq!(p(Some("uplaypc_v1")), ("uplaypc_v1".into(), "".into()));
        assert_eq!(p(None), ("".into(), "".into()));
    }

    #[test]
    fn default_resolver_binds_addresses() {
        let bind = BTreeMap::from([("10.0.0.2".to_string(), "bound".to_string())]);
        let r = DefaultResolver::new(bind, Arc::new(|| Ok("default".into())));
        assert_eq!(r.resolve(&LoginRequest::new("10.0.0.2", "", "")).unwrap().as_deref(), Some("bound"));
        assert_eq!(r.resolve(&LoginRequest::new("10.0.0.3", "", "")).unwrap().as_deref(), Some("default"));
    }
}
