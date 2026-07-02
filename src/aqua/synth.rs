//! Turn resolved+rendered per-platform data into a [`ToolConfig`] (plan §5/§6).
//!
//! The hard part is the `matching` value for version-in-asset templates: ubi's
//! `.matching()` is a fixed case-sensitive substring, so we must produce a
//! VERSION-INDEPENDENT fragment (plan §5). We never emit an empty string (that
//! would silently disable filtering and fall back to ubi heuristics).

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::config::{PlatformString, ToolConfig};

use super::resolve::{effective_for, registry_url, version_for_template, Branch};
use super::template::{self, asset_without_ext, Ctx};

/// The four target platforms we synthesize for (plan §6).
pub const PLATFORMS: &[(&str, &str)] = &[
    ("linux", "amd64"),
    ("linux", "arm64"),
    ("darwin", "amd64"),
    ("darwin", "arm64"),
];

/// One platform's synthesized data.
#[derive(Debug, Clone)]
struct Rendered {
    /// The version-independent `matching` substring for this platform.
    matching: String,
    /// files[].name → basename(rendered src) (for `rename` derivation).
    files: Vec<(String, String)>,
}

/// Synthesize a [`ToolConfig`] for `owner/repo` from the selected `branch` and
/// resolved-latest `tag`, over all supported linux/darwin platforms.
///
/// `name_override` sets the config key/exe name; otherwise the first file's
/// `name` is used, falling back to `repo`.
pub fn synth(
    branch: &Branch,
    tag: &str,
    owner: &str,
    repo: &str,
    name_override: Option<&str>,
) -> Result<(String, ToolConfig)> {
    // Reject non-github_release types surfaced by the winning branch.
    if let Some(t) = &branch.type_ {
        if t != "github_release" {
            bail!(
                "unsupported aqua construct: type `{t}` for {owner}/{repo}; \
                 see {} and add a `github:` entry manually",
                registry_url(owner, repo)
            );
        }
    }

    let mut matching_map: BTreeMap<String, String> = BTreeMap::new();
    // Files from the first successfully-rendered platform decide exe/exes/rename.
    let mut file_names: Option<Vec<(String, String)>> = None;

    for (goos, goarch) in PLATFORMS {
        let Some(eff) = effective_for(branch, goos, goarch)? else {
            continue; // unsupported / no_asset → skip this key
        };
        let version = version_for_template(tag, eff.version_prefix.as_deref());
        let rendered = render_platform(&eff, goos, goarch, &version, owner, repo)?;
        matching_map.insert(format!("{goos}-{goarch}"), rendered.matching);
        if file_names.is_none() && !rendered.files.is_empty() {
            file_names = Some(rendered.files);
        }
    }

    if matching_map.is_empty() {
        bail!(
            "unsupported aqua construct: no supported linux/darwin platform for {owner}/{repo}; \
             see {} and add a `github:` entry manually",
            registry_url(owner, repo)
        );
    }

    let files = file_names.unwrap_or_default();
    let name = name_override
        .map(str::to_string)
        .or_else(|| files.first().map(|(n, _)| n.clone()))
        .unwrap_or_else(|| repo.to_string());

    let mut tool = ToolConfig::from_spec(format!("github:{owner}/{repo}"));
    tool.matching = Some(PlatformString::PerPlatform(matching_map));

    match files.len() {
        0 => {}
        1 => {
            let (fname, src_base) = &files[0];
            tool.exe = Some(fname.clone());
            // rename when the in-archive basename differs from the wanted name.
            if src_base != fname {
                tool.rename = Some(fname.clone());
            }
        }
        _ => {
            // Multi-file → exes (mutually exclusive with rename, per github source).
            tool.exes = Some(files.iter().map(|(n, _)| n.clone()).collect());
        }
    }

    Ok((name, tool))
}

