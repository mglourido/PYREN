//! What lighting this machine actually has.
//!
//! Step 1 of `docs/04-rgb-porting-review.md` §"Suggested porting order",
//! and the reason it is step 1: the source project has **two unrelated
//! hardware paths** - per-key RGB over USB HID, and a 4-zone light strip
//! over ACPI-WMI - and *which one a laptop has is not decided by the model
//! name*. They share no transport, no privilege model and no detection, so
//! the only honest answer comes from looking.
//!
//! This is the same rule the `system` module follows (`docs/01-ipc-protocol.md`
//! §"`controls` and `compatibility` are measured, not looked up"). There is
//! no board list here and there will not be one.

use serde::Serialize;

use crate::lightbar;

/// The HP Gaming Keyboard II's lighting interface, as `lsusb` prints it.
const PER_KEY_VENDOR: &str = "0d62";
const PER_KEY_PRODUCT: &str = "54bf";

const USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Points the USB scan at a fixture directory, the way `PYREN_HWMON_DIR`
/// does for the fan checks. Without it the per-key probe can only ever be
/// tested on a machine that has the keyboard, which is no machine here.
fn usb_devices_root() -> String {
    std::env::var("PYREN_USB_DEVICES").unwrap_or_else(|_| USB_DEVICES.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub per_key: PerKey,
    pub lightbar: Lightbar,
    /// Whether *anything* here can be driven. What `core.capabilities`
    /// reports for this module, and what a UI should hide the page on.
    pub supported: bool,
}

/// Per-key RGB over USB HID. Detected, deliberately not driven - see
/// [`PerKey::ported`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerKey {
    pub present: bool,
    /// The USB id looked for, so a bug report says what was searched for
    /// rather than only that nothing was found.
    pub usb_id: &'static str,
    /// Always false in this build.
    ///
    /// Step 3 of the porting order: the per-key path is worth writing only
    /// once a `0d62:54bf` turns up, and the review's first finding - a
    /// disagreement between `set_all()` and `data/keys.json` about two
    /// bytes under the backspace key - has to be settled on that hardware
    /// before the key map is ported with a known inconsistency in it.
    pub ported: bool,
    pub detail: String,
}

/// The 4-zone light strip over `/proc/acpi/call`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lightbar {
    /// The firmware answered a read. The only one of these fields that
    /// means the hardware is there.
    pub present: bool,
    pub hp_wmi: bool,
    /// `/proc/acpi/call` exists right now.
    pub acpi_call: bool,
    /// The module is installed but not loaded, which is a different
    /// problem with a different fix.
    pub acpi_call_installed: bool,
    /// Whether the firmware was asked at all. `None` means the question
    /// could not be put - and that is not the same as the firmware saying
    /// no.
    pub answered: Option<bool>,
    /// Set when the interface is there and the question *still* could not
    /// be put; almost always "this is not root". Carries why.
    ///
    /// Its own field because the alternative is reporting it as a refusal,
    /// which reads as a permanent verdict on the hardware when the fix is
    /// `sudo`.
    pub unreachable: Option<String>,
    pub detail: String,
}

pub fn probe() -> Probe {
    let per_key = probe_per_key();
    let lightbar = probe_lightbar();
    Probe { supported: lightbar.present, per_key, lightbar }
}

fn probe_per_key() -> PerKey {
    let present = usb_device_present(PER_KEY_VENDOR, PER_KEY_PRODUCT);
    PerKey {
        present,
        usb_id: "0d62:54bf",
        ported: false,
        detail: if present {
            "an HP Gaming Keyboard II is attached, but this build does not drive it \
             (see docs/04-rgb-porting-review.md, finding 1)"
                .into()
        } else {
            "no HP Gaming Keyboard II on this machine".into()
        },
    }
}

fn probe_lightbar() -> Lightbar {
    let hp_wmi = lightbar::hp_wmi_present();
    let acpi_call = pyren_core::acpi::is_loaded();
    let acpi_call_installed = acpi_call || pyren_core::acpi::is_module_installed();

    // The question is only put when it can be: asking needs both the WMI
    // interface and the file to write it through.
    let answer = (hp_wmi && acpi_call).then(lightbar::ask);

    let (answered, unreachable) = match &answer {
        Some(lightbar::Answer::Pass) => (Some(true), None),
        Some(lightbar::Answer::Refused) => (Some(false), None),
        // The call itself failed, so nobody was asked anything.
        Some(lightbar::Answer::Unreachable(why)) => (None, Some(why.clone())),
        None => (None, None),
    };

    let detail = match (&answered, &unreachable) {
        (Some(true), _) => "the firmware answered a lightbar read".to_string(),
        (Some(false), _) => "the firmware was asked and refused, so this machine has no \
             light strip (or none this protocol reaches)"
            .to_string(),
        (None, Some(why)) => {
            format!("/proc/acpi/call is here but could not be used, so the firmware was not asked ({why})")
        }
        (None, None) if !hp_wmi => {
            "no hp-wmi interface on this machine, so there is nothing to ask".to_string()
        }
        (None, None) if acpi_call_installed => {
            "hp-wmi is here and acpi_call is installed but not loaded; \
             'sudo modprobe acpi_call' would answer this"
                .to_string()
        }
        (None, None) => format!(
            "hp-wmi is here but /proc/acpi/call is not, and the module is not installed either; \
             {}",
            pyren_core::acpi::INSTALL_HINT
        ),
    };

    Lightbar {
        present: answered == Some(true),
        hp_wmi,
        acpi_call,
        acpi_call_installed,
        answered,
        unreachable,
        detail,
    }
}

/// Walks `/sys/bus/usb/devices` rather than shelling out to `lsusb`, which
/// is not installed everywhere and would be a second thing to parse.
fn usb_device_present(vendor: &str, product: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(usb_devices_root()) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|entry| {
        let dir = entry.path();
        let id = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        id("idVendor") == vendor && id("idProduct") == product
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probing must not need the hardware, must not load anything, and
    /// must not panic on a machine that has none of it - which is every
    /// machine this is developed on.
    #[test]
    fn a_probe_on_a_machine_with_no_omen_hardware_says_so_calmly() {
        let probe = probe();
        assert!(!probe.per_key.ported, "the per-key path is not ported in this build");
        assert!(!probe.per_key.detail.is_empty());
        assert!(!probe.lightbar.detail.is_empty());
        // `present` is a claim about hardware; it may only be true if the
        // firmware was actually asked and said yes.
        assert_eq!(probe.lightbar.present, probe.lightbar.answered == Some(true));
        assert_eq!(probe.supported, probe.lightbar.present);
    }

    /// The distinction the `detail` text exists for: "not installed",
    /// "installed but not loaded" and "asked and refused" have three
    /// different fixes, and one message for all three sends people to the
    /// wrong one.
    #[test]
    fn a_machine_that_was_never_asked_does_not_report_a_refusal() {
        let probe = probe();
        if probe.lightbar.answered.is_none() {
            assert!(
                !probe.lightbar.detail.contains("refused"),
                "not being able to ask is not a refusal: {}",
                probe.lightbar.detail
            );
        }
    }
}
