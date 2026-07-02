//! prefix.dev GraphQL client — the single API ubix uses for conda package
//! metadata (used by the `pixi` source). Two operations:
//!
//! * [`latest_version`] — newest version of a package in a channel (powers
//!   `outdated`/`upgrade` for `pixi:` tools).
//! * [`search`] — cross-channel package search over ALL prefix.dev channels
//!   (conda-forge, bioconda, robostack, third-party channels, …), powering
//!   `ubix search --pixi`.
//!
//! Everything goes through the [`HttpClient`] POST seam; query builders and
//! response parsers are pure functions unit-tested with fixtures (no network).

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::http::HttpClient;

/// The public prefix.dev GraphQL endpoint (reads need no auth).
pub const ENDPOINT: &str = "https://prefix.dev/api/graphql";

/// The default conda channel when a `pixi:` locator names none.
pub const DEFAULT_CHANNEL: &str = "conda-forge";

/// One package search hit (across channels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub name: String,
    pub channel: String,
    pub version: String,
}

/// A package's info card (for `ubix info <pixi:spec>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    /// Platforms (conda subdirs) the latest version publishes builds for.
    pub platforms: Vec<String>,
}

/// JSON-escape a string for embedding inside a GraphQL query literal.
fn json_str(s: &str) -> String {
    // serde_json gives us a correctly-quoted/escaped JSON string literal.
    serde_json::Value::String(s.to_string()).to_string()
}

/// Build the GraphQL request body for a single package's latest version.
pub fn latest_query(channel: &str, name: &str) -> String {
    let query = format!(
        "{{ package(channelName:{}, name:{}) {{ latestVersion {{ version }} }} }}",
        json_str(channel),
        json_str(name)
    );
    serde_json::json!({ "query": query }).to_string()
}

/// Build the GraphQL request body for a single package's full info card.
pub fn info_query(channel: &str, name: &str) -> String {
    let query = format!(
        "{{ package(channelName:{}, name:{}) {{ name summary description \
         latestVersion {{ version platforms }} }} }}",
        json_str(channel),
        json_str(name)
    );
    serde_json::json!({ "query": query }).to_string()
}

/// Build the GraphQL request body for a cross-channel package search.
pub fn search_query(query: &str, limit: usize) -> String {
    let gql = format!(
        "{{ packages(filters:{{name:{{contains:{}}}}}, limit:{}) \
         {{ page {{ name channel {{ name }} latestVersion {{ version }} }} }} }}",
        json_str(query),
        limit
    );
    serde_json::json!({ "query": gql }).to_string()
}

// ---- response shapes ----

#[derive(Deserialize)]
struct GqlResp<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
}

/// A single `package(...)` query result — shared by the `latest` and `info`
/// queries (each selects a subset; unrequested fields default). `platforms` is
/// only populated by the `info` query.
#[derive(Deserialize)]
struct PackageData {
    package: Option<PackageDetail>,
}
#[derive(Deserialize)]
struct PackageDetail {
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "latestVersion", default)]
    latest_version: Option<VersionNode>,
}
#[derive(Deserialize)]
struct VersionNode {
    version: String,
    #[serde(default)]
    platforms: Vec<String>,
}

#[derive(Deserialize)]
struct SearchData {
    packages: PackagePage,
}
#[derive(Deserialize)]
struct PackagePage {
    #[serde(default)]
    page: Vec<PackageNode>,
}
#[derive(Deserialize)]
struct PackageNode {
    name: String,
    channel: ChannelNode,
    #[serde(rename = "latestVersion")]
    latest_version: Option<VersionNode>,
}
#[derive(Deserialize)]
struct ChannelNode {
    name: String,
}

/// Surface any GraphQL `errors` as an `Err` (pure helper).
fn check_errors(errors: &[GqlError]) -> Result<()> {
    if let Some(first) = errors.first() {
        anyhow::bail!("prefix.dev GraphQL error: {}", first.message);
    }
    Ok(())
}

/// Parse a `latest_query` response → `Some(version)` or `None` (package absent).
pub fn parse_latest(body: &str) -> Result<Option<String>> {
    let resp: GqlResp<PackageData> =
        serde_json::from_str(body).context("parsing prefix.dev latest response")?;
    check_errors(&resp.errors)?;
    Ok(resp
        .data
        .and_then(|d| d.package)
        .and_then(|p| p.latest_version)
        .map(|v| v.version))
}

/// Parse a `search_query` response into hits. When `channel` is `Some`, keep only
/// that channel; otherwise return all. Hits are returned in server order
/// (conda-forge tends to rank first).
pub fn parse_search(body: &str, channel: Option<&str>) -> Result<Vec<Hit>> {
    let resp: GqlResp<SearchData> =
        serde_json::from_str(body).context("parsing prefix.dev search response")?;
    check_errors(&resp.errors)?;
    let Some(data) = resp.data else {
        return Ok(Vec::new());
    };
    let hits = data
        .packages
        .page
        .into_iter()
        .filter(|p| channel.is_none_or(|c| p.channel.name == c))
        .map(|p| Hit {
            name: p.name,
            channel: p.channel.name,
            version: p.latest_version.map(|v| v.version).unwrap_or_default(),
        })
        .collect();
    Ok(hits)
}

