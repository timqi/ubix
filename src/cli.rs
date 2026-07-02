//! Clap CLI definition (§7) and command dispatch.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, ToolConfig};
use crate::engine::UbiEngine;
use crate::paths::Paths;
use crate::runner::{CommandRunner, SystemRunner};
use crate::sources::github::GithubSource;
use crate::sources::{handler_for, parse_spec, SourceKind};
use crate::state::{LockedState, ToolRecord};
use crate::{bootstrap, remove};

/// ubix — declarative binary/CLI tool installer & tracker.
#[derive(Debug, Parser)]
#[command(name = "ubix", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add a tool (writes config and installs immediately). Spec syntax per PRD §4.2.
    Add(AddArgs),
    /// Uninstall a tool and remove it from config (only removes state-tracked files).
    Remove(RemoveArgs),
    /// Upgrade tool(s) in place. Pinned `tag` tools are skipped unless --force.
    Upgrade(UpgradeArgs),
    /// Reconcile system state to config (M1: install missing declared tools).
    Sync(SyncArgs),
    /// List declared and installed tools.
    List,
    /// Show latest vs installed versions.
    Outdated,
    /// Show source, paths, and parameters for a tool.
    Info(InfoArgs),
    /// Open config.toml in $EDITOR.
    Edit,
    /// Check underlying tools and PATH readiness.
    Doctor,
    /// Bootstrap a toolchain / underlying tool.
    Bootstrap(BootstrapArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// `$source:$locator` spec (e.g. github:owner/repo, pypi:ruff).
    pub spec: String,
    /// Explicit tool name (defaults to derived from the locator).
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub matching: Option<String>,
    #[arg(long)]
    pub exe: Option<String>,
    /// Comma-separated multi-exe list (recognized; single-exe only in M1).
    #[arg(long, value_delimiter = ',')]
    pub exes: Option<Vec<String>>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub version: Option<String>,
    /// Rename the installed executable.
    #[arg(long)]
    pub rename: Option<String>,
    /// Block waiting for the state lock instead of failing fast.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub name: String,
    /// Adopt an untracked file into state and then remove it (§8.5).
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, Args)]
pub struct UpgradeArgs {
    /// Tool name; omit with --all to upgrade everything.
    pub name: Option<String>,
    #[arg(long)]
    pub all: bool,
    /// Re-install pinned-tag tools (§8.4).
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub dry_run: bool,
    /// Remove orphaned tools (state has it, config does not). Stub in M1.
    #[arg(long)]
    pub prune: bool,
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct BootstrapArgs {
    /// One of rust|go|uv|fnm.
    pub target: String,
    #[arg(long)]
    pub reinstall: bool,
}

/// Shared context for command execution.
pub struct App {
    pub paths: Paths,
    pub runner: Box<dyn CommandRunner>,
}

impl App {
    pub fn new() -> Result<Self> {
        Ok(Self {
            paths: Paths::resolve()?,
            runner: Box::new(SystemRunner::new()),
        })
    }

    pub fn run(&self, cli: Cli) -> Result<()> {
        match cli.command {
            Command::Add(a) => self.cmd_add(a),
            Command::Remove(a) => self.cmd_remove(a),
            Command::Upgrade(a) => self.cmd_upgrade(a),
            Command::Sync(a) => self.cmd_sync(a),
            Command::List => self.cmd_list(),
            Command::Outdated => self.cmd_outdated(),
            Command::Info(a) => self.cmd_info(a),
            Command::Edit => self.cmd_edit(),
            Command::Doctor => self.cmd_doctor(),
            Command::Bootstrap(a) => self.cmd_bootstrap(a),
        }
    }

    // ---- add ----
    fn cmd_add(&self, args: AddArgs) -> Result<()> {
        // Build the tool entry from args (no config needed for these fields).
        let mut tool = ToolConfig::from_spec(args.spec.clone());
        tool.matching = args.matching;
        tool.exe = args.exe;
        tool.exes = args.exes;
        tool.tag = args.tag;
        tool.host = args.host;
        tool.version = args.version;
        tool.rename = args.rename;

        // Acquire the state lock FIRST, then read-modify-write config INSIDE the
        // lock, so two concurrent `ubix add` invocations serialize and neither
        // drops the other's new entry (the lock guards config writes too).
        let cfg_path = self.paths.config_file();
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let mut cfg = Config::load_or_default(&cfg_path)?;

        let default_source = cfg.settings.default_source_kind()?;
        let parsed = parse_spec(&args.spec, default_source)
            .with_context(|| format!("invalid spec `{}`", args.spec))?;
        let name = args
            .name
            .clone()
            .unwrap_or_else(|| derive_name(&parsed.locator));

        // Guard against unimplemented sources before writing config.
        if !parsed.source.is_implemented() {
            bail!(
                "source `{}:` is not yet implemented ({}); cannot add `{name}` yet",
                parsed.source,
                parsed.source.milestone()
            );
        }

        // Install first, then persist state + config so we never record a failed install.
        let record = self.install_tool(&cfg, &name, &tool)?;
        locked.state.tools.insert(name.clone(), record);
        locked.save()?;

        cfg.tools.insert(name.clone(), tool);
        cfg.save(&cfg_path)?;
        println!("added `{name}` ({})", parsed.source);
        Ok(())
    }

