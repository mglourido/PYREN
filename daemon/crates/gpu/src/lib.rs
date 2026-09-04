//! GPU MUX switching - iGPU only / hybrid / discrete - through the
//! patched `hp-wmi` driver's own sysfs file, no `supergfxctl` involved.
//!
//! | method | params | result |
//! |---|---|---|
//! | `gpu.getStatus` | none | current mode, or `null` if never set |
//! | `gpu.setMode` | `{ "mode": "integrated" \| "hybrid" \| "discrete" \| "optimus" }` | the new status |
//!
//! ## Why this is not a `supergfxctl` wrapper
//!
//! `driver/hp-wmi-omen/hp-wmi.c` - the driver this project already patches
//! and installs for fan control - exposes
//! `/sys/devices/platform/hp-wmi/gpu_mux_mode` as a plain `RW` attribute
//! that talks to `HPWMI_GRAPHICS_MUX_QUERY` over ACPI-WMI directly. Where
//! that file exists, wrapping a second daemon to do the same round trip
//! would only add a dependency and a place for the two to disagree about
//! whose idea of the mode is current. Confirmed present and readable on
//! the development machine - see `dev/FINDINGS.md`.
//!
//! ## Reading and writing use the same small integer, one is not a bitmask
//!
//! The kernel source defines `HPWMI_MUX_MODE_*` as bits
//! (`hybrid = BIT(1)`, `discrete = BIT(2)`, ...) but that encoding is used
//! **only** to check a requested mode against the firmware's supported-set
//! query before writing - the byte actually read from and written to the
//! file is the plain index into `mux_bitmask_map`: `0` hybrid, `1`
//! discrete, `2` optimus (NVIDIA render offload), `3` uma (integrated
//! only). This module writes and parses that index, never the bitmask.
//!
//! ## There is no userspace query for which modes this board supports
//!
//! The capability check (`HPWMI_GET_SYSTEM_DESIGN_DATA`) happens inside
//! the kernel, on write, and is not published as its own sysfs file. So
//! unlike [`rgb`](../pyren_rgb) - which can probe every dialect with a
//! read that changes nothing - this module cannot list what a board
//! offers without asking it to switch. A write the firmware refuses comes
//! back `EOPNOTSUPP`, which [`ModuleError::NotCapable`] reports by name
//! rather than as a bare I/O failure.

use std::io::ErrorKind as IoErrorKind;
use std::path::{Path, PathBuf};

use pyren_core::{msg, ErrorKind, Module, ModuleError, ModuleResult};
use serde::Serialize;
use serde_json::{json, Value};

/// Where the patched driver publishes it. `PYREN_GPU_MUX_PATH` points this
/// at a fixture file, the only way to exercise the read/write logic on a
/// machine with no such driver loaded.
const MUX_PATH: &str = "/sys/devices/platform/hp-wmi/gpu_mux_mode";

/// Linux's `EOPNOTSUPP`/`ENOTSUP` (the two are the same number on Linux).
/// `std::io::ErrorKind::Unsupported` is the stable spelling for this and
/// is matched first; the raw number is a fallback for whatever Rust
/// version maps it to `Other` instead, which older ones did.
const EOPNOTSUPP: i32 = 95;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuMuxMode {
    Hybrid,
    Discrete,
    /// NVIDIA's own render-offload variant of "discrete" - distinct in
    /// the firmware's encoding, not currently offered by the app's UI,
    /// which only has three cards (`integrated` / `hybrid` / `discrete`).
    Optimus,
    /// The firmware's name is `uma`; the app calls this `integrated`,
    /// which is what it actually means to a person choosing it.
    Integrated,
}

impl GpuMuxMode {
    const ALL: [Self; 4] = [Self::Hybrid, Self::Discrete, Self::Optimus, Self::Integrated];

    fn index(self) -> u8 {
        match self {
            Self::Hybrid => 0,
            Self::Discrete => 1,
            Self::Optimus => 2,
            Self::Integrated => 3,
        }
    }

