//! GitLab release source (§5.1, M2). Like github but routes through ubi's
//! `forge(GitLab)` and, for self-hosted instances, `api_base_url(<host>/api/v4)`.

use anyhow::{bail, Result};
use ubi::ForgeType;

use crate::config::ToolConfig;
use crate::engine::{ReleaseEngine, ReleaseRequest};
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// Turn an optional self-hosted `host` (`https://gitlab.fish`) into the ubi
/// `api_base_url` (`https://gitlab.fish/api/v4`). Public gitlab.com → None (ubi
/// uses its built-in default).
pub fn api_base_url(host: Option<&str>) -> Option<String> {
    let host = host?.trim().trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    Some(format!("{host}/api/v4"))
}

/// Read the GitLab token from `UBIX_GITLAB_TOKEN` (D6).
pub fn gitlab_token_from_env() -> Option<String> {
    std::env::var("UBIX_GITLAB_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Build the release request for a gitlab tool.
pub fn build_request(
    tool: &ToolConfig,
    tool_name: &str,
    install_dir: std::path::PathBuf,
) -> Result<ReleaseRequest> {
    let parsed = parse_spec(&tool.spec, SourceKind::Gitlab)?;
    if parsed.source != SourceKind::Gitlab {
        bail!("gitlab source received non-gitlab spec `{}`", tool.spec);
    }
    // `exes` (extract_all) and `rename` (rename_exe_to) cannot be combined; ubi
    // build() would error. Fail clearly at build-request time (§5.1).
    if tool.exes.as_ref().is_some_and(|e| !e.is_empty()) && tool.rename.is_some() {
        bail!("`exes` and `rename` cannot be combined (rename applies to a single exe only)");
    }
    let final_name = tool
        .rename
        .clone()
        .or_else(|| tool.exe.clone())
        .unwrap_or_else(|| tool_name.to_string());

    // Resolve platform-portable `matching` for the current OS/arch.
    let matching = tool.resolved_matching(crate::platform::goos(), crate::platform::goarch())?;

    Ok(ReleaseRequest {
        project: parsed.locator,
        forge: ForgeType::GitLab,
        tag: tool.tag.clone(),
        matching,
        exe: tool.exe.clone(),
        exes: tool.exes.clone().unwrap_or_default(),
        rename: tool.rename.clone(),
        install_dir,
        final_name,
        github_token: None,
        gitlab_token: gitlab_token_from_env(),
        api_base_url: api_base_url(tool.host.as_deref()),
    })
}

/// Install a gitlab release via the shared engine.
pub fn install(
    tool: &ToolConfig,
    tool_name: &str,
    install_dir: std::path::PathBuf,
    engine: &dyn ReleaseEngine,
) -> Result<InstallOutcome> {
    let req = build_request(tool, tool_name, install_dir)?;
    crate::step!(
        "downloading & extracting {} via ubi (large assets may take a while)…",
        req.project
    );
    let result = engine.install(&req)?;
    Ok(InstallOutcome {
        installed_version: result.version.unwrap_or_else(|| "latest".to_string()),
        resolved_asset: None,
        install_paths: result.install_paths,
        sha256: Some(result.sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineResult;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FakeEngine {
        last: Mutex<Option<ReleaseRequest>>,
    }
    impl ReleaseEngine for FakeEngine {
        fn install(&self, req: &ReleaseRequest) -> Result<EngineResult> {
            *self.last.lock().unwrap() = Some(req.clone());
            let path = req.install_dir.join(&req.final_name);
            Ok(EngineResult {
                install_paths: vec![path],
                sha256: "abc".into(),
                version: req.tag.clone().or_else(|| Some("v1".into())),
            })
        }
    }

    #[test]
    fn public_gitlab_no_api_base() {
        assert_eq!(api_base_url(None), None);
        assert_eq!(api_base_url(Some("")), None);
    }

    #[test]
    fn self_hosted_api_base_mapping() {
        assert_eq!(
            api_base_url(Some("https://gitlab.fish")).as_deref(),
            Some("https://gitlab.fish/api/v4")
        );
        // Trailing slash is trimmed.
        assert_eq!(
            api_base_url(Some("https://gitlab.fish/")).as_deref(),
            Some("https://gitlab.fish/api/v4")
        );
    }

    #[test]
    fn build_request_sets_forge_and_api_base() {
        let mut t = ToolConfig::from_spec("gitlab:group/sub/repo");
        t.host = Some("https://gitlab.fish".into());
        let req = build_request(&t, "repo", PathBuf::from("/tmp/bin")).unwrap();
        assert_eq!(req.forge, ForgeType::GitLab);
        assert_eq!(req.project, "group/sub/repo");
        assert_eq!(req.api_base_url.as_deref(), Some("https://gitlab.fish/api/v4"));
    }

    #[test]
    fn exes_plus_rename_rejected() {
        let mut t = ToolConfig::from_spec("gitlab:group/repo");
        t.exes = Some(vec!["a".into(), "b".into()]);
        t.rename = Some("x".into());
        let err = build_request(&t, "repo", PathBuf::from("/tmp/bin")).unwrap_err();
        assert!(err.to_string().contains("cannot be combined"), "{err}");
    }

    #[test]
    fn install_flows_through_engine() {
        let engine = FakeEngine { last: Mutex::new(None) };
        let t = ToolConfig::from_spec("gitlab:group/repo");
        let out = install(&t, "repo", PathBuf::from("/tmp/bin"), &engine).unwrap();
        assert_eq!(out.install_paths, vec![PathBuf::from("/tmp/bin/repo")]);
        let captured = engine.last.lock().unwrap().clone().unwrap();
        assert_eq!(captured.forge, ForgeType::GitLab);
    }
}
