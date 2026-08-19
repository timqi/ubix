//! aqua-registry integration as a **config generator** (plan, Plan B).
//!
//! aqua is NOT a runtime source (there is no `SourceKind::Aqua`). Instead we
//! fetch an aqua package's `registry.yaml`, resolve its asset-selection template
//! for the current latest version across linux/darwin, and SYNTHESIZE a standard
//! `github:` [`ToolConfig`] (spec + per-platform `matching` + exe/rename). The
//! CLI's `add`/`search` commands call this; the install path never sees aqua.

pub mod hint;
pub mod prune;
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

    // Only github_release is supported at the top level (plan §2/§9). `type:
    // http` packages (templated URL, e.g. claude-code) can't be synthesized as
    // `github:`, but ubix's `url:` source covers exactly that shape — synthesize
    // a `url:` ToolConfig so `add`/`search --add` install it directly, just like
    // github. When the shape is too ambiguous to synth safely, fall back to the
    // ready-to-paste `ubix add 'url:…'` hint.
    match pkg.type_.as_deref() {
        Some("github_release") | None => {}
        Some("http") => {
            return hint::synth_url_config(&pkg, owner, repo, name_override)
                .ok_or_else(|| anyhow::anyhow!(hint::http_hint(&pkg, owner, repo)));
        }
        Some(other) => {
            return Err(resolve::unsupported(
                owner,
                repo,
                format!("package type `{other}` for {owner}/{repo}"),
            ))
        }
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
    let (name, mut tool) = synth::synth(&branch, &tag, owner, repo, name_override)?;

    // Drop per-platform `matching` where ubi would already pick a viable asset
    // on its own (best-effort: needs the real asset list; keep matching if the
    // fetch fails). This trims the common case where matching is pure noise.
    prune_synthesized_matching(http, &mut tool, &spec.locator);

    Ok((name, tool))
}

