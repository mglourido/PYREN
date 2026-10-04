//! Writing to the fans: what this machine will accept, and how to say it.
//!
//! Everything here goes through the `hp-wmi` hwmon interface, whose
//! semantics are not obvious from the file names. From the driver source
//! (`hp_wmi_hwmon_write` / `hp_wmi_apply_fan_settings`):
//!
//! | `pwm1_enable` | driver mode | what it does |
//! |---|---|---|
//! | `0` | `PWM_MODE_MAX` | full speed, refreshed by the driver's keep-alive |
//! | `1` | `PWM_MODE_MANUAL` | apply `pwm1`, likewise refreshed |
//! | `2` | `PWM_MODE_AUTO` | hand the fans back to the firmware curve |
//!
//! The two that matter for a machine like board `8D2F`: **max and auto go
//! through a WMI query that needs no per-board parameters, while manual
//! needs `pwm1`** - which the running driver only exposes for boards in its
//! feature table. So a machine can perfectly well be able to do
//! max/auto and not manual, and this module has to say so rather than
//! offering a slider that silently does nothing.
//!
//! The third thing the file names do not say: on a board in that feature
//! table, **writing `pwm1_enable = 1` overwrites both setpoints** with the
//! speed the fans are turning at that moment (the "smooth transition" in
//! `hp_wmi_hwmon_write`), and at 0 rpm that is `HP_FAN_SPEED_AUTOMATIC` -
//! the firmware curve again. So the mode switch has to come first, and
//! only when the driver is not already in manual; see [`speed_writes`].

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::FanPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FanMode {
    /// The firmware's own curve. The safe state, and the default.
    #[default]
    Auto,
    /// Full speed.
    Max,
    /// One fixed PWM value the user chose.
    Manual,
    /// Follow the stored temperature → speed curve.
    Curve,
}

impl FanMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "max" => Some(Self::Max),
            "manual" => Some(Self::Manual),
            "curve" => Some(Self::Curve),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Max => "max",
            Self::Manual => "manual",
            Self::Curve => "curve",
        }
    }

    /// Whether this mode has to write a specific speed, as opposed to
    /// naming one the firmware already knows.
    pub fn needs_pwm(self) -> bool {
        matches!(self, Self::Manual | Self::Curve)
    }
}

/// What this machine's driver actually lets us do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// `pwm1_enable` is present, so auto and max can be commanded.
    pub switch_mode: bool,
    /// `pwm1` is present, so a specific speed can be commanded.
    pub set_speed: bool,
}

impl Capabilities {
    pub(crate) fn detect(paths: &FanPaths) -> Self {
        Self {
            switch_mode: paths.pwm1_enable.as_deref().is_some_and(Path::exists),
            set_speed: paths.pwm1.as_deref().is_some_and(Path::exists),
        }
    }

    pub fn supports(&self, mode: FanMode) -> bool {
        match mode {
            FanMode::Auto | FanMode::Max => self.switch_mode,
            FanMode::Manual | FanMode::Curve => self.switch_mode && self.set_speed,
        }
    }
}

/// Why a write could not happen. Kept separate from `ModuleError` so this
/// file stays about hardware rather than about IPC.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("this machine's driver does not support {0} (missing {1})")]
    Unsupported(&'static str, &'static str),
    #[error("writing {0} needs root: {1}")]
    PermissionDenied(String, String),
    #[error("writing {0}: {1}")]
    Io(String, String),
    /// A measurement that holds the fans away from where the machine
    /// needs them was stopped, or refused, because of the heat.
    #[error("{0} °C is too hot to hold the fans for a measurement (the limit is {1} °C)")]
    TooHot(i64, i64),
    /// ...or because there was no temperature to watch while it ran.
    #[error("no temperature sensor could be read, so the fans cannot be held for a measurement")]
    NoTemperature,
}

