//! Branch selection, layered merge, and platform gating (plan §7).
//!
//! Given a parsed [`Package`] and a resolved-latest version, this produces an
//! [`Effective`] view (merged base ⊕ branch ⊕ matching goos/goarch override)
//! for a *specific* (goos, goarch), or reports that the platform is unavailable.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use super::schema::{FileEntry, Package, PlatformOverride, VersionOverride};

/// The registry.yaml URL for a package (used in every degrade message).
pub fn registry_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/aquaproj/aqua-registry/blob/main/pkgs/{owner}/{repo}/registry.yaml")
}

/// The standard degrade error: an aqua construct ubix can't synthesize, with a
/// pointer at the package's registry.yaml and the manual fallback.
pub fn unsupported(owner: &str, repo: &str, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "unsupported aqua construct: {reason}; see {} and add a `github:` entry manually",
        registry_url(owner, repo)
    )
}

/// Fully-merged, still-templated fields for one branch (pre-platform-override).
#[derive(Debug, Clone, Default)]
pub struct Branch {
    pub asset: Option<String>,
    pub format: Option<String>,
    pub files: Option<Vec<FileEntry>>,
    pub replacements: BTreeMap<String, String>,
    pub supported_envs: Vec<String>,
    pub version_prefix: Option<String>,
    /// Platform overrides visible in this branch (branch's own, else base's).
    pub overrides: Vec<PlatformOverride>,
    /// Non-github_release type found on the branch (→ degrade).
    pub type_: Option<String>,
    /// Branch-level `no_asset`: the whole branch is unavailable (no binary).
    pub no_asset: bool,
}

/// The effective (merged) fields for a single platform.
#[derive(Debug, Clone, Default)]
pub struct Effective {
    pub asset: String,
    pub format: String,
    pub files: Vec<FileEntry>,
    pub replacements: BTreeMap<String, String>,
    pub version_prefix: Option<String>,
}

/// Select the winning `version_override` branch for `latest_version`, merged
/// onto the package base (plan §7 steps 1–3).
///
/// * If any branch has `version_constraint == "true"`, take it.
/// * Else evaluate each branch's constraint against `latest_version`; take the
///   first match (list order).
/// * Else bail with the registry.yaml link.
///
/// `latest_version` is the resolved latest tag (may carry a `v`/prefix — we
/// only compare the semver core).
pub fn select_branch(pkg: &Package, latest_version: &str, owner: &str, repo: &str) -> Result<Branch> {
    // No version_overrides at all: the base itself is the branch (simple pkgs).
    if pkg.version_overrides.is_empty() {
        return Ok(base_branch(pkg));
    }

    // 1) `version_constraint == "true"` wins outright.
    if let Some(vo) = pkg
        .version_overrides
        .iter()
        .find(|v| v.version_constraint.as_deref() == Some("true"))
    {
        return Ok(merge_branch(pkg, vo));
    }

    // 2) Evaluate comparison constraints in list order; first match wins.
    for vo in &pkg.version_overrides {
        let Some(c) = vo.version_constraint.as_deref() else {
            continue;
        };
        if eval_constraint(c, latest_version)? {
            return Ok(merge_branch(pkg, vo));
        }
    }

    bail!(
        "no evaluable version branch for `{owner}/{repo}` at version `{latest_version}`; \
         see {} and add a `github:` entry manually",
        registry_url(owner, repo)
    );
}

/// The base package as a branch (used when there are no version_overrides).
fn base_branch(pkg: &Package) -> Branch {
    Branch {
        asset: pkg.asset.clone(),
        format: pkg.format.clone(),
        files: pkg.files.clone(),
        replacements: pkg.replacements.clone().unwrap_or_default(),
        supported_envs: pkg.supported_envs.clone().unwrap_or_default(),
        version_prefix: pkg.version_prefix.clone(),
        overrides: pkg.overrides.clone(),
        type_: pkg.type_.clone(),
        // Base package has no branch-level no_asset (only platform overrides do).
        no_asset: false,
    }
}

