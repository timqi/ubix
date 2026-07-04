//! HTTP-fetch seam so network logic (outdated queries, url source, go.dev/dl,
//! checksum discovery) is unit-testable WITHOUT network. Mirror of the
//! `CommandRunner` seam: a real reqwest-backed impl plus a fixture mock.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};

/// Abstraction over HTTP GET. Only what ubix needs: fetch text (JSON APIs,
/// checksum files) and fetch bytes (downloads).
/// `Send + Sync` so `&dyn HttpClient` can be shared across scoped threads (the
/// combined `search` fans out aqua + pixi queries concurrently).
pub trait HttpClient: Send + Sync {
    /// GET `url` and return the body as a UTF-8 string. Non-2xx is an error.
    fn get_text(&self, url: &str) -> Result<String>;

    /// GET `url` and return the raw bytes. Non-2xx is an error.
    fn get_bytes(&self, url: &str) -> Result<Vec<u8>>;

    /// POST a JSON `body` to `url` (Content-Type: application/json) and return
    /// the response body as a UTF-8 string. Non-2xx is an error. Used for the
    /// prefix.dev GraphQL API (conda latest-version + package search).
    fn post_json(&self, url: &str, body: &str) -> Result<String>;
}

/// Production client backed by an async reqwest driven on a current-thread
/// runtime (same pattern the engine uses for ubi).
pub struct ReqwestClient {
    user_agent: String,
    /// `UBIX_GITHUB_TOKEN`, attached to github.com requests (raw/API) to lift the
    /// low anonymous rate limit that `ubix search` was hitting.
    github_token: Option<String>,
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self {
            user_agent: format!("ubix/{}", env!("CARGO_PKG_VERSION")),
            github_token: std::env::var("UBIX_GITHUB_TOKEN").ok().filter(|s| !s.is_empty()),
        }
    }

    /// Attach `Authorization: Bearer <token>` when a token is set AND `url`
    /// targets a GitHub host. Other hosts (e.g. prefix.dev, go.dev) are left
    /// untouched so we never leak the token off-site.
    fn authorize(&self, req: reqwest::RequestBuilder, url: &str) -> reqwest::RequestBuilder {
        match &self.github_token {
            Some(tok) if is_github_host(url) => req.bearer_auth(tok),
            _ => req,
        }
    }

    fn runtime() -> Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting async runtime for HTTP")
    }

    fn client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .user_agent(&self.user_agent)
            .build()
            .context("building HTTP client")
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Does `url` point at a GitHub host we should send the token to? Matches the
/// API, the web host, and any `*.githubusercontent.com` (raw content, codeload).
/// Host-based (not substring) so a lookalike path can't trick us into leaking.
fn is_github_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(h) => {
            h == "github.com"
                || h == "api.github.com"
                // Content hosts are always subdomains (raw/codeload/...); the bare
                // apex serves nothing, so we don't allowlist it.
                || h.ends_with(".githubusercontent.com")
        }
        None => false,
    }
}

/// Does `url` target a GitLab host (gitlab.com or a self-hosted `gitlab.*`)?
/// Best-effort by host label — a self-hosted GitLab on an unrelated domain
/// (e.g. `git.example.com`) won't be recognized.
fn is_gitlab_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(h) => h == "gitlab.com" || h.starts_with("gitlab.") || h.contains(".gitlab."),
        None => false,
    }
}

/// When `status` signals rate limiting (429, or GitHub's 403-with-limit) on a
/// token-gated host whose token env var is unset, return a one-line hint to set
/// it. Anonymous GitHub/GitLab requests throttle aggressively; a token lifts the
/// ceiling. Returns `None` (no nag) when the token is already set or the host
/// isn't token-gated.
fn rate_limit_hint(url: &str, status: reqwest::StatusCode) -> Option<String> {
    let set = |var: &str| !std::env::var(var).unwrap_or_default().is_empty();
    rate_limit_hint_for(url, status, set("UBIX_GITHUB_TOKEN"), set("UBIX_GITLAB_TOKEN"))
}

/// Pure core of [`rate_limit_hint`] — env reads are lifted out to `*_token_set`
/// so the branch logic is unit-testable without touching process env.
fn rate_limit_hint_for(
    url: &str,
    status: reqwest::StatusCode,
    github_token_set: bool,
    gitlab_token_set: bool,
) -> Option<String> {
    use reqwest::StatusCode;
    if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::FORBIDDEN {
        return None;
    }
    if is_github_host(url) && !github_token_set {
        Some("looks rate-limited — set UBIX_GITHUB_TOKEN (a GitHub PAT) to raise the limit".into())
    } else if is_gitlab_host(url) && !gitlab_token_set {
        Some("looks rate-limited — set UBIX_GITLAB_TOKEN to raise the limit".into())
    } else {
        None
    }
}

/// Build the error for a non-2xx GET, appending a rate-limit hint when relevant.
fn get_status_error(url: &str, status: reqwest::StatusCode) -> anyhow::Error {
    match rate_limit_hint(url, status) {
        Some(hint) => anyhow::anyhow!("GET {url} returned HTTP {status}; {hint}"),
        None => anyhow::anyhow!("GET {url} returned HTTP {status}"),
    }
}

impl HttpClient for ReqwestClient {
    fn get_text(&self, url: &str) -> Result<String> {
        let client = self.client()?;
        let rt = Self::runtime()?;
        let url = url.to_string();
        rt.block_on(async move {
            let resp = self
                .authorize(client.get(&url), &url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(get_status_error(&url, status));
            }
            resp.text()
                .await
                .with_context(|| format!("reading body of {url}"))
        })
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let client = self.client()?;
        let rt = Self::runtime()?;
        let url = url.to_string();
        rt.block_on(async move {
            let resp = self
                .authorize(client.get(&url), &url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(get_status_error(&url, status));
            }
            let bytes = resp
                .bytes()
                .await
                .with_context(|| format!("reading body of {url}"))?;
            Ok(bytes.to_vec())
        })
    }

