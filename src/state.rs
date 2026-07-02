//! `state.toml` model (§4.5), schema policy (§4.6), and exclusive flock (§8.6).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::paths;

/// Current state schema version (D13).
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Top-level `state.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// `[tools.<name>]` records plus the reserved `[tools._runtime]` section.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolRecord>,

    /// `[tools._runtime]` — runtime facts (e.g. fnm default node). Kept separate
    /// so it does not collide with real tool records.
    #[serde(rename = "_runtime", default, skip_serializing_if = "Runtime::is_empty")]
    pub runtime: Runtime,
}

fn default_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            tools: BTreeMap::new(),
            runtime: Runtime::default(),
        }
    }
}

/// `[tools._runtime]` (§4.5). All fields optional.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Runtime {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub node_default: Option<String>,
}

impl Runtime {
    pub fn is_empty(&self) -> bool {
        self.node_default.is_none()
    }
}

/// A per-tool state record (§4.5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolRecord {
    pub source: String,
    pub installed_version: String,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resolved_asset: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub module: Option<String>,

    pub install_paths: Vec<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sha256: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub installed_at: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub updated_at: Option<String>,
}

impl State {
    /// Deserialize state from a TOML string, enforcing schema policy.
    pub fn from_toml(text: &str) -> Result<Self> {
        let state: State = toml::from_str(text).context("parsing state.toml")?;
        state.check_schema()?;
        Ok(state)
    }

    /// Serialize to a pretty TOML string.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing state")
    }

    /// Enforce the schema-version migration policy (§4.6).
    fn check_schema(&self) -> Result<()> {
        match self.schema_version.cmp(&STATE_SCHEMA_VERSION) {
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Less => {
                // Older state: run migrations (none needed for v1) then continue.
                Ok(())
            }
            std::cmp::Ordering::Greater => bail!(
                "state.toml schema_version {} is newer than this ubix supports ({}); \
                 please upgrade ubix",
                self.schema_version,
                STATE_SCHEMA_VERSION
            ),
        }
    }

    /// Whether a tool name is a real record (excludes the reserved runtime key).
    pub fn tool(&self, name: &str) -> Option<&ToolRecord> {
        self.tools.get(name)
    }
}

/// A held exclusive advisory lock plus the state it guards. Dropping releases
/// the lock. All mutating operations must go through this handle (§8.6).
///
/// The advisory flock is held on a SEPARATE, stable lock file
/// (`state.toml.lock`) rather than on `state.toml` itself. `save()` atomically
/// renames a temp file over `state.toml`, which replaces its inode; if the lock
/// lived on `state.toml` it would be silently released after the first save
/// (the lock stays on the now-unlinked old inode). Keeping the lock on a file
/// that is never renamed or unlinked during the operation keeps it valid across
/// every save within a session.
#[derive(Debug)]
pub struct LockedState {
    /// The lock-file handle; the flock lives here and must be kept alive.
    lock_file: File,
    path: PathBuf,
    pub state: State,
}

impl LockedState {
    /// Acquire the exclusive lock guarding `state.toml` and read current state.
    ///
    /// If `wait` is false and the lock is held elsewhere, fail fast.
    pub fn acquire(state_path: &Path, wait: bool) -> Result<Self> {
        paths::ensure_parent_dir(state_path)?;

        // Lock on a stable sibling file so the lock survives atomic renames of
        // state.toml itself (see the struct doc).
        let lock_path = lock_path_for(state_path);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening lock file {}", lock_path.display()))?;

        if wait {
            lock_file
                .lock_exclusive()
                .with_context(|| format!("waiting for lock on {}", lock_path.display()))?;
        } else {
            match lock_file.try_lock_exclusive() {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    bail!(
                        "another ubix process is running (state.toml is locked); \
                         re-run with --wait to block"
                    );
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("locking {}", lock_path.display()));
                }
            }
        }

        let state = if state_path.exists() {
            let text = std::fs::read_to_string(state_path)
                .with_context(|| format!("reading {}", state_path.display()))?;
            if text.trim().is_empty() {
                State::default()
            } else {
                State::from_toml(&text)?
            }
        } else {
            State::default()
        };

        Ok(Self {
            lock_file,
            path: state_path.to_path_buf(),
            state,
        })
    }

    /// Persist the current in-memory state to disk (temp write + atomic rename).
    /// The advisory lock is held on `state.toml.lock`, which is untouched here,
    /// so it remains valid for subsequent saves within this session.
    pub fn save(&mut self) -> Result<()> {
        let text = self.state.to_toml()?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming into {}", self.path.display()))?;
        // Keep the lock handle alive; it guards the sibling lock file, not
        // state.toml, so the atomic rename above does not release it.
        let _ = &self.lock_file;
        Ok(())
    }
}

/// The sibling lock-file path for a given state file (`state.toml` → `state.toml.lock`).
fn lock_path_for(state_path: &Path) -> PathBuf {
    let mut s = state_path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> ToolRecord {
        ToolRecord {
            source: "github".into(),
            installed_version: "v0.18.21".into(),
            resolved_asset: Some("eza_x86_64-unknown-linux-musl.tar.gz".into()),
            module: None,
            install_paths: vec![PathBuf::from("/home/qiqi/.local/bin/eza")],
            sha256: Some("abc".into()),
            installed_at: Some("2026-07-02T08:45:00Z".into()),
            updated_at: Some("2026-07-02T08:45:00Z".into()),
        }
    }

    #[test]
    fn roundtrip_serde() {
        let mut s = State::default();
        s.tools.insert("eza".into(), sample_record());
        s.runtime.node_default = Some("v22.14.0".into());
        let text = s.to_toml().unwrap();
        let back = State::from_toml(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn roundtrip_without_runtime() {
        let mut s = State::default();
        s.tools.insert("eza".into(), sample_record());
        let text = s.to_toml().unwrap();
        // Runtime is empty, so it should not be serialized.
        assert!(!text.contains("_runtime"));
        let back = State::from_toml(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn refuse_higher_schema() {
        let text = "schema_version = 99\n";
        let err = State::from_toml(text).unwrap_err();
        assert!(err.to_string().contains("newer than this ubix"), "{err}");
    }

    #[test]
    fn accept_lower_schema() {
        let text = "schema_version = 0\n";
        assert!(State::from_toml(text).is_ok());
    }

    #[test]
    fn lock_acquire_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let mut locked = LockedState::acquire(&path, false).unwrap();
        locked
            .state
            .tools
            .insert("eza".into(), sample_record());
        locked.save().unwrap();
        drop(locked);

        // Reopen and verify persisted.
        let locked2 = LockedState::acquire(&path, false).unwrap();
        assert!(locked2.state.tool("eza").is_some());
    }

    #[test]
    fn second_lock_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let first = LockedState::acquire(&path, false).unwrap();
        // Second acquisition without wait must fail fast.
        let err = LockedState::acquire(&path, false).unwrap_err();
        assert!(err.to_string().contains("another ubix process"), "{err}");
        drop(first);
    }

    #[test]
    fn lock_survives_a_save() {
        // Regression: save() atomically renames state.toml, replacing its inode.
        // The lock must live on a stable sibling file so it is NOT released after
        // the first save. A second acquire after a save must still fail fast.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");

        let mut first = LockedState::acquire(&path, false).unwrap();
        first.state.tools.insert("eza".into(), sample_record());
        first.save().unwrap();
        // Save happened; another save for good measure (upgrade --all does N saves).
        first.state.runtime.node_default = Some("v22.14.0".into());
        first.save().unwrap();

        // The lock must STILL be held → a concurrent acquire fails fast.
        let err = LockedState::acquire(&path, false).unwrap_err();
        assert!(err.to_string().contains("another ubix process"), "{err}");

        // Once the first session drops, a fresh acquire succeeds.
        drop(first);
        let reopened = LockedState::acquire(&path, false).unwrap();
        assert!(reopened.state.tool("eza").is_some());
    }
}
