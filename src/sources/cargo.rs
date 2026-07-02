//! Cargo source (§5.5, M5): `cargo install --root <local> <crate>`; binaries
//! land in `<local>/bin` and cargo's own ledger (`<local>/.crates.toml`) manages
//! lifecycle. Uninstall = `cargo uninstall --root <local> <crate>`.

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// `--root` is the parent of `install_dir` (which is `<root>/bin`). PRD default
/// install_dir is `~/.local/bin` → root `~/.local`.
pub fn root_for(install_dir: &std::path::Path) -> std::path::PathBuf {
    install_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| install_dir.to_path_buf())
}

/// Build `cargo install --root <root> [--version V] [--features a,b] [--locked] <crate>`.
pub fn install_args(tool: &ToolConfig, crate_name: &str, root: &str) -> Vec<String> {
    let mut args = vec![
        "install".to_string(),
        "--root".to_string(),
        root.to_string(),
    ];
    if let Some(v) = &tool.version {
        args.push("--version".to_string());
        args.push(v.clone());
    }
    if let Some(features) = &tool.features {
        if !features.is_empty() {
            args.push("--features".to_string());
            args.push(features.join(","));
        }
    }
    if tool.locked.unwrap_or(false) {
        args.push("--locked".to_string());
    }
    args.push(crate_name.to_string());
    args
}

/// Build `cargo uninstall --root <root> <crate>`.
pub fn uninstall_args(crate_name: &str, root: &str) -> Vec<String> {
    vec![
        "uninstall".into(),
        "--root".into(),
        root.to_string(),
        crate_name.to_string(),
    ]
}

pub fn install(
    tool: &ToolConfig,
    runner: &dyn CommandRunner,
    install_dir: &std::path::Path,
) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Cargo)?;
    if parsed.source != SourceKind::Cargo {
        bail!("cargo source received non-cargo spec `{}`", tool.spec);
    }
    if !runner.which("cargo") {
        bail!("`cargo` is not installed; run `ubix bootstrap rust` first");
    }
    let root = root_for(install_dir);
    let root_s = root.to_string_lossy().into_owned();
    let args = install_args(tool, &parsed.locator, &root_s);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::step!("cargo install {} (compiling, may take a while)…", parsed.locator);
    let out = runner
        .run("cargo", &arg_refs, &[])
        .context("running cargo install")?;
    if !out.success() {
        bail!("cargo install failed: {}", out.stderr.trim());
    }
    // Capture the ACTUAL installed binary name(s): cargo prints them (e.g.
    // `Installing /home/u/.local/bin/rg`). The binary name often differs from
    // the crate name (ripgrep→rg, fd-find→fd). Fall back to the crate name if
    // nothing is parseable.
    let bin_dir = install_dir.to_path_buf();
    let mut install_paths: Vec<std::path::PathBuf> = parse_installed_binaries(&out.stdout, &out.stderr)
        .into_iter()
        .map(|name| bin_dir.join(name))
        .collect();
    if install_paths.is_empty() {
        install_paths.push(bin_dir.join(&parsed.locator));
    }
    Ok(InstallOutcome {
        installed_version: tool.version.clone().unwrap_or_else(|| "latest".into()),
        resolved_asset: None,
        install_paths,
        sha256: None,
    })
}

/// Parse the binary names cargo reports installing. Handles both `Installing`
/// and `Replacing` lines, on stdout or stderr, of the form
/// `<verb> /path/to/bin/<name>`. Returns the basenames, de-duplicated in order.
pub fn parse_installed_binaries(stdout: &str, stderr: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for stream in [stdout, stderr] {
        for line in stream.lines() {
            let t = line.trim();
            let rest = t
                .strip_prefix("Installing ")
                .or_else(|| t.strip_prefix("Replacing "));
            let Some(path) = rest else { continue };
            // Only take entries that look like an installed binary path (have a
            // `/bin/` segment) to avoid matching "Installing <crate> v1.2.3".
            if !path.contains("/bin/") {
                continue;
            }
            let name = path.trim().rsplit('/').next().unwrap_or("").to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn root_is_parent_of_bin() {
        assert_eq!(
            root_for(Path::new("/home/u/.local/bin")),
            Path::new("/home/u/.local")
        );
    }

    #[test]
    fn plain_install_args() {
        let t = ToolConfig::from_spec("cargo:ripgrep");
        assert_eq!(
            install_args(&t, "ripgrep", "/home/u/.local"),
            vec!["install", "--root", "/home/u/.local", "ripgrep"]
        );
    }

    #[test]
    fn install_args_with_all_options() {
        let mut t = ToolConfig::from_spec("cargo:somecli");
        t.version = Some("1.2.3".into());
        t.features = Some(vec!["x".into(), "y".into()]);
        t.locked = Some(true);
        assert_eq!(
            install_args(&t, "somecli", "/r"),
            vec![
                "install", "--root", "/r", "--version", "1.2.3", "--features", "x,y", "--locked",
                "somecli"
            ]
        );
    }

    #[test]
    fn uninstall_args_shape() {
        assert_eq!(
            uninstall_args("ripgrep", "/r"),
            vec!["uninstall", "--root", "/r", "ripgrep"]
        );
    }

    #[test]
    fn install_requires_cargo() {
        let runner = crate::runner::MockRunner::new();
        let t = ToolConfig::from_spec("cargo:ripgrep");
        let err = install(&t, &runner, Path::new("/home/u/.local/bin")).unwrap_err();
        assert!(err.to_string().contains("bootstrap rust"), "{err}");
    }

    #[test]
    fn parse_binaries_from_installing_lines() {
        // ripgrep installs a binary named `rg`, not `ripgrep`.
        let stderr = "  Compiling ripgrep v14.1.0\n   Installing /home/u/.local/bin/rg\n    Installed package `ripgrep v14.1.0` (executable `rg`)\n";
        assert_eq!(parse_installed_binaries("", stderr), vec!["rg"]);
    }

    #[test]
    fn parse_binaries_handles_replacing_and_multiple() {
        let stderr = "   Replacing /home/u/.local/bin/cargo-nextest\n   Installing /home/u/.local/bin/nextest-helper\n";
        assert_eq!(
            parse_installed_binaries("", stderr),
            vec!["cargo-nextest", "nextest-helper"]
        );
    }

    #[test]
    fn parse_binaries_ignores_crate_version_line() {
        // "Installing ripgrep v1.2.3" (no /bin/ path) must NOT be captured.
        assert!(parse_installed_binaries("Installing ripgrep v14.1.0\n", "").is_empty());
    }

    #[test]
    fn install_records_actual_binary_name() {
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new().with_present("cargo").expect(
            "cargo install --root /home/u/.local ripgrep",
            CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: "   Installing /home/u/.local/bin/rg\n".into(),
            },
        );
        let t = ToolConfig::from_spec("cargo:ripgrep");
        let out = install(&t, &runner, Path::new("/home/u/.local/bin")).unwrap();
        assert_eq!(
            out.install_paths,
            vec![std::path::PathBuf::from("/home/u/.local/bin/rg")]
        );
    }

    #[test]
    fn install_falls_back_to_crate_name() {
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new().with_present("cargo").expect(
            "cargo install --root /home/u/.local somecli",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        let t = ToolConfig::from_spec("cargo:somecli");
        let out = install(&t, &runner, Path::new("/home/u/.local/bin")).unwrap();
        assert_eq!(
            out.install_paths,
            vec![std::path::PathBuf::from("/home/u/.local/bin/somecli")]
        );
    }
}