/// Render asset + files for one platform, and derive the matching substring.
fn render_platform(
    eff: &super::resolve::Effective,
    goos: &str,
    goarch: &str,
    version: &str,
    owner: &str,
    repo: &str,
) -> Result<Rendered> {
    // Apply replacements to the OS/Arch/Format tokens (plan §7).
    let os = replaced(&eff.replacements, goos);
    let arch = replaced(&eff.replacements, goarch);
    let format = if eff.format.is_empty() {
        String::new()
    } else {
        replaced(&eff.replacements, &eff.format)
    };

    let mut ctx = Ctx {
        os,
        arch,
        format: format.clone(),
        version: version.to_string(),
        asset_without_ext: None,
    };

    let rendered_asset = template::render(&eff.asset, &ctx, "asset")?;
    ctx.asset_without_ext = Some(asset_without_ext(&rendered_asset, &format));

    // files[].src rendered (may reference .AssetWithoutExt / nested paths).
    let mut files: Vec<(String, String)> = Vec::new();
    for f in &eff.files {
        let Some(name) = &f.name else { continue };
        let src = match &f.src {
            Some(s) => template::render(s, &ctx, "files.src")?,
            None => name.clone(),
        };
        let base = basename(&src).to_string();
        files.push((name.clone(), base));
    }

    let matching = matching_substring(&eff.asset, &rendered_asset, version, owner, repo)?;
    Ok(Rendered { matching, files })
}

/// Compute the version-independent matching substring (plan §5).
///
/// * No `{{...Version...}}` token in the *template* → the full rendered asset.
/// * Version token present → the rendered fragment AFTER the last version token
///   (the version-independent tail). If empty, fall back to the longest literal
///   BEFORE the first version token. If neither is a usable (non-separator)
///   fragment → bail (degrade); never emit an empty string.
fn matching_substring(
    asset_template: &str,
    rendered_asset: &str,
    version: &str,
    owner: &str,
    repo: &str,
) -> Result<String> {
    if !template_has_version(asset_template) {
        return Ok(rendered_asset.to_string());
    }

    // The rendered version string appears in the rendered asset; split on it.
    // Use the LAST occurrence for the tail, the FIRST for the prefix.
    let tail = rendered_asset
        .rsplit_once(version)
        .map(|(_, t)| t.to_string())
        .unwrap_or_default();
    if is_usable_fragment(&tail) {
        return Ok(tail);
    }

    let prefix = rendered_asset
        .split_once(version)
        .map(|(p, _)| p.to_string())
        .unwrap_or_default();
    if is_usable_fragment(&prefix) {
        return Ok(prefix);
    }

    bail!(
        "unsupported aqua construct: cannot derive a unique version-independent asset \
         fragment for {owner}/{repo} (rendered `{rendered_asset}`); \
         see {} and add a `github:` entry manually",
        registry_url(owner, repo)
    );
}

/// A fragment is usable if it has at least one alphanumeric character (i.e. it
/// is more than just separators like `_`/`-`/`.`).
fn is_usable_fragment(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphanumeric())
}

/// Whether the asset TEMPLATE contains a `{{ … Version … }}` action.
fn template_has_version(template: &str) -> bool {
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        if after[..end].contains("Version") {
            return true;
        }
        rest = &after[end + 2..];
    }
    false
}

