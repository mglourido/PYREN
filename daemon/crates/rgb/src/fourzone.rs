//! The four-zone keyboard, over the HP WMI BIOS interface.
//!
//! ## Where these numbers come from
//!
//! Not from this project's guesswork. The command and the two command
//! types are the ones two independent kernel drivers use, and both were
//! read before this was written:
//!
//! - the `hp-wmi` four-zone patch (Rishit Bansal, 2023, posted to
//!   linux-leds and carried by several distribution kernels), and
//! - `OmenLinux/omen-rgb-keyboard` (2025), whose header names the whole
//!   command-type space rather than only the two it uses.
//!
//! ```text
//! command      0x20009   HPWMI_FOURZONE - the lighting command
//! commandtype  2         FOURZONE_COLOR_GET   in 128, out 128
//!              3         FOURZONE_COLOR_SET   in 128, out 128
//!
//! the 128-byte state buffer
//!   0        unknown, and preserved
//!   1..25    zero on every machine seen, and checked
//!   25..28   zone 0 R,G,B
//!   28..31   zone 1
//!   31..34   zone 2
//!   34..37   zone 3
//!   37..128  unknown, and preserved
//! ```
//!
//! ## Layouts are data
//!
//! That table is one [`Layout`], and everything that reads or writes the
//! buffer goes through a layout: how long the buffer is, where each zone
//! sits, and which bytes are known to be zero. A machine with a longer
//! buffer or more zones is supported by adding a layout to [`LAYOUTS`] -
//! the length check, the shape check and the patching all follow from it,
//! so a new layout gets every guard below without anyone re-writing them.
//! A layout is never guessed: a reply that matches none is refused.
//!
//! ## A write is a read, then a write - and only a *whole* read
//!
//! Both reference drivers do a `COLOR_GET` first and patch the colour
//! bytes into what comes back, and this does the same. The buffer holds
//! fields nobody has identified - the comment in the 2023 patch is
//! literally *"Zones start at offset 25. Wonder what's in the rest of the
//! buffer?"* - so what is sent back must be exactly what was read, with
//! only the colours changed.
//!
//! **That is only fully possible when the whole buffer was read.**
//! `acpi_call` renders a buffer reply as `{0x50, 0x41, …}` into a fixed
//! result buffer, so a 128-byte answer arrives as 34 bytes. So there are
//! exactly two lengths a write can start from, and every other one is
//! refused ([`DialectError::Unsafe`]):
//!
//! - [`Layout::state_len`], the whole buffer: the strict path, always
//!   allowed. Every byte sent back is a byte that was read.
//! - [`Layout::truncated_len`], the length `acpi_call` is known to cut this
//!   layout's reply to: allowed only while the user setting
//!   `allowTruncatedFourZone` is on (it is by default - it is how the OMEN
//!   16 this project runs on gets its colours). The bytes past the cut
//!   **cannot** be known, and go out as zero; see [`Layout::truncated_len`].
//!
//! Both paths still need the known-zero bytes that *were* read to be zero,
//! still patch only the colour bytes, and still run under every other
//! guard: the one writer lock, the shared `acpi_call` lock, the call
//! deadline and the frame-rate caps.
//!
//! The shape check is also what catches a reply that is not ours. The
//! `acpi_call` file is global, and a program that ignores the shared lock
//! can have its answer read by us; that answer does not look like this
//! buffer, and so it is never written back to the firmware as lighting
//! state.
//!
//! On a machine where `acpi_call` truncates, reads still work - they are
//! harmless - and the way to drive the lights is the `kernelZones`
//! dialect, which has no such limit: see [`crate::kernel_zones`].
//!
//! ## Brightness is not in here
//!
//! There is a `SET_BRIGHTNESS = 5` command type, and this dialect does not
//! use it: nobody has published its payload, and the reference driver
//! scales the colours in software instead. So does this - see
//! [`crate::scale`] - which means brightness works identically on every
//! dialect rather than working on one and silently doing nothing on
//! another.

use std::ops::Range;

use pyren_core::{acpi, msg};

use crate::color::Rgb;
use crate::dialect::DialectError;
use crate::reply;

/// `HPWMI_FOURZONE` - the lighting command.
pub const COMMAND: u32 = 0x0002_0009;

