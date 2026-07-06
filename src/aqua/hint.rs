//! Best-effort `url:` hint for aqua packages ubix can't synthesize as a
//! `github:` config (currently `type: http`).
//!
//! ubix can't install a `type: http` aqua package through the github_release
//! path, but its `url:` source covers the exact same shape — a templated
//! download URL plus version discovery (the canonical case is claude-code). So
//! instead of a dead-end "add a github: entry manually", we translate the
//! registry.yaml into a ready-to-paste `ubix add 'url:…'` command.

use std::collections::BTreeMap;

use super::resolve::registry_url;
use super::schema::{FileEntry, Package, PlatformOverride};
use crate::config::ToolConfig;

/// Runtime GOARCH tokens, used to split aqua `replacements` into arch
/// (`--arch-replace`) vs os (`--os-replace`) buckets.
const ARCH_TOKENS: &[&str] = &[
    "amd64", "arm64", "386", "arm", "riscv64", "ppc64le", "s390x", "loong64", "mips64le",
];

/// Translate aqua Go-template tokens to ubix template tokens (`{version}`,
/// `{os}`, `{arch}`). `trimV`/`trimPrefix` wrappers collapse to `{version}`
/// since ubix strips a leading `v` itself. Unknown `{{…}}` are left untouched so
/// the hint still shows the user what needs hand-editing.
fn to_ubix_template(s: &str) -> String {
    s.replace("{{trimV .Version}}", "{version}")
        .replace("{{.Version}}", "{version}")
        .replace("{{.OS}}", "{os}")
        .replace("{{.Arch}}", "{arch}")
}

/// The http fields extracted (base ⊕ chosen branch) for the hint.
#[derive(Default)]
struct HttpFields {
    url: Option<String>,
    url_musl: Option<String>,
    files: Vec<String>,
    /// True when some `files[].src` basename differs from its `name` — that needs
    /// a `rename` the `url:` synth can't express, so auto-synth bails to the hint.
    needs_rename: bool,
    arch_replace: BTreeMap<String, String>,
    os_replace: BTreeMap<String, String>,
    version_source: Option<String>,
}

/// A `files[]` entry needs a `rename` when the asset member (`src` basename)
/// differs from the produced command `name`. A bare `name` (no `src`, the
/// `format: raw` case) never renames.
fn file_needs_rename(f: &FileEntry) -> bool {
    match (&f.name, &f.src) {
        (Some(name), Some(src)) => src.rsplit('/').next().unwrap_or(src) != name,
        _ => false,
    }
}

/// Find a musl download URL among platform overrides (an override whose `url`
/// mentions `musl`). Returns the ubix-templated form.
fn musl_url(overrides: &[PlatformOverride]) -> Option<String> {
    overrides
        .iter()
        .filter_map(|o| o.url.as_deref())
        .find(|u| u.contains("musl"))
        .map(to_ubix_template)
}

/// Extract the http fields, applying base then the winning branch. We pick the
/// `version_constraint: "true"` override (aqua's "general" branch, e.g.
/// claude-code), else the last override carrying a url, else the base.
fn extract(pkg: &Package) -> HttpFields {
    // Start from the base package.
    let mut url = pkg.url.clone();
    let mut files = pkg.files.clone();
    let mut replacements = pkg.replacements.clone().unwrap_or_default();
    let mut overrides = pkg.overrides.clone();

    // Choose the branch that carries the templated URL.
    let branch = pkg
        .version_overrides
        .iter()
        .find(|v| v.version_constraint.as_deref() == Some("true"))
        .or_else(|| pkg.version_overrides.iter().rev().find(|v| v.url.is_some()));
    if let Some(vo) = branch {
        if vo.url.is_some() {
            url = vo.url.clone();
        }
        if vo.files.is_some() {
            files = vo.files.clone();
        }
        if let Some(r) = &vo.replacements {
            for (k, v) in r {
                replacements.insert(k.clone(), v.clone());
            }
        }
        if !vo.overrides.is_empty() {
            overrides = vo.overrides.clone();
        }
    }

    let mut arch_replace = BTreeMap::new();
    let mut os_replace = BTreeMap::new();
    for (k, v) in replacements {
        if ARCH_TOKENS.contains(&k.as_str()) {
            arch_replace.insert(k, v);
        } else {
            os_replace.insert(k, v);
        }
    }

    let file_entries = files.unwrap_or_default();
    let needs_rename = file_entries.iter().any(file_needs_rename);

    HttpFields {
        url: url.as_deref().map(to_ubix_template),
        url_musl: musl_url(&overrides),
        files: file_entries.into_iter().filter_map(|f| f.name).collect(),
        needs_rename,
        arch_replace,
        os_replace,
        version_source: pkg.version_source.clone(),
    }
}

