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
///
/// `path` is the aqua package NAME, not just the repo: plain packages are filed
/// under `owner/repo`, but a repo that ships several tools files each one under
/// its full name (`kubernetes/kubernetes/kubectl`, `microsoft/vscode/code`) and
/// has no `pkgs/owner/repo/registry.yaml` at all.
pub fn pkg_url(path: &str) -> String {
    format!("{RAW_BASE}/pkgs/{path}/registry.yaml")
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

/// Fetch and parse a single aqua package's registry.yaml, addressed by its
/// package `path` (see [`pkg_url`]).
///
/// The document may contain multiple `packages`; we return the one whose `name`
/// is `path`, else the one whose repo matches `path`'s first/last segment
/// (case-insensitive — plain packages leave `name` implicit), else the first
/// (aqua puts the primary package first).
pub fn fetch_package(http: &dyn HttpClient, path: &str) -> Result<Package> {
    let url = pkg_url(path);
    let body = http
        .get_text(&url)
        .with_context(|| format!("fetching aqua registry for {path}"))?;
    let reg: Registry = serde_yml::from_str(&body)
        .with_context(|| format!("parsing aqua registry.yaml for {path}"))?;
    if reg.packages.is_empty() {
        bail!("aqua registry for {path} has no packages");
    }
    let lower = |s: &str| s.to_ascii_lowercase();
    let segments: Vec<&str> = path.split('/').collect();
    let owner = lower(segments.first().copied().unwrap_or_default());
    let repo = lower(segments.last().copied().unwrap_or_default());
    let by_name = reg
        .packages
        .iter()
        .find(|p| p.name.as_deref().map(lower) == Some(lower(path)));
    let by_repo = || {
        reg.packages.iter().find(|p| {
            p.repo_owner.as_deref().map(lower) == Some(owner.clone())
                && p.repo_name.as_deref().map(lower) == Some(repo.clone())
        })
    };
    Ok(by_name
        .or_else(by_repo)
        .unwrap_or(&reg.packages[0])
        .clone())
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

/// A package discovered in the aqua root index.
///
/// The root index inlines every package definition, so it doubles as a
/// name → (source, repo) map across ecosystems — that's what bare-name
/// discovery ([`crate::discover`]) scores. Only the fields needed for
/// discovery/search are modeled; everything else is skipped by the scanner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Candidate {
    pub owner: String,
    pub repo: String,
    /// aqua's explicit `name:`, when the package is not simply `owner/repo`
    /// (nested binaries like `microsoft/vscode/code`, or a cross-ecosystem
    /// entry like `crates.io/bat`).
    pub name: Option<String>,
    /// aqua `type:`. Absent in the registry means `github_release` (aqua's
    /// default), and we normalize it to that so callers can match on one value.
    pub kind: String,
    /// Top-level `files[].name` — the real command names this package installs
    /// (`cli/cli` → `gh`). Absent in the registry means "same as `repo`".
    pub exes: Vec<String>,
    /// Ecosystem locator carried by non-release types: `crate:` (`type: cargo`)
    /// or `path:` (`type: go_install`).
    pub locator: Option<String>,
    pub description: Option<String>,
    /// `aliases[].name` — former package names, kept so a rename still resolves.
    pub aliases: Vec<String>,
    /// `files[].name` from the ONE `version_overrides` branch that
    /// [`crate::aqua::resolve::select_branch`] would take — the first branch
    /// constrained `"true"`, else the last one listed (see
    /// [`parse_index`]). Packages like `sharkdp/bat` and `docker/cli/rootless`
    /// declare their commands nowhere else.
    ///
    /// Kept apart from [`Self::exes`] because a branch is version-scoped: the
    /// names hold for the version ubix resolves today, not for the package in
    /// general. Evidence only — never matched against a query (see
    /// `docs/KNOWN_LIMITATIONS.md`).
    pub override_exes: Vec<String>,
}

/// How sure we are about the commands a package installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    /// Declared at the package level: this is what lands on PATH.
    Declared,
    /// Declared only inside the version/platform override branch we would
    /// install from, so the set is scoped to that version range.
    PerVersion,
    /// Nothing declared anywhere: aqua installs the repo-named command (or the
    /// last segment of a nested package name).
    Implied,
}

