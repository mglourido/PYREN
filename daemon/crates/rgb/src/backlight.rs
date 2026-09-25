//! The keyboard backlight switch - the one the Fn backlight key flips.
//!
//! ## Why this exists beside the colours
//!
//! The firmware keeps a backlight on/off flag that is separate from the
//! zone colours. The Fn key toggles that flag and nothing else, so a
//! keyboard switched off from the keypad stays dark whatever colours are
//! written: every dialect's writes land, and none of them light anything.
//! [`turn_on`] is what `forceBacklightOn` uses at daemon start to undo
//! that.
//!
//! ## Where these numbers come from
//!
//! OmenMon (`Hardware/BiosCtl.cs`, `BiosData.cs`), which drives the same
//! BIOS interface on Windows. Neither Linux reference driver uses them.
//!
//! ```text
//! command      0x20009   HPWMI_FOURZONE - the lighting command
//! commandtype  4         BACKLIGHT_GET   in 4, out 4
//!              5         BACKLIGHT_SET   in 4, out 0
//!
//! byte 0 of the 4-byte payload
//!   0xE4   on   (what OmenMon writes, and what a read gives back after it)
//!   0x64   off  (what OmenMon writes)
//! ```
//!
//! ## Confirmed against the hardware (2026-09-25)
//!
//! On the OMEN 16-am0xxx (board 8D2F), with the keyboard switched off by
//! the Fn key: a read gave `0x00`, not `0x64`; a write of `0xE4` lit the
//! keyboard; the read after it gave `0xE4`. Switched on again by the Fn
//! key, it reads `0xE4` too. Only those two states: the key cycles
//! off/on on this machine, with no levels in between.
//!
//! [`is_on`] tests bit 7 - the one bit `0xE4` and `0x64` differ in - rather
//! than the whole byte. A firmware with levels the driver's keymap hints at
//! would then still read as on, and a lit keyboard is never taken for a
//! dark one: that mistake would pause an effect somebody is looking at.
//!
//! The flag is a switch, not a level. Brightness stays in software, as
//! [`crate::scale`] explains.

use pyren_core::acpi;

use crate::dialect::DialectError;
use crate::fourzone::COMMAND;
use crate::reply;

/// `BACKLIGHT_GET`.
pub const GET: u32 = 4;
/// `BACKLIGHT_SET`.
pub const SET: u32 = 5;

/// The payload both command types take.
pub const PAYLOAD_LEN: usize = 4;

/// Byte 0 when the backlight is on.
pub const ON: u8 = 0xE4;
/// Byte 0 OmenMon writes to switch it off. Not written by this daemon;
/// kept so the protocol is written down in one place.
pub const OFF: u8 = 0x64;

/// Whether the firmware reports the backlight on.
pub fn read() -> Result<bool, DialectError> {
    let reply = acpi::wmi_call(COMMAND, GET, &[0u8; PAYLOAD_LEN], PAYLOAD_LEN, PAYLOAD_LEN)?;
    is_on(&reply::payload(&reply)?)
}

/// The bit that is set in [`ON`] and clear in [`OFF`].
pub const ON_BIT: u8 = 0x80;

/// Reads the flag from a `BACKLIGHT_GET` payload: on when bit 7 is set.
/// The test laptop reads `0x00` when the Fn key has put it out, not
/// [`OFF`]; both are off.
pub fn is_on(payload: &[u8]) -> Result<bool, DialectError> {
    match payload.first() {
        Some(&byte) => Ok(byte & ON_BIT != 0),
        None => Err(DialectError::Unreadable(
            "the backlight reply had no data".into(),
        )),
    }
}

/// Switches the backlight on if it is off. Returns whether it had to.
///
/// Read first so a keyboard that is already lit costs one call, and so
/// the log can say whether this did anything.
pub fn turn_on() -> Result<bool, DialectError> {
    if read()? {
        return Ok(false);
    }
    let reply = acpi::wmi_call(COMMAND, SET, &[ON, 0, 0, 0], PAYLOAD_LEN, 0)?;
    reply::payload(&reply)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three payloads seen on the test laptop, and a level nobody has
    /// seen, which must not read as off.
    #[test]
    fn bit_7_is_on() {
        assert!(is_on(&[0xE4, 0, 0, 0]).unwrap());
        assert!(is_on(&[0xA4, 0, 0, 0]).unwrap(), "an unseen level");
        assert!(!is_on(&[0x00, 0, 0, 0]).unwrap(), "Fn-key off");
        assert!(!is_on(&[0x64, 0, 0, 0]).unwrap(), "OmenMon's off");
        assert!(matches!(is_on(&[]), Err(DialectError::Unreadable(_))));
    }

    /// The exact buffers the hardware test sent, byte for byte.
    #[test]
    fn requests_match_the_ones_tried_on_the_hardware() {
        assert_eq!(
            acpi::wmi_request(COMMAND, GET, PAYLOAD_LEN, &[0; 4]),
            "b5345435509000200040000000400000000000000"
        );
        assert_eq!(
            acpi::wmi_request(COMMAND, SET, PAYLOAD_LEN, &[ON, 0, 0, 0]),
            "b53454355090002000500000004000000e4000000"
        );
        assert_eq!(acpi::method_for_outsize(PAYLOAD_LEN), 2);
        assert_eq!(acpi::method_for_outsize(0), 1);
    }
}