/// Merge base ⊕ version_override into a [`Branch`] (shallow field override;
/// `replacements` map-merged with branch keys winning; `files` replaced wholesale
/// when the branch supplies them, else inherited).
fn merge_branch(pkg: &Package, vo: &VersionOverride) -> Branch {
    let mut b = base_branch(pkg);
    if vo.asset.is_some() {
        b.asset = vo.asset.clone();
    }
    if vo.format.is_some() {
        b.format = vo.format.clone();
    }
    if vo.files.is_some() {
        b.files = vo.files.clone();
    }
    if let Some(r) = &vo.replacements {
        for (k, v) in r {
            b.replacements.insert(k.clone(), v.clone());
        }
    }
    if let Some(se) = &vo.supported_envs {
        b.supported_envs = se.clone();
    }
    if vo.version_prefix.is_some() {
        b.version_prefix = vo.version_prefix.clone();
    }
    if !vo.overrides.is_empty() {
        b.overrides = vo.overrides.clone();
    }
    if vo.type_.is_some() {
        b.type_ = vo.type_.clone();
    }
    if vo.no_asset {
        b.no_asset = true;
    }
    b
}

/// Resolve the effective fields for a single (goos, goarch), applying the
/// matching platform override (specificity-first). Returns `Ok(None)` when the
/// platform is unavailable (unsupported env or `no_asset`).
pub fn effective_for(branch: &Branch, goos: &str, goarch: &str) -> Result<Option<Effective>> {
    // A branch marked `no_asset` has no binary for any platform → unavailable.
    if branch.no_asset {
        return Ok(None);
    }
    if !env_supported(&branch.supported_envs, goos, goarch) {
        return Ok(None);
    }

    let mut asset = branch.asset.clone();
    let mut format = branch.format.clone();
    let mut files = branch.files.clone();
    let mut replacements = branch.replacements.clone();

    if let Some(ov) = pick_override(&branch.overrides, goos, goarch) {
        if ov.no_asset {
            return Ok(None);
        }
        if ov.asset.is_some() {
            asset = ov.asset.clone();
        }
        if ov.format.is_some() {
            format = ov.format.clone();
        }
        if ov.files.is_some() {
            files = ov.files.clone();
        }
        if let Some(r) = &ov.replacements {
            for (k, v) in r {
                replacements.insert(k.clone(), v.clone());
            }
        }
    }

    let Some(asset) = asset else {
        // No asset template resolvable for this platform.
        return Ok(None);
    };

    Ok(Some(Effective {
        asset,
        // `format: raw` → treat as empty (no extension). aqua uses the literal
        // string "raw" to mean "bare binary".
        format: normalize_format(format),
        files: files.unwrap_or_default(),
        replacements,
        version_prefix: branch.version_prefix.clone(),
    }))
}

/// aqua's `format: raw` means "no archive / bare binary" → empty format token.
fn normalize_format(format: Option<String>) -> String {
    match format {
        Some(f) if f == "raw" => String::new(),
        Some(f) => f,
        None => String::new(),
    }
}

/// Whether (goos, goarch) is in `supported_envs`. Empty list ⇒ all supported
/// (aqua default). Entries: `all` / `<goos>` / `<goarch>` / `<goos>/<goarch>`.
pub fn env_supported(supported_envs: &[String], goos: &str, goarch: &str) -> bool {
    if supported_envs.is_empty() {
        return true;
    }
    let pair = format!("{goos}/{goarch}");
    supported_envs.iter().any(|e| {
        e == "all" || e == goos || e == goarch || e == &pair
    })
}

