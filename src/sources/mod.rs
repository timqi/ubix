//! Source abstraction: spec parsing plus the per-source handlers.

pub mod cargo;
pub mod github;
pub mod gitlab;
pub mod go;
pub mod http;
pub mod npm;
pub mod url;
pub mod uv;

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Result};

use crate::config::ToolConfig;
use crate::runner::CommandRunner;
use crate::state::ToolRecord;

/// The recognized source kinds (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Github,
    Gitlab,
    Url,
    Http,
    Pypi,
    Npm,
    Cargo,
    Go,
}

impl SourceKind {
    /// The canonical prefix string as stored in `state.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Github => "github",
            SourceKind::Gitlab => "gitlab",
            SourceKind::Url => "url",
            SourceKind::Http => "http",
            SourceKind::Pypi => "pypi",
            SourceKind::Npm => "npm",
            SourceKind::Cargo => "cargo",
            SourceKind::Go => "go",
        }
    }

}

impl FromStr for SourceKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "github" => SourceKind::Github,
            "gitlab" => SourceKind::Gitlab,
            "url" => SourceKind::Url,
            "http" => SourceKind::Http,
            "pypi" => SourceKind::Pypi,
            "npm" => SourceKind::Npm,
            "cargo" => SourceKind::Cargo,
            "go" => SourceKind::Go,
            other => bail!("unknown source prefix `{other}:` (expected one of github/gitlab/url/http/pypi/npm/cargo/go)"),
        })
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed `spec = "$source:$locator"` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSpec {
    pub source: SourceKind,
    /// The locator portion (after the prefix), e.g. `owner/repo`, a URL, a package name.
    pub locator: String,
}

/// Parse a `spec` string with the rules of §4.2.
///
/// * `default_source` is applied *only* when the locator has no `prefix:`.
/// * A prefixless locator must be `owner/repo` (exactly two non-empty segments).
/// * A prefixless bare single word (e.g. `ruff`) is rejected.
pub fn parse_spec(spec: &str, default_source: SourceKind) -> Result<ParsedSpec> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty spec");
    }

    // Detect an explicit `prefix:` where prefix is one of the known kinds.
    // We must be careful: `url:https://...` contains further colons, and a
    // GitHub locator never contains a colon. Split on the first colon only.
    if let Some((maybe_prefix, rest)) = spec.split_once(':') {
        if let Ok(kind) = SourceKind::from_str(maybe_prefix) {
            let locator = rest.trim();
            if locator.is_empty() {
                bail!("spec `{spec}` has an empty locator after `{maybe_prefix}:`");
            }
            validate_locator(kind, locator)?;
            return Ok(ParsedSpec {
                source: kind,
                locator: locator.to_string(),
            });
        }
        // A colon but not a known prefix. If it *looks* like `word:...` treat as
        // an unknown prefix rather than silently defaulting.
        if !maybe_prefix.contains('/') {
            bail!(
                "unknown source prefix `{maybe_prefix}:` in spec `{spec}` \
                 (expected one of github/gitlab/url/pypi/npm/cargo/go)"
            );
        }
    }

    // No recognized prefix: apply default_source, which requires `owner/repo`.
    let segments: Vec<&str> = spec.split('/').filter(|s| !s.is_empty()).collect();
    if spec.split('/').count() != 2 || segments.len() != 2 {
        bail!(
            "bare locator `{spec}` is not `owner/repo`; \
             prefix it explicitly, e.g. `pypi:{spec}` or `cargo:{spec}`"
        );
    }
    validate_locator(default_source, spec)?;
    Ok(ParsedSpec {
        source: default_source,
        locator: spec.to_string(),
    })
}

fn validate_locator(kind: SourceKind, locator: &str) -> Result<()> {
    match kind {
        SourceKind::Github => {
            let segs: Vec<&str> = locator.split('/').filter(|s| !s.is_empty()).collect();
            if locator.split('/').count() != 2 || segs.len() != 2 {
                bail!("github locator `{locator}` must be `owner/repo`");
            }
        }
        SourceKind::Gitlab => {
            // group[/subgroup...]/repo — at least two segments.
            let segs: Vec<&str> = locator.split('/').filter(|s| !s.is_empty()).collect();
            if segs.len() < 2 {
                bail!("gitlab locator `{locator}` must be `group[/subgroup…]/repo`");
            }
        }
        SourceKind::Url => {
            if !(locator.starts_with("http://") || locator.starts_with("https://")) {
                bail!("url locator `{locator}` must be an http(s) URL");
            }
        }
        SourceKind::Http => {
            // locator is a URL TEMPLATE (may contain {version}/{os}/{arch}).
            if !(locator.starts_with("http://") || locator.starts_with("https://")) {
                bail!("http locator `{locator}` must be an http(s) URL template");
            }
        }
        SourceKind::Pypi | SourceKind::Npm | SourceKind::Cargo => {
            if locator.contains(char::is_whitespace) {
                bail!("{kind} package name `{locator}` must not contain whitespace");
            }
        }
        SourceKind::Go => {
            // module path, optionally `@version`.
            if !locator.contains('.') && !locator.contains('/') {
                bail!("go locator `{locator}` should be a module path (e.g. example.com/cmd/tool@latest)");
            }
        }
    }
    Ok(())
}

