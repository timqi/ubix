//! Templated-HTTP source (aqua-style `type: http` + `version_source`).
//!
//! Distinct from the fixed-URL `url` source: the locator is a URL *template*
//! with `{version}`, `{os}`, `{arch}` variables rendered at install time, and
//! the version can be discovered from a `version_source` (e.g. a GitHub tag)
//! rather than pinned. Canonical case: claude-code (binary on a templated GCS
//! URL, version from GitHub tags).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::http::HttpClient;
use crate::outdated::{self, Latest};
use crate::platform;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, url as url_source, InstallOutcome, SourceKind};

/// Render a URL template, substituting `{version}`, `{os}`, `{arch}`. `os`/`arch`
/// are the effective (post-replace) tokens. Any other `{token}` → error.
pub fn render_template(template: &str, version: &str, os: &str, arch: &str) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            bail!("unterminated `{{` in URL template `{template}`");
        };
        let var = &after[..end];
        let sub = match var {
            "version" => version,
            "os" => os,
            "arch" => arch,
            other => bail!(
                "unknown template variable `{{{other}}}` in `{template}` \
                 (supported: version, os, arch)"
            ),
        };
        out.push_str(sub);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Apply a runtime-token → url-token replacement map (e.g. `amd64` → `x64`).
/// Returns the mapped token, or the input unchanged when absent.
fn apply_replace(token: &str, map: Option<&std::collections::BTreeMap<String, String>>) -> String {
    map.and_then(|m| m.get(token))
        .cloned()
        .unwrap_or_else(|| token.to_string())
}

/// Resolve the `{version}` value for an http tool (§ priority: pin > discovery).
///
/// * `tag` or `version` field set → use it (a pin), stripping one leading `v`.
/// * else `version_source = "github:owner/repo"` → query latest and strip `v`.
/// * else → error.
pub fn resolve_version(
    tool: &ToolConfig,
    http: &dyn HttpClient,
) -> Result<String> {
    if let Some(pin) = tool.tag.as_deref().or(tool.version.as_deref()) {
        return Ok(strip_leading_v(pin).to_string());
    }
    let Some(vs) = tool.version_source.as_deref() else {
        bail!("http source requires `version` or `version_source`");
    };
    let latest = query_version_source(vs, http)?;
    Ok(strip_leading_v(&latest).to_string())
}

/// Query a `version_source` for its latest version. Currently supports
/// `github:owner/repo` (reusing the outdated GitHub-releases path).
fn query_version_source(version_source: &str, http: &dyn HttpClient) -> Result<String> {
    let parsed = parse_spec(version_source, SourceKind::Github)
        .with_context(|| format!("invalid version_source `{version_source}`"))?;
    match parsed.source {
        SourceKind::Github | SourceKind::Gitlab => {
            match outdated::latest_version(http, &parsed, None)? {
                Latest::Version(v) => Ok(v),
                Latest::NotApplicable => {
                    bail!("version_source `{version_source}` has no discoverable version")
                }
            }
        }
        other => bail!(
            "version_source `{version_source}` uses unsupported source `{other}:` \
             (use github:owner/repo)"
        ),
    }
}

fn strip_leading_v(s: &str) -> &str {
    s.strip_prefix('v').unwrap_or(s)
}

/// Choose the effective URL template for the running platform's libc: on
/// Linux+musl use `url_musl` if set, else the primary template.
pub fn select_template<'a>(
    primary: &'a str,
    url_musl: Option<&'a str>,
    is_musl: bool,
) -> &'a str {
    match (is_musl, url_musl) {
        (true, Some(m)) => m,
        _ => primary,
    }
}

/// Render the full download URL for an http tool: resolve version, pick the
/// libc template, apply os/arch replacements, and substitute.
pub fn render_url(
    tool: &ToolConfig,
    primary_template: &str,
    version: &str,
    goos: &str,
    goarch: &str,
    is_musl: bool,
) -> Result<String> {
    let template = select_template(primary_template, tool.url_musl.as_deref(), is_musl);
    let os = apply_replace(goos, tool.os_replace.as_ref());
    let arch = apply_replace(goarch, tool.arch_replace.as_ref());
    render_template(template, version, &os, &arch)
}

