//! aqua-registry data source: per-package fetch + root-index cache/search
//! (plan §4). All network goes through the [`HttpClient`] seam.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

use crate::http::HttpClient;
use crate::paths;

use super::schema::{Package, Registry};

const RAW_BASE: &str = "https://raw.githubusercontent.com/aquaproj/aqua-registry/main";

/// Max age before the cached root index is re-fetched. Search reuses a cache
/// younger than this so repeated `ubix search` calls don't hammer (and get
/// rate-limited by) raw.githubusercontent.com.
const ROOT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The raw URL for a single package's registry.yaml.
pub fn pkg_url(owner: &str, repo: &str) -> String {
    format!("{RAW_BASE}/pkgs/{owner}/{repo}/registry.yaml")
}

/// The raw URL for the root (all-package) index.
pub fn root_url() -> String {
    format!("{RAW_BASE}/registry.yaml")
}

/// Cache path for the root index (`~/.cache/ubix/aqua-registry.yaml`, honoring
/// `$XDG_CACHE_HOME`).
pub fn root_cache_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => paths::home_dir().join(".cache"),
    };
    base.join("ubix").join("aqua-registry.yaml")
}

/// Fetch and parse a single package's registry.yaml. The document may contain
/// multiple `packages`; we return the one whose repo matches (case-insensitive),
/// else the first (aqua puts the primary package first).
pub fn fetch_package(http: &dyn HttpClient, owner: &str, repo: &str) -> Result<Package> {
    let url = pkg_url(owner, repo);
    let body = http
        .get_text(&url)
        .with_context(|| format!("fetching aqua registry for {owner}/{repo}"))?;
    let reg: Registry = serde_yml::from_str(&body)
        .with_context(|| format!("parsing aqua registry.yaml for {owner}/{repo}"))?;
    if reg.packages.is_empty() {
        bail!("aqua registry for {owner}/{repo} has no packages");
    }
    let chosen = reg
        .packages
        .iter()
        .find(|p| {
            p.repo_owner.as_deref().map(str::to_ascii_lowercase) == Some(owner.to_ascii_lowercase())
                && p.repo_name.as_deref().map(str::to_ascii_lowercase)
                    == Some(repo.to_ascii_lowercase())
        })
        .cloned()
        .unwrap_or_else(|| reg.packages[0].clone());
    Ok(chosen)
}

/// Refresh the root-index cache from upstream into `path`. Returns the bytes
/// written. `path` is threaded through (rather than re-deriving it) so it always
/// matches the location the caller reads back.
pub fn update(http: &dyn HttpClient, path: &Path) -> Result<usize> {
    let body = http
        .get_text(&root_url())
        .context("fetching aqua root registry index")?;
    paths::ensure_parent_dir(path)?;
    // Write to a temp sibling then rename, so a partial/failed write never
    // truncates the existing cache (which serves as the offline fallback).
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(body.len())
}

/// Whether a cache file's age is within `ttl` (fresh → reuse without fetching).
/// Missing file or an unreadable mtime → not fresh.
fn cache_fresh(path: &Path, ttl: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(mtime)
        .map(|age| age < ttl)
        .unwrap_or(false)
}

/// Return the aqua root-index text for searching. Reuses a cache younger than
/// [`ROOT_CACHE_TTL`] (skipping the network entirely) unless `force`. Otherwise
/// fetches upstream, falling back to any stale cache on failure; errors only
/// when there is no usable text at all.
pub fn root_index(http: &dyn HttpClient, force: bool) -> Result<String> {
    root_index_from(http, &root_cache_path(), ROOT_CACHE_TTL, force)
}

/// [`root_index`] with an explicit cache path + TTL (so tests can avoid the
/// env-derived cache path and time-sensitive network).
fn root_index_from(
    http: &dyn HttpClient,
    cache: &Path,
    ttl: Duration,
    force: bool,
) -> Result<String> {
    if !force && cache_fresh(cache, ttl) {
        if let Some(text) = read_root_cache(cache)? {
            crate::step!("using cached aqua root index (< 24h old)");
            return Ok(text);
        }
    }
    match update(http, cache) {
        Ok(n) => {
            crate::step!("refreshed aqua root index ({n} bytes)");
            read_root_cache(cache)?.context("root index cache missing after update")
        }
        Err(e) => match read_root_cache(cache)? {
            Some(text) => {
                crate::step!("aqua root index refresh failed ({e}); using cached index");
                Ok(text)
            }
            None => bail!("aqua root index unavailable (refresh failed: {e}, no cache)"),
        },
    }
}

/// Read the cached root index text, if present.
pub fn read_root_cache(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    ))
}

/// A search candidate discovered in the root index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub owner: String,
    pub repo: String,
}

/// Parse the root index `text` and return owner/repo candidates whose repo name
/// contains `query` (case-insensitive substring). Deduped, order-preserving.
///
/// The root index inlines every package; each carries `repo_owner:`/`repo_name:`
/// lines. We scan those line-pairs directly (robust to the huge document and to
/// fields we don't model). A `repo_owner` line is paired with the NEXT
/// `repo_name` line.
pub fn search_index(text: &str, query: &str) -> Vec<Candidate> {
    let q = query.to_ascii_lowercase();
    let mut out: Vec<Candidate> = Vec::new();
    let mut pending_owner: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = field(t, "repo_owner:") {
            pending_owner = Some(v.to_string());
        } else if let Some(repo) = field(t, "repo_name:") {
            if let Some(owner) = pending_owner.take() {
                if repo.to_ascii_lowercase().contains(&q) {
                    let cand = Candidate {
                        owner,
                        repo: repo.to_string(),
                    };
                    if !out.contains(&cand) {
                        out.push(cand);
                    }
                }
            }
        }
    }
    out
}

