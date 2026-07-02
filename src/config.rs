//! `config.toml` model, parsing, and validation (§4.2–§4.6).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::sources::{parse_spec, ParsedSpec, SourceKind};

/// Current config schema version (§4.6, D13).
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Top-level `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    #[serde(default)]
    pub settings: Settings,

    /// `[tools.<name>]` table. Uses `BTreeMap` for deterministic ordering.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolConfig>,
}

fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            settings: Settings::default(),
            tools: BTreeMap::new(),
        }
    }
}

/// `[settings]` block (§4.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default = "default_install_dir")]
    pub install_dir: String,
    #[serde(default = "default_go_root")]
    pub go_root: String,
    #[serde(default = "default_default_source")]
    pub default_source: String,
}

fn default_install_dir() -> String {
    "~/.local/bin".to_string()
}
fn default_go_root() -> String {
    "~/.local/share/go".to_string()
}
fn default_default_source() -> String {
    "github".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            install_dir: default_install_dir(),
            go_root: default_go_root(),
            default_source: default_default_source(),
        }
    }
}

impl Settings {
    /// Resolve `install_dir` to an absolute path with `~`/`$XDG` expansion.
    pub fn install_dir_path(&self) -> PathBuf {
        paths::expand(&self.install_dir)
    }

    /// Parse `default_source` into a [`SourceKind`].
    pub fn default_source_kind(&self) -> Result<SourceKind> {
        self.default_source
            .parse::<SourceKind>()
            .with_context(|| format!("invalid default_source `{}`", self.default_source))
    }
}

/// A value that is either a single string or a per-platform table, so one
/// dotfile config works across linux/mac. Used for `matching` (§4.4).
///
/// Deserialization is `#[serde(untagged)]`: a bare TOML string parses to
/// [`PlatformString::One`] (backward-compatible), a table to `PerPlatform`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PlatformString {
    /// A single value for all platforms.
    One(String),
    /// Platform-keyed values (`"{goos}-{goarch}"` / `"{goos}"` / `"*"`|`"default"`).
    PerPlatform(BTreeMap<String, String>),
}

impl PlatformString {
    /// Resolve the value for `goos`/`goarch`.
    ///
    /// * `One(s)` → `Some(s)`.
    /// * `PerPlatform`: most-specific first: `"{goos}-{goarch}"` → `"{goos}"` →
    ///   `"*"` → `"default"`. An empty-string value means "no filter" → `None`.
    ///   No applicable key and no `*`/`default` fallback → error.
    pub fn resolve(&self, goos: &str, goarch: &str) -> Result<Option<String>> {
        match self {
            PlatformString::One(s) => Ok(nonempty(s)),
            PlatformString::PerPlatform(map) => {
                let keys = [
                    format!("{goos}-{goarch}"),
                    goos.to_string(),
                    "*".to_string(),
                    "default".to_string(),
                ];
                for k in &keys {
                    if let Some(v) = map.get(k) {
                        return Ok(nonempty(v));
                    }
                }
                bail!(
                    "no `matching` entry for platform `{goos}-{goarch}`; \
                     add that key or a `*` fallback"
                );
            }
        }
    }
}

