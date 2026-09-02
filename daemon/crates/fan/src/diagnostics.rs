//! Fan-control self-test.
//!
//! Answers one question in detail: **does fan control actually work on this
//! machine, and if not, what exactly is missing?** That replaced installing
//! a patched driver as this project's approach - manual fan control is
//! upstream in recent kernels, so on most machines the right answer is
//! "the stock driver already does this", and the useful thing is to prove
//! it rather than to replace it.
//!
//! Every check is **read-only by default**. The one check that has to write
//! to hardware is opt-in, writes the value that is already set (so no fan
//! actually changes speed), and restores the previous mode afterwards even
//! if it fails partway.

use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use serde::Serialize;

use crate::FanPaths;

/// Board ids known to work, used only to warn - never to gate anything.
/// Mirrors the list in the installer crate and the Python original.
const SUPPORTED_BOARD_HINT: &str =
    "https://github.com/arfelious/omen-fan-control (patched hp-wmi driver)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckStatus {
    /// Works.
    Pass,
    /// Doesn't work, and something depends on it.
    Fail,
    /// Works but with a caveat, or an optional feature is absent.
    Warn,
    /// Couldn't be tested here (needs privileges, or a prerequisite failed).
    Skip,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about it, when there is something to do.
    pub remedy: Option<String>,
}

