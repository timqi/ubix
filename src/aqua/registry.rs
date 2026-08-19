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
    /// The commands that OVERRIDE [`Self::exes`]: `files[].name` declared inside
    /// the `version_overrides` branch [`crate::aqua::resolve::select_branch`]
    /// would take, or — when the branches are not read — under a base-level
    /// platform `overrides:`. Packages like `sharkdp/bat` and
    /// `docker/cli/rootless` declare their commands nowhere else, and
    /// `cubefs/cubefs` declares one at the package level that its selected branch
    /// drops.
    ///
    /// Empty when the selected branch declares no `files:` (it inherits the
    /// package's) or when the branches disagree about what they install (see
    /// [`Branches::commands`]).
    ///
    /// Kept apart from [`Self::exes`] because the two are alternatives, not a
    /// union, and only this one describes a current install. Evidence only —
    /// never matched against a query (see `docs/KNOWN_LIMITATIONS.md`).
    pub override_exes: Vec<String>,
    /// The registry says this package installs NOTHING on this host: its
    /// `supported_envs` exclude it, or the entry ubix would install from is
    /// `no_asset`. `aqua:ahkohd/oyo` (`oy`) is darwin-only, so discovery still
    /// reports it for a linux `ubix add oy` — but as a dead end, never a pick.
    pub unavailable: bool,
}

/// How sure we are about the commands a package installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    /// Declared by the config that a current release installs from — the package
    /// entry, or the unconditional branch that replaces it.
    Declared,
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
    /// The selected override branch's `files[]` come FIRST, because a branch that
    /// declares them replaces the package-level list rather than adding to it
    /// (`resolve::merge_branch`) — `cubefs/cubefs` lists `cfs-preload` at the
    /// package level and drops it in the branch we install from. Then the
    /// package-level `files[]`, then the aqua default (the last segment of a
    /// nested package name, since aqua names `kubernetes/kubernetes/kubectl`
    /// after the command it produces, else the repo name).
    pub fn commands(&self) -> (Certainty, Vec<&str>) {
        fn names(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        if !self.override_exes.is_empty() {
            return (Certainty::Declared, names(&self.override_exes));
        }
        if !self.exes.is_empty() {
            return (Certainty::Declared, names(&self.exes));
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
    Aliases,
    /// A `description:` block scalar (`|`, `|-`, `>-`, …), whose text lives on
    /// the following, more-indented lines.
    Description,
}

/// Which list the scanner is currently reading entries into.
#[derive(PartialEq, Eq, Clone, Copy)]
enum List {
    /// `files:` — entries are `- name: <command>` mappings.
    Files,
    /// `supported_envs:` (a package or branch) or `envs:` (a platform override)
    /// — entries are plain scalars.
    Envs,
}

/// Whose list it is. A package's own and a branch's are kept apart because a
/// branch REPLACES what it declares rather than adding to it.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Scope {
    /// The package entry itself.
    Package,
    /// The `version_overrides` branch being scanned.
    Branch,
    /// The platform `overrides[]` item being scanned, in either of those.
    Override,
}

/// The list being read, and the indent of the key that opened it (entries sit
/// two columns deeper).
#[derive(PartialEq, Eq, Clone, Copy)]
struct Sink {
    list: List,
    scope: Scope,
    at: usize,
}

/// How a `files:` / `overrides:` key declares its list.
#[derive(PartialEq, Eq, Clone, Copy)]
enum ListDecl {
    /// The entries follow on deeper lines (`files:`, or `files: &anchor`).
    Open,
    /// An inline empty list (`overrides: []`) — declared, and deliberately empty.
    Empty,
}

/// Classify `line` as a declaration of `key`, or `None` when it is another key.
///
/// A YAML alias (`files: *anchor`) is deliberately NOT a declaration: a line scan
/// cannot resolve it, so the scope inherits rather than claim an empty list. The
/// registry only aliases inside version-constrained branches, which
/// [`PkgBuf::commands`] never reads.
fn list_decl(line: &str, key: &str) -> Option<ListDecl> {
    match raw_field(line, key)? {
        "" => Some(ListDecl::Open),
        "[]" => Some(ListDecl::Empty),
        v if v.starts_with('&') => Some(ListDecl::Open),
        _ => None,
    }
}

/// A platform `overrides[]` item — the fields that decide whether it applies to
/// THIS host and what it installs here.
#[derive(Default)]
struct OverrideBuf {
    goos: Option<String>,
    goarch: Option<String>,
    /// `files:` declared by this item; `None` when it declares none (the scope's
    /// own list is then inherited, exactly as in `effective_for`).
    files: Option<Vec<String>>,
    /// `envs:` — an extra platform filter on top of `goos`/`goarch`.
    envs: Option<Vec<String>>,
}

/// The `files:`, platform `overrides:` and `supported_envs:` of one scope — a
/// package base, or one of its `version_overrides` branches.
///
/// All three are `Option` because DECLARING one replaces what a branch would
/// inherit while declaring nothing inherits it, and they are independent
/// (`docker/hub-tool`'s `"true"` branch declares `overrides: []`, clearing the
/// base's while keeping its `files:`). aqua draws the same distinction with a nil
/// check in `overrideVersion`.
#[derive(Default)]
struct FileScope {
    files: Option<Vec<String>>,
    overrides: Option<Vec<OverrideBuf>>,
    envs: Option<Vec<String>>,
}

/// What a scope installs on one host: the commands, or [`None`] when the host
/// gets nothing at all (unsupported env, or `no_asset`).
type Outcome<'a> = Option<Option<&'a [String]>>;

impl FileScope {
    /// [`Self::effective_over`] for a scope that stands alone (the package base).
    fn effective(&self, os: &str, arch: &str) -> Outcome<'_> {
        self.effective_over(self, os, arch)
    }

    /// What this scope installs on `(os, arch)` when it is a branch layered onto
    /// the package `base`: each field it does not declare is inherited
    /// ([`merge_branch`](super::resolve::merge_branch)), then the FIRST platform
    /// override that applies is layered on
    /// ([`effective_for`](super::resolve::effective_for)).
    ///
    /// Platform scoping cuts both ways: `jgm/pandoc` and `ImageMagick/ImageMagick`
    /// name their commands ONLY under a `goos: linux` override, while
    /// `kubernetes/node-problem-detector` names windows `.exe` variants a linux
    /// install never produces. Note that ONE override wins outright — if it
    /// declares no `files:`, the inherited list stands even when a later override
    /// declares one, which is what `effective_for` does.
    fn effective_over<'a>(&'a self, base: &'a FileScope, os: &str, arch: &str) -> Outcome<'a> {
        let envs = self.envs.as_deref().or(base.envs.as_deref());
        if !super::resolve::env_supported(envs, os, arch) {
            return None;
        }
        let files = || self.files.as_deref().or(base.files.as_deref());
        let overrides = self.overrides.as_deref().or(base.overrides.as_deref());
        let applied = overrides.unwrap_or_default().iter().find(|o| {
            super::resolve::override_matches(
                o.goos.as_deref(),
                o.goarch.as_deref(),
                o.envs.as_deref(),
                os,
                arch,
            )
        });
        match applied {
            Some(o) => Some(o.files.as_deref().or_else(files)),
            None => Some(files()),
        }
    }
}

