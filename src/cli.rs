//! Clap CLI definition (§7) and command dispatch.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::bootstrap;
use crate::config::{Config, ToolConfig};
use crate::engine::UbiEngine;
use crate::http::{HttpClient, ReqwestClient};
use crate::outdated::{self, Latest};
use crate::paths::Paths;
use crate::remove;
use crate::runner::{CommandRunner, SystemRunner};
use crate::sources::github::GithubSource;
use crate::sources::http as http_source;
use crate::sources::{cargo, gitlab, go, npm, parse_spec, url, uv, Source, SourceKind};
use crate::state::{LockedState, ToolRecord};

/// ubix — declarative binary/CLI tool installer & tracker.
#[derive(Debug, Parser)]
#[command(
    name = "ubix",
    version = env!("UBIX_VERSION"),
    long_version = concat!(
        env!("UBIX_VERSION"),
        "\ncommit ", env!("UBIX_GIT_SHA"),
        " (", env!("UBIX_COMMIT_DATE"), ")"
    ),
    about,
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Suppress progress output (only the final result on stdout).
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Show more detail, including dependency logs (ubi asset selection, etc.).
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

impl Cli {
    /// Resolve the effective verbosity from the global flags (`--quiet` wins).
    pub fn verbosity(&self) -> crate::progress::Verbosity {
        use crate::progress::Verbosity;
        if self.quiet {
            Verbosity::Quiet
        } else if self.verbose {
            Verbosity::Verbose
        } else {
            Verbosity::Normal
        }
    }
}

#[derive(Debug, Subcommand)]
// `Add` carries the most flags (it's the richest subcommand); the size gap to
// the other variants is expected for a clap args enum and boxing an `Args`
// struct is not supported by the derive.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Add a tool (writes config and installs immediately). Spec syntax per PRD §4.2.
    Add(AddArgs),
    /// Uninstall a tool and remove it from config (only removes state-tracked files).
    Remove(RemoveArgs),
    /// Upgrade tool(s) in place. Pinned `tag` tools are skipped unless --force.
    Upgrade(UpgradeArgs),
    /// Reconcile system state to config: install missing, converge, prune orphans.
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
    /// Bootstrap a language toolchain (rust|go).
    Bootstrap(BootstrapArgs),
    /// List supported spec prefixes and their install backends.
    Sources,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// `$source:$locator` spec (e.g. github:owner/repo, pypi:ruff).
    pub spec: String,
    /// Explicit tool name (defaults to derived from the locator).
    #[arg(long)]
    pub name: Option<String>,
    /// Asset disambiguation substring (single-platform). For a cross-platform
    /// per-OS/arch table, edit `[tools.<name>.matching]` in config.toml.
    #[arg(long)]
    pub matching: Option<String>,
    #[arg(long)]
    pub exe: Option<String>,
    /// Comma-separated multi-exe list.
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
    /// (http source) alternate URL template used on Linux+musl.
    #[arg(long)]
    pub url_musl: Option<String>,
    /// (http source) where to discover the version, e.g. github:owner/repo.
    #[arg(long)]
    pub version_source: Option<String>,
    /// (http source) runtime-arch → url-token override, repeatable, e.g. --arch-replace amd64=x64.
    #[arg(long, value_name = "K=V")]
    pub arch_replace: Vec<String>,
    /// Overwrite an existing tool of the same name (reinstall + replace config entry).
    #[arg(long)]
    pub force: bool,
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
    /// Remove orphaned tools (state has it, config does not) (§8.3).
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
    /// Toolchain to bootstrap: <rust|go>.
    pub target: String,
    #[arg(long)]
    pub reinstall: bool,
}

/// Shared context for command execution.
pub struct App {
    pub paths: Paths,
    pub runner: Box<dyn CommandRunner>,
    pub http: Box<dyn HttpClient>,
    /// Effective verbosity (also mirrored into the global used by `step!`).
    pub verbosity: crate::progress::Verbosity,
}

