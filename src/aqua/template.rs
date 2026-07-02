//! A tiny Go-template SUBSET renderer for aqua asset/file templates (plan §7).
//!
//! Supported constructs inside `{{ … }}`:
//!   * variables: `.OS` `.Arch` `.Format` `.Version` `.AssetWithoutExt`
//!   * function `trimV` in either form: `{{trimV .Version}}` or `{{.Version | trimV}}`
//!
//! Any other variable, function, or a multi-function pipeline is a HARD error
//! (`bail!`) that names the offending token — the caller degrades to "write the
//! `github:` entry by hand". This deliberately mirrors the strict, tokens-only
//! posture of `sources::template::render_template`.
//!
//! `replacements` (e.g. `amd64 → x86_64`, `darwin → apple-darwin`) are applied
//! to the OS/Arch/Format *token values* BEFORE substitution.

use anyhow::{bail, Result};

/// The resolved token values for one platform, post-`replacements`.
///
/// `version` is the already-prefix-stripped version string (see plan §7:
/// `version_prefix` is removed *before* `trimV`). `format` is the merged format
/// (may be empty for `format: raw`).
#[derive(Debug, Clone)]
pub struct Ctx {
    pub os: String,
    pub arch: String,
    pub format: String,
    pub version: String,
    /// Rendered asset with the trailing `.<format>` stripped (plan §7). Set to
    /// the rendered asset itself for `format: raw`. `None` while rendering the
    /// asset itself (before it is known) → referencing it there is an error.
    pub asset_without_ext: Option<String>,
}

/// Strip a single leading `v` (Go template `trimV`).
pub fn trim_v(s: &str) -> &str {
    s.strip_prefix('v').unwrap_or(s)
}

/// Render one `{{ … }}`-bearing template string against `ctx`.
///
/// `what` is a human label for errors (e.g. `asset` / `files.src`).
pub fn render(template: &str, ctx: &Ctx, what: &str) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            bail!("unterminated `{{{{` in {what} template `{template}`");
        };
        let expr = after[..end].trim();
        out.push_str(&eval_expr(expr, ctx, what, template)?);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Evaluate the inside of a `{{ … }}` action to its string value.
fn eval_expr(expr: &str, ctx: &Ctx, what: &str, template: &str) -> Result<String> {
    // Pipeline form: `X | f` (single stage only).
    if let Some((lhs, rhs)) = expr.split_once('|') {
        let lhs = lhs.trim();
        let rhs = rhs.trim();
        // Reject multi-stage pipelines (`a | b | c`) — unsupported.
        if rhs.contains('|') {
            bail!(
                "unsupported multi-function pipeline `{{{{{expr}}}}}` in {what} template `{template}`"
            );
        }
        let func = rhs;
        if func != "trimV" {
            bail!(
                "unknown template function `{func}` in `{{{{{expr}}}}}` in {what} template `{template}`"
            );
        }
        let v = eval_var(lhs, ctx, what, template)?;
        return Ok(trim_v(&v).to_string());
    }

    // Function-application form: `trimV .Version`.
    if let Some((func, arg)) = split_func_call(expr) {
        if func != "trimV" {
            bail!(
                "unknown template function `{func}` in `{{{{{expr}}}}}` in {what} template `{template}`"
            );
        }
        let v = eval_var(arg, ctx, what, template)?;
        return Ok(trim_v(&v).to_string());
    }

    // Bare variable.
    eval_var(expr, ctx, what, template)
}

/// If `expr` is `word arg` (two space-separated tokens, first not a `.field`),
/// return `(word, arg)`; else `None`.
fn split_func_call(expr: &str) -> Option<(&str, &str)> {
    let mut it = expr.split_whitespace();
    let first = it.next()?;
    let second = it.next()?;
    // A trailing extra token means an unsupported shape; let eval_var reject it
    // by returning it as a "variable" (which will fail). Here we only treat the
    // simple two-token `func arg` case as a call, and only when `func` isn't a
    // field access.
    if it.next().is_some() {
        return None;
    }
    if first.starts_with('.') {
        return None;
    }
    Some((first, second))
}

/// Resolve a single `.Field` variable to its string value.
fn eval_var(var: &str, ctx: &Ctx, what: &str, template: &str) -> Result<String> {
    Ok(match var {
        ".OS" => ctx.os.clone(),
        ".Arch" => ctx.arch.clone(),
        ".Format" => ctx.format.clone(),
        ".Version" => ctx.version.clone(),
        ".AssetWithoutExt" => match &ctx.asset_without_ext {
            Some(s) => s.clone(),
            None => bail!(
                "`.AssetWithoutExt` is not available while rendering the asset itself \
                 ({what} template `{template}`)"
            ),
        },
        other => bail!(
            "unknown template token `{{{{{other}}}}}` in {what} template `{template}` \
             (supported: .OS .Arch .Format .Version .AssetWithoutExt, function trimV)"
        ),
    })
}