impl Candidate {
    /// The aqua package path — how the package is addressed under `pkgs/` and in
    /// an `aqua:` spec. That is the explicit `name` when there is one (a repo
    /// shipping several tools has no `pkgs/owner/repo/registry.yaml`), else
    /// `owner/repo`.
    pub fn pkg_path(&self) -> String {
        match self.name.as_deref() {
            Some(n) if n.contains('/') => n.to_string(),
            _ => format!("{}/{}", self.owner, self.repo),
        }
    }

    /// The commands this package installs, and how sure we are.
    ///
    /// Best evidence first: package-level `files[]`, else the selected override
    /// branch's (see [`Self::override_exes`]), else the aqua default (the last
    /// segment of a nested package name, since aqua names
    /// `kubernetes/kubernetes/kubectl` after the command it produces, else the
    /// repo name).
    pub fn commands(&self) -> (Certainty, Vec<&str>) {
        fn names(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        if !self.exes.is_empty() {
            return (Certainty::Declared, names(&self.exes));
        }
        if !self.override_exes.is_empty() {
            return (Certainty::PerVersion, names(&self.override_exes));
        }
        let implied = match self.name.as_deref() {
            Some(n) if n.contains('/') => n.rsplit('/').next().unwrap_or(n),
            _ => self.repo.as_str(),
        };
        (Certainty::Implied, vec![implied])
    }

    /// Just the command names from [`Self::commands`], for callers that only
    /// display them.
    pub fn command_names(&self) -> Vec<&str> {
        self.commands().1
    }
}

/// Which indent-4 block the scanner is currently inside.
#[derive(PartialEq, Eq)]
enum Block {
    None,
    Files,
    Aliases,
    /// A `description:` block scalar (`|`, `|-`, `>-`, …), whose text lives on
    /// the following, more-indented lines.
    Description,
}

/// One `version_overrides` branch's declared commands, as the scanner walks it.
#[derive(Default)]
struct BranchBuf {
    /// `version_constraint: "true"` — the branch [`super::resolve::select_branch`]
    /// takes outright.
    is_true: bool,
    names: Vec<String>,
}

/// Parse the root index into one [`Candidate`] per package.
///
/// This is a block-aware line scan rather than a `serde_yml` parse: the document
/// is ~3 MB / ~100k lines and models far more than we need, and a scan over it
/// costs a few milliseconds (so there is no second index to build and keep
/// fresh). Structure relied on: a package starts at indent 2 (`  - key: v`),
/// its own fields sit at indent 4, and `files:`/`aliases:` entries at indent 6.
/// Anchoring on exact indents is what keeps a `files:` nested under
/// `version_overrides:` (indent 8) from leaking in as a top-level exe.
///
/// A `files:` deeper than that is collected per BRANCH, not merged, because
/// branches are mutually exclusive: `BurntSushi/ripgrep` installs `xrep` below
/// 0.0.10 and `rg` after, and `knative/func` installs `func` or `faas` depending
/// on the version. [`select_branch`](super::resolve::select_branch) picks the
/// first branch constrained `"true"`, else the first whose constraint holds, so
/// the scanner keeps the `"true"` branch when there is one and otherwise the last
/// branch listed — the closest proxy for "newest" without evaluating aqua's
/// constraint expressions.
///
/// Packages we could not act on are dropped (see [`actionable`]) — a candidate
/// with no installable spec would only be noise.
pub fn parse_index(text: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: std::collections::HashSet<Candidate> = std::collections::HashSet::new();
    let mut cur: Option<Candidate> = None;
    let mut block = Block::None;
    // Indent of a `files:` key nested inside an override body, while we are
    // collecting its entries.
    let mut deep_files: Option<usize> = None;
    // Are we inside this package's `version_overrides:` list?
    let mut in_branches = false;
    // The branch being scanned, and the best one seen so far in this package.
    let mut branch: Option<BranchBuf> = None;
    let mut chosen: Option<BranchBuf> = None;
    // `files:` found under a base-level platform `overrides:` — used only when the
    // package has no version branches at all.
    let mut base_deep: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start().trim_end_matches('\r');
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        // A description block scalar owns every following deeper-indented line,
        // whatever that indent is (an explicit indicator like `|4-` pushes the
        // text further right than the usual +2).
        if block == Block::Description && indent >= 6 {
            if let Some(c) = cur.as_mut() {
                let d = c.description.get_or_insert_with(String::new);
                if !d.is_empty() {
                    d.push(' ');
                }
                d.push_str(t);
            }
            continue;
        }
        match indent {
            2 if t.starts_with("- ") => {
                finish_branches(cur.as_mut(), &mut branch, &mut chosen, &mut base_deep);
                push_candidate(&mut out, &mut seen, cur.take());
                let mut c = Candidate::default();
                absorb(&mut c, &t[2..]); // the first field rides the dash line
                cur = Some(c);
                block = Block::None;
                deep_files = None;
                in_branches = false;
            }
            4 => {
                deep_files = None;
                // Any indent-4 key ends the branch list we were walking.
                commit_branch(&mut chosen, branch.take());
                in_branches = t == "version_overrides:";
                block = match t {
                    "files:" => Block::Files,
                    "aliases:" => Block::Aliases,
                    _ if is_block_description(t) => Block::Description,
                    _ => Block::None,
                };
                if let Some(c) = cur.as_mut() {
                    if block == Block::Description {
                        // The `|`/`>` marker is not the text; the following lines are.
                        c.description = Some(String::new());
                    } else {
                        absorb(c, t);
                    }
                }
            }
            6 => {
                // An override list item starts here, ending any `files:` block
                // we were mining inside the previous one.
                deep_files = None;
                if in_branches && t.starts_with("- ") {
                    commit_branch(&mut chosen, branch.take());
                    branch = Some(BranchBuf {
                        // The constraint may ride the dash line.
                        is_true: field(&t[2..], "version_constraint:") == Some("true"),
                        names: Vec::new(),
                    });
                }
                let Some(c) = cur.as_mut() else { continue };
                if block == Block::Files || block == Block::Aliases {
                    let entry = t.strip_prefix("- ").unwrap_or(t);
                    if let Some(v) = field(entry, "name:") {
                        if block == Block::Files {
                            c.exes.push(v.to_string());
                        } else {
                            c.aliases.push(v.to_string());
                        }
                    }
                }
            }
            // Indent 8+ is an override body (`version_overrides[].overrides[]…`).
            // Its `files:` are not the package's own commands, but they ARE the
            // commands of the branch we may install from — see
            // `Candidate::override_exes`.
            _ => {
                if field(t, "version_constraint:") == Some("true") {
                    if let Some(b) = branch.as_mut() {
                        b.is_true = true;
                    }
                } else if t == "files:" {
                    deep_files = Some(indent);
                } else if let Some(fi) = deep_files {
                    let entry = t.strip_prefix("- ").unwrap_or(t);
                    match (indent == fi + 2).then(|| field(entry, "name:")).flatten() {
                        Some(v) => {
                            let names = match branch.as_mut() {
                                Some(b) => &mut b.names,
                                // Outside a version branch: a platform override
                                // on the base package.
                                None => &mut base_deep,
                            };
                            if !names.iter().any(|e| e == v) {
                                names.push(v.to_string());
                            }
                        }
                        None if indent <= fi => deep_files = None,
                        None => {}
                    }
                }
            }
        }
    }
    finish_branches(cur.as_mut(), &mut branch, &mut chosen, &mut base_deep);
    push_candidate(&mut out, &mut seen, cur.take());
    out
}

