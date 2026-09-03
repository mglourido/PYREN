//! The 4-zone OMEN light strip, over ACPI-WMI.
//!
//! Ported from `src/lightbar.py` in `omen-rgb-linux`
//! (arfelious, GPL-3.0). The review that preceded this port is
//! `docs/04-rgb-porting-review.md`; three of its findings are fixed here
//! rather than carried over, and each fix is commented where it lands.
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
//! The firmware answers with the four bytes `PASS` on success. Anything
//! else - including an `acpi_call` error string - is a refusal.
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

use crate::color::Rgb;

/// The lightbar has four zones. Not a configurable number: the payload's
/// zone-count byte and its 12 bytes of colour are what the firmware reads.
pub const ZONES: usize = 4;

/// Finding 3 of the review: upstream's `_detect_acpi_path` has two
/// branches that return the same string, so it reads as a probe and is a
/// constant. It is a constant here, plainly.
///
/// If a machine ever turns up needing a different path, this becomes a
/// probe with branches that differ - which is the thing the dead code was
/// pretending to be.
const METHOD: &str = "\\_SB.WMID.WMAA";

const SIGNATURE: &[u8; 4] = b"SECU";
const PAYLOAD_LEN: usize = 128;
const COMMAND_WRITE: u32 = 0x0002_0009;
const COMMAND_READ: u32 = 0x0002_0008;
const TYPE_WRITE: u32 = 0x0b;
const TYPE_READ: u32 = 0x04;

/// The success sentinel, `PASS`, as the firmware returns it.
const PASS: &[u8; 4] = b"PASS";

