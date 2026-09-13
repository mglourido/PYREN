//! The light strip, over ACPI-WMI: **one of three dialects**, and the
//! first one this project shipped. Which of the three a machine speaks is
//! decided by [`crate::dialect`], not here.
//!
//! Since this was written, the command type it sends has been identified
//! in a second, independent source: `OmenLinux/omen-rgb-keyboard`'s header
//! names `HPWMI_SET_LIGHTBAR_COLORS = 11` under the same `0x20009`
//! lighting command, which is exactly what upstream's reverse engineering
//! arrived at. That corroborates the **write**. Nothing corroborates the
//! *read* below - `0x20008` command type 4 is upstream's alone, and no
//! published driver reads the strip at all - which is why this dialect can
//! fail to probe on a machine whose strip it could still drive. That is
//! the case the manual override exists for.
//!
//! Ported from `src/lightbar.py` in `omen-rgb-linux`
//! (arfelious, GPL-3.0). Three upstream bugs are fixed here rather than
//! carried over, and each fix is commented where it lands; the reasoning
//! is in `dev/FINDINGS.md` §"The RGB project has two unrelated hardware
//! paths".
//!
//! ## The protocol
//!
//! One ACPI method, `\_SB.WMID.WMAA`, called as `<method> 0 3 b<hex>`:
//! argument 0 is the instance, 3 selects the buffer-taking method, and the
//! buffer is a 16-byte header followed by 128 bytes of payload.
//!
//! ```text
//! header (16 bytes, little-endian)
//!   0..4    "SECU"       signature
//!   4..8    command      0x20009 write, 0x20008 read
//!   8..12   command type 0x0b write, 0x04 read
//!   12..16  size         128, the payload that follows
//!
//! payload (128 bytes)
//!   0       target device / zone index   (0 = the lightbar; the zone to
//!                                         read, on a read)
//!   1       mode        0 = static
//!   2       config      0 = static
//!   3       brightness  0-100
//!   4       tribe       0
//!   5       bass        0
//!   6       zone count  4
//!   7..19   zone 1-4 RGB, three bytes each
//!   19..128 zero
//! ```
//!
//! The firmware answers `PASS` and a zero return code on success, read by
//! [`crate::reply`] like every other WMI dialect's reply. Anything else -
//! a non-zero code, `PASS` somewhere other than the start, an `acpi_call`
//! error string - is a refusal.
//!
//! ## What is not known
//!
//! Every constant above is reverse-engineered upstream, and **none of it
//! has been confirmed against hardware by this project**: the development
//! laptop has no `acpi_call` installed (`dev/FINDINGS.md` §"The test laptop
//! has no per-key RGB keyboard"). The parts that can be tested without the
//! hardware - the buffer this builds, and the replies it accepts - are
//! tested below, so that when someone does install `acpi_call-dkms` the
//! only untested thing left is the firmware's own answer.

use pyren_core::acpi;

use crate::dialect::DialectError;

use crate::color::Rgb;

use crate::ZONES;

/// Finding 3 of the review: upstream's `_detect_acpi_path` has two
/// branches that return the same string, so it reads as a probe and is a
/// constant. It is a constant here, plainly - and it has since moved to
/// [`acpi::WMI_METHOD`], because all three dialects and the fan cleaner
/// call the same one.
///
pub const PAYLOAD_LEN: usize = 128;
pub const COMMAND_WRITE: u32 = 0x0002_0009;
pub const COMMAND_READ: u32 = 0x0002_0008;
pub const TYPE_WRITE: u32 = 0x0b;
/// `HPWMI_GET_LIGHTBAR_COLORS`, upstream's alone - see the module header
/// on what is and is not corroborated.
pub const TYPE_READ: u32 = 0x04;

/// Where the brightness percentage sits in the payload. The one field
/// this dialect has that the other two do not, which makes it the only
/// place the firmware's *own* brightness could be legible - see
/// `raw_read`.
pub const BRIGHTNESS_OFFSET: usize = 3;

/// Brightness is a percentage in this protocol, not a 0-255 level.
pub fn clamp_brightness(value: i64) -> u8 {
    value.clamp(0, 100) as u8
}