/// Record a finished `version_overrides` branch as the one we would install from,
/// mirroring [`select_branch`](super::resolve::select_branch): a `"true"` branch
/// wins outright, otherwise the last branch listed stands in for "newest".
/// Branches that declare no `files:` say nothing about the commands.
fn commit_branch(chosen: &mut Option<BranchBuf>, branch: Option<BranchBuf>) {
    let Some(b) = branch else { return };
    if b.names.is_empty() || chosen.as_ref().is_some_and(|c| c.is_true) {
        return;
    }
    *chosen = Some(b);
}

/// Close out a package's branch scan, moving the selected branch's commands onto
/// the candidate and resetting the per-package state.
fn finish_branches(
    cur: Option<&mut Candidate>,
    branch: &mut Option<BranchBuf>,
    chosen: &mut Option<BranchBuf>,
    base_deep: &mut Vec<String>,
) {
    commit_branch(chosen, branch.take());
    if let Some(c) = cur {
        c.override_exes = match chosen.take() {
            Some(b) => b.names,
            // No version branches — a platform override on the base package is
            // still better evidence than the package name.
            None => std::mem::take(base_deep),
        };
    }
    *chosen = None;
    base_deep.clear();
}

/// Whether a `description:` line opens a YAML block scalar (`|`, `|-`, `>2`, …)
/// instead of carrying its text inline.
///
/// Judged on the RAW value: a block scalar is never quoted, so a plain
/// description that happens to start with `|` (`description: "| piped"`) must not
/// be mistaken for one.
fn is_block_description(line: &str) -> bool {
    raw_field(line, "description:").is_some_and(|v| v.starts_with('|') || v.starts_with('>'))
}

