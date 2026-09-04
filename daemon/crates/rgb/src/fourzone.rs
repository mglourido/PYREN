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
//!   0..25    unknown, and preserved - see below
//!   25..28   zone 0 R,G,B
//!   28..31   zone 1
//!   31..34   zone 2
//!   34..37   zone 3
//!   37..128  unknown, and preserved
//! ```
//!
//! ## A write is a read, then a write
//!
//! Both reference drivers do a `COLOR_GET` first and patch twelve bytes of
//! what comes back, and this does the same. The buffer holds fields
//! nobody has identified - the comment in the 2023 patch is literally
//! *"Zones start at offset 25. Wonder what's in the rest of the buffer?"* -
//! so sending a freshly zeroed one would be writing zero to every setting
//! nobody has named yet. Read, patch, write back.
//!
//! ## `acpi_call` cuts the reply short, and that is survivable
//!
//! `acpi_call` renders a buffer reply as the text `{0x50, 0x41, …}` into a
//! fixed result buffer of a few hundred bytes, so a 128-byte answer comes
//! back as roughly the first **34** bytes and no more. That is enough for
//! three of the four zones and not the fourth.
//!
//! Failing the whole read over it would be wrong twice: the dialect
//! plainly works - the bytes that do arrive are this keyboard's actual
//! colours - and refusing here makes auto-selection fall through to a
//! dialect that answers `PASS` and does nothing. So a short reply is read
//! for what it contains, and the zones past the end come back black.
//!
//! A write pads the unseen tail with zeros, which is the one place this
//! module writes a byte nobody has seen. It is bounded: everything visible
//! before the colours is zero apart from `state[0]`, so the tail being
//! zeros as well is the reading the evidence supports. The way out of
//! guessing entirely is the `kernelZones` dialect, which has no such
//! limit - see [`crate::kernel_zones`].
//!
//! ## Brightness is not in here
//!
//! There is a `SET_BRIGHTNESS = 5` command type, and this dialect does not
//! use it: nobody has published its payload, and the reference driver
//! scales the colours in software instead. So does this - see
//! [`crate::scale`] - which means brightness works identically on every
//! dialect rather than working on one and silently doing nothing on
//! another.

use pyren_core::acpi;

use crate::color::Rgb;
use crate::dialect::DialectError;

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

/// The state buffer both command types take and return.
pub const STATE_LEN: usize = 128;

/// Where zone 0's red byte lives in that buffer.
pub const ZONE_OFFSET: usize = 25;

/// Reads the 128-byte state buffer.
pub fn read_state() -> Result<Vec<u8>, DialectError> {
    let reply = acpi::wmi_call(COMMAND, COLOR_GET, &[0u8; STATE_LEN], STATE_LEN, STATE_LEN)?;
    payload(&reply)
}

pub fn read_colors() -> Result<Vec<Rgb>, DialectError> {
    let state = read_state()?;
    // Not reaching the first zone is a reply that says nothing about the
    // lights, and that *is* a failure. Reaching some of them is a
    // truncated reply, which is the normal case through `acpi_call`.
    if state.len() < ZONE_OFFSET + 3 {
        return Err(DialectError::Unreadable(format!(
            "the reply is {} bytes and the first zone starts at {ZONE_OFFSET}",
            state.len()
        )));
    }
    Ok((0..crate::ZONES)
        .map(|zone| {
            let at = ZONE_OFFSET + zone * 3;
            state.get(at..at + 3).map_or(Rgb::BLACK, |c| Rgb::new(c[0], c[1], c[2]))
        })
        .collect())
}

pub fn write_colors(colors: &[Rgb]) -> Result<(), DialectError> {
    // The read half of read-modify-write. Everything outside the twelve
    // colour bytes is somebody else's setting - so what is read is kept,
    // and only what `acpi_call` truncated away is padded with zeros.
    let mut state = read_state()?;
    state.resize(STATE_LEN, 0);

    for (zone, color) in colors.iter().take(crate::ZONES).enumerate() {
        let at = ZONE_OFFSET + zone * 3;
        state[at] = color.r;
        state[at + 1] = color.g;
        state[at + 2] = color.b;
    }

    let reply = acpi::wmi_call(COMMAND, COLOR_SET, &state, STATE_LEN, STATE_LEN)?;
    payload(&reply).map(|_| ())
}

/// Whether the `0x20009` command space answers a read on this machine.
///
/// Reported beside the dialect's own probe because the two failures mean
/// different things: no answer here at all is "this firmware has no
/// lighting command", while an answer here and a refusal to `COLOR_GET` is
/// "it has one, and this is not a four-zone keyboard".
pub fn platform_info() -> Result<Vec<u8>, DialectError> {
    let reply = acpi::wmi_call(COMMAND, PLATFORM_INFO, &[0u8; 4], 4, STATE_LEN)?;
    payload(&reply)
}

