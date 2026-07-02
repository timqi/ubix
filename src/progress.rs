//! Progress logging to STDERR (stdout stays clean for the machine-facing result).
//!
//! A global verbosity level is set once at startup from the CLI flags; the
//! `step!`/`detail!` macros consult it. A global keeps source-handler signatures
//! unchanged (no logger threaded through every function).

use std::sync::atomic::{AtomicU8, Ordering};

/// Verbosity level, ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verbosity {
    /// `--quiet`: suppress step lines (errors still surface via the process).
    Quiet = 0,
    /// Default: show `==>` step lines, hide noisy dependency logs.
    Normal = 1,
    /// `--verbose`: step lines plus extra detail and dependency (`log`) output.
    Verbose = 2,
}

impl Verbosity {
    fn from_u8(v: u8) -> Verbosity {
        match v {
            0 => Verbosity::Quiet,
            2 => Verbosity::Verbose,
            _ => Verbosity::Normal,
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Verbosity::Normal as u8);

/// Set the global verbosity (called once from `main`/`App::new`).
pub fn set_verbosity(v: Verbosity) {
    LEVEL.store(v as u8, Ordering::Relaxed);
}

/// Current global verbosity.
pub fn verbosity() -> Verbosity {
    Verbosity::from_u8(LEVEL.load(Ordering::Relaxed))
}

/// Whether step lines should be shown (Normal or Verbose).
pub fn show_steps() -> bool {
    verbosity() >= Verbosity::Normal
}

/// Whether extra detail should be shown (Verbose only).
pub fn show_detail() -> bool {
    verbosity() >= Verbosity::Verbose
}

/// Print a key step line to stderr, prefixed with `==> ` (shown at Normal+).
#[macro_export]
macro_rules! step {
    ($($arg:tt)*) => {{
        if $crate::progress::show_steps() {
            eprintln!("==> {}", format!($($arg)*));
        }
    }};
}

/// Print an extra-detail line to stderr, prefixed with `    ` (shown at Verbose).
#[macro_export]
macro_rules! detail {
    ($($arg:tt)*) => {{
        if $crate::progress::show_detail() {
            eprintln!("    {}", format!($($arg)*));
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests mutate the global level; serialize them.
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn quiet_hides_steps_and_detail() {
        let _g = LOCK.lock().unwrap();
        set_verbosity(Verbosity::Quiet);
        assert!(!show_steps());
        assert!(!show_detail());
        set_verbosity(Verbosity::Normal); // restore
    }

    #[test]
    fn normal_shows_steps_not_detail() {
        let _g = LOCK.lock().unwrap();
        set_verbosity(Verbosity::Normal);
        assert!(show_steps());
        assert!(!show_detail());
    }

    #[test]
    fn verbose_shows_both() {
        let _g = LOCK.lock().unwrap();
        set_verbosity(Verbosity::Verbose);
        assert!(show_steps());
        assert!(show_detail());
        set_verbosity(Verbosity::Normal); // restore
    }

    #[test]
    fn ordering_is_sane() {
        assert!(Verbosity::Quiet < Verbosity::Normal);
        assert!(Verbosity::Normal < Verbosity::Verbose);
    }
}