/// Synthesize a `url:` [`ToolConfig`] from a `type: http` aqua package — the same
/// fields [`http_hint`] would print as an `ubix add 'url:…'` command, returned as
/// a config so `search --add` / `add aqua:` install it directly (mirroring the
/// `github:` synth path). Returns `None` when the package shape can't be
/// auto-synthesized safely; the caller then falls back to [`http_hint`]:
///
/// * no templated URL is extractable;
/// * the URL is version-gated with no catch-all (`version_constraint: "true"`)
///   branch — [`extract`] doesn't evaluate constraints, so it might pick a stale
///   branch for the current latest version;
/// * an asset member needs a `rename` the `url:` source can't express here.
pub fn synth_url_config(
    pkg: &Package,
    owner: &str,
    repo: &str,
    name_override: Option<&str>,
) -> Option<(String, ToolConfig)> {
    // Ambiguity guard: a URL only in version-gated branches (no `"true"`
    // catch-all) means `extract`'s "last branch with a url" heuristic may not
    // apply to the latest version. Defer to the manual hint rather than guess.
    // NOTE: this mirrors `extract`'s assumption that the `"true"` branch is
    // authoritative (in every real registry.yaml it's the last-declared branch,
    // used as the fallback); if a package ever placed `"true"` before a later
    // gated url, both would pick the catch-all — consistent, just position-blind.
    let has_catch_all = pkg
        .version_overrides
        .iter()
        .any(|v| v.version_constraint.as_deref() == Some("true"));
    if !has_catch_all && pkg.version_overrides.iter().any(|v| v.url.is_some()) {
        return None;
    }

    let f = extract(pkg);
    let url = f.url?;
    if f.needs_rename {
        return None;
    }

    let name = name_override.map(str::to_string).unwrap_or_else(|| match f.files.as_slice() {
        [one] => one.clone(),
        _ => repo.to_string(),
    });

    // Whether the URL needs rendering at all. A purely static URL (no
    // placeholders / musl variant / replacements) is a plain fixed-URL download:
    // leaving `version_source` UNSET keeps it on `url.rs`'s fixed-URL path (which
    // does sidecar-checksum discovery), instead of forcing the templated path
    // (which would make an unused version-discovery network call).
    let templated = url.contains('{')
        || f.url_musl.is_some()
        || !f.arch_replace.is_empty()
        || !f.os_replace.is_empty();

    let mut tool = ToolConfig::from_spec(format!("url:{url}"));
    match f.files.as_slice() {
        [] => {}
        [one] => tool.exe = Some(one.clone()),
        many => tool.exes = Some(many.to_vec()),
    }
    if templated {
        tool.version_source = Some(version_source_arg(f.version_source.as_deref(), owner, repo));
    }
    if !f.arch_replace.is_empty() {
        tool.arch_replace = Some(f.arch_replace);
    }
    if !f.os_replace.is_empty() {
        tool.os_replace = Some(f.os_replace);
    }
    tool.url_musl = f.url_musl;

    Some((name, tool))
}

/// Map aqua's `version_source` to a ubix `--version-source` value. aqua uses
/// `github_tag`/`github_release` (implicitly against repo_owner/repo_name), which
/// ubix expresses as `github:owner/repo`. Anything already namespaced (contains
/// `:`) passes through.
fn version_source_arg(raw: Option<&str>, owner: &str, repo: &str) -> String {
    match raw {
        Some(v) if v.contains(':') => v.to_string(),
        _ => format!("github:{owner}/{repo}"),
    }
}