impl Check {
    fn new(id: &str, title: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            status,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

/// Overall conclusion, in the terms a user cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// Reading and writing fan speed both work.
    FullControl,
    /// Fan speeds can be read, but not set.
    MonitoringOnly,
    /// This machine exposes no HP fan interface at all.
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnosis {
    pub verdict: Verdict,
    pub summary: String,
    /// Present when a driver that might help exists but isn't in use.
    pub driver_notice: Option<String>,
    pub checks: Vec<Check>,
    pub wrote_to_hardware: bool,
}

impl Diagnosis {
    pub fn passed(&self) -> usize {
        self.checks.iter().filter(|c| c.status == CheckStatus::Pass).count()
    }

    pub fn failed(&self) -> usize {
        self.checks.iter().filter(|c| c.status == CheckStatus::Fail).count()
    }
}

/// Runs the self-test. `allow_writes` enables the one check that touches
/// hardware; without it that check is reported as skipped.
pub(crate) fn diagnose(paths: &FanPaths, allow_writes: bool) -> Diagnosis {
    let mut checks = Vec::new();

    let hp_wmi = Path::new("/sys/devices/platform/hp-wmi").exists();
    checks.push(if hp_wmi {
        Check::new(
            "hp-wmi",
            "hp-wmi platform driver",
            CheckStatus::Pass,
            "/sys/devices/platform/hp-wmi is present",
        )
    } else {
        Check::new(
            "hp-wmi",
            "hp-wmi platform driver",
            CheckStatus::Fail,
            "/sys/devices/platform/hp-wmi does not exist",
        )
        .with_remedy(
            "This is normal on non-HP hardware. On an HP laptop, check that the hp_wmi module \
             is loaded (`modprobe hp_wmi`) and that the BIOS exposes WMI.",
        )
    });

    checks.push(match &paths.hwmon_dir {
        Some(dir) => Check::new(
            "hwmon",
            "hwmon node",
            CheckStatus::Pass,
            format!("found at {}", dir.display()),
        ),
        None => Check::new(
            "hwmon",
            "hwmon node",
            CheckStatus::Fail,
            "no hwmon directory under /sys/devices/platform/hp-wmi/hwmon",
        ),
    });

    checks.push(check_readable_number("fan1", "Fan 1 speed", paths.fan1_input.as_deref(), |rpm| {
        describe_rpm(rpm)
    }));
    checks.push(check_readable_number("fan2", "Fan 2 speed", paths.fan2_input.as_deref(), |rpm| {
        describe_rpm(rpm)
    }));

    checks.push(check_pwm(paths.pwm1.as_deref()));
    checks.push(check_pwm_enable(paths.pwm1_enable.as_deref()));
    checks.push(check_write(paths, allow_writes));
    checks.push(check_platform_profile());
    checks.push(check_cpu_temp(paths.cpu_temp.as_deref()));
    checks.push(check_acpi_call());

    let can_read = paths.fan1_input.is_some() || paths.fan2_input.is_some();
    let can_write = checks
        .iter()
        .any(|c| c.id == "pwm-write" && c.status == CheckStatus::Pass)
        || (paths.pwm1.is_some() && paths.pwm1_enable.is_some() && !allow_writes);

    let verdict = match (can_read, paths.pwm1.is_some(), can_write) {
        (_, true, true) => Verdict::FullControl,
        (true, _, _) => Verdict::MonitoringOnly,
        _ => Verdict::Unsupported,
    };

    let (summary, driver_notice) = conclude(verdict, hp_wmi, paths, allow_writes);

    Diagnosis {
        verdict,
        summary,
        driver_notice,
        checks,
        wrote_to_hardware: allow_writes,
    }
}

fn conclude(
    verdict: Verdict,
    hp_wmi: bool,
    paths: &FanPaths,
    allow_writes: bool,
) -> (String, Option<String>) {
    match verdict {
        Verdict::FullControl => {
            let summary = if allow_writes {
                "Fan control works: speeds can be read and the PWM channel accepted a write."
                    .to_string()
            } else {
                "Fan control looks available: the PWM channel is present. Re-run with writes \
                 enabled to confirm the hardware accepts them."
                    .to_string()
            };
            (summary, None)
        }
        Verdict::MonitoringOnly => (
            "Fan speeds can be read, but this driver exposes no PWM channel, so speed cannot \
             be set."
                .to_string(),
            // This is the case the patched driver exists for: hp-wmi is
            // there, but the running kernel's copy has no fan control for
            // this board.
            hp_wmi.then(|| {
                format!(
                    "The kernel's hp-wmi has no pwm1 for this board. Recent kernels ship manual \
                     fan control upstream, so upgrading the kernel is the first thing to try. \
                     Failing that, a patched out-of-tree driver exists: {SUPPORTED_BOARD_HINT}. \
                     Installing it replaces a kernel module, so it is a deliberate step, not \
                     something this app does for you."
                )
            }),
        ),
        Verdict::Unsupported => {
            let detail = if paths.cpu_temp.is_some() {
                "Temperature can still be read, so monitoring works."
            } else {
                "No HP fan interface and no usable temperature sensor were found."
            };
            (
                format!("This machine exposes no HP fan-control interface. {detail}"),
                (!hp_wmi).then(|| {
                    "No hp-wmi device at all. On an HP OMEN or Victus laptop, load the hp_wmi \
                     module; on other hardware this is expected and fan control is not \
                     applicable."
                        .to_string()
                }),
            )
        }
    }
}

fn describe_rpm(raw: i64) -> (CheckStatus, String) {
    // hp-wmi encodes fan-cleaner reverse-spin in the value itself; see
    // docs/02-kernel-driver.md in the source project.
    if raw >= 12800 {
        let actual = ((raw / 100) & 0x7F) * 100;
        return (
            CheckStatus::Warn,
            format!("{raw} raw -> {actual} rpm, spinning in reverse (fan cleaner active)"),
        );
    }
    if raw > 25000 {
        return (CheckStatus::Warn, format!("{raw} rpm is implausibly high"));
    }
    (CheckStatus::Pass, format!("{raw} rpm"))
}

fn check_readable_number(
    id: &str,
    title: &str,
    path: Option<&Path>,
    describe: impl Fn(i64) -> (CheckStatus, String),
) -> Check {
    let Some(path) = path else {
        return Check::new(id, title, CheckStatus::Skip, "not exposed by this driver");
    };
    match fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<i64>() {
            Ok(value) => {
                let (status, detail) = describe(value);
                Check::new(id, title, status, detail)
            }
            Err(_) => Check::new(
                id,
                title,
                CheckStatus::Fail,
                format!("{} contains something that is not a number", path.display()),
            ),
        },
        Err(e) => Check::new(id, title, CheckStatus::Fail, format!("{}: {e}", path.display())),
    }
}

fn check_pwm(path: Option<&Path>) -> Check {
    let Some(path) = path else {
        return Check::new(
            "pwm1",
            "PWM channel",
            CheckStatus::Fail,
            "pwm1 is not exposed, so fan speed cannot be set",
        )
        .with_remedy("Needs a kernel whose hp-wmi supports this board (see the notice below).");
    };
    if !path.exists() {
        return Check::new("pwm1", "PWM channel", CheckStatus::Fail, format!("{} is missing", path.display()))
            .with_remedy("Needs a kernel whose hp-wmi supports this board.");
    }

    match fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(value) if value <= 255 => Check::new(
                "pwm1",
                "PWM channel",
                CheckStatus::Pass,
                format!("pwm1 = {value} (0-255)"),
            ),
            Ok(value) => Check::new(
                "pwm1",
                "PWM channel",
                CheckStatus::Warn,
                format!("pwm1 = {value}, outside the documented 0-255 range"),
            ),
            Err(_) => Check::new("pwm1", "PWM channel", CheckStatus::Fail, "pwm1 is not a number"),
        },
        Err(e) => Check::new("pwm1", "PWM channel", CheckStatus::Fail, format!("{e}")),
    }
}

