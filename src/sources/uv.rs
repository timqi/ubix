//! PyPI source via `uv tool` (§5.3, M3).
//!
//! uv installs tool entry points as SYMLINKS into `~/.local/bin`, with the real
//! venv under `~/.local/share/uv/tools/<name>`. Therefore removal MUST go
//! through `uv tool uninstall` — never `rm` the symlink (that leaks the venv).

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// Build the `uv tool install` argument vector for a package (pure; testable).
///
/// `uv tool install <pkg>[==version] [--with X]... [--reinstall]`. Extras are
/// expressed as `<pkg>[extra1,extra2]`.
pub fn install_args(tool: &ToolConfig, locator: &str, reinstall: bool) -> Vec<String> {
    let mut pkg = locator.to_string();
    if let Some(extras) = &tool.extras {
        if !extras.is_empty() {
            pkg = format!("{pkg}[{}]", extras.join(","));
        }
    }
    if let Some(v) = &tool.version {
        pkg = format!("{pkg}=={v}");
    }
    let mut args = vec!["tool".to_string(), "install".to_string(), pkg];
    if let Some(withs) = &tool.with {
        for w in withs {
            args.push("--with".to_string());
            args.push(w.clone());
        }
    }
    if reinstall {
        args.push("--reinstall".to_string());
    }
    args
}

/// `uv tool upgrade <pkg>`.
pub fn upgrade_args(locator: &str) -> Vec<String> {
    vec!["tool".into(), "upgrade".into(), pkg_base(locator)]
}

/// `uv tool uninstall <pkg>` — the ONLY safe removal path (§5.3).
pub fn uninstall_args(locator: &str) -> Vec<String> {
    vec!["tool".into(), "uninstall".into(), pkg_base(locator)]
}

fn pkg_base(locator: &str) -> String {
    locator.split(['=', '@']).next().unwrap_or(locator).to_string()
}

/// Install a pypi tool via uv. Returns the tracked outcome. The install path is
/// the symlink uv drops into `install_dir`.
pub fn install(
    tool: &ToolConfig,
    runner: &dyn CommandRunner,
    install_dir: &std::path::Path,
    reinstall: bool,
) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Pypi)?;
    if parsed.source != SourceKind::Pypi {
        bail!("uv source received non-pypi spec `{}`", tool.spec);
    }
    if !runner.which("uv") {
        bail!("`uv` is not installed; run `ubix bootstrap uv` first");
    }
    let args = install_args(tool, &parsed.locator, reinstall);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // Ensure uv drops entry-point symlinks into our install_dir.
    let bin = install_dir.to_string_lossy().into_owned();
    let out = runner
        .run("uv", &arg_refs, &[("UV_TOOL_BIN_DIR", &bin)])
        .context("running uv tool install")?;
    if !out.success() {
        bail!("uv tool install failed: {}", out.stderr.trim());
    }

    let pkg = pkg_base(&parsed.locator);
    // Entry points are symlinks named after the package's console scripts. We
    // record the primary one (pkg name); additional entries can be reconciled
    // later. install_paths points at the symlink in install_dir.
    let install_paths = vec![install_dir.join(&pkg)];
    Ok(InstallOutcome {
        installed_version: tool.version.clone().unwrap_or_else(|| "latest".into()),
        resolved_asset: None,
        install_paths,
        sha256: None,
    })
}

/// Upgrade a pypi tool via `uv tool upgrade <pkg>` (§5.3).
pub fn upgrade(
    tool: &ToolConfig,
    runner: &dyn CommandRunner,
    install_dir: &std::path::Path,
) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Pypi)?;
    if !runner.which("uv") {
        bail!("`uv` is not installed; run `ubix bootstrap uv` first");
    }
    let args = upgrade_args(&parsed.locator);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // Keep entry-point symlinks in our install_dir (matches install()), so a
    // custom install_dir is honored on upgrade too.
    let bin = install_dir.to_string_lossy().into_owned();
    let out = runner
        .run("uv", &refs, &[("UV_TOOL_BIN_DIR", &bin)])
        .context("running uv tool upgrade")?;
    if !out.success() {
        bail!("uv tool upgrade failed: {}", out.stderr.trim());
    }
    let pkg = pkg_base(&parsed.locator);
    Ok(InstallOutcome {
        installed_version: tool.version.clone().unwrap_or_else(|| "latest".into()),
        resolved_asset: None,
        install_paths: vec![install_dir.join(&pkg)],
        sha256: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_install_args() {
        let t = ToolConfig::from_spec("pypi:ruff");
        assert_eq!(install_args(&t, "ruff", false), vec!["tool", "install", "ruff"]);
    }

    #[test]
    fn install_args_with_version_extras_with() {
        let mut t = ToolConfig::from_spec("pypi:ruff");
        t.version = Some("0.6.9".into());
        t.extras = Some(vec!["all".into()]);
        t.with = Some(vec!["ruff-lsp".into()]);
        assert_eq!(
            install_args(&t, "ruff", false),
            vec!["tool", "install", "ruff[all]==0.6.9", "--with", "ruff-lsp"]
        );
    }

    #[test]
    fn reinstall_flag() {
        let t = ToolConfig::from_spec("pypi:black");
        let args = install_args(&t, "black", true);
        assert!(args.contains(&"--reinstall".to_string()));
    }

    #[test]
    fn upgrade_and_uninstall_args() {
        assert_eq!(upgrade_args("ruff"), vec!["tool", "upgrade", "ruff"]);
        assert_eq!(uninstall_args("ruff==0.6"), vec!["tool", "uninstall", "ruff"]);
    }

    #[test]
    fn install_requires_uv_present() {
        let runner = crate::runner::MockRunner::new(); // uv not present
        let t = ToolConfig::from_spec("pypi:ruff");
        let err = install(&t, &runner, std::path::Path::new("/tmp/bin"), false).unwrap_err();
        assert!(err.to_string().contains("bootstrap uv"), "{err}");
    }

    #[test]
    fn install_runs_uv_with_bin_dir() {
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new().with_present("uv").expect(
            "uv tool install ruff",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        let t = ToolConfig::from_spec("pypi:ruff");
        let out = install(&t, &runner, std::path::Path::new("/home/u/.local/bin"), false).unwrap();
        assert_eq!(out.install_paths, vec![std::path::PathBuf::from("/home/u/.local/bin/ruff")]);
        // The install must pass UV_TOOL_BIN_DIR pointing at install_dir.
        let call = runner.last_call().unwrap();
        assert!(call
            .envs
            .iter()
            .any(|(k, v)| k == "UV_TOOL_BIN_DIR" && v == "/home/u/.local/bin"));
    }

    #[test]
    fn upgrade_passes_bin_dir() {
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new().with_present("uv").expect(
            "uv tool upgrade ruff",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        let t = ToolConfig::from_spec("pypi:ruff");
        upgrade(&t, &runner, std::path::Path::new("/custom/bin")).unwrap();
        let call = runner.last_call().unwrap();
        assert_eq!(call.args, vec!["tool", "upgrade", "ruff"]);
        assert!(call
            .envs
            .iter()
            .any(|(k, v)| k == "UV_TOOL_BIN_DIR" && v == "/custom/bin"));
    }
}