impl ControlError {
    /// The translatable sentence a client should show. The `{1}` operating-
    /// system error text is passed through as a param, not translated.
    pub fn to_msg(&self) -> pyren_core::Msg {
        use pyren_core::msg;
        match self {
            Self::Unsupported(what, missing) => msg!(
                "fan.control.unsupported",
                { "what" => *what, "missing" => *missing },
                "this machine's driver does not support {what} (missing {missing})"
            ),
            Self::PermissionDenied(path, error) => msg!(
                "fan.control.needsRoot",
                { "path" => path.clone(), "error" => error.clone() },
                "writing {path} needs root: {error}"
            ),
            Self::Io(path, error) => msg!(
                "fan.control.io",
                { "path" => path.clone(), "error" => error.clone() },
                "writing {path}: {error}"
            ),
            Self::TooHot(temp, limit) => msg!(
                "fan.control.tooHot",
                { "temp" => *temp, "limit" => *limit },
                "{temp} °C is too hot to hold the fans for a measurement (the limit is {limit} °C)"
            ),
            Self::NoTemperature => msg!(
                "fan.control.noTemperature",
                "no temperature sensor could be read, so the fans cannot be held for a measurement"
            ),
        }
    }
}

fn write_sysfs(path: &Path, value: &str) -> Result<(), ControlError> {
    let shown = path.display().to_string();
    fs::write(path, format!("{value}\n")).map_err(|e| match e.kind() {
        ErrorKind::PermissionDenied => ControlError::PermissionDenied(shown, e.to_string()),
        _ => ControlError::Io(shown, e.to_string()),
    })
}

/// The writes that put the fans at `pwm`, in the order they must happen.
///
/// The mode switch goes first and is skipped when the driver already
/// reports manual. The opposite order - speed, then mode - is what this
/// used to do, and on a feature-table board it cannot work: the speed
/// written while still in auto is thrown away by the auto path's reset,
/// and the `pwm1_enable = 1` after it replaces the setpoint with the
/// current fan speed. Re-sent every tick, it undid every curve step the
/// moment it was made, and with the fans stopped it meant firmware auto.
///
/// On the older path the driver stores `pwm1` as given, so switching first
/// runs its default of 128 for the microseconds until the next write -
/// far too short for a fan to answer. That is also the order the Python
/// original uses (`set_fan_pwm`).
///
/// `pwm2` is the GPU fan. The driver keeps a setpoint per fan, and a speed
/// written only to `pwm1` leaves the second fan wherever the mode switch
/// put it.
fn speed_writes<'a>(
    enable: &'a Path,
    pwm1: &'a Path,
    pwm2: Option<&'a Path>,
    hardware_mode: Option<u8>,
    pwm: u8,
) -> Vec<(&'a Path, String)> {
    let mut writes = Vec::with_capacity(3);
    if hardware_mode != Some(1) {
        writes.push((enable, "1".to_string()));
    }
    writes.push((pwm1, pwm.to_string()));
    if let Some(pwm2) = pwm2 {
        writes.push((pwm2, pwm.to_string()));
    }
    writes
}

/// Applies a mode to the hardware.
///
/// `pwm` is only consulted for the modes that need one; see
/// [`speed_writes`] for why those writes are ordered the way they are.
pub fn apply(
    paths: &FanPaths,
    caps: Capabilities,
    mode: FanMode,
    pwm: u8,
) -> Result<(), ControlError> {
    if !caps.supports(mode) {
        return Err(ControlError::Unsupported(
            mode.as_str(),
            if caps.switch_mode {
                "pwm1"
            } else {
                "pwm1_enable"
            },
        ));
    }

    let enable = paths
        .pwm1_enable
        .as_deref()
        .ok_or(ControlError::Unsupported(mode.as_str(), "pwm1_enable"))?;

    match mode {
        // Two writes, in order. `pwm1_enable = 2` is the mode switch and is
        // what `read_hardware_mode` reports, but on a driver whose auto
        // path is not `hp_wmi_fan_control_supported()` (`hp_wmi.c`,
        // `hp_wmi_apply_fan_settings`'s `PWM_MODE_AUTO` case) it only
        // cancels the keep-alive re-assert and clears the "max speed" WMI
        // bit - it never re-sends the fan speed the EC was last told to
        // hold, because it never touches `priv->cpu_pwm`/`gpu_pwm`. The
        // driver's own way to release a manual speed is `pwm1 = 0`
        // (`HP_FAN_SPEED_AUTOMATIC`, a sentinel distinct from
        // `pwm1_enable`): written while still in manual, `hp_wmi_hwmon_write`
        // takes it straight to `hp_wmi_fan_speed_set` with the sentinel and
        // the EC is told "auto" directly, before the mode switch happens at
        // all. So this writes the sentinel first, whenever there is a
        // `pwm1` to write it to - harmless when the board's auto path does
        // reset the setpoint itself (it is overwritten a moment later
        // anyway), and the difference between "keep-alive stops but the
        // last manual speed is left running" and "auto" on a board where it
        // does not.
        FanMode::Auto => {
            if let Some(pwm1) = paths.pwm1.as_deref().filter(|p| p.exists()) {
                write_sysfs(pwm1, "0")?;
            }
            write_sysfs(enable, "2")
        }
        FanMode::Max => write_sysfs(enable, "0"),
        FanMode::Manual | FanMode::Curve => {
            let pwm1 = paths
                .pwm1
                .as_deref()
                .ok_or(ControlError::Unsupported(mode.as_str(), "pwm1"))?;
            let pwm2 = paths.pwm2.as_deref().filter(|p| p.exists());
            for (path, value) in speed_writes(enable, pwm1, pwm2, read_hardware_mode(paths), pwm) {
                write_sysfs(path, &value)?;
            }
            Ok(())
        }
    }
}