/// `Some(s)` unless `s` is empty (empty = "no matching filter" sentinel).
fn nonempty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// A `[tools.<name>]` entry. All source-specific optional fields (§4.4) are
/// wired even when a given source is not yet implemented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolConfig {
    /// The `$source:$locator` spec (§4.2). Required.
    pub spec: String,

    // ---- release-family (github/gitlab/url) ----
    /// Case-sensitive substring to disambiguate release assets (ubi
    /// `.matching()`, `asset_name.contains(..)`; not glob/regex). Either a single
    /// string or a per-platform table; supports `{os}`/`{arch}` tokens.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub matching: Option<PlatformString>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rename: Option<String>,

    // ---- github/gitlab ----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tag: Option<String>,

    // ---- gitlab ----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub host: Option<String>,

    // ---- pypi/npm/cargo ----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version: Option<String>,

    // ---- pypi ----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub extras: Option<Vec<String>>,
    #[serde(rename = "with", skip_serializing_if = "Option::is_none", default)]
    pub with: Option<Vec<String>>,

    // ---- cargo ----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub features: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub locked: Option<bool>,

    // ---- url templating (setting any of these makes a `url:` tool templated;
    //      legacy prefixes `template:`/`http:` are aliases for `url:`) ----
    /// Alternate URL template used on Linux+musl (glibc uses the primary `url`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub url_musl: Option<String>,
    /// Where to discover `{version}` when not pinned, e.g. `github:owner/repo`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version_source: Option<String>,
    /// Runtime-arch → URL-token overrides applied before `{arch}` substitution
    /// (e.g. `amd64 = "x64"`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub arch_replace: Option<BTreeMap<String, String>>,
    /// Runtime-os → URL-token overrides applied before `{os}` substitution.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub os_replace: Option<BTreeMap<String, String>>,
}

impl ToolConfig {
    pub fn from_spec(spec: impl Into<String>) -> Self {
        Self {
            spec: spec.into(),
            ..Default::default()
        }
    }

    /// Resolve `matching` for `goos`/`goarch`: pick the platform value, then
    /// substitute `{os}`/`{arch}` tokens (applying `os_replace`/`arch_replace`).
    /// Returns `None` when no filter applies (bare/empty → let ubi decide).
    pub fn resolved_matching(&self, goos: &str, goarch: &str) -> Result<Option<String>> {
        let Some(pm) = &self.matching else {
            return Ok(None);
        };
        let Some(raw) = pm.resolve(goos, goarch)? else {
            return Ok(None);
        };
        let rendered = crate::sources::template::render_os_arch(
            &raw,
            goos,
            goarch,
            self.os_replace.as_ref(),
            self.arch_replace.as_ref(),
        )?;
        Ok(Some(rendered))
    }
}

