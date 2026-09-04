//! A logger for the daemon: four levels, one environment variable, no
//! dependencies.
//!
//! The problem it solves is narrow and real. The daemon's modules print
//! roughly twenty lines between them - a config file that loaded, a write
//! that needed root, a supervisor that moved the mode - and under systemd
//! all of them land in the journal at the same volume, whether the
//! machine is being diagnosed or has been running for a month. `PYREN_LOG`
//! turns that down to what went wrong, or up to everything.
//!
//! ```sh
//! PYREN_LOG=warn  pyren-daemon    # only what went wrong
//! PYREN_LOG=debug pyren-daemon    # everything, including per-tick detail
//! ```
//!
//! Three deliberate omissions, because a daemon this size does not need
//! them and each would cost a dependency: no timestamps (the journal
//! stamps every line already, and a duplicate stamp is noise), no
//! per-module filtering (`PYREN_LOG=power=debug` is a parser and a
//! lookup for a program with nine modules), and no writer indirection -
//! warnings and errors go to stderr, the rest to stdout, which is what
//! `systemd` and a terminal both expect.
//!
//! **What is not a log**: anything the user asked to see. `--check`'s
//! report, `--help`, the startup summary a person reads before deciding
//! whether the daemon is usable on their machine - those are the
//! program's output and are printed whatever the level says. Routing them
//! through here would mean `PYREN_LOG=warn` silently emptied a report
//! somebody ran on purpose.

use std::fmt;
use std::sync::OnceLock;

/// How much to say. Ordered, and `Level::Off` is what silences everything -
/// including errors, which is why it has to be asked for by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" => Some(Self::Off),
            "error" | "err" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" | "trace" => Some(Self::Debug),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Off => "",
            Self::Error => "error: ",
            Self::Warn => "warning: ",
            // Info is the level everything used to print at, and it keeps
            // reading the way it always has - a bare sentence after the
            // program name.
            Self::Info | Self::Debug => "",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        };
        f.write_str(name)
    }
}

static LEVEL: OnceLock<Level> = OnceLock::new();

/// The level in force, read from `PYREN_LOG` the first time anything logs.
///
/// A value that is not one of the names is `Info` rather than an error:
/// this is read from the environment of a service that may have been
/// edited by hand, and refusing to start over a typo in a logging variable
/// would be a worse outcome than logging slightly more than was asked for.
pub fn level() -> Level {
    *LEVEL.get_or_init(|| {
        std::env::var("PYREN_LOG").ok().and_then(|v| Level::parse(&v)).unwrap_or(Level::Info)
    })
}

/// Whether a message at this level would be printed. Worth checking before
/// building an expensive line; the macros do it for you.
pub fn enabled(level_wanted: Level) -> bool {
    level_wanted <= level()
}

/// Prints one line. Called by the macros; use those.
#[doc(hidden)]
pub fn emit(level_wanted: Level, message: fmt::Arguments<'_>) {
    if !enabled(level_wanted) {
        return;
    }
    let line = format!("pyren-daemon: {}{message}", level_wanted.tag());
    if level_wanted <= Level::Warn {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

/// Something did not work and somebody should know.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Error, format_args!($($arg)*))
    };
}

/// Something worked less well than it should have, or was skipped.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Warn, format_args!($($arg)*))
    };
}

/// The daemon did something worth a line in the journal.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Info, format_args!($($arg)*))
    };
}

/// Detail nobody wants until they are diagnosing this exact thing.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Debug, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_people_actually_type_are_all_understood() {
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse(" warning "), Some(Level::Warn));
        assert_eq!(Level::parse("err"), Some(Level::Error));
        assert_eq!(Level::parse("trace"), Some(Level::Debug));
        assert_eq!(Level::parse("off"), Some(Level::Off));
    }

    /// A typo in a logging variable must not be able to change what the
    /// daemon does, and "no logging at all" must be asked for by name.
    #[test]
    fn a_name_that_is_not_a_level_is_not_silence() {
        assert_eq!(Level::parse("quiet-ish"), None);
        assert_eq!(Level::parse(""), None);
    }

    /// The ordering is the filter, so it is worth pinning: everything is
    /// louder than off, and errors survive every level but that one.
    #[test]
    fn the_levels_are_ordered_from_silent_to_loud() {
        assert!(Level::Off < Level::Error);
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }

    #[test]
    fn warnings_and_errors_are_marked_and_ordinary_lines_are_not() {
        assert_eq!(Level::Error.tag(), "error: ");
        assert_eq!(Level::Warn.tag(), "warning: ");
        assert_eq!(Level::Info.tag(), "");
    }
}
