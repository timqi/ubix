//! XDG-aware path resolution and `~` / `$XDG_*` expansion.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Standard locations used by ubix.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `~/.config/ubix` (honors `$XDG_CONFIG_HOME`).
    pub config_dir: PathBuf,
    /// `~/.local/share/ubix` (honors `$XDG_DATA_HOME`).
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolve the standard config and data directories.
    pub fn resolve() -> Result<Self> {
        let config_home = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home_dir_join(".config"));
        let data_home =
            env_dir("XDG_DATA_HOME").unwrap_or_else(|| home_dir_join(".local").join("share"));
        Ok(Self {
            config_dir: config_home.join("ubix"),
            data_dir: data_home.join("ubix"),
        })
    }

    /// Path to `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// Path to `state.toml`.
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.toml")
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// Best-effort home directory. Falls back to `.` if unknown so callers never panic.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn home_dir_join(seg: &str) -> PathBuf {
    home_dir().join(seg)
}

/// Expand a leading `~` / `~/` and `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` /
/// `$HOME` tokens in a path string. Only the documented tokens are expanded so
/// that arbitrary environment injection is not possible.
pub fn expand(input: &str) -> PathBuf {
    PathBuf::from(expand_tilde(&expand_tokens(input)))
}

fn expand_tokens(input: &str) -> String {
    let mut out = input.to_string();
    for (token, value) in [
        (
            "$XDG_CONFIG_HOME",
            env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home_dir_join(".config")),
        ),
        (
            "$XDG_DATA_HOME",
            env_dir("XDG_DATA_HOME").unwrap_or_else(|| home_dir_join(".local").join("share")),
        ),
        ("$HOME", home_dir()),
    ] {
        if out.contains(token) {
            out = out.replace(token, &value.to_string_lossy());
        }
    }
    out
}

fn expand_tilde(input: &str) -> String {
    if input == "~" {
        return home_dir().to_string_lossy().into_owned();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home_dir().join(rest).to_string_lossy().into_owned();
    }
    input.to_string()
}

/// Ensure the parent directory of `path` exists.
pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(home: &str, f: impl FnOnce() -> T) -> T {
        // Tests that mutate process env must not run concurrently. `cargo test`
        // runs tests in parallel by default, so we serialize via a mutex.
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        let r = f();
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        r
    }

    #[test]
    fn expand_bare_tilde() {
        with_home("/home/alice", || {
            assert_eq!(expand("~"), PathBuf::from("/home/alice"));
        });
    }

    #[test]
    fn expand_tilde_slash() {
        with_home("/home/alice", || {
            assert_eq!(
                expand("~/.local/bin"),
                PathBuf::from("/home/alice/.local/bin")
            );
        });
    }

    #[test]
    fn expand_home_token() {
        with_home("/home/bob", || {
            assert_eq!(
                expand("$HOME/x"),
                PathBuf::from("/home/bob/x")
            );
        });
    }

    #[test]
    fn no_expansion_for_absolute() {
        assert_eq!(expand("/opt/bin"), PathBuf::from("/opt/bin"));
    }

    #[test]
    fn tilde_only_at_start() {
        // A tilde not at the start is left untouched.
        assert_eq!(expand("/x/~y"), PathBuf::from("/x/~y"));
    }

    #[test]
    fn xdg_config_default() {
        with_home("/home/carol", || {
            std::env::remove_var("XDG_CONFIG_HOME");
            let p = Paths::resolve().unwrap();
            assert_eq!(p.config_file(), PathBuf::from("/home/carol/.config/ubix/config.toml"));
            assert_eq!(
                p.state_file(),
                PathBuf::from("/home/carol/.local/share/ubix/state.toml")
            );
        });
    }
}