fn replaced(map: &BTreeMap<String, String>, token: &str) -> String {
    map.get(token).cloned().unwrap_or_else(|| token.to_string())
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aqua::resolve::select_branch;
    use crate::aqua::schema::Registry;

    const CODEX: &str = include_str!("../../tests/fixtures/aqua/openai_codex.yaml");
    const GH: &str = include_str!("../../tests/fixtures/aqua/cli_cli.yaml");

    fn pkg(s: &str) -> crate::aqua::schema::Package {
        let reg: Registry = serde_yml::from_str(s).unwrap();
        reg.packages.into_iter().next().unwrap()
    }

    #[test]
    fn synth_codex_exact_matching_and_rename() {
        let p = pkg(CODEX);
        let branch = select_branch(&p, "0.20.0", "openai", "codex").unwrap();
        let (name, tool) = synth(&branch, "rust-v0.20.0", "openai", "codex", None).unwrap();
        assert_eq!(name, "codex");
        assert_eq!(tool.spec, "github:openai/codex");
        let m = match &tool.matching {
            Some(PlatformString::PerPlatform(m)) => m,
            _ => panic!("expected per-platform matching"),
        };
        // Plan §6 exact values (no version token → full rendered asset name).
        assert_eq!(m["linux-amd64"], "codex-x86_64-unknown-linux-musl.zst");
        assert_eq!(m["linux-arm64"], "codex-aarch64-unknown-linux-musl.zst");
        assert_eq!(m["darwin-amd64"], "codex-x86_64-apple-darwin.zst");
        assert_eq!(m["darwin-arm64"], "codex-aarch64-apple-darwin.zst");
        // exe = codex; files.src is `{{.AssetWithoutExt}}` → basename differs → rename.
        assert_eq!(tool.exe.as_deref(), Some("codex"));
        assert_eq!(tool.rename.as_deref(), Some("codex"));
    }

    #[test]
    fn synth_gh_version_fragment_and_darwin_macos() {
        let p = pkg(GH);
        let branch = select_branch(&p, "2.65.0", "cli", "cli").unwrap();
        let (name, tool) = synth(&branch, "v2.65.0", "cli", "cli", None).unwrap();
        assert_eq!(name, "gh");
        let m = match &tool.matching {
            Some(PlatformString::PerPlatform(m)) => m,
            _ => panic!("expected per-platform matching"),
        };
        // Plan §6 exact values: version-independent tails.
        assert_eq!(m["linux-amd64"], "_linux_amd64.tar.gz");
        assert_eq!(m["linux-arm64"], "_linux_arm64.tar.gz");
        assert_eq!(m["darwin-amd64"], "_macOS_amd64.zip");
        assert_eq!(m["darwin-arm64"], "_macOS_arm64.zip");
        // exe = gh; base files.src is `.../bin/gh` → basename `gh` == name → no rename.
        assert_eq!(tool.exe.as_deref(), Some("gh"));
        assert_eq!(tool.rename, None);
    }

    #[test]
    fn matching_no_version_uses_full_asset() {
        let s = matching_substring("foo-{{.OS}}-{{.Arch}}", "foo-linux-amd64", "1.0.0", "o", "r").unwrap();
        assert_eq!(s, "foo-linux-amd64");
    }

    #[test]
    fn matching_version_tail() {
        let s = matching_substring(
            "gh_{{trimV .Version}}_{{.OS}}_{{.Arch}}.{{.Format}}",
            "gh_2.65.0_linux_amd64.tar.gz",
            "2.65.0",
            "o",
            "r",
        )
        .unwrap();
        assert_eq!(s, "_linux_amd64.tar.gz");
    }

    #[test]
    fn matching_version_at_end_falls_back_to_prefix() {
        // tool_{{.OS}}_{{trimV .Version}} → tail empty → use prefix `tool_linux_`.
        let s = matching_substring(
            "tool_{{.OS}}_{{trimV .Version}}",
            "tool_linux_2.65.0",
            "2.65.0",
            "o",
            "r",
        )
        .unwrap();
        assert_eq!(s, "tool_linux_");
    }

    #[test]
    fn matching_only_separators_bails() {
        // version in the middle, both sides only separators → degrade.
        let err = matching_substring("{{trimV .Version}}-", "2.65.0-", "2.65.0", "o", "r")
            .unwrap_err();
        assert!(err.to_string().contains("cannot derive"), "{err}");
    }

    #[test]
    fn name_override_applies() {
        let p = pkg(GH);
        let branch = select_branch(&p, "2.65.0", "cli", "cli").unwrap();
        let (name, _) = synth(&branch, "v2.65.0", "cli", "cli", Some("ghcli")).unwrap();
        assert_eq!(name, "ghcli");
    }
}
