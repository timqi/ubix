//! Prune redundant per-platform `matching` from synthesized aqua configs.
//!
//! aqua's `registry.yaml` encodes an exact asset per platform, so `synth` always
//! emits a `matching` substring. But ubi already selects the right asset on its
//! own for most tools — the explicit `matching` is then just noise. This module
//! decides, per platform, whether `matching` is actually needed by SIMULATING
//! ubi's own asset picker against the real release asset list.
//!
//! The simulation is a faithful port of `ubi-0.9`'s `AssetPicker` pipeline
//! (`picker.rs`: OS → arch → libc → 64-bit → macOS-arm → alphabetical tiebreak),
//! restricted to the four platforms aqua/`synth` targets (linux/darwin ×
//! amd64/arm64). ubi does not expose the picker publicly, so the regexes are
//! copied verbatim from `ubi-0.9/src/{os,arch,picker}.rs` to preserve fidelity.
//!
//! ## Drop rule (per platform)
//! Drop `matching` when ubi, WITHOUT any matching, would pick an asset that is
//! viable for the platform (its name matches the platform OS). We check both
//! libc modes (glibc and musl) because a generated config is portable across
//! hosts. When ubi can't pick without matching, we keep `matching` only if
//! matching would actually rescue that mode; otherwise the mode is hopeless
//! regardless (e.g. a glibc-only tool on a musl host) and doesn't justify
//! keeping `matching`. A viable pick may differ from aqua's curated choice
//! (e.g. ubi picks the `gnu` build where aqua preferred `musl`) — both run, and
//! ubi does host-correct libc filtering, so this is safe.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

// ---- regexes ported verbatim from ubi-0.9 (os.rs / arch.rs / picker.rs) ----

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

re!(LINUX_RE, r"(?i:(?:\b|_)linux(?:static)?(?:\b|_|32|64))");
re!(MACOS_RE, r"(?i:(?:\b|_)(?:darwin|mac(?:osx?)?|osx)(?:\b|_))");
re!(ANDROID_RE, r"(?i:(?:\b|_)android(?:\b|_))");

re!(AARCH64_RE, r"(?ix)(?:\b|_)(?:aarch_?64|arm_?64)(?:\b|_)");
re!(
    X86_64_RE,
    r"(?ix)(?:\b|_)(?:386|i586|i686|x86[_-]32|x86[_-]64|x64|amd64|linux64|win64)(?:\b|_)"
);
re!(
    MACOS_AARCH64_AND_X86_64_RE,
    r"(?ix)(?:\b|_)(?:aarch_?64|arm_?64|arm|x86[_-]64|x64|amd64|all)(?:\b|_)"
);
re!(MACOS_AARCH64_ONLY_RE, r"(?ix)(?:\b|_)(?:aarch_?64|arm_?64|arm)(?:\b|_)");
re!(
    CPU_64_BIT_RE,
    r"(?ix)(?:\b|_)(?:x86[_-]?64|x64|amd64|linux64|win64|aarch[_-]?64|arm[_-]?64|mips[_-]?64|powerpc(?:le)?[_-]?64|ppc(?:le)?[_-]?64|riscv[_-]?64|s390x?[_-]?64|sparc[_-]?64)(?:\b|_)"
);
re!(GLIBC_RE, r"(?i:(?:\b|_)(?:gnu|glibc)(?:\b|_))");
re!(MUSL_RE, r"(?i:(?:\b|_)(?:alpine|musl)(?:\b|_))");

// Union of every arch ubi knows — used for the "matches a CPU arch that is not
// ours" fallback in arch_matches. Copied from ubi's ALL_ARCHES_RE members.
re!(
    ALL_ARCHES_RE,
    r"(?ix)(?:\b|_)(?:aarch_?64|arm_?64|arm(?:v[0-7])?|mips(?:el|le)|mips_?64(?:el|le)?|mips|powerpc(?:64)?(?:be|le)?_?6?4?|ppc(?:64)?(?:be|le)?_?6?4?|riscv(?:_?64)?|s390x?(?:_?64)?|sparc(?:_?64)?|386|i586|i686|x86[_-]32|x86[_-]64|x64|amd64|linux64|win32|win64)(?:\b|_)"
);