/// Finish a scanned package: normalize the default `type`, drop the unusable
/// ones, and dedupe (the index lists a few packages twice under different
/// names). Dedupe is a `HashSet` rather than a linear scan so the 2277-package
/// index stays O(n).
fn push_candidate(
    out: &mut Vec<Candidate>,
    seen: &mut std::collections::HashSet<Candidate>,
    cand: Option<Candidate>,
) {
    let Some(mut c) = cand else { return };
    if c.kind.is_empty() {
        c.kind = "github_release".to_string();
    }
    if !actionable(&c) {
        return;
    }
    if seen.insert(c.clone()) {
        out.push(c);
    }
}

/// Whether a scanned package can yield an installable spec.
///
/// Release/http packages are addressed by their repo (aqua synthesis reads
/// `repo_owner`/`repo_name` and bails without them), but ecosystem packages
/// carry a self-contained locator: `crates.io/…` and `_go/…` entries may omit
/// the GitHub repo entirely, and `cargo:<crate>` / `go:<path>` is already a
/// complete spec.
fn actionable(c: &Candidate) -> bool {
    if !c.owner.is_empty() && !c.repo.is_empty() {
        return true;
    }
    matches!(c.kind.as_str(), "cargo" | "go_install" | "go_build") && c.locator.is_some()
}

/// Apply one `key: value` line to the package being scanned.
fn absorb(c: &mut Candidate, line: &str) {
    if let Some(v) = field(line, "repo_owner:") {
        c.owner = v.to_string();
    } else if let Some(v) = field(line, "repo_name:") {
        c.repo = v.to_string();
    } else if let Some(v) = field(line, "type:") {
        c.kind = v.to_string();
    } else if let Some(v) = field(line, "name:") {
        c.name = Some(v.to_string());
    } else if let Some(v) = field(line, "description:") {
        c.description = Some(v.to_string());
    } else if let Some(v) = field(line, "crate:").or_else(|| field(line, "path:")) {
        c.locator = Some(v.to_string());
    }
}

/// Parse the root index `text` and return candidates matching `query`
/// (case-insensitive substring). Deduped, order-preserving.
///
/// Matching is tiered so a short query doesn't drown in prose: repo / package
/// name / command name / alias hits win, and description hits are returned only
/// when nothing matched by name at all.
pub fn search_index(text: &str, query: &str) -> Vec<Candidate> {
    let q = query.to_ascii_lowercase();
    let all = parse_index(text);
    let by_name: Vec<Candidate> = all.iter().filter(|c| matches_name(c, &q)).cloned().collect();
    if !by_name.is_empty() {
        return by_name;
    }
    all.into_iter()
        .filter(|c| {
            c.description
                .as_deref()
                .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
        })
        .collect()
}