/// The 144-byte buffer for a write, as the hex argument `acpi_call` takes.
pub fn write_request(colors: &[Rgb], brightness: u8) -> String {
    acpi::wmi_request(
        COMMAND_WRITE,
        TYPE_WRITE,
        PAYLOAD_LEN,
        &payload_for(colors, brightness),
    )
}

/// The 128 payload bytes of a write.
pub fn payload_for(colors: &[Rgb], brightness: u8) -> [u8; PAYLOAD_LEN] {
    let mut payload = [0u8; PAYLOAD_LEN];
    payload[0] = 0; // target device: the lightbar
    payload[1] = 0; // mode: static
    payload[2] = 0; // config: static
    payload[3] = brightness.min(100);
    payload[4] = 0; // tribe
    payload[5] = 0; // bass
    payload[6] = ZONES as u8;

    // Short of four zones, the rest stay black; past four, the extras are
    // dropped - the firmware reads exactly twelve bytes here and a
    // thirteenth would land on a field that means something else.
    for (zone, color) in colors.iter().take(ZONES).enumerate() {
        let at = 7 + zone * 3;
        payload[at] = color.r;
        payload[at + 1] = color.g;
        payload[at + 2] = color.b;
    }

    payload
}

/// The buffer for reading one zone back. Zone index goes in the first
/// payload byte, where a write puts the target device.
pub fn read_request(zone: usize) -> String {
    let mut payload = [0u8; PAYLOAD_LEN];
    payload[0] = zone as u8;
    acpi::wmi_request(COMMAND_READ, TYPE_READ, PAYLOAD_LEN, &payload)
}

/// Whether a reply means the firmware did the thing: `PASS` at the start
/// and a zero return code, in any of the shapes `acpi_call` renders a
/// buffer in.
///
/// Upstream accepts the letters anywhere in the reply. That is how this
/// dialect used to "answer" on four-zone machines whose firmware said
/// *unknown operation* - so the code is read now, the same way
/// [`crate::fourzone`] reads it.
pub fn is_success(response: &str) -> bool {
    parse_bytes(response).is_some_and(|bytes| crate::reply::checked(&bytes).is_ok())
}

/// The bytes behind an `acpi_call` reply.
///
/// **Finding 2 of the review landed here** - upstream's
/// `clean_res.lstrip("b0x")` takes a character set rather than a prefix and
/// eats real data bytes - and then moved: the fan cleaner speaks the same
/// protocol, so the parser is [`acpi::parse_bytes`] and there is one copy
/// of it. Re-exported because this module's callers and tests read replies
/// through the lightbar.
pub use acpi::parse_bytes;

/// The RGB triple in a single-zone read reply: the first three data bytes
/// after the `PASS` header, and only when that header says it worked.
pub fn zone_color(reply: &[u8]) -> Option<Rgb> {
    let data = crate::reply::checked(reply).ok()?;
    let triple = data.get(0..3)?;
    Some(Rgb::new(triple[0], triple[1], triple[2]))
}

// --- the hardware ------------------------------------------------------

/// Sends one write. Goes through [`acpi::wmi_call`], which holds the
/// cross-module lock over the write/read pair - **finding 4 of the
/// review**, and the reason that lock is in `core` rather than here.
///
/// Brightness is a real field in this dialect's payload, so it goes to the
/// firmware rather than being scaled into the colours the way the other
/// two dialects have to do it.
pub fn write_colors(colors: &[Rgb], brightness: u8) -> Result<(), DialectError> {
    let reply = acpi::wmi_call(
        COMMAND_WRITE,
        TYPE_WRITE,
        &payload_for(colors, brightness),
        PAYLOAD_LEN,
        PAYLOAD_LEN,
    )?;
    crate::reply::payload(&reply).map(|_| ())
}

/// One zone read, returned whole.
///
/// [`read_colors`] throws away all but three bytes of this. The rest is
/// the only candidate this project has for reading the firmware's own
/// brightness - the level the laptop's backlight key moves without
/// telling the kernel anything - so it is reachable on its own.
pub fn raw_read(zone: usize) -> Result<Vec<u8>, DialectError> {
    let mut payload = [0u8; PAYLOAD_LEN];
    payload[0] = zone as u8;
    let reply = acpi::wmi_call(COMMAND_READ, TYPE_READ, &payload, PAYLOAD_LEN, PAYLOAD_LEN)?;
    crate::reply::payload(&reply)?;
    acpi::parse_bytes(&reply).ok_or(DialectError::Unreadable(reply))
}

