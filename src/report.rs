//! Machine-readable (`--json`) report models for `list` and `upgrade` (§7.2).
//!
//! With `--json`, stdout carries EXACTLY one of these documents and nothing
//! else — every human/progress line goes to stderr or is suppressed. Field
//! names reuse the `ToolRecord`/`ToolConfig` vocabulary (`source`, `locator`,
//! `installed_version`, `install_paths`, `installed_at`, `updated_at`, `tag`,
//! `version`) so a consumer that already reads `state.toml` sees the same keys.
//!
//! Every field is always serialized (absent values become `null` / `[]`) so the
//! document shape is stable; `schema_version` is bumped on any breaking change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ToolConfig;
use crate::state::ToolRecord;

/// Version of the `--json` document shape (independent of the config/state
/// schema version). Bump on any breaking change to the fields below.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Serialize `value` as the single JSON document on stdout.
pub fn emit<T: Serialize>(value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("serializing --json report")?;
    println!("{text}");
    Ok(())
}

// ---------------------------------------------------------------- list ----

/// `ubix list --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListReport {
    pub schema_version: u32,
    /// The effective `settings.install_dir`, expanded to an absolute path.
    pub install_dir: PathBuf,
    /// Declared tools, in config order.
    pub tools: Vec<ListEntry>,
}

impl ListReport {
    pub fn new(install_dir: PathBuf, tools: Vec<ListEntry>) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            install_dir,
            tools,
        }
    }
}

/// One declared tool: its config, its state record (if any), and whether the
/// tracked files are really on disk right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListEntry {
    pub name: String,
    pub spec: String,
    /// Source kind resolved from `spec` (falls back to the recorded source).
    pub source: String,
    pub locator: Option<String>,
    /// Whether a state record exists (declared-but-never-installed → false).
    pub installed: bool,
    pub installed_version: Option<String>,
    /// Absolute paths of the installed executable(s), as tracked in state.
    pub install_paths: Vec<PathBuf>,
    /// True only when every tracked path exists on disk right now; an
    /// uninstalled tool (no paths) is false.
    pub exists: bool,
    /// The subset of `install_paths` that is currently missing.
    pub missing_paths: Vec<PathBuf>,
    /// Pin: release tag (github/gitlab/url).
    pub tag: Option<String>,
    /// Pin: package version (pypi/npm/cargo/pixi).
    pub version: Option<String>,
    pub installed_at: Option<String>,
    pub updated_at: Option<String>,
}

impl ListEntry {
    /// Build an entry from config + state. `exists` is probed through the
    /// `path_exists` closure so tests need no filesystem.
    pub fn build(
        name: &str,
        tool: &ToolConfig,
        source: &str,
        record: Option<&ToolRecord>,
        path_exists: &dyn Fn(&Path) -> bool,
    ) -> Self {
        let install_paths: Vec<PathBuf> =
            record.map(|r| r.install_paths.clone()).unwrap_or_default();
        let missing_paths: Vec<PathBuf> = install_paths
            .iter()
            .filter(|p| !path_exists(p))
            .cloned()
            .collect();
        Self {
            name: name.to_string(),
            spec: tool.spec.clone(),
            source: source.to_string(),
            locator: record.and_then(|r| r.locator.clone()),
            installed: record.is_some(),
            installed_version: record.map(|r| r.installed_version.clone()),
            exists: !install_paths.is_empty() && missing_paths.is_empty(),
            install_paths,
            missing_paths,
            tag: tool.tag.clone(),
            version: tool.version.clone(),
            installed_at: record.and_then(|r| r.installed_at.clone()),
            updated_at: record.and_then(|r| r.updated_at.clone()),
        }
    }
}

// ------------------------------------------------------------- upgrade ----