/// `pwm1_enable`: 0 = max, 1 = manual, 2 = automatic (firmware curve).
fn check_pwm_enable(path: Option<&Path>) -> Check {
    let Some(path) = path else {
        return Check::new(
            "pwm1_enable",
            "Fan control mode",
            CheckStatus::Fail,
            "pwm1_enable is not exposed",
        );
    };
    match fs::read_to_string(path) {
        Ok(text) => {
            let value = text.trim();
            let meaning = match value {
                "0" => "max (firmware overridden to full speed)",
                "1" => "manual (pwm1 is in effect)",
                "2" => "automatic (firmware curve)",
                _ => "unknown mode",
            };
            let status =
                if matches!(value, "0" | "1" | "2") { CheckStatus::Pass } else { CheckStatus::Warn };
            Check::new("pwm1_enable", "Fan control mode", status, format!("{value} - {meaning}"))
        }
        Err(e) => Check::new("pwm1_enable", "Fan control mode", CheckStatus::Fail, format!("{e}")),
    }
}

/// The only check that writes.
///
/// It writes the value that is *already* set, so no fan changes speed, and
/// it puts the previous mode back afterwards - including when the readback
/// fails, which is why the restore is not conditional on success.
fn check_write(paths: &FanPaths, allow_writes: bool) -> Check {
    const ID: &str = "pwm-write";
    const TITLE: &str = "PWM accepts writes";

    let (Some(pwm), Some(enable)) = (paths.pwm1.as_deref(), paths.pwm1_enable.as_deref()) else {
        return Check::new(ID, TITLE, CheckStatus::Skip, "no PWM channel to write to");
    };
    if !allow_writes {
        return Check::new(
            ID,
            TITLE,
            CheckStatus::Skip,
            "not attempted; enable writes to test this",
        );
    }

    let Ok(original_mode) = fs::read_to_string(enable) else {
        return Check::new(ID, TITLE, CheckStatus::Fail, "could not read the current mode");
    };
    let Ok(original_pwm) = fs::read_to_string(pwm) else {
        return Check::new(ID, TITLE, CheckStatus::Fail, "could not read the current PWM value");
    };
    let (original_mode, original_pwm) = (original_mode.trim(), original_pwm.trim());

    // Manual mode, then re-write the value that is already set.
    let result = fs::write(enable, "1")
        .and_then(|()| fs::write(pwm, original_pwm))
        .and_then(|()| fs::read_to_string(pwm));

    // Restore before interpreting anything, so an early return can't leave
    // the fans under our control.
    let _ = fs::write(pwm, original_pwm);
    let restored = fs::write(enable, original_mode);

    let mut check = match result {
        Ok(readback) if readback.trim() == original_pwm => Check::new(
            ID,
            TITLE,
            CheckStatus::Pass,
            format!("wrote and read back pwm1 = {original_pwm} without changing fan speed"),
        ),
        Ok(readback) => Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!(
                "wrote {original_pwm} but read back {}; the driver may quantise or ignore values",
                readback.trim()
            ),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Check::new(ID, TITLE, CheckStatus::Skip, "permission denied; run as root to test writes")
        }
        Err(e) => Check::new(ID, TITLE, CheckStatus::Fail, format!("{e}")),
    };

    if restored.is_err() {
        check.detail.push_str(
            " - WARNING: the original fan mode could not be restored; set it manually or reboot",
        );
        check.status = CheckStatus::Warn;
    }
    check
}

