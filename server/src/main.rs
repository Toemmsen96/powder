//! The `powder-server` binary. The online-services server is still being ported to Rust: `run` serves HTTPS
//! (game-data pack + mods), the CA's revocation list, NEX (recorded replies + mods) and Storm, but not yet the
//! UbiServices routes, the NEX handlers or the demux, so the game itself still needs the Python server.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use powder_server::assets::Assets;
use powder_server::config::Config;
use powder_server::db::Db;
use powder_server::https::{self, HttpState};
use powder_server::logging;
use powder_server::mods::ModHost;
use powder_server::repos::Repos;
use powder_server::tls;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// Release version: the git tag baked in at build time, else the crate version.
fn version() -> &'static str {
    option_env!("POWDER_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Parser)]
#[command(name = "powder-server", version = version(),
          about = "Powder: a self-hosted stand-in for Steep's online services")]
struct Cli {
    /// The config file; its defaults are used if it does not exist.
    #[arg(short, long, global = true, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (the default).
    Run,
    /// Load the configured mods and print what they registered, then exit (no server, no saved state).
    Mods,
    /// Inspect and edit the server's data.
    Admin {
        #[command(subcommand)]
        cmd: AdminCommand,
    },
    /// Build or check the game-data pack.
    Gamedata {
        #[command(subcommand)]
        cmd: GamedataCommand,
    },
}

#[derive(Subcommand)]
enum GamedataCommand {
    /// Build a pack from your recordings (allow-list + privacy check).
    Build { recordings: PathBuf, pack: PathBuf },
    /// Check a pack's manifest and file hashes (default: the configured pack).
    Verify { pack: Option<PathBuf> },
    /// Pack the game-data pack into one .zip (to bring it to another PC).
    Zip {
        out: PathBuf,
        /// Pack folder (default: the configured one).
        #[arg(long)]
        pack: Option<PathBuf>,
    },
    /// Install a game-data pack from a .zip or folder.
    Install { source: PathBuf },
}

#[derive(Subcommand)]
enum AdminCommand {
    /// The local CA as a trusted root (needed by Ubisoft Connect; on Linux, the system store Wine reads).
    Trust {
        #[arg(value_enum, default_value = "status")]
        action: TrustAction,
    },
    /// List players.
    Players,
    /// List accounts (email -> player).
    Accounts,
    /// Create an account (asks for its password); for servers with closed registration.
    AccountCreate { email: String, name: String },
    /// Change an account's email.
    AccountEmail { email: String, new_email: String },
    /// Set a new password (for a player who forgot theirs; asks for it).
    AccountPassword { email: String },
    /// Rename an account's player.
    AccountRename { email: String, name: String },
    /// Let an account own another existing player instead.
    AccountLink { email: String, player: String },
    /// Delete an account (the player and its progress stay).
    AccountDelete { email: String },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TrustAction {
    Status,
    Install,
    Remove,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(Some(&cli.config))?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            logging::setup(Some(&config.data_dir().join("logs")), &config.server.log_level);
            tokio::runtime::Runtime::new()?.block_on(run(config))
        }
        Command::Mods => {
            logging::setup(None, &config.server.log_level);
            check_mods(&config)
        }
        Command::Admin { cmd } => {
            logging::setup(None, &config.server.log_level);
            match cmd {
                AdminCommand::Trust { action } => trust(action, &config),
                cmd => admin(cmd, config),
            }
        }
        Command::Gamedata { cmd } => {
            logging::setup(None, &config.server.log_level);
            gamedata(cmd, &config)
        }
    }
}

