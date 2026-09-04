//! The lighting dialects, and how one of them gets chosen.
//!
//! ## Why there is more than one
//!
//! There is no single "OMEN lighting protocol". There are at least three
//! ways a machine of this family can be lit, they share nothing but the
//! vendor, and **the model name does not say which one a laptop speaks** -
//! the same rule this project follows everywhere else. So they are all
//! implemented, all probed, and the machine picks.
//!
//! | dialect | how | needs |
//! |---|---|---|
//! | [`Dialect::KernelZones`] | `/sys/.../<driver>/rgb_zones/zone0*` | a kernel that publishes them |
//! | [`Dialect::FourZone`] | WMI `0x20009`, command types 2/3 | `acpi_call`, root |
//! | [`Dialect::Lightbar`] | WMI `0x20009`, command type 11 | `acpi_call`, root |
//!
//! The three are tried in that order, and the order is not arbitrary: the
//! kernel one cannot send the firmware a command it did not expect, so
//! wherever it exists it is the right answer.
//!
//! ## Probing is reading
//!
//! A dialect is *available* when a **read** through it answered. Nothing
//! here writes in order to find out whether writing works: a probe that
//! changes the lights is not a probe, and on a machine that speaks a
//! different dialect it would be a write of unknown meaning.
//!
//! That has one consequence worth stating, because it is why the manual
//! override exists at all: a dialect whose read is refused is reported
//! unavailable and auto-selection skips it, but the user can still force
//! it. A firmware that refuses this project's *reads* and accepts its
//! writes is not a machine anyone has seen, but it is not ruled out
//! either, and the cost of being wrong is one refused write.

use serde::{Deserialize, Serialize};

use crate::color::Rgb;
use crate::{fourzone, kernel_zones, lightbar};

/// A failure from one dialect, in the terms that dialect fails in.
#[derive(Debug, thiserror::Error)]
pub enum DialectError {
    #[error(transparent)]
    Acpi(#[from] pyren_core::acpi::AcpiError),
    /// The call completed and the firmware did not say `PASS`. On a
    /// machine that speaks a different dialect this is the normal answer,
    /// which is exactly why it is what the probe tests.
    #[error("the firmware refused (it answered: {0})")]
    Refused(String),
    /// It said `PASS` and then a non-zero return code. The codes are the
    /// driver's own: 3 unknown command, 4 unknown command type, 5 invalid
    /// parameters - and 4 is the interesting one, because it means this
    /// firmware has the lighting command and not this operation.
    #[error("the firmware returned code {0} ({})", return_code_meaning(*.0))]
    ReturnCode(u32),
    /// It answered, and the answer was not the shape this dialect reads.
    #[error("the answer could not be read as colours: {0}")]
    Unreadable(String),
    #[error("{0}")]
    Io(String),
    #[error("writing the zone files needs root")]
    NeedsRoot,
}

impl DialectError {
    /// Whether the firmware actually got the question.
    ///
    /// The distinction this whole module is built around: being unable to
    /// ask is not being told no. Permission and I/O failures never reached
    /// the firmware; a refusal, a return code and an unreadable answer all
    /// did.
    pub fn reached_the_firmware(&self) -> bool {
        !matches!(
            self,
            Self::Acpi(pyren_core::acpi::AcpiError::PermissionDenied)
                | Self::Acpi(pyren_core::acpi::AcpiError::NotLoaded)
                | Self::Acpi(pyren_core::acpi::AcpiError::Io(_))
                | Self::NeedsRoot
                | Self::Io(_)
        )
    }
}

/// The documented meanings, so an error names the fix rather than a number.
pub fn return_code_meaning(code: u32) -> &'static str {
    match code {
        2 => "wrong signature",
        3 => "this firmware does not have the lighting command",
        4 => "this firmware has the lighting command but not this operation",
        5 => "invalid parameters",
        _ => "undocumented",
    }
}