fn check_platform_profile() -> Check {
    const PATH: &str = "/sys/firmware/acpi/platform_profile";
    const CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";

    match (fs::read_to_string(PATH), fs::read_to_string(CHOICES)) {
        (Ok(current), Ok(choices)) => Check::new(
            "platform-profile",
            "ACPI platform profile",
            CheckStatus::Pass,
            format!("{} (available: {})", current.trim(), choices.trim()),
        ),
        _ => Check::new(
            "platform-profile",
            "ACPI platform profile",
            CheckStatus::Warn,
            "not exposed; power modes fall back to power-profiles-daemon or the CPU EPP hint",
        ),
    }
}

fn check_cpu_temp(path: Option<&Path>) -> Check {
    check_readable_number("cpu-temp", "CPU temperature", path, |millidegrees| {
        let celsius = millidegrees / 1000;
        if (0..=125).contains(&celsius) {
            (CheckStatus::Pass, format!("{celsius} °C"))
        } else {
            (CheckStatus::Warn, format!("{celsius} °C is implausible"))
        }
    })
}

fn check_acpi_call() -> Check {
    if Path::new("/proc/acpi/call").exists() {
        Check::new(
            "acpi-call",
            "acpi_call module (fan cleaner)",
            CheckStatus::Pass,
            "/proc/acpi/call is available",
        )
    } else {
        Check::new(
            "acpi-call",
            "acpi_call module (fan cleaner)",
            CheckStatus::Warn,
            "/proc/acpi/call not found; only the dust-removal fan cleaner needs it",
        )
        .with_remedy("Install acpi_call-dkms (Arch), acpi-call-dkms (Debian) or akmod-acpi_call (Fedora), then `modprobe acpi_call`.")
    }
}

