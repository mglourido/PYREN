//! Where the machine's temperatures are, and how to read one.
//!
//! Two modules want the same two numbers for different reasons - the fan
//! curve follows one, and the power supervisor backs off when either gets
//! high - so the discovery lives here rather than in whichever of them
//! grew it first. Nothing in this module caches: a caller that reads on a
//! timer holds the path it found, and one that reads once does not need
//! to.
//!
//! Both lookups name the hwmon drivers they accept rather than taking the
//! first `temp1_input` they see. Most of what is in `/sys/class/hwmon` on
//! a laptop is the temperature of something else - the chipset, an NVMe
//! drive, the battery, a wireless card - and driving fans or power modes
//! from one of those would be worse than having no sensor at all.

use std::fs;
use std::path::{Path, PathBuf};

const HWMON_CLASS_ROOT: &str = "/sys/class/hwmon";
const THERMAL_ZONE0: &str = "/sys/class/thermal/thermal_zone0/temp";

/// hwmon drivers that publish a CPU package temperature.
const CPU_DRIVERS: &[&str] = &["coretemp", "k10temp"];

/// ...and the ones that publish a GPU's. `nvidia` is the proprietary
/// driver's own hwmon, so reading it needs neither `nvidia-smi` nor root.
const GPU_DRIVERS: &[&str] = &["amdgpu", "nouveau", "nvidia", "radeon"];

/// The CPU package sensor, falling back to `thermal_zone0`.
///
/// The fallback is deliberate and the GPU has no equivalent: every machine
/// has *some* thermal zone 0 and on a laptop it is nearly always the
/// package, whereas a machine with no GPU hwmon simply has no GPU sensor.
pub fn cpu_temp_path() -> Option<PathBuf> {
    hwmon_temp_path(CPU_DRIVERS).or_else(|| {
        let fallback = Path::new(THERMAL_ZONE0);
        fallback.exists().then(|| fallback.to_path_buf())
    })
}

/// The discrete GPU's sensor, when it has one hwmon publishes. `None` is
/// the common case rather than a fault: an integrated-only machine has
/// nothing here, and so does one whose card was powered down when the
/// search ran.
pub fn gpu_temp_path() -> Option<PathBuf> {
    hwmon_temp_path(GPU_DRIVERS)
}

fn hwmon_temp_path(drivers: &[&str]) -> Option<PathBuf> {
    let entries = fs::read_dir(HWMON_CLASS_ROOT).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let dir = entry.path();
        let Ok(name) = fs::read_to_string(dir.join("name")) else {
            continue;
        };
        if drivers.contains(&name.trim()) {
            let temp_path = dir.join("temp1_input");
            if temp_path.exists() {
                return Some(temp_path);
            }
        }
    }
    None
}

/// sysfs temperature files report millidegrees C.
pub fn read_millideg_c(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse::<i64>().ok().map(|v| v / 1000)
}

/// The hottest of the CPU and GPU right now, in whole degrees.
///
/// "Hottest" rather than an average, and rather than the CPU alone: what a
/// caller asking this wants to know is whether the machine is in thermal
/// trouble, and a card at 90 C is trouble whatever the package says. A
/// sensor reading 0 is a part that is powered down, not a cold one, so it
/// is left out of the comparison instead of dragging it down.
pub fn hottest_c(cpu: Option<&Path>, gpu: Option<&Path>) -> Option<f64> {
    let readings = [cpu, gpu]
        .into_iter()
        .flatten()
        .filter_map(read_millideg_c)
        .filter(|t| *t > 0);
    readings.max().map(|t| t as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pyren-sensors-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn millidegrees_become_degrees() {
        let dir = temp_dir("read");
        let path = write(&dir, "temp1_input", "56000\n");
        assert_eq!(read_millideg_c(&path), Some(56));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sensor_that_is_not_a_number_reads_as_nothing() {
        let dir = temp_dir("garbage");
        let path = write(&dir, "temp1_input", "not a temperature");
        assert_eq!(read_millideg_c(&path), None);
        assert_eq!(read_millideg_c(&dir.join("absent")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hottest_part_is_the_one_that_answers() {
        let dir = temp_dir("hottest");
        let cpu = write(&dir, "cpu", "56000");
        let gpu = write(&dir, "gpu", "81000");
        assert_eq!(hottest_c(Some(&cpu), Some(&gpu)), Some(81.0));
        assert_eq!(hottest_c(Some(&cpu), None), Some(56.0));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A card that is powered down reads 0, and 0 C is not a temperature.
    /// Letting it into the comparison would be harmless for a maximum -
    /// but letting it *be* the answer, on a machine whose only sensor is
    /// asleep, would report a burning laptop as ice cold.
    #[test]
    fn a_powered_down_part_is_left_out_rather_than_counted_as_cold() {
        let dir = temp_dir("asleep");
        let cpu = write(&dir, "cpu", "72000");
        let gpu = write(&dir, "gpu", "0");
        assert_eq!(hottest_c(Some(&cpu), Some(&gpu)), Some(72.0));
        assert_eq!(hottest_c(None, Some(&gpu)), None, "asleep is not 0 C");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_sensors_at_all_is_not_a_temperature() {
        assert_eq!(hottest_c(None, None), None);
    }
}