    fn post_json(&self, url: &str, body: &str) -> Result<String> {
        let client = self.client()?;
        let rt = Self::runtime()?;
        let url = url.to_string();
        let body = body.to_string();
        rt.block_on(async move {
            let resp = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .with_context(|| format!("POST {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                bail!("POST {url} returned HTTP {status}");
            }
            resp.text()
                .await
                .with_context(|| format!("reading body of {url}"))
        })
    }
}

/// Fixture-based mock for unit tests. Maps exact URLs to canned bodies. Part of
/// the test seam; exercised only from tests.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct MockHttp {
    text: HashMap<String, String>,
    bytes: HashMap<String, Vec<u8>>,
    post: HashMap<String, String>,
}

#[allow(dead_code)]
impl MockHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(mut self, url: &str, body: &str) -> Self {
        self.text.insert(url.to_string(), body.to_string());
        self
    }

    pub fn with_bytes(mut self, url: &str, body: Vec<u8>) -> Self {
        self.bytes.insert(url.to_string(), body);
        self
    }

    /// Canned response for a `post_json` call to `url` (request body ignored).
    pub fn with_post(mut self, url: &str, body: &str) -> Self {
        self.post.insert(url.to_string(), body.to_string());
        self
    }
}

impl HttpClient for MockHttp {
    fn get_text(&self, url: &str) -> Result<String> {
        match self.text.get(url) {
            Some(b) => Ok(b.clone()),
            None => bail!("MockHttp: no canned text for `{url}`"),
        }
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        match self.bytes.get(url) {
            Some(b) => Ok(b.clone()),
            None => bail!("MockHttp: no canned bytes for `{url}`"),
        }
    }

    fn post_json(&self, url: &str, _body: &str) -> Result<String> {
        match self.post.get(url) {
            Some(b) => Ok(b.clone()),
            None => bail!("MockHttp: no canned POST response for `{url}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_hosts_are_recognized() {
        // Token-bearing hosts.
        assert!(is_github_host("https://raw.githubusercontent.com/a/b/main/x.yaml"));
        assert!(is_github_host("https://api.github.com/repos/a/b/releases"));
        assert!(is_github_host("https://github.com/a/b"));
        assert!(is_github_host("https://codeload.githubusercontent.com/a/b/tar.gz"));
        // Bare apex serves no content → not allowlisted.
        assert!(!is_github_host("https://githubusercontent.com/x"));
        // Off-site hosts must NOT get the token.
        assert!(!is_github_host("https://prefix.dev/api/graphql"));
        assert!(!is_github_host("https://go.dev/dl/go1.22.linux-amd64.tar.gz"));
        // Lookalike / path tricks are host-based, so they don't match.
        assert!(!is_github_host("https://evil.com/raw.githubusercontent.com/x"));
        assert!(!is_github_host("https://github.com.evil.com/x"));
        assert!(!is_github_host("not a url"));
    }

    #[test]
    fn rate_limit_hint_only_on_throttle_status_for_token_hosts() {
        use reqwest::StatusCode;
        let gh = "https://raw.githubusercontent.com/a/b/main/x.yaml";
        let gl = "https://gitlab.com/api/v4/projects/1/releases";
        let gl_self = "https://gitlab.fish/api/v4/projects/1/releases";

        // 429 / 403 on a github host with no token → hint UBIX_GITHUB_TOKEN.
        for st in [StatusCode::TOO_MANY_REQUESTS, StatusCode::FORBIDDEN] {
            let h = rate_limit_hint_for(gh, st, false, false).unwrap();
            assert!(h.contains("UBIX_GITHUB_TOKEN"), "{h}");
        }
        // gitlab.com and self-hosted gitlab.* → hint UBIX_GITLAB_TOKEN.
        for u in [gl, gl_self] {
            let h = rate_limit_hint_for(u, StatusCode::TOO_MANY_REQUESTS, false, false).unwrap();
            assert!(h.contains("UBIX_GITLAB_TOKEN"), "{u}: {h}");
        }
        // Token already set → no nag.
        assert!(rate_limit_hint_for(gh, StatusCode::FORBIDDEN, true, false).is_none());
        assert!(rate_limit_hint_for(gl, StatusCode::TOO_MANY_REQUESTS, false, true).is_none());
        // Non-throttle status → no hint even on a github host.
        assert!(rate_limit_hint_for(gh, StatusCode::NOT_FOUND, false, false).is_none());
        assert!(rate_limit_hint_for(gh, StatusCode::INTERNAL_SERVER_ERROR, false, false).is_none());
        // Non-token host (pypi) → never hinted.
        assert!(
            rate_limit_hint_for("https://pypi.org/pypi/ruff/json", StatusCode::TOO_MANY_REQUESTS, false, false)
                .is_none()
        );
    }

    #[test]
    fn mock_text_and_bytes() {
        let h = MockHttp::new()
            .with_text("https://x/api", "{\"v\":1}")
            .with_bytes("https://x/blob", vec![1, 2, 3]);
        assert_eq!(h.get_text("https://x/api").unwrap(), "{\"v\":1}");
        assert_eq!(h.get_bytes("https://x/blob").unwrap(), vec![1, 2, 3]);
        assert!(h.get_text("https://x/missing").is_err());
    }
}
