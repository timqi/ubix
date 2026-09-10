//! Per-tool lifecycle hooks: `post_install` (after a successful install or
//! upgrade) and `pre_remove` (before the binary is removed). Both are argv
//! arrays in `config.toml` — no shell, no expansion — run through the
//! [`CommandRunner`] seam with the tool's bin dir(s) prepended to `PATH` and
//! the install dir as working directory, so `argv[0]` can name the tool that
//! was just installed without knowing where it landed.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::runner::CommandRunner;

/// A hook is a user program ubix does not control; cap it rather than hang.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    PostInstall,
    PreRemove,
}

impl Hook {
    /// The `config.toml` key (also the label used in messages).
    pub fn key(self) -> &'static str {
        match self {
            Hook::PostInstall => "post_install",
            Hook::PreRemove => "pre_remove",
        }
    }
}

/// Config-time check: a hook must be a runnable argv (non-empty, with a
/// non-blank program).
pub fn validate(hook: Hook, argv: &[String]) -> Result<()> {
    match argv.first() {
        None => bail!("`{}` must be a non-empty argv array", hook.key()),
        Some(p) if p.trim().is_empty() => {
            bail!("`{}` argv[0] (the program) must not be blank", hook.key())
        }
        Some(_) => Ok(()),
    }
}

/// `post_install: rtk init -g …` — the human rendering used by `--dry-run`
/// and step lines.
pub fn describe(hook: Hook, argv: &[String]) -> String {
    format!("{}: {}", hook.key(), argv.join(" "))
}

/// Tracked binaries that are not on disk. Hooks only run against a tool that
/// is really there (callers decide whether "missing" is an error or a skip).
pub fn missing_binaries(install_paths: &[PathBuf]) -> Vec<&Path> {
    install_paths
        .iter()
        .filter(|p| !p.exists())
        .map(PathBuf::as_path)
        .collect()
}

/// `PATH` for a hook: `install_dir`, then any other directory holding one of
/// the tool's tracked binaries (pixi trampolines live outside `install_dir`),
/// then the inherited `existing` value.
pub fn hook_path(install_dir: &Path, install_paths: &[PathBuf], existing: Option<&OsStr>) -> OsString {
    let mut dirs: Vec<PathBuf> = vec![install_dir.to_path_buf()];
    for parent in install_paths.iter().filter_map(|p| p.parent()) {
        if !dirs.iter().any(|d| d == parent) {
            dirs.push(parent.to_path_buf());
        }
    }
    if let Some(existing) = existing {
        dirs.extend(std::env::split_paths(existing));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| install_dir.as_os_str().to_os_string())
}

/// Run `argv` for `hook`. The environment is inherited from ubix plus the
/// `PATH` from [`hook_path`]; the working directory is `install_dir`. A
/// non-zero exit is an error carrying the exit code and the trimmed stderr
/// (falling back to stdout, then a placeholder — never empty).
pub fn run(
    runner: &dyn CommandRunner,
    hook: Hook,
    argv: &[String],
    install_dir: &Path,
    install_paths: &[PathBuf],
) -> Result<()> {
    validate(hook, argv)?;
    let path = hook_path(install_dir, install_paths, std::env::var_os("PATH").as_deref());
    let path = path.to_string_lossy().into_owned();
    let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let rendered = argv.join(" ");
    let out = runner
        .run_in(&argv[0], &args, &[("PATH", &path)], install_dir, HOOK_TIMEOUT)
        .with_context(|| format!("{} hook `{rendered}`", hook.key()))?;
    if !out.success() {
        let stderr = out.stderr.trim();
        let stdout = out.stdout.trim();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "(no output)"
        };
        bail!("{} hook `{rendered}` exited {}: {detail}", hook.key(), out.status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, MockRunner};

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn out(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput { status, stdout: stdout.into(), stderr: stderr.into() }
    }

    #[test]
    fn validate_rejects_empty_and_blank_program() {
        assert!(validate(Hook::PostInstall, &[]).is_err());
        assert!(validate(Hook::PreRemove, &argv(&["  "])).is_err());
        assert!(validate(Hook::PostInstall, &argv(&["rtk"])).is_ok());
    }

    #[test]
    fn hook_path_prepends_install_dir_then_other_bin_dirs() {
        let p = hook_path(
            Path::new("/home/u/.local/bin"),
            &[PathBuf::from("/home/u/.local/bin/rtk"), PathBuf::from("/home/u/.pixi/bin/rg")],
            Some(OsStr::new("/usr/bin:/bin")),
        );
        assert_eq!(p, OsString::from("/home/u/.local/bin:/home/u/.pixi/bin:/usr/bin:/bin"));
        // No inherited PATH → just ours.
        let p = hook_path(Path::new("/b"), &[], None);
        assert_eq!(p, OsString::from("/b"));
    }

    #[test]
    fn run_spawns_argv_in_install_dir_with_path_prepended() {
        let runner = MockRunner::new().expect("rtk init -g --agent pi", out(0, "", ""));
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rtk");
        std::fs::write(&bin, b"x").unwrap();
        run(
            &runner,
            Hook::PostInstall,
            &argv(&["rtk", "init", "-g", "--agent", "pi"]),
            dir.path(),
            &[bin],
        )
        .unwrap();
        let call = runner.last_call().unwrap();
        assert_eq!(call.program, "rtk");
        assert_eq!(call.args, vec!["init", "-g", "--agent", "pi"]);
        assert_eq!(call.cwd.as_deref(), Some(dir.path()));
        let (k, v) = &call.envs[0];
        assert_eq!(k, "PATH");
        assert!(v.starts_with(&format!("{}{}", dir.path().display(), ':')) || v == &dir.path().display().to_string(), "{v}");
    }

    #[test]
    fn run_failure_carries_exit_code_and_stderr() {
        let runner = MockRunner::new().expect("rtk init", out(2, "", "  boom \n"));
        let err = run(&runner, Hook::PreRemove, &argv(&["rtk", "init"]), Path::new("/b"), &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("pre_remove hook `rtk init` exited 2: boom"), "{msg}");
    }

    #[test]
    fn run_failure_never_has_an_empty_detail() {
        let runner = MockRunner::new().expect("rtk init", out(1, "", ""));
        let err = run(&runner, Hook::PostInstall, &argv(&["rtk", "init"]), Path::new("/b"), &[]).unwrap_err();
        assert!(format!("{err:#}").ends_with("exited 1: (no output)"));
        // stdout is the fallback when stderr is silent.
        let runner = MockRunner::new().expect("rtk init", out(1, "only stdout", ""));
        let err = run(&runner, Hook::PostInstall, &argv(&["rtk", "init"]), Path::new("/b"), &[]).unwrap_err();
        assert!(format!("{err:#}").ends_with("exited 1: only stdout"));
    }

    #[test]
    fn run_spawn_error_names_the_hook() {
        // MockRunner errors on an unknown command, standing in for a spawn failure.
        let runner = MockRunner::new();
        let err = run(&runner, Hook::PostInstall, &argv(&["nope"]), Path::new("/b"), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("post_install hook `nope`"));
    }

    #[test]
    fn missing_binaries_lists_only_absent_paths() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("a");
        std::fs::write(&present, b"x").unwrap();
        let absent = dir.path().join("b");
        let paths = vec![present, absent.clone()];
        assert_eq!(missing_binaries(&paths), vec![absent.as_path()]);
        assert!(missing_binaries(&[]).is_empty());
    }

    #[test]
    fn describe_renders_key_and_argv() {
        assert_eq!(describe(Hook::PostInstall, &argv(&["rtk", "init", "-g"])), "post_install: rtk init -g");
    }
}