/// Extensions ubi never installs from — checksums/signatures/metadata plus OS
/// package formats (`.deb`/`.rpm`/`.msi`/`.dmg`/…) that aren't in ubi's
/// `Extension` enum and so are dropped by its extension filter. Removing them
/// keeps the simulated pick aligned with ubi's real choice (e.g. `gh` ships
/// `.deb`/`.rpm` beside the `.tar.gz`; ubi picks the tarball).
const COMPANION_EXTS: &[&str] = &[
    ".sha256", ".sha512", ".sha1", ".md5", ".asc", ".sig", ".pem", ".txt", ".sbom",
    ".json", ".sigstore", ".pubkey", ".minisig", ".cert", ".crt", ".pub", ".sha256sum",
    ".deb", ".rpm", ".msi", ".dmg", ".pkg", ".apk", ".exe", ".msix", ".snap", ".flatpak",
];

fn is_companion(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    COMPANION_EXTS.iter().any(|e| lower.ends_with(e)) || lower.contains("checksum")
}

fn os_re(goos: &str) -> &'static Regex {
    match goos {
        "darwin" => &MACOS_RE,
        _ => &LINUX_RE,
    }
}

fn is_macos_arm(goos: &str, goarch: &str) -> bool {
    goos == "darwin" && goarch == "arm64"
}

fn arch_re(goos: &str, goarch: &str) -> &'static Regex {
    if is_macos_arm(goos, goarch) {
        return &MACOS_AARCH64_AND_X86_64_RE;
    }
    match goarch {
        "arm64" => &AARCH64_RE,
        _ => &X86_64_RE, // amd64 (aqua only synths amd64/arm64)
    }
}

/// Simulate ubi's asset pick. Returns the picked asset name, or `None` when ubi
/// would fail to select (empty after a filter stage).
pub fn simulate_pick(
    assets: &[String],
    goos: &str,
    goarch: &str,
    is_musl: bool,
    matching: Option<&str>,
) -> Option<String> {
    // 1) extension filter (companions only — conservative subset of ubi's).
    let mut assets: Vec<String> = assets.iter().filter(|a| !is_companion(a)).cloned().collect();
    if assets.is_empty() {
        return None;
    }
    if assets.len() == 1 {
        return Some(assets.remove(0));
    }

    // 2) OS filter. aqua synths only linux/darwin, so an `android` token (which
    // the linux regex also matches) must be excluded.
    let os_matcher = os_re(goos);
    let os_matches: Vec<String> = assets
        .into_iter()
        .filter(|a| os_matcher.is_match(a) && !ANDROID_RE.is_match(a))
        .collect();
    if os_matches.is_empty() {
        return None;
    }

    // 3) arch filter (with ubi's single-asset + "no-arch fallback" behavior).
    let arch_matcher = arch_re(goos, goarch);
    let arch_matches: Vec<String> = if os_matches.len() == 1 {
        let only = &os_matches[0];
        if arch_matcher.is_match(only) || !ALL_ARCHES_RE.is_match(only) {
            vec![only.clone()]
        } else {
            vec![] // matches a different arch → reject
        }
    } else {
        let mut m: Vec<String> = os_matches.iter().filter(|a| arch_matcher.is_match(a)).cloned().collect();
        if m.is_empty() {
            // no arch match → keep assets that carry no arch token at all.
            m = os_matches.into_iter().filter(|a| !ALL_ARCHES_RE.is_match(a)).collect();
        }
        m
    };
    if arch_matches.is_empty() {
        return None;
    }

    // 4) libc filter (only meaningful on a musl target).
    let libc_matches = filter_libc(arch_matches, is_musl);
    if libc_matches.is_empty() {
        return None;
    }

    // 5) pick from remaining matches.
    pick_from_matches(libc_matches, goos, goarch, matching)
}

