//! Platform detection: GOOS/GOARCH tokens and musl-libc detection.
//!
//! musl detection mirrors ubi's own logic (`ldd $(which ls)` → contains
//! "musl"), routed through the `CommandRunner` seam so it is unit-testable.

use crate::runner::CommandRunner;

/// Go-style OS token for the running platform (linux | darwin | windows).
pub fn goos() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// Go-style arch token for the running platform (amd64 | arm64).
pub fn goarch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

/// Whether the running Linux platform uses musl as its libc. Non-Linux → false.
/// Mirrors ubi: run `ldd` on `ls` and look for "musl" in the output. If the
/// probe cannot run (missing tools, error), default to false (glibc).
pub fn is_musl(runner: &dyn CommandRunner) -> bool {
    if goos() != "linux" {
        return false;
    }
    // `ldd /bin/ls` (or whatever `ls` resolves to) prints the loader; musl's
    // loader output contains "musl".
    let Ok(out) = runner.run("ldd", &["/bin/ls"], &[]) else {
        return false;
    };
    out.success() && (out.stdout.contains("musl") || out.stderr.contains("musl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, MockRunner};

    #[test]
    fn goos_goarch_are_known_tokens() {
        assert!(["linux", "darwin", "windows"].contains(&goos()));
        assert!(["amd64", "arm64"].contains(&goarch()));
    }

    #[test]
    fn is_musl_true_when_ldd_reports_musl() {
        let runner = MockRunner::new().expect(
            "ldd /bin/ls",
            CommandOutput {
                status: 0,
                stdout: "/lib/ld-musl-x86_64.so.1\n".into(),
                stderr: String::new(),
            },
        );
        // Only meaningful on Linux; on other targets is_musl short-circuits false.
        if goos() == "linux" {
            assert!(is_musl(&runner));
        }
    }

    #[test]
    fn is_musl_false_for_glibc() {
        let runner = MockRunner::new().expect(
            "ldd /bin/ls",
            CommandOutput {
                status: 0,
                stdout: "linux-vdso.so.1\nlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6\n".into(),
                stderr: String::new(),
            },
        );
        assert!(!is_musl(&runner));
    }

    #[test]
    fn is_musl_false_when_probe_fails() {
        // No canned response → runner.run errors → default false.
        let runner = MockRunner::new();
        assert!(!is_musl(&runner));
    }
}
