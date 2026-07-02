//! aqua-registry integration as a **config generator** (plan, Plan B).
//!
//! aqua is NOT a runtime source (there is no `SourceKind::Aqua`). Instead we
//! fetch an aqua package's `registry.yaml`, resolve its asset-selection template
//! for the current latest version across linux/darwin, and SYNTHESIZE a standard
//! `github:` [`ToolConfig`] (spec + per-platform `matching` + exe/rename). The
//! CLI's `add`/`search` commands call this; the install path never sees aqua.

pub mod registry;
pub mod resolve;
pub mod schema;
pub mod synth;
pub mod template;

use anyhow::{bail, Context, Result};

use crate::config::{PlatformString, ToolConfig};
use crate::http::HttpClient;
use crate::outdated::{self, Latest};
use crate::sources::{ParsedSpec, SourceKind};

pub use registry::search_index;
pub use resolve::registry_url;

/// Resolve an aqua package into a synthesized `github:` [`ToolConfig`].
///
/// Steps (plan §3): fetch registry.yaml → verify github_release → discover the
/// latest version via `outdated::latest_version` → select the version branch →
/// synth per-platform matching. Returns `(tool_name, ToolConfig)`.
///
/// Any unsupported construct degrades to a `bail!` that names the registry.yaml
/// URL (plan §9).
pub fn resolve_package(
    http: &dyn HttpClient,
    owner: &str,
    repo: &str,
    name_override: Option<&str>,
) -> Result<(String, ToolConfig)> {
    let pkg = registry::fetch_package(http, owner, repo)?;

    // Only github_release is supported at the top level (plan §2/§9).
    match pkg.type_.as_deref() {
        Some("github_release") | None => {}
        Some(other) => bail!(
            "unsupported aqua construct: package type `{other}` for {owner}/{repo}; \
             see {} and add a `github:` entry manually",
            registry_url(owner, repo)
        ),
    }

    // Discover latest version (honors UBIX_GITHUB_TOKEN via outdated → github).
    let spec = ParsedSpec {
        source: SourceKind::Github,
        locator: format!("{owner}/{repo}"),
    };
    let tag = match outdated::latest_version(http, &spec, None)
        .with_context(|| format!("discovering latest version for {owner}/{repo}"))?
    {
        Latest::Version(v) => v,
        Latest::NotApplicable => bail!("no discoverable latest version for {owner}/{repo}"),
    };

    let branch = resolve::select_branch(&pkg, &tag, owner, repo)?;
    synth::synth(&branch, &tag, owner, repo, name_override)
}

/// Render a pretty TOML block for a synthesized tool (for `ubix search` output).
/// This is display-only; `add` persists via the normal `Config::save` path.
pub fn generate_snippet(name: &str, tool: &ToolConfig) -> String {
    let mut out = String::new();
    out.push_str(&format!("[tools.{name}]\n"));
    out.push_str(&format!("spec = \"{}\"\n", tool.spec));
    if let Some(exe) = &tool.exe {
        out.push_str(&format!("exe = \"{exe}\"\n"));
    }
    if let Some(exes) = &tool.exes {
        let list = exes
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("exes = [{list}]\n"));
    }
    if let Some(rename) = &tool.rename {
        out.push_str(&format!("rename = \"{rename}\"\n"));
    }
    if let Some(PlatformString::PerPlatform(map)) = &tool.matching {
        out.push_str(&format!("[tools.{name}.matching]\n"));
        for (k, v) in map {
            out.push_str(&format!("{k:<12} = \"{v}\"\n"));
        }
    } else if let Some(PlatformString::One(s)) = &tool.matching {
        out.push_str(&format!("matching = \"{s}\"\n"));
    }
    out
}

/// The matching value that WILL be used on the current platform, for a one-line
/// preview note. `None` when the current platform is unsupported.
pub fn current_platform_matching(tool: &ToolConfig) -> Option<String> {
    match &tool.matching {
        Some(PlatformString::PerPlatform(map)) => {
            let key = format!("{}-{}", crate::platform::goos(), crate::platform::goarch());
            map.get(&key).cloned()
        }
        Some(PlatformString::One(s)) => Some(s.clone()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttp;

    const CODEX: &str = include_str!("../../tests/fixtures/aqua/openai_codex.yaml");
    const GH: &str = include_str!("../../tests/fixtures/aqua/cli_cli.yaml");

    fn mock_codex() -> MockHttp {
        MockHttp::new()
            .with_text(&registry::pkg_url("openai", "codex"), CODEX)
            .with_text(
                "https://api.github.com/repos/openai/codex/releases/latest",
                r#"{"tag_name":"rust-v0.20.0"}"#,
            )
    }

    #[test]
    fn resolve_package_codex_end_to_end() {
        let http = mock_codex();
        let (name, tool) = resolve_package(&http, "openai", "codex", None).unwrap();
        assert_eq!(name, "codex");
        assert_eq!(tool.spec, "github:openai/codex");
        match tool.matching.unwrap() {
            PlatformString::PerPlatform(m) => {
                assert_eq!(m["linux-amd64"], "codex-x86_64-unknown-linux-musl.zst");
                assert_eq!(m["darwin-arm64"], "codex-aarch64-apple-darwin.zst");
            }
            _ => panic!("expected per-platform"),
        }
    }

    #[test]
    fn resolve_package_gh_end_to_end() {
        let http = MockHttp::new()
            .with_text(&registry::pkg_url("cli", "cli"), GH)
            .with_text(
                "https://api.github.com/repos/cli/cli/releases/latest",
                r#"{"tag_name":"v2.65.0"}"#,
            );
        let (name, tool) = resolve_package(&http, "cli", "cli", None).unwrap();
        assert_eq!(name, "gh");
        match tool.matching.unwrap() {
            PlatformString::PerPlatform(m) => {
                assert_eq!(m["linux-amd64"], "_linux_amd64.tar.gz");
                assert_eq!(m["darwin-amd64"], "_macOS_amd64.zip");
            }
            _ => panic!("expected per-platform"),
        }
    }

    #[test]
    fn generate_snippet_contains_key_fields() {
        let http = mock_codex();
        let (name, tool) = resolve_package(&http, "openai", "codex", None).unwrap();
        let snippet = generate_snippet(&name, &tool);
        assert!(snippet.contains("[tools.codex]"));
        assert!(snippet.contains("spec = \"github:openai/codex\""));
        assert!(snippet.contains("exe = \"codex\""));
        assert!(snippet.contains("[tools.codex.matching]"));
        assert!(snippet.contains("codex-x86_64-unknown-linux-musl.zst"));
    }

    #[test]
    fn unsupported_type_degrades() {
        let yaml = r#"
packages:
  - type: http
    repo_owner: x
    repo_name: y
    url: https://example.com/{{.Version}}
"#;
        let http = MockHttp::new().with_text(&registry::pkg_url("x", "y"), yaml);
        let err = resolve_package(&http, "x", "y", None).unwrap_err();
        assert!(err.to_string().contains("unsupported aqua construct"), "{err}");
        assert!(err.to_string().contains("registry.yaml"), "{err}");
    }
}