fn filter_libc(matches: Vec<String>, is_musl: bool) -> Vec<String> {
    if !is_musl {
        return matches;
    }
    let mut kept: Vec<String> = Vec::new();
    for a in &matches {
        if GLIBC_RE.is_match(a) {
            continue; // glibc asset is incompatible with a musl target
        }
        kept.push(a.clone()); // musl or libc-agnostic
    }
    if kept.len() > 1 {
        let musl_only: Vec<String> = kept.iter().filter(|a| MUSL_RE.is_match(a)).cloned().collect();
        if !musl_only.is_empty() {
            kept = musl_only;
        }
    }
    kept
}

fn pick_from_matches(
    mut matches: Vec<String>,
    goos: &str,
    goarch: &str,
    matching: Option<&str>,
) -> Option<String> {
    if matches.len() == 1 {
        return Some(matches.remove(0));
    }

    // matching substring filter.
    if let Some(m) = matching {
        matches.retain(|a| a.contains(m));
        if matches.is_empty() {
            return None;
        }
        if matches.len() == 1 {
            return Some(matches.remove(0));
        }
    }

    // macOS-arm: prefer the first arm64 binary outright.
    if is_macos_arm(goos, goarch) {
        if let Some(idx) = matches.iter().position(|a| MACOS_AARCH64_ONLY_RE.is_match(a)) {
            return Some(matches.remove(idx));
        }
    }

    // 64-bit filter (both amd64/arm64 targets are 64-bit).
    if matches.iter().any(|a| CPU_64_BIT_RE.is_match(a)) {
        let sixty: Vec<String> = matches.iter().filter(|a| a.contains("64")).cloned().collect();
        if !sixty.is_empty() {
            matches = sixty;
        }
    }
    if matches.len() == 1 {
        return Some(matches.remove(0));
    }

    // Deterministic tiebreak: alphabetical by name (ubi's behavior).
    matches.sort();
    matches.into_iter().next()
}

