//! The `powder-server` binary. The online-services server is still being ported to Rust: `run` serves HTTPS
//! (game-data pack + mods) and the CA's revocation list, but not yet the UbiServices routes, NEX, the demux or
//! Storm, so the game itself still needs the Python server.

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
}

#[derive(Subcommand)]
enum AdminCommand {
    /// The local CA as a trusted root (needed by Ubisoft Connect; on Linux, the system store Wine reads).
    Trust {
        #[arg(value_enum, default_value = "status")]
        action: TrustAction,
    },
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
            }
        }
    }
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
    let assets = if config.server.use_gamedata { Assets::load(config.gamedata_dir()) } else { Assets::default() };
    let mods = ModHost::load(&config, Arc::new(Repos::new(db)));
    let pki = Arc::new(tls::ensure_pki(&config.data_dir().join("tls"))?);
    log::info!(target: "powder", "local CA thumbprint {}", pki.ca_thumbprint()?);
    log::warn!(target: "powder", "not ported yet: UbiServices routes, NEX, demux, Storm (use the Python server to play)");

    let addr = |port: u16| -> Result<SocketAddr> {
        format!("{}:{port}", config.server.bind).parse().with_context(|| format!("[server] bind {:?}", config.server.bind))
    };
    let state = Arc::new(HttpState { config: config.clone(), assets: Arc::new(assets),
                                     mods: Some(Arc::new(parking_lot::Mutex::new(mods))) });
    let crl_paths = std::iter::once(tls::CRL_PATH).chain(tls::LEGACY_CRL_PATHS.iter().copied()).map(String::from).collect();
    let crl_pki = pki.clone();
    tokio::try_join!(
        https::serve(addr(config.server.https_port)?, pki.server_config()?, state),
        https::serve_plain(addr(config.server.crl_port)?, crl_paths, "application/pkix-crl", move || crl_pki.crl()),
    )?;
    Ok(())
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