/// Extract the value of `key` from a trimmed YAML line (`key: value`), stripping
/// surrounding quotes/whitespace. Returns `None` if the line isn't that key.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    Some(rest.trim().trim_matches('"').trim_matches('\''))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpClient, MockHttp};

    const CODEX: &str = include_str!("../../tests/fixtures/aqua/openai_codex.yaml");

    /// An HttpClient that fails the test if any network method is called — proves
    /// the TTL path served from cache without touching the network.
    struct NoNet;
    impl HttpClient for NoNet {
        fn get_text(&self, url: &str) -> Result<String> {
            panic!("unexpected network fetch: {url}")
        }
        fn get_bytes(&self, _: &str) -> Result<Vec<u8>> {
            panic!("unexpected network fetch")
        }
        fn post_json(&self, _: &str, _: &str) -> Result<String> {
            panic!("unexpected network fetch")
        }
    }

    fn tmp_cache(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ubix-registry-test-{name}.yaml"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn cache_fresh_true_for_new_false_for_zero_ttl() {
        let path = tmp_cache("fresh");
        std::fs::write(&path, "packages: []\n").unwrap();
        assert!(cache_fresh(&path, Duration::from_secs(3600)), "just-written cache is fresh");
        assert!(!cache_fresh(&path, Duration::ZERO), "zero TTL is never fresh");
        assert!(!cache_fresh(Path::new("/no/such/file"), Duration::from_secs(3600)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn root_index_reuses_fresh_cache_without_fetch() {
        let path = tmp_cache("reuse");
        std::fs::write(&path, "packages:\n  - repo_owner: a\n    repo_name: b\n").unwrap();
        // Fresh cache + NoNet → must return the cached text, never fetch.
        let text = root_index_from(&NoNet, &path, Duration::from_secs(3600), false).unwrap();
        assert!(text.contains("repo_name: b"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn root_index_force_and_stale_fall_back_to_cache_on_fetch_error() {
        let path = tmp_cache("stale");
        std::fs::write(&path, "packages:\n  - repo_owner: a\n    repo_name: b\n").unwrap();
        // force=true bypasses the fresh cache → tries update (MockHttp has no
        // canned root_url → errors) → falls back to the existing cache text.
        let text = root_index_from(&MockHttp::new(), &path, Duration::from_secs(3600), true).unwrap();
        assert!(text.contains("repo_name: b"));
        // Zero TTL (stale) takes the same fetch→fallback path.
        let text = root_index_from(&MockHttp::new(), &path, Duration::ZERO, false).unwrap();
        assert!(text.contains("repo_name: b"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn root_index_fetches_and_writes_cache_when_stale() {
        let path = tmp_cache("fetch");
        // No existing cache + a canned root_url → update writes to `path`, then
        // root_index_from reads it back from the SAME path (regression: update
        // used to hardcode root_cache_path()).
        let http = MockHttp::new()
            .with_text(&root_url(), "packages:\n  - repo_owner: x\n    repo_name: y\n");
        let text = root_index_from(&http, &path, Duration::ZERO, true).unwrap();
        assert!(text.contains("repo_name: y"));
        assert!(path.exists(), "cache written to the passed path");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn root_index_errors_when_no_cache_and_fetch_fails() {
        let path = tmp_cache("missing"); // removed by tmp_cache
        let err = root_index_from(&MockHttp::new(), &path, Duration::ZERO, false).unwrap_err();
        assert!(err.to_string().contains("unavailable"), "{err}");
    }

    #[test]
    fn urls_are_raw_github() {
        assert_eq!(
            pkg_url("openai", "codex"),
            "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/pkgs/openai/codex/registry.yaml"
        );
        assert_eq!(
            root_url(),
            "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/registry.yaml"
        );
    }

    #[test]
    fn fetch_package_uses_http_seam() {
        let http = MockHttp::new().with_text(&pkg_url("openai", "codex"), CODEX);
        let p = fetch_package(&http, "openai", "codex").unwrap();
        assert_eq!(p.repo_name.as_deref(), Some("codex"));
        assert_eq!(p.type_.as_deref(), Some("github_release"));
    }

    #[test]
    fn fetch_package_missing_errors() {
        let http = MockHttp::new();
        assert!(fetch_package(&http, "no", "such").is_err());
    }

    #[test]
    fn search_index_substring_match() {
        // A truncated root-index fixture with a few inlined packages.
        let root = r#"
packages:
  - type: github_release
    repo_owner: cli
    repo_name: cli
  - type: github_release
    repo_owner: openai
    repo_name: codex
  - type: github_release
    repo_owner: sharkdp
    repo_name: fd
"#;
        let hits = search_index(root, "cod");
        assert_eq!(hits, vec![Candidate { owner: "openai".into(), repo: "codex".into() }]);
        // Substring 'c' matches cli and codex (order preserved).
        let hits = search_index(root, "c");
        assert_eq!(
            hits,
            vec![
                Candidate { owner: "cli".into(), repo: "cli".into() },
                Candidate { owner: "openai".into(), repo: "codex".into() },
            ]
        );
        // No match.
        assert!(search_index(root, "zzz").is_empty());
    }
}