/// Whether `matching` is actually needed for `goos-goarch`, given the real asset
/// list. See the module docs for the rule.
pub fn matching_needed(assets: &[String], goos: &str, goarch: &str, aqua_matching: &str) -> bool {
    // No asset list to reason about → keep matching (safe default).
    if assets.is_empty() {
        return true;
    }
    // Only the four aqua-synth platforms are modeled; anything else → keep.
    let known = matches!(
        (goos, goarch),
        ("linux", "amd64") | ("linux", "arm64") | ("darwin", "amd64") | ("darwin", "arm64")
    );
    if !known {
        return true;
    }

    for is_musl in [false, true] {
        match simulate_pick(assets, goos, goarch, is_musl, None) {
            Some(name) if os_re(goos).is_match(&name) => {
                // Fine without matching in this mode.
            }
            _ => {
                // No viable pick without matching. Keep matching only if it
                // would rescue this mode; otherwise the mode is hopeless anyway.
                if let Some(name) = simulate_pick(assets, goos, goarch, is_musl, Some(aqua_matching)) {
                    if os_re(goos).is_match(&name) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Return the matching map keeping only the platforms that still need matching.
/// Keys not needing matching are dropped; a fully-redundant map returns empty.
pub fn prune_matching(
    map: &BTreeMap<String, String>,
    assets: &[String],
) -> BTreeMap<String, String> {
    map.iter()
        .filter(|(key, val)| {
            let (goos, goarch) = key.split_once('-').unwrap_or((key.as_str(), ""));
            matching_needed(assets, goos, goarch, val)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn glibc_host_picks_gnu_alphabetically() {
        // fd ships both gnu and musl on linux/amd64.
        let assets = v(&[
            "fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz",
            "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz",
            "fd-v10.2.0-aarch64-unknown-linux-gnu.tar.gz",
            "fd-v10.2.0-aarch64-unknown-linux-musl.tar.gz",
        ]);
        // glibc → gnu wins (alphabetical), musl target → musl.
        assert_eq!(
            simulate_pick(&assets, "linux", "amd64", false, None).as_deref(),
            Some("fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz")
        );
        assert_eq!(
            simulate_pick(&assets, "linux", "amd64", true, None).as_deref(),
            Some("fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz")
        );
    }

    #[test]
    fn fd_matching_is_redundant() {
        // Both libc modes yield a viable linux asset → matching not needed.
        let assets = v(&[
            "fd-v10.2.0-x86_64-unknown-linux-gnu.tar.gz",
            "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz",
            "fd-v10.2.0-aarch64-unknown-linux-gnu.tar.gz",
            "fd-v10.2.0-aarch64-unknown-linux-musl.tar.gz",
            "fd-v10.2.0-aarch64-apple-darwin.tar.gz",
            "fd-v10.2.0-x86_64-apple-darwin.tar.gz",
        ]);
        assert!(!matching_needed(&assets, "linux", "amd64", "-x86_64-unknown-linux-musl.tar.gz"));
        assert!(!matching_needed(&assets, "linux", "arm64", "-aarch64-unknown-linux-musl.tar.gz"));
        assert!(!matching_needed(&assets, "darwin", "arm64", "-aarch64-apple-darwin.tar.gz"));
    }

    #[test]
    fn gnu_only_tool_still_prunes() {
        // glibc mode picks gnu (viable); musl mode fails with OR without matching
        // (glibc filtered), so that mode is hopeless regardless → still prune.
        let assets = v(&[
            "tool-x86_64-unknown-linux-gnu.tar.gz",
            "tool-aarch64-unknown-linux-gnu.tar.gz",
        ]);
        assert!(!matching_needed(&assets, "linux", "amd64", "-x86_64-unknown-linux-gnu.tar.gz"));
    }

    #[test]
    fn single_asset_per_platform_prunes() {
        // codex-style: one asset per platform, distinct arch tokens.
        let assets = v(&[
            "codex-x86_64-unknown-linux-musl.zst",
            "codex-aarch64-unknown-linux-musl.zst",
            "codex-x86_64-apple-darwin.zst",
            "codex-aarch64-apple-darwin.zst",
        ]);
        assert!(!matching_needed(&assets, "linux", "amd64", "codex-x86_64-unknown-linux-musl.zst"));
        assert!(!matching_needed(&assets, "darwin", "arm64", "codex-aarch64-apple-darwin.zst"));
    }

    #[test]
    fn macos_arm_prefers_arm_over_x86() {
        let assets = v(&["app_darwin_amd64.tar.gz", "app_darwin_arm64.tar.gz"]);
        assert_eq!(
            simulate_pick(&assets, "darwin", "arm64", false, None).as_deref(),
            Some("app_darwin_arm64.tar.gz")
        );
    }

    #[test]
    fn companion_files_are_ignored() {
        let assets = v(&[
            "tool-linux-amd64.tar.gz",
            "tool-linux-amd64.tar.gz.sha256",
            "checksums.txt",
        ]);
        // Only one real asset for the platform → picks it, matching redundant.
        assert_eq!(
            simulate_pick(&assets, "linux", "amd64", false, None).as_deref(),
            Some("tool-linux-amd64.tar.gz")
        );
    }

    #[test]
    fn prune_matching_drops_all_when_redundant() {
        let assets = v(&[
            "codex-x86_64-unknown-linux-musl.zst",
            "codex-aarch64-unknown-linux-musl.zst",
            "codex-x86_64-apple-darwin.zst",
            "codex-aarch64-apple-darwin.zst",
        ]);
        let mut map = BTreeMap::new();
        map.insert("linux-amd64".into(), "codex-x86_64-unknown-linux-musl.zst".into());
        map.insert("darwin-arm64".into(), "codex-aarch64-apple-darwin.zst".into());
        assert!(prune_matching(&map, &assets).is_empty());
    }

    #[test]
    fn empty_asset_list_keeps_matching() {
        // No assets → simulate fails both modes, matching can't rescue → but the
        // caller treats an empty fetch as "keep" upstream; here matching_needed
        // returns false only when a viable pick exists, so verify it stays true.
        assert!(matching_needed(&[], "linux", "amd64", "-linux-musl.tar.gz"));
    }
}
