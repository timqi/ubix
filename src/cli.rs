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
use crate::sources::template as template_source;
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
    #[command(
        about = "Install missing, upgrade to latest, and converge pinned tools.\n\nActs on the named tools, or every declared tool with --all (one of the two is required). A tool already at its target version is skipped; --force reinstalls it. Pinned `tag`/`version` tools converge to the pin. --dry-run reports installed vs latest and the chosen action without touching anything. --prune removes orphans (in state but not config)."
    )]
    Upgrade(UpgradeArgs),
    /// List declared and installed tools.
    List,
    /// Show source, paths, and parameters for a tool.
    Info(InfoArgs),
    /// Open config.toml in $EDITOR.
    Edit,
    /// Check underlying tools and PATH readiness.
    Doctor,
    /// Bootstrap a language toolchain/runtime (rust|go|python|nodejs).
    Bootstrap(BootstrapArgs),
    /// List supported spec prefixes and their install backends.
    Sources,
    /// Search the aqua-registry and print (or add) a generated `github:` config.
    Search(SearchArgs),
    /// aqua-registry maintenance (root-index cache).
    Aqua(AquaArgs),
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
    /// (template source) alternate URL template used on Linux+musl.
    #[arg(long)]
    pub url_musl: Option<String>,
    /// (template source) where to discover the version, e.g. github:owner/repo.
    #[arg(long)]
    pub version_source: Option<String>,
    /// (template source) runtime-arch → url-token override, repeatable, e.g. --arch-replace amd64=x64.
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
    /// Tool names to upgrade/converge; omit with --all to act on every declared
    /// tool. Names may include orphans (state-only) when combined with --prune.
    pub names: Vec<String>,
    /// Act on every declared tool (plus orphans in scope).
    #[arg(long)]
    pub all: bool,
    /// Re-install even when already at the target version / pinned tag (§8.4).
    #[arg(long)]
    pub force: bool,
    /// Report installed vs latest and the chosen action without changing
    /// anything (read-only; no state lock, no install, no write).
    #[arg(long)]
    pub dry_run: bool,
    /// Remove orphaned tools in scope (state has it, config does not) (§8.3).
    #[arg(long)]
    pub prune: bool,
    /// Block waiting for the state lock instead of failing fast.
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct BootstrapArgs {
    /// Toolchain/runtime to bootstrap: <rust|go|python|nodejs>.
    pub target: String,
    #[arg(long)]
    pub reinstall: bool,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// `owner/repo` (direct lookup) or a repo-name substring (root-index search).
    pub query: String,
    /// Write the generated config and install immediately (like `add`).
    #[arg(long)]
    pub add: bool,
    /// Explicit tool name (defaults to the aqua command name / repo).
    #[arg(long)]
    pub name: Option<String>,
    /// Block waiting for the state lock instead of failing fast (with --add).
    #[arg(long)]
    pub wait: bool,
}

#[derive(Debug, Args)]
pub struct AquaArgs {
    #[command(subcommand)]
    pub command: AquaCommand,
}

