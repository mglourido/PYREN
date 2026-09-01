//! Applying a power mode to the machine.
//!
//! Three mechanisms, tried in order of how directly they map to what the
//! OMEN Gaming Hub does:
//!
//! 1. **ACPI platform profile** (`/sys/firmware/acpi/platform_profile`) -
//!    the same firmware-level switch the Fn+P hotkey drives on HP laptops.
//!    This is the real thing when it exists.
//! 2. **power-profiles-daemon**, the desktop-standard service most distros
//!    ship. Used when the firmware doesn't expose a profile of its own.
//! 3. **Energy performance preference** (intel_pstate/amd_pstate EPP), a
//!    per-CPU hint applied alongside either of the above.
//!
//! Every mechanism is best-effort and reports back what actually happened,
//! so the UI can say "applied via platform_profile" rather than claiming
//! success it can't verify. Writes need root; running the daemon
//! unprivileged surfaces a permission error instead of failing silently.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::PowerMode;

const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";
const PLATFORM_PROFILE_CHOICES: &str = "/sys/firmware/acpi/platform_profile_choices";
const CPU_ROOT: &str = "/sys/devices/system/cpu";

/// What the machine offers and what it is currently set to.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendState {
    pub platform_profile: Option<String>,
    pub platform_profile_choices: Vec<String>,
    pub power_profiles_daemon: Option<String>,
    pub energy_preference: Option<String>,
    pub governor: Option<String>,
    /// Mechanisms that could be used here, best first.
    pub available: Vec<&'static str>,
}

/// Outcome of one `setMode`, listing what was actually changed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub failed: Vec<String>,
}

impl ApplyReport {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

pub fn read_state() -> BackendState {
    let platform_profile = read_trimmed(PLATFORM_PROFILE);
    let platform_profile_choices = read_trimmed(PLATFORM_PROFILE_CHOICES)
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    let mut available = Vec::new();
    if platform_profile.is_some() {
        available.push("platform_profile");
    }
    let ppd = read_power_profiles_daemon();
    if ppd.is_some() {
        available.push("power-profiles-daemon");
    }
    let energy_preference =
        read_trimmed(format!("{CPU_ROOT}/cpu0/cpufreq/energy_performance_preference"));
    if energy_preference.is_some() {
        available.push("energy_performance_preference");
    }

    BackendState {
        platform_profile,
        platform_profile_choices,
        power_profiles_daemon: ppd,
        energy_preference,
        governor: read_trimmed(format!("{CPU_ROOT}/cpu0/cpufreq/scaling_governor")),
        available,
    }
}

pub fn apply(mode: PowerMode) -> ApplyReport {
    let state = read_state();
    let mut report = ApplyReport { applied: Vec::new(), failed: Vec::new() };

    if !state.platform_profile_choices.is_empty() {
        match pick_platform_profile(mode, &state.platform_profile_choices) {
            Some(profile) => match fs::write(PLATFORM_PROFILE, &profile) {
                Ok(()) => report.applied.push(format!("platform_profile={profile}")),
                Err(e) => report.failed.push(format!("platform_profile: {e}")),
            },
            None => report
                .failed
                .push("platform_profile: no choice matches this mode".to_string()),
        }
    } else if state.power_profiles_daemon.is_some() {
        let profile = power_profiles_daemon_name(mode);
        match set_power_profiles_daemon(profile) {
            Ok(()) => report.applied.push(format!("power-profiles-daemon={profile}")),
            Err(e) => report.failed.push(format!("power-profiles-daemon: {e}")),
        }
    }

    if state.energy_preference.is_some() {
        let preference = energy_preference_name(mode);
        match write_all_cpus("energy_performance_preference", preference) {
            Ok(count) => report
                .applied
                .push(format!("energy_performance_preference={preference} ({count} cpus)")),
            Err(e) => report.failed.push(format!("energy_performance_preference: {e}")),
        }
    }

    report
}

/// Maps a mode onto whichever profile names this firmware actually offers.
///
/// The ACPI ABI defines a fixed vocabulary but firmware exposes only a
/// subset (HP laptops typically `low-power`/`balanced`/`performance`), so
/// each mode has an ordered list of acceptable names.
fn pick_platform_profile(mode: PowerMode, choices: &[String]) -> Option<String> {
    let preferences: &[&str] = match mode {
        PowerMode::Eco => &["low-power", "quiet", "cool", "balanced"],
        PowerMode::Balanced => &["balanced", "balanced-performance", "quiet"],
        PowerMode::Performance => &["balanced-performance", "performance", "balanced"],
        // There is no firmware profile beyond "performance"; what makes
        // Unlimited different is the manual fan and power limits the fan
        // module applies on top, not a different platform profile.
        PowerMode::Unlimited => &["performance", "balanced-performance"],
    };
    preferences
        .iter()
        .find(|wanted| choices.iter().any(|c| c == *wanted))
        .map(|wanted| wanted.to_string())
}

fn power_profiles_daemon_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "power-saver",
        PowerMode::Balanced => "balanced",
        PowerMode::Performance | PowerMode::Unlimited => "performance",
    }
}

fn energy_preference_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "power",
        PowerMode::Balanced => "balance_performance",
        PowerMode::Performance | PowerMode::Unlimited => "performance",
    }
}

fn read_power_profiles_daemon() -> Option<String> {
    let output = Command::new("powerprofilesctl").arg("get").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn set_power_profiles_daemon(profile: &str) -> Result<(), String> {
    let output = Command::new("powerprofilesctl")
        .args(["set", profile])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

/// Writes one cpufreq attribute on every CPU, returning how many took it.
///
/// Partial success is normal on hybrid CPUs where some cores are offline,
/// so only a total failure is reported as an error.
fn write_all_cpus(attribute: &str, value: &str) -> Result<usize, String> {
    let Ok(entries) = fs::read_dir(CPU_ROOT) else {
        return Err(format!("{CPU_ROOT} is unreadable"));
    };

    let mut written = 0;
    let mut last_error = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = entry.path().join("cpufreq").join(attribute);
        if !path.exists() {
            continue;
        }
        match fs::write(&path, value) {
            Ok(()) => written += 1,
            Err(e) => last_error = Some(e.to_string()),
        }
    }

    match (written, last_error) {
        (0, Some(e)) => Err(e),
        (0, None) => Err("no cpu exposes this attribute".to_string()),
        (count, _) => Ok(count),
    }
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn eco_prefers_low_power_when_offered() {
        let available = choices(&["low-power", "balanced", "performance"]);
        assert_eq!(pick_platform_profile(PowerMode::Eco, &available).unwrap(), "low-power");
    }

    #[test]
    fn eco_falls_back_to_quiet_on_firmware_that_calls_it_that() {
        let available = choices(&["quiet", "balanced", "performance"]);
        assert_eq!(pick_platform_profile(PowerMode::Eco, &available).unwrap(), "quiet");
    }

    #[test]
    fn unlimited_maps_onto_performance() {
        let available = choices(&["low-power", "balanced", "performance"]);
        assert_eq!(
            pick_platform_profile(PowerMode::Unlimited, &available).unwrap(),
            "performance"
        );
    }

    #[test]
    fn a_firmware_offering_nothing_usable_yields_none() {
        assert_eq!(pick_platform_profile(PowerMode::Unlimited, &choices(&["custom"])), None);
    }
}