    // ---- remove ----
    fn cmd_remove(&self, args: RemoveArgs) -> Result<()> {
        // Lock first, then read-modify-write config inside the lock.
        let cfg_path = self.paths.config_file();
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let mut cfg = Config::load_or_default(&cfg_path)?;

        remove::remove_tool(&mut cfg, &mut locked.state, &args.name, args.force)?;

        locked.save()?;
        cfg.save(&cfg_path)?;
        println!("removed `{}`", args.name);
        Ok(())
    }

    // ---- upgrade ----
    fn cmd_upgrade(&self, args: UpgradeArgs) -> Result<()> {
        // Lock first, then read config inside the lock for a consistent view.
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let cfg = Config::load_or_default(&self.paths.config_file())?;

        let targets: Vec<String> = if args.all {
            cfg.tools.keys().cloned().collect()
        } else if let Some(n) = &args.name {
            vec![n.clone()]
        } else {
            bail!("specify a tool name or --all");
        };

        for name in targets {
            let Some(tool) = cfg.tools.get(&name) else {
                eprintln!("skip `{name}`: not in config");
                continue;
            };
            // Pinned tag → skip unless --force (§8.4).
            if tool.tag.is_some() && !args.force {
                println!("skip `{name}`: pinned to tag `{}` (use --force)", tool.tag.as_deref().unwrap_or(""));
                continue;
            }
            let record = self.install_tool(&cfg, &name, tool)?;
            locked.state.tools.insert(name.clone(), record);
            locked.save()?;
            println!("upgraded `{name}`");
        }
        Ok(())
    }

    // ---- sync (M1 minimal: install missing declared tools) ----
    fn cmd_sync(&self, args: SyncArgs) -> Result<()> {
        // Lock first, then read config inside the lock for a consistent view.
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let cfg = Config::load_or_default(&self.paths.config_file())?;

        // Orphans: state has it, config does not.
        for name in locked.state.tools.keys() {
            if !cfg.tools.contains_key(name) {
                if args.prune {
                    println!("orphan `{name}`: --prune is not implemented in M1 (deferred to M2); leaving in place");
                } else {
                    println!("orphan `{name}`: in state but not config (run `sync --prune` in a later milestone)");
                }
            }
        }

        let mut installed = 0usize;
        for (name, tool) in &cfg.tools {
            let already = locked.state.tool(name).is_some();
            if already {
                continue;
            }
            let parsed = cfg.parsed_spec(tool)?;
            if !parsed.source.is_implemented() {
                println!(
                    "skip `{name}`: source `{}:` not yet implemented ({})",
                    parsed.source,
                    parsed.source.milestone()
                );
                continue;
            }
            if args.dry_run {
                println!("would install `{name}` ({})", parsed.source);
                continue;
            }
            let record = self.install_tool(&cfg, name, tool)?;
            locked.state.tools.insert(name.clone(), record);
            locked.save()?;
            installed += 1;
            println!("installed `{name}`");
        }
        if args.dry_run {
            println!("dry-run complete");
        } else {
            println!("sync complete: {installed} tool(s) installed");
        }
        Ok(())
    }

    // ---- list ----
    fn cmd_list(&self) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        // list is read-only; no write lock (§8.6).
        let state = read_state_no_lock(&self.paths.state_file())?;

        if cfg.tools.is_empty() {
            println!("no tools declared");
            return Ok(());
        }
        for (name, tool) in &cfg.tools {
            let parsed = cfg.parsed_spec(tool).ok();
            let src = parsed.map(|p| p.source.to_string()).unwrap_or_else(|| "?".into());
            let ver = state
                .tools
                .get(name)
                .map(|r| r.installed_version.clone())
                .unwrap_or_else(|| "(not installed)".into());
            println!("{name:20} {src:8} {ver}");
        }
        Ok(())
    }

    // ---- outdated (stub for M1) ----
    fn cmd_outdated(&self) -> Result<()> {
        println!("`outdated` is not yet implemented (M6): latest-version queries land later.");
        Ok(())
    }

