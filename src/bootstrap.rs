//! Toolchain / underlying-tool bootstrap (§6, D9).
//!
//! * `uv`  (M3): install via the release engine (github:astral-sh/uv).
//! * `fnm` (M4): install via the release engine (github:Schniz/fnm), then
//!   `fnm install --lts` + `fnm default <lts>`.
//! * `rust`(M5): fetch rustup-init from static.rust-lang.org and run `-y`.
//! * `go`  (M5): fetch the latest stable tarball from go.dev/dl and extract to GOROOT.
//!
//! All are idempotent: if the target is already present they skip unless
//! `--reinstall`. External calls go through the `CommandRunner`/`HttpClient`
//! seams; the release fetches go through the `ReleaseEngine`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::archive;
use crate::engine::{ReleaseEngine, ReleaseRequest};
use crate::http::HttpClient;
use crate::runner::CommandRunner;

/// Bootstrap targets accepted by `ubix bootstrap <target>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapTarget {
    Rust,
    Go,
    Uv,
    Fnm,
}

impl std::str::FromStr for BootstrapTarget {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "rust" => BootstrapTarget::Rust,
            "go" => BootstrapTarget::Go,
            "uv" => BootstrapTarget::Uv,
            "fnm" => BootstrapTarget::Fnm,
            other => bail!("unknown bootstrap target `{other}` (expected rust|go|uv|fnm)"),
        })
    }
}

/// Dependencies for bootstrap operations (all behind seams for testability).
pub struct BootstrapCtx<'a> {
    pub runner: &'a dyn CommandRunner,
    pub http: &'a dyn HttpClient,
    pub engine: &'a dyn ReleaseEngine,
    pub install_dir: PathBuf,
    pub go_root: PathBuf,
}

/// Run the bootstrap for `target`.
pub fn bootstrap(target: BootstrapTarget, reinstall: bool, ctx: &BootstrapCtx) -> Result<()> {
    match target {
        BootstrapTarget::Uv => bootstrap_uv(ctx, reinstall),
        BootstrapTarget::Fnm => bootstrap_fnm(ctx, reinstall),
        BootstrapTarget::Rust => bootstrap_rust(ctx, reinstall),
        BootstrapTarget::Go => bootstrap_go(ctx, reinstall),
    }
}

fn bootstrap_uv(ctx: &BootstrapCtx, reinstall: bool) -> Result<()> {
    if ctx.runner.which("uv") && !reinstall {
        println!("uv already installed (use --reinstall to force)");
        return Ok(());
    }
    // uv ships uv+uvx in one archive. Install the primary `uv`; `uvx` is a thin
    // shim uv provides itself. Drive the engine directly with a request.
    let req = ReleaseRequest {
        project: "astral-sh/uv".into(),
        forge: ubi::ForgeType::GitHub,
        tag: None,
        matching: None,
        exe: Some("uv".into()),
        exes: Vec::new(),
        rename: None,
        install_dir: ctx.install_dir.clone(),
        final_name: "uv".into(),
        github_token: crate::sources::github::github_token_from_env(),
        gitlab_token: None,
        api_base_url: None,
    };
    ctx.engine.install(&req).context("installing uv via release engine")?;
    println!("bootstrapped uv into {}", ctx.install_dir.display());
    Ok(())
}