/// The player and account commands (the host's way out when someone forgets a password).
fn admin(cmd: AdminCommand, config: Config) -> Result<()> {
    use powder_server::constants::CREDITS_ITEM;
    use powder_server::core::sessions::SessionRegistry;
    use powder_server::services::accounts::AccountService;
    let config = Arc::new(config);
    let db = Arc::new(Db::open(&config.data_dir().join("powder.db"))?);
    db.migrate()?;
    let repos = Arc::new(Repos::new(db));
    let accounts = AccountService::new(config.clone(), repos.clone(), Arc::new(SessionRegistry::new()));
    let account = |email: &str| -> Result<_> {
        repos.account_by_email(email)?.ok_or_else(|| anyhow::anyhow!("no account with the email {email}"))
    };
    let new_password = || -> Result<String> {
        let password = rpassword::prompt_password("new password: ")?;
        if password != rpassword::prompt_password("again: ")? {
            anyhow::bail!("the passwords differ");
        }
        Ok(password)
    };
    match cmd {
        AdminCommand::Trust { .. } => unreachable!("handled by trust()"),
        AdminCommand::Players => {
            for p in repos.players()? {
                println!("{}  {:24} credits {}  since {}", p.profile_id, p.name,
                         repos.quantity(&p.profile_id, CREDITS_ITEM)?, p.created_at);
            }
            return Ok(());
        }
        AdminCommand::Accounts => {
            for acc in repos.accounts()? {
                let name = repos.player(&acc.profile_id)?.map(|p| p.name).unwrap_or_else(|| "?".into());
                println!("{:32} {name:16} {}  since {}", acc.email, acc.profile_id, acc.created_at);
            }
            if !accounts.required()? {
                println!("no accounts: every sign-in is accepted until the first player registers in the launcher");
            }
            return Ok(());
        }
        AdminCommand::AccountCreate { email, name } => {
            let acc = accounts.create_account(&email, &new_password()?, &name)?;
            println!("created {} -> player {}", acc.email, acc.profile_id);
            return Ok(());
        }
        AdminCommand::AccountEmail { email, new_email } => {
            accounts.change_email(&account(&email)?, &new_email)?;
        }
        AdminCommand::AccountPassword { email } => {
            let acc = account(&email)?;
            accounts.change_password(&acc, &new_password()?)?;
        }
        AdminCommand::AccountRename { email, name } => accounts.rename(&account(&email)?, &name)?,
        AdminCommand::AccountLink { email, player } => accounts.link(&account(&email)?, &player)?,
        AdminCommand::AccountDelete { email } => accounts.delete(&account(&email)?)?,
    }
    println!("done");
    Ok(())
}