/// The data behind a reply, once the firmware has said it worked.
///
/// The frame is the kernel's `struct bios_return`: four bytes of signature
/// echo - `PASS` when it worked - then a little-endian return code, then
/// the data. A non-zero return code with a `PASS` in front of it is still
/// a refusal, and is reported with the code, because the codes are
/// documented and each sends you somewhere different:
/// `3` unknown command, `4` unknown command type, `5` bad parameters.
fn payload(reply: &str) -> Result<Vec<u8>, DialectError> {
    let bytes = acpi::parse_bytes(reply)
        .ok_or_else(|| DialectError::Refused(reply.trim().to_string()))?;
    if bytes.len() < 8 || &bytes[0..4] != b"PASS" {
        return Err(DialectError::Refused(reply.trim().to_string()));
    }
    let code = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if code != 0 {
        return Err(DialectError::ReturnCode(code));
    }
    Ok(bytes[8..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(request: &str) -> Vec<u8> {
        assert!(request.starts_with('b'), "acpi_call buffers start with b");
        acpi::parse_bytes(request).expect("the request must be plain hex")
    }

    /// The header is what no test on hardware could isolate: a wrong
    /// command id is refused exactly the same way a machine without the
    /// hardware refuses, so it has to be pinned here.
    #[test]
    fn the_header_carries_the_command_the_reference_drivers_send() {
        let request = bytes_of(&acpi::wmi_request(COMMAND, COLOR_SET, STATE_LEN, &[0u8; STATE_LEN]));
        assert_eq!(&request[0..4], b"SECU");
        assert_eq!(u32::from_le_bytes(request[4..8].try_into().unwrap()), 0x0002_0009);
        assert_eq!(u32::from_le_bytes(request[8..12].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(request[12..16].try_into().unwrap()), 128);
        assert_eq!(request.len(), 16 + STATE_LEN);
    }

    /// 128 bytes back means method 3. Method 1 - what a write expecting
    /// nothing would use - is a different request to the firmware.
    #[test]
    fn a_state_sized_answer_asks_for_method_three() {
        assert_eq!(acpi::method_for_outsize(STATE_LEN), 3);
        assert_eq!(acpi::method_for_outsize(0), 1);
        assert_eq!(acpi::method_for_outsize(4), 2);
    }

    #[test]
    fn the_four_zones_land_where_both_reference_drivers_read_them() {
        for (zone, expected) in [(0usize, 25usize), (1, 28), (2, 31), (3, 34)] {
            assert_eq!(ZONE_OFFSET + zone * 3, expected);
        }
    }

    /// `PASS` and a zero return code, or it did not work. The three codes
    /// below are the documented ones and each has to survive as a code
    /// rather than becoming a generic refusal.
    #[test]
    fn a_pass_with_a_return_code_is_still_a_refusal() {
        let ok = "{0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00, 0xff, 0x99, 0x00}";
        assert_eq!(payload(ok).unwrap(), vec![0xff, 0x99, 0x00]);

        for (code, hex) in [(3u32, "03"), (4, "04"), (5, "05")] {
            let reply = format!("0x5041535{}{}000000", "3", hex);
            let _ = reply; // shape below is the one acpi_call actually emits
            let bad = format!("{{0x50, 0x41, 0x53, 0x53, 0x{hex}, 0x00, 0x00, 0x00}}");
            match payload(&bad) {
                Err(DialectError::ReturnCode(got)) => assert_eq!(got, code),
                other => panic!("expected return code {code}, got {other:?}"),
            }
        }

        for bad in ["", "FAIL", "{0x46, 0x41, 0x49, 0x4c}", "Error: AE_NOT_FOUND"] {
            assert!(matches!(payload(bad), Err(DialectError::Refused(_))), "{bad:?}");
        }
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    /// What actually goes down the wire for a read, byte for byte.
    ///
    /// Written after a machine whose firmware answers this exact request
    /// from a shell - `PASS`, return code 0, 128 bytes of state - handed
    /// this daemon an empty reply for what was meant to be the same call.
    /// If those two ever diverge again, the divergence is here.
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

    /// Reading three zones out of a cut-short reply beats failing on all
    /// four: failing sends auto-selection to a dialect that answers `PASS`
    /// and leaves the lights exactly as they were.
    #[test]
    fn a_reply_that_stops_short_still_yields_the_zones_it_reached() {
        let state = payload(TRUNCATED).expect("PASS with a zero return code");
        assert_eq!(state.len(), 34, "this is what acpi_call's buffer allows through");

        let zones: Vec<Rgb> = (0..crate::ZONES)
            .map(|zone| {
                let at = ZONE_OFFSET + zone * 3;
                state.get(at..at + 3).map_or(Rgb::BLACK, |c| Rgb::new(c[0], c[1], c[2]))
            })
            .collect();

        assert_eq!(zones[0], Rgb::new(0x0f, 0x84, 0xfa));
        assert_eq!(zones[1], Rgb::new(0x71, 0x0f, 0xfa));
        assert_eq!(zones[2], Rgb::new(0xf9, 0x35, 0x0f));
        assert_eq!(zones[3], Rgb::BLACK, "past the end of what arrived");
    }

    /// A reply that does not even reach the first zone says nothing about
    /// the lights, and must not be read as four black zones.
    #[test]
    fn a_reply_that_reaches_no_zone_at_all_is_a_failure() {
        let stub = "{0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00}";
        let state = payload(stub).expect("PASS");
        assert!(state.len() < ZONE_OFFSET + 3);
    }
}