/// Pick the matching platform override with **specificity-first** priority
/// (plan §7): `goos+goarch` > `goos`-only > (goarch-only) > unconstrained;
/// list order only as a tiebreaker within the same specificity.
fn pick_override<'a>(
    overrides: &'a [PlatformOverride],
    goos: &str,
    goarch: &str,
) -> Option<&'a PlatformOverride> {
    // Score: higher = more specific. -1 = does not apply.
    fn score(ov: &PlatformOverride, goos: &str, goarch: &str) -> i32 {
        let os_ok = ov.goos.as_deref().map(|g| g == goos);
        let arch_ok = ov.goarch.as_deref().map(|a| a == goarch);
        match (os_ok, arch_ok) {
            // both specified and match → most specific
            (Some(true), Some(true)) => 3,
            (Some(true), None) => 2,       // goos-only
            (None, Some(true)) => 1,       // goarch-only
            (None, None) => 0,             // unconstrained
            // any explicit mismatch → does not apply
            (Some(false), _) | (_, Some(false)) => -1,
        }
    }

    let mut best: Option<(&PlatformOverride, i32)> = None;
    for ov in overrides {
        let s = score(ov, goos, goarch);
        if s < 0 {
            continue;
        }
        match best {
            // strictly greater specificity wins; equal keeps the earlier (list order)
            Some((_, bs)) if s <= bs => {}
            _ => best = Some((ov, s)),
        }
    }
    best.map(|(ov, _)| ov)
}

// ---- version constraint evaluation ----

/// Evaluate an aqua `version_constraint` we understand. Supports:
///   * `semver("<op> X")`   where op ∈ < <= > >= ==
///   * `Version <op> "vX"`  where op ∈ < <= > >= ==
///
/// Anything else → error (caller degrades).
pub fn eval_constraint(constraint: &str, latest: &str) -> Result<bool> {
    let c = constraint.trim();
    if let Some(inner) = c.strip_prefix("semver(").and_then(|s| s.strip_suffix(')')) {
        let inner = inner.trim().trim_matches('"').trim();
        return eval_op_version(inner, latest);
    }
    if let Some(rest) = c.strip_prefix("Version") {
        // `Version <op> "vX"`
        let rest = rest.trim();
        return eval_op_version(rest, latest);
    }
    bail!("unsupported version_constraint `{constraint}`");
}

/// Evaluate `"<op> X"` (op then version, whitespace-separated, version may be
/// quoted) against `latest`.
fn eval_op_version(expr: &str, latest: &str) -> Result<bool> {
    let expr = expr.trim();
    // Longest ops first so `<=`/`>=`/`==` beat `<`/`>`.
    let (op, rhs) = if let Some(r) = expr.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = expr.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = expr.strip_prefix("==") {
        ("==", r)
    } else if let Some(r) = expr.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = expr.strip_prefix('>') {
        (">", r)
    } else {
        bail!("unsupported comparison in version_constraint `{expr}`");
    };
    let rhs = rhs.trim().trim_matches('"').trim();
    let ord = cmp_semver(latest, rhs);
    Ok(match op {
        "<" => ord == std::cmp::Ordering::Less,
        "<=" => ord != std::cmp::Ordering::Greater,
        ">" => ord == std::cmp::Ordering::Greater,
        ">=" => ord != std::cmp::Ordering::Less,
        "==" => ord == std::cmp::Ordering::Equal,
        _ => unreachable!(),
    })
}

/// Lightweight semver compare (plan §7): strip a leading `v`, split on `.`,
/// compare numeric parts; ignore pre-release refinements. Missing parts = 0.
pub fn cmp_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let pa = numeric_parts(a);
    let pb = numeric_parts(b);
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// Extract the leading numeric dotted parts (`v2.65.0-rc.1` → [2,65,0]).
fn numeric_parts(s: &str) -> Vec<u64> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    // Stop at the first non-numeric/non-dot boundary in each part.
    s.split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect()
}

