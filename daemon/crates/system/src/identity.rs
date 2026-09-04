//! Who this machine is: vendor, model, board, firmware, CPU, GPUs, and
//! whether the OMEN hardware features can be expected to work here.
//!
//! Everything is read once at startup - none of it changes while the
//! daemon runs, and re-reading DMI on every poll would be wasted work.

use std::fs;
use std::process::Command;

use pyren_core::{msg, Msg};
use serde::Serialize;

const DMI: &str = "/sys/class/dmi/id";
const HP_WMI: &str = "/sys/devices/platform/hp-wmi";

/// What this machine was found able to *do*.
///
/// Assembled by the daemon from the modules that own each hardware surface,
/// rather than probed here: `system` re-implementing their checks would be
/// a second copy of the same question, and a second copy is one that
/// drifts. See `daemon/daemon/src/main.rs`.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Controls {
    /// Fan mode switching (auto/max) was accepted.
    pub fan_mode: bool,
    /// A specific fan speed can be commanded.
    pub fan_speed: bool,
    /// Some power-mode mechanism answered (platform profile, PPD, EPP).
    pub power_mode: bool,
    /// The 4-zone lightbar answered an ACPI read. Named for the thing that
    /// was probed rather than "lighting": the per-key keyboard is a
    /// different device on a different bus, and a machine can have either,
    /// both or neither.
    pub lightbar: bool,
    /// `/sys/devices/platform/hp-wmi/gpu_mux_mode` answered a read - the
    /// patched driver's own GPU MUX switch, not `supergfxctl`.
    pub gpu_mux: bool,
}

impl Controls {
    fn any(&self) -> bool {
        self.fan_mode || self.fan_speed || self.power_mode || self.lightbar || self.gpu_mux
    }
}

/// How much of this machine can actually be driven.
///
/// **This is an observation, not a lookup.** An earlier version answered it
/// from a hand-copied list of DMI board ids, which was wrong in both
/// directions: it called board 8D2F "supported" on a machine that cannot
/// set a fan speed, and it would have called an unlisted board that works
/// perfectly "untested". It also had to be extended by hand, one board at a
/// time, for a driver this project does not install and cannot vouch for.
/// So the question is now put to the hardware.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Compatibility {
    /// Something here accepts control. `reason` says what.
    Controllable,
    /// The interfaces are present to read but nothing accepted control -
    /// e.g. fan speeds are readable and `pwm1` does not exist.
    MonitoringOnly,
    /// Nothing beyond what any Linux machine offers.
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemIdentity {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub board_name: Option<String>,
    pub board_vendor: Option<String>,
    pub bios_version: Option<String>,
    pub bios_date: Option<String>,
    pub kernel: Option<String>,
    pub cpu: Option<String>,
    pub cpu_cores: usize,
    pub gpus: Vec<String>,
    pub form_factor: &'static str,
    pub compatibility: Compatibility,
    /// What was found to work, itemised. The UI should gate on these rather
    /// than on `compatibility`, which is only their summary.
    pub controls: Controls,
    /// Convenience flag for the UI: true unless `Unsupported`.
    pub supported: bool,
    /// Short human-readable justification, shown next to the flag.
    /// Why that verdict, in one sentence. Translatable - render with `tm()`.
    pub reason: Msg,
}

impl SystemIdentity {
    /// `controls` is what the hardware modules found they could do. Pass
    /// [`Controls::default`] when that is not known yet - the verdict then
    /// reflects only what is visible, which is the honest answer.
    pub fn detect(controls: Controls) -> Self {
        let vendor = dmi("sys_vendor");
        let board_name = dmi("board_name");
        let model = product_name(&board_name);
        let form_factor = form_factor(dmi("chassis_type").as_deref());

        let (compatibility, reason) = classify(controls, hp_wmi_present());

        Self {
            board_vendor: dmi("board_vendor"),
            bios_version: dmi("bios_version"),
            bios_date: dmi("bios_date"),
            kernel: kernel_release(),
            cpu: cpu_model(),
            cpu_cores: cpu_count(),
            gpus: detect_gpus(),
            form_factor,
            supported: compatibility != Compatibility::Unsupported,
            compatibility,
            controls,
            reason,
            vendor,
            model,
            board_name,
        }
    }

    /// One-line summary for the daemon's startup log.
    pub fn summary(&self) -> String {
        format!(
            "{} {} (board {}, {}) - {:?}: {}",
            self.vendor.as_deref().unwrap_or("unknown vendor"),
            self.model.as_deref().unwrap_or("unknown model"),
            self.board_name.as_deref().unwrap_or("?"),
            self.form_factor,
            self.compatibility,
            self.reason,
        )
    }
}

