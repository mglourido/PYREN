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

use pyren_core::{msg, Msg};
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
    /// Translatable - render with `tm()`.
    pub title: Msg,
    pub status: CheckStatus,
    /// Translatable - render with `tm()`.
    pub detail: Msg,
    /// What to do about it, when there is something to do. Translatable.
    pub remedy: Option<Msg>,
}

impl Check {
    /// Public because `pyren-check` builds the power and lighting sections
    /// of its compatibility report out of the same shape - one check type,
    /// one renderer, one JSON schema, rather than three that drift.
    ///
    /// `title` and `detail` take `impl Into<Msg>`, so a `&str`/`String` still
    /// works (as a key-less, untranslated `Msg`) while a call site that has a
    /// catalog key passes `msg!(...)`.
    pub fn new(
        id: &str,
        title: impl Into<Msg>,
        status: CheckStatus,
        detail: impl Into<Msg>,
    ) -> Self {
        Self {
            id: id.to_string(),
            title: title.into(),
            status,
            detail: detail.into(),
            remedy: None,
        }
    }

    pub fn with_remedy(mut self, remedy: impl Into<Msg>) -> Self {
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
    /// Translatable - render with `tm()`.
    pub summary: Msg,
    /// Present when a driver that might help exists but isn't in use.
    /// Translatable.
    pub driver_notice: Option<Msg>,
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
    let hp_wmi_title = msg!("diagnostics.checks.hp-wmi.title", "hp-wmi platform driver");
    checks.push(if hp_wmi {
        Check::new(
            "hp-wmi",
            hp_wmi_title,
            CheckStatus::Pass,
            msg!("diagnostics.checks.hp-wmi.present", "/sys/devices/platform/hp-wmi is present"),
        )
    } else {
        Check::new(
            "hp-wmi",
            hp_wmi_title,
            CheckStatus::Fail,
            msg!("diagnostics.checks.hp-wmi.absent", "/sys/devices/platform/hp-wmi does not exist"),
        )
        .with_remedy(msg!(
            "diagnostics.checks.hp-wmi.remedy",
            "This is normal on non-HP hardware. On an HP laptop, check that the hp_wmi module \
             is loaded (`modprobe hp_wmi`) and that the BIOS exposes WMI."
        ))
    });

    let hwmon_title = msg!("diagnostics.checks.hwmon.title", "hwmon node");
    checks.push(match &paths.hwmon_dir {
        Some(dir) => Check::new(
            "hwmon",
            hwmon_title,
            CheckStatus::Pass,
            msg!(
                "diagnostics.checks.hwmon.found",
                { "path" => dir.display().to_string() },
                "found at {path}"
            ),
        ),
        None => Check::new(
            "hwmon",
            hwmon_title,
            CheckStatus::Fail,
            msg!(
                "diagnostics.checks.hwmon.absent",
                "no hwmon directory under /sys/devices/platform/hp-wmi/hwmon"
            ),
        ),
    });

    checks.push(check_readable_number(
        "fan1",
        msg!("diagnostics.checks.fan1.title", "Fan 1 speed"),
        paths.fan1_input.as_deref(),
        describe_rpm,
    ));
    checks.push(check_readable_number(
        "fan2",
        msg!("diagnostics.checks.fan2.title", "Fan 2 speed"),
        paths.fan2_input.as_deref(),
        describe_rpm,
    ));

    checks.push(check_pwm(paths.pwm1.as_deref()));
    checks.push(check_pwm_enable(paths.pwm1_enable.as_deref()));
    checks.push(check_write(paths, allow_writes));
    checks.push(check_hwmon_attributes(paths.hwmon_dir.as_deref()));
    checks.push(check_kernel_log());
    checks.push(check_platform_profile());
    checks.push(check_cpu_temp(paths.cpu_temp.as_deref()));
    checks.push(check_acpi_call());
    checks.push(check_fan_cleaner());

    // The verdict follows the *check results*, never the mere presence of a
    // path. Discovery fills in every path as soon as an hwmon directory
    // exists, whether or not the files behind them do - so deriving the
    // verdict from paths reported "fan control available" on exactly the
    // machines this tool exists for: an HP laptop whose stock driver has no
    // pwm1 for its board.
    let status_of = |id: &str| checks.iter().find(|c| c.id == id).map(|c| c.status);
    let readable = |id: &str| matches!(status_of(id), Some(CheckStatus::Pass | CheckStatus::Warn));

    let can_read = readable("fan1") || readable("fan2");
    let can_write = status_of("pwm1") == Some(CheckStatus::Pass)
        && status_of("pwm1_enable") == Some(CheckStatus::Pass)
        // Only an actual write failure rules control out. A skipped write
        // test (not requested, or no permission) leaves it untested, not
        // disproven - the summary says which.
        && status_of("pwm-write") != Some(CheckStatus::Fail);

    let verdict = match (can_read, can_write) {
        (_, true) => Verdict::FullControl,
        (true, false) => Verdict::MonitoringOnly,
        (false, false) => Verdict::Unsupported,
    };

    let write_tested = status_of("pwm-write") == Some(CheckStatus::Pass);

    let (summary, driver_notice) = conclude(verdict, hp_wmi, paths, write_tested);

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
    write_tested: bool,
) -> (Msg, Option<Msg>) {
    match verdict {
        Verdict::FullControl => {
            let summary = if write_tested {
                msg!(
                    "diagnostics.summary.fullControlTested",
                    "Fan control works: speeds can be read and the PWM channel accepted a write."
                )
            } else {
                msg!(
                    "diagnostics.summary.fullControlUntested",
                    "Fan control looks available: the PWM channel is present. Re-run with \
                     writes enabled to confirm the hardware accepts them."
                )
            };
            (summary, None)
        }
        Verdict::MonitoringOnly => (
            msg!(
                "diagnostics.summary.monitoringOnly",
                "Fan speeds can be read, but this driver exposes no PWM channel, so speed \
                 cannot be set."
            ),
            // This is the case the patched driver exists for: hp-wmi is
            // there, but the running kernel's copy has no fan control for
            // this board.
            hp_wmi.then(|| {
                msg!(
                    "diagnostics.driverNotice.noPwm",
                    { "hint" => SUPPORTED_BOARD_HINT },
                    "The kernel's hp-wmi has no pwm1 for this board. Recent kernels ship \
                     manual fan control upstream, so upgrading the kernel is the first thing \
                     to try. Failing that, a patched out-of-tree driver exists: {hint}. \
                     Installing it replaces a kernel module, so it is a deliberate step, not \
                     something this app does for you."
                )
            }),
        ),
        Verdict::Unsupported => {
            let summary = if paths.cpu_temp.is_some() {
                msg!(
                    "diagnostics.summary.unsupportedWithTemp",
                    "This machine exposes no HP fan-control interface. Temperature can still \
                     be read, so monitoring works."
                )
            } else {
                msg!(
                    "diagnostics.summary.unsupportedNoTemp",
                    "This machine exposes no HP fan-control interface. No HP fan interface \
                     and no usable temperature sensor were found."
                )
            };
            (
                summary,
                (!hp_wmi).then(|| {
                    msg!(
                        "diagnostics.driverNotice.noHpWmi",
                        "No hp-wmi device at all. On an HP OMEN or Victus laptop, load the \
                         hp_wmi module; on other hardware this is expected and fan control \
                         is not applicable."
                    )
                }),
            )
        }
    }
}

fn describe_rpm(raw: i64) -> (CheckStatus, Msg) {
    // hp-wmi encodes fan-cleaner reverse-spin in the value itself; see
    // docs/02-kernel-driver.md in the source project.
    if raw >= 12800 {
        let actual = ((raw / 100) & 0x7F) * 100;
        return (
            CheckStatus::Warn,
            msg!(
                "diagnostics.checks.fan.reverse",
                { "raw" => raw, "actual" => actual },
                "{raw} raw -> {actual} rpm, spinning in reverse (fan cleaner active)"
            ),
        );
    }
    if raw > 25000 {
        return (
            CheckStatus::Warn,
            msg!("diagnostics.checks.fan.tooHigh", { "raw" => raw }, "{raw} rpm is implausibly high"),
        );
    }
    (CheckStatus::Pass, msg!("diagnostics.checks.fan.rpm", { "raw" => raw }, "{raw} rpm"))
}

fn check_readable_number(
    id: &str,
    title: Msg,
    path: Option<&Path>,
    describe: impl Fn(i64) -> (CheckStatus, Msg),
) -> Check {
    let not_exposed =
        || msg!("diagnostics.checks.notExposed", "not exposed by this driver");
    let Some(path) = path else {
        return Check::new(id, title, CheckStatus::Skip, not_exposed());
    };
    // Discovery fills in a path as soon as an hwmon node exists, so the
    // file behind it may not. A missing fan2_input means the machine has
    // one fan, which is normal - not a failure.
    if !path.exists() {
        return Check::new(id, title, CheckStatus::Skip, not_exposed());
    }
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
                msg!(
                    "diagnostics.checks.notANumberAt",
                    { "path" => path.display().to_string() },
                    "{path} contains something that is not a number"
                ),
            ),
        },
        Err(e) => Check::new(
            id,
            title,
            CheckStatus::Fail,
            msg!(
                "diagnostics.checks.readError",
                { "path" => path.display().to_string(), "error" => e.to_string() },
                "{path}: {error}"
            ),
        ),
    }
}