/// Outcome of an install/upgrade operation, used to update state.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub installed_version: String,
    pub resolved_asset: Option<String>,
    pub install_paths: Vec<std::path::PathBuf>,
    pub sha256: Option<String>,
}

/// Interface for release-family handlers driven by the engine (github). The
/// command/HTTP-based sources (uv/npm/cargo/go/url) expose free `install`
/// functions instead, since they need the `CommandRunner`/`HttpClient` seams
/// and per-source options rather than a uniform object.
pub trait Source {
    /// Resolve and install the tool, returning what was installed.
    fn install(&self, tool: &ToolConfig, runner: &dyn CommandRunner) -> Result<InstallOutcome>;
}

/// Unlink a record's tracked files directly (safe: they are ubix-tracked).
/// Used for file-based sources (github/gitlab/url/go) whose removal is just an
/// unlink. Tool-managed sources (uv/cargo/npm) are handled by the CLI via their
/// own uninstall commands.
pub fn unlink_tracked(record: &ToolRecord) -> Result<()> {
    for p in &record.install_paths {
        if p.exists() {
            std::fs::remove_file(p)
                .map_err(|e| anyhow::anyhow!("removing {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(spec: &str) -> ParsedSpec {
        parse_spec(spec, SourceKind::Github).unwrap()
    }

    #[test]
    fn all_prefixes() {
        assert_eq!(p("github:eza-community/eza").source, SourceKind::Github);
        assert_eq!(p("gitlab:group/repo").source, SourceKind::Gitlab);
        assert_eq!(
            p("url:https://example.com/x-linux-x86_64.tar.gz").source,
            SourceKind::Url
        );
        assert_eq!(p("http:https://h/{version}/bin").source, SourceKind::Http);
        assert_eq!(p("pypi:ruff").source, SourceKind::Pypi);
        assert_eq!(p("npm:pnpm").source, SourceKind::Npm);
        assert_eq!(p("cargo:somecli").source, SourceKind::Cargo);
        assert_eq!(p("go:example.com/cmd/tool@latest").source, SourceKind::Go);
    }

    #[test]
    fn url_locator_keeps_colons() {
        let parsed = p("url:https://example.com:8443/x.tar.gz");
        assert_eq!(parsed.source, SourceKind::Url);
        assert_eq!(parsed.locator, "https://example.com:8443/x.tar.gz");
    }

    #[test]
    fn http_prefix_holds_url_template() {
        let parsed = p("http:https://h/{version}/{os}-{arch}/claude");
        assert_eq!(parsed.source, SourceKind::Http);
        assert_eq!(parsed.locator, "https://h/{version}/{os}-{arch}/claude");
    }

    #[test]
    fn http_locator_requires_scheme() {
        assert!(parse_spec("http:h/{version}/claude", SourceKind::Github).is_err());
    }

    #[test]
    fn default_source_applies_to_owner_repo() {
        let parsed = parse_spec("eza-community/eza", SourceKind::Github).unwrap();
        assert_eq!(parsed.source, SourceKind::Github);
        assert_eq!(parsed.locator, "eza-community/eza");
    }

    #[test]
    fn default_source_respected() {
        // If default were somehow gitlab, a bare owner/repo would use gitlab.
        let parsed = parse_spec("group/repo", SourceKind::Gitlab).unwrap();
        assert_eq!(parsed.source, SourceKind::Gitlab);
    }

    #[test]
    fn bare_single_word_rejected() {
        let err = parse_spec("ruff", SourceKind::Github).unwrap_err();
        assert!(err.to_string().contains("bare locator"), "{err}");
    }

    #[test]
    fn three_segment_bare_rejected() {
        assert!(parse_spec("a/b/c", SourceKind::Github).is_err());
    }

    #[test]
    fn unknown_prefix_rejected() {
        let err = parse_spec("brew:wget", SourceKind::Github).unwrap_err();
        assert!(err.to_string().contains("unknown source prefix"), "{err}");
    }

    #[test]
    fn empty_locator_rejected() {
        assert!(parse_spec("github:", SourceKind::Github).is_err());
    }

    #[test]
    fn github_requires_two_segments() {
        assert!(parse_spec("github:justrepo", SourceKind::Github).is_err());
    }

    #[test]
    fn url_requires_scheme() {
        assert!(parse_spec("url:example.com/x.tar.gz", SourceKind::Github).is_err());
    }

    #[test]
    fn roundtrip_all_kinds_as_str() {
        for k in [
            SourceKind::Github,
            SourceKind::Gitlab,
            SourceKind::Url,
            SourceKind::Http,
            SourceKind::Pypi,
            SourceKind::Npm,
            SourceKind::Cargo,
            SourceKind::Go,
        ] {
            assert_eq!(k.as_str().parse::<SourceKind>().unwrap(), k);
        }
    }
}
