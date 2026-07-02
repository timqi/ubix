//! Release install engine: wraps `ubi::UbiBuilder` with an ATOMIC replace.
//!
//! Strategy (§8.7 / D15): ubi installs into a private staging directory (a
//! tempdir), we verify the produced executable exists and is non-empty, compute
//! its sha256, then atomically `rename` it into the real `install_dir`. State is
//! updated by the caller ONLY after this returns Ok.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use ubi::{ForgeType, UbiBuilder};

/// Parameters for a single release install/upgrade.
#[derive(Debug, Clone)]
pub struct ReleaseRequest {
    /// `owner/repo` for github, `group/.../repo` for gitlab.
    pub project: String,
    pub forge: ForgeType,
    /// Optional pinned tag.
    pub tag: Option<String>,
    /// Optional disambiguation substring (ubi `.matching()`). Matching is a
    /// CASE-SENSITIVE substring test (`asset_name.contains(matching)`), per ubi
    /// v0.9 semantics — no case folding is applied.
    pub matching: Option<String>,
    /// Executable name inside the archive; defaults to the tool key.
    pub exe: Option<String>,
    /// Rename installed exe to this.
    pub rename: Option<String>,
    /// Final directory to install into (absolute).
    pub install_dir: PathBuf,
    /// The final installed file name (tool key or `rename`).
    pub final_name: String,
    /// Optional GitHub token (from env).
    pub github_token: Option<String>,
    /// Optional GitLab token (from env).
    pub gitlab_token: Option<String>,
    /// Optional API base URL (self-hosted gitlab → `<host>/api/v4`).
    pub api_base_url: Option<String>,
}

/// What the engine installed.
#[derive(Debug, Clone)]
pub struct EngineResult {
    /// Final absolute path of the installed executable.
    pub install_path: PathBuf,
    pub sha256: String,
    /// The tag/version ubi resolved, if known (best-effort).
    pub version: Option<String>,
}

/// Trait so the engine can be swapped for a fake in tests that must not hit the
/// network. The default production impl is [`UbiEngine`].
pub trait ReleaseEngine {
    fn install(&self, req: &ReleaseRequest) -> Result<EngineResult>;
}

/// Production engine backed by the `ubi` crate.
#[derive(Debug, Default)]
pub struct UbiEngine;

impl UbiEngine {
    pub fn new() -> Self {
        Self
    }
}

impl ReleaseEngine for UbiEngine {
    fn install(&self, req: &ReleaseRequest) -> Result<EngineResult> {
        // 1) Stage into a private tempdir so a failure never corrupts install_dir.
        let staging = tempfile::Builder::new()
            .prefix("ubix-stage-")
            .tempdir()
            .context("creating staging tempdir")?;
        let staging_dir = staging.path().to_path_buf();

        run_ubi(req, &staging_dir)?;

        // 2) Locate the produced executable in the staging dir.
        let staged_exe = locate_staged_exe(&staging_dir, req)?;

        // 3) Verify: non-empty file, then compute sha256.
        let meta = std::fs::metadata(&staged_exe)
            .with_context(|| format!("stat staged file {}", staged_exe.display()))?;
        if meta.len() == 0 {
            bail!("staged executable {} is empty", staged_exe.display());
        }
        let sha = sha256_file(&staged_exe)?;

        // 4) Atomic replace into install_dir.
        std::fs::create_dir_all(&req.install_dir)
            .with_context(|| format!("creating install_dir {}", req.install_dir.display()))?;
        let final_path = req.install_dir.join(&req.final_name);
        atomic_install(&staged_exe, &final_path)?;

        Ok(EngineResult {
            install_path: final_path,
            sha256: sha,
            version: req.tag.clone(),
        })
    }
}

/// Build and drive ubi on a minimal current-thread tokio runtime, installing
/// into `staging_dir`.
fn run_ubi(req: &ReleaseRequest, staging_dir: &Path) -> Result<()> {
    let mut builder = UbiBuilder::new()
        .project(&req.project)
        .install_dir(staging_dir)
        .forge(req.forge.clone());

    if let Some(tag) = &req.tag {
        builder = builder.tag(tag);
    }
    if let Some(m) = &req.matching {
        builder = builder.matching(m);
    }
    if let Some(e) = &req.exe {
        builder = builder.exe(e);
    }
    if let Some(r) = &req.rename {
        builder = builder.rename_exe_to(r);
    }
    // `token()` is forge-agnostic (ubi routes it by forge type). We pass
    // whichever token the caller supplied for the active forge.
    if let Some(t) = req.github_token.as_ref().or(req.gitlab_token.as_ref()) {
        builder = builder.token(t);
    }
    if let Some(base) = &req.api_base_url {
        builder = builder.api_base_url(base);
    }

    let mut ubi = builder.build().context("building ubi installer")?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting async runtime for ubi")?;
    rt.block_on(async { ubi.install_binary().await })
        .context("ubi install_binary failed")?;
    Ok(())
}