fn check_pwm(path: Option<&Path>) -> Check {
    let title = || msg!("diagnostics.checks.pwm1.title", "PWM channel");
    let Some(path) = path else {
        return Check::new(
            "pwm1",
            title(),
            CheckStatus::Fail,
            msg!(
                "diagnostics.checks.pwm1.notExposed",
                "pwm1 is not exposed, so fan speed cannot be set"
            ),
        )
        .with_remedy(msg!(
            "diagnostics.checks.pwm1.remedyNotice",
            "Needs a kernel whose hp-wmi supports this board (see the notice below)."
        ));
    };
    if !path.exists() {
        return Check::new(
            "pwm1",
            title(),
            CheckStatus::Fail,
            msg!(
                "diagnostics.checks.pwm1.missing",
                { "path" => path.display().to_string() },
                "{path} is missing"
            ),
        )
        .with_remedy(msg!(
            "diagnostics.checks.pwm1.remedy",
            "Needs a kernel whose hp-wmi supports this board."
        ));
    }

    match fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(value) if value <= 255 => Check::new(
                "pwm1",
                title(),
                CheckStatus::Pass,
                msg!("diagnostics.checks.pwm1.ok", { "value" => value }, "pwm1 = {value} (0-255)"),
            ),
            Ok(value) => Check::new(
                "pwm1",
                title(),
                CheckStatus::Warn,
                msg!(
                    "diagnostics.checks.pwm1.outOfRange",
                    { "value" => value },
                    "pwm1 = {value}, outside the documented 0-255 range"
                ),
            ),
            Err(_) => Check::new(
                "pwm1",
                title(),
                CheckStatus::Fail,
                msg!("diagnostics.checks.pwm1.notANumber", "pwm1 is not a number"),
            ),
        },
        Err(e) => Check::new(
            "pwm1",
            title(),
            CheckStatus::Fail,
            msg!("diagnostics.checks.genericError", { "error" => e.to_string() }, "{error}"),
        ),
    }
}

