//! Who this machine is: vendor, model, board, firmware, CPU, GPUs, and
//! whether the OMEN hardware features can be expected to work here.
//!
//! Everything is read once at startup - none of it changes while the
//! daemon runs, and re-reading DMI on every poll would be wasted work.

use std::fs;
use std::process::Command;

use serde::Serialize;

use crate::boards::is_supported_board;

const DMI: &str = "/sys/class/dmi/id";

/// How much the OMEN-specific features can be trusted on this machine.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Compatibility {
    /// HP board present in the known-good list.
    Supported,
    /// An HP OMEN/Victus machine whose board isn't in the list. Fan control
    /// may well work; the UI should warn rather than block.
    Untested,
    /// Not an HP gaming machine - monitoring works, hardware control won't.
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
    /// Convenience flag for the UI: true unless `Unsupported`.
    pub supported: bool,
    /// Short human-readable justification, shown next to the flag.
    pub reason: String,
}

impl SystemIdentity {
    pub fn detect() -> Self {
        let vendor = dmi("sys_vendor");
        let board_name = dmi("board_name");
        let model = product_name(&board_name);
        let form_factor = form_factor(dmi("chassis_type").as_deref());

        let (compatibility, reason) = classify(&vendor, &model, &board_name);

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

fn classify(
    vendor: &Option<String>,
    model: &Option<String>,
    board_name: &Option<String>,
) -> (Compatibility, String) {
    let is_hp = vendor
        .as_deref()
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.starts_with("hp") || v.contains("hewlett")
        })
        .unwrap_or(false);

    if !is_hp {
        return (
            Compatibility::Unsupported,
            format!(
                "{} is not an HP machine; monitoring works, OMEN hardware control does not",
                vendor.as_deref().unwrap_or("this machine")
            ),
        );
    }

    if let Some(board) = board_name {
        if is_supported_board(board) {
            return (
                Compatibility::Supported,
                format!("board {board} is on the known-good list"),
            );
        }
    }

    let looks_like_omen = model
        .as_deref()
        .map(|m| {
            let m = m.to_ascii_lowercase();
            m.contains("omen") || m.contains("victus")
        })
        .unwrap_or(false);

    if looks_like_omen {
        (
            Compatibility::Untested,
            format!(
                "HP OMEN/Victus machine, but board {} is untested - fan control may or may not work",
                board_name.as_deref().unwrap_or("?")
            ),
        )
    } else {
        (
            Compatibility::Unsupported,
            "HP machine, but not an OMEN or Victus model".to_string(),
        )
    }
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

    #[test]
    fn non_hp_hardware_is_unsupported() {
        let (compat, _) = classify(
            &Some("ASUS".into()),
            &Some("PRIME B660M-K D4".into()),
            &Some("PRIME B660M-K D4".into()),
        );
        assert_eq!(compat, Compatibility::Unsupported);
    }

    #[test]
    fn known_hp_board_is_supported() {
        let (compat, _) = classify(
            &Some("HP".into()),
            &Some("OMEN by HP Laptop 16".into()),
            &Some("8D41".into()),
        );
        assert_eq!(compat, Compatibility::Supported);
    }

    #[test]
    fn unknown_omen_board_is_untested_not_rejected() {
        let (compat, _) = classify(
            &Some("HP".into()),
            &Some("OMEN by HP Laptop 17".into()),
            &Some("FFFF".into()),
        );
        assert_eq!(compat, Compatibility::Untested);
    }

    #[test]
    fn lspci_quoted_fields_survive_splitting() {
        let line = r#"01:00.0 "VGA compatible controller" "NVIDIA Corporation" "GA106 [GeForce RTX 3060]" -r a1"#;
        let fields = split_lspci_fields(line);
        assert_eq!(fields[1], "VGA compatible controller");
        assert_eq!(fields[3], "GA106 [GeForce RTX 3060]");
    }
}
