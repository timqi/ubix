//! Templating helpers for the `url` source. A `url:` locator is a URL that MAY
//! contain `{version}`/`{os}`/`{arch}` placeholders; a plain URL is just the
//! degenerate template with no placeholders. This module holds the render +
//! version-resolution helpers (`is_templated`, `resolve_version`, `render_url`,
//! `resolve_and_render`, `latest`); the actual install/download lives in
//! `url.rs`, which calls [`resolve_and_render`] for templated tools and
//! dispatches via [`is_templated`]. Canonical templated case: claude-code
//! (binary on a templated GCS URL, version from GitHub tags).
//!
//! `template:`/`http:` remain kept-for-compat spec aliases for `url:`.

use anyhow::{bail, Context, Result};

use crate::config::ToolConfig;
use crate::http::HttpClient;
use crate::outdated::{self, Latest};
use crate::platform;
use crate::runner::CommandRunner;
use crate::sources::{parse_spec, SourceKind};

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
pub fn apply_replace(
    token: &str,
    map: Option<&std::collections::BTreeMap<String, String>>,
) -> String {
    map.and_then(|m| m.get(token))
        .cloned()
        .unwrap_or_else(|| token.to_string())
}

/// Render `{os}`/`{arch}` tokens (only) in `s`, applying the given replace maps
/// first. Reused by the platform-portable `matching` resolution. Unknown token
/// → error; a `{version}` here is unknown (matching has no version).
pub fn render_os_arch(
    s: &str,
    goos: &str,
    goarch: &str,
    os_replace: Option<&std::collections::BTreeMap<String, String>>,
    arch_replace: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<String> {
    let os = apply_replace(goos, os_replace);
    let arch = apply_replace(goarch, arch_replace);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            bail!("unterminated `{{` in `{s}`");
        };
        let sub = match &after[..end] {
            "os" => os.as_str(),
            "arch" => arch.as_str(),
            other => bail!("unknown template variable `{{{other}}}` in `{s}` (supported: os, arch)"),
        };
        out.push_str(sub);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
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
        bail!("template source requires `version` or `version_source`");
    };
    crate::step!("resolving version via {vs}");
    let latest = query_version_source(vs, http)?;
    let version = strip_leading_v(&latest).to_string();
    crate::step!("latest = {version}");
    Ok(version)
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

/// Whether a `url` tool needs the templated path: the URL carries a
/// `{version}`/`{os}`/`{arch}` placeholder, or a template-only field is set
/// (`version_source`, `url_musl`, `os_replace`/`arch_replace`). A plain fixed URL
/// (none of these) takes the direct-download path in `url.rs`.
pub fn is_templated(locator: &str, tool: &ToolConfig) -> bool {
    locator.contains('{')
        || tool.version_source.is_some()
        || tool.url_musl.is_some()
        || tool.os_replace.is_some()
        || tool.arch_replace.is_some()
}

/// Resolve the version and render the download URL for a templated `url` tool —
/// the templating spine, kept here so `url::install` stays a plain
/// download-and-install flow. Callers gate on [`is_templated`]. Returns
/// `(resolved_version, rendered_url)`.
pub fn resolve_and_render(
    tool: &ToolConfig,
    http: &dyn HttpClient,
    runner: &dyn CommandRunner,
    locator: &str,
) -> Result<(String, String)> {
    let version = resolve_version(tool, http)?;
    let is_musl = platform::is_musl(runner);
    let url = render_url(
        tool,
        locator,
        &version,
        platform::goos(),
        platform::goarch(),
        is_musl,
    )?;
    Ok((version, url))
}