/// One `version_overrides` branch, as the scanner walks it.
#[derive(Default)]
struct BranchBuf {
    /// `version_constraint: "true"` — the branch
    /// [`select_branch`](super::resolve::select_branch) takes outright.
    is_true: bool,
    /// `no_asset` / `error_message` as DECLARED by this branch. Both are
    /// pointers in aqua's `overrideVersion`, so an omitted one inherits the
    /// package's and an explicit `no_asset: false` clears an inherited true.
    no_asset: Option<bool>,
    /// A non-empty `error_message:` — aqua logs it and refuses the install, so
    /// it is `no_asset` by another name (`golang/tools/gorename` uses it to say
    /// the command was deleted).
    errored: Option<bool>,
    scope: FileScope,
}

/// Everything the scanner needs from one package to say what a CURRENT release
/// installs, as opposed to what the package-level `files[]` advertises.
#[derive(Default)]
struct PkgBuf {
    base: FileScope,
    branches: Vec<BranchBuf>,
    /// The package-level `version_constraint`.
    constraint: Option<String>,
    /// A package-level `no_asset: true`.
    no_asset: bool,
    /// A package-level non-empty `error_message:`.
    errored: bool,
}

impl PkgBuf {
    /// What a current release installs on `(os, arch)`: `None` when this host gets
    /// nothing, else the commands — empty when the registry does not say (see
    /// [`Candidate::override_exes`]).
    ///
    /// Mirrors [`select_branch`](super::resolve::select_branch) as far as a scan
    /// can: aqua reads `version_overrides` only when the package-level
    /// `version_constraint` is present and does NOT hold, and `"false"` is the one
    /// value that never holds (1693 packages, e.g. `sharkdp/bat`). Every other
    /// constraint is a `>= <old version>` guard that holds for a current release,
    /// so the branches are dead history — reporting them made `pkgxdev/pkgx` claim
    /// the long-renamed `tea`. Among the branches only an unconditional `"true"`
    /// one can be resolved without evaluating a constraint expression; when every
    /// branch is constrained, which applies is a function of the version being
    /// installed, and guessing the last-listed one made `dineshba/tf-summarize`
    /// claim `terraform-plan-summarize`, a command it no longer ships.
    fn commands(&self, os: &str, arch: &str) -> Outcome<'_> {
        let blocked = self.no_asset || self.errored;
        if self.constraint.as_deref() != Some("false") {
            return if blocked { None } else { self.base.effective(os, arch) };
        }
        match self.branches.iter().find(|b| b.is_true) {
            // Each flag the branch declares replaces the package's; each it omits
            // is inherited.
            Some(b) if b.no_asset.unwrap_or(self.no_asset) || b.errored.unwrap_or(self.errored) => {
                None
            }
            Some(b) => b.scope.effective_over(&self.base, os, arch),
            // Every branch is constrained: which one applies depends on the
            // version, so claim neither commands nor unavailability.
            None => Some(None),
        }
    }

    /// The scope deeper keys currently belong to: the branch being scanned, else
    /// the package base.
    fn scope(&mut self) -> &mut FileScope {
        match self.branches.last_mut() {
            Some(b) => &mut b.scope,
            None => &mut self.base,
        }
    }

    /// The platform `overrides[]` item being scanned in the current scope.
    fn override_item(&mut self) -> Option<&mut OverrideBuf> {
        self.scope().overrides.as_mut()?.last_mut()
    }

    /// The list a [`Sink`] feeds, creating it if the key that opened it has not
    /// been recorded yet. `None` when the item it belongs to is missing (a
    /// malformed `overrides:` with no `- ` item).
    fn list(&mut self, sink: Sink) -> Option<&mut Vec<String>> {
        let (files, envs) = match sink.scope {
            Scope::Package => (&mut self.base.files, &mut self.base.envs),
            Scope::Branch => {
                let s = self.scope();
                (&mut s.files, &mut s.envs)
            }
            Scope::Override => {
                let item = self.override_item()?;
                (&mut item.files, &mut item.envs)
            }
        };
        Some(
            match sink.list {
                List::Files => files,
                List::Envs => envs,
            }
            .get_or_insert_default(),
        )
    }
}