fn bootstrap_fnm(ctx: &BootstrapCtx, reinstall: bool) -> Result<()> {
    if ctx.runner.which("fnm") && !reinstall {
        println!("fnm already installed (use --reinstall to force)");
        return Ok(());
    }
    // fnm asset names are irregular (fnm-linux.zip / fnm-arm64.zip); ubi picks
    // by matching. Install the fnm binary via the release engine.
    let req = ReleaseRequest {
        project: "Schniz/fnm".into(),
        forge: ubi::ForgeType::GitHub,
        tag: None,
        matching: None,
        exe: Some("fnm".into()),
        exes: Vec::new(),
        rename: None,
        install_dir: ctx.install_dir.clone(),
        final_name: "fnm".into(),
        github_token: crate::sources::github::github_token_from_env(),
        gitlab_token: None,
        api_base_url: None,
    };
    ctx.engine.install(&req).context("installing fnm via release engine")?;

    // The just-installed fnm is NOT on PATH yet (it lives in install_dir which
    // the user may not have sourced), so invoke it by ABSOLUTE path.
    let fnm_bin = ctx.install_dir.join("fnm");
    let fnm_path = fnm_bin.to_string_lossy().into_owned();

    // Install the latest LTS node.
    let out = ctx
        .runner
        .run(&fnm_path, &["install", "--lts"], &[])
        .context("running fnm install --lts")?;
    if !out.success() {
        bail!("fnm install --lts failed: {}", out.stderr.trim());
    }
    // Prefer setting the default to the exact version fnm just installed (parsed
    // from its output); fall back to the `lts-latest` alias if we cannot parse.
    let version = parse_fnm_installed_version(&out.stdout).or_else(|| {
        // Some fnm versions print the "Installing ..." line to stderr.
        parse_fnm_installed_version(&out.stderr)
    });
    let default_arg = version.as_deref().unwrap_or("lts-latest");
    let out = ctx
        .runner
        .run(&fnm_path, &["default", default_arg], &[])
        .context("running fnm default")?;
    if !out.success() {
        bail!("fnm default failed: {}", out.stderr.trim());
    }
    println!("bootstrapped fnm and set default node to {default_arg}");
    Ok(())
}