/// Prune redundant per-platform `matching` from a synthesized tool using the
/// release asset list + a simulation of ubi's picker. No-op on any error (keeps
/// the safe, fully-specified matching) or when matching isn't a per-platform map.
fn prune_synthesized_matching(http: &dyn HttpClient, tool: &mut ToolConfig, locator: &str) {
    let Some(PlatformString::PerPlatform(map)) = &tool.matching else {
        return;
    };
    let assets = match outdated::github_release_asset_names(http, locator) {
        Ok(a) if !a.is_empty() => a,
        _ => return, // fetch failed / no assets → keep matching (safe default)
    };
    let pruned = prune::prune_matching(map, &assets);
    // Redundant platforms are neutralized to "" (not dropped — see prune_matching).
    let dropped = pruned.values().filter(|v| v.is_empty()).count();
    if dropped == 0 {
        return;
    }
    if pruned.values().all(String::is_empty) {
        crate::step!("matching not needed (ubi selects assets on its own); omitting");
        tool.matching = None;
    } else {
        crate::step!("pruned {dropped} redundant matching entr(y/ies)");
        tool.matching = Some(PlatformString::PerPlatform(pruned));
    }
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
    // url-source scalars — MUST precede any `[tools.name.X]` table header below,
    // else TOML would nest them under that subtable.
    if let Some(vs) = &tool.version_source {
        out.push_str(&format!("version_source = \"{vs}\"\n"));
    }
    if let Some(m) = &tool.url_musl {
        out.push_str(&format!("url_musl = \"{m}\"\n"));
    }
    if let Some(PlatformString::PerPlatform(map)) = &tool.matching {
        out.push_str(&format!("[tools.{name}.matching]\n"));
        for (k, v) in map {
            out.push_str(&format!("{k:<12} = \"{v}\"\n"));
        }
    } else if let Some(PlatformString::One(s)) = &tool.matching {
        out.push_str(&format!("matching = \"{s}\"\n"));
    }
    if let Some(map) = &tool.arch_replace {
        out.push_str(&format!("[tools.{name}.arch_replace]\n"));
        for (k, v) in map {
            out.push_str(&format!("{k} = \"{v}\"\n"));
        }
    }
    if let Some(map) = &tool.os_replace {
        out.push_str(&format!("[tools.{name}.os_replace]\n"));
        for (k, v) in map {
            out.push_str(&format!("{k} = \"{v}\"\n"));
        }
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
        // A non-http unsupported type still degrades with the generic message.
        let yaml = r#"
packages:
  - type: go_install
    repo_owner: x
    repo_name: y
"#;
        let http = MockHttp::new().with_text(&registry::pkg_url("x", "y"), yaml);
        let err = resolve_package(&http, "x", "y", None).unwrap_err();
        assert!(err.to_string().contains("unsupported aqua construct"), "{err}");
        assert!(err.to_string().contains("registry.yaml"), "{err}");
    }

    #[test]
    fn http_type_synthesizes_url_config() {
        // A `type: http` package now synthesizes a `url:` ToolConfig (like
        // github), so `add`/`search --add` install it directly — no more bail.
        let yaml = r#"
packages:
  - type: http
    repo_owner: x
    repo_name: y
    url: https://example.com/{{.Version}}/{{.OS}}-{{.Arch}}/y
    files:
      - name: y
    version_source: github_tag
"#;
        // No latest-version fetch is canned: the http arm must NOT hit the github
        // release API (it returns before version discovery).
        let http = MockHttp::new().with_text(&registry::pkg_url("x", "y"), yaml);
        let (name, tool) = resolve_package(&http, "x", "y", None).unwrap();
        assert_eq!(name, "y");
        assert_eq!(tool.spec, "url:https://example.com/{version}/{os}-{arch}/y");
        assert_eq!(tool.exe.as_deref(), Some("y"));
        assert_eq!(tool.version_source.as_deref(), Some("github:x/y"));
    }

    #[test]
    fn http_type_ambiguous_url_falls_back_to_hint() {
        // A URL only in a version-gated branch (no `"true"` catch-all) can't be
        // synthesized safely — degrade to the manual `ubix add 'url:…'` hint.
        let yaml = r#"
packages:
  - type: http
    repo_owner: x
    repo_name: y
    files:
      - name: y
    version_source: github_tag
    version_overrides:
      - version_constraint: 'semver("< 2.0.0")'
        url: https://example.com/{{.Version}}/{{.OS}}-{{.Arch}}/y
"#;
        let http = MockHttp::new().with_text(&registry::pkg_url("x", "y"), yaml);
        let err = resolve_package(&http, "x", "y", None).unwrap_err();
        let msg = err.to_string();
        // Degrades to the manual `ubix add 'url:…'` hint (not an auto-install).
        assert!(msg.contains("ubix add 'url:"), "{msg}");
        assert!(msg.contains("--version-source github:x/y"), "{msg}");
    }

    #[test]
    fn generate_snippet_renders_url_fields() {
        let mut tool = ToolConfig::from_spec("url:https://h/{version}/{os}-{arch}/claude");
        tool.exe = Some("claude".into());
        tool.version_source = Some("github:anthropics/claude-code".into());
        tool.url_musl = Some("https://h/{version}/{os}-{arch}-musl/claude".into());
        tool.arch_replace = Some([("amd64".to_string(), "x64".to_string())].into_iter().collect());
        let snippet = generate_snippet("claude", &tool);
        // Scalars appear before the `[tools.claude.arch_replace]` table header so
        // TOML doesn't nest them under it.
        let vs_at = snippet.find("version_source =").unwrap();
        let musl_at = snippet.find("url_musl =").unwrap();
        let table_at = snippet.find("[tools.claude.arch_replace]").unwrap();
        assert!(vs_at < table_at && musl_at < table_at, "{snippet}");
        assert!(snippet.contains("amd64 = \"x64\""), "{snippet}");
        // Round-trips as valid TOML under a config document.
        let cfg: crate::config::Config = toml::from_str(&snippet).unwrap();
        assert_eq!(cfg.tools["claude"].version_source.as_deref(), Some("github:anthropics/claude-code"));
    }
}