    fn from_index(index: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.index() == index)
    }

    /// Accepts both the firmware's own names and the app's `GpuMode`
    /// union (`"integrated" | "hybrid" | "discrete"`), so a client never
    /// has to know which vocabulary it is speaking.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "hybrid" => Some(Self::Hybrid),
            "discrete" | "dgpu" => Some(Self::Discrete),
            "optimus" => Some(Self::Optimus),
            "integrated" | "igpu" | "uma" => Some(Self::Integrated),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Discrete => "discrete",
            Self::Optimus => "optimus",
            Self::Integrated => "integrated",
        }
    }
}

fn mux_path() -> PathBuf {
    std::env::var("PYREN_GPU_MUX_PATH").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(MUX_PATH))
}

fn present() -> bool {
    Path::new(&mux_path()).exists()
}

/// `None` when the mode byte is one this build does not know - the
/// firmware's own status-flag bit in the high nibble, if it ever sets
/// one, rather than something to fail loudly over: the raw index is
/// still in the JSON reply for whoever needs it.
fn read_mode() -> Result<(u8, Option<GpuMuxMode>), ModuleError> {
    let path = mux_path();
    let text = std::fs::read_to_string(&path).map_err(|e| io_error(&path, e))?;
    let raw: u8 = text.trim().parse().map_err(|_| {
        ModuleError::Io(format!("{}: not a number ({:?})", path.display(), text.trim()))
    })?;
    Ok((raw, GpuMuxMode::from_index(raw)))
}

fn write_mode(mode: GpuMuxMode) -> Result<(), ModuleError> {
    let path = mux_path();
    std::fs::write(&path, mode.index().to_string()).map_err(|e| write_error(&path, mode, e))
}

fn io_error(path: &Path, e: std::io::Error) -> ModuleError {
    match e.kind() {
        IoErrorKind::PermissionDenied => ModuleError::localised(
            ErrorKind::PermissionDenied,
            msg!(
                "gpu.err.needsRoot",
                { "path" => path.display().to_string() },
                "reading {path} needs root"
            ),
        ),
        _ => ModuleError::Io(format!("{}: {e}", path.display())),
    }
}

fn write_error(path: &Path, mode: GpuMuxMode, e: std::io::Error) -> ModuleError {
    let is_unsupported =
        e.kind() == IoErrorKind::Unsupported || e.raw_os_error() == Some(EOPNOTSUPP);
    match e.kind() {
        IoErrorKind::PermissionDenied => ModuleError::localised(
            ErrorKind::PermissionDenied,
            msg!(
                "gpu.err.needsRoot",
                { "path" => path.display().to_string() },
                "writing {path} needs root"
            ),
        ),
        _ if is_unsupported => ModuleError::localised(
            ErrorKind::NotCapable,
            msg!(
                "gpu.err.modeNotSupported",
                { "mode" => mode.as_str() },
                "this machine's firmware does not offer '{mode}' mode"
            ),
        ),
        _ => ModuleError::Io(format!("{}: {e}", path.display())),
    }
}

fn status() -> Value {
    match read_mode() {
        Ok((raw, mode)) => json!({
            "supported": true,
            "mode": mode.map(GpuMuxMode::as_str),
            "raw": raw,
        }),
        Err(_) => json!({ "supported": false, "mode": null, "raw": null }),
    }
}

pub struct GpuModule;

