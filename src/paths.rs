//! XDG-aware path resolution and `~` / `$XDG_*` expansion.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Standard locations used by ubix.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Directory holding `config.toml`. `$UBIX_CONFIG_DIR` > `$XDG_CONFIG_HOME`
    /// `/ubix` > `~/.config/ubix`.
    pub config_dir: PathBuf,
    /// Directory holding `state.toml`. `$UBIX_DATA_DIR` > `$XDG_DATA_HOME`
    /// `/ubix` > `~/.local/share/ubix`.
    pub data_dir: PathBuf,
}

impl Paths {
    /// Resolve the effective config and data directories.
    ///
    /// Precedence (first non-empty wins):
    /// 1. `$UBIX_CONFIG_DIR` / `$UBIX_DATA_DIR` — the directory that DIRECTLY
    ///    holds `config.toml` / `state.toml` (no `/ubix` suffix appended), so a
    ///    caller can relocate ubix's own files without moving `$XDG_*` for every
    ///    child process ubix spawns (uv/fnm/cargo/go/pixi all read `$XDG_*`).
    /// 2. `$XDG_CONFIG_HOME/ubix` / `$XDG_DATA_HOME/ubix`.
    /// 3. `~/.config/ubix` / `~/.local/share/ubix`.
    ///
    /// The `UBIX_*` values go through [`expand`], so `~/…` works.
    pub fn resolve() -> Result<Self> {
        let config_dir = env_dir_expanded("UBIX_CONFIG_DIR").unwrap_or_else(|| {
            env_dir("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home_dir_join(".config"))
                .join("ubix")
        });
        let data_dir = env_dir_expanded("UBIX_DATA_DIR").unwrap_or_else(|| {
            env_dir("XDG_DATA_HOME")
                .unwrap_or_else(|| home_dir_join(".local").join("share"))
                .join("ubix")
        });
        Ok(Self {
            config_dir,
            data_dir,
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

/// Like [`env_dir`], but runs the value through [`expand`] so `~`/`$HOME` and
/// the `$XDG_*` tokens resolve. Empty value = unset.
fn env_dir_expanded(var: &str) -> Option<PathBuf> {
    let raw = std::env::var(var).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(expand(&raw))
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

    // Tests that mutate process env must not run concurrently. `cargo test`
    // runs tests in parallel by default, so we serialize via a mutex.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with `vars` applied to the process env (`None` = unset), then
    /// restore every touched variable. Serialized against every other env test.
    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<std::ffi::OsString>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let r = f();
        for (k, prev) in saved {
            match prev {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
        r
    }

    /// `with_env` for the common "only HOME matters" case; also clears the
    /// overrides so an ambient `$XDG_*`/`$UBIX_*` cannot skew the assertion.
    fn with_home<T>(home: &str, f: impl FnOnce() -> T) -> T {
        with_env(
            &[
                ("HOME", Some(home)),
                ("XDG_CONFIG_HOME", None),
                ("XDG_DATA_HOME", None),
                ("UBIX_CONFIG_DIR", None),
                ("UBIX_DATA_DIR", None),
            ],
            f,
        )
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
            let p = Paths::resolve().unwrap();
            assert_eq!(p.config_file(), PathBuf::from("/home/carol/.config/ubix/config.toml"));
            assert_eq!(
                p.state_file(),
                PathBuf::from("/home/carol/.local/share/ubix/state.toml")
            );
        });
    }

    #[test]
    fn xdg_env_appends_ubix_segment() {
        with_env(
            &[
                ("HOME", Some("/home/dave")),
                ("XDG_CONFIG_HOME", Some("/cfg")),
                ("XDG_DATA_HOME", Some("/data")),
                ("UBIX_CONFIG_DIR", None),
                ("UBIX_DATA_DIR", None),
            ],
            || {
                let p = Paths::resolve().unwrap();
                assert_eq!(p.config_file(), PathBuf::from("/cfg/ubix/config.toml"));
                assert_eq!(p.state_file(), PathBuf::from("/data/ubix/state.toml"));
            },
        );
    }

    #[test]
    fn ubix_dirs_hold_the_files_directly() {
        // No `/ubix` segment is appended, unlike the XDG vars.
        with_env(
            &[
                ("HOME", Some("/home/dave")),
                ("XDG_CONFIG_HOME", Some("/cfg")),
                ("XDG_DATA_HOME", Some("/data")),
                ("UBIX_CONFIG_DIR", Some("/srv/pier/conf")),
                ("UBIX_DATA_DIR", Some("/srv/pier/state")),
            ],
            || {
                let p = Paths::resolve().unwrap();
                assert_eq!(p.config_dir, PathBuf::from("/srv/pier/conf"));
                assert_eq!(p.config_file(), PathBuf::from("/srv/pier/conf/config.toml"));
                assert_eq!(p.data_dir, PathBuf::from("/srv/pier/state"));
                assert_eq!(p.state_file(), PathBuf::from("/srv/pier/state/state.toml"));
            },
        );
    }

    #[test]
    fn ubix_dirs_expand_tilde() {
        with_env(
            &[
                ("HOME", Some("/home/erin")),
                ("XDG_CONFIG_HOME", None),
                ("XDG_DATA_HOME", None),
                ("UBIX_CONFIG_DIR", Some("~/pier/conf")),
                ("UBIX_DATA_DIR", Some("$HOME/pier/state")),
            ],
            || {
                let p = Paths::resolve().unwrap();
                assert_eq!(p.config_file(), PathBuf::from("/home/erin/pier/conf/config.toml"));
                assert_eq!(p.state_file(), PathBuf::from("/home/erin/pier/state/state.toml"));
            },
        );
    }

    #[test]
    fn empty_ubix_dir_is_unset() {
        // An empty value must fall through to the XDG/default chain, not resolve
        // to the process CWD.
        with_env(
            &[
                ("HOME", Some("/home/frank")),
                ("XDG_CONFIG_HOME", None),
                ("XDG_DATA_HOME", None),
                ("UBIX_CONFIG_DIR", Some("")),
                ("UBIX_DATA_DIR", Some("")),
            ],
            || {
                let p = Paths::resolve().unwrap();
                assert_eq!(
                    p.config_file(),
                    PathBuf::from("/home/frank/.config/ubix/config.toml")
                );
                assert_eq!(
                    p.state_file(),
                    PathBuf::from("/home/frank/.local/share/ubix/state.toml")
                );
            },
        );
    }

    #[test]
    fn ubix_dir_overrides_xdg_independently() {
        // Only the config side is overridden; state still follows XDG_DATA_HOME.
        with_env(
            &[
                ("HOME", Some("/home/gina")),
                ("XDG_CONFIG_HOME", Some("/cfg")),
                ("XDG_DATA_HOME", Some("/data")),
                ("UBIX_CONFIG_DIR", Some("/srv/conf")),
                ("UBIX_DATA_DIR", None),
            ],
            || {
                let p = Paths::resolve().unwrap();
                assert_eq!(p.config_file(), PathBuf::from("/srv/conf/config.toml"));
                assert_eq!(p.state_file(), PathBuf::from("/data/ubix/state.toml"));
            },
        );
    }
}