/// Reads back the mode the driver reports, for `getStatus`.
///
/// Note this is the *driver's* mode, which is coarser than ours: it cannot
/// tell manual from curve, since a curve is a manual value that keeps
/// changing.
pub fn read_hardware_mode(paths: &FanPaths) -> Option<u8> {
    let raw = fs::read_to_string(paths.pwm1_enable.as_deref()?).ok()?;
    raw.trim().parse::<u8>().ok()
}

pub fn read_pwm(paths: &FanPaths) -> Option<u8> {
    let raw = fs::read_to_string(paths.pwm1.as_deref()?).ok()?;
    raw.trim().parse::<u8>().ok()
}

// --- the ceiling the driver works with -----------------------------------
//
// `fan1_max` / `fan2_max` are the number the driver converts between pwm
// and rpm against: what the firmware's max-speed query answered, else the
// fastest entry of its fan table, else a constant compiled in - and, on
// Pyren's driver, a calibration's measurement over all three. It is the
// truth about what a pwm value *asks for*, and only on the last of those a
// truth about the fans: a table's fastest entry is routinely well short of
// what max mode reaches. So it is reported and compared, and never fed to
// the thermal checker or the curve's hysteresis in place of a measurement.

/// What the driver takes for each fan's full speed, in rpm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriverCeiling {
    pub cpu: Option<i64>,
    pub gpu: Option<i64>,
}

impl DriverCeiling {
    /// The faster of the two, to stand beside `fan_max_rpm` - which is the
    /// peak of both fans.
    pub fn peak(self) -> Option<i64> {
        self.cpu.max(self.gpu)
    }
}

/// A driver older than the vendored source's `fan*_max` does not publish
/// the attribute. One of Pyren's still says what it was loaded with: a
/// non-zero `*_max_rpm_measured` is applied over everything else, so it
/// *is* the ceiling in force. `None` where neither says anything.
pub fn read_driver_ceiling(paths: &FanPaths) -> DriverCeiling {
    let read = |attribute: &str, param: &str| {
        let published = || {
            let raw = fs::read_to_string(paths.hwmon_dir.as_deref()?.join(attribute)).ok()?;
            raw.trim().parse::<i64>().ok().filter(|rpm| *rpm > 0)
        };
        let loaded_with = || {
            read_param(paths, param)
                .filter(|&hundreds| hundreds > 0)
                .map(|hundreds| i64::from(hundreds) * 100)
        };
        published().or_else(loaded_with)
    };
    DriverCeiling {
        cpu: read("fan1_max", MEASURED_CEILING_PARAM),
        gpu: read("fan2_max", MEASURED_GPU_CEILING_PARAM),
    }
}

/// Below this, a ceiling nobody measured is more likely the fan table's
/// fastest entry than the speed the fans top out at. The threshold is
/// omen-fan-control's, which refuses to install under it; here it only
/// words a notice, since calibrating needs the driver in place first.
pub const LOW_CEILING_RPM: i64 = 5000;

/// Whether the driver is working with a ceiling that looks too low and no
/// calibration has replaced it - neither this daemon's (`measured`) nor one
/// the driver was loaded with, which outlives a reset of the config.
pub fn ceiling_looks_low(paths: &FanPaths, measured: Option<i64>) -> bool {
    let loaded_with_a_measurement = [MEASURED_CEILING_PARAM, MEASURED_GPU_CEILING_PARAM]
        .iter()
        .any(|param| read_param(paths, param).is_some_and(|hundreds| hundreds > 0));
    measured.is_none()
        && !loaded_with_a_measurement
        && read_driver_ceiling(paths)
            .peak()
            .is_some_and(|rpm| rpm < LOW_CEILING_RPM)
}

