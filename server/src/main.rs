//! The `powder-server` binary. The online-services server itself is still being ported to Rust; what runs
//! today is the modding toolchain, so the game's own traffic is still served by the Python server.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use powder_server::config::Config;
use powder_server::db::Db;
use powder_server::logging;
use powder_server::mods::ModHost;
use powder_server::repos::Repos;
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
    /// Load the configured mods and print what they registered, then exit (no server, no saved state).
    Mods,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(Some(&cli.config))?;
    logging::setup(None, &config.server.log_level);
    match cli.command {
        Some(Command::Mods) => check_mods(&config),
        None => bail!("the Rust server is still being ported; for now try `powder-server mods`"),
    }
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