/// Reads one DMI attribute, treating firmware placeholder strings as absent.
///
/// Board vendors ship these defaults unset on retail boards ("System
/// Product Name", "To be filled by O.E.M."), and showing those verbatim in
/// the UI would be worse than showing nothing.
fn dmi(name: &str) -> Option<String> {
    let value = fs::read_to_string(format!("{DMI}/{name}")).ok()?;
    let value = value.trim();
    if value.is_empty() || is_placeholder(value) {
        return None;
    }
    Some(value.to_string())
}

fn is_placeholder(value: &str) -> bool {
    const PLACEHOLDERS: &[&str] = &[
        "to be filled by o.e.m.",
        "system product name",
        "system manufacturer",
        "default string",
        "not specified",
        "not applicable",
        "none",
        "unknown",
        "o.e.m.",
        "oem",
    ];
    let lower = value.to_ascii_lowercase();
    PLACEHOLDERS.contains(&lower.as_str())
}

/// Product name, falling back to the board when the firmware left it unset -
/// common on desktops, where the board *is* the useful identifier.
fn product_name(board_name: &Option<String>) -> Option<String> {
    dmi("product_name")
        .or_else(|| dmi("product_family"))
        .or_else(|| board_name.clone())
}

/// SMBIOS chassis type (DMI spec, table 17). Only the distinction between
/// portable and everything else matters here.
fn form_factor(chassis_type: Option<&str>) -> &'static str {
    match chassis_type.and_then(|v| v.trim().parse::<u32>().ok()) {
        Some(8..=11 | 14 | 30 | 31 | 32) => "laptop",
        Some(3..=7 | 13 | 15..=17 | 23 | 24) => "desktop",
        _ => "unknown",
    }
}

/// Whether the HP WMI platform interface exists at all. A directory test,
/// not a capability test - it separates "this machine has none of this
/// hardware" from "it has it and nothing is writable".
fn hp_wmi_present() -> bool {
    std::path::Path::new(HP_WMI).is_dir()
}

fn classify(controls: Controls, hp_wmi: bool) -> (Compatibility, Msg) {
    if !controls.any() {
        return if hp_wmi {
            (
                Compatibility::MonitoringOnly,
                msg!(
                    "system.reason.monitoringOnly",
                    "the hp-wmi interface is present but nothing here accepted control; \
                     fan speeds and temperatures can still be read"
                ),
            )
        } else {
            (
                Compatibility::Unsupported,
                msg!(
                    "system.reason.unsupported",
                    "no hp-wmi interface and no power-mode mechanism; monitoring works, \
                     hardware control does not"
                ),
            )
        };
    }

    let mut works: Vec<Msg> = Vec::new();
    if controls.fan_speed {
        works.push(msg!("system.can.fanSpeed", "fan speed"));
    } else if controls.fan_mode {
        // Worth spelling out: it is the common case on a board the driver
        // has no entry for, and "fans" alone would overpromise.
        works.push(msg!("system.can.fanMode", "fan mode (auto/max only)"));
    }
    if controls.power_mode {
        works.push(msg!("system.can.powerModes", "power modes"));
    }
    if controls.lightbar {
        works.push(msg!("system.can.lightbar", "lightbar colour"));
    }
    if controls.gpu_mux {
        works.push(msg!("system.can.gpuMux", "GPU switching"));
    }

    let list = Msg::join(works, ", ").unwrap_or_else(|| Msg::literal(""));
    (
        Compatibility::Controllable,
        msg!(
            "system.reason.accepts",
            { "list" => list.text },
            "this machine accepts: {list}"
        ),
    )
}

fn kernel_release() -> Option<String> {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
    Some(release.trim().to_string())
}

fn cpu_model() -> Option<String> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in cpuinfo.lines() {
        // x86 uses "model name"; arm64 has no equivalent, hence the fallback.
        if let Some(value) = line
            .strip_prefix("model name")
            .or_else(|| line.strip_prefix("Model"))
        {
            if let Some((_, name)) = value.split_once(':') {
                return Some(name.trim().to_string());
            }
        }
    }
    None
}

fn cpu_count() -> usize {
    fs::read_to_string("/proc/cpuinfo")
        .map(|info| info.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0)
}

/// GPU names, preferring `lspci` (it resolves PCI IDs to marketing names via
/// the system's pci.ids) and falling back to the DRM driver name so the UI
/// still has something to show when pciutils isn't installed.
fn detect_gpus() -> Vec<String> {
    if let Some(names) = gpus_from_lspci() {
        if !names.is_empty() {
            return names;
        }
    }
    gpus_from_drm()
}