/// Latest version for `outdated` (§7.1). A `version`/`tag` pin is the latest by
/// definition (mirrors [`resolve_version`]'s priority) so pinned tools never show
/// as perpetually outdated; otherwise query `version_source`, else n/a.
pub fn latest(tool: &ToolConfig, http: &dyn HttpClient) -> Result<Latest> {
    if let Some(pin) = tool.tag.as_deref().or(tool.version.as_deref()) {
        return Ok(Latest::Version(strip_leading_v(pin).to_string()));
    }
    match tool.version_source.as_deref() {
        Some(vs) => Ok(Latest::Version(strip_leading_v(&query_version_source(vs, http)?).to_string())),
        None => Ok(Latest::NotApplicable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttp;
    use crate::platform;
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
        let mut tool = ToolConfig::from_spec(format!("template:{CLAUDE_TEMPLATE}"));
        tool.arch_replace = Some(arch_map());
        let url = render_url(&tool, CLAUDE_TEMPLATE, "1.0.88", "linux", "amd64", false).unwrap();
        assert!(url.ends_with("/1.0.88/linux-x64/claude"), "{url}");
    }

    #[test]
    fn claude_glibc_and_musl_urls() {
        let mut tool = ToolConfig::from_spec(format!("template:{CLAUDE_TEMPLATE}"));
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
        let mut tool = ToolConfig::from_spec("template:https://h/{version}/bin");
        tool.version = Some("2.0.0".into());
        tool.version_source = Some("github:owner/repo".into());
        // No HTTP call needed since pin wins.
        let http = MockHttp::new();
        assert_eq!(resolve_version(&tool, &http).unwrap(), "2.0.0");
    }

    #[test]
    fn tag_pin_strips_leading_v() {
        let mut tool = ToolConfig::from_spec("template:https://h/{version}/bin");
        tool.tag = Some("v1.5.0".into());
        let http = MockHttp::new();
        assert_eq!(resolve_version(&tool, &http).unwrap(), "1.5.0");
    }

    #[test]
    fn version_source_github_latest_with_v_strip() {
        let mut tool = ToolConfig::from_spec("template:https://h/{version}/bin");
        tool.version_source = Some("github:anthropics/claude-code".into());
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/anthropics/claude-code/releases/latest",
            r#"{"tag_name":"v1.0.88"}"#,
        );
        assert_eq!(resolve_version(&tool, &http).unwrap(), "1.0.88");
    }

    #[test]
    fn error_when_neither_version_nor_source() {
        let tool = ToolConfig::from_spec("template:https://h/{version}/bin");
        let http = MockHttp::new();
        let err = resolve_version(&tool, &http).unwrap_err();
        assert!(err.to_string().contains("requires `version` or `version_source`"), "{err}");
    }

    #[test]
    fn install_accepts_legacy_http_prefix_alias() {
        // A legacy `http:` spec still installs via the template source.
        let mut tool = ToolConfig::from_spec("http:https://example.com/{version}/tool");
        tool.version = Some("1.0.0".into());
        let url = "https://example.com/1.0.0/tool";
        let http = MockHttp::new().with_bytes(url, b"bin".to_vec());
        let runner = MockRunner::new();
        let dir = tempfile::tempdir().unwrap();
        let out = crate::sources::url::install(&tool, &http, &runner, dir.path(), "tool").unwrap();
        assert_eq!(out.installed_version, "1.0.0");
        assert_eq!(out.install_paths, vec![dir.path().join("tool")]);
    }

    #[test]
    fn install_raw_binary_end_to_end() {
        // version_source discovers 1.0.88; template renders to the glibc URL;
        // the raw binary at that URL is installed as `claude`.
        let mut tool = ToolConfig::from_spec(format!("template:{CLAUDE_TEMPLATE}"));
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

        let out = crate::sources::url::install(&tool, &http, &runner, dir.path(), "claude").unwrap();
        assert_eq!(out.installed_version, "1.0.88");
        assert_eq!(out.resolved_asset.as_deref(), Some("claude"));
        assert_eq!(out.install_paths, vec![dir.path().join("claude")]);
        assert!(dir.path().join("claude").is_file());
        assert!(out.sha256.is_some());
    }

    #[test]
    fn latest_uses_version_source_or_na() {
        let mut tool = ToolConfig::from_spec("template:https://h/{version}/bin");
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

    #[test]
    fn latest_honors_pin_over_version_source() {
        // A pin makes `latest` == the pin (v-stripped) so a pinned tool never
        // shows as perpetually outdated even when version_source has advanced.
        let mut tool = ToolConfig::from_spec("template:https://h/{version}/bin");
        tool.version = Some("v1.2.3".into());
        tool.version_source = Some("github:anthropics/claude-code".into());
        // No HTTP canned: the pin path must not query version_source.
        let http = MockHttp::new();
        assert_eq!(latest(&tool, &http).unwrap(), Latest::Version("1.2.3".into()));
    }
}
