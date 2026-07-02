//! aqua-registry data source: per-package fetch + root-index cache/search
//! (plan §4). All network goes through the [`HttpClient`] seam.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::http::HttpClient;
use crate::paths;

use super::schema::{Package, Registry};

const RAW_BASE: &str = "https://raw.githubusercontent.com/aquaproj/aqua-registry/main";

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

/// Refresh the root-index cache from upstream. Returns the cache path and byte
/// size written.
pub fn update(http: &dyn HttpClient) -> Result<(PathBuf, usize)> {
    let body = http
        .get_text(&root_url())
        .context("fetching aqua root registry index")?;
    let path = root_cache_path();
    paths::ensure_parent_dir(&path)?;
    std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, body.len()))
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
    use crate::http::MockHttp;

    const CODEX: &str = include_str!("../../tests/fixtures/aqua/openai_codex.yaml");

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
