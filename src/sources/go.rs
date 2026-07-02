//! Go source (§5.6, M5): `GOBIN=<install_dir> go install <module>@<version>`.
//! Go has no ledger and no `go uninstall`; ubix records `install_paths` in
//! state and uninstall just deletes the tracked file.

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// Split a go locator `module@version` into (module, version). Defaults version
/// to `latest` when absent.
pub fn split_module(locator: &str) -> (String, String) {
    match locator.split_once('@') {
        Some((m, v)) if !v.is_empty() => (m.to_string(), v.to_string()),
        _ => (
            locator.trim_end_matches('@').to_string(),
            "latest".to_string(),
        ),
    }
}

/// The installed binary name is the last path segment of the module path.
pub fn binary_name(module: &str) -> String {
    module.rsplit('/').next().unwrap_or(module).to_string()
}

/// `go install <module>@<version>`.
pub fn install_args(module: &str, version: &str) -> Vec<String> {
    vec!["install".into(), format!("{module}@{version}")]
}

pub fn install(
    tool: &ToolConfig,
    runner: &dyn CommandRunner,
    install_dir: &std::path::Path,
) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Go)?;
    if parsed.source != SourceKind::Go {
        bail!("go source received non-go spec `{}`", tool.spec);
    }
    if !runner.which("go") {
        bail!("`go` is not installed; run `ubix bootstrap go` first");
    }
    let (module, version) = split_module(&parsed.locator);
    let args = install_args(&module, &version);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let gobin = install_dir.to_string_lossy().into_owned();
    let out = runner
        .run("go", &arg_refs, &[("GOBIN", &gobin)])
        .context("running go install")?;
    if !out.success() {
        bail!("go install failed: {}", out.stderr.trim());
    }
    let bin = binary_name(&module);
    Ok(InstallOutcome {
        installed_version: version,
        resolved_asset: None,
        install_paths: vec![install_dir.join(bin)],
        sha256: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_with_version() {
        assert_eq!(
            split_module("example.com/cmd/tool@v1.4.0"),
            ("example.com/cmd/tool".into(), "v1.4.0".into())
        );
    }

    #[test]
    fn split_defaults_latest() {
        assert_eq!(
            split_module("example.com/cmd/tool"),
            ("example.com/cmd/tool".into(), "latest".into())
        );
    }

    #[test]
    fn binary_is_last_segment() {
        assert_eq!(binary_name("example.com/cmd/gotool"), "gotool");
    }

    #[test]
    fn install_args_shape() {
        assert_eq!(
            install_args("example.com/cmd/tool", "latest"),
            vec!["install", "example.com/cmd/tool@latest"]
        );
    }

    #[test]
    fn install_requires_go() {
        let runner = crate::runner::MockRunner::new();
        let t = ToolConfig::from_spec("go:example.com/cmd/tool@latest");
        let err = install(&t, &runner, std::path::Path::new("/home/u/.local/bin")).unwrap_err();
        assert!(err.to_string().contains("bootstrap go"), "{err}");
    }

    #[test]
    fn install_runs_go_with_gobin() {
        use crate::runner::{CommandOutput, MockRunner};
        let runner = MockRunner::new().with_present("go").expect(
            "go install example.com/cmd/tool@latest",
            CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
        );
        let t = ToolConfig::from_spec("go:example.com/cmd/tool@latest");
        let out = install(&t, &runner, std::path::Path::new("/home/u/.local/bin")).unwrap();
        assert_eq!(out.installed_version, "latest");
        assert_eq!(
            out.install_paths,
            vec![std::path::PathBuf::from("/home/u/.local/bin/tool")]
        );
    }
}
