//! npm source via fnm's default LTS node (§5.4, D4, M4).
//!
//! fnm installs an LTS node and we mark it `default`; global npm packages install
//! onto that node. The alias bin dir (`<base>/aliases/default/bin`) is a symlink
//! that follows LTS jumps, so it is the stable PATH entry (§8.9). We detect the
//! fnm base at RUNTIME (never hardcode ~/.fnm) via `fnm env` / `$FNM_DIR`.
//!
//! CRITICAL: all npm operations run through `fnm exec --using=default -- npm …`,
//! never bare `npm`. A bare `npm` on PATH may be a version-manager shim (e.g.
//! mise) pointing at a *different* node, which would install the global package
//! into the wrong place while ubix records the fnm alias-bin path. Routing
//! through `fnm exec` guarantees the fnm default node's npm is used.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::paths;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// `fnm exec --using=default -- npm i -g <pkg>[@version]`.
pub fn global_install_args(pkg: &str, version: Option<&str>) -> Vec<String> {
    let spec = match version {
        Some(v) => format!("{pkg}@{v}"),
        None => pkg.to_string(),
    };
    fnm_exec_npm(&["i", "-g", &spec])
}

/// `fnm exec --using=default -- npm rm -g <pkg>`.
pub fn global_remove_args(pkg: &str) -> Vec<String> {
    fnm_exec_npm(&["rm", "-g", pkg])
}

/// Wrap an `npm` argument list so it runs on the fnm default node:
/// `fnm exec --using=default -- npm <args…>`.
fn fnm_exec_npm(npm_args: &[&str]) -> Vec<String> {
    let mut v = vec![
        "exec".to_string(),
        "--using=default".to_string(),
        "--".to_string(),
        "npm".to_string(),
    ];
    v.extend(npm_args.iter().map(|s| s.to_string()));
    v
}

/// Whether a fnm default node exists and works — pre-check before any npm op.
/// Runs `fnm exec --using=default -- node --version`; success means a default
/// node is set and usable.
pub fn has_default_node(runner: &dyn CommandRunner) -> bool {
    runner
        .run(
            "fnm",
            &["exec", "--using=default", "--", "node", "--version"],
            &[],
        )
        .map(|o| o.success())
        .unwrap_or(false)
}

/// Parse the fnm base directory from `fnm env` output. fnm prints lines like
/// `export FNM_DIR="/home/u/.local/share/fnm"` (or `set -x FNM_DIR ...` for
/// fish). We scan for the `FNM_DIR` assignment.
pub fn parse_fnm_dir_from_env(env_output: &str) -> Option<String> {
    for line in env_output.lines() {
        if let Some(val) = extract_env_value(line, "FNM_DIR") {
            return Some(val);
        }
    }
    None
}

/// Extract the value assigned to `key` from a single shell-export line, handling
/// bash (`export K="v"` / `export K=v`) and fish (`set -x K v` / `set -gx K v`).
fn extract_env_value(line: &str, key: &str) -> Option<String> {
    let line = line.trim();
    // fish: set -x FNM_DIR /path  OR  set -gx FNM_DIR "/path"
    if line.starts_with("set ") {
        let mut parts = line.split_whitespace();
        // set, flag(s)?, KEY, VALUE...
        let toks: Vec<&str> = parts.by_ref().collect();
        if let Some(idx) = toks.iter().position(|t| *t == key) {
            if let Some(v) = toks.get(idx + 1) {
                return Some(unquote(v));
            }
        }
        return None;
    }
    // bash/zsh: export FNM_DIR="/path"  OR  FNM_DIR=/path
    let stripped = line.strip_prefix("export ").unwrap_or(line);
    if let Some(rest) = stripped.strip_prefix(&format!("{key}=")) {
        return Some(unquote(rest.trim()));
    }
    None
}

fn unquote(s: &str) -> String {
    // Strip a trailing statement `;` (some `fnm env` fish lines end with one)
    // before removing surrounding quotes.
    let s = s.trim().trim_end_matches(';').trim();
    s.trim_matches('"').trim_matches('\'').to_string()
}

/// Compute the stable alias bin dir given the fnm base: `<base>/aliases/default/bin`.
pub fn alias_bin_dir(fnm_base: &str) -> PathBuf {
    PathBuf::from(fnm_base).join("aliases").join("default").join("bin")
}

