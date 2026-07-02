//! Toolchain / underlying-tool bootstrap (§6, D9). Stub for M1: recognizes the
//! targets and returns clear "not yet implemented" messages per milestone.

use anyhow::{bail, Result};

/// Bootstrap targets accepted by `ubix bootstrap <target>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapTarget {
    Rust,
    Go,
    Uv,
    Fnm,
}

impl BootstrapTarget {
    pub fn milestone(self) -> &'static str {
        match self {
            BootstrapTarget::Uv => "M3",
            BootstrapTarget::Fnm => "M4",
            BootstrapTarget::Rust | BootstrapTarget::Go => "M5",
        }
    }
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

/// Run the bootstrap. M1 is a stub.
pub fn bootstrap(target: BootstrapTarget, _reinstall: bool) -> Result<()> {
    bail!(
        "`bootstrap {target:?}` is not yet implemented ({}). \
         Toolchain/tool bootstrapping lands in a later milestone.",
        target.milestone()
    )
}