/// Whether any name-ish field of `c` contains the (already lowercased) `q`.
fn matches_name(c: &Candidate, q: &str) -> bool {
    let mut fields: Vec<&str> = vec![c.repo.as_str()];
    fields.extend(c.name.as_deref());
    fields.extend(c.exes.iter().map(String::as_str));
    fields.extend(c.aliases.iter().map(String::as_str));
    fields
        .iter()
        .any(|f| f.to_ascii_lowercase().contains(q))
}

/// Extract the value of `key` from a trimmed YAML line (`key: value`), stripping
/// surrounding quotes/whitespace. Returns `None` if the line isn't that key.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    Some(raw_field(line, key)?.trim_matches('"').trim_matches('\''))
}

/// Like [`field`] but keeps the quotes, for callers that must tell a quoted
/// string from YAML syntax.
fn raw_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    Some(line.strip_prefix(key)?.trim())
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
            pkg_url("openai/codex"),
            "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/pkgs/openai/codex/registry.yaml"
        );
        assert_eq!(
            root_url(),
            "https://raw.githubusercontent.com/aquaproj/aqua-registry/main/registry.yaml"
        );
    }

    #[test]
    fn fetch_package_uses_http_seam() {
        let http = MockHttp::new().with_text(&pkg_url("openai/codex"), CODEX);
        let p = fetch_package(&http, "openai/codex").unwrap();
        assert_eq!(p.repo_name.as_deref(), Some("codex"));
        assert_eq!(p.type_.as_deref(), Some("github_release"));
    }

    /// A multi-package document is selected by aqua package `name` first: the
    /// sibling packages of a nested tool all share one repo, so a repo match
    /// would return whichever came first.
    #[test]
    fn fetch_package_picks_the_named_package_from_a_multi_package_document() {
        let yaml = r#"
packages:
  - name: kubernetes/kubernetes/kubectl-convert
    type: http
    repo_owner: kubernetes
    repo_name: kubernetes
    url: https://dl.k8s.io/convert
  - name: kubernetes/kubernetes/kubectl
    type: http
    repo_owner: kubernetes
    repo_name: kubernetes
    url: https://dl.k8s.io/kubectl
"#;
        let path = "kubernetes/kubernetes/kubectl";
        let http = MockHttp::new().with_text(&pkg_url(path), yaml);
        let p = fetch_package(&http, path).unwrap();
        assert_eq!(p.name.as_deref(), Some(path));
        assert_eq!(p.url.as_deref(), Some("https://dl.k8s.io/kubectl"));
    }

    #[test]
    fn pkg_url_takes_the_full_package_path() {
        // aqua files nested packages under their whole name; `pkgs/kubernetes/
        // kubernetes/registry.yaml` does not exist.
        assert!(pkg_url("kubernetes/kubernetes/kubectl")
            .ends_with("/pkgs/kubernetes/kubernetes/kubectl/registry.yaml"));
    }

    #[test]
    fn candidate_path_and_commands_follow_a_nested_name() {
        let c = Candidate {
            owner: "kubernetes".into(),
            repo: "kubernetes".into(),
            name: Some("kubernetes/kubernetes/kubectl".into()),
            kind: "http".into(),
            ..Candidate::default()
        };
        assert_eq!(c.pkg_path(), "kubernetes/kubernetes/kubectl");
        assert_eq!(c.command_names(), vec!["kubectl"]);
        // A plain package keeps owner/repo, and a declared exe wins over both.
        let plain = Candidate {
            owner: "cli".into(),
            repo: "cli".into(),
            exes: vec!["gh".into()],
            ..Candidate::default()
        };
        assert_eq!(plain.pkg_path(), "cli/cli");
        assert_eq!(plain.command_names(), vec!["gh"]);
    }

    #[test]
    fn fetch_package_missing_errors() {
        let http = MockHttp::new();
        assert!(fetch_package(&http, "no/such").is_err());
    }

    /// A truncated root-index fixture exercising every shape the scanner cares
    /// about: a top-level `files:` (real command name), a `files:` nested under
    /// `version_overrides:` (must NOT leak), aliases, a cross-ecosystem `cargo`
    /// entry, and a package with no repo (must be dropped).
    const ROOT: &str = r#"