/// `pwm1_enable`: 0 = max, 1 = manual, 2 = automatic (firmware curve).
fn check_pwm_enable(path: Option<&Path>) -> Check {
    let title = || msg!("diagnostics.checks.pwm1_enable.title", "Fan control mode");
    let Some(path) = path else {
        return Check::new(
            "pwm1_enable",
            title(),
            CheckStatus::Fail,
            msg!("diagnostics.checks.pwm1_enable.notExposed", "pwm1_enable is not exposed"),
        );
    };
    match fs::read_to_string(path) {
        Ok(text) => {
            let value = text.trim();
            let detail = match value {
                "0" => msg!(
                    "diagnostics.checks.pwm1_enable.mode0",
                    "0 - max (firmware overridden to full speed)"
                ),
                "1" => msg!("diagnostics.checks.pwm1_enable.mode1", "1 - manual (pwm1 is in effect)"),
                "2" => {
                    msg!("diagnostics.checks.pwm1_enable.mode2", "2 - automatic (firmware curve)")
                }
                _ => msg!(
                    "diagnostics.checks.pwm1_enable.modeUnknown",
                    { "value" => value },
                    "{value} - unknown mode"
                ),
            };
            let status =
                if matches!(value, "0" | "1" | "2") { CheckStatus::Pass } else { CheckStatus::Warn };
            Check::new("pwm1_enable", title(), status, detail)
        }
        Err(e) => Check::new(
            "pwm1_enable",
            title(),
            CheckStatus::Fail,
            msg!("diagnostics.checks.genericError", { "error" => e.to_string() }, "{error}"),
        ),
    }
}

