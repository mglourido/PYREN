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

use crate::dialect::{self, Dialect, DialectProbe};
use crate::{fourzone, lightbar};

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
    pub lighting: Lighting,
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

/// The lighting the firmware can be driven with, and how each of the
/// dialects answered.
///
/// This used to be one dialect's verdict called `lightbar`. It is now the
/// whole question: *which* of the three ways of talking to these lights,
/// if any, this machine answers - because there is no single OMEN lighting
/// protocol and the model name does not say which one a laptop has.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lighting {
    /// At least one dialect answered a read. The only field that means the
    /// lights can be driven.
    pub present: bool,
    pub hp_wmi: bool,
    /// `/proc/acpi/call` exists right now.
    pub acpi_call: bool,
    /// The module is installed but not loaded, which is a different
    /// problem with a different fix.
    pub acpi_call_installed: bool,
    /// One entry per dialect, in the order they are tried.
    pub dialects: Vec<DialectProbe>,
    /// Whether the firmware's lighting command (`0x20009`) answered a
    /// plain read at all, independent of any dialect.
    ///
    /// Its own field because it separates the two failures that look the
    /// same from outside: `Some(false)` is "this firmware has no lighting
    /// command", while `Some(true)` with no available dialect is "it has
    /// one and none of the three operations this project knows is the one
    /// it wants" - which is a machine worth reporting, not a machine
    /// without lights.
    pub command_answers: Option<bool>,
    /// Set when the interfaces are there and nothing could be asked
    /// anyway; almost always "this is not root". Carries why.
    pub unreachable: Option<String>,
    pub detail: String,
}

pub fn probe() -> Probe {
    let per_key = probe_per_key();
    let lighting = probe_lighting();
    Probe { supported: lighting.present, per_key, lighting }
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

/// The three interface facts, which cost three `stat`s and no ACPI call.
///
/// Split out because they are what *changes* while the daemon runs -
/// somebody installs `acpi_call`, somebody `modprobe`s it - while asking
/// the firmware costs a round trip on the file the fan cleaner shares. A
/// caller that wants to know whether a re-probe is worth its cost compares
/// these; see [`RgbModule::current_probe`](crate::RgbModule).
pub fn interfaces() -> Interfaces {
    let acpi_call = pyren_core::acpi::is_loaded();
    Interfaces {
        hp_wmi: lightbar::hp_wmi_present(),
        acpi_call,
        acpi_call_installed: acpi_call || pyren_core::acpi::is_module_installed(),
    }
}

/// What [`interfaces`] answers. `PartialEq` is the whole point of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interfaces {
    pub hp_wmi: bool,
    pub acpi_call: bool,
    pub acpi_call_installed: bool,
}

impl Lighting {
    /// The interface facts this probe was taken with.
    pub fn interfaces(&self) -> Interfaces {
        Interfaces {
            hp_wmi: self.hp_wmi,
            acpi_call: self.acpi_call,
            acpi_call_installed: self.acpi_call_installed,
        }
    }
}

