//! ubix — declarative binary/CLI tool installer & tracker.
//!
//! M1 scope: config/state model (schema_version + flock), spec parsing, github
//! release install/upgrade/remove with atomic replace, list, minimal sync.

mod bootstrap;
mod cli;
mod config;
mod engine;
mod paths;
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