#[derive(Debug, thiserror::Error)]
pub enum LightbarError {
    #[error(transparent)]
    Acpi(#[from] acpi::AcpiError),
    /// The call went through and the firmware said no. On a machine with
    /// no light strip this is the normal answer, which is why it is what
    /// [`is_present`] tests.
    #[error("the firmware refused the lightbar call (it answered: {0})")]
    Refused(String),
    /// It said `PASS` and then the bytes made no sense.
    #[error("the firmware answered {0}, which is not a lightbar reply")]
    Unreadable(String),
}

/// Brightness is a percentage in this protocol, not a 0-255 level.
pub fn clamp_brightness(value: i64) -> u8 {
    value.clamp(0, 100) as u8
}

/// The 144-byte buffer for a write, as the hex argument `acpi_call` takes.
pub fn write_request(colors: &[Rgb], brightness: u8) -> String {
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

    encode(&header(COMMAND_WRITE, TYPE_WRITE), &payload)
}

/// The buffer for reading one zone back. Zone index goes in the first
/// payload byte, where a write puts the target device.
pub fn read_request(zone: usize) -> String {
    let mut payload = [0u8; PAYLOAD_LEN];
    payload[0] = zone as u8;
    encode(&header(COMMAND_READ, TYPE_READ), &payload)
}

fn header(command: u32, command_type: u32) -> [u8; 16] {
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(SIGNATURE);
    header[4..8].copy_from_slice(&command.to_le_bytes());
    header[8..12].copy_from_slice(&command_type.to_le_bytes());
    header[12..16].copy_from_slice(&(PAYLOAD_LEN as u32).to_le_bytes());
    header
}

/// `acpi_call` takes a buffer argument as `b` followed by plain hex.
fn encode(header: &[u8; 16], payload: &[u8; PAYLOAD_LEN]) -> String {
    let mut hex = String::with_capacity(1 + (16 + PAYLOAD_LEN) * 2);
    hex.push('b');
    for byte in header.iter().chain(payload.iter()) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Whether a reply means the firmware did the thing.
///
/// The four shapes are the ones `acpi_call` actually produces for a buffer
/// return, and upstream accepts all of them: a bare hex blob, a
/// `{0x50, 0x41, ...}` list, and the same list without spaces.
pub fn is_success(response: &str) -> bool {
    let upper = response.to_ascii_uppercase();
    if upper.contains("50415353") || upper.contains("PASS") {
        return true;
    }
    match parse_bytes(response) {
        Some(bytes) => bytes.starts_with(PASS),
        None => false,
    }
}

/// The bytes behind an `acpi_call` reply.
///
/// **Finding 2 of the review lands here.** Upstream strips the prefix with
/// `clean_res.lstrip("b0x")`, and `str.lstrip` takes a *character set*, not
/// a prefix: `'0xb0b0aa'.lstrip('b0x')` is `'aa'`, three bytes of real data
/// gone, and any reply whose first byte is zero loses that byte too. This
/// strips the prefix once, which is what was meant.
pub fn parse_bytes(response: &str) -> Option<Vec<u8>> {
    let text = response.trim();
    if text.is_empty() {
        return None;
    }

    // A `{0x50, 0x41, ...}` list. Every token has to fit in a byte, or
    // this is not a list of bytes - it is one long blob that happens to
    // start `0x`, and the branch below is the one that reads it.
    let tokens = hex_tokens(text);
    if !tokens.is_empty() {
        let parsed: Option<Vec<u8>> =
            tokens.iter().map(|t| u8::from_str_radix(t, 16).ok()).collect();
        if let Some(bytes) = parsed {
            return Some(bytes);
        }
    }

    let blob = text.trim_matches(|c| c == '{' || c == '}').trim();
    let blob = blob.strip_prefix("0x").or_else(|| blob.strip_prefix('b')).unwrap_or(blob);
    let blob: String = blob.chars().filter(|c| !c.is_whitespace()).collect();
    from_hex(&blob)
}

/// Every `0x…` run in the text, as its hex digits. Mirrors upstream's
/// `re.findall(r'0x[0-9a-fA-F]+', res)` without pulling in a regex crate
/// for one pattern.
fn hex_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '0' && (chars[i + 1] == 'x' || chars[i + 1] == 'X') {
            let start = i + 2;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_hexdigit() {
                end += 1;
            }
            if end > start {
                tokens.push(chars[start..end].iter().collect());
                i = end;
                continue;
            }
        }
        i += 1;
    }
    tokens
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = text
        .as_bytes()
        .chunks(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect();
    (bytes.len() == text.len() / 2).then_some(bytes)
}

/// The RGB triple in a single-zone read reply: four bytes past `PASS`,
/// then three bytes of colour.
pub fn zone_color(reply: &[u8]) -> Option<Rgb> {
    let at = find(reply, PASS)? + 8;
    let triple = reply.get(at..at + 3)?;
    Some(Rgb::new(triple[0], triple[1], triple[2]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

// --- the hardware ------------------------------------------------------

/// Sends one write. Goes through [`acpi::call`], which holds the
/// cross-module lock over the write/read pair - **finding 4 of the
/// review**, and the reason that lock is in `core` rather than here.
pub fn set_colors(colors: &[Rgb], brightness: u8) -> Result<(), LightbarError> {
    acpi::ensure_loaded()?;
    let reply = acpi::call(METHOD, &format!("0 3 {}", write_request(colors, brightness)))?;
    if is_success(&reply) {
        Ok(())
    } else {
        Err(LightbarError::Refused(reply))
    }
}

/// Reads the four zones back out of the firmware.
///
/// Unlike the per-key keyboard - whose HID lighting interface is
/// write-only, so its `get_colors` returns the driver's own buffer - this
/// really does ask the hardware. Worth saying out loud, because the two
/// paths having the same method name on the same module would otherwise
/// imply they answer the same question.
pub fn read_colors() -> Result<Vec<Rgb>, LightbarError> {
    acpi::ensure_loaded()?;
    let mut colors = Vec::with_capacity(ZONES);
    for zone in 0..ZONES {
        let reply = acpi::call(METHOD, &format!("0 3 {}", read_request(zone)))?;
        if !is_success(&reply) {
            return Err(LightbarError::Refused(reply));
        }
        let bytes = parse_bytes(&reply).ok_or_else(|| LightbarError::Unreadable(reply.clone()))?;
        colors.push(zone_color(&bytes).ok_or(LightbarError::Unreadable(reply))?);
    }
    Ok(colors)
}

/// What came back from putting the question to the firmware.
///
/// The third variant is the one that earns this being an enum rather than
/// a bool: **failing to ask is not being told no.** An unprivileged
/// process cannot write `/proc/acpi/call` at all, and reporting that as
/// "this machine has no light strip" would be a permanent-sounding verdict
/// on a machine that simply was not asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// It said `PASS`.
    Pass,
    /// The call completed and it said something else. On a machine with no
    /// light strip this is the normal answer.
    Refused,
    /// The call could not be made. Carries why.
    Unreachable(String),
}

/// Asks the firmware whether there is a lightbar here.
///
/// A *read* is what asks: it polls the current state without overwriting
/// it, so probing for a lightbar never changes one. Needs `acpi_call`
/// present already - probing does not load kernel modules, see
/// [`acpi::ensure_loaded`].
pub fn ask() -> Answer {
    match acpi::call(METHOD, &format!("0 3 {}", read_request(0))) {
        Ok(reply) if is_success(&reply) => Answer::Pass,
        Ok(_) => Answer::Refused,
        Err(e) => Answer::Unreachable(e.to_string()),
    }
}

/// Whether this machine has a light strip that answers.
pub fn is_present() -> bool {
    hp_wmi_present() && acpi::is_loaded() && ask() == Answer::Pass
}

pub fn hp_wmi_present() -> bool {
    std::path::Path::new("/sys/devices/platform/hp-wmi").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(request: &str) -> Vec<u8> {
        from_hex(request.strip_prefix('b').expect("acpi_call buffers start with b"))
            .expect("the request must be plain hex")
    }

    /// The header is the part no test on hardware could isolate: if it is
    /// wrong the firmware simply refuses, and every field looks equally
    /// guilty.
    #[test]
    fn a_write_carries_the_signature_command_and_size() {
        let buffer = bytes_of(&write_request(&[Rgb::new(1, 2, 3)], 100));

        assert_eq!(buffer.len(), 16 + 128);
        assert_eq!(&buffer[0..4], b"SECU");
        assert_eq!(u32::from_le_bytes(buffer[4..8].try_into().unwrap()), 0x20009);
        assert_eq!(u32::from_le_bytes(buffer[8..12].try_into().unwrap()), 0x0b);
        assert_eq!(u32::from_le_bytes(buffer[12..16].try_into().unwrap()), 128);
    }

    #[test]
    fn the_four_zones_land_where_the_firmware_reads_them() {
        let colors =
            [Rgb::new(255, 0, 0), Rgb::new(0, 255, 0), Rgb::new(0, 0, 255), Rgb::new(255, 255, 0)];
        let payload = bytes_of(&write_request(&colors, 80))[16..].to_vec();

        assert_eq!(payload[3], 80, "brightness");
        assert_eq!(payload[6], 4, "zone count");
        assert_eq!(&payload[7..19], &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0]);
        assert!(payload[19..].iter().all(|&b| b == 0), "the tail is padding");
    }

    /// Fewer than four colours is a caller being terse, not an error; more
    /// than four would write over the byte after the zone block.
    #[test]
    fn a_short_or_long_list_of_zones_still_fills_exactly_four() {
        let short = bytes_of(&write_request(&[Rgb::new(9, 9, 9)], 100))[16..].to_vec();
        assert_eq!(&short[7..10], &[9, 9, 9]);
        assert!(short[10..19].iter().all(|&b| b == 0), "zones 2-4 stay black");

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
        assert_eq!(u32::from_le_bytes(buffer[4..8].try_into().unwrap()), 0x20008);
        assert_eq!(u32::from_le_bytes(buffer[8..12].try_into().unwrap()), 0x04);
        assert_eq!(buffer[16], 2, "the zone index");
        assert!(buffer[17..].iter().all(|&b| b == 0), "a read carries no payload");
    }

    #[test]
    fn every_shape_of_pass_the_firmware_can_answer_in_is_a_success() {
        assert!(is_success("0x50415353"));
        assert!(is_success("{0x50, 0x41, 0x53, 0x53}"));
        assert!(is_success("{0x50,0x41,0x53,0x53}"));
        assert!(is_success("PASS"));

        assert!(!is_success(""));
        assert!(!is_success("Error: AE_NOT_FOUND"));
        assert!(!is_success("{0x46, 0x41, 0x49, 0x4c}"), "FAIL is not PASS");
    }

    /// Finding 2 of the review, as a test. `lstrip("b0x")` removes every
    /// leading `b`, `0` or `x`, so both of these lose real data: the first
    /// three bytes, and a leading zero byte.
    #[test]
    fn stripping_the_prefix_does_not_eat_data_bytes() {
        assert_eq!(parse_bytes("0xb0b0aa").unwrap(), vec![0xb0, 0xb0, 0xaa]);
        assert_eq!(parse_bytes("0x0050415353").unwrap(), vec![0x00, 0x50, 0x41, 0x53, 0x53]);
    }

    #[test]
    fn a_token_list_and_a_single_blob_both_read_back_as_bytes() {
        assert_eq!(parse_bytes("{0x50, 0x41, 0x53, 0x53}").unwrap(), b"PASS".to_vec());
        assert_eq!(parse_bytes("b50415353").unwrap(), b"PASS".to_vec());
        // A blob wide enough that its `0x…` run is not a byte falls
        // through to being read as a blob, not as one huge token.
        assert_eq!(parse_bytes("0x505050505050").unwrap(), vec![0x50; 6]);
    }

    #[test]
    fn garbage_is_none_rather_than_a_guess() {
        assert!(parse_bytes("").is_none());
        assert!(parse_bytes("Error: AE_NOT_FOUND").is_none());
        assert!(parse_bytes("0x5041535").is_none(), "an odd number of digits is not bytes");
    }

    #[test]
    fn a_zone_colour_is_the_three_bytes_four_past_pass() {
        let mut reply = b"\x00\x00PASS\x00\x00\x00\x00".to_vec();
        reply.extend_from_slice(&[0x11, 0x22, 0x33]);
        assert_eq!(zone_color(&reply), Some(Rgb::new(0x11, 0x22, 0x33)));

        assert_eq!(zone_color(b"PASS"), None, "a truncated reply is not a colour");
        assert_eq!(zone_color(b"nothing here"), None);
    }
}