fn gpus_from_lspci() -> Option<Vec<String>> {
    let output = Command::new("lspci").arg("-mm").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut names = Vec::new();
    for line in text.lines() {
        let fields = split_lspci_fields(line);
        // -mm output: slot, class, vendor, device, [rev/subsystem...]
        let Some(class) = fields.get(1) else { continue };
        let class = class.to_ascii_lowercase();
        if !(class.contains("vga") || class.contains("3d") || class.contains("display")) {
            continue;
        }
        match (fields.get(2), fields.get(3)) {
            (Some(vendor), Some(device)) => names.push(format!("{vendor} {device}")),
            _ => continue,
        }
    }
    Some(names)
}

/// `lspci -mm` quotes any field containing spaces; a plain split would cut
/// device names in half.
fn split_lspci_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

fn gpus_from_drm() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };

    let mut names = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Only whole cards ("card0"), not their connectors ("card0-HDMI-A-1").
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let uevent = entry.path().join("device/uevent");
        let Ok(text) = fs::read_to_string(uevent) else {
            continue;
        };
        let driver = text
            .lines()
            .find_map(|l| l.strip_prefix("DRIVER="))
            .unwrap_or("unknown");
        let pci_id = text
            .lines()
            .find_map(|l| l.strip_prefix("PCI_ID="))
            .unwrap_or("");
        names.push(format!("{driver} [{pci_id}]"));
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_treated_as_missing() {
        assert!(is_placeholder("To be filled by O.E.M."));
        assert!(is_placeholder("System Product Name"));
        assert!(!is_placeholder("OMEN by HP Laptop 16"));
    }

    #[test]
    fn chassis_types_map_to_form_factors() {
        assert_eq!(form_factor(Some("10")), "laptop");
        assert_eq!(form_factor(Some("3")), "desktop");
        assert_eq!(form_factor(Some("nonsense")), "unknown");
        assert_eq!(form_factor(None), "unknown");
    }

    /// A machine with none of the interfaces, e.g. a desktop.
    #[test]
    fn nothing_controllable_and_no_hp_wmi_is_unsupported() {
        let (compat, reason) = classify(Controls::default(), false);
        assert_eq!(compat, Compatibility::Unsupported);
        assert!(reason.contains("monitoring works"));
        // ...and it is shown in the user's language: a catalog key beside
        // the English text.
        assert_eq!(reason.key, "system.reason.unsupported");
    }

    /// Board 8D2F: the interface is there, and none of it is writable.
    /// The old board-list version called this machine "supported".
    #[test]
    fn an_interface_that_accepts_nothing_is_monitoring_only() {
        let (compat, _) = classify(Controls::default(), true);
        assert_eq!(compat, Compatibility::MonitoringOnly);
    }

    /// What 8D2F actually is once the fan module has reported in.
    #[test]
    fn fan_mode_without_fan_speed_says_so_rather_than_promising_fans() {
        let controls = Controls { fan_mode: true, fan_speed: false, ..Controls::default() };
        let (compat, reason) = classify(controls, true);

        assert_eq!(compat, Compatibility::Controllable);
        assert!(reason.contains("auto/max only"), "got: {reason}");
    }

    #[test]
    fn a_controllable_machine_lists_what_works() {
        let controls =
            Controls { fan_mode: true, fan_speed: true, power_mode: true, lightbar: false, gpu_mux: false };
        let (compat, reason) = classify(controls, true);

        assert_eq!(compat, Compatibility::Controllable);
        assert!(reason.contains("fan speed") && reason.contains("power modes"), "got: {reason}");
    }

    /// Power modes are not HP-specific, and a machine where only they work
    /// is still a machine this app can drive. The verdict follows what was
    /// observed, not what the vendor string says.
    #[test]
    fn power_modes_alone_are_enough_to_be_controllable() {
        let controls = Controls { power_mode: true, ..Controls::default() };
        assert_eq!(classify(controls, false).0, Compatibility::Controllable);
    }

    /// A machine whose only controllable thing is its light strip is still
    /// a machine this project can drive, and saying "monitoring only" about
    /// it would be the board-list mistake in a new costume.
    #[test]
    fn a_lightbar_alone_is_enough_to_be_controllable() {
        let controls = Controls { lightbar: true, ..Controls::default() };
        let (compat, reason) = classify(controls, true);

        assert_eq!(compat, Compatibility::Controllable);
        assert!(reason.contains("lightbar"), "got: {reason}");
    }

    #[test]
    fn lspci_quoted_fields_survive_splitting() {
        let line = r#"01:00.0 "VGA compatible controller" "NVIDIA Corporation" "GA106 [GeForce RTX 3060]" -r a1"#;
        let fields = split_lspci_fields(line);
        assert_eq!(fields[1], "VGA compatible controller");
        assert_eq!(fields[3], "GA106 [GeForce RTX 3060]");
    }
}
