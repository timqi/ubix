//! ubix — declarative binary/CLI tool installer & tracker.
//!
//! Implements M1–M6 (see docs/PRD.md §11): config/state model (schema_version +
//! flock), spec parsing, multi-source install/upgrade/remove (github, gitlab,
//! pypi/uv, npm/fnm, cargo, go, url) with atomic replace, sync/prune, doctor,
//! outdated, checksum discovery, and toolchain bootstrap.

#[macro_use]
mod progress;

mod aqua;
mod archive;
mod bootstrap;
mod checksum;
mod cli;
mod config;
mod engine;
mod http;
mod outdated;
mod paths;
mod platform;
mod remove;
mod runner;
mod sources;
mod state;

use anyhow::Result;
use clap::Parser;

use crate::progress::Verbosity;

/// Current UTC timestamp as RFC 3339 (used for state `installed_at`/`updated_at`).
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let verbosity = cli.verbosity();

    // Set global progress level and initialize dependency (`log`) output.
    progress::set_verbosity(verbosity);
    init_logger(verbosity);

    let app = cli::App::new(verbosity)?;
    if let Err(e) = app.run(cli) {
        // Print the full error chain with context.
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// Initialize `env_logger` so dependency logs (notably ubi) surface at a level
/// that matches ubix verbosity. `RUST_LOG`, if set, always wins.
fn init_logger(verbosity: Verbosity) {
    use env_logger::Env;
    // Default filter by verbosity: quiet→error, normal→warn (deps quiet),
    // verbose→info for ubi+ubix (deps still warn to avoid noise).
    let default = match verbosity {
        Verbosity::Quiet => "error",
        Verbosity::Normal => "warn",
        Verbosity::Verbose => "warn,ubi=info,ubix=info",
    };
    // `Env` honors RUST_LOG when present, else uses our default.
    env_logger::Builder::from_env(Env::default().default_filter_or(default))
        .format_timestamp(None)
        .init();
}
