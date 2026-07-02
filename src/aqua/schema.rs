//! Serde structs for the subset of aqua `registry.yaml` we consume (plan §3).
//!
//! Everything is lenient: unknown fields are ignored and every field is
//! `#[serde(default)]` so partial/older registry entries still parse. We only
//! model what the generator uses; the many aqua fields we don't (checksum,
//! rosetta2, windows_arm_emulation, github_artifact_attestations, …) are simply
//! dropped.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Top-level registry document: a list of `packages`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Registry {
    #[serde(default)]
    pub packages: Vec<Package>,
}

/// One package entry. Shared/base fields live here; version- and
/// platform-specific tweaks live in `version_overrides` / `overrides`.
///
/// Some fields (e.g. top-level `version_constraint`, override `type_`) are
/// parsed for completeness/leniency but not all are consulted by the current
/// generator; they are kept so the structs mirror the registry schema.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Package {
    /// aqua package type; we only support `github_release` (else degrade).
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    #[serde(default)]
    pub repo_owner: Option<String>,
    #[serde(default)]
    pub repo_name: Option<String>,

    // ---- fields that flow through the base ⊕ branch ⊕ override merge ----
    #[serde(default)]
    pub asset: Option<String>,
    /// `type: http` templated download URL (aqua Go-template). Consulted only by
    /// the `template:` hint for http packages; the github_release path ignores it.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub files: Option<Vec<FileEntry>>,
    #[serde(default)]
    pub replacements: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub supported_envs: Option<Vec<String>>,
    #[serde(default)]
    pub version_prefix: Option<String>,
    /// Where aqua discovers the version (e.g. `github_tag`, `github_release`).
    /// Used by the http `template:` hint to fill `--version-source`.
    #[serde(default)]
    pub version_source: Option<String>,

    /// Top-level branch selector; usually `"false"` when version_overrides carry
    /// the real branches, but may be a real constraint on simple packages.
    #[serde(default)]
    pub version_constraint: Option<String>,
    #[serde(default)]
    pub version_overrides: Vec<VersionOverride>,
    #[serde(default)]
    pub overrides: Vec<PlatformOverride>,
}

/// A `files[]` entry: the produced command `name` and its `src` inside the
/// asset (may be a Go-template referencing `.AssetWithoutExt`, nested paths…).
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct FileEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
}

/// A `version_overrides[]` branch, gated by `version_constraint`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct VersionOverride {
    #[serde(default)]
    pub version_constraint: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub files: Option<Vec<FileEntry>>,
    #[serde(default)]
    pub replacements: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub supported_envs: Option<Vec<String>>,
    #[serde(default)]
    pub version_prefix: Option<String>,
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    #[serde(default)]
    pub overrides: Vec<PlatformOverride>,
}

/// An `overrides[]` entry: platform-scoped field tweaks. `goos`/`goarch`
/// select which platforms it applies to (specificity-first match, plan §7).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PlatformOverride {
    #[serde(default)]
    pub goos: Option<String>,
    #[serde(default)]
    pub goarch: Option<String>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub files: Option<Vec<FileEntry>>,
    #[serde(default)]
    pub replacements: Option<BTreeMap<String, String>>,
    /// `no_asset: true` → this platform has no downloadable asset (unavailable).
    #[serde(default)]
    pub no_asset: bool,
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX: &str = include_str!("../../tests/fixtures/aqua/openai_codex.yaml");
    const GH: &str = include_str!("../../tests/fixtures/aqua/cli_cli.yaml");

    #[test]
    fn parses_codex_fixture() {
        let reg: Registry = serde_yml::from_str(CODEX).unwrap();
        assert_eq!(reg.packages.len(), 1);
        let p = &reg.packages[0];
        assert_eq!(p.type_.as_deref(), Some("github_release"));
        assert_eq!(p.repo_owner.as_deref(), Some("openai"));
        assert_eq!(p.repo_name.as_deref(), Some("codex"));
        assert_eq!(p.version_prefix.as_deref(), Some("rust-"));
        assert_eq!(p.version_constraint.as_deref(), Some("false"));
        assert_eq!(p.version_overrides.len(), 2);
        // The `"true"` branch is last.
        assert_eq!(
            p.version_overrides.last().unwrap().version_constraint.as_deref(),
            Some("true")
        );
    }

    #[test]
    fn parses_gh_fixture_with_base_files_and_overrides() {
        let reg: Registry = serde_yml::from_str(GH).unwrap();
        let p = &reg.packages[0];
        // Base-level files present (nested src).
        let files = p.files.as_ref().unwrap();
        assert_eq!(files[0].name.as_deref(), Some("gh"));
        assert_eq!(files[0].src.as_deref(), Some("gh_{{trimV .Version}}_{{.OS}}_{{.Arch}}/bin/gh"));
        // The `"true"` branch carries a linux→tar.gz override and darwin replacement.
        let last = p.version_overrides.last().unwrap();
        assert_eq!(last.version_constraint.as_deref(), Some("true"));
        assert!(last.overrides.iter().any(|o| o.goos.as_deref() == Some("linux")
            && o.format.as_deref() == Some("tar.gz")));
        assert_eq!(last.replacements.as_ref().unwrap()["darwin"], "macOS");
    }

    #[test]
    fn ignores_unknown_fields() {
        // checksum/rosetta2/etc are present in the gh fixture and must be ignored.
        let reg: Registry = serde_yml::from_str(GH).unwrap();
        assert!(!reg.packages.is_empty());
    }
}
