//! Seam for external command execution so later source handlers (uv/fnm/cargo/go)
//! are unit-testable with a mock. M1's github source barely shells out, but the
//! abstraction is established now.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Result of running an external command.
///
/// `stdout`/`stderr` are captured for later source handlers (uv/fnm/cargo/go)
/// that parse command output; M1's github path does not read them yet.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub status: i32,
    #[allow(dead_code)]
    pub stdout: String,
    #[allow(dead_code)]
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Abstraction over running external programs.
pub trait CommandRunner {
    /// Run `program` with `args` and optional environment overrides, capturing output.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<CommandOutput>;

    /// Like [`run`](Self::run), but with the child's working directory set to
    /// `cwd`, stdin closed, and a hard `timeout` after which the child is killed
    /// and an error returned. Used for user-supplied lifecycle hooks, whose
    /// runtime ubix does not control.
    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<CommandOutput>;

    /// Run an interactive program, **inheriting** the terminal (stdin/stdout/stderr),
    /// and return its exit code. Required for editors and other TTY-driven programs —
    /// the capturing `run` (pipes stdio) would deadlock them (child gets no TTY on stdin).
    fn run_interactive(&self, program: &str, args: &[&str]) -> Result<i32>;

    /// Whether a program is discoverable on `PATH`.
    fn which(&self, program: &str) -> bool;
}

/// Real implementation backed by `std::process::Command`.
#[derive(Debug, Default, Clone)]
pub struct SystemRunner;

impl SystemRunner {
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<CommandOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let output = cmd
            .output()
            .with_context(|| format!("failed to spawn `{program}`"))?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<CommandOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{program}`"))?;
        // Drain both pipes on threads so a chatty child can't block on a full
        // pipe while we poll for exit.
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("waiting for `{program}`"))?
            {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("`{program}` timed out after {}s and was killed", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        Ok(CommandOutput {
            status: status.code().unwrap_or(-1),
            stdout: stdout.map(join_drain).unwrap_or_default(),
            stderr: stderr.map(join_drain).unwrap_or_default(),
        })
    }

    fn run_interactive(&self, program: &str, args: &[&str]) -> Result<i32> {
        // `status()` inherits the parent's stdio (unlike `output()`), so the
        // child editor gets the controlling terminal and does not deadlock.
        let status = Command::new(program)
            .args(args)
            .status()
            .with_context(|| format!("failed to spawn `{program}`"))?;
        Ok(status.code().unwrap_or(-1))
    }

    fn which(&self, program: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            #[cfg(unix)]
            {
                // A PATH hit only counts if it's a regular file with an exec bit
                // set — a non-executable file of the same name is not a program.
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&candidate)
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                candidate.is_file()
            }
        })
    }
}

/// Read a child pipe to completion on a helper thread (lossy UTF-8).
fn drain<R: std::io::Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

fn join_drain(handle: std::thread::JoinHandle<String>) -> String {
    handle.join().unwrap_or_default()
}

/// A single recorded invocation (program, args, env overrides, and the working
/// directory when the call went through `run_in`).
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct RecordedCall {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

/// Deterministic mock runner for unit tests. Later milestones use this to test
/// uv/fnm/cargo/go handlers without touching the system. It is part of the
/// established test seam and is currently exercised only from tests. It also
/// records every invocation (including env overrides) so tests can assert them.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct MockRunner {
    /// Map of `"program arg1 arg2"` → canned output.
    pub responses: HashMap<String, CommandOutput>,
    /// Programs considered present on PATH.
    pub present: Vec<String>,
    /// Recorded invocations, newest last. Shared (`Rc`) so a test can keep a
    /// handle after boxing the runner into an `App`.
    pub calls: std::rc::Rc<std::cell::RefCell<Vec<RecordedCall>>>,
}

#[allow(dead_code)]
impl MockRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned response for an exact `program + args` invocation.
    pub fn expect(mut self, key: &str, out: CommandOutput) -> Self {
        self.responses.insert(key.to_string(), out);
        self
    }

    pub fn with_present(mut self, program: &str) -> Self {
        self.present.push(program.to_string());
        self
    }

    /// The most recent recorded call, if any.
    pub fn last_call(&self) -> Option<RecordedCall> {
        self.calls.borrow().last().cloned()
    }

    /// A handle onto the call log that outlives moving the runner into a `Box`.
    pub fn calls_handle(&self) -> std::rc::Rc<std::cell::RefCell<Vec<RecordedCall>>> {
        self.calls.clone()
    }

    fn record_and_respond(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
        cwd: Option<PathBuf>,
    ) -> Result<CommandOutput> {
        self.calls.borrow_mut().push(RecordedCall {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            envs: envs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd,
        });
        let key = if args.is_empty() {
            program.to_string()
        } else {
            format!("{program} {}", args.join(" "))
        };
        match self.responses.get(&key) {
            Some(o) => Ok(o.clone()),
            None => bail!("MockRunner: no canned response for `{key}`"),
        }
    }
}

impl CommandRunner for MockRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<CommandOutput> {
        self.record_and_respond(program, args, envs, None)
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        envs: &[(&str, &str)],
        cwd: &Path,
        _timeout: Duration,
    ) -> Result<CommandOutput> {
        self.record_and_respond(program, args, envs, Some(cwd.to_path_buf()))
    }

    fn run_interactive(&self, program: &str, args: &[&str]) -> Result<i32> {
        // Record the invocation (like `run`) and report success; tests assert the call.
        self.calls.borrow_mut().push(RecordedCall {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            envs: Vec::new(),
            cwd: None,
        });
        Ok(0)
    }

    fn which(&self, program: &str) -> bool {
        self.present.iter().any(|p| p == program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_returns_canned() {
        let r = MockRunner::new()
            .expect(
                "uv --version",
                CommandOutput {
                    status: 0,
                    stdout: "uv 0.1.0".into(),
                    stderr: String::new(),
                },
            )
            .with_present("uv");
        let out = r.run("uv", &["--version"], &[]).unwrap();
        assert!(out.success());
        assert!(out.stdout.contains("uv"));
        assert!(r.which("uv"));
        assert!(!r.which("fnm"));
    }

    #[test]
    fn mock_errors_on_unknown() {
        let r = MockRunner::new();
        assert!(r.run("nope", &[], &[]).is_err());
    }

    #[test]
    fn mock_run_in_records_cwd() {
        let r = MockRunner::new().expect(
            "tool init",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        r.run_in("tool", &["init"], &[("PATH", "/b")], Path::new("/b"), Duration::from_secs(1))
            .unwrap();
        let call = r.last_call().unwrap();
        assert_eq!(call.cwd.as_deref(), Some(Path::new("/b")));
        assert_eq!(call.envs, vec![("PATH".to_string(), "/b".to_string())]);
    }

    #[cfg(unix)]
    #[test]
    fn system_run_in_sets_cwd_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = SystemRunner::new()
            .run_in("sh", &["-c", "pwd; echo err >&2; exit 3"], &[], dir.path(), Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.status, 3);
        assert_eq!(
            std::fs::canonicalize(out.stdout.trim()).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(out.stderr.trim(), "err");
    }

    #[cfg(unix)]
    #[test]
    fn system_run_in_kills_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let err = SystemRunner::new()
            .run_in("sh", &["-c", "exec sleep 5"], &[], dir.path(), Duration::from_millis(200))
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
