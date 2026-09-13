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
//! ## A pinned dialect still has to answer
//!
//! The manual override chooses *among* the dialects that answered - it is
//! how a person picks `lightbar` over `fourZone` when both answer and only
//! one lights the keyboard. It does not send buffers to a firmware that
//! refused the dialect's read, or through an interface that is not there:
//! a write of unknown meaning is exactly what probing by reading exists to
//! avoid, and "the user asked for it" does not make it known.
//!
//! ## The lightbar is asked last, and only when it has to be
//!
//! Its read (`0x20008` type 4) is upstream's alone; no published driver
//! sends it. So it is only sent when nothing earlier answered, or when the
//! lightbar is the dialect somebody pinned.

use pyren_core::{msg, Msg};
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
    /// It answered, and what it answered is not safe to act on - a
    /// truncated buffer, one of the wrong shape. Nothing was written.
    #[error("{}", .0.text)]
    Unsafe(Msg),
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

    /// The sentence for this failure. Only the ACPI half is in the
    /// catalog; the rest carry firmware bytes and OS errors, which are
    /// passed through as params rather than translated.
    pub fn to_msg(&self) -> Msg {
        match self {
            Self::Acpi(inner) => inner.to_msg(),
            Self::Refused(answer) => msg!(
                "rgb.dialect.refused",
                { "answer" => answer.clone() },
                "the firmware refused this lighting dialect (it answered: {answer})"
            ),
            Self::ReturnCode(code) => msg!(
                "rgb.dialect.returnCode",
                { "code" => *code, "meaning" => return_code_meaning(*code) },
                "the firmware returned code {code}: {meaning}"
            ),
            Self::Unreadable(answer) => msg!(
                "rgb.dialect.unreadable",
                { "answer" => answer.clone() },
                "the firmware answered {answer}, which is not a colour reply"
            ),
            Self::NeedsRoot => msg!(
                "rgb.dialect.needsRoot",
                "writing the kernel's zone files needs root"
            ),
            Self::Io(detail) => msg!("rgb.dialect.io", { "error" => detail.clone() }, "{error}"),
            Self::Unsafe(why) => why.clone(),
        }
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
    pub fn transport(self) -> Msg {
        match self {
            Self::KernelZones => msg!(
                "rgb.dialect.transport.kernelZones",
                "the kernel's rgb_zones files"
            ),
            Self::FourZone => msg!(
                "rgb.dialect.transport.fourZone",
                "WMI 0x20009, command types 2/3"
            ),
            Self::Lightbar => msg!(
                "rgb.dialect.transport.lightbar",
                "WMI 0x20009, command type 11"
            ),
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

    /// The probe's question: a read that a write through this dialect
    /// could follow under `policy`, and how that write would go out. The
    /// same as [`Dialect::read_colors`] except on [`Dialect::FourZone`],
    /// which is only available where it is writable.
    fn probe_read(self, policy: Policy) -> Result<Option<fourzone::WriteMode>, DialectError> {
        match self {
            Self::FourZone => {
                fourzone::probe_colors(policy.allow_truncated_four_zone).map(|(_, mode)| Some(mode))
            }
            other => other.read_colors().map(|_| None),
        }
    }

    /// Writes the four zones, already scaled for brightness.
    ///
    /// `brightness` reaches the firmware only on [`Dialect::Lightbar`],
    /// which has a field for it. The other two have no such field and the
    /// colours arrive already scaled, which is what their reference
    /// drivers do - see [`crate::scale`].
    pub fn write_colors(
        self,
        colors: &[Rgb],
        brightness: u8,
        policy: Policy,
    ) -> Result<(), DialectError> {
        match self {
            Self::KernelZones => kernel_zones::write_colors(&crate::scale(colors, brightness)),
            Self::FourZone => fourzone::write_colors(
                &crate::scale(colors, brightness),
                policy.allow_truncated_four_zone,
            ),
            Self::Lightbar => lightbar::write_colors(colors, brightness),
        }
    }

    /// A writer for a stream of frames - what an animation holds for as
    /// long as it runs.
    ///
    /// Only [`Dialect::FourZone`] is different from [`Dialect::write_colors`]:
    /// it keeps the firmware buffer rather than reading it before every
    /// frame, which is what makes 30 fps cheap. The other two have nothing
    /// to cache - `kernelZones` does its read-modify-write in the kernel,
    /// and the lightbar sends a whole buffer every time anyway.
    pub fn frames(self, policy: Policy) -> Result<FrameSink, DialectError> {
        let writer = match self {
            Self::FourZone => Writer::FourZone(fourzone::FrameWriter::new(
                policy.allow_truncated_four_zone,
            )?),
            Self::KernelZones => Writer::KernelZones([None; crate::ZONES]),
            Self::Lightbar => Writer::Lightbar,
        };
        Ok(FrameSink { writer, last: None })
    }

    /// The most frames a second this dialect is sent, whatever the effect
    /// asked for. Each frame over WMI is a firmware call, and each zone
    /// file behind `kernelZones` is one in the kernel driver.
    pub fn max_fps(self) -> u8 {
        match self {
            Self::FourZone | Self::Lightbar => MAX_FPS_WMI,
            Self::KernelZones => MAX_FPS_KERNEL_ZONES,
        }
    }

    /// Whether this dialect could be tried at all without asking. Cheap:
    /// a `stat`, never a call. A dialect that fails this is not probed,
    /// so a machine with no `acpi_call` does not report two firmware
    /// refusals that never happened.
    fn reachable(self) -> Result<(), Msg> {
        match self {
            Self::KernelZones => kernel_zones::present().then_some(()).ok_or_else(|| {
                msg!(
                    "rgb.dialect.unreachable.kernelZones",
                    "no kernel rgb_zones files, under either hp-wmi or omen-rgb-keyboard"
                )
            }),
            Self::FourZone | Self::Lightbar => {
                if !lightbar::hp_wmi_present() {
                    Err(msg!(
                        "rgb.dialect.unreachable.noWmi",
                        "no hp-wmi interface on this machine"
                    ))
                } else if !lightbar::is_hp() {
                    Err(msg!(
                        "rgb.dialect.unreachable.notHp",
                        "this machine does not report HP as its maker, so no HP firmware \
                         command is sent"
                    ))
                } else if !pyren_core::acpi::is_loaded() {
                    Err(msg!(
                        "rgb.dialect.unreachable.noAcpiCall",
                        "/proc/acpi/call is not there, so the firmware cannot be asked"
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn probe(self, policy: Policy) -> DialectProbe {
        if let Err(why) = self.reachable() {
            return self.not_asked(why);
        }
        match self.probe_read(policy) {
            Ok(Some(fourzone::WriteMode::Truncated)) => DialectProbe {
                id: self.id(),
                transport: self.transport(),
                available: true,
                asked: true,
                detail: msg!(
                    "rgb.dialect.answeredTruncated",
                    "answered, with a reply acpi_call cut short; writes go through with the \
                     bytes it cut off sent as zero, because that is allowed in the lighting \
                     settings"
                ),
                write_mode: Some(fourzone::WriteMode::Truncated.id()),
            },
            Ok(mode) => DialectProbe {
                id: self.id(),
                transport: self.transport(),
                available: true,
                asked: true,
                detail: msg!("rgb.dialect.answered", "answered a read of all four zones"),
                write_mode: mode.map(fourzone::WriteMode::id),
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
                detail: e.to_msg(),
                write_mode: None,
            },
        }
    }

    /// The entry for a dialect that was deliberately not asked. See the
    /// module docs on the lightbar.
    pub fn not_asked(self, why: Msg) -> DialectProbe {
        DialectProbe {
            id: self.id(),
            transport: self.transport(),
            available: false,
            asked: false,
            detail: why,
            write_mode: None,
        }
    }
}

/// The user's settings that bear on what a dialect may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Write four-zone colours through a reply `acpi_call` cut to its
    /// layout's declared truncated length. See [`fourzone::Layout::truncated_len`].
    pub allow_truncated_four_zone: bool,
}

impl Default for Policy {
    /// On: the OMEN 16 this project runs on only has truncated replies.
    fn default() -> Self {
        Self {
            allow_truncated_four_zone: true,
        }
    }
}

/// Frames a second over WMI: one firmware call each.
pub const MAX_FPS_WMI: u8 = 20;
/// Frames a second through the zone files: up to four writes each, and a
/// firmware call behind every one of them.
pub const MAX_FPS_KERNEL_ZONES: u8 = 15;

/// See [`Dialect::frames`]. Skips a frame identical to the last one it
/// wrote - most of a slow effect, and all of a paused or black one.
pub struct FrameSink {
    writer: Writer,
    /// What was last written, as it went to the hardware.
    last: Option<(Vec<Rgb>, u8)>,
}

enum Writer {
    FourZone(fourzone::FrameWriter),
    /// The last colour written to each zone file, so an unchanged zone is
    /// not written again.
    KernelZones([Option<Rgb>; crate::ZONES]),
    Lightbar,
}

impl FrameSink {
    fn max_fps(&self) -> u8 {
        match self.writer {
            Writer::FourZone(_) => Dialect::FourZone.max_fps(),
            Writer::KernelZones(_) => Dialect::KernelZones.max_fps(),
            Writer::Lightbar => Dialect::Lightbar.max_fps(),
        }
    }
}

impl crate::effects::Sink for FrameSink {
    fn show(&mut self, colors: &[Rgb], brightness: u8) -> Result<(), DialectError> {
        let frame = (colors.to_vec(), brightness);
        if self.last.as_ref() == Some(&frame) {
            return Ok(());
        }
        let result = match &mut self.writer {
            Writer::FourZone(writer) => writer.write(&crate::scale(colors, brightness)),
            Writer::KernelZones(written) => {
                kernel_zones::write_changed(&crate::scale(colors, brightness), written)
            }
            Writer::Lightbar => lightbar::write_colors(colors, brightness),
        };
        // Only a frame that landed counts as written: after a failure the
        // same frame has to be tried again, not skipped.
        self.last = result.is_ok().then_some(frame);
        result
    }

    fn max_fps(&self) -> Option<u8> {
        Some(FrameSink::max_fps(self))
    }
}

/// What one dialect answered.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DialectProbe {
    pub id: &'static str,
    /// Translatable - render with `tm()`.
    pub transport: Msg,
    /// A read through it worked. The only field that means the lights can
    /// be driven this way.
    pub available: bool,
    /// Whether anything was actually asked. False means the dialect was
    /// skipped for want of `acpi_call` or the sysfs files - which is not
    /// the same as a refusal, the same distinction the module makes
    /// everywhere else.
    pub asked: bool,
    /// Translatable - render with `tm()`.
    pub detail: Msg,
    /// How a write goes out, where a dialect has more than one way:
    /// `"full"`, or `"truncated"` for four-zone writes through a cut-short
    /// reply. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_mode: Option<&'static str>,
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
    /// Auto picks the first that answered. A pinned dialect is used only
    /// **if it answered too**: pinning chooses among the dialects that work,
    /// it does not send buffers to a firmware that refused them. See the
    /// module docs.
    pub fn resolve(self, probes: &[DialectProbe]) -> Option<Dialect> {
        let answered = |d: Dialect| probes.iter().any(|p| p.id == d.id() && p.available);
        match self {
            Self::Fixed(d) => answered(d).then_some(d),
            Self::Auto => ORDER.into_iter().find(|&d| answered(d)),
        }
    }

    /// The dialect pinned, if any.
    pub fn pinned(self) -> Option<Dialect> {
        match self {
            Self::Auto => None,
            Self::Fixed(d) => Some(d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(id: &'static str, available: bool) -> DialectProbe {
        DialectProbe {
            id,
            transport: Msg::literal(""),
            available,
            asked: true,
            detail: Msg::literal(""),
            write_mode: None,
        }
    }

    #[test]
    fn every_dialect_round_trips_through_its_id() {
        for dialect in ORDER {
            assert_eq!(Dialect::from_id(dialect.id()), Some(dialect));
            assert_eq!(
                Selection::from_id(dialect.id()),
                Some(Selection::Fixed(dialect))
            );
        }
        assert_eq!(Selection::from_id("auto"), Some(Selection::Auto));
        assert_eq!(Selection::from_id("nonsense"), None);
        assert_eq!(Selection::Auto.id(), "auto");
    }

    /// Auto takes the first that answered, in [`ORDER`] - not the first
    /// listed, and not the last to answer.
    #[test]
    fn auto_takes_the_first_dialect_that_answered() {
        let none = [
            probe("kernelZones", false),
            probe("fourZone", false),
            probe("lightbar", false),
        ];
        assert_eq!(Selection::Auto.resolve(&none), None);

        let wmi = [
            probe("kernelZones", false),
            probe("fourZone", true),
            probe("lightbar", true),
        ];
        assert_eq!(Selection::Auto.resolve(&wmi), Some(Dialect::FourZone));

        let all = [
            probe("kernelZones", true),
            probe("fourZone", true),
            probe("lightbar", true),
        ];
        assert_eq!(Selection::Auto.resolve(&all), Some(Dialect::KernelZones));
    }

    /// Pinning picks among the dialects that answered - including one that
    /// auto would not have chosen - and never sends through one that did
    /// not.
    #[test]
    fn a_pinned_dialect_is_used_only_where_it_answered() {
        let none = [
            probe("kernelZones", false),
            probe("fourZone", false),
            probe("lightbar", false),
        ];
        assert_eq!(Selection::Fixed(Dialect::Lightbar).resolve(&none), None);

        let both = [
            probe("kernelZones", false),
            probe("fourZone", true),
            probe("lightbar", true),
        ];
        assert_eq!(
            Selection::Fixed(Dialect::Lightbar).resolve(&both),
            Some(Dialect::Lightbar),
            "a pin still overrides auto's order"
        );
    }

    /// A frame the hardware already shows is a firmware call for nothing.
    #[test]
    fn a_frame_identical_to_the_last_is_not_written_again() {
        use crate::effects::Sink;
        let mut sink = FrameSink {
            writer: Writer::KernelZones([Some(Rgb::BLACK); crate::ZONES]),
            last: Some((vec![Rgb::new(1, 2, 3); crate::ZONES], 50)),
        };
        // Would fail if it reached the (absent) zone files.
        assert!(sink.show(&[Rgb::new(1, 2, 3); crate::ZONES], 50).is_ok());
        assert_eq!(sink.max_fps(), MAX_FPS_KERNEL_ZONES);
    }

    /// Command type 4 is the answer that says "this firmware has lighting,
    /// and not this operation" - the whole reason for trying more than one
    /// dialect - so it must not read as a generic failure.
    #[test]
    fn the_return_codes_say_which_problem_it_is() {
        assert!(return_code_meaning(3).contains("does not have the lighting command"));
        assert!(return_code_meaning(4).contains("not this operation"));
        assert_eq!(return_code_meaning(99), "undocumented");
        assert!(DialectError::ReturnCode(4)
            .to_string()
            .contains("not this operation"));
    }
}
