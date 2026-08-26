//! Fan control module - ported from the `omen-fan-control` Python project
//! (see `../omen-fan-control-main/docs/` in the workspace for the full
//! behavioral spec this is ported from).
//!
//! Currently implemented: read-only status (`fan.getStatus`), which needs
//! no privileges. Everything that *writes* to hardware (`pwm1`,
//! `pwm1_enable`, the fan-cleaner ACPI calls) is intentionally not ported
//! yet - see `docs/04-fan-control-logic.md` in the source repo for the
//! full spec (curve interpolation, calibration, hysteresis, the fan
//! cleaner protocol) before implementing `setMode`/`setCurve`/`cleaner.*`.

use std::fs;
use std::path::{Path, PathBuf};

use omen_hub_core::{Module, ModuleError, ModuleResult};
use serde_json::{json, Value};

const HWMON_ROOT: &str = "/sys/devices/platform/hp-wmi/hwmon";
const HWMON_CLASS_ROOT: &str = "/sys/class/hwmon";
const THERMAL_ZONE0: &str = "/sys/class/thermal/thermal_zone0/temp";

/// Sysfs paths discovered for this machine. Any of these can be `None` if
/// the patched hp-wmi driver isn't installed, or if no supported CPU temp
/// sensor was found - callers must handle that, not assume presence.
#[derive(Debug, Clone, Default)]
struct FanPaths {
    hwmon_dir: Option<PathBuf>,
    #[allow(dead_code)] // not read yet; wired up once `setMode` is implemented
    pwm1: Option<PathBuf>,
    #[allow(dead_code)]
    pwm1_enable: Option<PathBuf>,
    fan1_input: Option<PathBuf>,
    fan2_input: Option<PathBuf>,
    cpu_temp: Option<PathBuf>,
}

pub struct FanModule {
    paths: FanPaths,
}

impl FanModule {
    pub fn new() -> Self {
        Self { paths: discover_paths() }
    }

    fn get_status(&self) -> Value {
        let cpu_temp_c = self.paths.cpu_temp.as_deref().and_then(read_millideg_c);
        let (fan_rpm, is_reverse) =
            read_fan_rpm(self.paths.fan1_input.as_deref(), self.paths.fan2_input.as_deref());

        json!({
            "driverInstalled": self.paths.hwmon_dir.is_some(),
            "cpuTempC": cpu_temp_c,
            "fanRpm": fan_rpm,
            "isReverse": is_reverse,
        })
    }
}

impl Default for FanModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for FanModule {
    fn id(&self) -> &'static str {
        "fan"
    }

    fn is_supported(&self) -> bool {
        self.paths.hwmon_dir.is_some()
    }

    fn call(&self, method: &str, _params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.get_status()),
            "setMode" | "setCurve" => Err(ModuleError::Other(format!(
                "'{method}' is not implemented yet - writing to hardware needs the daemon \
                 to run privileged and is deliberately not scaffolded here yet; see \
                 docs/04-fan-control-logic.md in the source repo"
            ))),
            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

fn discover_paths() -> FanPaths {
    let mut paths = FanPaths::default();

    if let Some(hwmon_dir) = find_hp_wmi_hwmon_dir() {
        paths.pwm1 = Some(hwmon_dir.join("pwm1"));
        paths.pwm1_enable = Some(hwmon_dir.join("pwm1_enable"));
        paths.fan1_input = Some(hwmon_dir.join("fan1_input"));
        paths.fan2_input = Some(hwmon_dir.join("fan2_input"));
        paths.hwmon_dir = Some(hwmon_dir);
    }

    paths.cpu_temp = find_cpu_temp_path();
    paths
}

/// Mirrors `FanController._find_paths` (`glob.glob(HWMON_PATH_PATTERN)`
/// taking the first match) in the Python original.
fn find_hp_wmi_hwmon_dir() -> Option<PathBuf> {
    fs::read_dir(HWMON_ROOT)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

/// Mirrors `FanController._find_cpu_temp_path`: prefer `coretemp`/`k10temp`
/// hwmon drivers, fall back to `thermal_zone0`.
fn find_cpu_temp_path() -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(HWMON_CLASS_ROOT) {
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            let Ok(name) = fs::read_to_string(dir.join("name")) else {
                continue;
            };
            if matches!(name.trim(), "coretemp" | "k10temp") {
                let temp_path = dir.join("temp1_input");
                if temp_path.exists() {
                    return Some(temp_path);
                }
            }
        }
    }

    let fallback = Path::new(THERMAL_ZONE0);
    fallback.exists().then(|| fallback.to_path_buf())
}

/// sysfs temperature files report millidegrees C.
fn read_millideg_c(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse::<i64>().ok().map(|v| v / 1000)
}

fn read_raw_rpm(path: Option<&Path>) -> Option<i64> {
    fs::read_to_string(path?).ok()?.trim().parse::<i64>().ok()
}

/// Mirrors `FanController.parse_hwmon_rpm`: hp-wmi encodes fan-cleaner
/// reverse-spin state in the fan?_input value itself (bit 7 of a
/// hundred-RPM-unit byte, i.e. raw value >= 12800). See
/// docs/02-kernel-driver.md ("The reverse-bit / fan-cleaner RPM encoding")
/// in the source repo for why this looks the way it does.
fn parse_hwmon_rpm(raw: Option<i64>) -> (i64, bool) {
    match raw {
        None => (0, false),
        Some(raw_rpm) if raw_rpm >= 12800 => {
            let reverse_bit_speed = raw_rpm / 100;
            let actual_speed = (reverse_bit_speed & 0x7F) * 100;
            (actual_speed, true)
        }
        Some(raw_rpm) => (raw_rpm, false),
    }
}

/// Mirrors `FanController.get_fan_speed_info`: report whichever fan reads
/// faster, and whether either fan is currently in reverse.
fn read_fan_rpm(fan1: Option<&Path>, fan2: Option<&Path>) -> (i64, bool) {
    let (rpm1, rev1) = parse_hwmon_rpm(read_raw_rpm(fan1));
    let (rpm2, rev2) = parse_hwmon_rpm(read_raw_rpm(fan2));
    (rpm1.max(rpm2), rev1 || rev2)
}
