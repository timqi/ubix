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

/// Install a pixi tool via `pixi global install`. The tracked install path is the
/// trampoline pixi drops into `$PIXI_HOME/bin` (named after the package).
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
        install_paths: vec![pixi_bin_dir().join(bare_name(&parsed.locator))],
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
        install_paths: vec![pixi_bin_dir().join(bare_name(&parsed.locator))],
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
        let runner = MockRunner::new().with_present("pixi").expect(
            "pixi global install samtools --channel https://prefix.dev/bioconda",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        let t = ToolConfig::from_spec("pixi:bioconda::samtools");
        let out = install(&t, &runner).unwrap();
        // Tracked binary is the bare name (channel stripped) in the pixi bin dir.
        assert_eq!(out.install_paths.len(), 1);
        assert!(out.install_paths[0].ends_with("samtools"), "{:?}", out.install_paths);
        let call = runner.last_call().unwrap();
        assert_eq!(
            call.args,
            vec!["global", "install", "samtools", "--channel", "https://prefix.dev/bioconda"]
        );
    }

    #[test]
    fn pixi_bin_dir_from_honors_pixi_home() {
        // Pure — no process-env mutation, so it can't race other tests.
        assert_eq!(pixi_bin_dir_from(Some("/opt/pixi")), PathBuf::from("/opt/pixi/bin"));
        assert_eq!(pixi_bin_dir_from(Some("  ")), pixi_bin_dir_from(None));
        assert!(pixi_bin_dir_from(None).ends_with(".pixi/bin"));
    }
}