/// What `upgrade` decided/did for one tool. Mirrors the `decide_action` state
/// machine, with `would-*` variants for `--dry-run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// Was missing, installed now.
    Installed,
    /// Was out of date, (re)installed to the target.
    Upgraded,
    /// Nothing to do (already current, or nothing to compare against).
    Skipped,
    /// Already at its `tag`/`version` pin — `--force` overrides.
    PinnedSkip,
    /// The install/upgrade/prune failed; `error` carries the text.
    Failed,
    /// Orphan (state has it, config does not) removed via `--prune`.
    Pruned,
    /// Orphan reported without `--prune` (nothing was changed).
    Orphan,
    /// `--dry-run` counterpart of `installed`.
    WouldInstall,
    /// `--dry-run` counterpart of `upgraded`.
    WouldUpgrade,
    /// `--dry-run` counterpart of `pruned`.
    WouldPrune,
}

impl Action {
    /// Every variant, so the summary can 0-fill each key (stable shape).
    pub const ALL: [Action; 10] = [
        Action::Installed,
        Action::Upgraded,
        Action::Skipped,
        Action::PinnedSkip,
        Action::Failed,
        Action::Pruned,
        Action::Orphan,
        Action::WouldInstall,
        Action::WouldUpgrade,
        Action::WouldPrune,
    ];

    /// The wire name (kebab-case), also used as the `by_action` key.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Installed => "installed",
            Action::Upgraded => "upgraded",
            Action::Skipped => "skipped",
            Action::PinnedSkip => "pinned-skip",
            Action::Failed => "failed",
            Action::Pruned => "pruned",
            Action::Orphan => "orphan",
            Action::WouldInstall => "would-install",
            Action::WouldUpgrade => "would-upgrade",
            Action::WouldPrune => "would-prune",
        }
    }

    /// Whether this action actually mutated the system (counted as `changed`).
    /// `would-*` never counts — a dry run changes nothing.
    pub fn is_change(self) -> bool {
        matches!(self, Action::Installed | Action::Upgraded | Action::Pruned)
    }
}

/// One tool's outcome in `upgrade --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpgradeEntry {
    pub name: String,
    pub action: Action,
    /// Recorded version before the run (`null` when not installed).
    pub from_version: Option<String>,
    /// Target version: the pin, or the queried latest (`null` when the source
    /// resolves it itself, e.g. a forced reinstall).
    pub to_version: Option<String>,
    /// Human reason for a skip (`null` otherwise).
    pub reason: Option<String>,
    /// Full error chain when `action` is `failed`. Also set on an `installed` /
    /// `upgraded` entry whose `post_install` hook failed: the tool landed and
    /// is recorded, but the run still counts it as failed. `null` otherwise.
    pub error: Option<String>,
}

impl UpgradeEntry {
    pub fn new(name: impl Into<String>, action: Action) -> Self {
        Self {
            name: name.into(),
            action,
            from_version: None,
            to_version: None,
            reason: None,
            error: None,
        }
    }

    /// Set both version fields (`from` = recorded, `to` = target).
    pub fn versions(mut self, from: Option<String>, to: Option<String>) -> Self {
        self.from_version = from;
        self.to_version = to;
        self
    }

    pub fn reason(mut self, r: impl Into<String>) -> Self {
        self.reason = Some(r.into());
        self
    }

    pub fn error(mut self, e: impl Into<String>) -> Self {
        self.error = Some(e.into());
        self
    }
}

/// Totals for an `upgrade --json` run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UpgradeSummary {
    /// Number of entries in `tools`.
    pub total: usize,
    /// Entries whose action mutated the system (never counts `would-*`).
    pub changed: usize,
    /// Entries with `action = "failed"` or a non-null `error` (a completed
    /// install whose `post_install` hook failed).
    pub failed: usize,
    /// Count per action; every action name is present (0 when unused).
    pub by_action: BTreeMap<String, usize>,
}

/// `ubix upgrade --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpgradeReport {
    pub schema_version: u32,
    pub dry_run: bool,
    /// Per-tool outcomes: orphans first (as `upgrade` emits them), then the
    /// declared tools in scope order.
    pub tools: Vec<UpgradeEntry>,
    pub summary: UpgradeSummary,
}

