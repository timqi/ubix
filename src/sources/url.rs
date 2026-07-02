//! Direct-URL source (§5.2, M6): download an archive/binary, extract, select the
//! wanted executable(s), atomically install into `install_dir`. No `latest`
//! concept — the URL is a fixed version; we record the content sha256 so a later
//! change to the URL's bytes can be detected.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::archive;
use crate::config::ToolConfig;
use crate::engine::{atomic_install, sha256_file};
use crate::http::HttpClient;
use crate::sources::{parse_spec, InstallOutcome, SourceKind};

/// Select which extracted files to install and under what final names.
///
/// * `exes` (multi-entry) → pick each named file from the extracted set.
/// * else `exe`/default → pick the single named file (or the sole file).
///
/// Returns pairs of (source_path, final_name).
pub fn select_exes<'a>(
    extracted: &'a [PathBuf],
    exe: Option<&str>,
    exes: Option<&[String]>,
    default_name: &str,
) -> Result<Vec<(&'a PathBuf, String)>> {
    if let Some(names) = exes {
        if !names.is_empty() {
            let mut out = Vec::new();
            for want in names {
                let found = extracted
                    .iter()
                    .find(|p| p.file_name().map(|f| f == want.as_str()).unwrap_or(false))
                    .with_context(|| {
                        format!("`exes` entry `{want}` not found in extracted archive")
                    })?;
                out.push((found, want.clone()));
            }
            return Ok(out);
        }
    }

    // Single-exe path.
    let wanted = exe.unwrap_or(default_name);
    if let Some(found) = extracted
        .iter()
        .find(|p| p.file_name().map(|f| f == wanted).unwrap_or(false))
    {
        return Ok(vec![(found, wanted.to_string())]);
    }
    // Fall back to the sole extracted file, installed under the default name.
    match extracted.len() {
        1 => Ok(vec![(&extracted[0], default_name.to_string())]),
        0 => bail!("archive produced no files"),
        _ => bail!(
            "could not find `{wanted}` among {} extracted files; \
             set `exe` or `exes` to disambiguate",
            extracted.len()
        ),
    }
}

/// Install from a direct URL. `http` fetches the bytes; extraction and atomic
/// install happen locally. `default_name` is the tool key.
pub fn install(
    tool: &ToolConfig,
    http: &dyn HttpClient,
    install_dir: &Path,
    default_name: &str,
) -> Result<InstallOutcome> {
    let parsed = parse_spec(&tool.spec, SourceKind::Url)?;
    if parsed.source != SourceKind::Url {
        bail!("url source received non-url spec `{}`", tool.spec);
    }
    let url = &parsed.locator;
    let bytes = http.get_bytes(url).with_context(|| format!("downloading {url}"))?;
    let content_sha = sha256_hex(&bytes);

    // Checksum discovery (§8.8): look for a sidecar next to the URL and verify.
    // A mismatch is fatal; absence is non-fatal (recorded as "none").
    match discover_and_verify(http, url, &content_sha) {
        Ok(Some(())) => { /* verified */ }
        Ok(None) => { /* no sidecar found → checksum "none" */ }
        Err(e) => return Err(e),
    }

    let install_paths = install_from_bytes(
        url,
        &bytes,
        install_dir,
        tool.exe.as_deref(),
        tool.exes.as_deref(),
        tool.rename.as_deref(),
        default_name,
    )?;

    Ok(InstallOutcome {
        installed_version: "url".to_string(),
        resolved_asset: Some(url.clone()),
        install_paths,
        // Record the content sha256 so a change to the URL's bytes is detectable.
        sha256: Some(content_sha),
    })
}

