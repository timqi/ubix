//! Toolchain bootstrap (§6, D9).
//!
//! * `rust` (M5): fetch rustup-init from static.rust-lang.org and run `-y`.
//! * `go`   (M5): fetch the latest stable tarball from go.dev/dl and extract to GOROOT.
//!
//! Underlying single-binary tools (uv, fnm) are ordinary github release tools,
//! installed as a side effect of the language bootstraps (`python`/`nodejs`),
//! which also provision a default runtime. The package sources that need them
//! (pypi/npm) point the user at those bootstraps when the toolchain is missing.
//!
//! Both targets are idempotent: if the target is already present they skip
//! unless `--reinstall`. External calls go through the `CommandRunner`/
//! `HttpClient` seams.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::archive;
use crate::http::HttpClient;
use crate::runner::CommandRunner;

/// Bootstrap targets accepted by `ubix bootstrap <target>`.
///
/// `Rust`/`Go` are toolchain fetches handled here (via [`bootstrap`]).
/// `Python`/`Nodejs` need the add/config/state machinery and are handled in the
/// CLI/App layer (`cmd_bootstrap`), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapTarget {
    Rust,
    Go,
    Python,
    Nodejs,
    Pixi,
}

impl std::str::FromStr for BootstrapTarget {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "rust" => BootstrapTarget::Rust,
            "go" => BootstrapTarget::Go,
            "python" => BootstrapTarget::Python,
            "nodejs" => BootstrapTarget::Nodejs,
            "pixi" => BootstrapTarget::Pixi,
            // uv/fnm are plain github releases, not toolchain bootstraps. Use the
            // language targets (python/nodejs) to install them + a runtime.
            "uv" => bail!(
                "`uv` is not a bootstrap target; run:\n    \
                 ubix bootstrap python\n\
                 (installs uv + a default Python)"
            ),
            "fnm" => bail!(
                "`fnm` is not a bootstrap target; run:\n    \
                 ubix bootstrap nodejs\n\
                 (installs fnm + a default LTS node)"
            ),
            other => {
                bail!("unknown bootstrap target `{other}` (expected rust|go|python|nodejs|pixi)")
            }
        })
    }
}

/// Dependencies for bootstrap operations (all behind seams for testability).
pub struct BootstrapCtx<'a> {
    pub runner: &'a dyn CommandRunner,
    pub http: &'a dyn HttpClient,
    pub go_root: PathBuf,
}

/// Run the toolchain bootstrap for `target` (rust/go only). python/nodejs are
/// handled by the CLI layer because they need config/state.
pub fn bootstrap(target: BootstrapTarget, reinstall: bool, ctx: &BootstrapCtx) -> Result<()> {
    match target {
        BootstrapTarget::Rust => bootstrap_rust(ctx, reinstall),
        BootstrapTarget::Go => bootstrap_go(ctx, reinstall),
        BootstrapTarget::Python | BootstrapTarget::Nodejs | BootstrapTarget::Pixi => {
            unreachable!("python/nodejs/pixi are handled in the CLI layer");
        }
    }
}

/// Extract a `vX.Y.Z` version from tool output (e.g. `fnm install --lts` prints
/// `Installing Node v22.14.0 (x64)`). Returns the first match, if any.
pub fn parse_semver_v(output: &str) -> Option<String> {
    for line in output.lines() {
        for tok in line.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            if is_v_semver(tok) {
                return Some(tok.to_string());
            }
        }
    }
    None
}