/// One way of talking to the lights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Dialect {
    /// The kernel's own sysfs zone files.
    KernelZones,
    /// The four-zone keyboard over WMI. Two kernel drivers' worth of
    /// corroboration; see [`crate::fourzone`].
    FourZone,
    /// The light strip over WMI, command type 11. What this project
    /// shipped first, ported from `omen-rgb-linux`; see
    /// [`crate::lightbar`].
    Lightbar,
}

/// Tried in this order. Kernel first, because it is the only one that
/// cannot send the firmware something it did not expect.
pub const ORDER: [Dialect; 3] = [Dialect::KernelZones, Dialect::FourZone, Dialect::Lightbar];

impl Dialect {
    pub fn id(self) -> &'static str {
        match self {
            Self::KernelZones => "kernelZones",
            Self::FourZone => "fourZone",
            Self::Lightbar => "lightbar",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        ORDER.into_iter().find(|d| d.id() == id)
    }

    /// What it talks to, in one phrase, for a UI that has to name it.
    pub fn transport(self) -> &'static str {
        match self {
            Self::KernelZones => "the kernel's rgb_zones files",
            Self::FourZone => "WMI 0x20009, command types 2/3",
            Self::Lightbar => "WMI 0x20009, command type 11",
        }
    }

    /// Reads the four zones. This is also the probe: it is the same
    /// question, and asking it twice would be two ACPI round trips for one
    /// answer.
    pub fn read_colors(self) -> Result<Vec<Rgb>, DialectError> {
        match self {
            Self::KernelZones => kernel_zones::read_colors(),
            Self::FourZone => fourzone::read_colors(),
            Self::Lightbar => lightbar::read_colors(),
        }
    }

    /// Writes the four zones, already scaled for brightness.
    ///
    /// `brightness` reaches the firmware only on [`Dialect::Lightbar`],
    /// which has a field for it. The other two have no such field and the
    /// colours arrive already scaled, which is what their reference
    /// drivers do - see [`crate::scale`].
    pub fn write_colors(self, colors: &[Rgb], brightness: u8) -> Result<(), DialectError> {
        match self {
            Self::KernelZones => kernel_zones::write_colors(&crate::scale(colors, brightness)),
            Self::FourZone => fourzone::write_colors(&crate::scale(colors, brightness)),
            Self::Lightbar => lightbar::write_colors(colors, brightness),
        }
    }

    /// Whether this dialect could be tried at all without asking. Cheap:
    /// a `stat`, never a call. A dialect that fails this is not probed,
    /// so a machine with no `acpi_call` does not report two firmware
    /// refusals that never happened.
    fn reachable(self) -> Result<(), &'static str> {
        match self {
            Self::KernelZones => kernel_zones::present()
                .then_some(())
                .ok_or("no kernel rgb_zones files, under either hp-wmi or omen-rgb-keyboard"),
            Self::FourZone | Self::Lightbar => {
                if !lightbar::hp_wmi_present() {
                    Err("no hp-wmi interface on this machine")
                } else if !pyren_core::acpi::is_loaded() {
                    Err("/proc/acpi/call is not there, so the firmware cannot be asked")
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn probe(self) -> DialectProbe {
        if let Err(why) = self.reachable() {
            return DialectProbe {
                id: self.id(),
                transport: self.transport(),
                available: false,
                asked: false,
                detail: why.to_string(),
            };
        }
        match self.read_colors() {
            Ok(_) => DialectProbe {
                id: self.id(),
                transport: self.transport(),
                available: true,
                asked: true,
                detail: "answered a read of all four zones".to_string(),
            },
            // A failure to *reach* the interface is not the firmware
            // saying no. The commonest one by far is an unprivileged
            // process, whose fix is `sudo` rather than different hardware,
            // and recording it as a refusal would put a verdict on the
            // machine that nobody established.
            Err(e) => DialectProbe {
                id: self.id(),
                transport: self.transport(),
                available: false,
                asked: e.reached_the_firmware(),
                detail: e.to_string(),
            },
        }
    }
}

/// What one dialect answered.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DialectProbe {
    pub id: &'static str,
    pub transport: &'static str,
    /// A read through it worked. The only field that means the lights can
    /// be driven this way.
    pub available: bool,
    /// Whether anything was actually asked. False means the dialect was
    /// skipped for want of `acpi_call` or the sysfs files - which is not
    /// the same as a refusal, the same distinction the module makes
    /// everywhere else.
    pub asked: bool,
    pub detail: String,
}

/// Which dialect to use: work it out, or the one the user picked.
///
/// Stored in `rgb.json` and settable over IPC, because auto-selection can
/// only ever pick a dialect that answers a *read*, and the person at the
/// keyboard can see whether the lights actually changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Selection {
    #[default]
    Auto,
    Fixed(Dialect),
}

