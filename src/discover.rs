//! Bare-name discovery: turn a command name (`bat`) into an installable spec
//! (`aqua:sharkdp/bat`) so users don't have to know the upstream repo or hunt
//! for the right aqua package by hand.
//!
//! The aqua root index inlines every package definition, which makes it a
//! ready-made name → (source, repo) map across ecosystems: `github_release` and
//! `http` entries become `aqua:` (synthesized `github:`/`url:` configs), `cargo`
//! entries carry a `crate:`, `go_install` entries a module `path:`. So discovery
//! is pure scoring over [`Candidate`]s — no network beyond the TTL-cached index
//! that `search` already fetches, and no second index to keep fresh.
//!
//! Ranking is deliberately conservative: only an EXACT name match is ever
//! auto-installed (see [`pick`]); substring and description hits are shown but
//! always require the user to choose.

use crate::aqua::registry::Candidate;

/// How a candidate matched the query, weakest → strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    /// Only the description mentions the query.
    Description,
    /// A name-ish field contains the query, but nothing matched exactly.
    Substring,
    /// A former package name (`aliases[].name`) matches exactly.
    Alias,
    /// A declared `files[].name` matches exactly (`gh` → `cli/cli`).
    Command,
    /// aqua's package `name:` ends with the query (`bat` → `crates.io/bat`).
    Name,
    /// The repo name matches exactly.
    Repo,
}

impl Why {
    /// Base score. Repo/package names outrank a declared command name because
    /// aqua only spells `files[].name` out when it differs from the repo — so an
    /// exe-only hit is often a mirror/fork (`rg` resolves to
    /// `microsoft/ripgrep-prebuilt`, not `BurntSushi/ripgrep`).
    fn base(self) -> u32 {
        match self {
            Why::Repo => 100,
            Why::Name => 90,
            Why::Command => 80,
            Why::Alias => 70,
            Why::Substring => 30,
            Why::Description => 10,
        }
    }

    /// Whether this is an exact match (as opposed to a fuzzy guess).
    pub fn is_exact(self) -> bool {
        self >= Why::Alias
    }

    /// Short evidence label for the candidate table.
    pub fn label(self) -> &'static str {
        match self {
            Why::Repo => "repo name",
            Why::Name => "package name",
            Why::Command => "command name",
            Why::Alias => "former name",
            Why::Substring => "name contains",
            Why::Description => "description",
        }
    }
}

/// One ranked discovery result.
#[derive(Debug, Clone)]
pub struct Hit {
    pub candidate: Candidate,
    pub why: Why,
    /// The spec `ubix add` would install, or `None` when ubix has no equivalent
    /// source for this aqua package type.
    pub spec: Option<String>,
    pub score: u32,
    /// Whether this package is KNOWN to install a command named like the query.
    /// A package whose declared `files[]` prove otherwise (`bottom` installs
    /// `btm`) must not out-pick one that does — see [`pick`] — and neither must
    /// one where the commands are only known per-version.
    pub provides: bool,
}

/// The spec ubix would install a candidate from, or `None` when its aqua type
/// has no ubix equivalent (`github_content`/`github_archive` are registry file
/// layouts, not release artifacts).
///
/// `aqua:` is used rather than a bare `github:` so the install path goes through
/// the registry synthesis (per-platform `matching`, `exe`, `rename`) instead of
/// leaving ubi to guess.
pub fn spec_for(c: &Candidate) -> Option<String> {
    match c.kind.as_str() {
        // `http` is synthesized as a `url:` tool by the same aqua entry point.
        // The package PATH (not owner/repo) is what addresses it in the registry.
        "github_release" | "http" => Some(format!("aqua:{}", c.pkg_path())),
        "cargo" => c.locator.as_ref().map(|k| format!("cargo:{k}")),
        // aqua defaults a go module path to the repo path when `path:` is absent
        // — but a `_go/…` package may carry the path and no repo at all, so
        // neither half may be assumed present.
        "go_install" | "go_build" => c
            .locator
            .clone()
            .or_else(|| {
                (!c.owner.is_empty() && !c.repo.is_empty())
                    .then(|| format!("github.com/{}/{}", c.owner, c.repo))
            })
            .map(|path| format!("go:{path}")),
        _ => None,
    }
}

/// Preference between equally-well-named candidates: a prebuilt release beats a
/// source build, which beats something ubix cannot install at all.
fn kind_bonus(kind: &str) -> u32 {
    match kind {
        "github_release" => 6,
        "http" => 4,
        "cargo" => 2,
        "go_install" | "go_build" => 1,
        _ => 0,
    }
}