/// What became of a calibration's ceiling on its way to the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CeilingPin {
    /// The loaded driver is converting against the measurement.
    Applied,
    /// It is not yet: the measurement is in `/etc/modprobe.d`, waiting for
    /// the next time `hp-wmi` loads.
    Pending,
    /// It is not, and no load will change that: the measurement is in this
    /// daemon's config only. Writing the file failed, or was refused.
    NotWritten,
    /// The loaded driver has no parameter to take one through.
    Unsupported,
}

const MEASURED_CEILING_PARAM: &str = "cpu_max_rpm_measured";
const MEASURED_GPU_CEILING_PARAM: &str = "gpu_max_rpm_measured";

/// A measurement as the driver would hold it: whole hundreds of rpm.
/// `None` for one its parameter cannot express.
fn as_pinned(rpm: i64) -> Option<i64> {
    let hundreds = pyren_installer::measured_param_value(u32::try_from(rpm).ok()?)?;
    Some(i64::from(hundreds) * 100)
}

/// What the file a pin is written to holds for each fan, in rpm.
fn read_pinned_ceiling(paths: &FanPaths) -> DriverCeiling {
    let Some(conf) = paths
        .ceiling_conf
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
    else {
        return DriverCeiling::default();
    };
    let pinned = pyren_installer::pinned_ceiling_in(&conf);
    DriverCeiling {
        cpu: pinned.cpu.map(i64::from),
        gpu: pinned.gpu.map(i64::from),
    }
}

/// Compares what calibration measured, per fan, with what the driver says
/// it is using. `None` where there is nothing to compare: no per-fan
/// measurement, or no hp-wmi hwmon at all.
pub fn ceiling_pin(
    paths: &FanPaths,
    fan1_max_rpm: Option<i64>,
    fan2_max_rpm: Option<i64>,
) -> Option<CeilingPin> {
    let measured = [fan1_max_rpm, fan2_max_rpm];
    if measured.iter().all(Option::is_none) {
        return None;
    }
    paths.hwmon_dir.as_deref()?;

    let accepts = paths
        .driver_params
        .as_deref()
        .is_some_and(|dir| dir.join(MEASURED_CEILING_PARAM).exists());
    if !accepts {
        return Some(CeilingPin::Unsupported);
    }

    // `pin_measured_ceiling` writes both fans or neither: one measurement
    // the parameter cannot hold, and the other was not written either.
    let wanted = measured.map(|rpm| rpm.map(as_pinned));
    if wanted.iter().flatten().any(Option::is_none) {
        return Some(CeilingPin::NotWritten);
    }

    // On a driver that takes a measurement, a fan with no ceiling to read
    // back is one whose parameter is still zero: not pinned yet.
    let driver = read_driver_ceiling(paths);
    let on_disk = read_pinned_ceiling(paths);
    let mut verdict = CeilingPin::Applied;
    for (fan, wanted) in wanted.into_iter().enumerate() {
        let Some(wanted) = wanted.flatten() else {
            continue;
        };
        let (in_force, written) = match fan {
            0 => (driver.cpu, on_disk.cpu),
            _ => (driver.gpu, on_disk.gpu),
        };
        if in_force == Some(wanted) {
            continue;
        }
        if written != Some(wanted) {
            // Not what the driver uses and not what its next load will
            // give it: "pending" would promise a reboot that fixes nothing.
            return Some(CeilingPin::NotWritten);
        }
        verdict = CeilingPin::Pending;
    }
    Some(verdict)
}

// --- the manual-speed floor --------------------------------------------
//
// Pyren's driver patch (`installer::patch::add_min_rpm_params`) reports the
// fan table's slowest entry, which is what the upstream driver clamps every
// manual speed up to, and lets that clamp be replaced at runtime. Both are
// module parameters in hundreds of rpm. A driver without them is one where
// the table's floor is the only floor there is.

const MIN_RPM_TABLE: &str = "min_rpm_table";
const MIN_RPM_OVERRIDE: &str = "min_rpm_override";