/// Parse the root index into one [`Candidate`] per package, for the running host.
pub fn parse_index(text: &str) -> Vec<Candidate> {
    parse_index_on(text, crate::platform::goos(), crate::platform::goarch())
}

/// Parse the root index as `(os, arch)` would install it.
///
/// This is a block-aware line scan rather than a `serde_yml` parse: the document
/// is ~3 MB / ~100k lines and models far more than we need, and a scan over it
/// costs a few milliseconds (so there is no second index to build and keep
/// fresh). Structure relied on: a package starts at indent 2 (`  - key: v`), its
/// own fields sit at indent 4, and `files:`/`aliases:` entries at indent 6.
/// Anchoring on exact indents is what keeps a `files:` nested under
/// `version_overrides:` (indent 8) or a platform `overrides:` from leaking in as a
/// top-level exe.
///
/// The deeper `files:` lists are kept SEPARATE, per scope, and resolved at the end
/// of the package the way [`select_branch`](super::resolve::select_branch) and
/// [`effective_for`](super::resolve::effective_for) would — see
/// [`PkgBuf::commands`]. Merging them instead claimed commands the install never
/// produces: `BurntSushi/ripgrep` shipped as `xrep` below 0.0.10 and `rg` after,
/// and `kubernetes/node-problem-detector` names its windows `.exe` variants in a
/// platform override.
///
/// Packages we could not act on are dropped (see [`actionable`]) — a candidate
/// with no installable spec would only be noise.
fn parse_index_on(text: &str, os: &str, arch: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: std::collections::HashSet<Candidate> = std::collections::HashSet::new();
    let mut cur: Option<Candidate> = None;
    let mut pkg = PkgBuf::default();
    let mut block = Block::None;
    // The `files:` / env list being read, and where its key sits.
    let mut sink: Option<Sink> = None;
    // Which indent-4 list the deeper lines belong to.
    let mut in_base_overrides = false;
    let mut in_branches = false;
    // …and whether we are inside the current BRANCH's own platform `overrides:`.
    let mut in_branch_overrides = false;
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
        // A key at or left of the `files:` that opened the list ends it.
        if sink.is_some_and(|s| indent <= s.at) {
            sink = None;
        }
        // An entry of the `files:` list currently open, whatever depth it sits at.
        // Checked before the structural arms below, since a `files:` may be opened
        // by any of them and its entries then land on THEIR field indent.
        if let Some(target) = sink {
            if indent == target.at + 2 {
                let entry = t.strip_prefix("- ").unwrap_or(t);
                // `files:` entries are mappings keyed by `name:`; env entries are
                // bare scalars (`- darwin`, `- windows/arm64`).
                let value = match target.list {
                    List::Files => field(entry, "name:"),
                    List::Envs => t
                        .strip_prefix("- ")
                        .map(|v| v.trim().trim_matches('"').trim_matches('\'')),
                };
                if let (Some(v), Some(items)) = (value, pkg.list(target)) {
                    if !items.iter().any(|e| e == v) {
                        items.push(v.to_string());
                    }
                }
                continue;
            }
        }
        match indent {
            2 if t.starts_with("- ") => {
                finish_package(&mut out, &mut seen, cur.take(), &pkg, os, arch);
                pkg = PkgBuf::default();
                let mut c = Candidate::default();
                absorb(&mut c, &t[2..]); // the first field rides the dash line
                pkg.constraint = field(&t[2..], "version_constraint:").map(str::to_string);
                cur = Some(c);
                block = Block::None;
                sink = None;
                in_base_overrides = false;
                in_branches = false;
                in_branch_overrides = false;
            }
            // A package field. Whatever list we were walking ends here.
            4 => {
                let files = list_decl(t, "files:");
                let overrides = list_decl(t, "overrides:");
                let envs = list_decl(t, "supported_envs:");
                in_base_overrides = overrides == Some(ListDecl::Open);
                in_branches = t == "version_overrides:";
                in_branch_overrides = false;
                if let Some(v) = field(t, "version_constraint:") {
                    pkg.constraint = Some(v.to_string());
                }
                pkg.no_asset |= field(t, "no_asset:") == Some("true");
                pkg.errored |= field(t, "error_message:").is_some_and(|m| !m.is_empty());
                block = match t {
                    "aliases:" => Block::Aliases,
                    _ if is_block_description(t) => Block::Description,
                    _ => Block::None,
                };
                if files.is_some() {
                    pkg.base.files = Some(Vec::new());
                }
                if overrides.is_some() {
                    pkg.base.overrides = Some(Vec::new());
                }
                if envs.is_some() {
                    pkg.base.envs = Some(Vec::new());
                }
                sink = match (files, envs) {
                    (Some(ListDecl::Open), _) => Some(Sink { list: List::Files, scope: Scope::Package, at: indent }),
                    (_, Some(ListDecl::Open)) => Some(Sink { list: List::Envs, scope: Scope::Package, at: indent }),
                    _ => sink,
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
            // An entry of a package-level list, or a base-override / branch item.
            6 => {
                let entry = t.strip_prefix("- ").unwrap_or(t);
                if t.starts_with("- ") && in_base_overrides {
                    pkg.base.overrides.get_or_insert_default().push(OverrideBuf::default());
                } else if t.starts_with("- ") && in_branches {
                    pkg.branches.push(BranchBuf::default());
                    in_branch_overrides = false;
                }
                if in_base_overrides || in_branches {
                    // The item's first field rides the dash line, so it sits two
                    // columns right of the dash — where the item's other fields
                    // are. Passing the dash's own indent would make a list opened
                    // here swallow the sibling key that follows it.
                    let at = if t.starts_with("- ") { indent + 2 } else { indent };
                    absorb_scope_field(&mut pkg, entry, in_base_overrides, &mut in_branch_overrides, &mut sink, at);
                } else if block == Block::Aliases {
                    if let (Some(v), Some(c)) = (field(entry, "name:"), cur.as_mut()) {
                        c.aliases.push(v.to_string());
                    }
                }
            }
            // A field of the base-override item, or of the branch, we are inside.
            8 if in_base_overrides || in_branches => {
                // A branch's own field level: any platform `overrides:` it opened
                // (whose items sit at indent 10) ends here.
                in_branch_overrides &= !in_branches;
                absorb_scope_field(&mut pkg, t, in_base_overrides, &mut in_branch_overrides, &mut sink, indent);
            }
            // An item of a branch's own platform `overrides:`, and its fields.
            10 | 12 if in_branch_overrides => {
                let entry = if indent == 10 { t.strip_prefix("- ") } else { None };
                if let Some(entry) = entry {
                    pkg.scope().overrides.get_or_insert_default().push(OverrideBuf::default());
                    // Same dash-line offset as at indent 6, above.
                    absorb_scope_field(&mut pkg, entry, false, &mut in_branch_overrides, &mut sink, indent + 2);
                } else {
                    absorb_scope_field(&mut pkg, t, false, &mut in_branch_overrides, &mut sink, indent);
                }
            }
            _ => {}
        }
    }
    finish_package(&mut out, &mut seen, cur.take(), &pkg, os, arch);
    out
}

/// Read one field of a platform-override item or `version_overrides` branch.
///
/// `dash` lines and continuation lines carry the same keys, so both go through
/// here; only `files:`/`overrides:` change where later lines land.
fn absorb_scope_field(
    pkg: &mut PkgBuf,
    t: &str,
    base_override: bool,
    in_branch_overrides: &mut bool,
    sink: &mut Option<Sink>,
    indent: usize,
) {
    let in_override = base_override || *in_branch_overrides;
    let scope = if in_override { Scope::Override } else { Scope::Branch };
    if let Some(decl) = list_decl(t, "files:") {
        // Record the key even when the list is inline-empty (`files: []`), which
        // DECLARES an empty list rather than inheriting one.
        if let Some(items) = pkg.list(Sink { list: List::Files, scope, at: indent }) {
            items.clear();
        }
        *sink = (decl == ListDecl::Open).then_some(Sink { list: List::Files, scope, at: indent });
        return;
    }
    // `envs:` scopes ONE platform override; `supported_envs:` scopes a whole
    // branch. aqua spells them differently and they never appear together.
    let env_key = if in_override { "envs:" } else { "supported_envs:" };
    if let Some(decl) = list_decl(t, env_key) {
        if let Some(items) = pkg.list(Sink { list: List::Envs, scope, at: indent }) {
            items.clear();
        }
        *sink = (decl == ListDecl::Open).then_some(Sink { list: List::Envs, scope, at: indent });
        return;
    }
    if !in_override {
        if let Some(decl) = list_decl(t, "overrides:") {
            // A branch declaring its own platform overrides replaces the base's —
            // including `overrides: []`, which clears them.
            pkg.scope().overrides = Some(Vec::new());
            *in_branch_overrides = decl == ListDecl::Open;
            return;
        }
    }
    if in_override {
        for (key, set) in [("goos:", true), ("goarch:", false)] {
            if let Some(v) = field(t, key) {
                if let Some(item) = pkg.override_item() {
                    if set {
                        item.goos = Some(v.to_string());
                    } else {
                        item.goarch = Some(v.to_string());
                    }
                }
                return;
            }
        }
        return;
    }
    // Plain branch fields.
    if field(t, "version_constraint:") == Some("true") {
        if let Some(b) = pkg.branches.last_mut() {
            b.is_true = true;
        }
    } else if let Some(v) = field(t, "no_asset:") {
        if let Some(b) = pkg.branches.last_mut() {
            b.no_asset = Some(v == "true");
        }
    } else if let Some(m) = field(t, "error_message:") {
        if let Some(b) = pkg.branches.last_mut() {
            // A block scalar (`error_message: |`) leaves `|` as the value here —
            // still non-empty, which is all that matters.
            b.errored = Some(!m.is_empty());
        }
    }
}

/// Resolve a finished package's command evidence and hand it to
/// [`push_candidate`].
fn finish_package(
    out: &mut Vec<Candidate>,
    seen: &mut std::collections::HashSet<Candidate>,
    cand: Option<Candidate>,
    pkg: &PkgBuf,
    os: &str,
    arch: &str,
) {
    let Some(mut c) = cand else { return };
    c.exes = pkg.base.files.clone().unwrap_or_default();
    match pkg.commands(os, arch) {
        // The registry says this host installs nothing at all.
        None => c.unavailable = true,
        // `override_exes` records only what OVERRIDES the matched list.
        Some(Some(cmds)) if cmds != c.exes.as_slice() => c.override_exes = cmds.to_vec(),
        Some(_) => {}
    }
    push_candidate(out, seen, Some(c));
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
        // …and since no branch is unconditional, the constrained one is not
        // evidence of anything: `gh` is what a current version installs.
        assert!(gh.override_exes.is_empty(), "{:?}", gh.override_exes);
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
    version_constraint: "false"
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
        // The branch's OWN `files:`, in declaration order — `vpnkit-darwin` sits
        // under a platform override inside the branch and applies only there.
        assert_eq!(c.override_exes, vec!["rootlesskit", "vpnkit"]);

        let (certainty, cmds) = c.commands();
        assert_eq!(certainty, Certainty::Declared);
        assert_eq!(cmds, vec!["rootlesskit", "vpnkit"]);
        // …and the nested-name fallback ("rootless") is never reported as an exe.
        assert!(!cmds.contains(&"rootless"));
    }

    /// Branches are MUTUALLY EXCLUSIVE, so their `files:` must not be merged.
    /// `BurntSushi/ripgrep` shipped as `xrep` before 0.0.10 and `rg` after:
    /// merging would claim both, and claiming a command the selected branch does
    /// not install is exactly what `Hit.provides` must not do.
    #[test]
    fn parse_index_keeps_only_the_branch_that_would_be_installed() {
        // A `"true"` branch wins outright, wherever it sits in the list — that is
        // what `resolve::select_branch` does.
        let rg = parse_index(
            r#"
packages:
  - repo_owner: BurntSushi
    repo_name: ripgrep
    version_constraint: "false"
    version_overrides:
      - version_constraint: semver("<= 0.0.9")
        files:
          - name: xrep
      - version_constraint: "true"
        files:
          - name: rg
"#,
        );
        assert_eq!(rg[0].override_exes, vec!["rg"], "the `true` branch, not the union");

        // No `"true"` branch and the branches DISAGREE about what they install, so
        // which one applies is a function of the version: no branch is evidence and
        // the package-level declaration stands. Guessing the last-listed branch
        // made `dineshba/tf-summarize` claim `terraform-plan-summarize`, a command
        // it no longer ships.
        let staged = parse_index(
            r#"
packages:
  - repo_owner: a
    repo_name: b
    files:
      - name: current
    version_constraint: "false"
    version_overrides:
      - version_constraint: semver("<= 1.0.0")
        files:
          - name: old
      - version_constraint: semver("<= 2.0.0")
        files:
          - name: older-still
"#,
        );
        assert!(staged[0].override_exes.is_empty(), "{:?}", staged[0].override_exes);
        assert_eq!(staged[0].commands(), (Certainty::Declared, vec!["current"]));

        // The selected branch declaring no `files:` INHERITS the package's, the
        // way `resolve::merge_branch` does — it must not leave a losing branch's
        // names standing (`volta-cli/volta` installs `volta`, not `notion`).
        let volta = parse_index(
            r#"
packages:
  - repo_owner: volta-cli
    repo_name: volta
    version_constraint: "false"
    version_overrides:
      - version_constraint: semver("<= 1.0.0")
        files:
          - name: notion
      - version_constraint: "true"
        supported_envs:
          - darwin
"#,
        );
        assert!(volta[0].override_exes.is_empty(), "{:?}", volta[0].override_exes);
        assert_eq!(volta[0].commands(), (Certainty::Implied, vec!["volta"]));
    }

    /// A `files:` under a platform override INSIDE the selected branch is scoped to
    /// that platform, so it neither replaces the base list nor contributes names —
    /// `kubernetes/node-problem-detector` and `moby/buildkit` would otherwise
    /// report windows/qemu binaries as the commands of a linux install.
    #[test]
    fn parse_index_ignores_platform_files_nested_in_the_selected_branch() {
        // Pinned to a non-windows host: the override below deliberately does not
        // apply, and the assertions describe exactly that.
        let cands = parse_index_on(
            r#"
packages:
  - repo_owner: kubernetes
    repo_name: node-problem-detector
    files:
      - name: node-problem-detector
      - name: health-checker
      - name: log-counter
    version_constraint: "false"
    version_overrides:
      - version_constraint: "true"
        overrides:
          - goos: windows
            files:
              - name: node-problem-detector.exe
              - name: health-checker.exe
"#,
            "linux",
            "amd64",
        );
        let c = &cands[0];
        assert!(c.override_exes.is_empty(), "{:?}", c.override_exes);
        // The branch declares no `files:` of its own, so it inherits the base.
        assert_eq!(
            c.commands(),
            (
                Certainty::Declared,
                vec!["node-problem-detector", "health-checker", "log-counter"]
            )
        );
    }

    /// The package-level `version_constraint` decides whether the base entry
    /// applies at all; when it holds — as `>= <old version>` does for every
    /// current release — the branches are history. `ajeetdsouza/zoxide` keeps a
    /// `"true"` branch for pre-0.8.2 asset naming, and reporting it made
    /// `pkgxdev/pkgx` claim the long-renamed `tea`.
    #[test]
    fn parse_index_ignores_branches_when_the_package_constraint_holds() {
        let yaml = r#"
packages:
  - repo_owner: pkgxdev
    repo_name: pkgx
    version_constraint: semver(">= 1.0.0")
    version_overrides:
      - version_constraint: "true"
        files:
          - name: tea
"#;
        let c = &parse_index(yaml)[0];
        assert!(c.override_exes.is_empty(), "{:?}", c.override_exes);
        assert_eq!(c.commands(), (Certainty::Implied, vec!["pkgx"]));

        // `"false"` is the opposite instruction: the base NEVER applies, so the
        // branch is what gets installed (`sharkdp/bat`, `ripgrep`, `cubefs`).
        let always = yaml.replace(r#"semver(">= 1.0.0")"#, r#""false""#);
        assert_eq!(parse_index(&always)[0].override_exes, vec!["tea"]);
    }

    /// `merge_branch` REPLACES the package-level `files[]` when the branch has its
    /// own, so a command declared only at the package level is not installed.
    /// `cubefs/cubefs` lists `cfs-preload` package-wide and drops it in its
    /// `"true"` branch.
    #[test]
    fn parse_index_lets_the_selected_branch_replace_package_level_files() {
        let cands = parse_index(
            r#"
packages:
  - repo_owner: cubefs
    repo_name: cubefs
    files:
      - name: cfs-cli
      - name: cfs-preload
    version_constraint: "false"
    version_overrides:
      - version_constraint: "true"
        files:
          - name: cfs-cli
          - name: cfs-server
"#,
        );
        let c = &cands[0];
        assert_eq!(c.exes, vec!["cfs-cli", "cfs-preload"]);
        assert_eq!(c.override_exes, vec!["cfs-cli", "cfs-server"]);
        // The branch wins: `cfs-preload` is never reported as installed.
        assert_eq!(c.commands(), (Certainty::Declared, vec!["cfs-cli", "cfs-server"]));
    }

    /// A base-level platform `overrides[].files:` counts only when the item applies
    /// to THIS host: `kubernetes/node-problem-detector` names windows `.exe`
    /// variants a linux install never produces, while `jgm/pandoc` and
    /// `ImageMagick/ImageMagick` name their real commands ONLY under `goos: linux`.
    #[test]
    fn parse_index_reads_only_the_host_platform_override_files() {
        let pkg = |over: &str| format!("packages:\n  - repo_owner: a\n    repo_name: b\n    overrides:\n{over}");
        let windows = pkg("      - goos: windows\n        files:\n          - name: b.exe\n");
        let linux = pkg("      - goos: linux\n        files:\n          - name: real-b\n");

        // The foreign override is not evidence; only the repo name is left.
        let c = &parse_index_on(&windows, "linux", "amd64")[0];
        assert!(c.override_exes.is_empty(), "{:?}", c.override_exes);
        assert_eq!(c.commands(), (Certainty::Implied, vec!["b"]));

        // The host's override is, even though the package declares no `files:`.
        let c = &parse_index_on(&linux, "linux", "amd64")[0];
        assert_eq!(c.commands(), (Certainty::Declared, vec!["real-b"]));
        // …and it REPLACES the package-level list rather than adding to it.
        let both = format!("{linux}    files:\n      - name: generic-b\n");
        let c = &parse_index_on(&both, "linux", "amd64")[0];
        assert_eq!(c.exes, vec!["generic-b"]);
        assert_eq!(c.commands(), (Certainty::Declared, vec!["real-b"]));
        // On another OS the same package keeps its own list.
        let c = &parse_index_on(&both, "darwin", "arm64")[0];
        assert_eq!(c.commands(), (Certainty::Declared, vec!["generic-b"]));
    }

    /// A package whose `supported_envs` exclude this host installs nothing here,
    /// however many commands its `files[]` advertise (`aqua:ahkohd/oyo` is
    /// darwin-only, so a linux `ubix add oy` is a dead end, not a pick).
    #[test]
    fn parse_index_marks_a_package_unsupported_here_as_unavailable() {
        let yaml = "packages:\n  - repo_owner: ahkohd\n    repo_name: oyo\n    files:\n      - name: oy\n    supported_envs:\n      - darwin\n";
        let linux = &parse_index_on(yaml, "linux", "amd64")[0];
        assert!(linux.unavailable);
        // The advertised list is still reported — it is what OTHER hosts get.
        assert_eq!(linux.exes, vec!["oy"]);
        let mac = &parse_index_on(yaml, "darwin", "arm64")[0];
        assert!(!mac.unavailable);
    }

    /// `supported_envs` declared by the branch we install from replaces the
    /// package's, exactly as `merge_branch` does.
    #[test]
    fn parse_index_lets_the_selected_branch_replace_supported_envs() {
        let yaml = "packages:\n  - repo_owner: a\n    repo_name: b\n    supported_envs:\n      - darwin\n    version_constraint: \"false\"\n    version_overrides:\n      - version_constraint: \"true\"\n        supported_envs:\n          - linux/amd64\n        files:\n          - name: b-linux\n";
        let c = &parse_index_on(yaml, "linux", "amd64")[0];
        assert!(!c.unavailable, "the branch widened the envs to this host");
        assert_eq!(c.commands(), (Certainty::Declared, vec!["b-linux"]));
        // …and narrowed them away from the package's own darwin.
        assert!(parse_index_on(yaml, "darwin", "arm64")[0].unavailable);
    }

    /// A non-empty `error_message` is `no_asset` by another name: aqua refuses
    /// the install. `golang/tools/gorename` uses it to say the command is gone,
    /// and without this the scanner offered `go:github.com/golang/tools`, a
    /// module root that does not install `gorename` at all.
    #[test]
    fn parse_index_treats_a_branch_error_message_as_unavailable() {
        let yaml = "packages:\n  - type: go_install\n    repo_owner: golang\n    repo_name: tools\n    name: golang/tools/gorename\n    version_constraint: \"false\"\n    version_overrides:\n      - version_constraint: \"true\"\n        error_message: gorename was deleted at v0.26.0\n";
        assert!(parse_index_on(yaml, "linux", "amd64")[0].unavailable);
        // A block scalar still counts as a message.
        let block = yaml.replace("gorename was deleted at v0.26.0", "|");
        assert!(parse_index_on(&block, "linux", "amd64")[0].unavailable);
    }

    /// An explicitly EMPTY env list is the opposite of an absent one — aqua's
    /// `matchEnvs` loop never runs and returns false.
    #[test]
    fn parse_index_reads_an_empty_env_list_as_no_platform() {
        let pkg = "packages:\n  - repo_owner: a\n    repo_name: b\n    supported_envs: []\n";
        assert!(parse_index_on(pkg, "linux", "amd64")[0].unavailable);

        // …and an `envs: []` override must not swallow the one after it.
        let ov = "packages:\n  - repo_owner: a\n    repo_name: b\n    overrides:\n      - envs: []\n        files:\n          - name: wrong\n      - goos: linux\n        files:\n          - name: right\n";
        let c = &parse_index_on(ov, "linux", "amd64")[0];
        assert!(!c.unavailable);
        assert_eq!(c.command_names(), vec!["right"]);
    }

    /// `no_asset` is a pointer in aqua, so the branch we install from can CLEAR
    /// one the package set.
    #[test]
    fn parse_index_lets_a_branch_clear_a_package_level_no_asset() {
        let yaml = "packages:\n  - repo_owner: a\n    repo_name: b\n    no_asset: true\n    version_constraint: \"false\"\n    version_overrides:\n      - version_constraint: \"true\"\n        no_asset: false\n        files:\n          - name: b-again\n";
        let c = &parse_index_on(yaml, "linux", "amd64")[0];
        assert!(!c.unavailable);
        assert_eq!(c.command_names(), vec!["b-again"]);
    }

    /// A branch marked `no_asset` installs nothing anywhere — `apache/tomcat`'s
    /// `"true"` branch is the registry's one real case.
    #[test]
    fn parse_index_marks_a_no_asset_branch_as_unavailable() {
        let yaml = "packages:\n  - repo_owner: apache\n    repo_name: tomcat\n    version_constraint: \"false\"\n    version_overrides:\n      - version_constraint: \"true\"\n        no_asset: true\n";
        for (os, arch) in [("linux", "amd64"), ("darwin", "arm64"), ("windows", "amd64")] {
            assert!(parse_index_on(yaml, os, arch)[0].unavailable, "{os}/{arch}");
        }
    }

    /// An override's `envs:` gates it just like `supported_envs` — `eza` leads
    /// with a darwin/windows-arm64 entry that must not swallow a linux install.
    #[test]
    fn parse_index_honors_an_override_envs_gate() {
        let yaml = "packages:\n  - repo_owner: eza-community\n    repo_name: eza\n    overrides:\n      - envs:\n          - darwin\n          - windows/arm64\n        files:\n          - name: eza-cargo\n      - goos: linux\n        files:\n          - name: eza-linux\n";
        let cmds = |os, arch| parse_index_on(yaml, os, arch)[0].command_names().join(",");
        assert_eq!(cmds("linux", "amd64"), "eza-linux");
        assert_eq!(cmds("darwin", "arm64"), "eza-cargo");
        assert_eq!(cmds("windows", "arm64"), "eza-cargo");
        // windows/amd64 matches neither → the repo name is all that is left.
        assert_eq!(cmds("windows", "amd64"), "eza");
    }

    /// `effective_for` applies the FIRST override that matches, in declaration
    /// order — aqua's `getOverride` does not rank by specificity.
    #[test]
    fn parse_index_applies_the_first_matching_platform_override() {
        let yaml = "packages:\n  - repo_owner: a\n    repo_name: b\n    overrides:\n      - goos: linux\n        files:\n          - name: os-only\n      - goos: linux\n        goarch: amd64\n        files:\n          - name: os-and-arch\n      - goarch: arm64\n        files:\n          - name: arch-only\n";
        let cmds = |os, arch| parse_index_on(yaml, os, arch)[0].command_names().join(",");
        // linux/amd64 matches the first entry, so the narrower one never runs.
        assert_eq!(cmds("linux", "amd64"), "os-only");
        assert_eq!(cmds("linux", "riscv64"), "os-only");
        assert_eq!(cmds("darwin", "arm64"), "arch-only");
        // Nothing applies → the repo name is the only guess left.
        assert_eq!(cmds("windows", "amd64"), "b");
    }

    /// A branch declaring `overrides:` replaces the package's — including
    /// `overrides: []`, which CLEARS them (`docker/hub-tool`'s `"true"` branch).
    #[test]
    fn parse_index_lets_a_branch_clear_inherited_platform_overrides() {
        let yaml = r#"
packages:
  - repo_owner: docker
    repo_name: hub-tool
    files:
      - name: hub-tool
    overrides:
      - goos: linux
        files:
          - name: hub-tool-linux
    version_constraint: "false"
    version_overrides:
      - version_constraint: "true"
        overrides: []
"#;
        let c = &parse_index_on(yaml, "linux", "amd64")[0];
        // The base install would use the override; the current release does not.
        assert_eq!(c.exes, vec!["hub-tool"]);
        assert_eq!(c.override_exes, Vec::<String>::new());
        assert_eq!(c.commands(), (Certainty::Declared, vec!["hub-tool"]));
    }

    /// A platform override INSIDE a branch is scoped to that branch and still
    /// host-filtered.
    #[test]
    fn parse_index_reads_platform_overrides_inside_a_branch() {
        let yaml = r#"
packages:
  - repo_owner: a
    repo_name: b
    files:
      - name: old
    version_constraint: "false"
    version_overrides:
      - version_constraint: "true"
        overrides:
          - goos: windows
            files:
              - name: new.exe
          - goos: linux
            files:
              - name: new
"#;
        assert_eq!(parse_index_on(yaml, "linux", "amd64")[0].command_names(), vec!["new"]);
        assert_eq!(parse_index_on(yaml, "windows", "amd64")[0].command_names(), vec!["new.exe"]);
        // No override applies → the branch inherits the package's `files:`.
        assert_eq!(parse_index_on(yaml, "darwin", "arm64")[0].command_names(), vec!["old"]);
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