/// Install an http-source tool. `default_name` is the tool key.
pub fn install(
    tool: &ToolConfig,
    http: &dyn HttpClient,
    runner: &dyn CommandRunner,
    install_dir: &Path,
    default_name: &str,
) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Http)?;
    if parsed.source != SourceKind::Http {
        bail!("http source received non-http spec `{}`", tool.spec);
    }

    let version = resolve_version(tool, http)?;
    let is_musl = platform::is_musl(runner);
    let url = render_url(
        tool,
        &parsed.locator,
        &version,
        platform::goos(),
        platform::goarch(),
        is_musl,
    )?;

    let bytes = http.get_bytes(&url).with_context(|| format!("downloading {url}"))?;
    let content_sha = url_source::sha256_hex(&bytes);

    let install_paths = url_source::install_from_bytes(
        &url,
        &bytes,
        install_dir,
        tool.exe.as_deref(),
        tool.exes.as_deref(),
        tool.rename.as_deref(),
        default_name,
    )?;

    // Rendered filename (last path segment) for state.resolved_asset.
    let asset = url.split(['?', '#']).next().unwrap_or(&url);
    let asset = asset.rsplit('/').next().unwrap_or(asset).to_string();

    Ok(InstallOutcome {
        installed_version: version,
        resolved_asset: Some(asset),
        install_paths,
        sha256: Some(content_sha),
    })
}

