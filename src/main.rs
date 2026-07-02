//! ubix — declarative binary/CLI tool installer & tracker.
//!
//! Implements M1–M6 (see docs/PRD.md §11): config/state model (schema_version +
//! flock), spec parsing, multi-source install/upgrade/remove (github, gitlab,
//! pypi/uv, npm/fnm, cargo, go, url) with atomic replace, sync/prune, doctor,
//! outdated, checksum discovery, and toolchain bootstrap.

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

/// Current UTC timestamp as RFC 3339 (used for state `installed_at`/`updated_at`).
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let app = cli::App::new()?;
    if let Err(e) = app.run(cli) {
        // Print the full error chain with context.
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}
