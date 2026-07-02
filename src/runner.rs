//! Seam for external command execution so later source handlers (uv/fnm/cargo/go)
//! are unit-testable with a mock. M1's github source barely shells out, but the
//! abstraction is established now.

use std::collections::HashMap;
use std::process::Command;

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

    fn which(&self, program: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
        })
    }
}

/// Deterministic mock runner for unit tests. Later milestones use this to test
/// uv/fnm/cargo/go handlers without touching the system. It is part of the
/// established test seam and is currently exercised only from tests.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct MockRunner {
    /// Map of `"program arg1 arg2"` → canned output.
    pub responses: HashMap<String, CommandOutput>,
    /// Programs considered present on PATH.
    pub present: Vec<String>,
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
}

impl CommandRunner for MockRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _envs: &[(&str, &str)],
    ) -> Result<CommandOutput> {
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
}