fn gamedata(cmd: GamedataCommand, config: &Config) -> Result<()> {
    use powder_server::gamedata::{build, pack};
    let built = |m: &serde_json::Map<String, serde_json::Value>| m.get("built").cloned().unwrap_or_default();
    match cmd {
        GamedataCommand::Zip { out, pack: root } => {
            let m = pack::zip_pack(root.as_deref().unwrap_or(config.gamedata_dir()), &out)?;
            println!("wrote {} (pack built {})", out.display(), built(&m));
        }
        GamedataCommand::Install { source } => {
            let m = pack::install(&source, config.gamedata_dir())?;
            println!("installed the game-data pack (built {}) into {}", built(&m), config.gamedata_dir().display());
        }
        GamedataCommand::Build { recordings, pack: out } => {
            let report = build::build(&recordings, &out)?;
            for (name, n) in &report.kept {
                println!("  {name:20} {n}");
            }
            if !report.findings.is_empty() {
                println!("PRIVACY CHECK FAILED ({} findings); nothing written:", report.findings.len());
                for f in report.findings.iter().take(40) {
                    println!("  {f}");
                }
                std::process::exit(1);
            }
            println!("pack written to {} (privacy check passed)", out.display());
        }
        GamedataCommand::Verify { pack: root } => {
            let root = root.as_deref().unwrap_or(config.gamedata_dir());
            let manifest = pack::read_manifest(root)?;
            let bad = pack::verify_files(root, &manifest);
            let state = if bad.is_empty() { "OK".to_string() } else { format!("CHANGED: {}", bad.join(", ")) };
            println!("pack {}: built {}, {}, {state}", root.display(), built(&manifest),
                     manifest.get("sandbox").cloned().unwrap_or_default());
            if !bad.is_empty() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// Ubisoft Connect checks certificates through the OS, so the local CA must be a trusted root.
fn trust(action: TrustAction, config: &Config) -> Result<()> {
    let pki = tls::ensure_pki(&config.data_dir().join("tls"))?;
    match action {
        TrustAction::Install => powder_server::trust::install(&pki.ca_pem)?,
        TrustAction::Remove => powder_server::trust::remove(&pki.ca_pem)?,
        TrustAction::Status => {}
    }
    let trusted = powder_server::trust::is_trusted(&pki.ca_pem)?;
    println!("local CA {}: {} ({})", pki.ca_thumbprint()?, if trusted { "TRUSTED" } else { "not trusted" },
             pki.ca_pem.display());
    for other in powder_server::trust::other_tool_roots() {
        println!("note: a {other} CA is also trusted (capture tooling); remove it when no longer needed");
    }
    Ok(())
}

async fn run(config: Config) -> Result<()> {
    let config = Arc::new(config);
    log::info!(target: "powder", "Powder {} ({}): config {}", version(), config.server.name,
               config.path.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(defaults)".into()));
    let db = Arc::new(Db::open(&config.data_dir().join("powder.db"))?);
    db.migrate()?;
    log::info!(target: "powder", "data: {} (schema v{})", config.data_dir().display(), db.version()?);
    let assets = Arc::new(if config.server.use_gamedata { Assets::load(config.gamedata_dir()) } else { Assets::default() });
    let mods = Arc::new(parking_lot::Mutex::new(ModHost::load(&config, Arc::new(Repos::new(db)))));
    let pki = Arc::new(tls::ensure_pki(&config.data_dir().join("tls"))?);
    log::warn!(target: "powder", "not ported yet: UbiServices routes, NEX handlers, demux (use the Python server to play)");
    let s = &config.server;
    let addr = |port: u16| -> Result<SocketAddr> {
        format!("{}:{port}", s.bind).parse().with_context(|| format!("[server] bind {:?}", s.bind))
    };

    // The NEX handshake is signed with this key; its public half goes into the rendezvous config.
    let signing_key = powder_server::nex::secure::load_or_create_signing_key(&config.data_dir().join("nex_signing_key.pem"))?;
    let dispatcher = powder_server::nex::dispatch::Dispatcher::new(assets.clone(), Some(mods.clone()));
    powder_server::nex::server::start(dispatcher, signing_key, &s.bind, &s.public_host, s.nex_port).await?;
    log::info!(target: "powder", "NEX listening on udp {}:{} (game reaches it at {})", s.bind, s.nex_port, s.public_host);

    if config.multiplayer.storm == "powder" {
        match powder_server::storm::start(&s.bind, s.storm_port).await {
            Ok(_) => log::info!(target: "powder", "Storm (peer-to-peer helpers) on udp {}:{}-{}", s.bind,
                                s.storm_port, s.storm_port + 5),
            Err(e) => log::warn!(target: "powder", "cannot start Storm on udp {}:{}-{} ({e:#}): multiplayer will \
                                 not find other players", s.bind, s.storm_port, s.storm_port + 5),
        }
    }

    let crl_paths = std::iter::once(tls::CRL_PATH).chain(tls::LEGACY_CRL_PATHS.iter().copied()).map(String::from).collect();
    let (crl_pki, crl_addr) = (pki.clone(), addr(s.crl_port)?);
    tokio::spawn(async move {
        // returns only if the port can't be bound
        if let Err(e) = https::serve_plain(crl_addr, crl_paths, "application/pkix-crl", move || crl_pki.crl()).await {
            log::warn!(target: "powder", "cannot serve the CRL ({e:#}): Ubisoft Connect will reject our certificate");
        }
    });
    log::info!(target: "powder", "HTTPS on {}:{} (local CA {})", s.bind, s.https_port, pki.ca_thumbprint()?);
    let state = Arc::new(HttpState { config: config.clone(), assets, mods: Some(mods) });
    https::serve(addr(s.https_port)?, pki.server_config()?, state).await
}

/// Load the mods against a throwaway in-memory database (so a mod's setup can save/load without touching the
/// real data), then print what each registered.
fn check_mods(config: &Config) -> Result<()> {
    let db = Arc::new(Db::memory()?);
    db.migrate()?;
    let host = ModHost::load(config, Arc::new(Repos::new(db)));
    let report = host.report();

    println!("mods folder: {}", config.mods.dir.display());
    if report.loaded.is_empty() {
        println!("no mods loaded.");
        return Ok(());
    }
    println!("{} mod(s) loaded: {}", report.loaded.len(), report.loaded.join(", "));
    if !report.events.is_empty() {
        println!("\nevents:");
        for (event, mods) in &report.events {
            println!("  {event:<16} {}", mods.join(", "));
        }
    }
    if !report.rmc.is_empty() {
        println!("\nNEX methods (before_rmc):");
        for (method, mods) in &report.rmc {
            println!("  {method}  <- {}", mods.join(", "));
        }
    }
    if !report.http.is_empty() {
        println!("\nHTTP routes (before_http):");
        for (who, method, glob) in &report.http {
            println!("  {method:<6} {glob}  <- {who}");
        }
    }
    Ok(())
}