/// `HPWMI_FOURZONE_COLOR_GET`.
pub const COLOR_GET: u32 = 2;
/// `HPWMI_FOURZONE_COLOR_SET`.
pub const COLOR_SET: u32 = 3;
/// `HPWMI_GET_PLATFORM_INFO`. Not used to drive anything - it is the
/// cheapest read that says whether the `0x20009` command space answers on
/// this machine at all, which is a different question from whether the
/// four-zone colours do.
pub const PLATFORM_INFO: u32 = 1;

/// The state buffer both command types take and return, on
/// [`HP_FOURZONE_128`].
pub const STATE_LEN: usize = 128;

/// Where zone 0's red byte lives in that buffer.
pub const ZONE_OFFSET: usize = 25;

/// One firmware's four-zone buffer, described rather than hard-coded.
/// See the module docs.
#[derive(Debug)]
pub struct Layout {
    /// A name for errors and logs.
    pub id: &'static str,
    pub command: u32,
    pub get: u32,
    pub set: u32,
    /// The whole buffer. A read of exactly this many data bytes can always
    /// be written back.
    pub state_len: usize,
    /// The one shorter length a read of this layout is known to arrive at,
    /// where `acpi_call` cuts it, and that a write may start from when the
    /// user allows it. `None` for a layout never seen truncated.
    ///
    /// **What that costs is not hidden:** the firmware is sent
    /// `state_len` bytes and only `truncated_len` of them were read, so the
    /// rest go out as zero. On [`HP_FOURZONE_128`] every byte that *can* be
    /// seen before the colours is zero apart from byte 0, which is the
    /// best evidence there is that zero is what the tail holds - but it is
    /// evidence, not a reading. Declare this only for a length measured on
    /// real hardware.
    pub truncated_len: Option<usize>,
    /// Where each zone's red byte is; green and blue follow it.
    pub zones: &'static [usize],
    /// Bytes that are zero in every valid read. A read where they are not
    /// is not this layout, and is never written back.
    pub zero: &'static [Range<usize>],
}

/// The layout both reference drivers use. See the table in the module docs.
// One known-zero range, deliberately a slice of ranges: a layout may have
// several.
#[allow(clippy::single_range_in_vec_init)]
pub const HP_FOURZONE_128: Layout = Layout {
    id: "hp-fourzone-128",
    command: COMMAND,
    get: COLOR_GET,
    set: COLOR_SET,
    state_len: STATE_LEN,
    // Measured on an OMEN 16, three runs: 42 bytes of reply, less the
    // 8-byte PASS header.
    truncated_len: Some(34),
    zones: &[25, 28, 31, 34],
    zero: &[1..25],
};

/// Every layout this build knows, tried in order. Add one here and it is
/// probed, validated and written with the same guards.
pub const LAYOUTS: &[Layout] = &[HP_FOURZONE_128];

impl Layout {
    /// How many zones a reply this long actually carries.
    pub fn zones_in(&self, len: usize) -> usize {
        self.zones.iter().take_while(|&&at| at + 3 <= len).count()
    }

    /// The first known-zero byte that is not zero, among the bytes present.
    fn misshapen(&self, state: &[u8]) -> Option<(usize, u8)> {
        self.zero
            .iter()
            .flat_map(|range| range.start.min(state.len())..range.end.min(state.len()))
            .find(|&at| state[at] != 0)
            .map(|at| (at, state[at]))
    }

    /// Whether `state` may be patched and sent back, and how: the whole
    /// buffer, or - with `allow_truncated` - exactly [`Layout::truncated_len`]
    /// of it. Either way in this layout's shape. The one gate every write
    /// goes through.
    pub fn check_writable(
        &self,
        state: &[u8],
        allow_truncated: bool,
    ) -> Result<WriteMode, DialectError> {
        let mode = if state.len() == self.state_len {
            WriteMode::Full
        } else if Some(state.len()) == self.truncated_len {
            if !allow_truncated {
                return Err(DialectError::Unsafe(msg!(
                    "rgb.dialect.fourZone.truncatedOff",
                    { "got" => state.len(), "need" => self.state_len, "layout" => self.id },
                    "the firmware's reply was cut short to {got} of {need} bytes ({layout}), \
                     and writing through a cut-short reply is turned off, so nothing was written"
                )));
            }
            WriteMode::Truncated
        } else {
            return Err(DialectError::Unsafe(msg!(
                "rgb.dialect.fourZone.length",
                { "got" => state.len(), "need" => self.state_len, "layout" => self.id },
                "the firmware's reply was {got} bytes, a length the {layout} layout does not \
                 know (it needs {need}), so nothing was written"
            )));
        };
        self.check_shape(state)?;
        Ok(mode)
    }