#[derive(Debug, Subcommand)]
pub enum AquaCommand {
    /// Refresh the aqua-registry root-index cache used by `search`.
    Update,
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
            Command::List => self.cmd_list(),
            Command::Info(a) => self.cmd_info(a),
            Command::Edit => self.cmd_edit(),
            Command::Doctor => self.cmd_doctor(),
            Command::Bootstrap(a) => self.cmd_bootstrap(a),
            Command::Sources => self.cmd_sources(),
            Command::Search(a) => self.cmd_search(a),
            Command::Aqua(a) => self.cmd_aqua(a),
        }
    }

    // ---- add ----
    fn cmd_add(&self, args: AddArgs) -> Result<()> {
        // aqua: prefix is intercepted BEFORE parse_spec (§8): resolve the aqua
        // package into a synthesized `github:` ToolConfig, then take the normal
        // add flow. `aqua:` never reaches parse_spec/SourceKind.
        if let Some(rest) = args.spec.strip_prefix("aqua:") {
            let (owner, repo) = split_owner_repo(rest.trim())?;
            step!("resolving aqua:{owner}/{repo}");
            let (name, tool) =
                crate::aqua::resolve_package(self.http.as_ref(), &owner, &repo, args.name.as_deref())?;
            return self.persist_and_install(name, tool, args.force, args.wait);
        }

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

        let default_source = Config::load_or_default(&self.paths.config_file())?
            .settings
            .default_source_kind()?;
        let parsed = parse_spec(&args.spec, default_source)
            .with_context(|| format!("invalid spec `{}`", args.spec))?;
        let name = args.name.clone().unwrap_or_else(|| derive_name(&parsed.locator));

        self.persist_and_install(name, tool, args.force, args.wait)
    }

    /// Shared install-first-then-persist flow used by `add` and `search --add`:
    /// take the lock, guard existence, install via the tool's source, then write
    /// state + config. `tool.spec` decides the source (aqua synthesizes a
    /// `github:` spec upstream, so this stays source-agnostic).
    fn persist_and_install(
        &self,
        name: String,
        tool: ToolConfig,
        force: bool,
        wait: bool,
    ) -> Result<()> {
        let cfg_path = self.paths.config_file();
        let mut locked = LockedState::acquire(&self.paths.state_file(), wait)?;
        let mut cfg = Config::load_or_default(&cfg_path)?;

        // Resolve the source now (for the final message + validation).
        let default_source = cfg.settings.default_source_kind()?;
        let parsed = parse_spec(&tool.spec, default_source)
            .with_context(|| format!("invalid spec `{}`", tool.spec))?;

        // Existence guard: refuse to clobber an existing declaration unless --force.
        if cfg.tools.contains_key(&name) && !force {
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

    // ---- upgrade (unified converge / upgrade / report / prune) ----
    fn cmd_upgrade(&self, args: UpgradeArgs) -> Result<()> {
        let cfg = Config::load_or_default(&self.paths.config_file())?;

        // 1) Select the scope (declared to act on + orphans in scope). Preserves
        //    user input order; unknown names error.
        let cfg_keys: Vec<String> = cfg.tools.keys().cloned().collect();
        // For select_targets we need the state keys; read them up front. dry-run
        // reads without a lock; otherwise we take the write lock.
        let dry_run = args.dry_run;
        // Acquire state: read-only (no lock) on --dry-run, else the write lock.
        // We deliberately accept that a concurrent writer could change state
        // between our read and our decisions on --dry-run [C11].
        let mut locked_opt = if dry_run {
            None
        } else {
            Some(LockedState::acquire(&self.paths.state_file(), args.wait)?)
        };
        let ro_state = if dry_run {
            Some(read_state_no_lock(&self.paths.state_file())?)
        } else {
            None
        };
        // A single accessor for the current state regardless of lock mode.
        macro_rules! state {
            () => {{
                match (&locked_opt, &ro_state) {
                    (Some(l), _) => &l.state,
                    (_, Some(s)) => s,
                    _ => unreachable!("state is always present"),
                }
            }};
        }

        let state_keys: Vec<String> = state!().tools.keys().cloned().collect();
        let selection = select_targets(&cfg_keys, &state_keys, &args.names, args.all)?;

        if selection.declared.is_empty() && selection.orphans.is_empty() && !args.all {
            // select_targets only returns empty when names is empty AND !all.
            bail!("specify tool names or --all");
        }

        // 2) LTS-jump detection (§5.4): if the live fnm default differs from the
        //    recorded one, npm tools in scope must be reinstalled on the new node.
        let recorded_node = state!().runtime.node_default.clone();
        let live_node = npm::current_default_node(self.runner.as_ref());
        let lts_jumped = npm::lts_jump(recorded_node.as_deref(), live_node.as_deref());
        if lts_jumped && live_node.is_some() {
            println!(
                "node default changed {} -> {} (npm tools will be reinstalled)",
                recorded_node.as_deref().unwrap_or("none"),
                live_node.as_deref().unwrap_or("?")
            );
        }

        // 3) Backfill the real version for records stuck on the `latest` sentinel,
        //    by running the installed binary's `--version` (no reinstall, no
        //    network). Done BEFORE version comparison so the decision sees the
        //    true installed version. `!dry_run` writes it back.
        for name in &selection.declared {
            let Some(rec) = state!().tool(name) else {
                continue;
            };
            // [R1] Only backfill sentinel `"latest"`. If install_paths is empty
            // the probe cannot run — leave the sentinel in place; the action
            // decision treats an unresolved sentinel as "allow upgrade".
            if rec.installed_version != "latest" || rec.install_paths.is_empty() {
                continue;
            }
            let bin = rec.install_paths[0].clone();
            let Some(ver) = probe_binary_version(self.runner.as_ref(), &bin) else {
                continue;
            };
            if ver == "latest" {
                continue;
            }
            if dry_run {
                println!("would backfill `{name}` version: {ver}");
                continue;
            }
            if let Some(locked) = locked_opt.as_mut() {
                if let Some(rec) = locked.state.tools.get_mut(name) {
                    rec.installed_version = ver.clone();
                    rec.updated_at = Some(crate::now_iso8601());
                }
                locked.save()?;
            }
            step!("backfilled `{name}` version: {ver}");
        }

        // 4) Orphans: in state but not config (§8.3), filtered to scope. Emitted
        //    BEFORE the declared installs.
        for name in &selection.orphans {
            if args.prune {
                if dry_run {
                    println!("would prune orphan `{name}`");
                } else if let Some(locked) = locked_opt.as_mut() {
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
                println!(
                    "orphan `{name}`: in state but not config (use `upgrade --prune` to remove)"
                );
            }
        }

        // 5) Per-tool action decision + execution over declared tools.
        let mut changed = 0usize;
        for name in &selection.declared {
            let tool = &cfg.tools[name];
            let parsed = cfg.parsed_spec(tool)?;
            let installed = state!().tool(name).cloned();

            let action = self.decide_action(
                &cfg,
                &parsed,
                tool,
                installed.as_ref(),
                lts_jumped,
                args.force,
            )?;

            match action {
                UpgradeAction::Skip { reason } => {
                    if dry_run {
                        println!("{name:20} {:16} action: skip ({reason})", installed_ver(&installed));
                    } else {
                        println!("skip `{name}`: {reason}");
                    }
                }
                UpgradeAction::Install { latest } | UpgradeAction::Upgrade { latest } => {
                    let is_install = installed.is_none();
                    let verb = if is_install { "install" } else { "upgrade" };
                    if dry_run {
                        println!(
                            "{name:20} {:16} -> {} action: {verb}",
                            installed_ver(&installed),
                            latest.as_deref().unwrap_or("latest"),
                        );
                        continue;
                    }
                    match &installed {
                        Some(_) => step!("upgrading `{name}`"),
                        None => step!("installing `{name}`"),
                    }
                    // Route pypi through uv::upgrade (in-place); others reinstall.
                    let record = self.upgrade_tool(&cfg, name, tool)?;
                    if let Some(locked) = locked_opt.as_mut() {
                        locked.state.tools.insert(name.clone(), record);
                        locked.save()?;
                    }
                    changed += 1;
                    println!("{}d `{name}`", verb);
                }
            }
        }

        // 6) Record the (possibly new) node default — ONLY on `--all` and not
        //    dry-run (aligns with the former `!scoped` semantics). A scoped
        //    upgrade reinstalls just the named npm tools, not every npm tool, so
        //    leaving `node_default` unchanged lets a later `upgrade --all` still
        //    detect the LTS jump and reinstall the rest.
        if args.all && !dry_run {
            if let (Some(live), Some(locked)) = (live_node, locked_opt.as_mut()) {
                if locked.state.runtime.node_default.as_deref() != Some(live.as_str()) {
                    locked.state.runtime.node_default = Some(live);
                    locked.save()?;
                }
            }
        }

        if dry_run {
            println!("dry-run complete");
        } else {
            println!("upgrade complete: {changed} tool(s) changed");
        }
        Ok(())
    }

    /// Decide the action for one declared tool (§8.2/§8.4). All version/tag
    /// comparisons use [`same_version`] (never `!=`). Queries `latest` only when
    /// needed (unpinned github/gitlab/template/npm/go with a resolved version).
    fn decide_action(
        &self,
        _cfg: &Config,
        parsed: &crate::sources::ParsedSpec,
        tool: &ToolConfig,
        installed: Option<&ToolRecord>,
        lts_jumped: bool,
        force: bool,
    ) -> Result<UpgradeAction> {
        // No state record → install (respecting any pin). Fixes the old
        // pinned+missing bug (never skip an uninstalled pinned tool).
        let Some(rec) = installed else {
            return Ok(UpgradeAction::Install { latest: None });
        };

        // --force always reinstalls (to the target: pin if set, else latest).
        if force {
            return Ok(UpgradeAction::Upgrade { latest: None });
        }

        // npm LTS jump → reinstall all npm tools on the new node.
        if parsed.source == SourceKind::Npm && lts_jumped {
            return Ok(UpgradeAction::Upgrade { latest: None });
        }

        // Pinned tag: converge to the tag; skip once it matches.
        if let Some(tag) = &tool.tag {
            if same_version(&rec.installed_version, tag) {
                return Ok(UpgradeAction::Skip {
                    reason: format!("pinned to tag `{tag}` (use --force)"),
                });
            }
            return Ok(UpgradeAction::Upgrade { latest: Some(tag.clone()) });
        }

        // Pinned version (pypi/cargo): converge to the version; skip once matched.
        if let Some(ver) = &tool.version {
            if matches!(parsed.source, SourceKind::Pypi | SourceKind::Cargo) {
                if same_version(&rec.installed_version, ver) {
                    return Ok(UpgradeAction::Skip {
                        reason: format!("pinned to version `{ver}` (use --force)"),
                    });
                }
                return Ok(UpgradeAction::Upgrade { latest: Some(ver.clone()) });
            }
        }

        // url has no `latest` concept (§5.2): default skip, --force reinstalls
        // (handled above). No sha256 diff here (out of scope).
        if parsed.source == SourceKind::Url {
            return Ok(UpgradeAction::Skip {
                reason: "url source has no latest concept (use --force to reinstall)".to_string(),
            });
        }

        // npm/go with the unresolved `"latest"` sentinel (backfill failed): allow
        // an upgrade — never compare against the literal `"latest"` string [C1].
        if matches!(parsed.source, SourceKind::Npm | SourceKind::Go)
            && rec.installed_version == "latest"
        {
            return Ok(UpgradeAction::Upgrade { latest: None });
        }

        // Unpinned github/gitlab/template/npm/go: query latest and compare.
        // Route template through template_source::latest; others through
        // outdated::latest_version.
        let latest_res = if parsed.source == SourceKind::Template {
            template_source::latest(tool, self.http.as_ref())
        } else {
            outdated::latest_version(self.http.as_ref(), parsed, tool.host.as_deref())
        };
        let latest = match latest_res {
            Ok(Latest::Version(v)) => v,
            Ok(Latest::NotApplicable) => {
                // No latest concept for this source → nothing to compare.
                return Ok(UpgradeAction::Skip {
                    reason: "no latest version available (use --force to reinstall)".to_string(),
                });
            }
            Err(e) => {
                // Query failed → can't compare; skip conservatively (unless
                // --force, already handled). Report the reason.
                return Ok(UpgradeAction::Skip {
                    reason: format!("latest query failed ({e})"),
                });
            }
        };

        // installed version "unknown" (sentinel never backfilled) → upgrade
        // directly (skip the same-version optimization) [C1].
        if rec.installed_version == "latest" {
            return Ok(UpgradeAction::Upgrade { latest: Some(latest) });
        }

        if same_version(&rec.installed_version, &latest) {
            Ok(UpgradeAction::Skip {
                reason: format!("already at latest `{latest}`"),
            })
        } else {
            Ok(UpgradeAction::Upgrade { latest: Some(latest) })
        }
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
        use bootstrap::BootstrapTarget;
        let target: BootstrapTarget = args.target.parse()?;
        match target {
            // python/nodejs need the add/config/state machinery → handled here.
            BootstrapTarget::Python => self.cmd_bootstrap_python(args.reinstall),
            BootstrapTarget::Nodejs => self.cmd_bootstrap_nodejs(args.reinstall),
            // rust/go are pure toolchain fetches → the ctx-only bootstrap.
            BootstrapTarget::Rust | BootstrapTarget::Go => {
                let cfg = Config::load_or_default(&self.paths.config_file())?;
                let ctx = bootstrap::BootstrapCtx {
                    runner: self.runner.as_ref(),
                    http: self.http.as_ref(),
                    go_root: crate::paths::expand(&cfg.settings.go_root)?,
                };
                bootstrap::bootstrap(target, args.reinstall, &ctx)
            }
        }
    }

    /// Ensure `name` is installed via the `ubix add` path for `spec` (with the
    /// given multi-exe list), then return the resolved install_dir. Idempotent:
    /// skips the add when the tool is already in config AND on PATH, unless
    /// `reinstall`. Takes the state lock, installs, and writes config+state.
    fn ensure_added(
        &self,
        spec: &str,
        name: &str,
        exes: &[&str],
        reinstall: bool,
    ) -> Result<std::path::PathBuf> {
        let cfg_path = self.paths.config_file();
        let mut locked = LockedState::acquire(&self.paths.state_file(), false)?;
        let mut cfg = Config::load_or_default(&cfg_path)?;
        let install_dir = cfg.settings.install_dir_path()?;

        let present = cfg.tools.contains_key(name) && self.runner.which(name);
        if present && !reinstall {
            step!("`{name}` already installed (config + PATH); skipping add");
            return Ok(install_dir);
        }

        step!("installing `{name}` via {spec}");
        let mut tool = ToolConfig::from_spec(spec);
        if !exes.is_empty() {
            tool.exes = Some(exes.iter().map(|s| s.to_string()).collect());
        }
        let record = self.install_tool(&cfg, name, &tool)?;
        let version = record.installed_version.clone();
        locked.state.tools.insert(name.to_string(), record);
        locked.save()?;
        cfg.tools.insert(name.to_string(), tool);
        cfg.save(&cfg_path)?;
        println!("added `{name}` (github) {version}");
        Ok(install_dir)
    }

    // ---- bootstrap python (uv + default Python) ----
    fn cmd_bootstrap_python(&self, reinstall: bool) -> Result<()> {
        let install_dir =
            self.ensure_added("github:astral-sh/uv", "uv", &["uv", "uvx"], reinstall)?;
        run_python_runtime(self.runner.as_ref(), &install_dir)?;
        println!("bootstrapped python via uv (python/python3 in {})", install_dir.display());
        Ok(())
    }

    // ---- bootstrap nodejs (fnm + default LTS node) ----
    fn cmd_bootstrap_nodejs(&self, reinstall: bool) -> Result<()> {
        let install_dir = self.ensure_added("github:Schniz/fnm", "fnm", &[], reinstall)?;
        let default_arg = run_nodejs_runtime(self.runner.as_ref(), &install_dir)?;

        // The stable PATH entry is the fnm alias bin dir (follows LTS jumps).
        let path_hint = npm::detect_fnm_base(self.runner.as_ref())
            .map(|base| npm::alias_bin_dir(&base).display().to_string())
            .unwrap_or_else(|| "<fnm base>/aliases/default/bin".to_string());
        println!("bootstrapped nodejs via fnm; default node = {default_arg}");
        println!("add to PATH: {path_hint}");
        Ok(())
    }

    // ---- sources ----
    fn cmd_sources(&self) -> Result<()> {
        for line in format_sources() {
            println!("{line}");
        }
        Ok(())
    }

    // ---- search (aqua generator) ----
    fn cmd_search(&self, args: SearchArgs) -> Result<()> {
        // `owner/repo` → direct per-pkg lookup; otherwise a root-index search.
        let (owner, repo) = if args.query.contains('/') {
            split_owner_repo(args.query.trim())?
        } else {
            let (o, r) = self.resolve_search_query(&args.query)?;
            (o, r)
        };

        let (name, tool) =
            crate::aqua::resolve_package(self.http.as_ref(), &owner, &repo, args.name.as_deref())?;

        if args.add {
            return self.persist_and_install(name, tool, false, args.wait);
        }

        // Default: print the generated snippet + a one-line platform preview.
        print!("{}", crate::aqua::generate_snippet(&name, &tool));
        match crate::aqua::current_platform_matching(&tool) {
            Some(m) => println!(
                "# on {}-{}: matches asset containing `{m}`",
                crate::platform::goos(),
                crate::platform::goarch()
            ),
            None => println!(
                "# note: {}-{} is not among the supported platforms for this package",
                crate::platform::goos(),
                crate::platform::goarch()
            ),
        }
        Ok(())
    }

    /// Resolve a fuzzy `search <name>` query to a single `owner/repo` via the
    /// root-index cache (auto-fetching it if missing). Multiple candidates →
    /// list them and bail; none → bail.
    fn resolve_search_query(&self, query: &str) -> Result<(String, String)> {
        let cache = crate::aqua::registry::root_cache_path();
        let text = match crate::aqua::registry::read_root_cache(&cache)? {
            Some(t) => t,
            None => {
                step!("aqua root index not cached; fetching…");
                let (_, n) = crate::aqua::registry::update(self.http.as_ref())?;
                step!("cached aqua root index ({n} bytes)");
                crate::aqua::registry::read_root_cache(&cache)?
                    .context("root index cache missing after update")?
            }
        };
        let mut hits = crate::aqua::search_index(&text, query);
        match hits.len() {
            0 => bail!("no aqua package matching `{query}`"),
            1 => {
                let c = hits.remove(0);
                Ok((c.owner, c.repo))
            }
            _ => {
                // Exact repo-name match disambiguates a strong single hit.
                if let Some(exact) = hits.iter().find(|c| c.repo == query) {
                    return Ok((exact.owner.clone(), exact.repo.clone()));
                }
                let mut msg = format!("multiple aqua packages match `{query}`:\n");
                for c in &hits {
                    msg.push_str(&format!("  {}/{}\n", c.owner, c.repo));
                }
                msg.push_str("re-run with the exact `owner/repo`");
                bail!(msg);
            }
        }
    }

    // ---- aqua (root-index cache maintenance) ----
    fn cmd_aqua(&self, args: AquaArgs) -> Result<()> {
        match args.command {
            AquaCommand::Update => {
                let (path, n) = crate::aqua::registry::update(self.http.as_ref())?;
                println!("refreshed aqua root index: {} ({n} bytes)", path.display());
                Ok(())
            }
        }
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
            SourceKind::Template => template_source::install(
                tool,
                self.http.as_ref(),
                self.runner.as_ref(),
                &install_dir,
                name,
            )?,
        };

        // Key step: where it landed.
        for p in &outcome.install_paths {
            step!("installing → {}", p.display());
        }

        // Record the real version. For unpinned github/gitlab, ubi doesn't expose
        // the resolved tag (outcome is "latest"), so query the releases API once
        // to record an accurate string; falls back to the ubi value on failure.
        let installed_version = resolve_record_version(
            self.http.as_ref(),
            &parsed,
            tool.tag.as_deref(),
            tool.host.as_deref(),
            &outcome.installed_version,
        );
        detail!("resolved version = {installed_version}");
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
            installed_version,
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

/// Determine the version string to record after a successful install.
///
/// * `tag` pinned → the tag (no query).
/// * unpinned github/gitlab → query the releases API for the latest tag (ubi
///   0.9 exposes no resolved-tag getter). `Ok(Version(v))` → `v`; error or
///   `NotApplicable` → `fallback` (the install already succeeded via ubi).
/// * other sources → `fallback` unchanged (they record their own real version).
pub fn resolve_record_version(
    http: &dyn HttpClient,
    parsed: &crate::sources::ParsedSpec,
    tag: Option<&str>,
    host: Option<&str>,
    fallback: &str,
) -> String {
    if let Some(t) = tag {
        return t.to_string();
    }
    if !matches!(parsed.source, SourceKind::Github | SourceKind::Gitlab) {
        return fallback.to_string();
    }
    match outdated::latest_version(http, parsed, host) {
        Ok(Latest::Version(v)) => v,
        Ok(Latest::NotApplicable) => {
            detail!("version query returned n/a; recording `{fallback}`");
            fallback.to_string()
        }
        Err(e) => {
            detail!("version query failed ({e}); recording `{fallback}`");
            fallback.to_string()
        }
    }
}

/// Install the latest stable Python as the default via the freshly-installed
/// uv, invoked by ABSOLUTE path (`<install_dir>/uv`) since install_dir may not
/// be on PATH yet. Prefers `uv python install --default` (installs latest stable
/// and creates default `python`/`python3`); falls back to `uv python install`
/// on older uv that lacks `--default`.
fn run_python_runtime(runner: &dyn CommandRunner, install_dir: &std::path::Path) -> Result<()> {
    let uv = install_dir.join("uv");
    let uv_s = uv.to_string_lossy().into_owned();
    step!("uv python install --default (latest stable Python)…");
    let out = runner
        .run(&uv_s, &["python", "install", "--default"], &[])
        .context("running uv python install --default")?;
    if out.success() {
        return Ok(());
    }
    step!("`--default` not accepted; retrying `uv python install`…");
    let out2 = runner
        .run(&uv_s, &["python", "install"], &[])
        .context("running uv python install")?;
    if !out2.success() {
        bail!("uv python install failed: {}", out2.stderr.trim());
    }
    println!(
        "note: installed latest Python without --default (older uv); \
         `uv python install --default` unsupported here"
    );
    Ok(())
}

/// Install the latest LTS node and set it as the fnm default via the
/// freshly-installed fnm, invoked by ABSOLUTE path (`<install_dir>/fnm`).
/// Returns the argument passed to `fnm default` (the parsed `vX.Y.Z` or the
/// `lts-latest` alias fallback).
fn run_nodejs_runtime(
    runner: &dyn CommandRunner,
    install_dir: &std::path::Path,
) -> Result<String> {
    let fnm = install_dir.join("fnm");
    let fnm_s = fnm.to_string_lossy().into_owned();

    step!("fnm install --lts…");
    let out = runner
        .run(&fnm_s, &["install", "--lts"], &[])
        .context("running fnm install --lts")?;
    if !out.success() {
        bail!("fnm install --lts failed: {}", out.stderr.trim());
    }
    // Exact installed version (parsed from output), else the `lts-latest` alias.
    let version = bootstrap::parse_semver_v(&out.stdout)
        .or_else(|| bootstrap::parse_semver_v(&out.stderr));
    let default_arg = version.unwrap_or_else(|| "lts-latest".to_string());
    step!("fnm default {default_arg}…");
    let out = runner
        .run(&fnm_s, &["default", &default_arg], &[])
        .context("running fnm default")?;
    if !out.success() {
        bail!("fnm default failed: {}", out.stderr.trim());
    }
    Ok(default_arg)
}

/// Probe an installed binary's real version by running it, for backfilling
/// records stored as the `latest` sentinel. Tries `--version`, `-V`, `version`
/// in order; on each, runs `<bin> <flag>` via the runner, scans combined
/// stdout+stderr for the first semver (`v?MAJOR.MINOR.PATCH[-/+/.suffix]`), and
/// returns it (preserving a leading `v`). No network, no reinstall.
pub fn probe_binary_version(
    runner: &dyn CommandRunner,
    bin_path: &std::path::Path,
) -> Option<String> {
    let bin = bin_path.to_string_lossy();
    for flag in ["--version", "-V", "version"] {
        let Ok(out) = runner.run(&bin, &[flag], &[]) else {
            continue;
        };
        // Run the tool even on non-zero exit — some print version to stderr and
        // exit non-zero; scan whatever we got.
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        if let Some(v) = scan_semver(&combined) {
            return Some(v);
        }
    }
    None
}

/// Find the first `v?MAJOR.MINOR.PATCH[suffix]` substring in `text`. Preserves a
/// leading `v`. Hand-rolled (no regex dep).
fn scan_semver(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Optional leading `v` (only when followed by a digit).
        let has_v = bytes[i] == b'v' && i + 1 < n && bytes[i + 1].is_ascii_digit();
        let start = i;
        let mut j = if has_v { i + 1 } else { i };
        if j < n && bytes[j].is_ascii_digit() {
            // MAJOR.MINOR.PATCH — require three dot-separated digit runs.
            if let Some(end) = match_core_semver(bytes, j) {
                j = end;
                // Optional pre-release / build suffix: [-+.][0-9A-Za-z.-]+
                if j < n && matches!(bytes[j], b'-' | b'+' | b'.') {
                    let mut k = j + 1;
                    while k < n && is_suffix_byte(bytes[k]) {
                        k += 1;
                    }
                    if k > j + 1 {
                        j = k;
                    }
                }
                return Some(text[start..j].to_string());
            }
        }
        i += 1;
    }
    None
}

/// Match `DIGITS.DIGITS.DIGITS` starting at `start`; return the end index or None.
fn match_core_semver(bytes: &[u8], start: usize) -> Option<usize> {
    let n = bytes.len();
    let mut i = start;
    for part in 0..3 {
        let run_start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == run_start {
            return None; // empty numeric run
        }
        if part < 2 {
            if i >= n || bytes[i] != b'.' {
                return None; // need a dot between the first three parts
            }
            i += 1;
        }
    }
    Some(i)
}

fn is_suffix_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'-'
}

/// Compare two version strings ignoring ONE leading `v`/`V` on each, so a
/// backfilled bare `14.1.1` matches a tag `v14.1.1`.
pub fn same_version(a: &str, b: &str) -> bool {
    fn strip_v(s: &str) -> &str {
        s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s)
    }
    strip_v(a) == strip_v(b)
}