/// Path set built from an explicit directory, so the checks can be pointed
/// at a fixture instead of the real machine.
#[cfg(test)]
fn paths_for_testing(hwmon_dir: PathBuf, cpu_temp: Option<PathBuf>) -> FanPaths {
    FanPaths {
        pwm1: Some(hwmon_dir.join("pwm1")),
        pwm1_enable: Some(hwmon_dir.join("pwm1_enable")),
        fan1_input: Some(hwmon_dir.join("fan1_input")),
        fan2_input: Some(hwmon_dir.join("fan2_input")),
        hwmon_dir: Some(hwmon_dir),
        cpu_temp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("omen-hub-fan-diag-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, value: &str) {
        fs::write(dir.join(name), value).unwrap();
    }

    fn check<'a>(diagnosis: &'a Diagnosis, id: &str) -> &'a Check {
        diagnosis.checks.iter().find(|c| c.id == id).expect("check should exist")
    }

    #[test]
    fn a_machine_with_no_interface_at_all_is_unsupported() {
        let diagnosis = diagnose(&FanPaths::default(), false);
        assert_eq!(diagnosis.verdict, Verdict::Unsupported);
        assert_eq!(check(&diagnosis, "fan1").status, CheckStatus::Skip);
    }

    #[test]
    fn readable_fans_without_pwm_are_monitoring_only() {
        let dir = fixture("monitoring");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "fan2_input", "2300\n");
        let mut paths = paths_for_testing(dir, None);
        // The board's driver exposes speeds but no control channel.
        paths.pwm1 = None;
        paths.pwm1_enable = None;

        let diagnosis = diagnose(&paths, false);
        assert_eq!(diagnosis.verdict, Verdict::MonitoringOnly);
        assert_eq!(check(&diagnosis, "fan1").status, CheckStatus::Pass);
        assert_eq!(check(&diagnosis, "pwm1").status, CheckStatus::Fail);
    }

    #[test]
    fn a_full_interface_reports_full_control() {
        let dir = fixture("full");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "fan2_input", "2500\n");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(diagnosis.verdict, Verdict::FullControl);
        assert_eq!(check(&diagnosis, "pwm1").status, CheckStatus::Pass);
        assert!(check(&diagnosis, "pwm1_enable").detail.contains("automatic"));
    }

    #[test]
    fn the_reverse_spin_encoding_is_decoded_rather_than_reported_as_a_huge_rpm() {
        let dir = fixture("reverse");
        // 0x80 | 24 hundreds of rpm, i.e. 2400 rpm spinning backwards.
        write(&dir, "fan1_input", "15200\n");
        write(&dir, "pwm1", "100\n");
        write(&dir, "pwm1_enable", "1\n");

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        let fan1 = check(&diagnosis, "fan1");
        assert_eq!(fan1.status, CheckStatus::Warn);
        assert!(fan1.detail.contains("2400 rpm"), "got: {}", fan1.detail);
        assert!(fan1.detail.contains("reverse"));
    }

    #[test]
    fn the_write_test_is_skipped_unless_asked_for() {
        let dir = fixture("nowrite");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(check(&diagnosis, "pwm-write").status, CheckStatus::Skip);
        assert!(!diagnosis.wrote_to_hardware);
    }

    #[test]
    fn the_write_test_restores_the_previous_mode() {
        let dir = fixture("write");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let diagnosis = diagnose(&paths_for_testing(dir.clone(), None), true);
        assert_eq!(check(&diagnosis, "pwm-write").status, CheckStatus::Pass);
        // Automatic mode, and the same speed, exactly as before.
        assert_eq!(fs::read_to_string(dir.join("pwm1_enable")).unwrap().trim(), "2");
        assert_eq!(fs::read_to_string(dir.join("pwm1")).unwrap().trim(), "128");
    }

    /// The notice logic is tested through `conclude` directly, because
    /// whether hp-wmi exists is a property of the machine running the
    /// tests - which must not decide whether the assertion is meaningful.
    #[test]
    fn an_hp_machine_without_pwm_is_told_a_patched_driver_exists() {
        let (_, notice) = conclude(Verdict::MonitoringOnly, true, &FanPaths::default(), false);
        let notice = notice.expect("an HP machine with no pwm should get the notice");
        // Upgrading the kernel comes first: fan control is upstream now, so
        // replacing a kernel module should be the fallback, not the advice.
        let kernel_hint = notice.find("upgrading the kernel").expect("should suggest the kernel");
        let driver_hint = notice.find("patched").expect("should mention the patched driver");
        assert!(kernel_hint < driver_hint);
    }

    #[test]
    fn a_non_hp_machine_is_not_pointed_at_an_hp_driver() {
        // Suggesting a patched HP driver on a machine with no hp-wmi at all
        // would just be noise.
        let (_, notice) = conclude(Verdict::MonitoringOnly, false, &FanPaths::default(), false);
        assert!(notice.is_none());
    }

    #[test]
    fn a_working_machine_gets_no_driver_notice() {
        let (_, notice) = conclude(Verdict::FullControl, true, &FanPaths::default(), true);
        assert!(notice.is_none());
    }

    #[test]
    fn a_machine_with_no_hp_wmi_is_told_what_that_means() {
        let (summary, notice) = conclude(Verdict::Unsupported, false, &FanPaths::default(), false);
        assert!(summary.contains("no HP fan-control interface"));
        assert!(notice.unwrap().contains("hp_wmi"));
    }

    #[test]
    fn an_unparseable_sysfs_value_fails_rather_than_being_read_as_zero() {
        let dir = fixture("garbage");
        write(&dir, "fan1_input", "not a number\n");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(check(&diagnosis, "fan1").status, CheckStatus::Fail);
    }
}
