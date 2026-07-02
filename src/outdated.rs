//! `outdated` latest-version queries (§7.1). Every query goes through the
//! `HttpClient` seam and the response parsing is a pure function, so all of it
//! is unit-tested with fixtures and no network.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::http::HttpClient;
use crate::sources::{ParsedSpec, SourceKind};

/// The latest version for a source, or `n/a` for sources without the concept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Latest {
    Version(String),
    NotApplicable,
}

impl std::fmt::Display for Latest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Latest::Version(v) => f.write_str(v),
            Latest::NotApplicable => f.write_str("n/a"),
        }
    }
}

/// Resolve the latest version for a parsed spec via the given HTTP client.
/// `host` is the optional self-hosted gitlab base (`https://gitlab.fish`).
pub fn latest_version(
    http: &dyn HttpClient,
    spec: &ParsedSpec,
    host: Option<&str>,
) -> Result<Latest> {
    match spec.source {
        SourceKind::Github => {
            let url = format!(
                "https://api.github.com/repos/{}/releases/latest",
                spec.locator
            );
            let body = http.get_text(&url)?;
            Ok(Latest::Version(parse_github(&body)?))
        }
        SourceKind::Gitlab => {
            let base = host.unwrap_or("https://gitlab.com").trim_end_matches('/');
            // GitLab needs the URL-encoded project path as the :id.
            let encoded = urlencode(&spec.locator);
            let url = format!("{base}/api/v4/projects/{encoded}/releases");
            let body = http.get_text(&url)?;
            Ok(Latest::Version(parse_gitlab(&body)?))
        }
        SourceKind::Pypi => {
            let pkg = pkg_name(&spec.locator);
            let url = format!("https://pypi.org/pypi/{pkg}/json");
            let body = http.get_text(&url)?;
            Ok(Latest::Version(parse_pypi(&body)?))
        }
        SourceKind::Npm => {
            let pkg = npm_pkg_name(&spec.locator);
            let url = format!("https://registry.npmjs.org/{pkg}/latest");
            let body = http.get_text(&url)?;
            Ok(Latest::Version(parse_npm(&body)?))
        }
        SourceKind::Cargo => {
            let name = pkg_name(&spec.locator);
            let url = format!("https://crates.io/api/v1/crates/{name}");
            let body = http.get_text(&url)?;
            Ok(Latest::Version(parse_cargo(&body)?))
        }
        SourceKind::Go => {
            let module = spec.locator.split('@').next().unwrap_or(&spec.locator);
            let url = format!("https://proxy.golang.org/{module}/@latest");
            let body = http.get_text(&url)?;
            Ok(Latest::Version(parse_go(&body)?))
        }
        SourceKind::Url => Ok(Latest::NotApplicable),
        // template latest depends on the tool's `version_source` config, not the
        // spec alone; the CLI routes template tools to `sources::template::latest`.
        SourceKind::Template => Ok(Latest::NotApplicable),
    }
}

/// Strip a version constraint (`ruff==0.6`, `ruff@1`) to the bare package name.
fn pkg_name(locator: &str) -> String {
    locator
        .split(['@', '='])
        .next()
        .unwrap_or(locator)
        .to_string()
}

/// Bare npm package name, preserving a leading `@scope/`. npm uses `@` both for
/// the scope prefix AND the version separator (`@scope/pkg@1.2.3`), so a plain
/// split-on-`@` would wrongly return `""` for scoped packages.
fn npm_pkg_name(locator: &str) -> String {
    match locator.strip_prefix('@') {
        // Scoped: `@scope/pkg[@version]` → keep `@scope/pkg`, drop a trailing
        // `@version` (the second `@`).
        Some(rest) => match rest.split_once('@') {
            Some((name, _version)) => format!("@{name}"),
            None => format!("@{rest}"),
        },
        // Unscoped: `pkg[@version]`.
        None => pkg_name(locator),
    }
}

/// Minimal percent-encoding for a gitlab project path (`/` → `%2F`).
fn urlencode(s: &str) -> String {
    s.replace('/', "%2F")
}

// ---- pure parsers (fixture-tested) ----

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

pub fn parse_github(body: &str) -> Result<String> {
    let rel: GithubRelease =
        serde_json::from_str(body).context("parsing GitHub releases/latest JSON")?;
    Ok(rel.tag_name)
}

#[derive(Deserialize)]
struct GithubReleaseAssets {
    #[serde(default)]
    assets: Vec<GithubAsset>,
}
#[derive(Deserialize)]
struct GithubAsset {
    name: String,
}

/// Parse the `assets[].name` list from a GitHub `releases/latest` payload.
pub fn parse_github_asset_names(body: &str) -> Result<Vec<String>> {
    let rel: GithubReleaseAssets =
        serde_json::from_str(body).context("parsing GitHub release assets JSON")?;
    Ok(rel.assets.into_iter().map(|a| a.name).collect())
}

/// Fetch the asset names of a GitHub repo's latest release (best-effort input to
/// the aqua matching-pruning simulation). `locator` is `owner/repo`.
pub fn github_release_asset_names(http: &dyn HttpClient, locator: &str) -> Result<Vec<String>> {
    let url = format!("https://api.github.com/repos/{locator}/releases/latest");
    let body = http.get_text(&url)?;
    parse_github_asset_names(&body)
}

#[derive(Deserialize)]
struct GitlabRelease {
    tag_name: String,
}

/// GitLab `/releases` returns an array ordered newest-first.
pub fn parse_gitlab(body: &str) -> Result<String> {
    let rels: Vec<GitlabRelease> =
        serde_json::from_str(body).context("parsing GitLab releases JSON")?;
    rels.into_iter()
        .next()
        .map(|r| r.tag_name)
        .context("GitLab project has no releases")
}