impl UpgradeReport {
    pub fn new(dry_run: bool) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            dry_run,
            tools: Vec::new(),
            summary: UpgradeSummary::default(),
        }
    }

    pub fn push(&mut self, entry: UpgradeEntry) {
        self.tools.push(entry);
    }

    /// Recompute `summary` from `tools`. Call once before serializing.
    pub fn finalize(&mut self) {
        let mut by_action: BTreeMap<String, usize> = Action::ALL
            .iter()
            .map(|a| (a.as_str().to_string(), 0))
            .collect();
        let (mut changed, mut failed) = (0usize, 0usize);
        for t in &self.tools {
            *by_action.entry(t.action.as_str().to_string()).or_default() += 1;
            if t.action.is_change() {
                changed += 1;
            }
            if t.action == Action::Failed || t.error.is_some() {
                failed += 1;
            }
        }
        self.summary = UpgradeSummary {
            total: self.tools.len(),
            changed,
            failed,
            by_action,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(version: &str, paths: &[&str]) -> ToolRecord {
        ToolRecord {
            source: "github".into(),
            installed_version: version.into(),
            locator: Some("eza-community/eza".into()),
            resolved_asset: None,
            module: None,
            install_paths: paths.iter().map(PathBuf::from).collect(),
            sha256: None,
            installed_at: Some("2026-07-02T08:45:00Z".into()),
            updated_at: Some("2026-07-03T08:45:00Z".into()),
            pre_remove: None,
        }
    }

    #[test]
    fn list_entry_reports_existing_paths() {
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        let rec = record("v0.23.4", &["/home/u/.local/bin/eza"]);
        let e = ListEntry::build("eza", &tool, "github", Some(&rec), &|_| true);
        assert!(e.installed && e.exists);
        assert!(e.missing_paths.is_empty());
        assert_eq!(e.install_paths, vec![PathBuf::from("/home/u/.local/bin/eza")]);
        assert_eq!(e.installed_version.as_deref(), Some("v0.23.4"));
        assert_eq!(e.updated_at.as_deref(), Some("2026-07-03T08:45:00Z"));
    }

    #[test]
    fn list_entry_flags_a_vanished_binary() {
        // Tracked in state, deleted on disk → installed but not exists.
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        let rec = record("v0.23.4", &["/home/u/.local/bin/eza", "/home/u/.local/bin/x"]);
        let e = ListEntry::build("eza", &tool, "github", Some(&rec), &|p| {
            p.ends_with("eza")
        });
        assert!(e.installed);
        assert!(!e.exists);
        assert_eq!(e.missing_paths, vec![PathBuf::from("/home/u/.local/bin/x")]);
    }

    #[test]
    fn list_entry_uninstalled_is_not_exists() {
        let mut tool = ToolConfig::from_spec("pypi:ruff");
        tool.version = Some("0.6.9".into());
        let e = ListEntry::build("ruff", &tool, "pypi", None, &|_| true);
        assert!(!e.installed && !e.exists);
        assert!(e.install_paths.is_empty() && e.missing_paths.is_empty());
        assert_eq!(e.installed_version, None);
        assert_eq!(e.version.as_deref(), Some("0.6.9"));
        assert_eq!(e.tag, None);
    }

    #[test]
    fn list_report_serializes_stable_keys() {
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        let e = ListEntry::build("eza", &tool, "github", None, &|_| false);
        let r = ListReport::new(PathBuf::from("/home/u/.local/bin"), vec![e]);
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["install_dir"], "/home/u/.local/bin");
        let t = &v["tools"][0];
        for key in [
            "name", "spec", "source", "locator", "installed", "installed_version",
            "install_paths", "exists", "missing_paths", "tag", "version",
            "installed_at", "updated_at",
        ] {
            assert!(t.get(key).is_some(), "missing key `{key}` in {t}");
        }
        // Absent values are null, not omitted.
        assert!(t["installed_version"].is_null());
        assert!(t["tag"].is_null());
    }

    #[test]
    fn action_wire_names_are_kebab_case() {
        for a in Action::ALL {
            let json = serde_json::to_string(&a).unwrap();
            assert_eq!(json, format!("\"{}\"", a.as_str()));
        }
    }

    #[test]
    fn only_real_mutations_count_as_change() {
        assert!(Action::Installed.is_change());
        assert!(Action::Upgraded.is_change());
        assert!(Action::Pruned.is_change());
        for a in [
            Action::Skipped,
            Action::PinnedSkip,
            Action::Failed,
            Action::Orphan,
            Action::WouldInstall,
            Action::WouldUpgrade,
            Action::WouldPrune,
        ] {
            assert!(!a.is_change(), "{a:?} must not count as changed");
        }
    }

    #[test]
    fn summary_counts_and_zero_fills_every_action() {
        let mut r = UpgradeReport::new(false);
        r.push(UpgradeEntry::new("a", Action::Upgraded).versions(Some("1".into()), None));
        r.push(UpgradeEntry::new("b", Action::Installed));
        r.push(UpgradeEntry::new("c", Action::PinnedSkip).reason("pinned"));
        r.push(UpgradeEntry::new("d", Action::Failed).error("boom"));
        r.finalize();
        assert_eq!(r.summary.total, 4);
        assert_eq!(r.summary.changed, 2);
        assert_eq!(r.summary.failed, 1);
        assert_eq!(r.summary.by_action["upgraded"], 1);
        assert_eq!(r.summary.by_action["pinned-skip"], 1);
        // Unused actions are present with 0 so consumers can index blindly.
        assert_eq!(r.summary.by_action.len(), Action::ALL.len());
        assert_eq!(r.summary.by_action["pruned"], 0);
    }

    #[test]
    fn failure_keeps_its_error_text() {
        let mut r = UpgradeReport::new(false);
        r.push(
            UpgradeEntry::new("eza", Action::Failed)
                .versions(Some("v0.1.0".into()), Some("v0.2.0".into()))
                .error("installing `eza`: ubi failed: no matching asset"),
        );
        r.finalize();
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["tools"][0]["action"], "failed");
        assert_eq!(v["tools"][0]["from_version"], "v0.1.0");
        assert_eq!(v["tools"][0]["to_version"], "v0.2.0");
        assert!(v["tools"][0]["error"]
            .as_str()
            .unwrap()
            .contains("no matching asset"));
        assert_eq!(v["summary"]["failed"], 1);
    }

    #[test]
    fn hook_error_on_installed_entry_counts_as_failed() {
        // The install happened (action stays truthful) but the run failed.
        let mut r = UpgradeReport::new(false);
        r.push(
            UpgradeEntry::new("rtk", Action::Installed)
                .versions(None, Some("v1".into()))
                .error("post_install hook `rtk init` exited 1: boom"),
        );
        r.push(UpgradeEntry::new("eza", Action::Upgraded));
        r.finalize();
        assert_eq!(r.summary.changed, 2);
        assert_eq!(r.summary.failed, 1);
        assert_eq!(r.summary.by_action["installed"], 1);
        assert_eq!(r.summary.by_action["failed"], 0);
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["tools"][0]["action"], "installed");
        assert!(v["tools"][0]["error"].as_str().unwrap().contains("post_install"));
        assert!(v["tools"][1]["error"].is_null());
    }

    #[test]
    fn dry_run_flag_is_carried() {
        let mut r = UpgradeReport::new(true);
        r.push(UpgradeEntry::new("eza", Action::WouldUpgrade).versions(None, Some("v2".into())));
        r.finalize();
        assert!(r.dry_run);
        assert_eq!(r.summary.changed, 0);
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["tools"][0]["action"], "would-upgrade");
    }
}