/// Build the multi-line `type: http` hint. Always names the registry.yaml; when
/// a URL is extractable, appends a ready-to-paste `ubix add 'url:…'`.
pub fn http_hint(pkg: &Package, owner: &str, repo: &str) -> String {
    let f = extract(pkg);
    let reg = registry_url(owner, repo);

    let Some(url) = f.url else {
        // No URL extractable — fall back to a pointer at the registry + source.
        return format!(
            "`{owner}/{repo}` is an aqua `type: http` package (binary on a templated URL, \
             not a GitHub release), which `search` can't synthesize as `github:`.\n\
             ubix's `url:` source handles this shape — inspect the URL/files in\n  {reg}\n\
             then run `ubix add 'url:<url with {{version}}/{{os}}/{{arch}}>' …` \
             (see `ubix sources`)."
        );
    };

    // The command name / exe: prefer the produced file name(s).
    let (name, exe_line, exes_line) = match f.files.as_slice() {
        [] => (repo.to_string(), None, None),
        [one] => (
            one.clone(),
            Some(format!("  --exe {one} \\\n")),
            None,
        ),
        many => (
            repo.to_string(),
            None,
            Some(format!("  --exes {} \\\n", many.join(","))),
        ),
    };

    let mut cmd = format!("ubix add 'url:{url}' \\\n  --name {name} \\\n");
    if let Some(l) = exe_line {
        cmd.push_str(&l);
    }
    if let Some(l) = exes_line {
        cmd.push_str(&l);
    }
    cmd.push_str(&format!(
        "  --version-source {} \\\n",
        version_source_arg(f.version_source.as_deref(), owner, repo)
    ));
    for (k, v) in &f.arch_replace {
        cmd.push_str(&format!("  --arch-replace {k}={v} \\\n"));
    }
    for (k, v) in &f.os_replace {
        cmd.push_str(&format!("  --os-replace {k}={v} \\\n"));
    }
    if let Some(m) = &f.url_musl {
        cmd.push_str(&format!("  --url-musl '{m}' \\\n"));
    }
    // Drop the trailing ` \` continuation on the last line.
    let cmd = cmd.trim_end().trim_end_matches('\\').trim_end().to_string();

    format!(
        "`{owner}/{repo}` is an aqua `type: http` package (binary on a templated URL, \
         not a GitHub release), so `search` can't synthesize a `github:` config.\n\
         Use ubix's `url:` source instead — translated from the registry.yaml:\n\n\
         {cmd}\n\n\
         Verify/tweak against {reg}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"
packages:
  - type: http
    repo_owner: anthropics
    repo_name: claude-code
    files:
      - name: claude
    version_source: github_tag
    version_constraint: "false"
    version_overrides:
      - version_constraint: Version == "v2.1.88"
        no_asset: true
      - version_constraint: "true"
        format: raw
        url: https://storage.googleapis.com/x/claude-code-releases/{{trimV .Version}}/{{.OS}}-{{.Arch}}/claude
        files:
          - name: claude
        replacements:
          amd64: x64
        overrides:
          - goos: linux
            url: https://storage.googleapis.com/x/claude-code-releases/{{trimV .Version}}/{{.OS}}-{{.Arch}}-musl/claude
        supported_envs:
          - darwin
          - linux
"#;

    fn parse(s: &str) -> Package {
        let reg: super::super::schema::Registry = serde_yml::from_str(s).unwrap();
        reg.packages.into_iter().next().unwrap()
    }

    #[test]
    fn claude_code_generates_url_command() {
        let pkg = parse(CLAUDE);
        let hint = http_hint(&pkg, "anthropics", "claude-code");
        // Ready-to-paste url add with the ubix-tokenized URL.
        assert!(hint.contains(
            "ubix add 'url:https://storage.googleapis.com/x/claude-code-releases/{version}/{os}-{arch}/claude'"
        ), "{hint}");
        assert!(hint.contains("--name claude"), "{hint}");
        assert!(hint.contains("--exe claude"), "{hint}");
        assert!(hint.contains("--version-source github:anthropics/claude-code"), "{hint}");
        assert!(hint.contains("--arch-replace amd64=x64"), "{hint}");
        assert!(hint.contains(
            "--url-musl 'https://storage.googleapis.com/x/claude-code-releases/{version}/{os}-{arch}-musl/claude'"
        ), "{hint}");
        // No dangling line-continuation at the end of the command block.
        assert!(!hint.contains("\\\n\nVerify"), "{hint}");
    }

    #[test]
    fn no_url_falls_back_to_generic_pointer() {
        let pkg = parse(
            r#"
packages:
  - type: http
    repo_owner: x
    repo_name: y
"#,
        );
        let hint = http_hint(&pkg, "x", "y");
        assert!(hint.contains("url:"), "{hint}");
        assert!(hint.contains("registry.yaml") || hint.contains("aqua-registry"), "{hint}");
        // No concrete generated command (no resolved flags) — just guidance.
        assert!(!hint.contains("--version-source"), "{hint}");
    }

    #[test]
    fn claude_code_synthesizes_url_config() {
        let pkg = parse(CLAUDE);
        let (name, tool) = synth_url_config(&pkg, "anthropics", "claude-code", None).unwrap();
        assert_eq!(name, "claude");
        assert_eq!(
            tool.spec,
            "url:https://storage.googleapis.com/x/claude-code-releases/{version}/{os}-{arch}/claude"
        );
        assert_eq!(tool.exe.as_deref(), Some("claude"));
        assert_eq!(tool.version_source.as_deref(), Some("github:anthropics/claude-code"));
        assert_eq!(tool.arch_replace.as_ref().unwrap()["amd64"], "x64");
        assert_eq!(
            tool.url_musl.as_deref(),
            Some("https://storage.googleapis.com/x/claude-code-releases/{version}/{os}-{arch}-musl/claude")
        );
    }

    #[test]
    fn name_override_wins_over_file_name() {
        let pkg = parse(CLAUDE);
        let (name, _) = synth_url_config(&pkg, "anthropics", "claude-code", Some("cc")).unwrap();
        assert_eq!(name, "cc");
    }

    #[test]
    fn synth_bails_when_no_url() {
        let pkg = parse(
            r#"
packages:
  - type: http
    repo_owner: x
    repo_name: y
"#,
        );
        assert!(synth_url_config(&pkg, "x", "y", None).is_none());
    }

    #[test]
    fn static_url_omits_version_source() {
        // A placeholder-free URL is a plain fixed-URL download: no version_source
        // (keeps url.rs on the fixed-URL/sidecar-checksum path, no version query).
        let pkg = parse(
            r#"
packages:
  - type: http
    repo_owner: o
    repo_name: r
    url: https://example.com/download/r
    files:
      - name: r
    version_source: github_tag
"#,
        );
        let (name, tool) = synth_url_config(&pkg, "o", "r", None).unwrap();
        assert_eq!(name, "r");
        assert_eq!(tool.spec, "url:https://example.com/download/r");
        assert!(tool.version_source.is_none(), "{:?}", tool.version_source);
    }

    #[test]
    fn synth_bails_when_member_needs_rename() {
        // `src` basename differs from `name` → needs a rename url: can't express.
        let pkg = parse(
            r#"
packages:
  - type: http
    repo_owner: o
    repo_name: r
    url: https://h/{{.Version}}/{{.OS}}-{{.Arch}}.tar.gz
    files:
      - name: r
        src: dist/r-bin
    version_source: github_tag
"#,
        );
        assert!(synth_url_config(&pkg, "o", "r", None).is_none());
    }

    #[test]
    fn os_replacement_becomes_os_replace_flag() {
        let pkg = parse(
            r#"
packages:
  - type: http
    repo_owner: o
    repo_name: r
    url: https://h/{{.Version}}/{{.OS}}-{{.Arch}}/bin
    files:
      - name: bin
    replacements:
      darwin: macOS
      amd64: x86_64
"#,
        );
        let hint = http_hint(&pkg, "o", "r");
        assert!(hint.contains("--os-replace darwin=macOS"), "{hint}");
        assert!(hint.contains("--arch-replace amd64=x86_64"), "{hint}");
    }
}