/// Find the executable ubi produced in the staging dir. ubi names the installed
/// file after `rename`, then `exe`, then the project repo name.
fn locate_staged_exe(staging_dir: &Path, req: &ReleaseRequest) -> Result<PathBuf> {
    // Preferred candidate names, in order.
    let repo = req
        .project
        .rsplit('/')
        .next()
        .unwrap_or(&req.project)
        .to_string();
    let mut candidates: Vec<String> = Vec::new();
    if let Some(r) = &req.rename {
        candidates.push(r.clone());
    }
    if let Some(e) = &req.exe {
        candidates.push(e.clone());
    }
    candidates.push(repo);
    candidates.push(req.final_name.clone());

    for name in &candidates {
        let p = staging_dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }

    // Fallback: if exactly one regular file exists in the staging dir, take it.
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(staging_dir)
        .with_context(|| format!("reading staging dir {}", staging_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            files.push(entry.path());
        }
    }
    match files.len() {
        1 => Ok(files.remove(0)),
        0 => bail!(
            "ubi produced no file in staging dir (looked for {:?})",
            candidates
        ),
        _ => bail!(
            "ubi produced multiple files {:?}; expected one of {:?} \
             (multi-exe `exes` is not supported in M1)",
            files,
            candidates
        ),
    }
}

/// Atomically move `src` to `dst`. Tries a plain rename first (same filesystem),
/// falling back to copy + rename via a temp in the destination directory when
/// the source is on a different filesystem (the common tempdir case).
fn atomic_install(src: &Path, dst: &Path) -> Result<()> {
    // Ensure exec bit on unix before publishing.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(src)
            .with_context(|| format!("stat {}", src.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(src, perms)
            .with_context(|| format!("chmod {}", src.display()))?;
    }

    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }

    // Cross-device: copy into a temp file in the destination dir, then rename.
    let dst_dir = dst
        .parent()
        .ok_or_else(|| anyhow::anyhow!("install path {} has no parent", dst.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".ubix-install-")
        .tempfile_in(dst_dir)
        .with_context(|| format!("creating temp in {}", dst_dir.display()))?;
    {
        use std::io::Write;
        let bytes = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
        tmp.write_all(&bytes)
            .with_context(|| format!("writing temp in {}", dst_dir.display()))?;
        tmp.flush().ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = tmp.as_file();
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(0o755);
        f.set_permissions(perms)?;
    }
    let tmp_path = tmp.into_temp_path();
    std::fs::rename(&tmp_path, dst)
        .with_context(|| format!("atomic rename into {}", dst.display()))?;
    // rename consumed the temp path; prevent TempPath drop from unlinking.
    std::mem::forget(tmp_path);
    Ok(())
}

/// Compute the hex sha256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_install_across_dirs_and_chmod() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("tool");
        std::fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
        let dst = dst_dir.path().join("tool");

        atomic_install(&src, &dst).unwrap();
        assert!(dst.is_file());
        assert_eq!(std::fs::read(&dst).unwrap(), b"#!/bin/sh\necho hi\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dst).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "exec bits set");
        }
    }

    #[test]
    fn sha256_matches_known() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        std::fs::write(&f, b"abc").unwrap();
        // sha256("abc")
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn locate_single_file_fallback() {
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(staging.path().join("weird-name-x86_64"), b"bin").unwrap();
        let req = ReleaseRequest {
            project: "owner/repo".into(),
            forge: ForgeType::GitHub,
            tag: None,
            matching: None,
            exe: None,
            rename: None,
            install_dir: staging.path().to_path_buf(),
            final_name: "repo".into(),
            github_token: None,
            gitlab_token: None,
            api_base_url: None,
        };
        let found = locate_staged_exe(staging.path(), &req).unwrap();
        assert!(found.file_name().unwrap().to_string_lossy().contains("weird-name"));
    }

    #[test]
    fn locate_multiple_files_errors() {
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(staging.path().join("a"), b"1").unwrap();
        std::fs::write(staging.path().join("b"), b"2").unwrap();
        let req = ReleaseRequest {
            project: "owner/repo".into(),
            forge: ForgeType::GitHub,
            tag: None,
            matching: None,
            exe: None,
            rename: None,
            install_dir: staging.path().to_path_buf(),
            final_name: "repo".into(),
            github_token: None,
            gitlab_token: None,
            api_base_url: None,
        };
        let err = locate_staged_exe(staging.path(), &req).unwrap_err();
        assert!(err.to_string().contains("multiple files"), "{err}");
    }
}
