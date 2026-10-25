//! Server configuration from a TOML file (see config.example.toml). Every key is optional; unknown keys are
//! refused, so a typo doesn't silently do nothing.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// "127.0.0.1": this PC only; "0.0.0.0" for friends on a LAN/VPN or an internet server.
    pub bind: String,
    /// Address the games use to reach this server (NEX station URLs, peer-to-peer helpers).
    pub public_host: String,
    pub https_port: u16,
    pub nex_port: u16,
    /// Storm (UDP): NAT detection on this port to +3, traversal on +5.
    pub storm_port: u16,
    /// Plain HTTP: the local CA's revocation list Ubisoft Connect checks.
    pub crl_port: u16,
    /// Ubisoft Connect keeps the game's save files on this server (data/cloudsaves/).
    pub connect_cloud_saves: bool,
    pub data_dir: PathBuf,
    pub gamedata_dir: PathBuf,
    pub use_gamedata: bool,
    pub recordings_dir: PathBuf,
    pub country: String,
    pub region: String,
    pub log_level: String,
    /// Refuse UbiServices/NEX tickets this server did not issue.
    pub strict_tickets: bool,
    /// The server's name, shown by the launcher and in /_powder/status.
    pub name: String,
    /// A short text for players (rules, contact), shown by the launcher.
    pub motd: String,
    /// An internet-facing server: sign-ins always need an account, and /_powder/status shows a caller only
    /// its own connections (no other players' addresses).
    pub public: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            public_host: "127.0.0.1".into(),
            https_port: 443,
            nex_port: 6005,
            storm_port: 11000,
            crl_port: 80,
            connect_cloud_saves: true,
            data_dir: "data".into(),
            gamedata_dir: "gamedata".into(),
            use_gamedata: true,
            recordings_dir: "recordings".into(),
            country: "US".into(),
            region: "NA".into(),
            log_level: "INFO".into(),
            strict_tickets: false,
            name: "Powder".into(),
            motd: String::new(),
            public: false,
        }
    }
}

/// Gameplay settings; read per call via Config::toggles(profile_id), never cached.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Toggles {
    pub unlimited_heli_tickets: bool,
    pub starting_credits: i64,
    pub unlock_all: bool,
    pub xp_multiplier: f64,
    pub free_purchases: bool,
    pub reward_credit_multiplier: f64,
}