/// The only check that writes.
///
/// It writes the value that is *already* set, so no fan changes speed, and
/// it puts the previous mode back afterwards - including when the readback
/// fails, which is why the restore is not conditional on success.
fn check_write(paths: &FanPaths, allow_writes: bool) -> Check {
    const ID: &str = "pwm-write";
    let title = || msg!("diagnostics.checks.pwm-write.title", "PWM accepts writes");

    let (Some(pwm), Some(enable)) = (paths.pwm1.as_deref(), paths.pwm1_enable.as_deref()) else {
        return Check::new(
            ID,
            title(),
            CheckStatus::Skip,
            msg!("diagnostics.checks.pwm-write.noChannel", "no PWM channel to write to"),
        );
    };
    if !allow_writes {
        return Check::new(
            ID,
            title(),
            CheckStatus::Skip,
            msg!(
                "diagnostics.checks.pwm-write.notAttempted",
                "not attempted; enable writes to test this"
            ),
        );
    }

    let Ok(original_mode) = fs::read_to_string(enable) else {
        return Check::new(
            ID,
            title(),
            CheckStatus::Fail,
            msg!("diagnostics.checks.pwm-write.noMode", "could not read the current mode"),
        );
    };
    let Ok(original_pwm) = fs::read_to_string(pwm) else {
        return Check::new(
            ID,
            title(),
            CheckStatus::Fail,
            msg!("diagnostics.checks.pwm-write.noValue", "could not read the current PWM value"),
        );
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

    let (status, detail) = match result {
        Ok(readback) if readback.trim() == original_pwm => (
            CheckStatus::Pass,
            msg!(
                "diagnostics.checks.pwm-write.ok",
                { "value" => original_pwm },
                "wrote and read back pwm1 = {value} without changing fan speed"
            ),
        ),
        Ok(readback) => (
            CheckStatus::Warn,
            msg!(
                "diagnostics.checks.pwm-write.mismatch",
                { "wrote" => original_pwm, "readback" => readback.trim() },
                "wrote {wrote} but read back {readback}; the driver may quantise or ignore values"
            ),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => (
            CheckStatus::Skip,
            msg!(
                "diagnostics.checks.pwm-write.denied",
                "permission denied; run as root to test writes"
            ),
        ),
        Err(e) => (
            CheckStatus::Fail,
            msg!("diagnostics.checks.genericError", { "error" => e.to_string() }, "{error}"),
        ),
    };

    if restored.is_err() {
        return Check::new(
            ID,
            title(),
            CheckStatus::Warn,
            msg!(
                "diagnostics.checks.pwm-write.notRestored",
                { "detail" => detail.text },
                "{detail} - WARNING: the original fan mode could not be restored; set it \
                 manually or reboot"
            ),
        );
    }
    Check::new(ID, title(), status, detail)
}

/// Lists what the hwmon node actually exposes.
///
/// Without this, a missing `pwm1` is a dead end: the report says the file
/// isn't there but not what *is*, which is the first thing anyone
/// diagnosing a partially-supported board needs to know.
fn check_hwmon_attributes(hwmon_dir: Option<&Path>) -> Check {
    const ID: &str = "hwmon-attrs";
    let title = || msg!("diagnostics.checks.hwmon-attrs.title", "hwmon attributes");

    let Some(dir) = hwmon_dir else {
        return Check::new(
            ID,
            title(),
            CheckStatus::Skip,
            msg!("diagnostics.checks.hwmon-attrs.noNode", "no hwmon node"),
        );
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Check::new(
            ID,
            title(),
            CheckStatus::Skip,
            msg!(
                "diagnostics.checks.hwmon-attrs.unreadable",
                { "path" => dir.display().to_string() },
                "{path} is unreadable"
            ),
        );
    };

    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        // Symlinks back to the device and the power/ subtree say nothing
        // about what the driver exposes.
        .filter(|name| !matches!(name.as_str(), "device" | "subsystem" | "power" | "uevent"))
        .collect();
    names.sort();

    if names.is_empty() {
        return Check::new(
            ID,
            title(),
            CheckStatus::Warn,
            msg!("diagnostics.checks.hwmon-attrs.empty", "the hwmon node is empty"),
        );
    }
    // A bare list of attribute names - not a sentence, nothing to translate.
    Check::new(ID, title(), CheckStatus::Pass, names.join(" "))
}

/// hp-wmi's own kernel messages, which usually say why a board came up
/// with reduced functionality.
fn check_kernel_log() -> Check {
    const ID: &str = "kernel-log";
    let title = || msg!("diagnostics.checks.kernel-log.title", "hp-wmi kernel messages");

    // Via `dmesg` rather than /dev/kmsg: reading that device directly can
    // block waiting for new messages, and it is root-only wherever
    // kernel.dmesg_restrict is set.
    let Ok(output) = std::process::Command::new("dmesg").output() else {
        return Check::new(
            ID,
            title(),
            CheckStatus::Skip,
            msg!("diagnostics.checks.kernel-log.noDmesg", "dmesg is not available"),
        );
    };
    if !output.status.success() {
        return Check::new(
            ID,
            title(),
            CheckStatus::Skip,
            msg!(
                "diagnostics.checks.kernel-log.notReadable",
                "kernel log not readable; run as root, or paste `dmesg | grep -i hp.wmi`"
            ),
        );
    }
    let log = String::from_utf8_lossy(&output.stdout);

    let lines: Vec<&str> = log
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("hp-wmi") || lower.contains("hp_wmi")
        })
        .collect();

    if lines.is_empty() {
        return Check::new(
            ID,
            title(),
            CheckStatus::Pass,
            msg!("diagnostics.checks.kernel-log.none", "no hp-wmi messages"),
        );
    }
    // Only the message text matters; the priority/timestamp prefix is noise.
    // These are the kernel's own words, quoted verbatim - not translated.
    let cleaned: Vec<String> = lines
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|line| line.trim().to_string())
        .collect();
    let quoted = cleaned.join(" | ");

    // The check exists to surface *why a board came up with reduced
    // functionality*. Warning about every line it finds defeats that: the
    // driver announcing that it registered successfully is not a problem,
    // and neither is the taint notice below, so a healthy machine got a
    // yellow row with nothing wrong in it.
    if lines.iter().any(|line| is_concerning(line)) {
        return Check::new(ID, title(), CheckStatus::Warn, quoted);
    }
    if lines.iter().any(|line| is_unsigned_module_notice(line)) {
        return Check::new(
            ID,
            title(),
            CheckStatus::Pass,
            msg!(
                "diagnostics.checks.kernel-log.selfBuilt",
                { "lines" => quoted },
                "Nothing wrong here. The 'module verification failed' line is the kernel \
                 noting that a module built outside your distribution's kernel package was \
                 loaded, and tainting itself to record that - which is exactly what \
                 installing the patched hp-wmi does, so it is expected rather than a fault: \
                 {lines}"
            ),
        );
    }
    Check::new(ID, title(), CheckStatus::Pass, quoted)
}