/// Render an installed version for the dry-run report, or `(none)` when unset.
fn installed_ver(installed: &Option<ToolRecord>) -> String {
    installed
        .as_ref()
        .map(|r| r.installed_version.clone())
        .unwrap_or_else(|| "(none)".into())
}

/// The per-tool action chosen by `decide_action`. `latest` carries the target
/// version string for the dry-run report (pin value or queried latest); `None`
/// means "install/reinstall to whatever the source resolves".
#[derive(Debug, PartialEq, Eq)]
pub enum UpgradeAction {
    /// Not installed → install to the target.
    Install { latest: Option<String> },
    /// Installed but out of date / forced → (re)install to the target.
    Upgrade { latest: Option<String> },
    /// Already at the target (or nothing to compare) → do nothing.
    Skip { reason: String },
}

/// Which tools an `upgrade` invocation should act on.
#[derive(Debug, PartialEq, Eq)]
pub struct TargetSelection {
    /// Declared (config) tool names to converge/upgrade, in user-input order
    /// (or config order for `--all`).
    pub declared: Vec<String>,
    /// Orphan (state-only) tool names to report/prune, in user-input order (or
    /// state order for `--all`).
    pub orphans: Vec<String>,
}

/// Compute the upgrade scope from config keys, state keys, the requested names,
/// and `--all`. `config_keys`/`state_keys` preserve iteration order.
///
/// * `all = true` (names empty) → all declared tools + all orphans (state ∖ config).
/// * `names` non-empty → classify each name in input order:
///   * in config → declared.
///   * else in state (orphan) → orphan.
///   * in neither → error `no tool \`<n>\` in config or state`.
///   Order is preserved as given by `names` [R3]. Mixed config+orphan is allowed.
/// * names empty and `all = false` → empty selection (caller errors).
pub fn select_targets(
    config_keys: &[String],
    state_keys: &[String],
    names: &[String],
    all: bool,
) -> Result<TargetSelection> {
    let in_config = |n: &str| config_keys.iter().any(|c| c == n);
    let in_state = |n: &str| state_keys.iter().any(|s| s == n);

    if names.is_empty() {
        if all {
            return Ok(TargetSelection {
                declared: config_keys.to_vec(),
                orphans: state_keys
                    .iter()
                    .filter(|k| !in_config(k))
                    .cloned()
                    .collect(),
            });
        }
        return Ok(TargetSelection {
            declared: Vec::new(),
            orphans: Vec::new(),
        });
    }

    // Named subset: classify in input order, preserving duplicates-free order.
    let mut declared = Vec::new();
    let mut orphans = Vec::new();
    for n in names {
        if in_config(n) {
            if !declared.iter().any(|d| d == n) {
                declared.push(n.clone());
            }
        } else if in_state(n) {
            if !orphans.iter().any(|o| o == n) {
                orphans.push(n.clone());
            }
        } else {
            bail!("no tool `{n}` in config or state");
        }
    }
    Ok(TargetSelection { declared, orphans })
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
/// `PREFIX | BACKEND | EXAMPLE` row per source (aligned), with the summary and
/// install location on an indented second line. Data-driven from
/// `SourceKind::all()`.
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
        out.push(format!(
            "{:prefix_w$}  {} · installs to {}",
            "", info.summary, info.location
        ));
    }
    // aqua is NOT a source kind — it is a config GENERATOR. It resolves an
    // aqua-registry package into a `github:` entry (spec + per-platform
    // matching) via `ubix add aqua:owner/repo` or `ubix search`.
    out.push(String::new());
    out.push("generator (not a source kind):".to_string());
    out.push(format!(
        "{:<prefix_w$}  {}  {}",
        "aqua:", "aqua-registry → github: config", "aqua:openai/codex"
    ));
    out.push(format!(
        "{:prefix_w$}  resolves an aqua package to a `github:` entry · via `add`/`search`",
        ""
    ));
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

