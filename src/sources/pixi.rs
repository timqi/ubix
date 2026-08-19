//! pixi (conda) source via `pixi global` (§5.x).
//!
//! pixi installs global tools into per-package conda environments under
//! `$PIXI_HOME` (default `~/.pixi`) and exposes their entry points as
//! trampolines in `$PIXI_HOME/bin` — NOT into ubix's `install_dir`. pixi has no
//! bin-dir redirect (unlike uv's `UV_TOOL_BIN_DIR`), so we let pixi own its bin
//! dir and TRACK the exposed path there. Removal MUST therefore go through
//! `pixi global uninstall` — never `rm` the trampoline (that leaks the env).
//!
//! Locators may be channel-qualified with conda's `channel::name` syntax
//! (e.g. `pixi:bioconda::samtools`); a bare name defaults to conda-forge. The
//! channel flows verbatim into `pixi global install` and into the prefix.dev
//! latest-version query (see `outdated`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// The pixi bin dir where trampolines are exposed: `$PIXI_HOME/bin`, default
/// `~/.pixi/bin`. Honors `PIXI_HOME` so a custom pixi layout is tracked correctly.
pub fn pixi_bin_dir() -> PathBuf {
    pixi_bin_dir_from(std::env::var("PIXI_HOME").ok().as_deref())
}

/// Pure core of [`pixi_bin_dir`]: `$PIXI_HOME/bin` or `~/.pixi/bin`. Split out so
/// it is testable without mutating the process environment.
fn pixi_bin_dir_from(pixi_home: Option<&str>) -> PathBuf {
    match pixi_home {
        Some(h) if !h.trim().is_empty() => PathBuf::from(h).join("bin"),
        _ => crate::paths::home_dir().join(".pixi").join("bin"),
    }
}

/// Split a locator into `(channel, bare_name)`. `channel::name` → that channel;
/// a bare `name` → the default conda-forge channel. A trailing `=version` /
/// MatchSpec `[...]` is stripped from the name.
pub fn split_channel(locator: &str) -> (String, String) {
    let (channel, rest) = match locator.split_once("::") {
        Some((ch, rest)) => (ch.trim().to_string(), rest),
        None => (crate::prefix_dev::DEFAULT_CHANNEL.to_string(), locator),
    };
    (channel, strip_pin(rest).to_string())
}

/// Resolve a channel name to what `pixi --channel` needs. conda-forge is on
/// anaconda.org (pixi's default host) so its bare name works; every OTHER channel
/// we surface comes from prefix.dev (conda-forge, bioconda, robostack, and
/// prefix.dev-only channels like `github-releases` are all hosted there), and a
/// bare name would wrongly resolve against anaconda.org — so use the full URL.
/// A channel already given as a URL passes through untouched.
fn channel_url(channel: &str) -> String {
    if channel.contains("://") || channel == crate::prefix_dev::DEFAULT_CHANNEL {
        channel.to_string()
    } else {
        format!("https://prefix.dev/{channel}")
    }
}

/// The bare package name (no channel, no version) — used for the exposed
/// binary name, `pixi global update`, and `pixi global uninstall`.
fn bare_name(locator: &str) -> String {
    split_channel(locator).1
}

/// Strip a trailing conda MatchSpec version/build constraint and whitespace.
fn strip_pin(s: &str) -> &str {
    s.split(['=', '[', ' ', '\t']).next().unwrap_or(s).trim()
}

/// Build the `pixi global install` argument vector (pure; testable).
///
/// `pixi global install <pkg>[=version] [--channel URL] [--with X]...`. The
/// package is the BARE name and a non-conda-forge channel is passed via
/// `--channel <prefix.dev URL>` — NOT as a `channel::pkg` matchspec, which pixi
/// resolves against anaconda.org (breaking prefix.dev-only channels like
/// `github-releases`). Version pins use conda MatchSpec syntax (`pkg=version`).
pub fn install_args(tool: &ToolConfig, locator: &str) -> Vec<String> {
    let (channel, name) = split_channel(locator);
    let mut pkg = name;
    if let Some(v) = &tool.version {
        pkg = format!("{pkg}={v}");
    }
    let mut args = vec!["global".to_string(), "install".to_string(), pkg];
    if channel != crate::prefix_dev::DEFAULT_CHANNEL {
        args.push("--channel".to_string());
        args.push(channel_url(&channel));
    }
    if let Some(withs) = &tool.with {
        for w in withs {
            args.push("--with".to_string());
            args.push(w.clone());
        }
    }
    args
}

