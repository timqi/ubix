//! Checksum discovery & verification (§8.8, M6).
//!
//! The pure, network-free pieces: given a checksum sidecar file's contents and
//! the target asset name, extract the expected sha256; and verify a computed
//! digest against it. Discovery order (§8.8): `<asset>.sha256` → `.sha256sum` →
//! combined `checksums.txt` / `SHA256SUMS`. When none is found, callers record
//! `checksum = "none"` (non-fatal).

use anyhow::{bail, Result};

/// Candidate sidecar file names to try, in priority order, for `asset` (§8.8):
/// `<asset>.sha256` → `<asset>.sha256sum` → combined `checksums.txt` / `SHA256SUMS`.
pub fn candidate_names(asset: &str) -> Vec<String> {
    vec![
        format!("{asset}.sha256"),
        format!("{asset}.sha256sum"),
        "checksums.txt".to_string(),
        "SHA256SUMS".to_string(),
    ]
}

/// Parse a sidecar file's contents to find the sha256 for `asset`.
///
/// Handles both single-hash files (`<hash>` or `<hash>  <name>`) and combined
/// files with one `"<hash>  <name>"` line per asset. Matching is by exact
/// basename of the second column, or — for single-line files with no name — the
/// lone hash is returned.
pub fn extract_for_asset(contents: &str, asset: &str) -> Option<String> {
    let lines: Vec<&str> = contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    // Combined / named form: `<hash>  <name>` (one or two spaces, or `*name`).
    for line in &lines {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        // The name may be prefixed with `*` (binary mode) or a path.
        let name = name.trim_start_matches('*');
        let base = name.rsplit('/').next().unwrap_or(name);
        if base == asset && is_hex_sha256(hash) {
            return Some(hash.to_ascii_lowercase());
        }
    }

    // Single bare hash (a `<asset>.sha256` with just the digest and no name).
    if lines.len() == 1 {
        let mut toks = lines[0].split_whitespace();
        let first = toks.next().unwrap_or("");
        // Only treat as a bare hash when there is no second (name) column;
        // otherwise a named line whose name did not match must NOT be returned.
        if toks.next().is_none() && is_hex_sha256(first) {
            return Some(first.to_ascii_lowercase());
        }
    }

    None
}

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Verify `computed` (lowercase hex) equals `expected` (case-insensitive).
pub fn verify(expected: &str, computed: &str) -> Result<()> {
    if expected.eq_ignore_ascii_case(computed) {
        Ok(())
    } else {
        bail!("checksum mismatch: expected {expected}, got {computed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn candidate_order() {
        let c = candidate_names("eza.tar.gz");
        assert_eq!(c[0], "eza.tar.gz.sha256");
        assert_eq!(c[1], "eza.tar.gz.sha256sum");
        assert_eq!(c[2], "checksums.txt");
        assert_eq!(c[3], "SHA256SUMS");
    }

    #[test]
    fn bare_single_hash() {
        assert_eq!(extract_for_asset(H, "eza.tar.gz").as_deref(), Some(H));
    }

    #[test]
    fn hash_with_name() {
        let body = format!("{H}  eza.tar.gz\n");
        assert_eq!(extract_for_asset(&body, "eza.tar.gz").as_deref(), Some(H));
    }

    #[test]
    fn combined_file_picks_matching_line() {
        let body = format!(
            "{H}  other.tar.gz\n{H}  eza.tar.gz\n0000  ignored\n"
        );
        assert_eq!(extract_for_asset(&body, "eza.tar.gz").as_deref(), Some(H));
    }

    #[test]
    fn binary_mode_star_and_path_prefix() {
        let body = format!("{H} *dist/eza.tar.gz\n");
        assert_eq!(extract_for_asset(&body, "eza.tar.gz").as_deref(), Some(H));
    }

    #[test]
    fn no_match_returns_none() {
        let body = format!("{H}  something-else.tar.gz\n");
        assert_eq!(extract_for_asset(&body, "eza.tar.gz"), None);
    }

    #[test]
    fn none_sentinel_when_empty() {
        assert_eq!(extract_for_asset("", "x"), None);
    }

    #[test]
    fn verify_ok_and_mismatch() {
        assert!(verify(H, H).is_ok());
        assert!(verify(H, &H.to_uppercase()).is_ok());
        assert!(verify(H, "deadbeef").is_err());
    }
}