packages:
  - type: github_release
    repo_owner: cli
    repo_name: cli
    description: GitHub's official command line tool
    files:
      - name: gh
        src: gh_{{trimV .Version}}/bin/gh
    version_overrides:
      - version_constraint: semver("<= 0.4.0")
        files:
          - name: legacy-gh
  - type: github_release
    repo_owner: openai
    repo_name: codex
  - type: github_release
    repo_owner: sharkdp
    repo_name: fd
    aliases:
      - name: sharkdp/fd-find
  - name: crates.io/bat
    type: cargo
    repo_owner: sharkdp
    repo_name: bat
    description: A cat(1) clone with wings
    crate: bat
  - name: _go/sigsum.org/sigsum-go#cmd/sigsum-submit
    type: go_install
    description: |
      One of Sigsum command line tools.
      Creates and submits add-leaf requests
    path: sigsum.org/sigsum-go/cmd/sigsum-submit
    go_version_path: sigsum.org/sigsum-go
  - type: http
    name: no-repo/tool
    url: https://example.com/tool
"#;

    fn find<'a>(cands: &'a [Candidate], repo: &str, kind: &str) -> &'a Candidate {
        cands
            .iter()
            .find(|c| c.repo == repo && c.kind == kind)
            .unwrap_or_else(|| panic!("no {repo} ({kind}) candidate"))
    }

    #[test]
    fn parse_index_reads_fields_block_aware() {
        let cands = parse_index(ROOT);
        // The repo-less `http` package is dropped (aqua synthesis needs a repo);
        // the repo-less `go_install` one is kept (its `path:` is a whole spec).
        assert_eq!(cands.len(), 5, "{cands:#?}");

        let gh = find(&cands, "cli", "github_release");
        assert_eq!(gh.owner, "cli");
        // Only the TOP-LEVEL files entry — the version_overrides one is nested
        // deeper and must not be picked up.
        assert_eq!(gh.exes, vec!["gh"]);
        // …it is only recorded as per-version evidence, and a package-level
        // declaration outranks it.
        assert_eq!(gh.override_exes, vec!["legacy-gh"]);
        assert_eq!(gh.commands(), (Certainty::Declared, vec!["gh"]));
        assert_eq!(gh.description.as_deref(), Some("GitHub's official command line tool"));

        let fd = find(&cands, "fd", "github_release");
        assert_eq!(fd.aliases, vec!["sharkdp/fd-find"]);
        assert!(fd.exes.is_empty(), "no files: → exes stays empty");
        assert_eq!(fd.command_names(), vec!["fd"], "exe defaults to the repo name");

        // A cross-ecosystem entry keeps its aqua name, type and crate locator.
        let bat = find(&cands, "bat", "cargo");
        assert_eq!(bat.name.as_deref(), Some("crates.io/bat"));
        assert_eq!(bat.locator.as_deref(), Some("bat"));
    }

    /// A package may carry a locator and NO GitHub repo (`_go/…`, some
    /// `crates.io/…`). Dropping those for lack of a repo lost installable tools.
    #[test]
    fn parse_index_keeps_a_repoless_package_that_has_a_locator() {
        let cands = parse_index(ROOT);
        let go = cands
            .iter()
            .find(|c| c.kind == "go_install")
            .expect("repo-less go package kept");
        assert!(go.owner.is_empty() && go.repo.is_empty());
        assert_eq!(go.locator.as_deref(), Some("sigsum.org/sigsum-go/cmd/sigsum-submit"));
        // The command comes from the last segment of the nested aqua name.
        assert_eq!(go.command_names(), vec!["sigsum-submit"]);
        // …while a repo-less `http` package has no way to be installed at all.
        assert!(!cands.iter().any(|c| c.kind == "http"), "{cands:#?}");
    }

    /// `description: |` puts the text on the FOLLOWING lines; recording the `|`
    /// marker itself made those descriptions unsearchable.
    #[test]
    fn parse_index_folds_a_block_scalar_description() {
        let cands = parse_index(ROOT);
        let go = cands.iter().find(|c| c.kind == "go_install").unwrap();
        assert_eq!(
            go.description.as_deref(),
            Some("One of Sigsum command line tools. Creates and submits add-leaf requests")
        );
        // A word that exists only inside the folded block is findable.
        let hits = search_index(ROOT, "add-leaf");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "go_install");
        // The continuation lines must not be mistaken for fields of the package.
        assert!(go.exes.is_empty() && go.aliases.is_empty());
    }

    /// The shape of the real `docker/cli/rootless`: no package-level `files:`, so
    /// the only evidence of what it installs sits inside a `version_overrides`
    /// entry — and the aqua name's last segment ("rootless") is NOT a command.
    const OVERRIDE_ONLY: &str = r#"