/// `pixi global update <pkg>` (channel/version stripped — pixi keys on env name).
pub fn upgrade_args(locator: &str) -> Vec<String> {
    vec!["global".into(), "update".into(), bare_name(locator)]
}

/// `pixi global uninstall <pkg>` — the ONLY safe removal path.
pub fn uninstall_args(locator: &str) -> Vec<String> {
    vec!["global".into(), "uninstall".into(), bare_name(locator)]
}

const PIXI_MISSING: &str =
    "`pixi` not found; install it with:\n    ubix bootstrap pixi\n    (or: ubix add prefix-dev/pixi)";

/// `pixi global list --json` — reports each global environment's exposed
/// trampoline names.
pub fn list_args() -> Vec<String> {
    vec!["global".into(), "list".into(), "--json".into()]
}

/// Extract the trampoline names pixi exposed for the global environment `env`,
/// from the JSON of `pixi global list --json`.
///
/// A conda package's executables are NOT necessarily its package name:
/// `bubblewrap` exposes `bwrap`, `vim` exposes `ex`/`view`/`vim`/`xxd`. The
/// environment is keyed by name (which pixi derives from the package), and the
/// `exposed[].exposed_name` entries are the actual files in `$PIXI_HOME/bin`.
/// Returns empty when the env is absent, so callers can fall back.
pub fn exposed_names(json: &str, env: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(envs) = v.as_array() else { return Vec::new() };
    let Some(entry) = envs
        .iter()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(env))
    else {
        return Vec::new();
    };
    entry
        .get("exposed")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("exposed_name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the trampoline paths pixi dropped for `locator`. Best-effort: any
/// failure falls back to the package name (the old, often-wrong assumption).
fn tracked_paths(runner: &dyn CommandRunner, locator: &str) -> Vec<PathBuf> {
    let env = bare_name(locator);
    let args = list_args();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let discovered = match runner.run("pixi", &refs, &[]) {
        Ok(out) if out.success() => exposed_names(&out.stdout, &env),
        _ => Vec::new(),
    };
    let names = if discovered.is_empty() {
        vec![env.clone()]
    } else {
        crate::sources::order_exes(discovered, &env)
    };
    let dir = pixi_bin_dir();
    names.iter().map(|n| dir.join(n)).collect()
}

/// Install a pixi tool via `pixi global install`. The tracked install paths are
/// the trampolines pixi drops into `$PIXI_HOME/bin`, whose names come from the
/// package's exposed entry points — not from the package name.
pub fn install(tool: &ToolConfig, runner: &dyn CommandRunner) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Pixi)?;
    if parsed.source != SourceKind::Pixi {
        bail!("pixi source received non-pixi spec `{}`", tool.spec);
    }
    if !runner.which("pixi") {
        bail!("{PIXI_MISSING}");
    }
    let args = install_args(tool, &parsed.locator);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::step!("pixi global install {}…", strip_pin(&parsed.locator));
    let out = runner
        .run("pixi", &arg_refs, &[])
        .context("running pixi global install")?;
    if !out.success() {
        bail!("pixi global install failed: {}", out.stderr.trim());
    }
    Ok(InstallOutcome {
        installed_version: tool.version.clone().unwrap_or_else(|| "latest".into()),
        resolved_asset: None,
        install_paths: tracked_paths(runner, &parsed.locator),
        sha256: None,
    })
}