/// Shared download-payload installer used by the `url` and `http` sources:
/// extract `bytes` (named by `name_hint`, e.g. the URL, for format detection),
/// select the wanted exe/exes, chmod +x, and atomically install into
/// `install_dir`. Returns the installed paths.
pub fn install_from_bytes(
    name_hint: &str,
    bytes: &[u8],
    install_dir: &Path,
    exe: Option<&str>,
    exes: Option<&[String]>,
    rename: Option<&str>,
    default_name: &str,
) -> Result<Vec<PathBuf>> {
    let staging = tempfile::Builder::new()
        .prefix("ubix-dl-")
        .tempdir()
        .context("creating staging tempdir")?;
    let extracted = archive::extract_all(name_hint, bytes, staging.path())?;

    let final_name = rename.map(str::to_string).unwrap_or_else(|| default_name.to_string());
    let selections = select_exes(&extracted, exe, exes, &final_name)?;

    std::fs::create_dir_all(install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let mut install_paths = Vec::new();
    for (src, name) in selections {
        archive::make_executable(src)?;
        let dst = install_dir.join(&name);
        atomic_install(src, &dst)?;
        // sha256 of the installed file (used for info/verification).
        let _ = sha256_file(&dst)?;
        install_paths.push(dst);
    }
    Ok(install_paths)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Try the sidecar checksum files for `url` (§8.8). Returns `Ok(Some)` if a
/// sidecar was found and verified, `Ok(None)` if none was found, `Err` on a
/// mismatch. Only the per-asset sidecars are attempted for a raw URL (there is
/// no release "asset list" to scan for combined files).
fn discover_and_verify(
    http: &dyn HttpClient,
    url: &str,
    computed_sha: &str,
) -> Result<Option<()>> {
    let asset = url.split(['?', '#']).next().unwrap_or(url);
    let base = asset.rsplit('/').next().unwrap_or(asset);
    let dir = asset.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    // Try each candidate sidecar in §8.8 priority order. Per-asset sidecars live
    // next to the asset (`<asset>.sha256`); combined files live in the same dir.
    for candidate in crate::checksum::candidate_names(base) {
        let sidecar_url = if let Some(suffix) = candidate.strip_prefix(base) {
            // Per-asset sidecar (`<asset>.sha256`) — attach to the full URL.
            format!("{asset}{suffix}")
        } else if dir.is_empty() {
            candidate.clone()
        } else {
            format!("{dir}/{candidate}")
        };
        let Ok(body) = http.get_text(&sidecar_url) else {
            continue;
        };
        if let Some(expected) = crate::checksum::extract_for_asset(&body, base) {
            crate::checksum::verify(&expected, computed_sha)
                .with_context(|| format!("verifying {url} against {sidecar_url}"))?;
            return Ok(Some(()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, name.as_bytes()).unwrap();
        p
    }

    #[test]
    fn select_single_by_exe() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "tool");
        let b = write_file(dir.path(), "readme");
        let files = vec![a.clone(), b];
        let sel = select_exes(&files, Some("tool"), None, "tool").unwrap();
        assert_eq!(sel.len(), 1);
        assert_eq!(*sel[0].0, a);
        assert_eq!(sel[0].1, "tool");
    }

    #[test]
    fn select_sole_file_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "weird-name");
        let files = vec![a.clone()];
        let sel = select_exes(&files, None, None, "mytool").unwrap();
        assert_eq!(*sel[0].0, a);
        assert_eq!(sel[0].1, "mytool");
    }

    #[test]
    fn select_multi_exes() {
        let dir = tempfile::tempdir().unwrap();
        let uv = write_file(dir.path(), "uv");
        let uvx = write_file(dir.path(), "uvx");
        let other = write_file(dir.path(), "LICENSE");
        let files = vec![uv.clone(), uvx.clone(), other];
        let sel = select_exes(
            &files,
            None,
            Some(&["uv".to_string(), "uvx".to_string()]),
            "uv",
        )
        .unwrap();
        assert_eq!(sel.len(), 2);
        assert_eq!(sel[0].1, "uv");
        assert_eq!(sel[1].1, "uvx");
    }

    #[test]
    fn select_missing_exes_entry_errors() {
        let dir = tempfile::tempdir().unwrap();
        let uv = write_file(dir.path(), "uv");
        let files = vec![uv];
        let err = select_exes(&files, None, Some(&["uvx".to_string()]), "uv").unwrap_err();
        assert!(err.to_string().contains("uvx"), "{err}");
    }

    #[test]
    fn select_ambiguous_multi_errors() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_file(dir.path(), "a");
        let b = write_file(dir.path(), "b");
        let files = vec![a, b];
        let err = select_exes(&files, Some("missing"), None, "missing").unwrap_err();
        assert!(err.to_string().contains("disambiguate"), "{err}");
    }

    #[test]
    fn install_verifies_matching_sidecar_checksum() {
        use crate::http::MockHttp;
        let url = "https://example.com/rawtool";
        let payload = b"raw-binary-bytes";
        let sha = sha256_hex(payload);
        let http = MockHttp::new()
            .with_bytes(url, payload.to_vec())
            .with_text(&format!("{url}.sha256"), &format!("{sha}  rawtool\n"));
        let dir = tempfile::tempdir().unwrap();
        let tool = ToolConfig::from_spec(format!("url:{url}"));
        let out = install(&tool, &http, dir.path(), "rawtool").unwrap();
        assert_eq!(out.sha256.as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn install_rejects_mismatched_sidecar_checksum() {
        use crate::http::MockHttp;
        let url = "https://example.com/rawtool";
        let payload = b"raw-binary-bytes";
        let bad = "0".repeat(64);
        let http = MockHttp::new()
            .with_bytes(url, payload.to_vec())
            .with_text(&format!("{url}.sha256"), &format!("{bad}  rawtool\n"));
        let dir = tempfile::tempdir().unwrap();
        let tool = ToolConfig::from_spec(format!("url:{url}"));
        let err = install(&tool, &http, dir.path(), "rawtool").unwrap_err();
        assert!(err.to_string().contains("verifying"), "{err}");
    }

    #[test]
    fn install_downloads_extracts_and_records_sha() {
        use crate::http::MockHttp;
        // Build a tar.gz with a single "tool" binary.
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let data = b"binary-payload";
            let mut h = tar::Header::new_gnu();
            h.set_path("tool").unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append(&h, &data[..]).unwrap();
            b.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut e = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            e.write_all(&tar_bytes).unwrap();
            e.finish().unwrap();
        }
        let url = "https://example.com/tool-linux.tar.gz";
        let http = MockHttp::new().with_bytes(url, gz.clone());
        let install_dir = tempfile::tempdir().unwrap();

        let tool = ToolConfig::from_spec(format!("url:{url}"));
        let out = install(&tool, &http, install_dir.path(), "tool").unwrap();
        assert_eq!(out.install_paths.len(), 1);
        assert!(out.install_paths[0].is_file());
        assert_eq!(out.installed_version, "url");
        // Content sha256 recorded = sha of the downloaded gz bytes.
        assert_eq!(out.sha256.as_deref(), Some(sha256_hex(&gz).as_str()));
    }
}