impl App {
    pub fn new(verbosity: crate::progress::Verbosity) -> Result<Self> {
        // Keep the global (consulted by the step!/detail! macros) in sync.
        crate::progress::set_verbosity(verbosity);
        Ok(Self {
            paths: Paths::resolve()?,
            runner: Box::new(SystemRunner::new()),
            http: Box::new(ReqwestClient::new()),
            verbosity,
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
            Command::Sources => self.cmd_sources(),
        }
    }

    // ---- add ----
    fn cmd_add(&self, args: AddArgs) -> Result<()> {
        let mut tool = ToolConfig::from_spec(args.spec.clone());
        tool.matching = args.matching.map(crate::config::PlatformString::One);
        tool.exe = args.exe;
        tool.exes = args.exes;
        tool.tag = args.tag;
        tool.host = args.host;
        tool.version = args.version;
        tool.rename = args.rename;
        // http-source fields.
        tool.url_musl = args.url_musl;
        tool.version_source = args.version_source;
        if !args.arch_replace.is_empty() {
            tool.arch_replace = Some(parse_kv_pairs(&args.arch_replace)?);
        }

        let cfg_path = self.paths.config_file();
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let mut cfg = Config::load_or_default(&cfg_path)?;

        let default_source = cfg.settings.default_source_kind()?;
        let parsed = parse_spec(&args.spec, default_source)
            .with_context(|| format!("invalid spec `{}`", args.spec))?;
        let name = args.name.clone().unwrap_or_else(|| derive_name(&parsed.locator));

        // Existence guard: refuse to clobber an existing declaration unless --force.
        if cfg.tools.contains_key(&name) && !args.force {
            bail!(
                "tool `{name}` already exists in config; use `ubix upgrade {name}` to reinstall, \
                 or `ubix add --force` to overwrite its parameters"
            );
        }

        // Install first, then persist state + config so we never record a failed install.
        let record = self.install_tool(&cfg, &name, &tool)?;
        let version = record.installed_version.clone();
        locked.state.tools.insert(name.clone(), record);
        locked.save()?;

        cfg.tools.insert(name.clone(), tool);
        cfg.save(&cfg_path)?;
        // stdout: machine-facing result, augmented with the resolved version.
        println!("added `{name}` ({}) {version}", parsed.source);
        Ok(())
    }

    // ---- remove ----
    fn cmd_remove(&self, args: RemoveArgs) -> Result<()> {
        let cfg_path = self.paths.config_file();
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let mut cfg = Config::load_or_default(&cfg_path)?;

        remove::remove_tool(
            &mut cfg,
            &mut locked.state,
            self.runner.as_ref(),
            &args.name,
            args.force,
        )?;

        locked.save()?;
        cfg.save(&cfg_path)?;
        println!("removed `{}`", args.name);
        Ok(())
    }

    // ---- upgrade ----
    fn cmd_upgrade(&self, args: UpgradeArgs) -> Result<()> {
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
                println!(
                    "skip `{name}`: pinned to tag `{}` (use --force)",
                    tool.tag.as_deref().unwrap_or("")
                );
                continue;
            }
            let record = self.upgrade_tool(&cfg, &name, tool)?;
            locked.state.tools.insert(name.clone(), record);
            locked.save()?;
            println!("upgraded `{name}`");
        }
        Ok(())
    }

    // ---- sync (full reconcile: install missing + converge + prune) ----
    fn cmd_sync(&self, args: SyncArgs) -> Result<()> {
        let mut locked = LockedState::acquire(&self.paths.state_file(), args.wait)?;
        let cfg = Config::load_or_default(&self.paths.config_file())?;

        // 1) npm LTS-jump detection (§5.4): if the live fnm default differs from
        //    the recorded one, all npm tools must be reinstalled on the new node.
        let recorded_node = locked.state.runtime.node_default.clone();
        let live_node = npm::current_default_node(self.runner.as_ref());
        let lts_jumped = npm::lts_jump(recorded_node.as_deref(), live_node.as_deref());
        if lts_jumped && live_node.is_some() {
            println!(
                "node default changed {} -> {} (npm tools will be reinstalled)",
                recorded_node.as_deref().unwrap_or("none"),
                live_node.as_deref().unwrap_or("?")
            );
        }

        // 2) Orphans: in state but not config (§8.3).
        let orphans: Vec<String> = locked
            .state
            .tools
            .keys()
            .filter(|n| !cfg.tools.contains_key(*n))
            .cloned()
            .collect();
        for name in &orphans {
            if args.prune {
                if args.dry_run {
                    println!("would prune orphan `{name}`");
                } else {
                    // Prune uses a throwaway config so remove_tool can still find
                    // the source from the state record.
                    let mut throwaway = cfg.clone();
                    remove::remove_tool(
                        &mut throwaway,
                        &mut locked.state,
                        self.runner.as_ref(),
                        name,
                        false,
                    )
                    .with_context(|| format!("pruning orphan `{name}`"))?;
                    locked.save()?;
                    step!("pruning orphan `{name}`");
                    println!("pruned orphan `{name}`");
                }
            } else {
                println!("orphan `{name}`: in state but not config (use `sync --prune` to remove)");
            }
        }

        // 3) Converge declared tools.
        let mut changed = 0usize;
        for (name, tool) in &cfg.tools {
            let parsed = cfg.parsed_spec(tool)?;
            let installed = locked.state.tool(name).cloned();
            let needs = needs_install(&parsed, tool, installed.as_ref(), lts_jumped);
            if !needs {
                continue;
            }
            if args.dry_run {
                match installed {
                    Some(_) => println!("would converge `{name}` ({})", parsed.source),
                    None => println!("would install `{name}` ({})", parsed.source),
                }
                continue;
            }
            match &installed {
                Some(_) => step!("converging `{name}`"),
                None => step!("installing `{name}`"),
            }
            let record = self.install_tool(&cfg, name, tool)?;
            locked.state.tools.insert(name.clone(), record);
            locked.save()?;
            changed += 1;
            println!("synced `{name}`");
        }

        // 4) Record the (possibly new) node default after reinstalls.
        if let Some(live) = live_node {
            if locked.state.runtime.node_default.as_deref() != Some(live.as_str()) {
                locked.state.runtime.node_default = Some(live);
                if !args.dry_run {
                    locked.save()?;
                }
            }
        }

        if args.dry_run {
            println!("dry-run complete");
        } else {
            println!("sync complete: {changed} tool(s) changed");
        }
        Ok(())
    }

    // ---- list ----
    fn cmd_list(&self) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        let state = read_state_no_lock(&self.paths.state_file())?;

        if cfg.tools.is_empty() {
            println!("no tools declared");
            return Ok(());
        }
        // Columns: name, spec (encodes the source), installed version. The spec
        // column is width-aligned to the widest spec so rows stay readable.
        let rows: Vec<(String, String, String)> = cfg
            .tools
            .iter()
            .map(|(name, tool)| {
                let ver = state
                    .tools
                    .get(name)
                    .map(|r| r.installed_version.clone())
                    .unwrap_or_else(|| "(not installed)".into());
                (name.clone(), tool.spec.clone(), ver)
            })
            .collect();
        for line in format_list(&rows) {
            println!("{line}");
        }
        Ok(())
    }

    // ---- outdated (§7.1) ----
    fn cmd_outdated(&self) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        let state = read_state_no_lock(&self.paths.state_file())?;
        if cfg.tools.is_empty() {
            println!("no tools declared");
            return Ok(());
        }
        for (name, tool) in &cfg.tools {
            let parsed = match cfg.parsed_spec(tool) {
                Ok(p) => p,
                Err(e) => {
                    println!("{name:20} error: {e}");
                    continue;
                }
            };
            let installed = state
                .tools
                .get(name)
                .map(|r| r.installed_version.clone())
                .unwrap_or_else(|| "(none)".into());
            // http latest depends on the tool's `version_source` config, so it
            // is routed to the http source; everything else uses the spec-only path.
            let latest_res = if parsed.source == SourceKind::Http {
                http_source::latest(tool, self.http.as_ref())
            } else {
                outdated::latest_version(self.http.as_ref(), &parsed, tool.host.as_deref())
            };
            let latest = match latest_res {
                Ok(Latest::Version(v)) => v,
                Ok(Latest::NotApplicable) => "n/a".to_string(),
                Err(e) => format!("query-failed ({e})"),
            };
            let marker = if latest != "n/a" && latest != installed && installed != "(none)" {
                " *"
            } else {
                ""
            };
            println!("{name:20} {installed:16} -> {latest}{marker}");
        }
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
        if tool.matching.is_some() {
            // Show the value resolved for THIS platform (per-platform tables
            // resolve to the current OS/arch entry).
            match tool.resolved_matching(crate::platform::goos(), crate::platform::goarch()) {
                Ok(Some(m)) => println!("matching: {m}"),
                Ok(None) => println!("matching: (none for this platform)"),
                Err(e) => println!("matching: (unresolved: {e})"),
            }
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
        if !cfg_path.exists() {
            Config::default().save(&cfg_path)?;
        }
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        // Support editors carrying args, e.g. EDITOR="code --wait" or "vim -p".
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or("vi");
        let mut args: Vec<&str> = parts.collect();
        let path_str = cfg_path.to_string_lossy().into_owned();
        args.push(&path_str);
        // Launch interactively so the editor inherits the terminal (avoids a hang).
        let code = self
            .runner
            .run_interactive(program, &args)
            .with_context(|| format!("launching editor `{editor}`"))?;
        if code != 0 {
            bail!("editor `{editor}` exited with status {code}");
        }
        Ok(())
    }

    // ---- doctor (§8.9) ----
    fn cmd_doctor(&self) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        let install_dir = cfg.settings.install_dir_path()?;
        detail!("verbosity = {:?}", self.verbosity);
        println!("ubix doctor");
        println!("  config: {}", self.paths.config_file().display());
        println!("  state:  {}", self.paths.state_file().display());

        // PATH segments to verify (§8.9).
        let home = crate::paths::home_dir();
        let mut segments: Vec<std::path::PathBuf> = vec![
            install_dir.clone(),
            home.join(".cargo").join("bin"),
            crate::paths::expand(&cfg.settings.go_root)?.join("bin"),
        ];
        if let Some(base) = npm::detect_fnm_base(self.runner.as_ref()) {
            segments.push(npm::alias_bin_dir(&base));
        }

        for seg in &segments {
            let ok = path_contains(seg);
            println!("  [{}] {} on $PATH", if ok { "ok" } else { "!!" }, seg.display());
            if !ok {
                println!("      add to your shell rc: export PATH=\"{}:$PATH\"", seg.display());
            }
        }

        // Underlying tools.
        for tool in ["uv", "fnm", "rustup", "go", "cargo", "npm"] {
            let present = self.runner.which(tool);
            println!("  [{}] {tool}", if present { "ok" } else { "--" });
        }
        Ok(())
    }

    // ---- bootstrap ----
    fn cmd_bootstrap(&self, args: BootstrapArgs) -> Result<()> {
        let target: bootstrap::BootstrapTarget = args.target.parse()?;
        let cfg = Config::load_or_default(&self.paths.config_file())?;
        let ctx = bootstrap::BootstrapCtx {
            runner: self.runner.as_ref(),
            http: self.http.as_ref(),
            go_root: crate::paths::expand(&cfg.settings.go_root)?,
        };
        bootstrap::bootstrap(target, args.reinstall, &ctx)
    }

    // ---- sources ----
    fn cmd_sources(&self) -> Result<()> {
        for line in format_sources() {
            println!("{line}");
        }
        Ok(())
    }

    // ---- install / upgrade dispatch ----

    /// Upgrade a tool. pypi uses `uv tool upgrade`; other sources reinstall in
    /// place (which is an in-place upgrade for release/go/cargo/npm).
    fn upgrade_tool(&self, cfg: &Config, name: &str, tool: &ToolConfig) -> Result<ToolRecord> {
        let parsed = cfg.parsed_spec(tool)?;
        if parsed.source == SourceKind::Pypi {
            let install_dir = cfg.settings.install_dir_path()?;
            let outcome = uv::upgrade(tool, self.runner.as_ref(), &install_dir)?;
            let now = crate::now_iso8601();
            return Ok(ToolRecord {
                source: parsed.source.to_string(),
                installed_version: outcome.installed_version,
                locator: Some(parsed.locator.clone()),
                resolved_asset: outcome.resolved_asset,
                module: None,
                install_paths: outcome.install_paths,
                sha256: outcome.sha256,
                installed_at: Some(now.clone()),
                updated_at: Some(now),
            });
        }
        self.install_tool(cfg, name, tool)
    }

    /// Install a tool via its source handler and return a fresh state record.
    fn install_tool(&self, cfg: &Config, name: &str, tool: &ToolConfig) -> Result<ToolRecord> {
        let parsed = cfg.parsed_spec(tool)?;
        let install_dir = cfg.settings.install_dir_path()?;

        // Key step: what we're resolving, plus any pins/filters that shape it.
        let mut extras: Vec<String> = Vec::new();
        if let Some(t) = &tool.tag {
            extras.push(format!("tag={t}"));
        }
        if tool.matching.is_some() {
            if let Ok(Some(m)) =
                tool.resolved_matching(crate::platform::goos(), crate::platform::goarch())
            {
                extras.push(format!("matching={m}"));
            }
        }
        if let Some(v) = &tool.version {
            extras.push(format!("version={v}"));
        }
        let suffix = if extras.is_empty() {
            String::new()
        } else {
            format!(" ({})", extras.join(", "))
        };
        step!("resolving {}{}", tool.spec, suffix);

        let outcome = match parsed.source {
            SourceKind::Github => {
                let src = GithubSource::for_tool(
                    name.to_string(),
                    install_dir,
                    Box::new(UbiEngine::new()),
                );
                src.install(tool, self.runner.as_ref())?
            }
            SourceKind::Gitlab => {
                gitlab::install(tool, name, install_dir, &UbiEngine::new())?
            }
            SourceKind::Pypi => uv::install(tool, self.runner.as_ref(), &install_dir, false)?,
            SourceKind::Npm => npm::install(tool, self.runner.as_ref())?,
            SourceKind::Cargo => cargo::install(tool, self.runner.as_ref(), &install_dir)?,
            SourceKind::Go => go::install(tool, self.runner.as_ref(), &install_dir)?,
            SourceKind::Url => url::install(tool, self.http.as_ref(), &install_dir, name)?,
            SourceKind::Http => http_source::install(
                tool,
                self.http.as_ref(),
                self.runner.as_ref(),
                &install_dir,
                name,
            )?,
        };

        // Key step: where it landed. Verbose adds the resolved version/asset/sha.
        for p in &outcome.install_paths {
            step!("installing → {}", p.display());
        }
        detail!("resolved version = {}", outcome.installed_version);
        if let Some(asset) = &outcome.resolved_asset {
            detail!("resolved asset = {asset}");
        }
        if let Some(sha) = &outcome.sha256 {
            detail!("sha256 = {sha}");
        }

        // Record go/url modules for reference.
        let module = match parsed.source {
            SourceKind::Go => Some(parsed.locator.split('@').next().unwrap_or(&parsed.locator).to_string()),
            _ => None,
        };

        let now = crate::now_iso8601();
        Ok(ToolRecord {
            source: parsed.source.to_string(),
            installed_version: outcome.installed_version,
            locator: Some(parsed.locator.clone()),
            resolved_asset: outcome.resolved_asset,
            module,
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

/// Decide whether a declared tool needs (re)installing during sync (§8.2).
///
/// * missing from state → install.
/// * npm source and an LTS jump occurred → reinstall (§5.4).
/// * pinned `tag` (github/gitlab) differs from the installed version → converge.
/// * pinned `version` (pypi/cargo) differs from the installed version → converge.
fn needs_install(
    parsed: &crate::sources::ParsedSpec,
    tool: &ToolConfig,
    installed: Option<&ToolRecord>,
    lts_jumped: bool,
) -> bool {
    let Some(rec) = installed else {
        return true;
    };
    if parsed.source == SourceKind::Npm && lts_jumped {
        return true;
    }
    if let Some(tag) = &tool.tag {
        if &rec.installed_version != tag {
            return true;
        }
    }
    if let Some(ver) = &tool.version {
        if matches!(parsed.source, SourceKind::Pypi | SourceKind::Cargo)
            && &rec.installed_version != ver
        {
            return true;
        }
    }
    false
}

/// Format `ubix list` rows (name, spec, version) into aligned lines. The name
/// and spec columns are padded to the widest entry so output stays readable.
/// Max width of the spec column in `list`; longer specs (e.g. http templates
/// with a long URL) are truncated with `…` so alignment stays sane. Full spec
/// is always visible via `ubix info <name>`.
const LIST_SPEC_MAX: usize = 48;

/// Truncate `s` to at most `max` chars, appending `…` when shortened.
fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

fn format_list(rows: &[(String, String, String)]) -> Vec<String> {
    let name_w = rows.iter().map(|(n, _, _)| n.chars().count()).max().unwrap_or(0);
    let specs: Vec<String> = rows
        .iter()
        .map(|(_, s, _)| truncate_ellipsis(s, LIST_SPEC_MAX))
        .collect();
    // Column width = widest (already-capped) spec, so it never exceeds the cap.
    let spec_w = specs.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    rows.iter()
        .zip(specs.iter())
        .map(|((name, _, ver), spec)| format!("{name:<name_w$}  {spec:<spec_w$}  {ver}"))
        .collect()
}

/// Format the `ubix sources` table from every [`SourceKind`]. A header plus one
/// `PREFIX | BACKEND | EXAMPLE` row per source (aligned), with the summary on an
/// indented second line. Data-driven from `SourceKind::all()`.
fn format_sources() -> Vec<String> {
    let infos: Vec<_> = SourceKind::all().iter().map(|k| k.describe()).collect();
    // `prefix:` including the trailing colon for the PREFIX column.
    let prefix_col: Vec<String> = infos.iter().map(|i| format!("{}:", i.prefix)).collect();
    let prefix_w = prefix_col
        .iter()
        .map(String::len)
        .chain(std::iter::once("PREFIX".len()))
        .max()
        .unwrap_or(0);
    let backend_w = infos
        .iter()
        .map(|i| i.backend.len())
        .chain(std::iter::once("BACKEND".len()))
        .max()
        .unwrap_or(0);

    let mut out = Vec::new();
    out.push(format!(
        "{:<prefix_w$}  {:<backend_w$}  {}",
        "PREFIX", "BACKEND", "EXAMPLE"
    ));
    for (i, info) in infos.iter().enumerate() {
        out.push(format!(
            "{:<prefix_w$}  {:<backend_w$}  {}",
            prefix_col[i], info.backend, info.example
        ));
        out.push(format!("{:prefix_w$}  {}", "", info.summary));
    }
    out
}

/// Parse repeatable `k=v` CLI values into a map (used for `--arch-replace`).
fn parse_kv_pairs(pairs: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut map = std::collections::BTreeMap::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .with_context(|| format!("expected `key=value`, got `{p}`"))?;
        if k.is_empty() {
            bail!("empty key in `{p}`");
        }
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

/// Derive a tool name from a locator: last path segment, stripped of `@version`.
pub fn derive_name(locator: &str) -> String {
    let base = locator.rsplit('/').next().unwrap_or(locator);
    let base = base.split('@').next().unwrap_or(base);
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
    fn parse_kv_pairs_ok_and_errors() {
        let m = parse_kv_pairs(&["amd64=x64".to_string(), "arm64=arm64".to_string()]).unwrap();
        assert_eq!(m["amd64"], "x64");
        assert_eq!(m["arm64"], "arm64");
        assert!(parse_kv_pairs(&["novalue".to_string()]).is_err());
        assert!(parse_kv_pairs(&["=x".to_string()]).is_err());
    }

    #[test]
    fn cli_parses_http_add_flags() {
        let cli = Cli::try_parse_from([
            "ubix",
            "add",
            "http:https://h/{version}/{os}-{arch}/claude",
            "--version-source",
            "github:anthropics/claude-code",
            "--url-musl",
            "https://h/{version}/{os}-{arch}-musl/claude",
            "--arch-replace",
            "amd64=x64",
            "--exe",
            "claude",
        ])
        .unwrap();
        match cli.command {
            Command::Add(a) => {
                assert_eq!(a.version_source.as_deref(), Some("github:anthropics/claude-code"));
                assert!(a.url_musl.is_some());
                assert_eq!(a.arch_replace, vec!["amd64=x64".to_string()]);
                assert_eq!(a.exe.as_deref(), Some("claude"));
            }
            _ => panic!("expected add"),
        }
    }

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
    fn cli_add_force_defaults_false_and_parses() {
        let plain = Cli::try_parse_from(["ubix", "add", "github:owner/repo"]).unwrap();
        match plain.command {
            Command::Add(a) => assert!(!a.force, "force should default to false"),
            _ => panic!("expected add"),
        }
        let forced =
            Cli::try_parse_from(["ubix", "add", "github:owner/repo", "--force"]).unwrap();
        match forced.command {
            Command::Add(a) => assert!(a.force),
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

    #[test]
    fn cli_parses_sync_flags() {
        let cli = Cli::try_parse_from(["ubix", "sync", "--dry-run", "--prune"]).unwrap();
        match cli.command {
            Command::Sync(a) => {
                assert!(a.dry_run && a.prune);
            }
            _ => panic!("expected sync"),
        }
    }

    use crate::sources::{ParsedSpec, SourceKind};

    fn rec(version: &str) -> ToolRecord {
        ToolRecord {
            source: "github".into(),
            installed_version: version.into(),
            locator: None,
            resolved_asset: None,
            module: None,
            install_paths: vec![],
            sha256: None,
            installed_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn needs_install_when_missing() {
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let tool = ToolConfig::from_spec("github:o/r");
        assert!(needs_install(&parsed, &tool, None, false));
    }

    #[test]
    fn skip_when_present_and_unpinned() {
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let tool = ToolConfig::from_spec("github:o/r");
        assert!(!needs_install(&parsed, &tool, Some(&rec("latest")), false));
    }

    #[test]
    fn converge_when_pinned_tag_differs() {
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let mut tool = ToolConfig::from_spec("github:o/r");
        tool.tag = Some("v2".into());
        assert!(needs_install(&parsed, &tool, Some(&rec("v1")), false));
        // Same tag → skip.
        assert!(!needs_install(&parsed, &tool, Some(&rec("v2")), false));
    }

    #[test]
    fn npm_reinstalls_on_lts_jump() {
        let parsed = ParsedSpec { source: SourceKind::Npm, locator: "pnpm".into() };
        let tool = ToolConfig::from_spec("npm:pnpm");
        assert!(needs_install(&parsed, &tool, Some(&rec("latest")), true));
        assert!(!needs_install(&parsed, &tool, Some(&rec("latest")), false));
    }

    #[test]
    fn pypi_converges_on_version_change() {
        let parsed = ParsedSpec { source: SourceKind::Pypi, locator: "ruff".into() };
        let mut tool = ToolConfig::from_spec("pypi:ruff");
        tool.version = Some("0.7.0".into());
        assert!(needs_install(&parsed, &tool, Some(&rec("0.6.0")), false));
    }

    #[test]
    fn format_list_aligns_and_shows_spec_and_version() {
        let rows = vec![
            ("eza".to_string(), "github:eza-community/eza".to_string(), "v0.23.4".to_string()),
            ("ruff".to_string(), "pypi:ruff".to_string(), "(not installed)".to_string()),
        ];
        let lines = format_list(&rows);
        // Name column padded to width of "ruff" (4); spec padded to widest spec.
        assert_eq!(lines[0], "eza   github:eza-community/eza  v0.23.4");
        assert!(lines[1].starts_with("ruff  pypi:ruff"));
        assert!(lines[1].ends_with("(not installed)"));
        // Both the spec and the version appear on each line.
        assert!(lines[0].contains("github:eza-community/eza"));
        assert!(lines[1].contains("pypi:ruff"));
    }

    #[test]
    fn format_list_empty() {
        assert!(format_list(&[]).is_empty());
    }

    #[test]
    fn format_list_truncates_long_spec() {
        let long = format!("http:https://example.com/{}/bin", "x".repeat(200));
        let rows = vec![("claude".to_string(), long, "(not installed)".to_string())];
        let lines = format_list(&rows);
        // Spec is capped at LIST_SPEC_MAX chars and ends with the ellipsis.
        let spec_part = lines[0].split("  ").nth(1).unwrap();
        assert_eq!(spec_part.chars().count(), LIST_SPEC_MAX);
        assert!(spec_part.ends_with('…'));
        assert!(lines[0].ends_with("(not installed)"));
    }

    #[test]
    fn format_sources_has_header_and_every_source() {
        let lines = format_sources();
        // Header + two lines (row + summary) per source.
        assert_eq!(lines.len(), 1 + SourceKind::all().len() * 2);
        assert!(lines[0].contains("PREFIX"));
        assert!(lines[0].contains("BACKEND"));
        assert!(lines[0].contains("EXAMPLE"));
        let joined = lines.join("\n");
        // Every prefix and its example appear.
        for &k in SourceKind::all() {
            let info = k.describe();
            assert!(joined.contains(&format!("{}:", info.prefix)), "missing prefix {}", info.prefix);
            assert!(joined.contains(info.example), "missing example {}", info.example);
            assert!(joined.contains(info.backend), "missing backend {}", info.backend);
        }
        // Spot-check a couple of the required backend strings.
        assert!(joined.contains("ubi (GitHub Releases)"));
        assert!(joined.contains("cargo install --root ~/.local"));
    }

    #[test]
    fn cli_parses_sources_subcommand() {
        let cli = Cli::try_parse_from(["ubix", "sources"]).unwrap();
        assert!(matches!(cli.command, Command::Sources));
    }
}