/// Reads the four zones back out of the firmware, one call per zone.
///
/// Unlike the per-key keyboard - whose HID lighting interface is
/// write-only, so its `get_colors` returns the driver's own buffer - this
/// really does ask the hardware. Worth saying out loud, because the two
/// paths having the same method name on the same module would otherwise
/// imply they answer the same question.
pub fn read_colors() -> Result<Vec<Rgb>, DialectError> {
    let mut colors = Vec::with_capacity(ZONES);
    for zone in 0..ZONES {
        let mut payload = [0u8; PAYLOAD_LEN];
        payload[0] = zone as u8;
        let reply = acpi::wmi_call(COMMAND_READ, TYPE_READ, &payload, PAYLOAD_LEN, PAYLOAD_LEN)?;
        crate::reply::payload(&reply)?;
        let bytes =
            acpi::parse_bytes(&reply).ok_or_else(|| DialectError::Unreadable(reply.clone()))?;
        colors.push(zone_color(&bytes).ok_or(DialectError::Unreadable(reply))?);
    }
    Ok(colors)
}

pub fn hp_wmi_present() -> bool {
    std::path::Path::new("/sys/devices/platform/hp-wmi").exists()
}

/// Whether the firmware says HP made this machine. `hp-wmi` binding is not
/// proof on its own - the driver is matched by WMI GUID - and every buffer
/// the WMI dialects send is an HP one.
pub fn is_hp() -> bool {
    std::fs::read_to_string("/sys/class/dmi/id/sys_vendor").is_ok_and(|v| vendor_is_hp(&v))
}