fn is_v_semver(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn bootstrap_rust(ctx: &BootstrapCtx, reinstall: bool) -> Result<()> {
    if ctx.runner.which("rustup") && !reinstall {
        println!("rustup already installed (use --reinstall to force)");
        return Ok(());
    }
    let url = rustup_init_url(current_target());
    let bytes = ctx
        .http
        .get_bytes(&url)
        .with_context(|| format!("downloading {url}"))?;

    let staging = tempfile::Builder::new()
        .prefix("ubix-rustup-")
        .tempdir()
        .context("staging tempdir")?;
    let init = staging.path().join("rustup-init");
    std::fs::write(&init, &bytes).with_context(|| format!("writing {}", init.display()))?;
    archive::make_executable(&init)?;

    let out = ctx
        .runner
        .run(&init.to_string_lossy(), &["-y"], &[])
        .context("running rustup-init -y")?;
    if !out.success() {
        bail!("rustup-init failed: {}", out.stderr.trim());
    }
    println!("bootstrapped rust via rustup (rustc/cargo in ~/.cargo/bin)");
    Ok(())
}

/// The go executable's file name for the running platform (`go.exe` on Windows).
fn go_exe_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "go.exe"
    } else {
        "go"
    }
}

fn bootstrap_go(ctx: &BootstrapCtx, reinstall: bool) -> Result<()> {
    if ctx.go_root.join("bin").join(go_exe_name()).exists() && !reinstall {
        println!("go already installed at {} (use --reinstall to force)", ctx.go_root.display());
        return Ok(());
    }
    // Query go.dev for the latest stable release for our platform.
    let index = ctx
        .http
        .get_text("https://go.dev/dl/?mode=json")
        .context("querying go.dev/dl")?;
    let archive = pick_go_archive(&index, go_os(), go_arch())?;
    let filename = archive.filename;
    let url = format!("https://go.dev/dl/{filename}");
    let bytes = ctx.http.get_bytes(&url).with_context(|| format!("downloading {url}"))?;

    // Verify the tarball against the sha256 published in the go.dev/dl JSON
    // BEFORE extracting anything into GOROOT (§8.8). Absent hash → non-fatal.
    if !archive.sha256.is_empty() {
        let computed = crate::engine::sha256_bytes(&bytes);
        crate::checksum::verify(&archive.sha256, &computed)
            .with_context(|| format!("verifying {filename} against go.dev published sha256"))?;
    }

    // The tarball contains a top-level `go/` dir. Extract into a staging dir on
    // the SAME filesystem as go_root, then move the extracted `go/` to EXACTLY
    // `ctx.go_root` — so a custom go_root whose basename isn't `go` still lands
    // in the right place (a plain "extract into parent" would create `<parent>/go`).
    let parent = ctx
        .go_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".ubix-go-")
        .tempdir_in(&parent)
        .with_context(|| format!("creating go staging dir in {}", parent.display()))?;
    archive::extract_into(&filename, &bytes, staging.path())?;
    let extracted = staging.path().join("go");
    if !extracted.join("bin").join(go_exe_name()).exists() {
        bail!("go tarball did not contain the expected `go/bin/` layout");
    }
    // Replace any existing GOROOT (stale/partial dir, or --reinstall) — propagate
    // a removal failure rather than extracting into a mixed-version directory.
    if ctx.go_root.exists() {
        std::fs::remove_dir_all(&ctx.go_root)
            .with_context(|| format!("removing existing {}", ctx.go_root.display()))?;
    }
    std::fs::rename(&extracted, &ctx.go_root)
        .with_context(|| format!("moving extracted go into {}", ctx.go_root.display()))?;
    println!(
        "bootstrapped go into {} (add {}/bin to PATH)",
        ctx.go_root.display(),
        ctx.go_root.display()
    );
    Ok(())
}

/// The rustup-init URL for a target triple (§6.1).
pub fn rustup_init_url(target: &str) -> String {
    format!("https://static.rust-lang.org/rustup/dist/{target}/rustup-init")
}

/// Best-effort current rust target triple for the running platform.
pub fn current_target() -> &'static str {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    {
        "x86_64-unknown-linux-gnu"
    }
}

use crate::platform::{goarch as go_arch, goos as go_os};

/// A chosen go download: filename plus its published sha256 (for verification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoArchive {
    pub filename: String,
    pub sha256: String,
}