/// Whether `spec` is a bare tool name (no source prefix, no `owner/repo`) and so
/// should go through discovery instead of `parse_spec`.
pub fn is_bare_name(spec: &str) -> bool {
    let s = spec.trim();
    !s.is_empty() && !s.contains(':') && !s.contains('/')
}

/// Score every candidate against `query`, strongest first. Candidates that match
/// nothing are dropped. Ties keep root-index order (stable sort).
pub fn rank(cands: &[Candidate], query: &str) -> Vec<Hit> {
    let q = query.to_ascii_lowercase();
    let mut hits: Vec<Hit> = cands
        .iter()
        .filter_map(|c| {
            let why = why_matched(c, &q)?;
            Some(Hit {
                score: why.base() + kind_bonus(&c.kind),
                why,
                spec: spec_for(c),
                provides: provides(c, &q),
                candidate: c.clone(),
            })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score));
    // Collapse rows that would install the exact same thing, keeping the
    // best-scoring one. Without this, a repo listed twice in the registry reads
    // as a tie and [`pick`] would refuse a choice that has only one outcome.
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(h.spec.clone().unwrap_or_else(|| h.candidate.pkg_path())));
    hits
}

/// Whether `c` is KNOWN to install a command named `q` (already lowercased).
///
/// [`Candidate::commands`] is what makes this trustworthy: when a package
/// declares `files[]` only inside its version overrides, those names — not the
/// implied repo/package name — are the evidence. `sharkdp/bat` declares `bat`
/// there, so it provides `bat`; `docker/cli/rootless` declares
/// `rootlesskit`/`vpnkit`, so it does NOT provide `rootless` even though its
/// package name ends that way.
fn provides(c: &Candidate, q: &str) -> bool {
    c.commands().1.iter().any(|n| n.to_ascii_lowercase() == *q)
}

/// The strongest reason `c` matches the (already lowercased) `q`, if any.
fn why_matched(c: &Candidate, q: &str) -> Option<Why> {
    let eq = |s: &str| s.to_ascii_lowercase() == *q;
    // aqua names are paths (`crates.io/bat`, `microsoft/vscode/code`); the last
    // segment is the command-ish part.
    let last = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    if eq(&c.repo) {
        return Some(Why::Repo);
    }
    if c.name.as_deref().is_some_and(|n| eq(&last(n))) {
        return Some(Why::Name);
    }
    if c.exes.iter().any(|e| eq(e)) {
        return Some(Why::Command);
    }
    if c.aliases.iter().any(|a| eq(&last(a))) {
        return Some(Why::Alias);
    }
    let contains = |s: &str| s.to_ascii_lowercase().contains(q);
    if contains(&c.repo)
        || c.name.as_deref().is_some_and(contains)
        || c.exes.iter().any(|e| contains(e))
        || c.aliases.iter().any(|a| contains(a))
    {
        return Some(Why::Substring);
    }
    if c.description.as_deref().is_some_and(contains) {
        return Some(Why::Description);
    }
    None
}