impl GpuModule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GpuModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for GpuModule {
    fn id(&self) -> &'static str {
        "gpu"
    }

    fn is_supported(&self) -> bool {
        present()
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(status()),

            "setMode" => {
                if !present() {
                    return Err(ModuleError::Unsupported);
                }
                let raw = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "gpu.err.modeRequired",
                            "params.mode is required: 'integrated', 'hybrid', 'discrete' or 'optimus'"
                        ),
                    )
                })?;
                let mode = GpuMuxMode::parse(raw).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "gpu.err.modeUnknown",
                            { "mode" => raw.to_string() },
                            "'{mode}' is not a GPU mode"
                        ),
                    )
                })?;
                write_mode(mode)?;
                Ok(status())
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `PYREN_GPU_MUX_PATH` is process-global state; tests that set it must
    // not run concurrently with each other or with a bare `mux_path()`
    // call from another test module in the same binary.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct Fixture {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = std::env::temp_dir()
                .join(format!("pyren-gpu-test-{}-{:?}", std::process::id(), std::thread::current().id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("gpu_mux_mode");
            std::env::set_var("PYREN_GPU_MUX_PATH", &path);
            Self { _guard: guard, dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("gpu_mux_mode")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::env::remove_var("PYREN_GPU_MUX_PATH");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn index_round_trips_through_all_four_modes() {
        for mode in GpuMuxMode::ALL {
            assert_eq!(GpuMuxMode::from_index(mode.index()), Some(mode));
        }
    }

    #[test]
    fn parse_accepts_the_apps_own_vocabulary_too() {
        assert_eq!(GpuMuxMode::parse("integrated"), Some(GpuMuxMode::Integrated));
        assert_eq!(GpuMuxMode::parse("uma"), Some(GpuMuxMode::Integrated));
        assert_eq!(GpuMuxMode::parse("dgpu"), Some(GpuMuxMode::Discrete));
        assert_eq!(GpuMuxMode::parse("HYBRID"), Some(GpuMuxMode::Hybrid));
        assert_eq!(GpuMuxMode::parse("nonsense"), None);
    }

    #[test]
    fn no_file_means_unsupported_not_an_error() {
        let fx = Fixture::new();
        assert!(!Path::new(&fx.path()).exists());
        let module = GpuModule::new();
        assert!(!module.is_supported());
        let reply = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(reply["supported"], json!(false));
        assert_eq!(reply["mode"], json!(null));
    }

    #[test]
    fn get_status_reads_the_current_mode() {
        let fx = Fixture::new();
        std::fs::write(fx.path(), "1\n").unwrap();
        let module = GpuModule::new();
        assert!(module.is_supported());
        let reply = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(reply["supported"], json!(true));
        assert_eq!(reply["mode"], json!("discrete"));
        assert_eq!(reply["raw"], json!(1));
    }

    #[test]
    fn set_mode_writes_the_index_not_the_bitmask() {
        let fx = Fixture::new();
        std::fs::write(fx.path(), "0\n").unwrap();
        let module = GpuModule::new();
        let reply = module.call("setMode", json!({ "mode": "integrated" })).unwrap();
        assert_eq!(reply["mode"], json!("integrated"));
        assert_eq!(std::fs::read_to_string(fx.path()).unwrap(), "3");
    }

    #[test]
    fn set_mode_rejects_an_unknown_name_before_touching_the_file() {
        let fx = Fixture::new();
        std::fs::write(fx.path(), "0\n").unwrap();
        let module = GpuModule::new();
        let err = module.call("setMode", json!({ "mode": "quantum" })).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
        // Unchanged: the bad name never reached a write.
        assert_eq!(std::fs::read_to_string(fx.path()).unwrap(), "0\n");
    }

    #[test]
    fn set_mode_without_the_param_is_invalid_params() {
        let fx = Fixture::new();
        std::fs::write(fx.path(), "0\n").unwrap();
        let module = GpuModule::new();
        let err = module.call("setMode", json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn set_mode_with_no_hardware_is_unsupported() {
        let fx = Fixture::new();
        assert!(!Path::new(&fx.path()).exists());
        let module = GpuModule::new();
        let err = module.call("setMode", json!({ "mode": "hybrid" })).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn unknown_method_is_reported_by_name() {
        let _fx = Fixture::new();
        let module = GpuModule::new();
        let err = module.call("frobnicate", Value::Null).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownMethod);
    }
}