/// Parse go.dev/dl JSON and pick the archive for the latest stable release
/// matching `os`/`arch` (kind = "archive"), returning its filename and the
/// published sha256. Pure/testable.
pub fn pick_go_archive(index_json: &str, os: &str, arch: &str) -> Result<GoArchive> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct Release {
        stable: bool,
        files: Vec<GoFile>,
    }
    #[derive(Deserialize)]
    struct GoFile {
        filename: String,
        os: String,
        arch: String,
        kind: String,
        #[serde(default)]
        sha256: String,
    }
    let releases: Vec<Release> =
        serde_json::from_str(index_json).context("parsing go.dev/dl JSON")?;
    for rel in releases.iter().filter(|r| r.stable) {
        for f in &rel.files {
            if f.os == os && f.arch == arch && f.kind == "archive" {
                return Ok(GoArchive {
                    filename: f.filename.clone(),
                    sha256: f.sha256.clone(),
                });
            }
        }
    }
    bail!("no stable go archive found for {os}/{arch}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttp;
    use crate::runner::MockRunner;

    #[test]
    fn target_parsing() {
        assert_eq!("rust".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Rust);
        assert_eq!("GO".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Go);
        assert_eq!("python".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Python);
        assert_eq!("Nodejs".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Nodejs);
        assert_eq!("pixi".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Pixi);
        assert!("brew".parse::<BootstrapTarget>().is_err());
    }

    #[test]
    fn parse_semver_v_extracts_version() {
        assert_eq!(
            parse_semver_v("Installing Node v22.14.0 (x64)").as_deref(),
            Some("v22.14.0")
        );
        assert_eq!(parse_semver_v("v20.11.1").as_deref(), Some("v20.11.1"));
        assert_eq!(parse_semver_v("no version here"), None);
        assert_eq!(parse_semver_v("v22"), None); // not 3-part
    }

    #[test]
    fn uv_target_points_to_language_bootstrap() {
        // uv/fnm are not standalone bootstrap targets; parsing them yields a clear
        // error pointing at the language bootstrap that installs them + a runtime.
        let err = "uv".parse::<BootstrapTarget>().unwrap_err().to_string();
        assert!(err.contains("ubix bootstrap python"), "{err}");
    }

    #[test]
    fn fnm_target_points_to_language_bootstrap() {
        let err = "fnm".parse::<BootstrapTarget>().unwrap_err().to_string();
        assert!(err.contains("ubix bootstrap nodejs"), "{err}");
    }

    #[test]
    fn rustup_url_construction() {
        assert_eq!(
            rustup_init_url("x86_64-unknown-linux-gnu"),
            "https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init"
        );
    }

    #[test]
    fn pick_go_archive_matches_platform_and_sha() {
        let json = r#"[
            {"version":"go1.99.0","stable":true,"files":[
                {"filename":"go1.99.0.linux-amd64.tar.gz","os":"linux","arch":"amd64","kind":"archive","sha256":"aaaa"},
                {"filename":"go1.99.0.linux-amd64.msi","os":"linux","arch":"amd64","kind":"installer","sha256":"bbbb"},
                {"filename":"go1.99.0.darwin-arm64.tar.gz","os":"darwin","arch":"arm64","kind":"archive","sha256":"cccc"}
            ]},
            {"version":"go1.98.0","stable":true,"files":[]}
        ]"#;
        let a = pick_go_archive(json, "linux", "amd64").unwrap();
        assert_eq!(a.filename, "go1.99.0.linux-amd64.tar.gz");
        assert_eq!(a.sha256, "aaaa");
        let d = pick_go_archive(json, "darwin", "arm64").unwrap();
        assert_eq!(d.filename, "go1.99.0.darwin-arm64.tar.gz");
        assert_eq!(d.sha256, "cccc");
    }

    #[test]
    fn pick_go_archive_skips_unstable_and_missing() {
        let json = r#"[{"version":"go1.99rc1","stable":false,"files":[
            {"filename":"go.linux-amd64.tar.gz","os":"linux","arch":"amd64","kind":"archive","sha256":"x"}]}]"#;
        assert!(pick_go_archive(json, "linux", "amd64").is_err());
    }

    /// Build a tar.gz whose top-level dir is `go/` with a `go/bin/go` file.
    fn fake_go_tarball() -> Vec<u8> {
        use std::io::Write;
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let data = b"#!/bin/sh\n";
            let mut h = tar::Header::new_gnu();
            h.set_path("go/bin/go").unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append(&h, &data[..]).unwrap();
            b.finish().unwrap();
        }
        let mut gz = Vec::new();
        let mut e = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        e.write_all(&tar_bytes).unwrap();
        e.finish().unwrap();
        gz
    }

    #[test]
    fn go_bootstrap_verifies_sha_then_extracts() {
        let gz = fake_go_tarball();
        let sha = crate::engine::sha256_bytes(&gz);
        let os = go_os();
        let arch = go_arch();
        let fname = format!("go1.99.0.{os}-{arch}.tar.gz");
        let index = format!(
            r#"[{{"version":"go1.99.0","stable":true,"files":[
                {{"filename":"{fname}","os":"{os}","arch":"{arch}","kind":"archive","sha256":"{sha}"}}]}}]"#
        );
        let http = MockHttp::new()
            .with_text("https://go.dev/dl/?mode=json", &index)
            .with_bytes(&format!("https://go.dev/dl/{fname}"), gz);

        let goroot = tempfile::tempdir().unwrap();
        let go_root = goroot.path().join("go");
        let runner = MockRunner::new();
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            go_root: go_root.clone(),
        };
        bootstrap(BootstrapTarget::Go, false, &ctx).unwrap();
        assert!(go_root.join("bin").join("go").is_file());
    }

    #[test]
    fn go_bootstrap_honors_custom_go_root_basename() {
        // A go_root whose basename is NOT `go` must still receive the toolchain.
        let gz = fake_go_tarball();
        let sha = crate::engine::sha256_bytes(&gz);
        let os = go_os();
        let arch = go_arch();
        let fname = format!("go1.99.0.{os}-{arch}.tar.gz");
        let index = format!(
            r#"[{{"version":"go1.99.0","stable":true,"files":[
                {{"filename":"{fname}","os":"{os}","arch":"{arch}","kind":"archive","sha256":"{sha}"}}]}}]"#
        );
        let http = MockHttp::new()
            .with_text("https://go.dev/dl/?mode=json", &index)
            .with_bytes(&format!("https://go.dev/dl/{fname}"), gz);

        let base = tempfile::tempdir().unwrap();
        let go_root = base.path().join("golang-1.99"); // basename != "go"
        let runner = MockRunner::new();
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            go_root: go_root.clone(),
        };
        bootstrap(BootstrapTarget::Go, false, &ctx).unwrap();
        assert!(go_root.join("bin").join("go").is_file());
        // No stray `<parent>/go` left behind.
        assert!(!base.path().join("go").exists());
    }

    #[test]
    fn go_bootstrap_rejects_bad_sha_before_extract() {
        let gz = fake_go_tarball();
        let os = go_os();
        let arch = go_arch();
        let fname = format!("go1.99.0.{os}-{arch}.tar.gz");
        let bad = "0".repeat(64);
        let index = format!(
            r#"[{{"version":"go1.99.0","stable":true,"files":[
                {{"filename":"{fname}","os":"{os}","arch":"{arch}","kind":"archive","sha256":"{bad}"}}]}}]"#
        );
        let http = MockHttp::new()
            .with_text("https://go.dev/dl/?mode=json", &index)
            .with_bytes(&format!("https://go.dev/dl/{fname}"), gz);

        let goroot = tempfile::tempdir().unwrap();
        let go_root = goroot.path().join("go");
        let runner = MockRunner::new();
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            go_root: go_root.clone(),
        };
        let err = bootstrap(BootstrapTarget::Go, false, &ctx).unwrap_err();
        assert!(err.to_string().contains("verifying"), "{err}");
        // Nothing extracted.
        assert!(!go_root.exists());
    }
}