    fn check_shape(&self, state: &[u8]) -> Result<(), DialectError> {
        match self.misshapen(state) {
            None => Ok(()),
            Some((at, value)) => Err(DialectError::Unsafe(msg!(
                "rgb.dialect.fourZone.shape",
                { "layout" => self.id, "at" => at, "value" => value },
                "the firmware's reply does not have the {layout} layout's shape (byte {at} is \
                 {value} where it should be 0), so nothing was written"
            ))),
        }
    }

    /// The zones present in `state`, in order.
    pub fn colors(&self, state: &[u8]) -> Vec<Rgb> {
        self.zones[..self.zones_in(state.len())]
            .iter()
            .map(|&at| Rgb::new(state[at], state[at + 1], state[at + 2]))
            .collect()
    }

    /// Writes `colors` into their zones. Past this layout's zones they are
    /// dropped; short of them the rest keep what was read.
    fn patch(&self, state: &mut [u8], colors: &[Rgb]) {
        for (&at, color) in self.zones.iter().zip(colors) {
            if at + 3 <= state.len() {
                state[at] = color.r;
                state[at + 1] = color.g;
                state[at + 2] = color.b;
            }
        }
    }

    fn read(&self) -> Result<Vec<u8>, DialectError> {
        let reply = acpi::wmi_call(
            self.command,
            self.get,
            &vec![0u8; self.state_len],
            self.state_len,
            self.state_len,
        )?;
        reply::payload(&reply)
    }

    /// Checks, patches and sends. `state` is what was read, and is checked
    /// again here anyway, because this is the last line before the firmware.
    /// A truncated `state` is extended to `state_len` with zeros in a copy
    /// (see [`Layout::truncated_len`]), so the colours past the cut - zone 3
    /// on the OMEN 16 - are still written.
    fn send(
        &self,
        state: &[u8],
        colors: &[Rgb],
        allow_truncated: bool,
    ) -> Result<(), DialectError> {
        self.check_writable(state, allow_truncated)?;
        let mut buffer = state.to_vec();
        buffer.resize(self.state_len, 0);
        self.patch(&mut buffer, colors);
        let reply = acpi::wmi_call(
            self.command,
            self.set,
            &buffer,
            self.state_len,
            self.state_len,
        )?;
        reply::payload(&reply).map(|_| ())
    }
}

/// How a write went out. Reported by the probe and the status, so a UI can
/// say when writes are running on a cut-short read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Every byte sent back was read.
    Full,
    /// Read at [`Layout::truncated_len`]; the tail went out as zero.
    Truncated,
}

impl WriteMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Truncated => "truncated",
        }
    }
}

/// Reads the state buffer in the first layout's size. What diagnostics
/// dump; nothing that writes uses it.
pub fn read_state() -> Result<Vec<u8>, DialectError> {
    LAYOUTS[0].read()
}

/// How many zones a reply this long carries, on [`HP_FOURZONE_128`].
///
/// **A full four never arrive through `acpi_call`.** Its result buffer is
/// 256 characters and it prints each byte as `0x00, ` - six characters -
/// so a reply is capped at 42 bytes however much the firmware sent. Eight
/// of those are the `PASS` header, leaving 34, and zone 3 lives at bytes
/// 34..37. Measured on an OMEN 16 across three runs.
pub fn zones_in(state: &[u8]) -> usize {
    HP_FOURZONE_128.zones_in(state.len())
}

