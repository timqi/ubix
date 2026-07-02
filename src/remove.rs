//! `remove` safety logic (§8.5 / D14): only delete state-tracked files that
//! ubix installed; `--force` adopts an untracked file into state, then removes.
//!
//! Removal strategy by source (§8.7 matrix):
//! * github / gitlab / url / go → unlink the tracked `install_paths`.
//! * pypi(uv) → `uv tool uninstall` (never rm the symlink — that leaks the venv).
//! * cargo → `cargo uninstall --root <root>`.
//! * npm(fnm) → `fnm exec --using=default -- npm rm -g <pkg>`.

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::runner::CommandRunner;
use crate::sources::{cargo, npm, parse_spec, unlink_tracked, uv, SourceKind};
use crate::state::{State, ToolRecord};

/// Remove a tool from state (uninstalling / deleting its files) and from config.
pub fn remove_tool(
    cfg: &mut Config,
    state: &mut State,
    runner: &dyn CommandRunner,
    name: &str,
    force: bool,
) -> Result<()> {
    let in_config = cfg.tools.contains_key(name);
    let tracked = state.tool(name).cloned();

    let record = match tracked {
        Some(record) => record,
        None => {
            if !force {
                bail!(
                    "`{name}` is not tracked in state; refusing to delete files ubix did not install. \
                     Re-run with --force to adopt and remove."
                );
            }
            let Some(tool) = cfg.tools.get(name) else {
                bail!(
                    "`{name}` is neither tracked in state nor declared in config; nothing to adopt"
                );
            };
            let parsed = cfg.parsed_spec(tool)?;
            let install_dir = cfg.settings.install_dir_path();
            let final_name = tool
                .rename
                .clone()
                .or_else(|| tool.exe.clone())
                .unwrap_or_else(|| name.to_string());
            let path = install_dir.join(&final_name);
            ToolRecord {
                source: parsed.source.to_string(),
                installed_version: "adopted".to_string(),
                locator: Some(parsed.locator.clone()),
                resolved_asset: None,
                module: None,
                install_paths: vec![path],
                sha256: None,
                installed_at: Some(crate::now_iso8601()),
                updated_at: Some(crate::now_iso8601()),
            }
        }
    };

    uninstall_record(cfg, &record, runner, name)?;
    state.tools.remove(name);

    if in_config {
        cfg.tools.remove(name);
    }
    Ok(())
}

/// Perform the source-appropriate uninstall (does NOT touch state).
fn uninstall_record(
    cfg: &Config,
    record: &ToolRecord,
    runner: &dyn CommandRunner,
    name: &str,
) -> Result<()> {
    let kind = record.source.parse::<SourceKind>().ok();
    match kind {
        Some(SourceKind::Pypi) => {
            let locator = resolve_locator(record, cfg, name);
            let args = uv::uninstall_args(&locator);
            run_uninstall(runner, "uv", &args, "uv tool uninstall")?;
        }
        Some(SourceKind::Cargo) => {
            let locator = resolve_locator(record, cfg, name);
            let install_dir = cfg.settings.install_dir_path();
            let root = cargo::root_for(&install_dir);
            let args = cargo::uninstall_args(&locator, &root.to_string_lossy());
            run_uninstall(runner, "cargo", &args, "cargo uninstall")?;
        }
        Some(SourceKind::Npm) => {
            // Route through the fnm default node (never bare `npm`, §5.4).
            let locator = resolve_locator(record, cfg, name);
            let args = npm::global_remove_args(&locator);
            run_uninstall(runner, "fnm", &args, "npm rm -g via fnm default node")?;
        }
        // github / gitlab / url / go / unknown → unlink tracked files.
        _ => {
            unlink_tracked(record)?;
        }
    }
    Ok(())
}

fn run_uninstall(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[String],
    label: &str,
) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = runner
        .run(program, &refs, &[])
        .with_context(|| format!("running {label}"))?;
    if !out.success() {
        bail!("{label} failed: {}", out.stderr.trim());
    }
    Ok(())
}

/// Resolve the package/crate name to uninstall. Prefer the locator recorded in
/// state at install time (survives even when the config key differs from the
/// package and even after the config entry is gone, e.g. an orphan prune);
/// fall back to parsing the config spec, then to the tool key.
fn resolve_locator(record: &ToolRecord, cfg: &Config, name: &str) -> String {
    if let Some(loc) = &record.locator {
        if !loc.is_empty() {
            return loc.clone();
        }
    }
    locator_from_config(cfg, name).unwrap_or_else(|| name.to_string())
}