impl Selection {
    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed(d) => d.id(),
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        if id == "auto" {
            return Some(Self::Auto);
        }
        Dialect::from_id(id).map(Self::Fixed)
    }

    /// The dialect this resolves to, given what the probes said.
    ///
    /// A forced dialect is used **whether or not it probed**: the user has
    /// overridden the automatic choice, and refusing to honour that
    /// because the automatic machinery disagrees would make the setting
    /// decorative. Auto only ever picks one that answered.
    pub fn resolve(self, probes: &[DialectProbe]) -> Option<Dialect> {
        match self {
            Self::Fixed(d) => Some(d),
            Self::Auto => ORDER
                .into_iter()
                .find(|d| probes.iter().any(|p| p.id == d.id() && p.available)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(id: &'static str, available: bool) -> DialectProbe {
        DialectProbe { id, transport: "", available, asked: true, detail: String::new() }
    }

    #[test]
    fn every_dialect_round_trips_through_its_id() {
        for dialect in ORDER {
            assert_eq!(Dialect::from_id(dialect.id()), Some(dialect));
            assert_eq!(Selection::from_id(dialect.id()), Some(Selection::Fixed(dialect)));
        }
        assert_eq!(Selection::from_id("auto"), Some(Selection::Auto));
        assert_eq!(Selection::from_id("nonsense"), None);
        assert_eq!(Selection::Auto.id(), "auto");
    }

    /// Auto takes the first that answered, in [`ORDER`] - not the first
    /// listed, and not the last to answer.
    #[test]
    fn auto_takes_the_first_dialect_that_answered() {
        let none = [probe("kernelZones", false), probe("fourZone", false), probe("lightbar", false)];
        assert_eq!(Selection::Auto.resolve(&none), None);

        let wmi = [probe("kernelZones", false), probe("fourZone", true), probe("lightbar", true)];
        assert_eq!(Selection::Auto.resolve(&wmi), Some(Dialect::FourZone));

        let all = [probe("kernelZones", true), probe("fourZone", true), probe("lightbar", true)];
        assert_eq!(Selection::Auto.resolve(&all), Some(Dialect::KernelZones));
    }

    /// The point of the setting: a machine whose lights this project reads
    /// wrongly and writes rightly is still drivable by hand.
    #[test]
    fn a_forced_dialect_is_used_even_when_nothing_probed() {
        let none = [probe("kernelZones", false), probe("fourZone", false), probe("lightbar", false)];
        assert_eq!(Selection::Fixed(Dialect::Lightbar).resolve(&none), Some(Dialect::Lightbar));
    }

    /// Command type 4 is the answer that says "this firmware has lighting,
    /// and not this operation" - the whole reason for trying more than one
    /// dialect - so it must not read as a generic failure.
    #[test]
    fn the_return_codes_say_which_problem_it_is() {
        assert!(return_code_meaning(3).contains("does not have the lighting command"));
        assert!(return_code_meaning(4).contains("not this operation"));
        assert_eq!(return_code_meaning(99), "undocumented");
        assert!(DialectError::ReturnCode(4).to_string().contains("not this operation"));
    }
}
