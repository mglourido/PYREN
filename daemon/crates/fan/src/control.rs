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
}

fn write_sysfs(path: &Path, value: &str) -> Result<(), ControlError> {
    let shown = path.display().to_string();
    fs::write(path, format!("{value}\n")).map_err(|e| match e.kind() {
        ErrorKind::PermissionDenied => ControlError::PermissionDenied(shown, e.to_string()),
        _ => ControlError::Io(shown, e.to_string()),
    })
}

/// Applies a mode to the hardware.
///
/// `pwm` is only consulted for the modes that need one, and the order of
/// the two writes is deliberate: writing `pwm1` first stores the value in
/// the driver, so that switching to `PWM_MODE_MANUAL` immediately applies
/// *it* rather than the driver's own default of 128 (a bare
/// `pwm1_enable = 1` on a fresh boot means 50 %, chosen by nobody).
pub fn apply(paths: &FanPaths, caps: Capabilities, mode: FanMode, pwm: u8) -> Result<(), ControlError> {
    if !caps.supports(mode) {
        return Err(ControlError::Unsupported(
            mode.as_str(),
            if caps.switch_mode { "pwm1" } else { "pwm1_enable" },
        ));
    }

    let enable = paths
        .pwm1_enable
        .as_deref()
        .ok_or(ControlError::Unsupported(mode.as_str(), "pwm1_enable"))?;

    match mode {
        FanMode::Auto => write_sysfs(enable, "2"),
        FanMode::Max => write_sysfs(enable, "0"),
        FanMode::Manual | FanMode::Curve => {
            let pwm1 = paths.pwm1.as_deref().ok_or(ControlError::Unsupported(mode.as_str(), "pwm1"))?;
            write_sysfs(pwm1, &pwm.to_string())?;
            write_sysfs(enable, "1")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(tag: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pyren-fan-control-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in files {
            fs::write(dir.join(f), "2\n").unwrap();
        }
        dir
    }

    fn paths(dir: &Path) -> FanPaths {
        FanPaths {
            hwmon_dir: Some(dir.to_path_buf()),
            pwm1: Some(dir.join("pwm1")),
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            fan2_input: Some(dir.join("fan2_input")),
            cpu_temp: None,
        }
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap().trim().to_string()
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
        let err = apply(&paths(&dir), Capabilities::detect(&paths(&dir)), FanMode::Manual, 128)
            .expect_err("manual must be refused");

        assert!(matches!(err, ControlError::Unsupported("manual", "pwm1")));
        assert_eq!(read(&dir, "pwm1_enable"), "2", "the firmware curve must be left alone");
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

    /// The speed has to be in place before the mode switch, or the driver
    /// applies its own default of 128 for one keep-alive period.
    #[test]
    fn manual_writes_the_speed_before_switching_mode() {
        let dir = fixture("manual", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);

        apply(&p, Capabilities::detect(&p), FanMode::Manual, 200).unwrap();

        assert_eq!(read(&dir, "pwm1"), "200");
        assert_eq!(read(&dir, "pwm1_enable"), "1");
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
