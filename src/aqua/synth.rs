//! Turn resolved+rendered per-platform data into a [`ToolConfig`] (plan §5/§6).
//!
//! The hard part is the `matching` value for version-in-asset templates: ubi's
//! `.matching()` is a fixed case-sensitive substring, so we must produce a
//! VERSION-INDEPENDENT fragment (plan §5). We never emit an empty string (that
//! would silently disable filtering and fall back to ubi heuristics).

use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::{PlatformString, ToolConfig};

use super::resolve::{effective_for, unsupported, version_for_template, Branch};
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
    /// files[].name → rendered src path (for alias dedup + `rename` derivation).
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
            return Err(unsupported(owner, repo, format!("type `{t}` for {owner}/{repo}")));
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
        return Err(unsupported(
            owner,
            repo,
            format!("no supported linux/darwin platform for {owner}/{repo}"),
        ));
    }

    let (files, dropped_aliases) = dedup_aliases(file_names.unwrap_or_default());
    let name = name_override
        .map(str::to_string)
        .or_else(|| files.first().map(|(n, _)| n.clone()))
        .unwrap_or_else(|| repo.to_string());

    let mut tool = ToolConfig::from_spec(format!("github:{owner}/{repo}"));
    tool.matching = Some(PlatformString::PerPlatform(matching_map));

    match files.len() {
        0 => {}
        1 => {
            let (fname, src) = &files[0];
            tool.exe = Some(fname.clone());
            // rename when the in-archive basename differs from the wanted name.
            if needs_rename(fname, src) {
                tool.rename = Some(fname.clone());
            }
        }
        _ => {
            // Multi-file → exes (mutually exclusive with rename, per github
            // source): `plan_exe_installs` looks each entry up by exact file
            // basename, so a name that differs from its archive member can't
            // be expressed here — degrade instead of emitting a config that
            // fails at install time.
            if let Some((fname, src)) = files.iter().find(|(n, s)| needs_rename(n, s)) {
                return Err(unsupported(
                    owner,
                    repo,
                    format!(
                        "multi-file package {owner}/{repo} wants `{fname}` renamed from \
                         archive member `{src}` (`exes` can't rename)"
                    ),
                ));
            }
            tool.exes = Some(files.iter().map(|(n, _)| n.clone()).collect());
        }
    }

    if !dropped_aliases.is_empty() {
        crate::step!(
            "omitting aqua alias exe(s) {} (extra link(s) to the same archive member)",
            dropped_aliases
                .iter()
                .map(|d| format!("`{d}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok((name, tool))
}

/// Whether producing command `name` from archive member `src` requires a
/// rename (the member's basename differs from the wanted name).
pub(super) fn needs_rename(name: &str, src: &str) -> bool {
    basename(src) != name
}

/// Collapse aqua alias entries: several `files[]` names may point at the SAME
/// in-archive member (e.g. claude-squad ships `cs` with `src: claude-squad`).
/// aqua materializes aliases as extra links at install time, but the archive
/// holds ONE real file — emitting every name as an `exes` entry fails at
/// install ("`cs` not found in extracted archive"). Keeps one entry per
/// rendered src (preferring the name that equals the member basename); the
/// second element returns the dropped alias names for reporting.
/// Also reused by `hint::extract` on unrendered (name, effective src) pairs.
pub(super) fn dedup_aliases(files: Vec<(String, String)>) -> (Vec<(String, String)>, Vec<String>) {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for (name, src) in files {
        match out.iter_mut().find(|(_, s)| *s == src) {
            None => out.push((name, src)),
            Some(kept) => {
                // Prefer keeping the name that matches the archive member.
                if name == basename(&src) {
                    dropped.push(std::mem::replace(&mut kept.0, name));
                } else {
                    dropped.push(name);
                }
            }
        }
    }
    (out, dropped)
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
        files.push((name.clone(), src));
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

    Err(unsupported(
        owner,
        repo,
        format!(
            "cannot derive a unique version-independent asset fragment for {owner}/{repo} \
             (rendered `{rendered_asset}`)"
        ),
    ))
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

    // smtg-ai/claude-squad shape: `cs` is an aqua ALIAS (`src: claude-squad`)
    // pointing at the one real archive member — not a second file.
    const SQUAD: &str = r#"
packages:
  - type: github_release
    repo_owner: smtg-ai
    repo_name: claude-squad
    asset: claude-squad_{{trimV .Version}}_{{.OS}}_{{.Arch}}.{{.Format}}
    format: tar.gz
    files:
      - name: claude-squad
      - name: cs
        src: claude-squad
"#;

    #[test]
    fn synth_collapses_alias_files_to_single_exe() {
        // Regression: previously synthesized exes = ["claude-squad", "cs"], and
        // install failed with "`cs` not found in extracted archive".
        let p = pkg(SQUAD);
        let branch = select_branch(&p, "1.0.5", "smtg-ai", "claude-squad").unwrap();
        let (name, tool) = synth(&branch, "v1.0.5", "smtg-ai", "claude-squad", None).unwrap();
        assert_eq!(name, "claude-squad");
        assert_eq!(tool.exe.as_deref(), Some("claude-squad"));
        assert_eq!(tool.exes, None);
        assert_eq!(tool.rename, None);
    }

    #[test]
    fn dedup_prefers_real_member_name_even_when_alias_listed_first() {
        let files = vec![
            ("cs".to_string(), "claude-squad".to_string()),
            ("claude-squad".to_string(), "claude-squad".to_string()),
        ];
        assert_eq!(
            dedup_aliases(files),
            (
                vec![("claude-squad".to_string(), "claude-squad".to_string())],
                vec!["cs".to_string()]
            )
        );
    }

    #[test]
    fn dedup_keeps_genuinely_distinct_files() {
        let files = vec![
            ("uv".to_string(), "uv-dist/uv".to_string()),
            ("uvx".to_string(), "uv-dist/uvx".to_string()),
        ];
        assert_eq!(dedup_aliases(files.clone()), (files, vec![]));
    }

    #[test]
    fn synth_bails_on_multi_file_rename() {
        // Two REAL files where one wants a name differing from its archive
        // member: `exes` can't rename, so degrade to the manual hint.
        let p = pkg(
            r#"
packages:
  - type: github_release
    repo_owner: o
    repo_name: r
    asset: r_{{.OS}}_{{.Arch}}.tar.gz
    files:
      - name: a
      - name: b
        src: dist/b-real
"#,
        );
        let branch = select_branch(&p, "1.0.0", "o", "r").unwrap();
        let err = synth(&branch, "v1.0.0", "o", "r", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsupported aqua construct"), "{msg}");
        assert!(msg.contains("`exes` can't rename"), "{msg}");
        assert!(msg.contains("registry.yaml"), "{msg}");
    }

    #[test]
    fn name_override_applies() {
        let p = pkg(GH);
        let branch = select_branch(&p, "2.65.0", "cli", "cli").unwrap();
        let (name, _) = synth(&branch, "v2.65.0", "cli", "cli", Some("ghcli")).unwrap();
        assert_eq!(name, "ghcli");
    }
}