impl Default for Toggles {
    fn default() -> Self {
        Self { unlimited_heli_tickets: false, starting_credits: 0, unlock_all: false, xp_multiplier: 1.0,
               free_purchases: false, reward_credit_multiplier: 1.0 }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlayersConfig {
    /// Player used when a login cannot be told apart (only while there is no account).
    pub default: Option<String>,
    /// Client IP -> profile id (legacy, before accounts).
    pub bind: BTreeMap<String, String>,
    /// "open": anyone who reaches the server can register an account; "closed": only the host creates
    /// accounts (`admin account-create`).
    pub registration: Registration,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    #[default]
    Open,
    Closed,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MultiplayerConfig {
    /// "powder": this server is the games' peer-to-peer helper (Storm); "off": none (multiplayer off).
    pub storm: String,
    /// Testing alone: these players (names) look online in Steep, so the game offers to invite them.
    pub pretend_online: Vec<String>,
    /// Most players per shared-world session (the game asks for 32).
    pub max_session_size: Option<u32>,
}

impl Default for MultiplayerConfig {
    fn default() -> Self {
        Self { storm: "powder".into(), pretend_online: vec![], max_session_size: None }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModsConfig {
    /// Folder of server mods (relative to the config file).
    pub dir: PathBuf,
    /// Mod folder names, "*" = all.
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
}

impl Default for ModsConfig {
    fn default() -> Self {
        Self { dir: "mods".into(), enabled: vec!["*".into()], disabled: vec![] }
    }
}

impl ModsConfig {
    pub fn allows(&self, name: &str) -> bool {
        !self.disabled.iter().any(|d| d == name) && self.enabled.iter().any(|e| e == "*" || e == name)
    }
}

/// What the file holds, as written.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    server: ServerConfig,
    players: PlayersConfig,
    multiplayer: MultiplayerConfig,
    gameplay: toml::Table,
    mods: Option<ModsConfig>,
    plugins: Option<ModsConfig>, // before mods: the Python server's plugins (same filter)
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub players: PlayersConfig,
    pub multiplayer: MultiplayerConfig,
    pub gameplay: Toggles,
    pub gameplay_per_player: BTreeMap<String, toml::Table>,
    pub mods: ModsConfig,
    /// The file this was loaded from.
    pub path: Option<PathBuf>,
}

impl Config {
    pub fn data_dir(&self) -> &Path {
        &self.server.data_dir
    }

    pub fn gamedata_dir(&self) -> &Path {
        &self.server.gamedata_dir
    }

    /// The folder the config file is in (relative paths start there).
    pub fn base_dir(&self) -> PathBuf {
        self.path.as_ref().and_then(|p| absolute(p).parent().map(Path::to_path_buf))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }

    pub fn toggles(&self, profile_id: Option<&str>) -> Toggles {
        let Some(over) = profile_id.and_then(|p| self.gameplay_per_player.get(p)) else {
            return self.gameplay.clone();
        };
        let mut merged = toml::Table::try_from(&self.gameplay).unwrap_or_default();
        for (k, v) in over {
            merged.insert(k.clone(), v.clone());
        }
        toml::Value::Table(merged).try_into().unwrap_or_else(|_| self.gameplay.clone())
    }

    pub fn load(path: Option<&Path>) -> Result<Config> {
        let Some(path) = path.filter(|p| p.exists()) else {
            // defaults, next to where the file would be
            let base = path.and_then(|p| absolute(p).parent().map(Path::to_path_buf))
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let mut cfg = Config { path: path.map(Path::to_path_buf), ..Default::default() };
            cfg.server.data_dir = base.join("data");
            cfg.server.gamedata_dir = base.join("gamedata");
            cfg.server.recordings_dir = base.join("recordings");
            cfg.mods.dir = base.join("mods");
            return Ok(cfg);
        };
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text).with_context(|| format!("{}", path.display()))?;
        let base = absolute(path).parent().map(Path::to_path_buf).unwrap_or_default();
        let mut server = raw.server;
        for dir in [&mut server.data_dir, &mut server.gamedata_dir, &mut server.recordings_dir] {
            if dir.is_relative() {
                *dir = base.join(&*dir);
            }
        }
        let mut multiplayer = raw.multiplayer;
        if multiplayer.storm == "pack" {
            multiplayer.storm = "powder".into(); // before Powder had its own: now the same
        }
        if multiplayer.storm != "off" && multiplayer.storm != "powder" {
            bail!("[multiplayer] storm must be \"powder\" or \"off\", not {:?}", multiplayer.storm);
        }
        let mut gameplay_raw = raw.gameplay;
        let per_player = match gameplay_raw.remove("player") {
            Some(toml::Value::Table(t)) => t,
            Some(_) => bail!("[gameplay.player] must be a table of players"),
            None => toml::Table::new(),
        };
        let gameplay: Toggles = toml::Value::Table(gameplay_raw).try_into().context("[gameplay]")?;
        let mut gameplay_per_player = BTreeMap::new();
        for (pid, over) in per_player {
            let toml::Value::Table(over) = over else { bail!("[gameplay.player.{pid}] must be a table") };
            // check the keys and types now (merged with the defaults)
            let mut merged = toml::Table::try_from(&gameplay).unwrap_or_default();
            merged.extend(over.clone());
            let _: Toggles = toml::Value::Table(merged).try_into()
                .with_context(|| format!("[gameplay.player.{pid}]"))?;
            gameplay_per_player.insert(pid.to_lowercase(), over);
        }
        let mut mods = raw.mods.or(raw.plugins).unwrap_or_default();
        if mods.dir.is_relative() {
            mods.dir = base.join(&mods.dir);
        }
        Ok(Config { server, players: raw.players, multiplayer, gameplay, gameplay_per_player, mods,
                    path: Some(path.to_path_buf()) })
    }
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../config.example.toml");
        let cfg = Config::parse(text, Path::new("/srv/powder/config.toml")).unwrap();
        assert_eq!(cfg.server.https_port, 443);
        assert_eq!(cfg.server.data_dir, PathBuf::from("/srv/powder/data"));
        assert_eq!(cfg.multiplayer.storm, "powder");
    }

    #[test]
    fn unknown_keys_and_per_player_toggles() {
        assert!(Config::parse("[server]\nhttps_prot = 1\n", Path::new("/x/c.toml")).is_err());
        let cfg = Config::parse("[gameplay]\nstarting_credits = 5\n[gameplay.player.\"AB\"]\nfree_purchases = true\n",
                                Path::new("/x/c.toml")).unwrap();
        assert!(cfg.toggles(Some("ab")).free_purchases);
        assert_eq!(cfg.toggles(Some("ab")).starting_credits, 5);
        assert!(!cfg.toggles(Some("cd")).free_purchases);
        assert!(Config::parse("[gameplay.player.x]\nnope = 1\n", Path::new("/x/c.toml")).is_err());
        assert!(Config::parse("[multiplayer]\nstorm = \"maybe\"\n", Path::new("/x/c.toml")).is_err());
    }
}
