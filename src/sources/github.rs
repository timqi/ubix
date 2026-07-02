//! GitHub release source (§5.1). Delegates asset/platform heuristics to ubi.

use anyhow::{bail, Result};
use ubi::ForgeType;

use crate::config::ToolConfig;
use crate::engine::{ReleaseEngine, ReleaseRequest, UbiEngine};
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, InstallOutcome, Source, SourceKind};

/// GitHub release installer. Holds a boxed [`ReleaseEngine`] so tests can inject
/// a fake that never touches the network.
pub struct GithubSource {
    engine: Box<dyn ReleaseEngine>,
    /// The tool key (used as default exe/final name). Set via [`Self::for_tool`].
    tool_name: String,
    /// install_dir resolved by the caller.
    install_dir: std::path::PathBuf,
}

impl GithubSource {
    /// Placeholder constructor used by `handler_for`; real installs go through
    /// [`Self::for_tool`]. Kept so the `Source` object is constructible.
    pub fn new() -> Self {
        Self {
            engine: Box::new(UbiEngine::new()),
            tool_name: String::new(),
            install_dir: std::path::PathBuf::new(),
        }
    }

    /// Construct a handler bound to a specific tool name + install_dir.
    pub fn for_tool(
        tool_name: impl Into<String>,
        install_dir: std::path::PathBuf,
        engine: Box<dyn ReleaseEngine>,
    ) -> Self {
        Self {
            engine,
            tool_name: tool_name.into(),
            install_dir,
        }
    }

    fn build_request(&self, tool: &ToolConfig) -> Result<ReleaseRequest> {
        let parsed = parse_spec(&tool.spec, SourceKind::Github)?;
        if parsed.source != SourceKind::Github {
            bail!(
                "GithubSource received a non-github spec `{}`",
                tool.spec
            );
        }

        // M1: single-exe only. `exes` (multi-entry) is wired in config but not
        // handled here (§12).
        if let Some(exes) = &tool.exes {
            if !exes.is_empty() {
                bail!(
                    "multi-entry `exes` ({exes:?}) is not supported in M1 \
                     (single-exe only; see PRD §5.1/§12). Use `exe` for a single binary."
                );
            }
        }

        let final_name = tool
            .rename
            .clone()
            .or_else(|| tool.exe.clone())
            .unwrap_or_else(|| self.tool_name.clone());

        Ok(ReleaseRequest {
            project: parsed.locator,
            forge: ForgeType::GitHub,
            tag: tool.tag.clone(),
            matching: tool.matching.clone(),
            exe: tool.exe.clone(),
            rename: tool.rename.clone(),
            install_dir: self.install_dir.clone(),
            final_name,
            github_token: github_token_from_env(),
            gitlab_token: None,
            api_base_url: None,
        })
    }
}

impl Default for GithubSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for GithubSource {
    fn install(&self, tool: &ToolConfig, _runner: &dyn CommandRunner) -> Result<InstallOutcome> {
        if self.tool_name.is_empty() {
            bail!("GithubSource must be constructed with `for_tool` before install");
        }
        let req = self.build_request(tool)?;
        let result = self.engine.install(&req)?;
        Ok(InstallOutcome {
            installed_version: result
                .version
                .unwrap_or_else(|| "latest".to_string()),
            resolved_asset: None,
            install_paths: vec![result.install_path],
            sha256: Some(result.sha256),
        })
    }
}

/// Read the GitHub token from `UBIX_GITHUB_TOKEN` (D6).
pub fn github_token_from_env() -> Option<String> {
    std::env::var("UBIX_GITHUB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineResult;
    use crate::runner::MockRunner;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FakeEngine {
        last: Mutex<Option<ReleaseRequest>>,
    }
    impl ReleaseEngine for FakeEngine {
        fn install(&self, req: &ReleaseRequest) -> Result<EngineResult> {
            *self.last.lock().unwrap() = Some(req.clone());
            Ok(EngineResult {
                install_path: req.install_dir.join(&req.final_name),
                sha256: "deadbeef".into(),
                version: req.tag.clone().or_else(|| Some("v1.0.0".into())),
            })
        }
    }

    #[test]
    fn install_builds_expected_request() {
        let fake = Box::new(FakeEngine {
            last: Mutex::new(None),
        });
        let src = GithubSource::for_tool("codex", PathBuf::from("/tmp/bin"), fake);
        let mut tool = ToolConfig::from_spec("github:openai/codex");
        tool.matching = Some("linux".into());
        tool.exe = Some("codex".into());
        let runner = MockRunner::new();
        let out = src.install(&tool, &runner).unwrap();
        assert_eq!(out.install_paths, vec![PathBuf::from("/tmp/bin/codex")]);
        assert_eq!(out.sha256.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn tag_pin_flows_to_version() {
        let fake = Box::new(FakeEngine {
            last: Mutex::new(None),
        });
        let src = GithubSource::for_tool("eza", PathBuf::from("/tmp/bin"), fake);
        let mut tool = ToolConfig::from_spec("github:eza-community/eza");
        tool.tag = Some("v0.18.21".into());
        let out = src.install(&tool, &MockRunner::new()).unwrap();
        assert_eq!(out.installed_version, "v0.18.21");
    }

    #[test]
    fn exes_rejected_in_m1() {
        let fake = Box::new(FakeEngine {
            last: Mutex::new(None),
        });
        let src = GithubSource::for_tool("uv", PathBuf::from("/tmp/bin"), fake);
        let mut tool = ToolConfig::from_spec("github:astral-sh/uv");
        tool.exes = Some(vec!["uv".into(), "uvx".into()]);
        let err = src.install(&tool, &MockRunner::new()).unwrap_err();
        assert!(err.to_string().contains("not supported in M1"), "{err}");
    }

    #[test]
    fn rename_takes_precedence_for_final_name() {
        let fake = Box::new(FakeEngine {
            last: Mutex::new(None),
        });
        let src = GithubSource::for_tool("thekey", PathBuf::from("/tmp/bin"), fake);
        let mut tool = ToolConfig::from_spec("github:owner/repo");
        tool.rename = Some("newname".into());
        let out = src.install(&tool, &MockRunner::new()).unwrap();
        assert_eq!(out.install_paths, vec![PathBuf::from("/tmp/bin/newname")]);
    }
}