    // ---- info ----
    fn cmd_info(&self, args: InfoArgs) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        let state = read_state_no_lock(&self.paths.state_file())?;
        let Some(tool) = cfg.tools.get(&args.name) else {
            bail!("tool `{}` is not declared in config", args.name);
        };
        let parsed = cfg.parsed_spec(tool)?;
        println!("name:    {}", args.name);
        println!("source:  {}", parsed.source);
        println!("locator: {}", parsed.locator);
        println!("spec:    {}", tool.spec);
        if let Some(m) = &tool.matching {
            println!("matching: {m}");
        }
        if let Some(e) = &tool.exe {
            println!("exe:     {e}");
        }
        if let Some(t) = &tool.tag {
            println!("tag:     {t}");
        }
        if let Some(rec) = state.tools.get(&args.name) {
            println!("installed_version: {}", rec.installed_version);
            println!("install_paths: {:?}", rec.install_paths);
            if let Some(sha) = &rec.sha256 {
                println!("sha256:  {sha}");
            }
        } else {
            println!("(not installed)");
        }
        Ok(())
    }

    // ---- edit ----
    fn cmd_edit(&self) -> Result<()> {
        let cfg_path = self.paths.config_file();
        // Ensure the file exists so the editor opens something.
        if !cfg_path.exists() {
            Config::default().save(&cfg_path)?;
        }
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let out = self
            .runner
            .run(&editor, &[&cfg_path.to_string_lossy()], &[])
            .with_context(|| format!("launching editor `{editor}`"))?;
        if !out.success() {
            bail!("editor `{editor}` exited with status {}", out.status);
        }
        Ok(())
    }

    // ---- doctor ----
    fn cmd_doctor(&self) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        let install_dir = cfg.settings.install_dir_path()?;
        println!("ubix doctor");
        println!("  config: {}", self.paths.config_file().display());
        println!("  state:  {}", self.paths.state_file().display());
        println!("  install_dir: {}", install_dir.display());

        // PATH check for install_dir (§8.9).
        let on_path = path_contains(&install_dir);
        println!(
            "  [{}] install_dir on $PATH",
            if on_path { "ok" } else { "!!" }
        );
        if !on_path {
            println!(
                "      add to your shell rc: export PATH=\"{}:$PATH\"",
                install_dir.display()
            );
        }

        // Underlying tools (informational in M1).
        for tool in ["uv", "fnm", "rustup", "go"] {
            let present = self.runner.which(tool);
            println!("  [{}] {tool}", if present { "ok" } else { "--" });
        }
        Ok(())
    }

    // ---- bootstrap ----
    fn cmd_bootstrap(&self, args: BootstrapArgs) -> Result<()> {
        let target: bootstrap::BootstrapTarget = args.target.parse()?;
        bootstrap::bootstrap(target, args.reinstall)
    }

    // ---- helpers ----

    /// Install a tool via its source handler and return a fresh state record.
    fn install_tool(&self, cfg: &Config, name: &str, tool: &ToolConfig) -> Result<ToolRecord> {
        let parsed = cfg.parsed_spec(tool)?;
        let install_dir = cfg.settings.install_dir_path()?;

        let outcome = match parsed.source {
            SourceKind::Github => {
                let src = GithubSource::for_tool(
                    name.to_string(),
                    install_dir,
                    Box::new(UbiEngine::new()),
                );
                use crate::sources::Source;
                src.install(tool, self.runner.as_ref())?
            }
            other => {
                // Recognized but unimplemented → clean error via handler_for.
                let _ = handler_for(other)?;
                unreachable!("handler_for returns Err for unimplemented sources");
            }
        };

        let now = crate::now_iso8601();
        Ok(ToolRecord {
            source: parsed.source.to_string(),
            installed_version: outcome.installed_version,
            resolved_asset: outcome.resolved_asset,
            module: None,
            install_paths: outcome.install_paths,
            sha256: outcome.sha256,
            installed_at: Some(now.clone()),
            updated_at: Some(now),
        })
    }
}

/// Read state without taking the write lock (for read-only commands, §8.6).
fn read_state_no_lock(path: &std::path::Path) -> Result<crate::state::State> {
    if !path.exists() {
        return Ok(crate::state::State::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(crate::state::State::default());
    }
    crate::state::State::from_toml(&text)
}

/// Derive a tool name from a locator: last path segment, stripped of `@version`.
pub fn derive_name(locator: &str) -> String {
    let base = locator.rsplit('/').next().unwrap_or(locator);
    let base = base.split('@').next().unwrap_or(base);
    // For URLs, strip a trailing archive extension.
    base.trim_end_matches(".tar.gz")
        .trim_end_matches(".tar.xz")
        .trim_end_matches(".zip")
        .to_string()
}

/// Whether `dir` appears as an entry in `$PATH`.
fn path_contains(dir: &std::path::Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| p == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_name_owner_repo() {
        assert_eq!(derive_name("eza-community/eza"), "eza");
    }

    #[test]
    fn derive_name_go_module() {
        assert_eq!(derive_name("example.com/cmd/tool@latest"), "tool");
    }

    #[test]
    fn derive_name_url_archive() {
        assert_eq!(
            derive_name("https://example.com/something-linux.tar.gz"),
            "something-linux"
        );
    }

    #[test]
    fn cli_parses_add() {
        let cli = Cli::try_parse_from(["ubix", "add", "github:owner/repo", "--tag", "v1"]).unwrap();
        match cli.command {
            Command::Add(a) => {
                assert_eq!(a.spec, "github:owner/repo");
                assert_eq!(a.tag.as_deref(), Some("v1"));
            }
            _ => panic!("expected add"),
        }
    }

    #[test]
    fn cli_parses_exes_list() {
        let cli =
            Cli::try_parse_from(["ubix", "add", "github:astral-sh/uv", "--exes", "uv,uvx"]).unwrap();
        match cli.command {
            Command::Add(a) => assert_eq!(a.exes, Some(vec!["uv".into(), "uvx".into()])),
            _ => panic!("expected add"),
        }
    }
}