fn read_param(paths: &FanPaths, name: &str) -> Option<u8> {
    let raw = fs::read_to_string(paths.driver_params.as_deref()?.join(name)).ok()?;
    raw.trim().parse::<u8>().ok()
}

/// The floor the upstream driver enforces - the fan table's slowest entry -
/// in rpm. `None` on a driver that does not report it, or read no table.
pub fn read_driver_floor(paths: &FanPaths) -> Option<i64> {
    read_param(paths, MIN_RPM_TABLE)
        .filter(|&v| v > 0)
        .map(|v| i64::from(v) * 100)
}

/// Whether this driver lets the floor be replaced.
pub fn floor_override_supported(paths: &FanPaths) -> bool {
    paths
        .driver_params
        .as_deref()
        .is_some_and(|dir| dir.join(MIN_RPM_OVERRIDE).exists())
}

/// The replacement floor in force, in hundreds of rpm; 0 is the table's.
pub fn read_floor_override(paths: &FanPaths) -> Option<u8> {
    read_param(paths, MIN_RPM_OVERRIDE)
}

/// Replaces the driver's floor; `0` gives the table's back. Takes effect on
/// the next speed written, not on the one already running.
pub fn set_floor_override(paths: &FanPaths, hundreds: u8) -> Result<(), ControlError> {
    let dir = paths
        .driver_params
        .as_deref()
        .ok_or(ControlError::Unsupported(
            "a floor override",
            MIN_RPM_OVERRIDE,
        ))?;
    let path = dir.join(MIN_RPM_OVERRIDE);
    if !path.exists() {
        return Err(ControlError::Unsupported(
            "a floor override",
            MIN_RPM_OVERRIDE,
        ));
    }
    write_sysfs(&path, &hundreds.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(tag: &str, files: &[&str]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pyren-fan-control-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in files {
            fs::write(dir.join(f), "2\n").unwrap();
        }
        dir
    }

    /// A hwmon directory with the driver's ceilings in it, and optionally
    /// the parameter Pyren's driver takes a measured one through.
    fn ceiling_fixture(tag: &str, fan1_max: &str, fan2_max: &str, accepts: bool) -> FanPaths {
        let dir = fixture(tag, &[]);
        fs::write(dir.join("fan1_max"), fan1_max).unwrap();
        fs::write(dir.join("fan2_max"), fan2_max).unwrap();
        let params = dir.join("parameters");
        fs::create_dir_all(&params).unwrap();
        if accepts {
            fs::write(params.join(MEASURED_CEILING_PARAM), "0\n").unwrap();
        }
        FanPaths {
            ceiling_conf: Some(dir.join("pyren-hp-wmi.conf")),
            hwmon_dir: Some(dir),
            driver_params: Some(params),
            ..Default::default()
        }
    }

    /// What `pin_measured_ceiling` leaves for the driver's next load.
    fn pin_on_disk(paths: &FanPaths, options: &str) {
        let conf = paths.ceiling_conf.as_deref().unwrap();
        fs::write(conf, format!("options hp-wmi {options}\n")).unwrap();
    }

    #[test]
    fn a_driver_without_fan_max_reports_no_ceiling() {
        let paths = FanPaths {
            hwmon_dir: Some(fixture("ceiling-absent", &["fan1_input"])),
            ..Default::default()
        };
        assert_eq!(read_driver_ceiling(&paths), DriverCeiling::default());
        assert_eq!(read_driver_ceiling(&FanPaths::default()).peak(), None);
    }

    #[test]
    fn a_zero_ceiling_is_not_a_ceiling() {
        let paths = ceiling_fixture("ceiling-zero", "0\n", "4400\n", true);
        let ceiling = read_driver_ceiling(&paths);
        assert_eq!(ceiling.cpu, None);
        assert_eq!(ceiling.peak(), Some(4400));
    }

    #[test]
    fn only_an_unmeasured_ceiling_can_look_low() {
        let low = ceiling_fixture("low", "4400\n", "4100\n", true);
        assert!(ceiling_looks_low(&low, None));
        assert!(!ceiling_looks_low(&low, Some(4400)));
        // A slow ceiling the driver was loaded with is a measurement too.
        let params = low.driver_params.clone().unwrap();
        fs::write(params.join(MEASURED_CEILING_PARAM), "44\n").unwrap();
        assert!(!ceiling_looks_low(&low, None));

        // One fan at a plausible speed is enough: the peak is what counts.
        let mixed = ceiling_fixture("low-mixed", "6000\n", "4100\n", false);
        assert!(!ceiling_looks_low(&mixed, None));
        assert!(!ceiling_looks_low(&FanPaths::default(), None));
    }

    #[test]
    fn a_pin_is_applied_when_the_driver_holds_the_measurement_in_hundreds() {
        let paths = ceiling_fixture("pin-applied", "5300\n", "5100\n", true);
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::Applied)
        );
    }

    #[test]
    fn a_pin_is_pending_while_the_driver_still_uses_its_own_ceiling() {
        let paths = ceiling_fixture("pin-pending", "4400\n", "5100\n", true);
        pin_on_disk(&paths, "cpu_max_rpm_measured=53 gpu_max_rpm_measured=51");
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::Pending)
        );
    }

    #[test]
    fn a_pin_no_load_will_deliver_is_not_called_pending() {
        // The write failed: nothing on disk for the next load to pick up.
        let paths = ceiling_fixture("pin-not-written", "4400\n", "5100\n", true);
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::NotWritten)
        );
        // An earlier calibration's file is no better than none.
        pin_on_disk(&paths, "cpu_max_rpm_measured=50 gpu_max_rpm_measured=51");
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::NotWritten)
        );
        // Already in force: what the file says no longer matters.
        let loaded = ceiling_fixture("pin-loaded-no-file", "5300\n", "5100\n", true);
        assert_eq!(
            ceiling_pin(&loaded, Some(5342), Some(5199)),
            Some(CeilingPin::Applied)
        );
    }

    #[test]
    fn one_ceiling_the_parameter_cannot_hold_leaves_both_unwritten() {
        // The pin is refused whole, so the fan that could have been pinned
        // is not waiting for a reload either - whatever the driver uses.
        let paths = ceiling_fixture("pin-refused", "5300\n", "5100\n", true);
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(40)),
            Some(CeilingPin::NotWritten)
        );
        assert_eq!(
            ceiling_pin(&paths, Some(40), None),
            Some(CeilingPin::NotWritten)
        );
    }

    #[test]
    fn a_fan_calibration_did_not_measure_is_left_out_of_the_comparison() {
        let paths = ceiling_fixture("pin-one-fan", "5300\n", "5800\n", true);
        assert_eq!(
            ceiling_pin(&paths, Some(5342), None),
            Some(CeilingPin::Applied)
        );
    }

    #[test]
    fn a_driver_without_the_parameter_cannot_be_pinned() {
        let paths = ceiling_fixture("pin-unsupported", "6000\n", "5800\n", false);
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::Unsupported)
        );
    }

    #[test]
    fn nothing_measured_or_nothing_to_read_back_is_no_verdict() {
        let paths = ceiling_fixture("pin-none", "5300\n", "5100\n", true);
        assert_eq!(ceiling_pin(&paths, None, None), None);
        assert_eq!(ceiling_pin(&FanPaths::default(), Some(5342), None), None);
    }

    #[test]
    fn a_driver_older_than_fan_max_is_checked_through_its_parameters() {
        let dir = fixture("pin-no-fan-max", &[]);
        let params = dir.join("parameters");
        fs::create_dir_all(&params).unwrap();
        fs::write(params.join(MEASURED_CEILING_PARAM), "53\n").unwrap();
        fs::write(params.join(MEASURED_GPU_CEILING_PARAM), "0\n").unwrap();
        let paths = FanPaths {
            driver_params: Some(params.clone()),
            ceiling_conf: Some(dir.join("pyren-hp-wmi.conf")),
            hwmon_dir: Some(dir),
            ..Default::default()
        };
        pin_on_disk(&paths, "cpu_max_rpm_measured=53 gpu_max_rpm_measured=51");
        let ceiling = read_driver_ceiling(&paths);
        assert_eq!((ceiling.cpu, ceiling.gpu), (Some(5300), None));
        // The second fan's parameter was never loaded with anything.
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::Pending)
        );
        fs::write(params.join(MEASURED_GPU_CEILING_PARAM), "51\n").unwrap();
        assert_eq!(
            ceiling_pin(&paths, Some(5342), Some(5199)),
            Some(CeilingPin::Applied)
        );
    }

    fn paths(dir: &Path) -> FanPaths {
        FanPaths {
            hwmon_dir: Some(dir.to_path_buf()),
            pwm1: Some(dir.join("pwm1")),
            pwm2: Some(dir.join("pwm2")),
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            fan2_input: Some(dir.join("fan2_input")),
            cpu_temp: None,
            gpu_temp: None,
            driver_params: Some(dir.join("parameters")),
            ceiling_conf: None,
        }
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn modes_parse_case_insensitively_and_reject_junk() {
        assert_eq!(FanMode::parse("AUTO"), Some(FanMode::Auto));
        assert_eq!(FanMode::parse("curve"), Some(FanMode::Curve));
        assert_eq!(FanMode::parse("turbo"), None);
    }

    /// Board 8D2F: `pwm1_enable` without `pwm1`. Max and auto are real
    /// there; manual and curve are not, and must be refused rather than
    /// half-applied.
    #[test]
    fn a_machine_without_pwm1_can_still_switch_between_auto_and_max() {
        let dir = fixture("nopwm", &["pwm1_enable", "fan1_input"]);
        let caps = Capabilities::detect(&paths(&dir));

        assert!(caps.switch_mode && !caps.set_speed);
        assert!(caps.supports(FanMode::Auto) && caps.supports(FanMode::Max));
        assert!(!caps.supports(FanMode::Manual) && !caps.supports(FanMode::Curve));
    }

    #[test]
    fn asking_such_a_machine_for_a_speed_is_an_error_not_a_silent_no_op() {
        let dir = fixture("nopwm-apply", &["pwm1_enable"]);
        let err = apply(
            &paths(&dir),
            Capabilities::detect(&paths(&dir)),
            FanMode::Manual,
            128,
        )
        .expect_err("manual must be refused");

        assert!(matches!(err, ControlError::Unsupported("manual", "pwm1")));
        assert_eq!(
            read(&dir, "pwm1_enable"),
            "2",
            "the firmware curve must be left alone"
        );
    }

    #[test]
    fn auto_and_max_write_the_documented_values() {
        let dir = fixture("modes", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);
        let caps = Capabilities::detect(&p);

        apply(&p, caps, FanMode::Max, 0).unwrap();
        assert_eq!(read(&dir, "pwm1_enable"), "0");

        apply(&p, caps, FanMode::Auto, 0).unwrap();
        assert_eq!(read(&dir, "pwm1_enable"), "2");
    }

    /// A board without `pwm1` at all (8D2F) must still be able to switch to
    /// auto - the sentinel write is skipped, not a hard requirement.
    #[test]
    fn auto_without_a_pwm1_channel_still_switches_mode() {
        let dir = fixture("auto-nopwm1", &["pwm1_enable"]);
        let p = paths(&dir);

        apply(&p, Capabilities::detect(&p), FanMode::Auto, 0).unwrap();

        assert_eq!(read(&dir, "pwm1_enable"), "2");
    }

    /// Restoring to auto after a manual/probe run must release the EC's
    /// last commanded speed, not just stop the keep-alive. See the comment
    /// on the `Auto` arm of `apply`: `hp_wmi_apply_fan_settings`'s auto
    /// path does not always reset the setpoint itself, so Pyren has to
    /// write the `HP_FAN_SPEED_AUTOMATIC` sentinel (`pwm1 = 0`) itself,
    /// before the mode switch.
    #[test]
    fn auto_releases_the_last_manual_speed_via_the_sentinel_before_the_mode_switch() {
        let dir = fixture("auto-sentinel", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);
        let caps = Capabilities::detect(&p);

        // Simulate the state a speed probe leaves behind: manual mode, fans
        // commanded to a high pwm.
        apply(&p, caps, FanMode::Manual, 200).unwrap();
        assert_eq!(read(&dir, "pwm1"), "200");

        apply(&p, caps, FanMode::Auto, 0).unwrap();

        assert_eq!(
            read(&dir, "pwm1"),
            "0",
            "pwm1 must be released to the HP_FAN_SPEED_AUTOMATIC sentinel, \
             not left at the last commanded speed"
        );
        assert_eq!(read(&dir, "pwm1_enable"), "2");
    }

    #[test]
    fn manual_sets_both_fans_and_the_mode() {
        let dir = fixture("manual", &["pwm1_enable", "pwm1", "pwm2"]);
        let p = paths(&dir);

        apply(&p, Capabilities::detect(&p), FanMode::Manual, 200).unwrap();

        assert_eq!(read(&dir, "pwm1"), "200");
        assert_eq!(read(&dir, "pwm2"), "200");
        assert_eq!(read(&dir, "pwm1_enable"), "1");
    }

    /// Plenty of drivers have no second channel; that is not an error.
    #[test]
    fn a_driver_without_pwm2_is_driven_through_pwm1_alone() {
        let dir = fixture("manual-nopwm2", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);

        apply(&p, Capabilities::detect(&p), FanMode::Curve, 90).unwrap();

        assert_eq!(read(&dir, "pwm1"), "90");
        assert!(
            !dir.join("pwm2").exists(),
            "a missing channel must not be created"
        );
    }

    /// On a feature-table board `pwm1_enable = 1` replaces the setpoints
    /// with the current fan speed, so it has to come before the speed.
    #[test]
    fn the_mode_switch_comes_before_the_speed() {
        let (enable, pwm1, pwm2) = (Path::new("e"), Path::new("p1"), Path::new("p2"));

        let from_auto = speed_writes(enable, pwm1, Some(pwm2), Some(2), 200);

        assert_eq!(
            from_auto,
            vec![
                (enable, "1".to_string()),
                (pwm1, "200".to_string()),
                (pwm2, "200".to_string())
            ]
        );
    }

    /// And a curve tick in a driver that is already in manual must not
    /// switch again, or it undoes the speed it is about to set.
    #[test]
    fn a_driver_already_in_manual_is_not_switched_again() {
        let (enable, pwm1) = (Path::new("e"), Path::new("p1"));

        let writes = speed_writes(enable, pwm1, None, Some(1), 120);

        assert_eq!(writes, vec![(pwm1, "120".to_string())]);
    }

    /// A mode that cannot be read is not known to be manual.
    #[test]
    fn an_unreadable_mode_is_switched_to_be_sure() {
        let (enable, pwm1) = (Path::new("e"), Path::new("p1"));

        assert_eq!(
            speed_writes(enable, pwm1, None, None, 120)[0],
            (enable, "1".to_string())
        );
    }

    fn with_params(dir: &Path, table: &str) {
        fs::create_dir_all(dir.join("parameters")).unwrap();
        fs::write(dir.join("parameters/min_rpm_table"), format!("{table}\n")).unwrap();
        fs::write(dir.join("parameters/min_rpm_override"), "0\n").unwrap();
    }

    #[test]
    fn the_drivers_floor_is_read_in_rpm() {
        let dir = fixture("floor", &["pwm1_enable", "pwm1"]);
        with_params(&dir, "18");
        let p = paths(&dir);

        assert_eq!(read_driver_floor(&p), Some(1800));
        assert!(floor_override_supported(&p));
    }

    /// A table the driver could not read reports 0, which is no floor.
    #[test]
    fn a_zero_table_floor_is_no_floor() {
        let dir = fixture("floor-zero", &["pwm1_enable", "pwm1"]);
        with_params(&dir, "0");
        assert_eq!(read_driver_floor(&paths(&dir)), None);
    }

    #[test]
    fn the_override_is_written_in_hundreds() {
        let dir = fixture("override", &["pwm1_enable", "pwm1"]);
        with_params(&dir, "18");
        let p = paths(&dir);

        set_floor_override(&p, 7).unwrap();
        assert_eq!(read_floor_override(&p), Some(7));
    }

    /// A driver built before the parameter existed: the table's floor is
    /// the only one, and asking for another is an error, not a silent no-op.
    #[test]
    fn a_driver_without_the_override_refuses_one() {
        let dir = fixture("no-override", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);

        assert!(!floor_override_supported(&p));
        assert!(matches!(
            set_floor_override(&p, 7),
            Err(ControlError::Unsupported(..))
        ));
        assert!(!dir.join("parameters/min_rpm_override").exists());
    }

    #[test]
    fn a_machine_with_no_interface_supports_nothing() {
        let caps = Capabilities::detect(&FanPaths::default());
        assert!(!caps.switch_mode && !caps.set_speed);
        for mode in [FanMode::Auto, FanMode::Max, FanMode::Manual, FanMode::Curve] {
            assert!(!caps.supports(mode));
        }
    }
}