/// Derive `.AssetWithoutExt` from a rendered asset name and the merged format
/// (plan §7): strip a trailing `.<format>` only when `format` is non-empty and
/// the asset actually ends with it. For `format: raw` (empty format), the value
/// equals the asset itself (no strip).
pub fn asset_without_ext(rendered_asset: &str, format: &str) -> String {
    if format.is_empty() {
        return rendered_asset.to_string();
    }
    let suffix = format!(".{format}");
    match rendered_asset.strip_suffix(&suffix) {
        Some(stem) => stem.to_string(),
        None => rendered_asset.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(os: &str, arch: &str, format: &str, version: &str) -> Ctx {
        Ctx {
            os: os.into(),
            arch: arch.into(),
            format: format.into(),
            version: version.into(),
            asset_without_ext: None,
        }
    }

    #[test]
    fn codex_asset_zst() {
        // asset: codex-{{.Arch}}-{{.OS}}.{{.Format}} with replacements applied.
        let c = ctx("unknown-linux-musl", "x86_64", "zst", "0.20.0");
        let asset = render("codex-{{.Arch}}-{{.OS}}.{{.Format}}", &c, "asset").unwrap();
        assert_eq!(asset, "codex-x86_64-unknown-linux-musl.zst");
        // AssetWithoutExt strips the `.zst`.
        assert_eq!(asset_without_ext(&asset, "zst"), "codex-x86_64-unknown-linux-musl");
    }

    #[test]
    fn codex_files_src_uses_asset_without_ext() {
        let mut c = ctx("unknown-linux-musl", "x86_64", "zst", "0.20.0");
        c.asset_without_ext = Some("codex-x86_64-unknown-linux-musl".into());
        let src = render("{{.AssetWithoutExt}}", &c, "files.src").unwrap();
        assert_eq!(src, "codex-x86_64-unknown-linux-musl");
    }

    #[test]
    fn gh_version_asset_and_trimv_func() {
        // gh: gh_{{trimV .Version}}_{{.OS}}_{{.Arch}}.{{.Format}}
        let c = ctx("linux", "amd64", "tar.gz", "v2.65.0");
        let asset = render("gh_{{trimV .Version}}_{{.OS}}_{{.Arch}}.{{.Format}}", &c, "asset").unwrap();
        assert_eq!(asset, "gh_2.65.0_linux_amd64.tar.gz");
    }

    #[test]
    fn trimv_pipeline_form_equivalent() {
        let c = ctx("linux", "amd64", "tar.gz", "v2.65.0");
        let piped = render("gh_{{.Version | trimV}}_{{.OS}}", &c, "asset").unwrap();
        assert_eq!(piped, "gh_2.65.0_linux");
    }

    #[test]
    fn raw_format_asset_without_ext_is_asset_itself() {
        // format: raw → empty format token, no extension strip.
        let c = ctx("linux", "amd64", "", "1.0.0");
        let asset = render("{{.Arch}}-{{.OS}}-eza", &c, "asset").unwrap();
        assert_eq!(asset, "amd64-linux-eza");
        assert_eq!(asset_without_ext(&asset, ""), "amd64-linux-eza");
    }

    #[test]
    fn unknown_token_is_hard_error() {
        let c = ctx("linux", "amd64", "tar.gz", "1.0.0");
        let err = render("x-{{.SemVer}}", &c, "asset").unwrap_err();
        assert!(err.to_string().contains(".SemVer"), "{err}");
    }

    #[test]
    fn unknown_function_is_hard_error() {
        let c = ctx("linux", "amd64", "tar.gz", "1.0.0");
        let err = render("{{toLower .OS}}", &c, "asset").unwrap_err();
        assert!(err.to_string().contains("toLower"), "{err}");
    }

    #[test]
    fn multi_stage_pipeline_rejected() {
        let c = ctx("linux", "amd64", "tar.gz", "1.0.0");
        let err = render("{{.Version | trimV | toLower}}", &c, "asset").unwrap_err();
        assert!(err.to_string().contains("multi-function pipeline"), "{err}");
    }

    #[test]
    fn asset_without_ext_only_strips_matching_suffix() {
        assert_eq!(asset_without_ext("foo.tar.gz", "tar.gz"), "foo");
        // Suffix mismatch → unchanged.
        assert_eq!(asset_without_ext("foo.zip", "tar.gz"), "foo.zip");
    }
}