fn probe_lighting() -> Lighting {
    let Interfaces { hp_wmi, acpi_call, acpi_call_installed } = interfaces();

    // Every dialect, always - including the ones that were skipped. A
    // dialect missing from the list would be indistinguishable from a
    // dialect that failed, and the whole point of the list is that a
    // person can see which of them was even asked.
    let dialects: Vec<DialectProbe> = dialect::ORDER.into_iter().map(Dialect::probe).collect();
    let present = dialects.iter().any(|d| d.available);

    // Asked only when it can be, and only once: it is one ACPI round trip
    // and it answers a question none of the dialect probes do.
    let command_answers = (hp_wmi && acpi_call).then(|| fourzone::platform_info().is_ok());

    // "Nothing could be asked" is not "the firmware said no". A dialect
    // whose interfaces are here and which still could not put the question
    // - an unprivileged daemon, almost always - is the case this field
    // exists to keep out of the refusal count.
    let unreachable = dialects
        .iter()
        .find(|d| !d.asked && !d.available && d.detail.contains("root"))
        .map(|d| d.detail.clone());

    let detail = if present {
        let names: Vec<&str> =
            dialects.iter().filter(|d| d.available).map(|d| d.id).collect();
        format!("the lights answered on: {}", names.join(", "))
    } else if let Some(why) = &unreachable {
        format!("the interfaces are here but nothing could be asked ({why})")
    } else if !hp_wmi && !kernel_zones_present() {
        "no hp-wmi interface and no kernel rgb_zones files, so there is nothing to ask"
            .to_string()
    } else if hp_wmi && !acpi_call {
        if acpi_call_installed {
            "hp-wmi is here and acpi_call is installed but not loaded; \
             'sudo modprobe acpi_call' would answer this"
                .to_string()
        } else {
            format!(
                "hp-wmi is here but /proc/acpi/call is not, and the module is not installed \
                 either; {}",
                pyren_core::acpi::INSTALL_HINT
            )
        }
    } else if command_answers == Some(true) {
        "the firmware has a lighting command but refused every dialect this build knows; \
         forcing one by hand is the next thing to try"
            .to_string()
    } else {
        "the firmware was asked in every dialect this build knows and refused each one, \
         so this machine has no lighting these protocols reach"
            .to_string()
    };

    Lighting {
        present,
        hp_wmi,
        acpi_call,
        acpi_call_installed,
        dialects,
        command_answers,
        unreachable,
        detail,
    }
}

fn kernel_zones_present() -> bool {
    crate::kernel_zones::present()
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
    /// machine this was developed on.
    #[test]
    fn a_probe_on_a_machine_with_no_omen_hardware_says_so_calmly() {
        // Asks the real machine, so nothing may redirect it meanwhile.
        let _acpi = crate::testenv::real();
        let probe = probe();
        assert!(!probe.per_key.ported, "the per-key path is not ported in this build");
        assert!(!probe.per_key.detail.is_empty());
        assert!(!probe.lighting.detail.is_empty());
        // `present` is a claim about hardware: it may only be true when a
        // dialect actually answered a read.
        assert_eq!(probe.lighting.present, probe.lighting.dialects.iter().any(|d| d.available));
        assert_eq!(probe.supported, probe.lighting.present);
    }

    /// Every dialect is reported whether or not it was asked. A list that
    /// silently omitted the skipped ones would make "not asked" and
    /// "refused" indistinguishable, which is the distinction this module
    /// exists to keep.
    #[test]
    fn every_dialect_appears_in_the_probe_in_the_order_it_is_tried() {
        // Asks the real machine, so nothing may redirect it meanwhile.
        let _acpi = crate::testenv::real();
        let probe = probe();
        let ids: Vec<&str> = probe.lighting.dialects.iter().map(|d| d.id).collect();
        let expected: Vec<&str> = dialect::ORDER.iter().map(|d| d.id()).collect();
        assert_eq!(ids, expected);
        for d in &probe.lighting.dialects {
            assert!(!d.detail.is_empty(), "{} says nothing about itself", d.id);
            assert!(!d.available || d.asked, "a dialect nobody asked cannot be available");
        }
    }

    /// The cheap half must agree with the full probe, or a re-probe would
    /// be triggered on every call - or, worse, never.
    #[test]
    fn the_cheap_interface_check_matches_what_a_full_probe_recorded() {
        // Asks the real machine, so nothing may redirect it meanwhile.
        let _acpi = crate::testenv::real();
        assert_eq!(probe().lighting.interfaces(), interfaces());
    }

    /// The distinction the `detail` text exists for: "not installed",
    /// "installed but not loaded" and "asked and refused" have three
    /// different fixes, and one message for all three sends people to the
    /// wrong one.
    #[test]
    fn a_machine_that_was_never_asked_does_not_report_a_refusal() {
        // Asks the real machine, so nothing may redirect it meanwhile.
        let _acpi = crate::testenv::real();
        let probe = probe();
        if probe.lighting.dialects.iter().all(|d| !d.asked) {
            assert!(
                !probe.lighting.detail.contains("refused"),
                "not being able to ask is not a refusal: {}",
                probe.lighting.detail
            );
        }
    }
}