/// The kernel noting that an out-of-tree module was loaded.
///
/// Guaranteed on any kernel built with module signing as soon as a module
/// the distribution did not sign is loaded, which is precisely what Pyren's
/// own driver installer produces. It is a receipt for something the user
/// asked for, not a fault, and calling it one sends people hunting for a
/// problem that is not there.
fn is_unsigned_module_notice(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("module verification failed") || lower.contains("tainting kernel")
}

/// Whether a line is the kernel reporting trouble, rather than narrating
/// something normal.
///
/// Deliberately checked *after* the unsigned-module notice is excluded:
/// that line contains the word "failed" and would otherwise match here,
/// which is how a working machine ended up with a warning.
fn is_concerning(line: &str) -> bool {
    if is_unsigned_module_notice(line) {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    [
        "fail",
        "error",
        "unknown ec layout",
        "cannot",
        "unable",
        "not supported",
        "reduced",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn check_platform_profile() -> Check {
    const PATH: &str = "/sys/firmware/acpi/platform_profile";
    const CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";

    let title = || msg!("diagnostics.checks.platform-profile.title", "ACPI platform profile");
    match (fs::read_to_string(PATH), fs::read_to_string(CHOICES)) {
        (Ok(current), Ok(choices)) => Check::new(
            "platform-profile",
            title(),
            CheckStatus::Pass,
            msg!(
                "diagnostics.checks.platform-profile.ok",
                { "current" => current.trim(), "choices" => choices.trim() },
                "{current} (available: {choices})"
            ),
        ),
        _ => Check::new(
            "platform-profile",
            title(),
            CheckStatus::Warn,
            msg!(
                "diagnostics.checks.platform-profile.absent",
                "not exposed; power modes fall back to power-profiles-daemon or the CPU EPP hint"
            ),
        ),
    }
}

fn check_cpu_temp(path: Option<&Path>) -> Check {
    check_readable_number(
        "cpu-temp",
        msg!("diagnostics.checks.cpu-temp.title", "CPU temperature"),
        path,
        |millidegrees| {
            let celsius = millidegrees / 1000;
            if (0..=125).contains(&celsius) {
                (
                    CheckStatus::Pass,
                    msg!("diagnostics.checks.cpu-temp.ok", { "celsius" => celsius }, "{celsius} °C"),
                )
            } else {
                (
                    CheckStatus::Warn,
                    msg!(
                        "diagnostics.checks.cpu-temp.implausible",
                        { "celsius" => celsius },
                        "{celsius} °C is implausible"
                    ),
                )
            }
        },
    )
}

/// Two things need this now, not one: the dust-removal fan cleaner (not
/// ported yet) and the RGB lightbar. Naming only the fan cleaner made a
/// warning look optional to anyone who does not want it.
fn check_acpi_call() -> Check {
    let title = || msg!("diagnostics.checks.acpi-call.title", "acpi_call module");
    if Path::new(&pyren_core::acpi::call_path()).exists() {
        Check::new(
            "acpi-call",
            title(),
            CheckStatus::Pass,
            msg!("diagnostics.checks.acpi-call.ok", "/proc/acpi/call is available"),
        )
    } else {
        Check::new(
            "acpi-call",
            title(),
            CheckStatus::Warn,
            msg!(
                "diagnostics.checks.acpi-call.absent",
                "/proc/acpi/call not found; the RGB lightbar and the fan cleaner both need it"
            ),
        )
        .with_remedy(msg!(
            "diagnostics.checks.acpi-call.remedy",
            "Install acpi_call-dkms (Arch), acpi-call-dkms (Debian) or akmod-acpi_call \
             (Fedora), then `modprobe acpi_call`."
        ))
    }
}

/// Whether this machine's firmware has the dust-removal fan cleaner.
///
/// Read-only, like everything else here: it puts the two capability
/// *queries* to the firmware - the same class of question the lighting
/// section's lightbar read is - and commands nothing. It never loads the
/// kernel module either, so on a machine without `acpi_call` this is a
/// skip and the check above it says what to install.
///
/// The three outcomes are deliberately not two. "Asked and told no" is a
/// fact about the hardware with no remedy; "could not ask" is a missing
/// package or a missing `sudo`, and reporting it as the first sends
/// someone looking for a different laptop.
fn check_fan_cleaner() -> Check {
    let title = || msg!("diagnostics.checks.fan-cleaner.title", "Fan cleaner (reverse spin)");
    let probe = crate::cleaner::probe();

    if let Some(why) = probe.unreachable {
        return Check::new("fan-cleaner", title(), CheckStatus::Skip, why).with_remedy(msg!(
            "diagnostics.checks.fan-cleaner.remedyAsk",
            { "path" => pyren_core::acpi::CALL_PATH },
            "The firmware is asked over {path}, which needs the acpi_call module loaded \
             and root to write. With both, run this again."
        ));
    }

    match probe.generation {
        Some(_) => Check::new("fan-cleaner", title(), CheckStatus::Pass, probe.detail),
        // A refusal, and most machines give one. Not a fault, so not a
        // failure - and nothing to suggest, so no remedy.
        None => Check::new("fan-cleaner", title(), CheckStatus::Warn, probe.detail),
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
        gpu_temp: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same distinction the lighting section makes, for the same
    /// reason: on a machine with no `acpi_call` the firmware was never
    /// asked, and reporting that as "no fan cleaner here" is claiming
    /// something nobody established - over a missing package.
    #[test]
    fn a_fan_cleaner_that_was_never_asked_about_is_not_reported_as_absent() {
        let dir = std::env::temp_dir().join(format!("pyren-diag-cleaner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _no_acpi = crate::testenv::without_acpi_call(&dir);

        let check = check_fan_cleaner();
        assert_eq!(check.id, "fan-cleaner");
        assert_eq!(check.status, CheckStatus::Skip, "not asked is not a verdict");
        assert!(check.remedy.is_some(), "not being able to ask comes with a way to ask");
        assert!(!check.detail.contains("has no fan cleaner"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two lines this machine actually produced after the patched
    /// driver was installed. Neither is a fault, and the checker used to
    /// mark both as a warning - which is what sent someone looking for a
    /// problem that was not there.
    #[test]
    fn a_healthy_hp_wmi_log_is_not_a_warning() {
        for line in [
            "[ 5849.627625] hp_wmi: module verification failed: signature and/or required key missing - tainting kernel",
            "[ 5849.820528] hp_wmi: Registered as platform profile handler",
            "[    5.585175] input: HP WMI hotkeys as /devices/virtual/input/input10",
        ] {
            assert!(!super::is_concerning(line), "should not warn about: {line}");
        }
    }

    /// "module verification failed" contains "failed": excluding it has to
    /// happen before the keyword search, or the fix does nothing.
    #[test]
    fn the_taint_notice_is_recognised_before_the_word_failed_matches() {
        let taint = "hp_wmi: module verification failed: signature and/or required key missing - tainting kernel";
        assert!(super::is_unsigned_module_notice(taint));
        assert!(!super::is_concerning(taint));
    }

    /// The lines the check exists for still have to reach the user. The
    /// first is the driver's own `pr_warn` for a board whose EC layout it
    /// does not know - which is a live possibility here, since the board
    /// was added to the table with a params variant chosen by heuristic.
    #[test]
    fn a_real_driver_complaint_is_still_a_warning() {
        for line in [
            "hp_wmi: Unknown EC layout for board 8D2F. Thermal profile readback will be disabled.",
            "hp_wmi: query 0x2b returned error 0x5",
            "hp_wmi: unable to register platform profile",
        ] {
            assert!(super::is_concerning(line), "should warn about: {line}");
        }
    }

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pyren-fan-diag-{tag}-{}", std::process::id()));
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

    /// A single-fan machine is not a broken one.
    #[test]
    fn a_missing_second_fan_is_skipped_rather_than_failed() {
        let dir = fixture("onefan");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");
        // No fan2_input: the path is discovered, the file does not exist.

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(check(&diagnosis, "fan2").status, CheckStatus::Skip);
        assert_eq!(diagnosis.verdict, Verdict::FullControl);
    }

    /// The case this whole tool exists for: an HP laptop whose hwmon node
    /// is present but whose stock driver exposes no PWM for the board.
    ///
    /// Discovery fills in the pwm1 path regardless of whether the file
    /// exists, so a verdict derived from paths rather than results claimed
    /// fan control worked here - which it does not.
    #[test]
    fn an_hwmon_node_without_a_pwm_file_is_monitoring_only() {
        let dir = fixture("nopwm");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "fan2_input", "2300\n");
        // No pwm1 or pwm1_enable written: the paths exist, the files don't.

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(diagnosis.verdict, Verdict::MonitoringOnly);
        assert_eq!(check(&diagnosis, "fan1").status, CheckStatus::Pass);
        assert_eq!(check(&diagnosis, "pwm1").status, CheckStatus::Fail);
    }

    /// Not being *allowed* to write is not the same as the driver refusing
    /// the write, so it must not downgrade the verdict - it asks for root.
    #[test]
    fn a_write_test_blocked_by_permissions_asks_for_root_instead_of_failing() {
        let dir = fixture("readonly");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let mut perms = fs::metadata(dir.join("pwm1")).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(dir.join("pwm1"), perms).unwrap();

        // Root ignores the permission bits, which would make this test
        // assert the opposite of what it means to.
        if fs::write(dir.join("pwm1"), "128").is_ok() {
            eprintln!("skipped: running with privileges that bypass file permissions");
            return;
        }

        let diagnosis = diagnose(&paths_for_testing(dir, None), true);
        let write_check = check(&diagnosis, "pwm-write");
        assert_eq!(write_check.status, CheckStatus::Skip);
        assert!(write_check.detail.contains("root"), "got: {}", write_check.detail);
        assert_eq!(diagnosis.verdict, Verdict::FullControl);
    }

    #[test]
    fn a_write_test_that_was_never_asked_for_does_not_rule_control_out() {
        let dir = fixture("untested");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(diagnosis.verdict, Verdict::FullControl);
        // ...but it must say the write path is unverified.
        assert!(diagnosis.summary.contains("Re-run"), "got: {}", diagnosis.summary);
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
        let kernel_hint =
            notice.text.find("upgrading the kernel").expect("should suggest the kernel");
        let driver_hint = notice.text.find("patched").expect("should mention the patched driver");
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

    /// The self-test is shown in the user's language: a check and the
    /// summary must carry a catalog key alongside the English text.
    #[test]
    fn checks_and_the_summary_are_translatable() {
        let dir = fixture("i18n");
        write(&dir, "fan1_input", "2400\n");
        write(&dir, "pwm1", "128\n");
        write(&dir, "pwm1_enable", "2\n");

        let diagnosis = diagnose(&paths_for_testing(dir, None), false);
        assert_eq!(diagnosis.summary.key, "diagnostics.summary.fullControlUntested");
        let pwm1 = check(&diagnosis, "pwm1");
        assert_eq!(pwm1.title.key, "diagnostics.checks.pwm1.title");
        assert_eq!(pwm1.detail.key, "diagnostics.checks.pwm1.ok");
        assert_eq!(pwm1.detail.params["value"], 128);
        assert_eq!(pwm1.detail.text, "pwm1 = 128 (0-255)");
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