/// Parse the installed node version (e.g. `v22.14.0`) from `fnm install --lts`
/// output. fnm prints lines like `Installing Node v22.14.0 (x64)`.
pub fn parse_fnm_installed_version(output: &str) -> Option<String> {
    for line in output.lines() {
        for tok in line.split_whitespace() {
            let t = tok.trim_end_matches(['(', ')']);
            if is_node_version(t) {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn is_node_version(s: &str) -> bool {
    // vMAJOR.MINOR.PATCH
    if let Some(rest) = s.strip_prefix('v') {
        let parts: Vec<&str> = rest.split('.').collect();
        return parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    }
    false
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

fn bootstrap_go(ctx: &BootstrapCtx, reinstall: bool) -> Result<()> {
    if ctx.go_root.join("bin").join("go").exists() && !reinstall {
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

    // The tarball contains a top-level `go/` dir; extract into go_root's parent
    // so `<go_root>` becomes the extracted `go/`.
    let parent = ctx
        .go_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    // Remove an existing GOROOT on reinstall to avoid mixing versions.
    if reinstall && ctx.go_root.exists() {
        std::fs::remove_dir_all(&ctx.go_root).ok();
    }
    archive::extract_all(&filename, &bytes, &parent)?;
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

fn go_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn go_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    }
}

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
    use crate::engine::EngineResult;
    use crate::http::MockHttp;
    use crate::runner::{CommandOutput, MockRunner};
    use std::sync::Mutex;

    struct RecordingEngine {
        reqs: Mutex<Vec<ReleaseRequest>>,
    }
    impl ReleaseEngine for RecordingEngine {
        fn install(&self, req: &ReleaseRequest) -> Result<EngineResult> {
            self.reqs.lock().unwrap().push(req.clone());
            let path = req.install_dir.join(&req.final_name);
            Ok(EngineResult {
                install_paths: vec![path],
                sha256: "x".into(),
                version: Some("v1".into()),
            })
        }
    }

    #[test]
    fn target_parsing() {
        assert_eq!("rust".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Rust);
        assert_eq!("GO".parse::<BootstrapTarget>().unwrap(), BootstrapTarget::Go);
        assert!("brew".parse::<BootstrapTarget>().is_err());
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

    #[test]
    fn uv_skips_when_present_without_reinstall() {
        let runner = MockRunner::new().with_present("uv");
        let http = MockHttp::new();
        let engine = RecordingEngine { reqs: Mutex::new(Vec::new()) };
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            engine: &engine,
            install_dir: PathBuf::from("/tmp/bin"),
            go_root: PathBuf::from("/tmp/go"),
        };
        bootstrap(BootstrapTarget::Uv, false, &ctx).unwrap();
        assert!(engine.reqs.lock().unwrap().is_empty(), "should skip");
    }

    #[test]
    fn uv_installs_via_engine_when_missing() {
        let runner = MockRunner::new(); // uv absent
        let http = MockHttp::new();
        let engine = RecordingEngine { reqs: Mutex::new(Vec::new()) };
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            engine: &engine,
            install_dir: PathBuf::from("/tmp/bin"),
            go_root: PathBuf::from("/tmp/go"),
        };
        bootstrap(BootstrapTarget::Uv, false, &ctx).unwrap();
        let reqs = engine.reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].project, "astral-sh/uv");
    }

    #[test]
    fn fnm_uses_absolute_path_and_parses_version() {
        // fnm is invoked by absolute path (/tmp/bin/fnm), and `default` targets
        // the exact version parsed from the install output — not the alias.
        let runner = MockRunner::new()
            .expect(
                "/tmp/bin/fnm install --lts",
                CommandOutput {
                    status: 0,
                    stdout: "Installing Node v22.14.0 (x64)\n".into(),
                    stderr: String::new(),
                },
            )
            .expect(
                "/tmp/bin/fnm default v22.14.0",
                CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            );
        let http = MockHttp::new();
        let engine = RecordingEngine { reqs: Mutex::new(Vec::new()) };
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            engine: &engine,
            install_dir: PathBuf::from("/tmp/bin"),
            go_root: PathBuf::from("/tmp/go"),
        };
        bootstrap(BootstrapTarget::Fnm, false, &ctx).unwrap();
        assert_eq!(engine.reqs.lock().unwrap()[0].project, "Schniz/fnm");
    }

    #[test]
    fn fnm_falls_back_to_alias_when_no_version() {
        let runner = MockRunner::new()
            .expect(
                "/tmp/bin/fnm install --lts",
                CommandOutput { status: 0, stdout: "done\n".into(), stderr: String::new() },
            )
            .expect(
                "/tmp/bin/fnm default lts-latest",
                CommandOutput { status: 0, stdout: String::new(), stderr: String::new() },
            );
        let http = MockHttp::new();
        let engine = RecordingEngine { reqs: Mutex::new(Vec::new()) };
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            engine: &engine,
            install_dir: PathBuf::from("/tmp/bin"),
            go_root: PathBuf::from("/tmp/go"),
        };
        bootstrap(BootstrapTarget::Fnm, false, &ctx).unwrap();
    }

    #[test]
    fn parse_fnm_version_variants() {
        assert_eq!(
            parse_fnm_installed_version("Installing Node v22.14.0 (x64)").as_deref(),
            Some("v22.14.0")
        );
        assert_eq!(parse_fnm_installed_version("v20.11.1").as_deref(), Some("v20.11.1"));
        assert_eq!(parse_fnm_installed_version("no version here"), None);
        // Not a 3-part version.
        assert_eq!(parse_fnm_installed_version("v22"), None);
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
        let engine = RecordingEngine { reqs: Mutex::new(Vec::new()) };
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            engine: &engine,
            install_dir: PathBuf::from("/tmp/bin"),
            go_root: go_root.clone(),
        };
        bootstrap(BootstrapTarget::Go, false, &ctx).unwrap();
        assert!(go_root.join("bin").join("go").is_file());
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
        let engine = RecordingEngine { reqs: Mutex::new(Vec::new()) };
        let ctx = BootstrapCtx {
            runner: &runner,
            http: &http,
            engine: &engine,
            install_dir: PathBuf::from("/tmp/bin"),
            go_root: go_root.clone(),
        };
        let err = bootstrap(BootstrapTarget::Go, false, &ctx).unwrap_err();
        assert!(err.to_string().contains("verifying"), "{err}");
        // Nothing extracted.
        assert!(!go_root.exists());
    }
}