/// Upgrade a pixi tool via `pixi global update <pkg>`.
pub fn upgrade(tool: &ToolConfig, runner: &dyn CommandRunner) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Pixi)?;
    if !runner.which("pixi") {
        bail!("{PIXI_MISSING}");
    }
    let args = upgrade_args(&parsed.locator);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::step!("pixi global update {}…", bare_name(&parsed.locator));
    let out = runner
        .run("pixi", &refs, &[])
        .context("running pixi global update")?;
    if !out.success() {
        bail!("pixi global update failed: {}", out.stderr.trim());
    }
    Ok(InstallOutcome {
        installed_version: tool.version.clone().unwrap_or_else(|| "latest".into()),
        resolved_asset: None,
        install_paths: tracked_paths(runner, &parsed.locator),
        sha256: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_channel_defaults_to_conda_forge() {
        assert_eq!(split_channel("ripgrep"), ("conda-forge".into(), "ripgrep".into()));
        assert_eq!(split_channel("bioconda::samtools"), ("bioconda".into(), "samtools".into()));
        assert_eq!(split_channel("bioconda::samtools=1.2"), ("bioconda".into(), "samtools".into()));
    }

    #[test]
    fn plain_install_args() {
        let t = ToolConfig::from_spec("pixi:ripgrep");
        assert_eq!(install_args(&t, "ripgrep"), vec!["global", "install", "ripgrep"]);
    }

    #[test]
    fn install_args_nondefault_channel_uses_prefix_dev_url() {
        let mut t = ToolConfig::from_spec("pixi:bioconda::samtools");
        t.version = Some("1.23.1".into());
        // Bare package name + explicit prefix.dev channel URL (NOT a
        // `bioconda::samtools` matchspec, which pixi resolves via anaconda.org).
        assert_eq!(
            install_args(&t, "bioconda::samtools"),
            vec!["global", "install", "samtools=1.23.1", "--channel", "https://prefix.dev/bioconda"]
        );
    }

    #[test]
    fn install_args_conda_forge_has_no_channel_flag() {
        let t = ToolConfig::from_spec("pixi:ripgrep");
        // conda-forge (default) → bare name, pixi's default host resolves it.
        assert_eq!(install_args(&t, "ripgrep"), vec!["global", "install", "ripgrep"]);
    }

    #[test]
    fn install_args_prefix_dev_only_channel() {
        // github-releases lives on prefix.dev, not anaconda.org — this is the bug
        // that failed with `Package 'neovim' requested unavailable channel`.
        let t = ToolConfig::from_spec("pixi:github-releases::neovim");
        assert_eq!(
            install_args(&t, "github-releases::neovim"),
            vec!["global", "install", "neovim", "--channel", "https://prefix.dev/github-releases"]
        );
    }

    #[test]
    fn install_args_with_extra_deps() {
        let mut t = ToolConfig::from_spec("pixi:ipython");
        t.with = Some(vec!["numpy".into()]);
        assert_eq!(
            install_args(&t, "ipython"),
            vec!["global", "install", "ipython", "--with", "numpy"]
        );
    }

    #[test]
    fn upgrade_uninstall_strip_channel_and_pin() {
        assert_eq!(upgrade_args("bioconda::samtools=1.2"), vec!["global", "update", "samtools"]);
        assert_eq!(uninstall_args("conda-forge::ripgrep"), vec!["global", "uninstall", "ripgrep"]);
    }

    #[test]
    fn install_requires_pixi_present() {
        let runner = crate::runner::MockRunner::new(); // pixi not present
        let t = ToolConfig::from_spec("pixi:ripgrep");
        let err = install(&t, &runner).unwrap_err();
        assert!(err.to_string().contains("ubix bootstrap pixi"), "{err}");
    }

    #[test]
    fn install_runs_pixi_and_tracks_bin() {
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new()
            .with_present("pixi")
            .expect(
                "pixi global install samtools --channel https://prefix.dev/bioconda",
                CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            )
            .expect(
                "pixi global list --json",
                CommandOutput {
                    status: 0,
                    stdout: r#"[{"name":"samtools","exposed":[{"exposed_name":"samtools","executable":"samtools"}]}]"#
                        .into(),
                    stderr: String::new(),
                },
            );
        let t = ToolConfig::from_spec("pixi:bioconda::samtools");
        let out = install(&t, &runner).unwrap();
        // Tracked binary is the exposed name (channel stripped) in the pixi bin dir.
        assert_eq!(out.install_paths.len(), 1);
        assert!(out.install_paths[0].ends_with("samtools"), "{:?}", out.install_paths);
        let calls = runner.calls.borrow();
        assert!(calls.iter().any(|c| c.program == "pixi"
            && c.args
                == ["global", "install", "samtools", "--channel", "https://prefix.dev/bioconda"]));
    }

    #[test]
    fn exposed_names_reads_trampoline_names() {
        // A conda package's executables need not match its name: `bubblewrap`
        // exposes `bwrap`.
        let json = r#"[{"name":"vim","exposed":[{"exposed_name":"vim","executable":"vim"}]},
                       {"name":"bubblewrap","exposed":[{"exposed_name":"bwrap","executable":"bwrap"}]}]"#;
        assert_eq!(exposed_names(json, "bubblewrap"), vec!["bwrap"]);
        assert_eq!(exposed_names(json, "vim"), vec!["vim"]);
        // Absent env / junk / missing `exposed` → empty, so callers fall back.
        assert!(exposed_names(json, "nope").is_empty());
        assert!(exposed_names("not json", "vim").is_empty());
        assert!(exposed_names(r#"[{"name":"x"}]"#, "x").is_empty());
    }

    #[test]
    fn install_tracks_renamed_trampoline() {
        // Regression: `pixi:bubblewrap` used to record `~/.pixi/bin/bubblewrap`,
        // which does not exist — pixi exposes `bwrap`. `remove` therefore
        // unlinked nothing and install-path checks saw a missing file.
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new()
            .with_present("pixi")
            .expect(
                "pixi global install bubblewrap",
                CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            )
            .expect(
                "pixi global list --json",
                CommandOutput {
                    status: 0,
                    stdout: r#"[{"name":"bubblewrap","exposed":[{"exposed_name":"bwrap","executable":"bwrap"}]}]"#
                        .into(),
                    stderr: String::new(),
                },
            );
        let t = ToolConfig::from_spec("pixi:bubblewrap");
        let out = install(&t, &runner).unwrap();
        assert_eq!(out.install_paths.len(), 1);
        assert!(out.install_paths[0].ends_with("bwrap"), "{:?}", out.install_paths);
    }

    #[test]
    fn install_tracks_all_exposed_with_primary_first() {
        // Multi-binary conda package: every trampoline is tracked so `remove`
        // is complete, and the package-named one leads so version backfill
        // (which probes install_paths[0]) can succeed.
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new()
            .with_present("pixi")
            .expect(
                "pixi global install vim",
                CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            )
            .expect(
                "pixi global list --json",
                CommandOutput {
                    status: 0,
                    stdout: r#"[{"name":"vim","exposed":[
                        {"exposed_name":"xxd","executable":"xxd"},
                        {"exposed_name":"ex","executable":"ex"},
                        {"exposed_name":"vim","executable":"vim"}]}]"#
                        .into(),
                    stderr: String::new(),
                },
            );
        let t = ToolConfig::from_spec("pixi:vim");
        let out = install(&t, &runner).unwrap();
        assert_eq!(out.install_paths.len(), 3);
        assert!(out.install_paths[0].ends_with("vim"), "{:?}", out.install_paths);
        assert!(out.install_paths[1].ends_with("ex"));
        assert!(out.install_paths[2].ends_with("xxd"));
    }

    #[test]
    fn install_falls_back_to_package_name_when_list_fails() {
        // `pixi global list` unavailable → still record a plausible path.
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new().with_present("pixi").expect(
            "pixi global install ripgrep",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        let t = ToolConfig::from_spec("pixi:ripgrep");
        let out = install(&t, &runner).unwrap();
        assert_eq!(out.install_paths.len(), 1);
        assert!(out.install_paths[0].ends_with("ripgrep"), "{:?}", out.install_paths);
    }

    #[test]
    fn pixi_bin_dir_from_honors_pixi_home() {
        // Pure — no process-env mutation, so it can't race other tests.
        assert_eq!(pixi_bin_dir_from(Some("/opt/pixi")), PathBuf::from("/opt/pixi/bin"));
        assert_eq!(pixi_bin_dir_from(Some("  ")), pixi_bin_dir_from(None));
        assert!(pixi_bin_dir_from(None).ends_with(".pixi/bin"));
    }
}