/// Strip `version_prefix` (if present) then a leading `v` (`trimV`) — the plan
/// §7 order that avoids double-stripping. Returns the value for `.Version`.
pub fn version_for_template(tag: &str, version_prefix: Option<&str>) -> String {
    let stripped = match version_prefix {
        Some(p) if !p.is_empty() => tag.strip_prefix(p).unwrap_or(tag),
        _ => tag,
    };
    super::template::trim_v(stripped).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX: &str = include_str!("../../tests/fixtures/aqua/openai_codex.yaml");
    const GH: &str = include_str!("../../tests/fixtures/aqua/cli_cli.yaml");

    fn parse(s: &str) -> Package {
        let reg: super::super::schema::Registry = serde_yml::from_str(s).unwrap();
        reg.packages.into_iter().next().unwrap()
    }

    #[test]
    fn cmp_semver_basic() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_semver("v2.65.0", "2.20.0"), Greater);
        assert_eq!(cmp_semver("2.20.0", "2.20.0"), Equal);
        assert_eq!(cmp_semver("v0.4.0", "0.5.2"), Less);
        // Missing parts treated as 0.
        assert_eq!(cmp_semver("2", "2.0.0"), Equal);
    }

    #[test]
    fn eval_semver_op_forms() {
        assert!(eval_constraint(r#"semver("<= 2.20.0")"#, "1.0.0").unwrap());
        assert!(!eval_constraint(r#"semver("<= 2.20.0")"#, "2.65.0").unwrap());
        assert!(eval_constraint(r#"semver("< 2.50.0")"#, "2.49.0").unwrap());
    }

    #[test]
    fn eval_version_eq_form() {
        assert!(eval_constraint(r#"Version == "v0.11.0""#, "0.11.0").unwrap());
        assert!(!eval_constraint(r#"Version == "v0.11.0""#, "0.12.0").unwrap());
    }

    #[test]
    fn unknown_constraint_errors() {
        assert!(eval_constraint(r#"semverWithVersion("x")"#, "1.0.0").is_err());
    }

    #[test]
    fn select_true_branch_for_codex() {
        let pkg = parse(CODEX);
        // codex latest (rust-stripped) is high → the `"true"` branch (last) wins.
        let branch = select_branch(&pkg, "0.20.0", "openai", "codex").unwrap();
        assert_eq!(branch.format.as_deref(), Some("zst"));
        assert_eq!(branch.version_prefix.as_deref(), Some("rust-"));
        // `"true"` present → picked outright regardless of the earlier semver branch.
        assert!(branch.replacements.contains_key("linux"));
    }

    #[test]
    fn select_semver_fallback_when_no_true() {
        // Synthesize a package with only comparison branches.
        let yaml = r#"
packages:
  - type: github_release
    repo_owner: x
    repo_name: y
    version_overrides:
      - version_constraint: semver("<= 1.0.0")
        asset: old-{{.OS}}
        format: raw
      - version_constraint: semver("<= 2.0.0")
        asset: mid-{{.OS}}
        format: raw
"#;
        let pkg = parse(yaml);
        // latest 1.5.0 → first branch (<=1.0.0) fails, second (<=2.0.0) matches.
        let branch = select_branch(&pkg, "1.5.0", "x", "y").unwrap();
        assert_eq!(branch.asset.as_deref(), Some("mid-{{.OS}}"));
    }

    #[test]
    fn select_no_branch_bails() {
        let yaml = r#"
packages:
  - type: github_release
    repo_owner: x
    repo_name: y
    version_overrides:
      - version_constraint: semver("<= 1.0.0")
        asset: old
        format: raw
"#;
        let pkg = parse(yaml);
        let err = select_branch(&pkg, "9.9.9", "x", "y").unwrap_err();
        assert!(err.to_string().contains("no evaluable version branch"), "{err}");
        assert!(err.to_string().contains("registry.yaml"), "{err}");
    }

    #[test]
    fn merge_specificity_goos_arch_beats_goos_only_regardless_of_order() {
        // goos-only listed BEFORE goos+goarch → the more specific one must win.
        let overrides = vec![
            PlatformOverride {
                goos: Some("linux".into()),
                format: Some("tar.gz".into()),
                ..Default::default()
            },
            PlatformOverride {
                goos: Some("linux".into()),
                goarch: Some("arm64".into()),
                format: Some("tar.xz".into()),
                ..Default::default()
            },
        ];
        let picked = pick_override(&overrides, "linux", "arm64").unwrap();
        assert_eq!(picked.format.as_deref(), Some("tar.xz"));
        // linux/amd64 has no goos+goarch match → falls to goos-only.
        let picked2 = pick_override(&overrides, "linux", "amd64").unwrap();
        assert_eq!(picked2.format.as_deref(), Some("tar.gz"));
    }

    #[test]
    fn gh_linux_override_switches_format_to_tar_gz() {
        let pkg = parse(GH);
        let branch = select_branch(&pkg, "2.65.0", "cli", "cli").unwrap();
        // base/branch format is zip; linux override → tar.gz.
        let linux = effective_for(&branch, "linux", "amd64").unwrap().unwrap();
        assert_eq!(linux.format, "tar.gz");
        // darwin keeps zip (no override) and replacements map darwin→macOS.
        let darwin = effective_for(&branch, "darwin", "amd64").unwrap().unwrap();
        assert_eq!(darwin.format, "zip");
        assert_eq!(darwin.replacements.get("darwin").map(String::as_str), Some("macOS"));
    }

    #[test]
    fn supported_envs_gating() {
        assert!(env_supported(&[], "linux", "amd64")); // empty = all
        assert!(env_supported(&["all".into()], "linux", "amd64"));
        assert!(env_supported(&["linux".into()], "linux", "arm64"));
        assert!(env_supported(&["amd64".into()], "darwin", "amd64"));
        assert!(env_supported(&["linux/amd64".into()], "linux", "amd64"));
        assert!(!env_supported(&["linux/amd64".into()], "linux", "arm64"));
        assert!(!env_supported(&["darwin".into()], "linux", "amd64"));
    }

    #[test]
    fn no_asset_makes_platform_unavailable() {
        let branch = Branch {
            asset: Some("x-{{.OS}}".into()),
            format: Some("raw".into()),
            overrides: vec![PlatformOverride {
                goos: Some("linux".into()),
                no_asset: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(effective_for(&branch, "linux", "amd64").unwrap().is_none());
        // darwin unaffected.
        assert!(effective_for(&branch, "darwin", "amd64").unwrap().is_some());
    }

    #[test]
    fn branch_level_no_asset_makes_branch_unavailable() {
        // A version_override branch with `no_asset: true` must NOT inherit the
        // base asset; every platform is unavailable.
        let yaml = r#"
packages:
  - type: github_release
    repo_owner: x
    repo_name: y
    asset: base-{{.OS}}
    format: raw
    version_overrides:
      - version_constraint: "true"
        no_asset: true
"#;
        let pkg = parse(yaml);
        let branch = select_branch(&pkg, "1.0.0", "x", "y").unwrap();
        assert!(branch.no_asset);
        assert!(effective_for(&branch, "linux", "amd64").unwrap().is_none());
        assert!(effective_for(&branch, "darwin", "arm64").unwrap().is_none());
    }

    #[test]
    fn version_for_template_strips_prefix_then_v() {
        // codex: rust-v0.20.0 → strip `rust-` → v0.20.0 → trimV → 0.20.0
        assert_eq!(version_for_template("rust-v0.20.0", Some("rust-")), "0.20.0");
        // no prefix: plain trimV.
        assert_eq!(version_for_template("v2.65.0", None), "2.65.0");
        // prefix that already has no v after it.
        assert_eq!(version_for_template("cli-2.0.0", Some("cli-")), "2.0.0");
    }
}
