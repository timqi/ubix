//! GitHub release source (§5.1). Delegates asset/platform heuristics to ubi.

use anyhow::{bail, Result};
use ubi::ForgeType;

use crate::config::ToolConfig;
use crate::engine::{ReleaseEngine, ReleaseRequest};
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

        // `exes` (multi-entry, → ubi extract_all) is incompatible with `rename`
        // (→ ubi rename_exe_to); ubi's build() would error deep inside. Fail
        // clearly at build-request time instead (§5.1).
        if tool.exes.as_ref().is_some_and(|e| !e.is_empty()) && tool.rename.is_some() {
            bail!("`exes` and `rename` cannot be combined (rename applies to a single exe only)");
        }

        let final_name = tool
            .rename
            .clone()
            .or_else(|| tool.exe.clone())
            .unwrap_or_else(|| self.tool_name.clone());

        // Resolve platform-portable `matching` for the current OS/arch; `None`
        // means "no filter" so ubi's heuristic decides (we won't call .matching()).
        let matching = tool.resolved_matching(crate::platform::goos(), crate::platform::goarch())?;

        Ok(ReleaseRequest {
            project: parsed.locator,
            forge: ForgeType::GitHub,
            tag: tool.tag.clone(),
            matching,
            exe: tool.exe.clone(),
            exes: tool.exes.clone().unwrap_or_default(),
            rename: tool.rename.clone(),
            install_dir: self.install_dir.clone(),
            final_name,
            github_token: github_token_from_env(),
            gitlab_token: None,
            api_base_url: None,
        })
    }
}

impl Source for GithubSource {
    fn install(&self, tool: &ToolConfig, _runner: &dyn CommandRunner) -> Result<InstallOutcome> {
        let req = self.build_request(tool)?;
        crate::step!(
            "downloading & extracting {} via ubi (large assets may take a while)…",
            req.project
        );
        let result = self.engine.install(&req)?;
        Ok(InstallOutcome {
            installed_version: result
                .version
                .unwrap_or_else(|| "latest".to_string()),
            resolved_asset: None,
            install_paths: result.install_paths,
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
            let paths = if req.exes.is_empty() {
                vec![req.install_dir.join(&req.final_name)]
            } else {
                req.exes.iter().map(|e| req.install_dir.join(e)).collect()
            };
            Ok(EngineResult {
                install_paths: paths,
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
        tool.matching = Some(crate::config::PlatformString::One("linux".into()));
        tool.exe = Some("codex".into());
        let runner = MockRunner::new();
        let out = src.install(&tool, &runner).unwrap();
        assert_eq!(out.install_paths, vec![PathBuf::from("/tmp/bin/codex")]);
        assert_eq!(out.sha256.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn build_request_resolves_per_platform_matching() {
        use crate::config::PlatformString;
        use std::collections::BTreeMap;
        let fake = FakeEngine { last: Mutex::new(None) };
        // Per-platform map with the CURRENT platform's exact key plus a `*`
        // fallback, so the test is host-agnostic and exercises resolution.
        let goos = crate::platform::goos();
        let goarch = crate::platform::goarch();
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        map.insert(format!("{goos}-{goarch}"), "exact-platform-asset".into());
        map.insert("*".into(), "fallback-asset".into());

        // Build the request directly to capture the resolved matching.
        let src = GithubSource::for_tool("codex", PathBuf::from("/tmp/bin"), Box::new(fake));
        let mut tool = ToolConfig::from_spec("github:openai/codex");
        tool.matching = Some(PlatformString::PerPlatform(map));
        let req = src.build_request(&tool).unwrap();
        assert_eq!(req.matching.as_deref(), Some("exact-platform-asset"));
    }

    #[test]
    fn build_request_matching_none_lets_ubi_decide() {
        let src = GithubSource::for_tool("eza", PathBuf::from("/tmp/bin"), Box::new(FakeEngine {
            last: Mutex::new(None),
        }));
        let tool = ToolConfig::from_spec("github:eza-community/eza");
        let req = src.build_request(&tool).unwrap();
        assert_eq!(req.matching, None);
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
    fn exes_installs_multiple_entries() {
        let fake = Box::new(FakeEngine {
            last: Mutex::new(None),
        });
        let src = GithubSource::for_tool("uv", PathBuf::from("/tmp/bin"), fake);
        let mut tool = ToolConfig::from_spec("github:astral-sh/uv");
        tool.exes = Some(vec!["uv".into(), "uvx".into()]);
        let out = src.install(&tool, &MockRunner::new()).unwrap();
        assert_eq!(
            out.install_paths,
            vec![
                PathBuf::from("/tmp/bin/uv"),
                PathBuf::from("/tmp/bin/uvx"),
            ]
        );
    }

    #[test]
    fn exes_plus_rename_rejected() {
        let fake = Box::new(FakeEngine { last: Mutex::new(None) });
        let src = GithubSource::for_tool("uv", PathBuf::from("/tmp/bin"), fake);
        let mut tool = ToolConfig::from_spec("github:astral-sh/uv");
        tool.exes = Some(vec!["uv".into(), "uvx".into()]);
        tool.rename = Some("nope".into());
        let err = src.install(&tool, &MockRunner::new()).unwrap_err();
        assert!(err.to_string().contains("cannot be combined"), "{err}");
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