/// Parse an `info_query` response into a `PackageInfo`, or `None` if absent.
pub fn parse_info(body: &str) -> Result<Option<PackageInfo>> {
    let resp: GqlResp<PackageData> =
        serde_json::from_str(body).context("parsing prefix.dev info response")?;
    check_errors(&resp.errors)?;
    Ok(resp.data.and_then(|d| d.package).map(|p| {
        let (version, platforms) = match p.latest_version {
            Some(v) => (Some(v.version), v.platforms),
            None => (None, Vec::new()),
        };
        PackageInfo {
            name: p.name,
            summary: p.summary,
            description: p.description,
            version,
            platforms,
        }
    }))
}

// ---- network entry points ----

/// Newest version of `name` in `channel`, or `None` if the package is not found.
pub fn latest_version(http: &dyn HttpClient, channel: &str, name: &str) -> Result<Option<String>> {
    let body = http.post_json(ENDPOINT, &latest_query(channel, name))?;
    parse_latest(&body)
}

/// Fetch a package's info card from `channel`, or `None` if not found.
pub fn package_info(
    http: &dyn HttpClient,
    channel: &str,
    name: &str,
) -> Result<Option<PackageInfo>> {
    let body = http.post_json(ENDPOINT, &info_query(channel, name))?;
    parse_info(&body)
}

/// Search prefix.dev for packages whose name contains `query`, optionally scoped
/// to a single `channel`.
pub fn search(
    http: &dyn HttpClient,
    query: &str,
    channel: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let body = http.post_json(ENDPOINT, &search_query(query, limit))?;
    parse_search(&body, channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_query_escapes_args() {
        let b = latest_query("conda-forge", "ripgrep");
        assert!(b.contains("channelName:\\\"conda-forge\\\""), "{b}");
        assert!(b.contains("name:\\\"ripgrep\\\""), "{b}");
    }

    #[test]
    fn search_query_has_contains_and_limit() {
        let b = search_query("ripgrep", 20);
        assert!(b.contains("contains:\\\"ripgrep\\\""), "{b}");
        assert!(b.contains("limit:20"), "{b}");
    }

    #[test]
    fn parse_latest_ok() {
        let body = r#"{"data":{"package":{"name":"ripgrep","latestVersion":{"version":"15.1.0"}}}}"#;
        assert_eq!(parse_latest(body).unwrap(), Some("15.1.0".to_string()));
    }

    #[test]
    fn parse_latest_absent_package() {
        let body = r#"{"data":{"package":null}}"#;
        assert_eq!(parse_latest(body).unwrap(), None);
    }

    #[test]
    fn parse_latest_surfaces_errors() {
        let body = r#"{"data":null,"errors":[{"message":"boom"}]}"#;
        let err = parse_latest(body).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn parse_search_all_channels() {
        let body = r#"{"data":{"packages":{"page":[
            {"name":"ripgrep","channel":{"name":"conda-forge"},"latestVersion":{"version":"15.1.0"}},
            {"name":"ripgrep","channel":{"name":"bioconda"},"latestVersion":{"version":"13.0.0"}}
        ]}}}"#;
        let hits = parse_search(body, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit { name: "ripgrep".into(), channel: "conda-forge".into(), version: "15.1.0".into() });
    }

    #[test]
    fn parse_search_channel_filter() {
        let body = r#"{"data":{"packages":{"page":[
            {"name":"ripgrep","channel":{"name":"conda-forge"},"latestVersion":{"version":"15.1.0"}},
            {"name":"ripgrep","channel":{"name":"bioconda"},"latestVersion":{"version":"13.0.0"}}
        ]}}}"#;
        let hits = parse_search(body, Some("bioconda")).unwrap();
        assert_eq!(hits, vec![Hit { name: "ripgrep".into(), channel: "bioconda".into(), version: "13.0.0".into() }]);
    }

    #[test]
    fn parse_info_ok_with_null_description() {
        let body = r#"{"data":{"package":{"name":"vim","summary":"the editor","description":null,
            "latestVersion":{"version":"9.2","platforms":["linux-64","osx-arm64"]}}}}"#;
        let info = parse_info(body).unwrap().unwrap();
        assert_eq!(info.name, "vim");
        assert_eq!(info.summary.as_deref(), Some("the editor"));
        assert_eq!(info.description, None);
        assert_eq!(info.version.as_deref(), Some("9.2"));
        assert_eq!(info.platforms, vec!["linux-64", "osx-arm64"]);
    }

    #[test]
    fn parse_info_absent() {
        assert_eq!(parse_info(r#"{"data":{"package":null}}"#).unwrap(), None);
    }

    #[test]
    fn latest_version_via_mock() {
        use crate::http::MockHttp;
        let http = MockHttp::new().with_post(
            ENDPOINT,
            r#"{"data":{"package":{"latestVersion":{"version":"15.1.0"}}}}"#,
        );
        assert_eq!(
            latest_version(&http, "conda-forge", "ripgrep").unwrap(),
            Some("15.1.0".to_string())
        );
    }
}