fn vendor_is_hp(vendor: &str) -> bool {
    let vendor = vendor.trim();
    vendor == "HP" || vendor.starts_with("HP ") || vendor.starts_with("Hewlett")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(request: &str) -> Vec<u8> {
        assert!(request.starts_with('b'), "acpi_call buffers start with b");
        parse_bytes(request).expect("the request must be plain hex")
    }

    /// The header is the part no test on hardware could isolate: if it is
    /// wrong the firmware simply refuses, and every field looks equally
    /// guilty.
    #[test]
    fn a_write_carries_the_signature_command_and_size() {
        let buffer = bytes_of(&write_request(&[Rgb::new(1, 2, 3)], 100));

        assert_eq!(buffer.len(), 16 + 128);
        assert_eq!(&buffer[0..4], b"SECU");
        assert_eq!(
            u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
            0x20009
        );
        assert_eq!(u32::from_le_bytes(buffer[8..12].try_into().unwrap()), 0x0b);
        assert_eq!(u32::from_le_bytes(buffer[12..16].try_into().unwrap()), 128);
    }

    #[test]
    fn the_four_zones_land_where_the_firmware_reads_them() {
        let colors = [
            Rgb::new(255, 0, 0),
            Rgb::new(0, 255, 0),
            Rgb::new(0, 0, 255),
            Rgb::new(255, 255, 0),
        ];
        let payload = bytes_of(&write_request(&colors, 80))[16..].to_vec();

        assert_eq!(payload[3], 80, "brightness");
        assert_eq!(payload[6], 4, "zone count");
        assert_eq!(
            &payload[7..19],
            &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]
        );
        assert!(payload[19..].iter().all(|&b| b == 0), "the tail is padding");
    }

    /// Fewer than four colours is a caller being terse, not an error; more
    /// than four would write over the byte after the zone block.
    #[test]
    fn a_short_or_long_list_of_zones_still_fills_exactly_four() {
        let short = bytes_of(&write_request(&[Rgb::new(9, 9, 9)], 100))[16..].to_vec();
        assert_eq!(&short[7..10], &[9, 9, 9]);
        assert!(
            short[10..19].iter().all(|&b| b == 0),
            "zones 2-4 stay black"
        );

        let long = bytes_of(&write_request(&[Rgb::new(1, 1, 1); 6], 100))[16..].to_vec();
        assert_eq!(&long[7..19], &[1u8; 12]);
        assert_eq!(long[19], 0, "the fifth zone must not exist");
    }

    #[test]
    fn brightness_is_a_percentage_not_a_level() {
        assert_eq!(clamp_brightness(400), 100);
        assert_eq!(clamp_brightness(-3), 0);
        assert_eq!(bytes_of(&write_request(&[], 255))[16 + 3], 100);
    }

    #[test]
    fn a_read_asks_for_one_zone_and_writes_no_colour() {
        let buffer = bytes_of(&read_request(2));
        assert_eq!(
            u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
            0x20008
        );
        assert_eq!(u32::from_le_bytes(buffer[8..12].try_into().unwrap()), 0x04);
        assert_eq!(buffer[16], 2, "the zone index");
        assert!(
            buffer[17..].iter().all(|&b| b == 0),
            "a read carries no payload"
        );
    }

    #[test]
    fn every_shape_of_pass_the_firmware_can_answer_in_is_a_success() {
        assert!(is_success("0x5041535300000000"));
        assert!(is_success(
            "{0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00}"
        ));
        assert!(is_success("{0x50,0x41,0x53,0x53,0x00,0x00,0x00,0x00}"));

        assert!(!is_success(""));
        assert!(!is_success("PASS"), "the letters are not a reply frame");
        assert!(
            !is_success("0x50415353"),
            "no return code is not a zero one"
        );
        assert!(!is_success("Error: AE_NOT_FOUND"));
        assert!(!is_success("{0x46, 0x41, 0x49, 0x4c}"), "FAIL is not PASS");
        assert!(
            !is_success("{0x50, 0x41, 0x53, 0x53, 0x04, 0x00, 0x00, 0x00}"),
            "unknown operation is a refusal, whatever it starts with"
        );
    }

    #[test]
    fn only_hp_is_hp() {
        assert!(vendor_is_hp("HP\n"));
        assert!(vendor_is_hp("Hewlett-Packard"));
        assert!(!vendor_is_hp("LENOVO"));
        assert!(!vendor_is_hp("CHPC Inc"));
        assert!(!vendor_is_hp(""));
    }

    /// Finding 2 of the review, as a test. `lstrip("b0x")` removes every
    /// leading `b`, `0` or `x`, so both of these lose real data: the first
    /// three bytes, and a leading zero byte.
    #[test]
    fn stripping_the_prefix_does_not_eat_data_bytes() {
        assert_eq!(parse_bytes("0xb0b0aa").unwrap(), vec![0xb0, 0xb0, 0xaa]);
        assert_eq!(
            parse_bytes("0x0050415353").unwrap(),
            vec![0x00, 0x50, 0x41, 0x53, 0x53]
        );
    }

    #[test]
    fn a_token_list_and_a_single_blob_both_read_back_as_bytes() {
        assert_eq!(
            parse_bytes("{0x50, 0x41, 0x53, 0x53}").unwrap(),
            b"PASS".to_vec()
        );
        assert_eq!(parse_bytes("b50415353").unwrap(), b"PASS".to_vec());
        // A blob wide enough that its `0x…` run is not a byte falls
        // through to being read as a blob, not as one huge token.
        assert_eq!(parse_bytes("0x505050505050").unwrap(), vec![0x50; 6]);
    }

    #[test]
    fn garbage_is_none_rather_than_a_guess() {
        assert!(parse_bytes("").is_none());
        assert!(parse_bytes("Error: AE_NOT_FOUND").is_none());
        assert!(
            parse_bytes("0x5041535").is_none(),
            "an odd number of digits is not bytes"
        );
    }

    #[test]
    fn a_zone_colour_is_the_three_bytes_after_the_header() {
        let mut reply = b"PASS\x00\x00\x00\x00".to_vec();
        reply.extend_from_slice(&[0x11, 0x22, 0x33]);
        assert_eq!(zone_color(&reply), Some(Rgb::new(0x11, 0x22, 0x33)));

        let mut shifted = b"\x00\x00".to_vec();
        shifted.extend_from_slice(&reply);
        assert_eq!(zone_color(&shifted), None, "PASS must be at the start");

        let mut refused = b"PASS\x04\x00\x00\x00".to_vec();
        refused.extend_from_slice(&[0x11, 0x22, 0x33]);
        assert_eq!(zone_color(&refused), None, "a return code is not a colour");

        assert_eq!(
            zone_color(b"PASS"),
            None,
            "a truncated reply is not a colour"
        );
        assert_eq!(zone_color(b"nothing here"), None);
    }
}