/// Resolve the source locator (package/crate/module name) for a config tool.
fn locator_from_config(cfg: &Config, name: &str) -> Option<String> {
    let tool = cfg.tools.get(name)?;
    let parsed = parse_spec(&tool.spec, cfg.settings.default_source_kind().ok()?).ok()?;
    Some(parsed.locator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolConfig;
    use crate::runner::{CommandOutput, MockRunner};

    fn cfg_with(name: &str, spec: &str) -> Config {
        let mut c = Config::default();
        c.tools.insert(name.into(), ToolConfig::from_spec(spec));
        c
    }

    fn record(source: &str, paths: Vec<std::path::PathBuf>) -> ToolRecord {
        ToolRecord {
            source: source.into(),
            installed_version: "v1".into(),
            locator: None,
            resolved_asset: None,
            module: None,
            install_paths: paths,
            sha256: None,
            installed_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn refuse_untracked_without_force() {
        let mut cfg = cfg_with("eza", "github:eza-community/eza");
        let mut state = State::default();
        let runner = MockRunner::new();
        let err = remove_tool(&mut cfg, &mut state, &runner, "eza", false).unwrap_err();
        assert!(err.to_string().contains("not tracked"), "{err}");
        assert!(cfg.tools.contains_key("eza"));
    }

    #[test]
    fn removes_tracked_github_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("eza");
        std::fs::write(&bin, b"binary").unwrap();
        let mut cfg = cfg_with("eza", "github:eza-community/eza");
        let mut state = State::default();
        state.tools.insert("eza".into(), record("github", vec![bin.clone()]));

        let runner = MockRunner::new();
        remove_tool(&mut cfg, &mut state, &runner, "eza", false).unwrap();
        assert!(!bin.exists());
        assert!(state.tool("eza").is_none());
        assert!(!cfg.tools.contains_key("eza"));
    }

    #[test]
    fn pypi_uses_uv_tool_uninstall() {
        let mut cfg = cfg_with("ruff", "pypi:ruff");
        let mut state = State::default();
        state.tools.insert("ruff".into(), record("pypi", vec![]));
        let runner = MockRunner::new().with_present("uv").expect(
            "uv tool uninstall ruff",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        remove_tool(&mut cfg, &mut state, &runner, "ruff", false).unwrap();
        assert!(state.tool("ruff").is_none());
    }

    #[test]
    fn cargo_uses_cargo_uninstall() {
        let mut cfg = cfg_with("ripgrep", "cargo:ripgrep");
        cfg.settings.install_dir = "/home/u/.local/bin".into();
        let mut state = State::default();
        state.tools.insert("ripgrep".into(), record("cargo", vec![]));
        let runner = MockRunner::new().expect(
            "cargo uninstall --root /home/u/.local ripgrep",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        remove_tool(&mut cfg, &mut state, &runner, "ripgrep", false).unwrap();
        assert!(state.tool("ripgrep").is_none());
    }

    #[test]
    fn orphan_prune_uses_recorded_locator_not_key() {
        // An orphan: no config entry, and the state KEY (`myrg`) differs from the
        // crate name. The recorded `locator` must drive the uninstall.
        let mut cfg = Config::default();
        cfg.settings.install_dir = "/home/u/.local/bin".into();
        let mut state = State::default();
        let mut rec = record("cargo", vec![]);
        rec.locator = Some("ripgrep".into());
        state.tools.insert("myrg".into(), rec);
        let runner = MockRunner::new().expect(
            "cargo uninstall --root /home/u/.local ripgrep",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        // force=false is fine because the tool IS tracked in state.
        remove_tool(&mut cfg, &mut state, &runner, "myrg", false).unwrap();
        assert!(state.tool("myrg").is_none());
    }

    #[test]
    fn npm_uses_fnm_exec_npm_rm() {
        let mut cfg = cfg_with("pnpm", "npm:pnpm");
        let mut state = State::default();
        state.tools.insert("pnpm".into(), record("npm", vec![]));
        let runner = MockRunner::new().expect(
            "fnm exec --using=default -- npm rm -g pnpm",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        remove_tool(&mut cfg, &mut state, &runner, "pnpm", false).unwrap();
        assert!(state.tool("pnpm").is_none());
    }

    #[test]
    fn force_adopts_then_removes() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("eza");
        std::fs::write(&bin, b"preexisting").unwrap();
        let mut cfg = cfg_with("eza", "github:eza-community/eza");
        cfg.settings.install_dir = dir.path().to_string_lossy().into_owned();
        let mut state = State::default();
        let runner = MockRunner::new();
        remove_tool(&mut cfg, &mut state, &runner, "eza", true).unwrap();
        assert!(!bin.exists());
        assert!(!cfg.tools.contains_key("eza"));
    }

    #[test]
    fn force_without_config_errors() {
        let mut cfg = Config::default();
        let mut state = State::default();
        let runner = MockRunner::new();
        let err = remove_tool(&mut cfg, &mut state, &runner, "ghost", true).unwrap_err();
        assert!(err.to_string().contains("nothing to adopt"), "{err}");
    }
}
