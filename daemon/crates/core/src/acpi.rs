//! `/proc/acpi/call`: one file, one lock.
//!
//! `acpi_call` exposes a *single global* interface. A call is a write
//! followed by a read of the same file, and the two are not tied together
//! by anything - if a second process writes between our write and our
//! read, we read its answer and it reads ours. A short-lived CLI gets away
//! with that; a daemon with a control loop does not.
//!
//! So every use of the file in this process goes through [`call`], which
//! holds [`GATE`] across the write/read pair. This lives in `core` rather
//! than in a module because more than one module needs it: the RGB
//! lightbar drives the light strip through it, and the fan cleaner (not
//! ported yet) drives reverse spin through it. Two modules serialising
//! against two different mutexes would be two modules not serialising at
//! all.
//!
//! The lock is per *process*, which is the scope that is ours to control.
//! Another program on the machine using `acpi_call` at the same moment is
//! outside it, and nothing short of the kernel could fix that.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// The interface, when the kernel module is loaded.
pub const CALL_PATH: &str = "/proc/acpi/call";

/// What to tell a user whose machine has no `acpi_call`. Written once
/// because it is the entire remedy, and an error that names the hardware
/// but not the package sends people to the wrong place.
pub const INSTALL_HINT: &str = "install the acpi_call kernel module: \
     'sudo pacman -S acpi_call-dkms' on Arch, \
     'sudo apt install acpi_call-dkms' on Debian/Ubuntu, \
     'sudo dnf install akmod-acpi_call' on Fedora";

/// Serialises the write/read pair. See the module docs.
static GATE: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
pub enum AcpiError {
    /// `/proc/acpi/call` is not there. Installable, not permanent.
    #[error("the acpi_call kernel module is not loaded, so {CALL_PATH} does not exist; {INSTALL_HINT}")]
    NotLoaded,
    /// The file exists and this process may not write to it.
    #[error("writing {CALL_PATH} needs root")]
    PermissionDenied,
    #[error("{CALL_PATH}: {0}")]
    Io(String),
}

impl AcpiError {
    /// The same sentence, translatable. The `Io` variant carries a raw OS
    /// error, which is passed through as a param rather than translated.
    pub fn to_msg(&self) -> crate::Msg {
        match self {
            Self::NotLoaded => crate::msg!(
                "acpi.notLoaded",
                { "path" => CALL_PATH, "hint" => INSTALL_HINT },
                "the acpi_call kernel module is not loaded, so {path} does not exist; {hint}"
            ),
            Self::PermissionDenied => crate::msg!(
                "acpi.needsRoot",
                { "path" => CALL_PATH },
                "writing {path} needs root"
            ),
            Self::Io(e) => crate::msg!(
                "acpi.io",
                { "path" => CALL_PATH, "error" => e.clone() },
                "{path}: {error}"
            ),
        }
    }
}

/// Where the interface is. `PYREN_ACPI_CALL` redirects it at a plain file,
/// which is how the request framing is exercised in a test on a machine
/// with no `acpi_call` - including in CI.
pub fn call_path() -> String {
    std::env::var("PYREN_ACPI_CALL").unwrap_or_else(|_| CALL_PATH.to_string())
}

fn is_redirected() -> bool {
    std::env::var_os("PYREN_ACPI_CALL").is_some()
}

/// Whether the interface is there *now*. Never loads anything: probing is
/// a question, and a question should not change the answer.
pub fn is_loaded() -> bool {
    Path::new(&call_path()).exists()
}

/// Whether the module is installed but not loaded - the state a machine is
/// in between `pacman -S acpi_call-dkms` and the next `modprobe`.
///
/// Told apart from "not installed at all" because they have different
/// remedies, and a message offering the wrong one costs an evening.
pub fn is_module_installed() -> bool {
    std::process::Command::new("modinfo")
        .args(["-n", "acpi_call"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Loads `acpi_call` if it is installed and we are root.
///
/// Only ever called from a path where the user has asked for something
/// that needs it. The daemon does not modprobe at startup: loading a
/// kernel module is a change to the machine, and this project does not
/// make those on its own (see `dev/TODO.md` §4, "the daemon does not touch
/// the fans until asked").
pub fn ensure_loaded() -> Result<(), AcpiError> {
    if is_loaded() {
        return Ok(());
    }
    let _ = std::process::Command::new("modprobe")
        .arg("acpi_call")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if is_loaded() {
        Ok(())
    } else {
        Err(AcpiError::NotLoaded)
    }
}

/// One ACPI call: write `<method> <args>`, then read the reply.
///
/// Holds [`GATE`] across both halves, which is the whole reason this
/// function exists rather than each caller opening the file itself.
pub fn call(method: &str, args: &str) -> Result<String, AcpiError> {
    let path = call_path();
    let request = format!("{method} {args}");

    let _guard = GATE.lock().unwrap_or_else(|e| e.into_inner());

    let mut file = fs::OpenOptions::new()
        .write(true)
        // procfs ignores truncation; a redirect target is a real file and
        // would otherwise keep the tail of a longer previous request.
        .truncate(is_redirected())
        .create(is_redirected())
        .open(&path)
        .map_err(map_open_error)?;
    file.write_all(request.as_bytes()).map_err(|e| map_io_error(&e))?;
    drop(file);

    let response = fs::read_to_string(&path).map_err(|e| map_io_error(&e))?;
    // acpi_call terminates its reply with a NUL, which `trim` does not
    // remove and `str::parse` chokes on.
    Ok(response.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string())
}

fn map_open_error(e: std::io::Error) -> AcpiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AcpiError::NotLoaded,
        std::io::ErrorKind::PermissionDenied => AcpiError::PermissionDenied,
        _ => AcpiError::Io(e.to_string()),
    }
}

fn map_io_error(e: &std::io::Error) -> AcpiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AcpiError::NotLoaded,
        std::io::ErrorKind::PermissionDenied => AcpiError::PermissionDenied,
        _ => AcpiError::Io(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not three: `PYREN_ACPI_CALL` is process-global and the
    /// test harness runs threads in parallel, so cases that set it have to
    /// be cases that cannot run at the same time as each other.
    #[test]
    fn the_request_framing_and_the_absent_interface() {
        let dir = std::env::temp_dir().join(format!("pyren-acpi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("call");
        std::env::set_var("PYREN_ACPI_CALL", &path);

        // The request is one line, `<method> <args>`, with exactly one
        // space: acpi_call parses positionally, and a stray separator
        // shifts every argument along by one. Reading a regular file back
        // returns the request itself, which is the thing under test.
        let echoed = call("\\_SB.WMID.WMAA", "0 3 b53454355").expect("a plain file accepts it");
        assert_eq!(echoed, "\\_SB.WMID.WMAA 0 3 b53454355");

        // A shorter request must not leave the tail of a longer one behind.
        assert_eq!(call("\\_SB", "0").unwrap(), "\\_SB 0");

        std::env::set_var("PYREN_ACPI_CALL", dir.join("definitely-not-here/call"));
        assert!(
            matches!(call("\\_SB", "0"), Err(AcpiError::NotLoaded)),
            "a missing interface is 'not loaded', which names a fix, not a bare io error"
        );

        std::env::remove_var("PYREN_ACPI_CALL");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