/// The zones this dialect can actually read - **not always four**.
///
/// Short by design rather than padded: a truncated read reports the zones
/// it reached, so it is never mistaken for a keyboard whose last zone is
/// off. The reply must still have a known layout's shape.
pub fn read_colors() -> Result<Vec<Rgb>, DialectError> {
    let mut last = None;
    for layout in LAYOUTS {
        let state = layout.read()?;
        if layout.zones_in(state.len()) == 0 {
            last = Some(DialectError::Unreadable(format!(
                "the reply is {} bytes and the {} layout's first zone starts at {}",
                state.len(),
                layout.id,
                layout.zones[0]
            )));
            continue;
        }
        match layout.check_shape(&state) {
            Ok(()) => return Ok(layout.colors(&state)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| DialectError::Unreadable("no four-zone layout is known".into())))
}

/// A buffer a write may start from, the layout it matched and how it may
/// be written - or why there is none. What every write starts from, and
/// what the probe asks: this dialect is only available where it can be
/// written under the current setting.
pub fn writable_state(
    allow_truncated: bool,
) -> Result<(&'static Layout, Vec<u8>, WriteMode), DialectError> {
    let mut last = None;
    for layout in LAYOUTS {
        let state = layout.read()?;
        match layout.check_writable(&state, allow_truncated) {
            Ok(mode) => return Ok((layout, state, mode)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| DialectError::Unreadable("no four-zone layout is known".into())))
}

/// The probe: the colours from a read that a write could follow, and how
/// that write would go out.
pub fn probe_colors(allow_truncated: bool) -> Result<(Vec<Rgb>, WriteMode), DialectError> {
    let (layout, state, mode) = writable_state(allow_truncated)?;
    Ok((layout.colors(&state), mode))
}

pub fn write_colors(colors: &[Rgb], allow_truncated: bool) -> Result<(), DialectError> {
    let (layout, state, _) = writable_state(allow_truncated)?;
    layout.send(&state, colors, allow_truncated)
}

/// How long an animation trusts the buffer it read before reading it again.
///
/// Nothing is known to change the bytes around the colours while the
/// machine runs, but nothing is known about most of them at all. A read
/// every few seconds costs 2.3 ms against the 20 s of frames between them,
/// so it is cheap insurance against sending back a setting that moved.
const REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

/// The writer an animation uses: one `COLOR_GET`, then one `COLOR_SET` a
/// frame.
///
/// [`write_colors`] reads before every write, and the read is 80 % of the
/// cost - 2.3 ms against 0.7 ms for the set (`dev/FINDINGS.md`, "Lighting
/// effects"). So an animation keeps the buffer and only patches the colours
/// into it - a buffer that passed the same checks a single write does, and
/// passes them again at every refresh.
pub struct FrameWriter {
    layout: &'static Layout,
    state: Vec<u8>,
    allow_truncated: bool,
    read_at: std::time::Instant,
}

impl FrameWriter {
    pub fn new(allow_truncated: bool) -> Result<Self, DialectError> {
        let (layout, state, _) = writable_state(allow_truncated)?;
        Ok(Self {
            layout,
            state,
            allow_truncated,
            read_at: std::time::Instant::now(),
        })
    }

    pub fn write(&mut self, colors: &[Rgb]) -> Result<(), DialectError> {
        if self.read_at.elapsed() >= REFRESH {
            let state = self.layout.read()?;
            self.layout.check_writable(&state, self.allow_truncated)?;
            self.state = state;
            self.read_at = std::time::Instant::now();
        }
        self.layout.send(&self.state, colors, self.allow_truncated)
    }
}

/// Whether the `0x20009` command space answers a read on this machine.
///
/// Reported beside the dialect's own probe because the two failures mean
/// different things: no answer here at all is "this firmware has no
/// lighting command", while an answer here and a refusal to `COLOR_GET` is
/// "it has one, and this is not a four-zone keyboard".
pub fn platform_info() -> Result<Vec<u8>, DialectError> {
    let reply = acpi::wmi_call(COMMAND, PLATFORM_INFO, &[0u8; 4], 4, STATE_LEN)?;
    reply::payload(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(request: &str) -> Vec<u8> {
        assert!(request.starts_with('b'), "acpi_call buffers start with b");
        acpi::parse_bytes(request).expect("the request must be plain hex")
    }

    /// A whole buffer in the shape of the 128-byte layout, zones lit.
    fn whole() -> Vec<u8> {
        let mut state = vec![0u8; STATE_LEN];
        state[0] = 0x03;
        for (i, at) in [25usize, 28, 31, 34].into_iter().enumerate() {
            state[at..at + 3].copy_from_slice(&[i as u8 + 1, 0x40, 0x80]);
        }
        // The tail is unknown, not zero: it must survive a patch as read.
        state[100] = 0xaa;
        state
    }

    #[test]
    fn a_truncated_reply_reports_the_zones_it_reached_and_no_more() {
        // 34 bytes: what an OMEN 16 actually returns, measured.
        assert_eq!(
            zones_in(&[0u8; 34]),
            3,
            "zone 3 starts one byte past the end"
        );
        assert_eq!(zones_in(&[0u8; STATE_LEN]), crate::ZONES);
        assert_eq!(zones_in(&[0u8; STATE_LEN * 2]), crate::ZONES);
        assert_eq!(zones_in(&[0u8; ZONE_OFFSET]), 0);
        assert_eq!(zones_in(&[]), 0);
    }

    /// The guard this module exists for: nothing short of the whole buffer
    /// is ever sent back, and nothing is padded.
    #[test]
    fn only_a_whole_buffer_in_the_right_shape_may_be_written() {
        let layout = &HP_FOURZONE_128;
        for allow in [true, false] {
            assert_eq!(
                layout.check_writable(&whole(), allow).unwrap(),
                WriteMode::Full
            );

            for wrong in [33usize, 35, 127, 129, 0] {
                let mut state = whole();
                state.resize(wrong, 0);
                match layout.check_writable(&state, allow) {
                    Err(DialectError::Unsafe(m)) => {
                        assert_eq!(m.key, "rgb.dialect.fourZone.length", "{wrong} bytes")
                    }
                    other => panic!("{wrong} bytes must be refused, got {other:?}"),
                }
            }

            let mut foreign = whole();
            foreign[5] = 0x50; // somebody else's reply, or a firmware we have not seen
            match layout.check_writable(&foreign, allow) {
                Err(DialectError::Unsafe(m)) => assert_eq!(m.key, "rgb.dialect.fourZone.shape"),
                other => panic!("expected a shape refusal, got {other:?}"),
            }
        }
    }

    /// The opt-in: the one declared truncated length, and only while it is
    /// allowed - and never past the shape check.
    #[test]
    fn a_truncated_read_is_written_only_while_allowed() {
        let layout = &HP_FOURZONE_128;
        let mut cut = whole();
        cut.truncate(34);
        assert_eq!(
            layout.check_writable(&cut, true).unwrap(),
            WriteMode::Truncated
        );
        match layout.check_writable(&cut, false) {
            Err(DialectError::Unsafe(m)) => assert_eq!(m.key, "rgb.dialect.fourZone.truncatedOff"),
            other => panic!("expected a refusal with the setting off, got {other:?}"),
        }
        cut[3] = 1;
        assert!(
            matches!(
                layout.check_writable(&cut, true),
                Err(DialectError::Unsafe(_))
            ),
            "the setting does not waive the shape check"
        );
    }

    /// Patching changes the colour bytes and nothing else.
    #[test]
    fn a_patch_touches_only_the_colour_bytes() {
        let layout = &HP_FOURZONE_128;
        let before = whole();
        let mut after = before.clone();
        let colors = [Rgb::new(9, 8, 7); 4];
        layout.patch(&mut after, &colors);
        for (at, (old, new)) in before.iter().zip(&after).enumerate() {
            let in_zone = layout.zones.iter().any(|&z| (z..z + 3).contains(&at));
            if !in_zone {
                assert_eq!(old, new, "byte {at} is not a colour and must not change");
            }
        }
        assert_eq!(layout.colors(&after), colors.to_vec());

        // Fewer colours than zones: the rest keep what was read.
        let mut partial = before.clone();
        layout.patch(&mut partial, &[Rgb::new(1, 1, 1)]);
        assert_eq!(&partial[28..37], &before[28..37]);
    }

    /// Every declared layout has to be internally consistent, or a new one
    /// could put a colour on top of a byte it also declares zero.
    #[test]
    fn every_layout_is_self_consistent() {
        for layout in LAYOUTS {
            assert!(!layout.zones.is_empty(), "{}", layout.id);
            assert!(layout.zones.len() >= crate::ZONES, "{}", layout.id);
            for &at in layout.zones {
                assert!(at + 3 <= layout.state_len, "{}: zone at {at}", layout.id);
                for range in layout.zero {
                    assert!(
                        at + 3 <= range.start || at >= range.end,
                        "{}: zone at {at} overlaps a known-zero range",
                        layout.id
                    );
                }
            }
            for pair in layout.zones.windows(2) {
                assert!(pair[1] >= pair[0] + 3, "{}: zones overlap", layout.id);
            }
            for range in layout.zero {
                assert!(range.end <= layout.state_len, "{}", layout.id);
            }
            if let Some(cut) = layout.truncated_len {
                assert!(cut < layout.state_len, "{}: a cut is shorter", layout.id);
                let first = layout.zones[0];
                assert!(
                    first + 3 <= cut,
                    "{}: a cut reaching no zone is no read",
                    layout.id
                );
            }
        }
    }

    /// The header is what no test on hardware could isolate.
    #[test]
    fn the_header_carries_the_command_the_reference_drivers_send() {
        let request = bytes_of(&acpi::wmi_request(
            COMMAND,
            COLOR_SET,
            STATE_LEN,
            &[0u8; STATE_LEN],
        ));
        assert_eq!(&request[0..4], b"SECU");
        assert_eq!(
            u32::from_le_bytes(request[4..8].try_into().unwrap()),
            0x0002_0009
        );
        assert_eq!(u32::from_le_bytes(request[8..12].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(request[12..16].try_into().unwrap()), 128);
        assert_eq!(request.len(), 16 + STATE_LEN);
    }

    #[test]
    fn a_state_sized_answer_asks_for_method_three() {
        assert_eq!(acpi::method_for_outsize(STATE_LEN), 3);
        assert_eq!(acpi::method_for_outsize(0), 1);
        assert_eq!(acpi::method_for_outsize(4), 2);
    }

    #[test]
    fn the_four_zones_land_where_both_reference_drivers_read_them() {
        assert_eq!(HP_FOURZONE_128.zones, &[25, 28, 31, 34]);
        assert_eq!(HP_FOURZONE_128.zones[0], ZONE_OFFSET);
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// What actually goes down the wire for a read, byte for byte.
    #[test]
    fn a_read_puts_exactly_this_on_the_wire() {
        let dir = std::env::temp_dir().join(format!("pyren-wire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir is writable");
        let path = dir.join("call");

        let written = {
            let _acpi = crate::testenv::redirect(&path);
            let _ = read_state();
            std::fs::read_to_string(&path).expect("the request was written")
        };
        let _ = std::fs::remove_dir_all(&dir);

        let expected = format!(
            "\\_SB.WMID.WMAA 0 3 b53454355{}{}{}{}",
            "09000200", // command 0x20009, little-endian
            "02000000", // command type 2, COLOR_GET
            "80000000", // datasize 128
            "0".repeat(256),
        );
        assert_eq!(written, expected);
    }

    /// A plain file echoes the request back, which is no reply at all: a
    /// write must stop at the read and send nothing.
    #[test]
    fn a_write_after_a_bad_read_sends_nothing() {
        let dir = std::env::temp_dir().join(format!("pyren-wire-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir is writable");
        let path = dir.join("call");

        let written = {
            let _acpi = crate::testenv::redirect(&path);
            assert!(write_colors(&[Rgb::new(255, 0, 0); 4], true).is_err());
            std::fs::read_to_string(&path).expect("the read was written")
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            written.contains("b5345435509000200020000"),
            "only the GET went out"
        );
        assert!(
            !written.contains("b5345435509000200030000"),
            "no SET went out"
        );
    }

    /// A fake firmware: a FIFO the test answers, recording each request.
    /// Every call is one request followed by one reply.
    fn fake_firmware(
        replies: Vec<String>,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Vec<String>>) {
        let dir = std::env::temp_dir().join(format!(
            "pyren-fake-fw-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("call");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let served = fifo.clone();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in replies {
                requests.push(std::fs::read_to_string(&served).unwrap());
                std::fs::write(&served, reply).unwrap();
            }
            requests
        });
        (fifo, handle)
    }

    fn pass_with(data: &[u8]) -> String {
        let mut bytes = b"PASS\0\0\0\0".to_vec();
        bytes.extend_from_slice(data);
        let hex: Vec<String> = bytes.iter().map(|b| format!("0x{b:02x}")).collect();
        format!("{{{}}}", hex.join(", "))
    }

    fn truncated_state() -> Vec<u8> {
        let mut state = vec![0u8; 34];
        state[0] = 3;
        state[25..34].copy_from_slice(&[0x0f, 0x84, 0xfa, 0x71, 0x0f, 0xfa, 0xf9, 0x35, 0x0f]);
        state
    }

    /// Setting on, the reply the OMEN 16 gives: the SET goes out, with
    /// the read bytes kept, the colours patched - zone 3 included - and
    /// the unread tail zero.
    #[test]
    fn with_the_setting_on_a_truncated_read_is_written() {
        let (fifo, fw) = fake_firmware(vec![pass_with(&truncated_state()), pass_with(&[])]);
        let red = Rgb::new(0xff, 0, 0);
        write_at(&fifo, &[red; 4], true).expect("allowed");
        let requests = fw.join().unwrap();
        assert_eq!(requests.len(), 2);
        let set = acpi::parse_bytes(requests[1].rsplit(' ').next().unwrap()).unwrap();
        assert_eq!(
            u32::from_le_bytes(set[8..12].try_into().unwrap()),
            COLOR_SET
        );
        let body = &set[16..];
        assert_eq!(body.len(), STATE_LEN);
        assert_eq!(body[0], 3, "a byte that was read goes back as read");
        for at in [25usize, 28, 31, 34] {
            assert_eq!(&body[at..at + 3], &[0xff, 0, 0], "zone at {at}");
        }
        assert!(
            body[37..].iter().all(|&b| b == 0),
            "the unread tail is zero"
        );
    }

    /// Setting off: the read happens, the write does not.
    #[test]
    fn with_the_setting_off_a_truncated_read_is_refused() {
        let (fifo, fw) = fake_firmware(vec![pass_with(&truncated_state())]);
        let refused = write_at(&fifo, &[Rgb::BLACK; 4], false);
        assert!(
            matches!(refused, Err(DialectError::Unsafe(_))),
            "{refused:?}"
        );
        assert_eq!(fw.join().unwrap().len(), 1, "only the GET went out");
    }

    /// Any other length, whatever the setting.
    #[test]
    fn a_read_of_any_other_length_is_refused_either_way() {
        for allow in [true, false] {
            let mut state = truncated_state();
            state.push(0);
            let (fifo, fw) = fake_firmware(vec![pass_with(&state)]);
            assert!(matches!(
                write_at(&fifo, &[Rgb::BLACK; 4], allow),
                Err(DialectError::Unsafe(_))
            ));
            assert_eq!(fw.join().unwrap().len(), 1, "only the GET went out");
        }
    }

    /// [`write_colors`] against an explicit interface path.
    fn write_at(path: &std::path::Path, colors: &[Rgb], allow: bool) -> Result<(), DialectError> {
        let _acpi = crate::testenv::redirect(path);
        write_colors(colors, allow)
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    /// The reply `acpi_call` actually handed this project on an OMEN 16:
    /// `PASS`, a zero return code, and then 34 bytes of state where the
    /// firmware sent 128. The three zones it reaches are real colours.
    const TRUNCATED: &str = "{0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00, \
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, \
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, \
        0x00, 0x0f, 0x84, 0xfa, 0x71, 0x0f, 0xfa, 0xf9, 0x35, 0x0f,";

    /// Reading three zones out of a cut-short reply is still fine: a read
    /// changes nothing.
    #[test]
    fn a_reply_that_stops_short_still_yields_the_zones_it_reached() {
        let state = reply::payload(TRUNCATED).expect("PASS with a zero return code");
        assert_eq!(state.len(), 34);
        let layout = &HP_FOURZONE_128;
        assert!(
            layout.check_shape(&state).is_ok(),
            "the bytes present have the shape"
        );
        assert_eq!(
            layout.colors(&state),
            vec![
                Rgb::new(0x0f, 0x84, 0xfa),
                Rgb::new(0x71, 0x0f, 0xfa),
                Rgb::new(0xf9, 0x35, 0x0f),
            ]
        );
    }

    /// And writing through it follows the setting: the real reply is the
    /// declared truncated length, in the layout's shape.
    #[test]
    fn the_same_reply_is_written_back_only_while_allowed() {
        let state = reply::payload(TRUNCATED).unwrap();
        assert_eq!(
            HP_FOURZONE_128.check_writable(&state, true).unwrap(),
            WriteMode::Truncated
        );
        match HP_FOURZONE_128.check_writable(&state, false) {
            Err(DialectError::Unsafe(m)) => {
                assert_eq!(m.key, "rgb.dialect.fourZone.truncatedOff");
                assert!(m.text.contains("34"), "{}", m.text);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_reply_that_reaches_no_zone_at_all_is_a_failure() {
        let stub = "{0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00}";
        let state = reply::payload(stub).expect("PASS");
        assert_eq!(HP_FOURZONE_128.zones_in(state.len()), 0);
    }
}
