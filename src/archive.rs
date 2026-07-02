//! Archive extraction for the `url` source and the go toolchain tarball.
//!
//! ubi handles release-asset extraction internally; this module covers the two
//! places ubix must extract on its own: a user-supplied `url:` archive and the
//! official go tarball from go.dev/dl. Format is detected by filename suffix.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Detected archive format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    TarXz,
    TarBz2,
    Tar,
    Zip,
    Gz,
    /// Not an archive — a bare (possibly executable) file.
    Raw,
}

impl ArchiveFormat {
    /// Detect the archive format from a filename/URL suffix (§5.1 step 4).
    pub fn detect(name: &str) -> ArchiveFormat {
        let lower = name.to_ascii_lowercase();
        // Strip a query string for URLs.
        let lower = lower.split(['?', '#']).next().unwrap_or(&lower);
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            ArchiveFormat::TarGz
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            ArchiveFormat::TarXz
        } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz") || lower.ends_with(".tbz2")
        {
            ArchiveFormat::TarBz2
        } else if lower.ends_with(".tar") {
            ArchiveFormat::Tar
        } else if lower.ends_with(".zip") {
            ArchiveFormat::Zip
        } else if lower.ends_with(".gz") {
            ArchiveFormat::Gz
        } else {
            ArchiveFormat::Raw
        }
    }
}

/// Extract `bytes` (an archive named `name`) fully into `dest_dir`. For `Raw`,
/// writes the bytes as a single file named after `name`'s basename. Returns the
/// list of regular files produced (absolute paths).
pub fn extract_all(name: &str, bytes: &[u8], dest_dir: &Path) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    match ArchiveFormat::detect(name) {
        ArchiveFormat::TarGz => untar(&mut flate2::read::GzDecoder::new(bytes), dest_dir),
        ArchiveFormat::TarXz => untar(&mut xz2::read::XzDecoder::new(bytes), dest_dir),
        ArchiveFormat::TarBz2 => {
            // bzip2 decoder is not a declared dep; surface a clear error rather
            // than mis-extracting. .tar.gz/.tar.xz/.zip cover the vast majority.
            let _ = bytes;
            bail!("bzip2 archives are not supported; please use a .tar.gz/.tar.xz/.zip asset")
        }
        ArchiveFormat::Tar => untar(&mut &bytes[..], dest_dir),
        ArchiveFormat::Zip => unzip(bytes, dest_dir),
        ArchiveFormat::Gz => {
            // Single gzip-compressed file → the decompressed payload named after
            // the archive with `.gz` stripped.
            let mut dec = flate2::read::GzDecoder::new(bytes);
            let mut out = Vec::new();
            dec.read_to_end(&mut out).context("gunzip")?;
            let base = basename(name);
            let base = base.strip_suffix(".gz").unwrap_or(&base);
            let path = dest_dir.join(base);
            std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
            make_executable(&path)?;
            Ok(vec![path])
        }
        ArchiveFormat::Raw => {
            let path = dest_dir.join(basename(name));
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            make_executable(&path)?;
            Ok(vec![path])
        }
    }
}

fn basename(name: &str) -> String {
    let no_query = name.split(['?', '#']).next().unwrap_or(name);
    no_query
        .rsplit('/')
        .next()
        .unwrap_or(no_query)
        .to_string()
}

fn untar<R: Read>(reader: &mut R, dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut ar = tar::Archive::new(reader);
    ar.unpack(dest_dir)
        .with_context(|| format!("unpacking tar into {}", dest_dir.display()))?;
    collect_files(dest_dir)
}

fn unzip(bytes: &[u8], dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor).context("opening zip archive")?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).context("reading zip entry")?;
        let Some(rel) = file.enclosed_name() else {
            // Skip entries with unsafe (absolute / traversing) paths.
            continue;
        };
        let out_path = dest_dir.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out_path)
                .with_context(|| format!("creating {}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("creating {}", out_path.display()))?;
        std::io::copy(&mut file, &mut out)
            .with_context(|| format!("writing {}", out_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                let _ = std::fs::set_permissions(
                    &out_path,
                    std::fs::Permissions::from_mode(mode),
                );
            }
        }
    }
    collect_files(dest_dir)
}

/// Recursively collect regular files under `dir`.
pub fn collect_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)
            .with_context(|| format!("reading dir {}", d.display()))?
        {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Make a file executable on unix (no-op elsewhere).
pub fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detect_formats() {
        assert_eq!(ArchiveFormat::detect("x.tar.gz"), ArchiveFormat::TarGz);
        assert_eq!(ArchiveFormat::detect("x.tgz"), ArchiveFormat::TarGz);
        assert_eq!(ArchiveFormat::detect("x.tar.xz"), ArchiveFormat::TarXz);
        assert_eq!(ArchiveFormat::detect("x.zip"), ArchiveFormat::Zip);
        assert_eq!(ArchiveFormat::detect("x.gz"), ArchiveFormat::Gz);
        assert_eq!(ArchiveFormat::detect("x.tar"), ArchiveFormat::Tar);
        assert_eq!(
            ArchiveFormat::detect("https://h/x-linux.tar.gz?token=1"),
            ArchiveFormat::TarGz
        );
        assert_eq!(ArchiveFormat::detect("plainbinary"), ArchiveFormat::Raw);
    }

    #[test]
    fn extract_targz_roundtrip() {
        // Build a tar.gz containing a single file "tool".
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let data = b"#!/bin/sh\necho hi\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("tool").unwrap();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, &data[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc =
                flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(&tar_bytes).unwrap();
            enc.finish().unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let files = extract_all("bundle.tar.gz", &gz, dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap(), "tool");
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"#!/bin/sh\necho hi\n");
    }

    #[test]
    fn extract_raw_binary() {
        let dir = tempfile::tempdir().unwrap();
        let files = extract_all("rustup-init", b"ELF...", dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap(), "rustup-init");
    }

    #[test]
    fn extract_gz_single_file() {
        let mut gz = Vec::new();
        {
            let mut enc =
                flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
            enc.write_all(b"payload").unwrap();
            enc.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let files = extract_all("thing.gz", &gz, dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_name().unwrap(), "thing");
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"payload");
    }
}