impl Config {
    /// Load config from `path`. Returns `Ok(None)` if the file does not exist.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.check_schema()?;
        cfg.validate()?;
        Ok(Some(cfg))
    }

    /// Load config, or the default (empty) config if absent.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        Ok(Self::load(path)?.unwrap_or_default())
    }

    /// Serialize and write to `path` atomically-ish (write temp + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        paths::ensure_parent_dir(path)?;
        let text = toml::to_string_pretty(self).context("serializing config")?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }

    /// Enforce the schema-version migration policy (§4.6).
    fn check_schema(&self) -> Result<()> {
        match self.schema_version.cmp(&CONFIG_SCHEMA_VERSION) {
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Less => {
                // config is a human file → do not rewrite, only proceed (M1 has
                // no prior version, so nothing to migrate). A note is fine.
                Ok(())
            }
            std::cmp::Ordering::Greater => bail!(
                "config.toml schema_version {} is newer than this ubix supports ({}); \
                 please upgrade ubix",
                self.schema_version,
                CONFIG_SCHEMA_VERSION
            ),
        }
    }

    /// Validate every tool spec parses under the effective default source.
    fn validate(&self) -> Result<()> {
        let default_source = self.settings.default_source_kind()?;
        for (name, tool) in &self.tools {
            self.parse_tool_spec(tool, default_source)
                .with_context(|| format!("tool `{name}`"))?;
        }
        Ok(())
    }

    /// Parse a tool's spec using this config's default source.
    pub fn parse_tool_spec(
        &self,
        tool: &ToolConfig,
        default_source: SourceKind,
    ) -> Result<ParsedSpec> {
        parse_spec(&tool.spec, default_source)
    }

    /// Convenience: parse a tool's spec with the config's own default source.
    pub fn parsed_spec(&self, tool: &ToolConfig) -> Result<ParsedSpec> {
        let ds = self.settings.default_source_kind()?;
        self.parse_tool_spec(tool, ds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PlatformString ----

    #[test]
    fn platform_string_deserializes_bare_string_to_one() {
        #[derive(Deserialize)]
        struct W {
            m: PlatformString,
        }
        let w: W = toml::from_str(r#"m = "linux-musl""#).unwrap();
        assert_eq!(w.m, PlatformString::One("linux-musl".into()));
    }

    #[test]
    fn platform_string_deserializes_table_to_per_platform() {
        #[derive(Deserialize)]
        struct W {
            m: PlatformString,
        }
        let w: W = toml::from_str(
            r#"
[m]
linux-amd64 = "a"
darwin-arm64 = "b"
"#,
        )
        .unwrap();
        match w.m {
            PlatformString::PerPlatform(map) => {
                assert_eq!(map["linux-amd64"], "a");
                assert_eq!(map["darwin-arm64"], "b");
            }
            _ => panic!("expected PerPlatform"),
        }
    }

    #[test]
    fn platform_string_serde_roundtrip_both_forms() {
        for ps in [
            PlatformString::One("x".into()),
            PlatformString::PerPlatform(
                [("linux".to_string(), "y".to_string())].into_iter().collect(),
            ),
        ] {
            #[derive(Serialize, Deserialize, PartialEq, Debug)]
            struct W {
                m: PlatformString,
            }
            let w = W { m: ps.clone() };
            let text = toml::to_string(&w).unwrap();
            let back: W = toml::from_str(&text).unwrap();
            assert_eq!(back.m, ps);
        }
    }

    #[test]
    fn resolve_one_returns_value() {
        assert_eq!(
            PlatformString::One("s".into()).resolve("linux", "amd64").unwrap(),
            Some("s".to_string())
        );
    }

    #[test]
    fn resolve_precedence_os_arch_over_os_over_star() {
        let map: BTreeMap<String, String> = [
            ("linux-amd64", "exact"),
            ("linux", "os"),
            ("*", "fallback"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let ps = PlatformString::PerPlatform(map);
        assert_eq!(ps.resolve("linux", "amd64").unwrap(), Some("exact".into()));
        // no exact → os key.
        assert_eq!(ps.resolve("linux", "arm64").unwrap(), Some("os".into()));
        // neither → * fallback.
        assert_eq!(ps.resolve("darwin", "arm64").unwrap(), Some("fallback".into()));
    }

    #[test]
    fn resolve_default_key_as_fallback() {
        let ps = PlatformString::PerPlatform(
            [("default".to_string(), "d".to_string())].into_iter().collect(),
        );
        assert_eq!(ps.resolve("windows", "amd64").unwrap(), Some("d".into()));
    }

    #[test]
    fn resolve_empty_string_means_no_filter() {
        let ps = PlatformString::PerPlatform(
            [("linux-amd64".to_string(), String::new())].into_iter().collect(),
        );
        assert_eq!(ps.resolve("linux", "amd64").unwrap(), None);
    }

    #[test]
    fn resolve_missing_platform_errors() {
        let ps = PlatformString::PerPlatform(
            [("linux-amd64".to_string(), "a".to_string())].into_iter().collect(),
        );
        let err = ps.resolve("darwin", "arm64").unwrap_err();
        assert!(err.to_string().contains("no `matching` entry for platform `darwin-arm64`"), "{err}");
    }

    #[test]
    fn resolved_matching_applies_arch_replace_tokens() {
        let mut tool = ToolConfig::from_spec("github:o/r");
        tool.matching = Some(PlatformString::One("linux-{arch}".into()));
        let mut am = BTreeMap::new();
        am.insert("amd64".to_string(), "x64".to_string());
        tool.arch_replace = Some(am);
        assert_eq!(
            tool.resolved_matching("linux", "amd64").unwrap(),
            Some("linux-x64".to_string())
        );
        // Without a mapping the token passes through unchanged.
        assert_eq!(
            tool.resolved_matching("linux", "arm64").unwrap(),
            Some("linux-arm64".to_string())
        );
    }

    #[test]
    fn resolved_matching_none_when_unset() {
        let tool = ToolConfig::from_spec("github:o/r");
        assert_eq!(tool.resolved_matching("linux", "amd64").unwrap(), None);
    }

    #[test]
    fn resolved_matching_unknown_token_errors() {
        let mut tool = ToolConfig::from_spec("github:o/r");
        tool.matching = Some(PlatformString::One("linux-{libc}".into()));
        assert!(tool.resolved_matching("linux", "amd64").is_err());
    }

    #[test]
    fn resolved_matching_cross_platform_codex_example() {
        let text = r#"
[tools.codex]
spec = "github:openai/codex"
[tools.codex.matching]
linux-amd64  = "codex-x86_64-unknown-linux-musl.tar.gz"
linux-arm64  = "codex-aarch64-unknown-linux-musl.tar.gz"
darwin-amd64 = "codex-x86_64-apple-darwin.tar.gz"
darwin-arm64 = "codex-aarch64-apple-darwin.zst"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        let codex = &cfg.tools["codex"];
        assert_eq!(
            codex.resolved_matching("linux", "amd64").unwrap().as_deref(),
            Some("codex-x86_64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(
            codex.resolved_matching("darwin", "arm64").unwrap().as_deref(),
            Some("codex-aarch64-apple-darwin.zst")
        );
        // A platform not in the table errors (oversight guard).
        assert!(codex.resolved_matching("windows", "amd64").is_err());
    }

    #[test]
    fn roundtrip_serde() {
        let mut cfg = Config::default();
        let mut eza = ToolConfig::from_spec("github:eza-community/eza");
        eza.matching = Some(PlatformString::One("linux-musl".into()));
        eza.exe = Some("eza".into());
        cfg.tools.insert("eza".into(), eza);
        cfg.tools
            .insert("selfhosted".into(), ToolConfig::from_spec("gitlab:group/sub/repo"));

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.schema_version, CONFIG_SCHEMA_VERSION);
    }

    #[test]
    fn roundtrip_template_fields() {
        let mut cfg = Config::default();
        let mut claude = ToolConfig::from_spec(
            "template:https://h/claude-code-releases/{version}/{os}-{arch}/claude",
        );
        claude.url_musl = Some("https://h/{version}/{os}-{arch}-musl/claude".into());
        claude.version_source = Some("github:anthropics/claude-code".into());
        claude.exe = Some("claude".into());
        let mut am = BTreeMap::new();
        am.insert("amd64".to_string(), "x64".to_string());
        claude.arch_replace = Some(am);
        cfg.tools.insert("claude".into(), claude);

        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
        let c = &back.tools["claude"];
        assert_eq!(c.version_source.as_deref(), Some("github:anthropics/claude-code"));
        assert_eq!(c.arch_replace.as_ref().unwrap()["amd64"], "x64");
        // Config validation accepts the http spec.
        back.validate().unwrap();
    }

    #[test]
    fn parses_full_example() {
        let text = r#"
schema_version = 1

[settings]
install_dir = "~/.local/bin"
default_source = "github"

[tools.eza]
spec = "github:eza-community/eza"

[tools.codex]
spec = "github:openai/codex"
matching = "codex-x86_64-unknown-linux"
exe = "codex"

[tools.ruff]
spec = "pypi:ruff"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        cfg.check_schema().unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.tools.len(), 3);
        // A bare string still deserializes to PlatformString::One (back-compat).
        assert_eq!(
            cfg.tools["codex"].matching,
            Some(PlatformString::One("codex-x86_64-unknown-linux".into()))
        );
    }

    #[test]
    fn refuse_higher_schema() {
        let text = "schema_version = 2\n";
        let cfg: Config = toml::from_str(text).unwrap();
        let err = cfg.check_schema().unwrap_err();
        assert!(err.to_string().contains("newer than this ubix"), "{err}");
    }

    #[test]
    fn accept_lower_schema() {
        let text = "schema_version = 0\n";
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.check_schema().is_ok());
    }

    #[test]
    fn invalid_tool_spec_rejected_on_validate() {
        let text = r#"
[tools.bad]
spec = "ruff"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn missing_settings_uses_defaults() {
        let cfg: Config = toml::from_str("schema_version = 1\n").unwrap();
        assert_eq!(cfg.settings.install_dir, "~/.local/bin");
        assert_eq!(cfg.settings.default_source, "github");
    }
}