/// Detect the fnm base at runtime: try `fnm env`, then `$FNM_DIR`, then the
/// legacy `~/.fnm` fallback.
pub fn detect_fnm_base(runner: &dyn CommandRunner) -> Option<String> {
    if runner.which("fnm") {
        if let Ok(out) = runner.run("fnm", &["env"], &[]) {
            if out.success() {
                if let Some(dir) = parse_fnm_dir_from_env(&out.stdout) {
                    return Some(dir);
                }
            }
        }
    }
    if let Ok(dir) = std::env::var("FNM_DIR") {
        if !dir.is_empty() {
            return Some(dir);
        }
    }
    // Legacy fallback.
    let legacy = paths::home_dir().join(".fnm");
    if legacy.exists() {
        return Some(legacy.to_string_lossy().into_owned());
    }
    None
}

/// Read the current fnm default node version (e.g. `v22.14.0`). Best-effort;
/// returns None if fnm is unavailable or the output is unexpected.
pub fn current_default_node(runner: &dyn CommandRunner) -> Option<String> {
    if !runner.which("fnm") {
        return None;
    }
    // Query the default node's version via `fnm exec` — reliable from a plain
    // subprocess (same mechanism as `has_default_node`), unlike `fnm current`,
    // which needs a fnm-activated shell (FNM_MULTISHELL_PATH) and otherwise
    // reports `none`/`system`, hiding real LTS jumps.
    let out = runner
        .run(
            "fnm",
            &["exec", "--using=default", "--", "node", "--version"],
            &[],
        )
        .ok()?;
    if !out.success() {
        return None;
    }
    let v = out.stdout.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Whether the live default node differs from what state recorded — i.e. an LTS
/// jump occurred and npm tools must be reinstalled on the new default (§5.4).
pub fn lts_jump(recorded: Option<&str>, live: Option<&str>) -> bool {
    match (recorded, live) {
        (Some(r), Some(l)) => r != l,
        // No recorded version yet but a live one exists → treat as (first) jump.
        (None, Some(_)) => true,
        _ => false,
    }
}

pub fn install(tool: &ToolConfig, runner: &dyn CommandRunner) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Npm)?;
    if parsed.source != SourceKind::Npm {
        bail!("npm source received non-npm spec `{}`", tool.spec);
    }
    if !runner.which("fnm") {
        bail!(
            "npm tools need a Node runtime (fnm + a default node); run:\n    \
             ubix bootstrap nodejs"
        );
    }
    // Require a fnm DEFAULT node (not just fnm) so `npm` runs on the right node.
    if !has_default_node(runner) {
        bail!(
            "no fnm default node set; run `ubix bootstrap nodejs` \
             (installs fnm's default LTS) first"
        );
    }
    // Run via `fnm exec --using=default -- npm …` (never bare `npm`, which may be
    // a mise/asdf shim on a different node).
    let args = global_install_args(&parsed.locator, tool.version.as_deref());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::step!("fnm exec --using=default -- npm i -g {}…", parsed.locator);
    let out = runner
        .run("fnm", &arg_refs, &[])
        .context("running npm i -g via fnm default node")?;
    if !out.success() {
        bail!("npm install failed: {}", out.stderr.trim());
    }
    // Global npm entry points live on the default node bin; the stable PATH entry
    // is the alias bin dir. We track the alias-bin path for the package binary.
    let install_paths = match detect_fnm_base(runner) {
        Some(base) => vec![alias_bin_dir(&base).join(&parsed.locator)],
        None => Vec::new(),
    };
    Ok(InstallOutcome {
        installed_version: tool.version.clone().unwrap_or_else(|| "latest".into()),
        resolved_asset: None,
        install_paths,
        sha256: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, MockRunner};

    #[test]
    fn install_args_route_through_fnm_exec() {
        assert_eq!(
            global_install_args("pnpm", None),
            vec!["exec", "--using=default", "--", "npm", "i", "-g", "pnpm"]
        );
        assert_eq!(
            global_install_args("pnpm", Some("9.1.0")),
            vec!["exec", "--using=default", "--", "npm", "i", "-g", "pnpm@9.1.0"]
        );
    }

    #[test]
    fn remove_args_route_through_fnm_exec() {
        assert_eq!(
            global_remove_args("pnpm"),
            vec!["exec", "--using=default", "--", "npm", "rm", "-g", "pnpm"]
        );
    }

    #[test]
    fn parse_fnm_dir_bash() {
        let out = "export FNM_MULTISHELL_PATH=\"/x\"\nexport FNM_DIR=\"/home/u/.local/share/fnm\"\nexport PATH=\"/x:$PATH\"\n";
        assert_eq!(
            parse_fnm_dir_from_env(out).as_deref(),
            Some("/home/u/.local/share/fnm")
        );
    }

    #[test]
    fn parse_fnm_dir_fish() {
        let out = "set -gx FNM_DIR /home/u/.fnm\nset -gx PATH /x $PATH\n";
        assert_eq!(parse_fnm_dir_from_env(out).as_deref(), Some("/home/u/.fnm"));
    }

    #[test]
    fn parse_fnm_dir_bare_assignment() {
        assert_eq!(
            parse_fnm_dir_from_env("FNM_DIR=/opt/fnm").as_deref(),
            Some("/opt/fnm")
        );
    }

    #[test]
    fn alias_bin_dir_shape() {
        assert_eq!(
            alias_bin_dir("/home/u/.local/share/fnm"),
            PathBuf::from("/home/u/.local/share/fnm/aliases/default/bin")
        );
    }

    #[test]
    fn detect_base_via_fnm_env() {
        let runner = MockRunner::new().with_present("fnm").expect(
            "fnm env",
            CommandOutput {
                status: 0,
                stdout: "export FNM_DIR=\"/detected/fnm\"\n".into(),
                stderr: String::new(),
            },
        );
        assert_eq!(detect_fnm_base(&runner).as_deref(), Some("/detected/fnm"));
    }

    #[test]
    fn current_default_node_parsed() {
        let runner = MockRunner::new().with_present("fnm").expect(
            "fnm exec --using=default -- node --version",
            CommandOutput {
                status: 0,
                stdout: "v22.14.0\n".into(),
                stderr: String::new(),
            },
        );
        assert_eq!(current_default_node(&runner).as_deref(), Some("v22.14.0"));
    }

    #[test]
    fn current_default_none_when_no_default() {
        // No default node → `fnm exec --using=default` fails → None.
        let runner = MockRunner::new().with_present("fnm").expect(
            "fnm exec --using=default -- node --version",
            CommandOutput { status: 1, stdout: String::new(), stderr: "no default".into() },
        );
        assert_eq!(current_default_node(&runner), None);
    }

    #[test]
    fn lts_jump_detection() {
        assert!(lts_jump(Some("v20.0.0"), Some("v22.0.0")));
        assert!(!lts_jump(Some("v22.0.0"), Some("v22.0.0")));
        assert!(lts_jump(None, Some("v22.0.0")));
        assert!(!lts_jump(Some("v22.0.0"), None));
        assert!(!lts_jump(None, None));
    }

    #[test]
    fn install_requires_fnm() {
        // fnm itself missing → point at the nodejs bootstrap.
        let runner = MockRunner::new();
        let t = ToolConfig::from_spec("npm:pnpm");
        let err = install(&t, &runner).unwrap_err();
        assert!(err.to_string().contains("ubix bootstrap nodejs"), "{err}");
    }

    #[test]
    fn install_requires_default_node() {
        // fnm present but the default-node pre-check fails → bootstrap nodejs.
        let runner = MockRunner::new().with_present("fnm").expect(
            "fnm exec --using=default -- node --version",
            CommandOutput { status: 1, stdout: String::new(), stderr: "no default".into() },
        );
        let t = ToolConfig::from_spec("npm:pnpm");
        let err = install(&t, &runner).unwrap_err();
        assert!(err.to_string().contains("ubix bootstrap nodejs"), "{err}");
    }

    #[test]
    fn install_runs_npm_through_fnm_exec() {
        // fnm present, default node OK → install runs `fnm exec … npm i -g`.
        let runner = MockRunner::new()
            .with_present("fnm")
            .expect(
                "fnm exec --using=default -- node --version",
                CommandOutput { status: 0, stdout: "v22.14.0\n".into(), stderr: String::new() },
            )
            .expect(
                "fnm exec --using=default -- npm i -g pnpm",
                CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            )
            .expect(
                "fnm env",
                CommandOutput {
                    status: 0,
                    stdout: "export FNM_DIR=\"/home/u/.local/share/fnm\"\n".into(),
                    stderr: String::new(),
                },
            );
        let t = ToolConfig::from_spec("npm:pnpm");
        let out = install(&t, &runner).unwrap();
        // Recorded install path is the fnm alias-bin dir + package name (unchanged).
        assert_eq!(
            out.install_paths,
            vec![PathBuf::from("/home/u/.local/share/fnm/aliases/default/bin/pnpm")]
        );
        // The npm install was invoked via `fnm exec`, not bare `npm`.
        let calls = runner.calls.borrow();
        assert!(calls.iter().any(|c| c.program == "fnm"
            && c.args == ["exec", "--using=default", "--", "npm", "i", "-g", "pnpm"]));
        assert!(!calls.iter().any(|c| c.program == "npm"), "must not invoke bare npm");
    }

    #[test]
    fn has_default_node_true_false() {
        let ok = MockRunner::new().expect(
            "fnm exec --using=default -- node --version",
            CommandOutput { status: 0, stdout: "v22.0.0\n".into(), stderr: String::new() },
        );
        assert!(has_default_node(&ok));
        // Missing canned response → run errors → false.
        assert!(!has_default_node(&MockRunner::new()));
    }
}