packages:
  - type: github_release
    repo_owner: docker
    repo_name: cli
    name: docker/cli/rootless
    description: "| pipes are fine inside a quoted scalar"
    version_overrides:
      - version_constraint: "true"
        files:
          - name: rootlesskit
            src: docker-rootless-extras/rootlesskit
          - name: vpnkit
        overrides:
          - goos: darwin
            files:
              - name: vpnkit-darwin
"#;

    #[test]
    fn parse_index_mines_override_only_files_as_per_version_evidence() {
        let cands = parse_index(OVERRIDE_ONLY);
        assert_eq!(cands.len(), 1, "{cands:#?}");
        let c = &cands[0];
        assert!(c.exes.is_empty(), "nothing is declared package-wide");
        // Every nesting depth of `files:` inside the selected branch contributes,
        // in declaration order.
        assert_eq!(c.override_exes, vec!["rootlesskit", "vpnkit", "vpnkit-darwin"]);

        let (certainty, cmds) = c.commands();
        assert_eq!(certainty, Certainty::PerVersion);
        assert_eq!(cmds, vec!["rootlesskit", "vpnkit", "vpnkit-darwin"]);
        // …and the nested-name fallback ("rootless") is never reported as an exe.
        assert!(!cmds.contains(&"rootless"));
    }

    /// Branches are MUTUALLY EXCLUSIVE, so their `files:` must not be merged.
    /// `BurntSushi/ripgrep` shipped as `xrep` before 0.0.10 and `knative/func`
    /// installs `faas` from its `"true"` branch and `func` from a newer one:
    /// merging would claim both, and claiming a command the selected branch does
    /// not install is exactly what `Hit.provides` must not do.
    #[test]
    fn parse_index_keeps_only_the_branch_that_would_be_installed() {
        // A `"true"` branch wins outright, wherever it sits in the list — that is
        // what `resolve::select_branch` does.
        let func = parse_index(
            r#"
packages:
  - repo_owner: knative
    repo_name: func
    version_overrides:
      - version_constraint: semver(">= 0.9.0")
        files:
          - name: func
      - version_constraint: "true"
        files:
          - name: faas
"#,
        );
        assert_eq!(func[0].override_exes, vec!["faas"], "the `true` branch, not the union");

        // With no `"true"` branch, the last one listed stands in for "newest".
        let staged = parse_index(
            r#"
packages:
  - repo_owner: a
    repo_name: b
    version_overrides:
      - version_constraint: semver("<= 1.0.0")
        files:
          - name: old
      - version_constraint: semver("<= 2.0.0")
        files:
          - name: new
"#,
        );
        assert_eq!(staged[0].override_exes, vec!["new"]);

        // A branch that declares no files says nothing, so an earlier branch's
        // evidence survives instead of being blanked.
        let sparse = parse_index(
            r#"
packages:
  - repo_owner: a
    repo_name: b
    version_overrides:
      - version_constraint: semver("<= 1.0.0")
        files:
          - name: only
      - version_constraint: "true"
        supported_envs:
          - darwin
"#,
        );
        assert_eq!(sparse[0].override_exes, vec!["only"]);
    }

    /// `files:` under a base-level platform `overrides:` belongs to no branch, but
    /// it is still better evidence than the repo name.
    #[test]
    fn parse_index_reads_base_platform_override_files() {
        let cands = parse_index(
            "packages:\n  - repo_owner: a\n    repo_name: b\n    overrides:\n      - goos: windows\n        files:\n          - name: b.exe\n",
        );
        assert_eq!(cands[0].override_exes, vec!["b.exe"]);
    }

    /// A `description:` whose value merely STARTS with `|` (inside quotes) is an
    /// ordinary inline scalar, not a block scalar; folding it would swallow the
    /// fields that follow.
    #[test]
    fn parse_index_keeps_a_quoted_pipe_description_inline() {
        let c = &parse_index(OVERRIDE_ONLY)[0];
        assert_eq!(c.description.as_deref(), Some("| pipes are fine inside a quoted scalar"));
        assert_eq!(c.owner, "docker");
        assert_eq!(c.name.as_deref(), Some("docker/cli/rootless"));
    }

    /// Block scalars carry optional indentation/chomping indicators and their
    /// content may sit deeper than the usual 6 columns.
    #[test]
    fn parse_index_folds_indented_and_chomped_block_scalars() {
        let cands = parse_index(
            "packages:\n  - repo_owner: a\n    repo_name: b\n    description: |4-\n        deep block\n        second line\n    files:\n      - name: exe-b\n  - repo_owner: c\n    repo_name: d\n    description: >-\n      folded\n",
        );
        assert_eq!(cands.len(), 2, "{cands:#?}");
        assert_eq!(cands[0].description.as_deref(), Some("deep block second line"));
        // Folding stopped at the next 4-column field instead of eating it.
        assert_eq!(cands[0].exes, vec!["exe-b"]);
        // …and at the next package boundary.
        assert_eq!(cands[1].repo, "d");
        assert_eq!(cands[1].description.as_deref(), Some("folded"));
    }

    #[test]
    fn parse_index_defaults_missing_type_to_github_release() {
        let cands = parse_index("packages:\n  - repo_owner: a\n    repo_name: b\n");
        assert_eq!(cands[0].kind, "github_release");
    }

    #[test]
    fn search_index_substring_match() {
        let hits = search_index(ROOT, "cod");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, "codex");
        // Substring 'c' matches cli, codex, crates.io/bat and the `#cmd/` go
        // package (order preserved).
        let paths: Vec<String> = search_index(ROOT, "c").into_iter().map(|c| c.pkg_path()).collect();
        assert_eq!(
            paths,
            vec![
                "cli/cli",
                "openai/codex",
                "crates.io/bat",
                "_go/sigsum.org/sigsum-go#cmd/sigsum-submit"
            ]
        );
        // No match.
        assert!(search_index(ROOT, "zzz").is_empty());
    }

    #[test]
    fn search_index_matches_command_name_and_alias() {
        // `gh` lives only in files[].name; `fd-find` only in aliases. Neither is
        // findable by repo name, which was the old behavior.
        let hits = search_index(ROOT, "gh");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, "cli");
        let hits = search_index(ROOT, "fd-find");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, "fd");
    }

    #[test]
    fn search_index_falls_back_to_description_only_when_no_name_hit() {
        // "clone" appears only in bat's description.
        let hits = search_index(ROOT, "clone");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, "bat");
        // "cli" matches names, so the description hit for cli is not duplicated
        // and description-only candidates are excluded entirely.
        let repos: Vec<String> = search_index(ROOT, "cli").into_iter().map(|c| c.repo).collect();
        assert_eq!(repos, vec!["cli"]);
    }
}