/// Split an `owner/repo` string into its two non-empty segments.
pub fn split_owner_repo(s: &str) -> Result<(String, String)> {
    let segs: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if s.split('/').count() != 2 || segs.len() != 2 {
        bail!("expected `owner/repo`, got `{s}`");
    }
    Ok((segs[0].to_string(), segs[1].to_string()))
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
    fn cli_parses_template_add_flags() {
        let cli = Cli::try_parse_from([
            "ubix",
            "add",
            "template:https://h/{version}/{os}-{arch}/claude",
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
    fn cli_parses_upgrade_flags() {
        let cli =
            Cli::try_parse_from(["ubix", "upgrade", "--all", "--dry-run", "--prune"]).unwrap();
        match cli.command {
            Command::Upgrade(a) => {
                assert!(a.all && a.dry_run && a.prune);
                assert!(a.names.is_empty());
            }
            _ => panic!("expected upgrade"),
        }
    }

    #[test]
    fn cli_parses_upgrade_multi_names() {
        // `upgrade foo bar` → two variadic names.
        match Cli::try_parse_from(["ubix", "upgrade", "foo", "bar"]).unwrap().command {
            Command::Upgrade(a) => {
                assert_eq!(a.names, vec!["foo".to_string(), "bar".to_string()]);
                assert!(!a.all);
            }
            _ => panic!("expected upgrade"),
        }
        // `upgrade foo --force` → name + flag.
        match Cli::try_parse_from(["ubix", "upgrade", "foo", "--force"]).unwrap().command {
            Command::Upgrade(a) => {
                assert_eq!(a.names, vec!["foo".to_string()]);
                assert!(a.force);
            }
            _ => panic!("expected upgrade"),
        }
        // Bare `upgrade` → no names, no --all (cmd errors at runtime).
        match Cli::try_parse_from(["ubix", "upgrade"]).unwrap().command {
            Command::Upgrade(a) => {
                assert!(a.names.is_empty() && !a.all);
            }
            _ => panic!("expected upgrade"),
        }
    }

    #[test]
    fn select_targets_all_is_config_plus_orphans() {
        let cfg = vec!["eza".to_string(), "ruff".to_string()];
        let state = vec!["eza".to_string(), "orphan".to_string()];
        let sel = select_targets(&cfg, &state, &[], true).unwrap();
        assert_eq!(sel.declared, vec!["eza", "ruff"]);
        assert_eq!(sel.orphans, vec!["orphan"]);
    }

    #[test]
    fn select_targets_known_config_name() {
        let cfg = vec!["eza".to_string(), "ruff".to_string()];
        let state = vec!["eza".to_string()];
        let sel = select_targets(&cfg, &state, &["ruff".to_string()], false).unwrap();
        assert_eq!(sel.declared, vec!["ruff"]);
        assert!(sel.orphans.is_empty());
    }

    #[test]
    fn select_targets_orphan_name() {
        let cfg = vec!["eza".to_string()];
        let state = vec!["eza".to_string(), "gone".to_string()];
        let sel = select_targets(&cfg, &state, &["gone".to_string()], false).unwrap();
        assert!(sel.declared.is_empty());
        assert_eq!(sel.orphans, vec!["gone"]);
    }

    #[test]
    fn select_targets_unknown_name_errors() {
        let cfg = vec!["eza".to_string()];
        let state = vec!["eza".to_string()];
        let err = select_targets(&cfg, &state, &["nope".to_string()], false).unwrap_err();
        assert!(err.to_string().contains("no tool `nope` in config or state"), "{err}");
    }

    #[test]
    fn select_targets_mixed_config_and_orphan_preserves_order() {
        // tool1 ∈ config, tool2 ∈ orphan; order must follow the input names.
        let cfg = vec!["tool1".to_string(), "zzz".to_string()];
        let state = vec!["tool2".to_string()];
        let sel = select_targets(
            &cfg,
            &state,
            &["tool1".to_string(), "tool2".to_string()],
            false,
        )
        .unwrap();
        assert_eq!(sel.declared, vec!["tool1"]);
        assert_eq!(sel.orphans, vec!["tool2"]);
    }

    #[test]
    fn select_targets_empty_no_all_is_empty() {
        let cfg = vec!["eza".to_string()];
        let state = vec!["eza".to_string()];
        let sel = select_targets(&cfg, &state, &[], false).unwrap();
        assert!(sel.declared.is_empty() && sel.orphans.is_empty());
    }

    use crate::sources::{ParsedSpec, SourceKind};

    #[test]
    fn resolve_record_version_tag_pin_no_query() {
        use crate::http::MockHttp;
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        // MockHttp has no canned responses; a query would error. Tag pin must not
        // query, so this returns the tag.
        let http = MockHttp::new();
        assert_eq!(
            resolve_record_version(&http, &parsed, Some("v1.2.3"), None, "latest"),
            "v1.2.3"
        );
    }

    #[test]
    fn resolve_record_version_github_unpinned_queries_latest() {
        use crate::http::MockHttp;
        let parsed = ParsedSpec {
            source: SourceKind::Github,
            locator: "eza-community/eza".into(),
        };
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/eza-community/eza/releases/latest",
            r#"{"tag_name":"v0.23.4"}"#,
        );
        assert_eq!(
            resolve_record_version(&http, &parsed, None, None, "latest"),
            "v0.23.4"
        );
    }

    #[test]
    fn resolve_record_version_github_query_error_falls_back() {
        use crate::http::MockHttp;
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        // No canned response → query errors → fallback preserved.
        let http = MockHttp::new();
        assert_eq!(
            resolve_record_version(&http, &parsed, None, None, "latest"),
            "latest"
        );
    }

    #[test]
    fn resolve_record_version_non_release_source_unchanged() {
        use crate::http::MockHttp;
        let parsed = ParsedSpec { source: SourceKind::Pypi, locator: "ruff".into() };
        // pypi never queries here; returns fallback unchanged (uv reports the real one).
        let http = MockHttp::new();
        assert_eq!(
            resolve_record_version(&http, &parsed, None, None, "0.6.9"),
            "0.6.9"
        );
    }

    #[test]
    fn resolve_record_version_gitlab_unpinned_queries_with_host() {
        use crate::http::MockHttp;
        let parsed = ParsedSpec {
            source: SourceKind::Gitlab,
            locator: "group/sub/repo".into(),
        };
        let http = MockHttp::new().with_text(
            "https://gitlab.fish/api/v4/projects/group%2Fsub%2Frepo/releases",
            r#"[{"tag_name":"v3.1.0"}]"#,
        );
        assert_eq!(
            resolve_record_version(&http, &parsed, None, Some("https://gitlab.fish"), "latest"),
            "v3.1.0"
        );
    }

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

    // ---- action decision (decide_action) ----

    use crate::http::MockHttp;

    /// Build a test `App` with a mock http client and runner. Paths point at a
    /// throwaway dir; `decide_action` only touches http/runner (never the FS).
    fn test_app(http: MockHttp) -> App {
        App {
            paths: Paths { config_dir: "/tmp/ubix-test".into(), data_dir: "/tmp/ubix-test".into() },
            runner: Box::new(MockRunner::new()),
            http: Box::new(http),
            verbosity: crate::progress::Verbosity::Quiet,
        }
    }

    /// Run `decide_action` for a tool with no config/state extras beyond `tool`.
    fn decide(
        app: &App,
        parsed: &ParsedSpec,
        tool: &ToolConfig,
        installed: Option<&ToolRecord>,
        lts_jumped: bool,
        force: bool,
    ) -> UpgradeAction {
        let cfg = Config::default();
        app.decide_action(&cfg, parsed, tool, installed, lts_jumped, force).unwrap()
    }

    #[test]
    fn action_missing_pinned_installs() {
        // Pinned+missing must install (fixes the old skip-when-pinned bug).
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let mut tool = ToolConfig::from_spec("github:o/r");
        tool.tag = Some("v2".into());
        assert_eq!(
            decide(&app, &parsed, &tool, None, false, false),
            UpgradeAction::Install { latest: None }
        );
    }

    #[test]
    fn action_pinned_tag_same_version_skips() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let mut tool = ToolConfig::from_spec("github:o/r");
        // Installed bare `1.0.0` vs tag `v1.0.0` → same_version → skip.
        tool.tag = Some("v1.0.0".into());
        match decide(&app, &parsed, &tool, Some(&rec("1.0.0")), false, false) {
            UpgradeAction::Skip { .. } => {}
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn action_pinned_tag_differs_upgrades() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let mut tool = ToolConfig::from_spec("github:o/r");
        tool.tag = Some("v2".into());
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("v1")), false, false),
            UpgradeAction::Upgrade { latest: Some("v2".into()) }
        );
    }

    #[test]
    fn action_force_reinstalls_even_when_pinned_match() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Github, locator: "o/r".into() };
        let mut tool = ToolConfig::from_spec("github:o/r");
        tool.tag = Some("v1".into());
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("v1")), false, true),
            UpgradeAction::Upgrade { latest: None }
        );
    }

    #[test]
    fn action_pypi_pinned_version_converges() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Pypi, locator: "ruff".into() };
        let mut tool = ToolConfig::from_spec("pypi:ruff");
        tool.version = Some("0.7.0".into());
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("0.6.0")), false, false),
            UpgradeAction::Upgrade { latest: Some("0.7.0".into()) }
        );
        // Same version → skip.
        match decide(&app, &parsed, &tool, Some(&rec("0.7.0")), false, false) {
            UpgradeAction::Skip { .. } => {}
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn action_npm_lts_jump_reinstalls() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Npm, locator: "pnpm".into() };
        let tool = ToolConfig::from_spec("npm:pnpm");
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("latest")), true, false),
            UpgradeAction::Upgrade { latest: None }
        );
    }

    #[test]
    fn action_npm_sentinel_latest_allows_upgrade() {
        // npm record stuck on the literal `"latest"` sentinel (backfill failed):
        // allow an upgrade — never compare against the literal string.
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec { source: SourceKind::Npm, locator: "pnpm".into() };
        let tool = ToolConfig::from_spec("npm:pnpm");
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("latest")), false, false),
            UpgradeAction::Upgrade { latest: None }
        );
    }

    #[test]
    fn action_go_sentinel_latest_allows_upgrade() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec {
            source: SourceKind::Go,
            locator: "example.com/cmd/tool".into(),
        };
        let tool = ToolConfig::from_spec("go:example.com/cmd/tool@latest");
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("latest")), false, false),
            UpgradeAction::Upgrade { latest: None }
        );
    }

    #[test]
    fn action_unpinned_github_same_latest_skips() {
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/eza-community/eza/releases/latest",
            r#"{"tag_name":"v0.23.4"}"#,
        );
        let app = test_app(http);
        let parsed = ParsedSpec {
            source: SourceKind::Github,
            locator: "eza-community/eza".into(),
        };
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        // Installed bare `0.23.4` vs latest `v0.23.4` → same_version → skip.
        match decide(&app, &parsed, &tool, Some(&rec("0.23.4")), false, false) {
            UpgradeAction::Skip { .. } => {}
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn action_unpinned_github_diff_latest_upgrades() {
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/eza-community/eza/releases/latest",
            r#"{"tag_name":"v0.23.4"}"#,
        );
        let app = test_app(http);
        let parsed = ParsedSpec {
            source: SourceKind::Github,
            locator: "eza-community/eza".into(),
        };
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("v0.20.0")), false, false),
            UpgradeAction::Upgrade { latest: Some("v0.23.4".into()) }
        );
    }

    #[test]
    fn action_unpinned_github_unknown_installed_upgrades() {
        // installed sentinel `"latest"` never backfilled → upgrade directly.
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/eza-community/eza/releases/latest",
            r#"{"tag_name":"v0.23.4"}"#,
        );
        let app = test_app(http);
        let parsed = ParsedSpec {
            source: SourceKind::Github,
            locator: "eza-community/eza".into(),
        };
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("latest")), false, false),
            UpgradeAction::Upgrade { latest: Some("v0.23.4".into()) }
        );
    }

    #[test]
    fn action_url_skips_unless_forced() {
        let app = test_app(MockHttp::new());
        let parsed = ParsedSpec {
            source: SourceKind::Url,
            locator: "https://x/y.tar.gz".into(),
        };
        let tool = ToolConfig::from_spec("url:https://x/y.tar.gz");
        match decide(&app, &parsed, &tool, Some(&rec("1.0.0")), false, false) {
            UpgradeAction::Skip { .. } => {}
            other => panic!("expected skip, got {other:?}"),
        }
        assert_eq!(
            decide(&app, &parsed, &tool, Some(&rec("1.0.0")), false, true),
            UpgradeAction::Upgrade { latest: None }
        );
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
        let long = format!("template:https://example.com/{}/bin", "x".repeat(200));
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
        // Header + two lines (row + summary) per source, plus the aqua generator
        // note block (blank + heading + 2 lines).
        assert_eq!(lines.len(), 1 + SourceKind::all().len() * 2 + 4);
        // aqua is documented as a generator, NOT a SourceKind.
        let joined_all = lines.join("\n");
        assert!(joined_all.contains("generator (not a source kind)"));
        assert!(joined_all.contains("aqua:openai/codex"));
        assert!(lines[0].contains("PREFIX"));
        assert!(lines[0].contains("BACKEND"));
        assert!(lines[0].contains("EXAMPLE"));
        let joined = lines.join("\n");
        // Every prefix, example, backend, and install location appear.
        for &k in SourceKind::all() {
            let info = k.describe();
            assert!(joined.contains(&format!("{}:", info.prefix)), "missing prefix {}", info.prefix);
            assert!(joined.contains(info.example), "missing example {}", info.example);
            assert!(joined.contains(info.backend), "missing backend {}", info.backend);
            assert!(joined.contains(info.location), "missing location {}", info.location);
        }
        // The location is rendered as an "installs to <location>" suffix.
        assert!(joined.contains("installs to "));
        // Spot-check a couple of the required backend strings.
        assert!(joined.contains("ubi (GitHub Releases)"));
        assert!(joined.contains("cargo install --root ~/.local"));
        // Spot-check locations: the default install_dir and npm's fnm alias path.
        assert!(joined.contains("~/.local/bin"));
        assert!(joined.contains("~/.local/share/fnm/aliases/default/bin"));
    }

    #[test]
    fn cli_parses_sources_subcommand() {
        let cli = Cli::try_parse_from(["ubix", "sources"]).unwrap();
        assert!(matches!(cli.command, Command::Sources));
    }

    #[test]
    fn cli_parses_search_flags() {
        let cli = Cli::try_parse_from(["ubix", "search", "codex", "--add", "--name", "cx"]).unwrap();
        match cli.command {
            Command::Search(a) => {
                assert_eq!(a.query, "codex");
                assert!(a.add);
                assert_eq!(a.name.as_deref(), Some("cx"));
            }
            _ => panic!("expected search"),
        }
    }

    #[test]
    fn cli_parses_aqua_update() {
        let cli = Cli::try_parse_from(["ubix", "aqua", "update"]).unwrap();
        match cli.command {
            Command::Aqua(a) => assert!(matches!(a.command, AquaCommand::Update)),
            _ => panic!("expected aqua"),
        }
    }

    #[test]
    fn split_owner_repo_ok_and_errors() {
        assert_eq!(split_owner_repo("openai/codex").unwrap(), ("openai".into(), "codex".into()));
        assert!(split_owner_repo("codex").is_err());
        assert!(split_owner_repo("a/b/c").is_err());
    }

    // ---- bootstrap python/nodejs runtime command construction ----
    use crate::runner::{CommandOutput, MockRunner};
    use std::path::Path;

    fn ok_out(stdout: &str) -> CommandOutput {
        CommandOutput { status: 0, stdout: stdout.into(), stderr: String::new() }
    }

    #[test]
    fn nodejs_runtime_absolute_path_and_parsed_version() {
        let dir = "/home/u/.local/bin";
        // fnm invoked by ABSOLUTE path; version parsed from install output.
        let runner = MockRunner::new()
            .expect(
                "/home/u/.local/bin/fnm install --lts",
                ok_out("Installing Node v22.14.0 (x64)\n"),
            )
            .expect("/home/u/.local/bin/fnm default v22.14.0", ok_out(""));
        let arg = run_nodejs_runtime(&runner, Path::new(dir)).unwrap();
        assert_eq!(arg, "v22.14.0");
        // Verify arg order + absolute path of the recorded calls.
        let calls = runner.calls.borrow();
        assert_eq!(calls[0].program, "/home/u/.local/bin/fnm");
        assert_eq!(calls[0].args, vec!["install", "--lts"]);
        assert_eq!(calls[1].program, "/home/u/.local/bin/fnm");
        assert_eq!(calls[1].args, vec!["default", "v22.14.0"]);
    }

    #[test]
    fn nodejs_runtime_falls_back_to_lts_latest_alias() {
        let dir = "/opt/bin";
        let runner = MockRunner::new()
            .expect("/opt/bin/fnm install --lts", ok_out("done, no version line\n"))
            .expect("/opt/bin/fnm default lts-latest", ok_out(""));
        let arg = run_nodejs_runtime(&runner, Path::new(dir)).unwrap();
        assert_eq!(arg, "lts-latest");
    }

    #[test]
    fn python_runtime_uses_uv_python_install_default() {
        let dir = "/home/u/.local/bin";
        let runner = MockRunner::new()
            .expect("/home/u/.local/bin/uv python install --default", ok_out(""));
        run_python_runtime(&runner, Path::new(dir)).unwrap();
        let calls = runner.calls.borrow();
        assert_eq!(calls[0].program, "/home/u/.local/bin/uv");
        assert_eq!(calls[0].args, vec!["python", "install", "--default"]);
    }

    #[test]
    fn python_runtime_falls_back_without_default_flag() {
        let dir = "/home/u/.local/bin";
        // `--default` fails (status != 0) → retry plain `uv python install`.
        let runner = MockRunner::new()
            .expect(
                "/home/u/.local/bin/uv python install --default",
                CommandOutput { status: 2, stdout: String::new(), stderr: "unexpected argument".into() },
            )
            .expect("/home/u/.local/bin/uv python install", ok_out(""));
        run_python_runtime(&runner, Path::new(dir)).unwrap();
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].args, vec!["python", "install"]);
    }

    // ---- version backfill helpers ----

    #[test]
    fn probe_version_from_version_flag() {
        let runner = MockRunner::new()
            .expect("/bin/rg --version", ok_out("ripgrep 14.1.1\nfeatures:+pcre2\n"));
        assert_eq!(
            probe_binary_version(&runner, Path::new("/bin/rg")).as_deref(),
            Some("14.1.1")
        );
    }

    #[test]
    fn probe_version_falls_through_to_dash_capital_v() {
        // `--version` yields no semver; `-V` does.
        let runner = MockRunner::new()
            .expect("/bin/tool --version", ok_out("no version info here\n"))
            .expect("/bin/tool -V", ok_out("tool version 2.0.5\n"));
        assert_eq!(
            probe_binary_version(&runner, Path::new("/bin/tool")).as_deref(),
            Some("2.0.5")
        );
    }

    #[test]
    fn probe_version_prerelease_and_leading_v() {
        let rc = MockRunner::new().expect("/b/x --version", ok_out("x 1.2.3-rc.1\n"));
        assert_eq!(probe_binary_version(&rc, Path::new("/b/x")).as_deref(), Some("1.2.3-rc.1"));
        // Preserves a leading `v`.
        let v = MockRunner::new().expect("/b/eza --version", ok_out("eza v0.23.4\n"));
        assert_eq!(probe_binary_version(&v, Path::new("/b/eza")).as_deref(), Some("v0.23.4"));
    }

    #[test]
    fn probe_version_none_when_no_semver() {
        let runner = MockRunner::new()
            .expect("/b/x --version", ok_out("unknown"))
            .expect("/b/x -V", ok_out("still nothing"))
            .expect("/b/x version", ok_out("nope 1.2 only two parts"));
        assert_eq!(probe_binary_version(&runner, Path::new("/b/x")), None);
    }

    #[test]
    fn probe_version_reads_stderr_too() {
        // Some tools print version to stderr (and may exit non-zero).
        let runner = MockRunner::new().expect(
            "/b/x --version",
            CommandOutput { status: 1, stdout: String::new(), stderr: "x 3.4.5\n".into() },
        );
        assert_eq!(probe_binary_version(&runner, Path::new("/b/x")).as_deref(), Some("3.4.5"));
    }

    #[test]
    fn same_version_ignores_leading_v() {
        assert!(same_version("v1.2.3", "1.2.3"));
        assert!(same_version("1.2.3", "v1.2.3"));
        assert!(same_version("v1.2.3", "v1.2.3"));
        assert!(!same_version("1.2.3", "1.2.4"));
        assert!(!same_version("v1.2.3", "1.2.4"));
    }
}