#[derive(Deserialize)]
struct PypiJson {
    info: PypiInfo,
}
#[derive(Deserialize)]
struct PypiInfo {
    version: String,
}

pub fn parse_pypi(body: &str) -> Result<String> {
    let j: PypiJson = serde_json::from_str(body).context("parsing PyPI JSON")?;
    Ok(j.info.version)
}

#[derive(Deserialize)]
struct NpmLatest {
    version: String,
}

pub fn parse_npm(body: &str) -> Result<String> {
    let j: NpmLatest = serde_json::from_str(body).context("parsing npm latest JSON")?;
    Ok(j.version)
}

#[derive(Deserialize)]
struct CargoJson {
    #[serde(rename = "crate")]
    krate: CargoCrate,
}
#[derive(Deserialize)]
struct CargoCrate {
    max_stable_version: Option<String>,
    max_version: Option<String>,
}

pub fn parse_cargo(body: &str) -> Result<String> {
    let j: CargoJson = serde_json::from_str(body).context("parsing crates.io JSON")?;
    j.krate
        .max_stable_version
        .or(j.krate.max_version)
        .context("crates.io response has no version")
}

#[derive(Deserialize)]
struct GoLatest {
    #[serde(rename = "Version")]
    version: String,
}

pub fn parse_go(body: &str) -> Result<String> {
    let j: GoLatest = serde_json::from_str(body).context("parsing go proxy @latest JSON")?;
    Ok(j.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttp;

    #[test]
    fn github_json() {
        assert_eq!(
            parse_github(r#"{"tag_name":"v0.18.21","name":"eza"}"#).unwrap(),
            "v0.18.21"
        );
    }

    #[test]
    fn gitlab_array_newest_first() {
        let body = r#"[{"tag_name":"v2.0.0"},{"tag_name":"v1.0.0"}]"#;
        assert_eq!(parse_gitlab(body).unwrap(), "v2.0.0");
    }

    #[test]
    fn gitlab_empty_errors() {
        assert!(parse_gitlab("[]").is_err());
    }

    #[test]
    fn pypi_json() {
        let body = r#"{"info":{"version":"0.6.9","name":"ruff"},"releases":{}}"#;
        assert_eq!(parse_pypi(body).unwrap(), "0.6.9");
    }

    #[test]
    fn npm_json() {
        assert_eq!(parse_npm(r#"{"version":"9.1.0","name":"pnpm"}"#).unwrap(), "9.1.0");
    }

    #[test]
    fn cargo_prefers_max_stable() {
        let body = r#"{"crate":{"max_stable_version":"1.2.3","max_version":"1.3.0-beta"}}"#;
        assert_eq!(parse_cargo(body).unwrap(), "1.2.3");
    }

    #[test]
    fn cargo_falls_back_to_max_version() {
        let body = r#"{"crate":{"max_version":"0.9.0"}}"#;
        assert_eq!(parse_cargo(body).unwrap(), "0.9.0");
    }

    #[test]
    fn go_json() {
        let body = r#"{"Version":"v1.4.0","Time":"2026-01-01T00:00:00Z"}"#;
        assert_eq!(parse_go(body).unwrap(), "v1.4.0");
    }

    #[test]
    fn url_is_not_applicable() {
        let http = MockHttp::new();
        let spec = ParsedSpec {
            source: SourceKind::Url,
            locator: "https://x/y.tar.gz".into(),
        };
        assert_eq!(latest_version(&http, &spec, None).unwrap(), Latest::NotApplicable);
    }

    #[test]
    fn latest_version_dispatches_url_and_parses() {
        let http = MockHttp::new().with_text(
            "https://api.github.com/repos/eza-community/eza/releases/latest",
            r#"{"tag_name":"v0.20.0"}"#,
        );
        let spec = ParsedSpec {
            source: SourceKind::Github,
            locator: "eza-community/eza".into(),
        };
        assert_eq!(
            latest_version(&http, &spec, None).unwrap(),
            Latest::Version("v0.20.0".into())
        );
    }

    #[test]
    fn gitlab_encodes_project_path_and_uses_host() {
        let http = MockHttp::new().with_text(
            "https://gitlab.fish/api/v4/projects/group%2Fsub%2Frepo/releases",
            r#"[{"tag_name":"v3.1.0"}]"#,
        );
        let spec = ParsedSpec {
            source: SourceKind::Gitlab,
            locator: "group/sub/repo".into(),
        };
        assert_eq!(
            latest_version(&http, &spec, Some("https://gitlab.fish")).unwrap(),
            Latest::Version("v3.1.0".into())
        );
    }

    #[test]
    fn pkg_name_strips_constraints() {
        assert_eq!(pkg_name("ruff==0.6"), "ruff");
        assert_eq!(pkg_name("ripgrep"), "ripgrep");
    }

    #[test]
    fn npm_pkg_name_preserves_scope() {
        assert_eq!(npm_pkg_name("pnpm"), "pnpm");
        assert_eq!(npm_pkg_name("pnpm@9.1.0"), "pnpm");
        assert_eq!(npm_pkg_name("@babel/core"), "@babel/core");
        assert_eq!(npm_pkg_name("@babel/core@7.24.0"), "@babel/core");
    }

    #[test]
    fn npm_scoped_package_queries_correct_url() {
        let http = MockHttp::new().with_text(
            "https://registry.npmjs.org/@babel/core/latest",
            r#"{"version":"7.24.0","name":"@babel/core"}"#,
        );
        let spec = ParsedSpec {
            source: SourceKind::Npm,
            locator: "@babel/core".into(),
        };
        assert_eq!(
            latest_version(&http, &spec, None).unwrap(),
            Latest::Version("7.24.0".into())
        );
    }
}