/// The unambiguous winner among ranked `hits`, if there is one.
///
/// Four conditions, all required, so `add` never silently installs a guess:
/// the top hit matched EXACTLY, ubix can install it, no other candidate scored
/// as high, and it is not SHADOWED — i.e. its own `files[]` don't prove it
/// installs some other command while a rival exact match does install the one
/// that was asked for.
///
/// Shadowing is what separates a real conflict from the benign case: `bottom`
/// installs `btm` and nothing else claims `bottom`, so it still auto-picks (the
/// caller names the real command); but a repo literally named `foo` that ships
/// only `bar` must not beat the package that ships `foo`.
pub fn pick(hits: &[Hit]) -> Option<&Hit> {
    let top = hits.first()?;
    let unique = hits.get(1).is_none_or(|next| next.score < top.score);
    let shadowed = !top.provides && hits[1..].iter().any(|h| h.provides && h.why.is_exact());
    (top.why.is_exact() && top.spec.is_some() && unique && !shadowed).then_some(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(owner: &str, repo: &str, kind: &str) -> Candidate {
        Candidate {
            owner: owner.into(),
            repo: repo.into(),
            kind: kind.into(),
            ..Candidate::default()
        }
    }

    /// `bat` is in the registry twice: a github_release and a `crates.io/bat`
    /// cargo entry. Both match the repo name exactly, so the prebuilt release
    /// must win on the kind bonus alone — and win decisively enough to auto-add.
    #[test]
    fn prebuilt_release_outranks_source_build_for_the_same_repo() {
        let cargo = Candidate {
            name: Some("crates.io/bat".into()),
            locator: Some("bat".into()),
            ..cand("sharkdp", "bat", "cargo")
        };
        let hits = rank(&[cargo, cand("sharkdp", "bat", "github_release")], "bat");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].spec.as_deref(), Some("aqua:sharkdp/bat"));
        assert_eq!(hits[1].spec.as_deref(), Some("cargo:bat"));
        assert_eq!(pick(&hits).unwrap().spec.as_deref(), Some("aqua:sharkdp/bat"));
    }

    #[test]
    fn command_name_resolves_when_it_differs_from_the_repo() {
        let gh = Candidate { exes: vec!["gh".into()], ..cand("cli", "cli", "github_release") };
        let hits = rank(&[gh], "gh");
        assert_eq!(hits[0].why, Why::Command);
        assert_eq!(pick(&hits).unwrap().spec.as_deref(), Some("aqua:cli/cli"));
    }

    #[test]
    fn exact_repo_outranks_a_command_name_hit_from_a_mirror() {
        // The `rg`-shaped case: a mirror declares `files: - name: rg`, the real
        // repo matches by name. The name match must win.
        let mirror = Candidate {
            exes: vec!["rg".into()],
            ..cand("microsoft", "ripgrep-prebuilt", "github_release")
        };
        let real = cand("BurntSushi", "rg", "github_release");
        let hits = rank(&[mirror, real], "rg");
        assert_eq!(hits[0].candidate.owner, "BurntSushi");
    }

    #[test]
    fn alias_still_resolves_after_a_rename() {
        let renamed = Candidate {
            aliases: vec!["Azure/aztfy".into()],
            ..cand("Azure", "aztfexport", "github_release")
        };
        let hits = rank(&[renamed], "aztfy");
        assert_eq!(hits[0].why, Why::Alias);
        assert!(pick(&hits).is_some(), "an exact alias hit is unambiguous");
    }

    #[test]
    fn substring_and_description_hits_are_never_auto_picked() {
        let hits = rank(&[cand("sharkdp", "bat-extras", "github_release")], "bat");
        assert_eq!(hits[0].why, Why::Substring);
        assert!(pick(&hits).is_none(), "a fuzzy hit must not install itself");

        let described = Candidate {
            description: Some("A cat(1) clone with wings".into()),
            ..cand("sharkdp", "bat", "github_release")
        };
        let hits = rank(&[described], "clone");
        assert_eq!(hits[0].why, Why::Description);
        assert!(pick(&hits).is_none());
    }

    #[test]
    fn two_equally_named_repos_stay_ambiguous() {
        let hits = rank(
            &[cand("alice", "tool", "github_release"), cand("bob", "tool", "github_release")],
            "tool",
        );
        assert_eq!(hits[0].score, hits[1].score);
        assert!(pick(&hits).is_none(), "a tie must ask the user");
    }

    #[test]
    fn uninstallable_types_rank_but_never_auto_pick() {
        let hits = rank(&[cand("some", "tool", "github_content")], "tool");
        assert_eq!(hits[0].why, Why::Repo);
        assert!(hits[0].spec.is_none());
        assert!(pick(&hits).is_none());
    }

    /// A repo literally named `foo` that ships only `bar` must not out-pick the
    /// package that actually ships `foo` — that is a real conflict, not the
    /// benign rename case below.
    #[test]
    fn a_repo_name_match_that_ships_another_command_is_shadowed() {
        let namesake =
            Candidate { exes: vec!["bar".into()], ..cand("alice", "foo", "github_release") };
        let provider =
            Candidate { exes: vec!["foo".into()], ..cand("bob", "tool", "github_release") };
        let hits = rank(&[namesake, provider], "foo");
        assert_eq!(hits[0].candidate.owner, "alice", "repo name still ranks first");
        assert!(!hits[0].provides);
        assert!(hits[1].provides);
        assert!(pick(&hits).is_none(), "a shadowed top hit must ask the user");
    }

    /// `ClementTsang/bottom` installs `btm` and nothing else claims `bottom`, so
    /// it is still the right unattended answer (the caller names the real
    /// command in its resolution note).
    #[test]
    fn a_renamed_binary_still_auto_picks_when_nothing_rivals_it() {
        let bottom =
            Candidate { exes: vec!["btm".into()], ..cand("ClementTsang", "bottom", "github_release") };
        let hits = rank(&[bottom], "bottom");
        assert!(!hits[0].provides);
        assert_eq!(
            pick(&hits).map(|h| h.candidate.command_names()),
            Some(vec!["btm"]),
            "no rival installs `bottom`, so this is unambiguous"
        );
    }

    /// `docker/cli/rootless` matches `rootless` on its aqua name, but the only
    /// commands it declares (`rootlesskit`, `vpnkit`) live in a version override.
    /// Nothing named `rootless` is ever installed, so we must not claim it is.
    #[test]
    fn per_version_evidence_never_claims_to_provide_the_query() {
        let rootless = Candidate {
            name: Some("docker/cli/rootless".into()),
            override_exes: vec!["rootlesskit".into(), "vpnkit".into()],
            ..cand("docker", "cli", "github_release")
        };
        let hits = rank(std::slice::from_ref(&rootless), "rootless");
        assert_eq!(hits[0].why, Why::Name, "the aqua name still matches exactly");
        assert!(!hits[0].provides, "the override names it installs are not `rootless`");
        // Unrivaled, so it is still offered — with an honest per-version note.
        assert!(pick(&hits).is_some());

        // But a package that really does ship `rootless` shadows it.
        let real = Candidate {
            exes: vec!["rootless".into()],
            ..cand("someone", "rootless-tool", "github_release")
        };
        let hits = rank(&[rootless, real], "rootless");
        assert!(hits.iter().any(|h| h.provides));
        assert!(pick(&hits).is_none());
    }

    /// The flip side: `sharkdp/bat` has NO package-level `files[]` either — every
    /// one of its `version_overrides` declares `bat`. That IS evidence it installs
    /// `bat`, so it must not be shadowed by the `crates.io/bat` cargo entry.
    #[test]
    fn an_override_that_declares_the_query_still_counts_as_providing_it() {
        let release = Candidate {
            override_exes: vec!["bat".into()],
            ..cand("sharkdp", "bat", "github_release")
        };
        let cargo = Candidate {
            name: Some("crates.io/bat".into()),
            locator: Some("bat".into()),
            ..cand("sharkdp", "bat", "cargo")
        };
        let hits = rank(&[release, cargo], "bat");
        assert!(hits[0].provides, "declared per version, but declared");
        assert_eq!(
            pick(&hits).and_then(|h| h.spec.clone()),
            Some("aqua:sharkdp/bat".into()),
            "the prebuilt release still wins outright"
        );
    }

    /// `_go/…` packages carry a module path and no GitHub repo at all.
    #[test]
    fn a_repoless_go_package_resolves_from_its_locator_alone() {
        let c = Candidate {
            name: Some("_go/sigsum.org/sigsum-go#cmd/sigsum-submit".into()),
            kind: "go_install".into(),
            locator: Some("sigsum.org/sigsum-go/cmd/sigsum-submit".into()),
            ..Candidate::default()
        };
        let hits = rank(&[c], "sigsum-submit");
        assert_eq!(hits[0].why, Why::Name);
        assert!(hits[0].provides);
        assert_eq!(
            pick(&hits).and_then(|h| h.spec.clone()),
            Some("go:sigsum.org/sigsum-go/cmd/sigsum-submit".into())
        );
        // With neither locator nor repo there is nothing to install.
        let bare = Candidate { kind: "go_install".into(), ..Candidate::default() };
        assert_eq!(spec_for(&bare), None);
    }

    #[test]
    fn go_module_path_falls_back_to_the_repo_path() {
        let with_path = Candidate {
            locator: Some("github.com/x/y/cmd/z".into()),
            ..cand("x", "y", "go_install")
        };
        assert_eq!(spec_for(&with_path).as_deref(), Some("go:github.com/x/y/cmd/z"));
        assert_eq!(
            spec_for(&cand("x", "y", "go_install")).as_deref(),
            Some("go:github.com/x/y")
        );
    }

    #[test]
    fn bare_name_detection() {
        assert!(is_bare_name("bat"));
        assert!(is_bare_name("kubectl-who_can"));
        assert!(!is_bare_name("owner/repo"));
        assert!(!is_bare_name("pypi:ruff"));
        assert!(!is_bare_name("https://example.com/x"));
        assert!(!is_bare_name(""));
    }
}
