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
    pub fn install_dir_path(&self) -> Result<PathBuf> {
        paths::expand(&self.install_dir)
    }

    /// Parse `default_source` into a [`SourceKind`].
    pub fn default_source_kind(&self) -> Result<SourceKind> {
        self.default_source
            .parse::<SourceKind>()
            .with_context(|| format!("invalid default_source `{}`", self.default_source))
    }
}

/// A `[tools.<name>]` entry. All source-specific optional fields (§4.4) are
/// wired even when a given source is not yet implemented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolConfig {
    /// The `$source:$locator` spec (§4.2). Required.
    pub spec: String,

    // ---- release-family (github/gitlab/url) ----
    /// Case-sensitive substring used to disambiguate release assets (ubi
    /// `.matching()`, `asset_name.contains(..)`); not glob/regex.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub matching: Option<String>,
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
}

impl ToolConfig {
    pub fn from_spec(spec: impl Into<String>) -> Self {
        Self {
            spec: spec.into(),
            ..Default::default()
        }
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

    #[test]
    fn roundtrip_serde() {
        let mut cfg = Config::default();
        let mut eza = ToolConfig::from_spec("github:eza-community/eza");
        eza.matching = Some("linux-musl".into());
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
        assert_eq!(cfg.tools["codex"].matching.as_deref(), Some("codex-x86_64-unknown-linux"));
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
