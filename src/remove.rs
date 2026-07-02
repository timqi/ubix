//! `remove` safety logic (§8.5 / D14): only delete state-tracked files that
//! ubix installed; `--force` adopts an untracked file into state, then removes.

use anyhow::{bail, Result};

use crate::config::Config;
use crate::sources::{handler_for, SourceKind};
use crate::state::{State, ToolRecord};

/// Remove a tool from state (deleting its installed files) and from config.
///
/// * If the tool is tracked in state → delete its `install_paths`, drop record.
/// * If NOT tracked:
///   * without `force` → refuse (do not delete files ubix did not install).
///   * with `force` → adopt the config-derived install path into state, then
///     delete it ("adopt-then-remove", §8.5).
pub fn remove_tool(
    cfg: &mut Config,
    state: &mut State,
    name: &str,
    force: bool,
) -> Result<()> {
    let in_config = cfg.tools.contains_key(name);
    let tracked = state.tool(name).cloned();

    match tracked {
        Some(record) => {
            delete_record_files(&record, state, name)?;
        }
        None => {
            if !force {
                bail!(
                    "`{name}` is not tracked in state; refusing to delete files ubix did not install. \
                     Re-run with --force to adopt and remove."
                );
            }
            // Adopt: derive the would-be install path from config, register it,
            // then delete. Requires the tool to be declared in config so we can
            // compute the install path safely.
            let Some(tool) = cfg.tools.get(name) else {
                bail!(
                    "`{name}` is neither tracked in state nor declared in config; nothing to adopt"
                );
            };
            let parsed = cfg.parsed_spec(tool)?;
            let install_dir = cfg.settings.install_dir_path()?;
            let final_name = tool
                .rename
                .clone()
                .or_else(|| tool.exe.clone())
                .unwrap_or_else(|| name.to_string());
            let path = install_dir.join(&final_name);
            let adopted = ToolRecord {
                source: parsed.source.to_string(),
                installed_version: "adopted".to_string(),
                resolved_asset: None,
                module: None,
                install_paths: vec![path],
                sha256: None,
                installed_at: Some(crate::now_iso8601()),
                updated_at: Some(crate::now_iso8601()),
            };
            delete_record_files(&adopted, state, name)?;
        }
    }

    if in_config {
        cfg.tools.remove(name);
    }
    Ok(())
}

/// Delete a record's tracked files (via the source handler when possible) and
/// remove the record from state.
fn delete_record_files(record: &ToolRecord, state: &mut State, name: &str) -> Result<()> {
    // Prefer the source-specific remove (e.g. uv needs `uv tool uninstall`).
    // In M1 only github exists; its remove just unlinks tracked files.
    match record.source.parse::<SourceKind>() {
        Ok(kind) if kind.is_implemented() => {
            let handler = handler_for(kind)?;
            handler.remove(record)?;
        }
        _ => {
            // Unknown/unimplemented source recorded in state: fall back to
            // unlinking the tracked paths directly (safe: they are tracked).
            for p in &record.install_paths {
                if p.exists() {
                    std::fs::remove_file(p)
                        .map_err(|e| anyhow::anyhow!("removing {}: {e}", p.display()))?;
                }
            }
        }
    }
    state.tools.remove(name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolConfig;

    fn cfg_with(name: &str, spec: &str) -> Config {
        let mut c = Config::default();
        c.tools.insert(name.into(), ToolConfig::from_spec(spec));
        c
    }

    #[test]
    fn refuse_untracked_without_force() {
        let mut cfg = cfg_with("eza", "github:eza-community/eza");
        let mut state = State::default();
        let err = remove_tool(&mut cfg, &mut state, "eza", false).unwrap_err();
        assert!(err.to_string().contains("not tracked"), "{err}");
        // Config must be untouched on refusal.
        assert!(cfg.tools.contains_key("eza"));
    }

    #[test]
    fn removes_tracked_file() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("eza");
        std::fs::write(&bin, b"binary").unwrap();

        let mut cfg = cfg_with("eza", "github:eza-community/eza");
        let mut state = State::default();
        state.tools.insert(
            "eza".into(),
            ToolRecord {
                source: "github".into(),
                installed_version: "v1".into(),
                resolved_asset: None,
                module: None,
                install_paths: vec![bin.clone()],
                sha256: None,
                installed_at: None,
                updated_at: None,
            },
        );

        remove_tool(&mut cfg, &mut state, "eza", false).unwrap();
        assert!(!bin.exists(), "tracked file should be deleted");
        assert!(state.tool("eza").is_none());
        assert!(!cfg.tools.contains_key("eza"));
    }

    #[test]
    fn force_adopts_then_removes() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("eza");
        std::fs::write(&bin, b"preexisting").unwrap();

        // install_dir points at our tempdir so the adopted path matches.
        let mut cfg = cfg_with("eza", "github:eza-community/eza");
        cfg.settings.install_dir = dir.path().to_string_lossy().into_owned();
        let mut state = State::default();

        remove_tool(&mut cfg, &mut state, "eza", true).unwrap();
        assert!(!bin.exists(), "adopted file should be deleted");
        assert!(!cfg.tools.contains_key("eza"));
    }

    #[test]
    fn force_without_config_errors() {
        let mut cfg = Config::default();
        let mut state = State::default();
        let err = remove_tool(&mut cfg, &mut state, "ghost", true).unwrap_err();
        assert!(err.to_string().contains("nothing to adopt"), "{err}");
    }
}