/// Latest version for `outdated` (§7.1): query `version_source` if set, else n/a.
pub fn latest(tool: &ToolConfig, http: &dyn HttpClient) -> Result<Latest> {
    match tool.version_source.as_deref() {
        Some(vs) => Ok(Latest::Version(strip_leading_v(&query_version_source(vs, http)?).to_string())),
        None => Ok(Latest::NotApplicable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttp;
    use crate::runner::MockRunner;
    use std::collections::BTreeMap;

    const CLAUDE_TEMPLATE: &str = "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/{version}/{os}-{arch}/claude";
    const CLAUDE_MUSL: &str = "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/{version}/{os}-{arch}-musl/claude";

    fn arch_map() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("amd64".into(), "x64".into());
        m
    }

    #[test]
    fn render_basic_tokens() {
        assert_eq!(
            render_template("https://h/{version}/{os}-{arch}/bin", "1.2.3", "linux", "x64").unwrap(),
            "https://h/1.2.3/linux-x64/bin"
        );
    }

    #[test]
    fn render_unknown_var_errors() {
        let err = render_template("https://h/{libc}/x", "1", "linux", "amd64").unwrap_err();
        assert!(err.to_string().contains("unknown template variable"), "{err}");
    }

    #[test]
    fn render_unterminated_brace_errors() {
        assert!(render_template("https://h/{version", "1", "l", "a").is_err());
    }

    #[test]
    fn arch_replace_amd64_to_x64() {
        let mut tool = ToolConfig::from_spec(format!("http:{CLAUDE_TEMPLATE}"));
        tool.arch_replace = Some(arch_map());
        let url = render_url(&tool, CLAUDE_TEMPLATE, "1.0.88", "linux", "amd64", false).unwrap();
        assert!(url.ends_with("/1.0.88/linux-x64/claude"), "{url}");
    }

    #[test]
    fn claude_glibc_and_musl_urls() {
        let mut tool = ToolConfig::from_spec(format!("http:{CLAUDE_TEMPLATE}"));
        tool.url_musl = Some(CLAUDE_MUSL.into());
        tool.arch_replace = Some(arch_map());
        // glibc → primary template, x64.
        let glibc = render_url(&tool, CLAUDE_TEMPLATE, "1.0.88", "linux", "amd64", false).unwrap();
        assert_eq!(
            glibc,
            "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/1.0.88/linux-x64/claude"
        );
        // musl → musl template, x64.
        let musl = render_url(&tool, CLAUDE_TEMPLATE, "1.0.88", "linux", "amd64", true).unwrap();
        assert_eq!(
            musl,
            "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/1.0.88/linux-x64-musl/claude"
        );
    }

    #[test]
    fn select_template_prefers_musl_only_when_set_and_musl() {
        assert_eq!(select_template("p", Some("m"), true), "m");
        assert_eq!(select_template("p", Some("m"), false), "p");
        // musl but no url_musl → fall back to primary (glibc).
        assert_eq!(select_template("p", None, true), "p");
    }

    #[test]
    fn version_pin_takes_priority() {
        let mut tool = ToolConfig::from_spec("http:https://h/{version}/bin");
        tool.version = Some("2.0.0".into());
        tool.version_source = Some("github:owner/repo".into());
        // No HTTP call needed since pin wins.
        let http = MockHttp::new();
        assert_eq!(resolve_version(&tool, &http).unwrap(), "2.0.0");
    }

    #[test]
    fn tag_pin_strips_leading_v() {
        let mut tool = ToolConfig::from_spec("http:https://h/{version}/bin");
        tool.tag = Some("v1.5.0".into());
        let http = MockHttp::new();
        assert_eq!(resolve_version(&tool, &http).unwrap(), "1.5.0");
    }

    #[test]
    fn version_source_github_latest_with_v_strip() {
        let mut tool = ToolConfig::from_spec("http:https://h/{version}/bin");
        tool.version_source = Some("github:anthropics/claude-code".into());
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/anthropics/claude-code/releases/latest",
            r#"{"tag_name":"v1.0.88"}"#,
        );
        assert_eq!(resolve_version(&tool, &http).unwrap(), "1.0.88");
    }

    #[test]
    fn error_when_neither_version_nor_source() {
        let tool = ToolConfig::from_spec("http:https://h/{version}/bin");
        let http = MockHttp::new();
        let err = resolve_version(&tool, &http).unwrap_err();
        assert!(err.to_string().contains("requires `version` or `version_source`"), "{err}");
    }

    #[test]
    fn install_raw_binary_end_to_end() {
        // version_source discovers 1.0.88; template renders to the glibc URL;
        // the raw binary at that URL is installed as `claude`.
        let mut tool = ToolConfig::from_spec(format!("http:{CLAUDE_TEMPLATE}"));
        tool.url_musl = Some(CLAUDE_MUSL.into());
        tool.version_source = Some("github:anthropics/claude-code".into());
        tool.arch_replace = Some(arch_map());
        tool.exe = Some("claude".into());

        // On a glibc linux/amd64 test host the rendered URL is the x64 glibc one;
        // serve that. To keep the test host-agnostic, serve BOTH candidate URLs.
        let ver = "1.0.88";
        let glibc_url = format!(
            "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/{ver}/{os}-{arch}/claude",
            os = platform::goos(),
            arch = "x64",
        );
        let musl_url = format!(
            "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/{ver}/{os}-{arch}-musl/claude",
            os = platform::goos(),
            arch = "x64",
        );
        let http = MockHttp::new()
            .with_text(
                "https://api.github.com/repos/anthropics/claude-code/releases/latest",
                &format!(r#"{{"tag_name":"v{ver}"}}"#),
            )
            .with_bytes(&glibc_url, b"raw-claude-binary".to_vec())
            .with_bytes(&musl_url, b"raw-claude-binary".to_vec());
        // Non-musl runner (ldd not canned → is_musl=false) for determinism.
        let runner = MockRunner::new();
        let dir = tempfile::tempdir().unwrap();

        let out = install(&tool, &http, &runner, dir.path(), "claude").unwrap();
        assert_eq!(out.installed_version, "1.0.88");
        assert_eq!(out.resolved_asset.as_deref(), Some("claude"));
        assert_eq!(out.install_paths, vec![dir.path().join("claude")]);
        assert!(dir.path().join("claude").is_file());
        assert!(out.sha256.is_some());
    }

    #[test]
    fn latest_uses_version_source_or_na() {
        let mut tool = ToolConfig::from_spec("http:https://h/{version}/bin");
        // No version_source → n/a.
        let http = MockHttp::new();
        assert_eq!(latest(&tool, &http).unwrap(), Latest::NotApplicable);
        // With version_source → discovered version (v-stripped).
        tool.version_source = Some("github:anthropics/claude-code".into());
        let http2 = MockHttp::new().with_text(
            "https://api.github.com/repos/anthropics/claude-code/releases/latest",
            r#"{"tag_name":"v9.9.9"}"#,
        );
        assert_eq!(latest(&tool, &http2).unwrap(), Latest::Version("9.9.9".into()));
    }
}
