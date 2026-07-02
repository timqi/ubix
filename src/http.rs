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
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self {
            user_agent: format!("ubix/{}", env!("CARGO_PKG_VERSION")),
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

impl HttpClient for ReqwestClient {
    fn get_text(&self, url: &str) -> Result<String> {
        let client = self.client()?;
        let rt = Self::runtime()?;
        let url = url.to_string();
        rt.block_on(async move {
            let resp = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                bail!("GET {url} returned HTTP {status}");
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
            let resp = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                bail!("GET {url} returned HTTP {status}");
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
    fn mock_text_and_bytes() {
        let h = MockHttp::new()
            .with_text("https://x/api", "{\"v\":1}")
            .with_bytes("https://x/blob", vec![1, 2, 3]);
        assert_eq!(h.get_text("https://x/api").unwrap(), "{\"v\":1}");
        assert_eq!(h.get_bytes("https://x/blob").unwrap(), vec![1, 2, 3]);
        assert!(h.get_text("https://x/missing").is_err());
    }
}
