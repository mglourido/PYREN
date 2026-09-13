//! Reading what the HP WMI firmware answered - one parser for every dialect
//! that speaks over `acpi_call`.
//!
//! The frame is the kernel's `struct bios_return`: four bytes of signature
//! echo - `PASS` when it worked - then a little-endian return code, then
//! the data. Both halves are checked, **at offset 0**: a `PASS` somewhere
//! further along, or a `PASS` followed by a non-zero code, is not success.
//! The lightbar used to accept the letters anywhere in the reply, which is
//! how a firmware answering "unknown operation" read as a dialect that
//! works.

use pyren_core::acpi;

use crate::dialect::DialectError;

/// The success sentinel, as the firmware returns it.
pub const PASS: &[u8; 4] = b"PASS";

/// The header in front of the data: the sentinel and the return code.
pub const HEADER_LEN: usize = 8;

/// The data behind a reply, once the firmware has said it worked.
///
/// A non-zero return code is reported with the code, because the codes are
/// documented and each sends you somewhere different: `3` unknown command,
/// `4` unknown command type, `5` bad parameters.
pub fn payload(reply: &str) -> Result<Vec<u8>, DialectError> {
    let bytes =
        acpi::parse_bytes(reply).ok_or_else(|| DialectError::Refused(reply.trim().to_string()))?;
    let data = checked(&bytes).map_err(|e| match e {
        DialectError::Refused(_) => DialectError::Refused(reply.trim().to_string()),
        other => other,
    })?;
    Ok(data.to_vec())
}

/// [`payload`] for bytes already parsed.
pub fn checked(bytes: &[u8]) -> Result<&[u8], DialectError> {
    if bytes.len() < HEADER_LEN || &bytes[0..4] != PASS {
        return Err(DialectError::Refused(format!("{bytes:02x?}")));
    }
    let code = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if code != 0 {
        return Err(DialectError::ReturnCode(code));
    }
    Ok(&bytes[HEADER_LEN..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PASS` and a zero return code, or it did not work. The three codes
    /// below are the documented ones and each has to survive as a code
    /// rather than becoming a generic refusal.
    #[test]
    fn a_pass_with_a_return_code_is_still_a_refusal() {
        let ok = "{0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00, 0xff, 0x99, 0x00}";
        assert_eq!(payload(ok).unwrap(), vec![0xff, 0x99, 0x00]);

        for (code, hex) in [(3u32, "03"), (4, "04"), (5, "05")] {
            let bad = format!("{{0x50, 0x41, 0x53, 0x53, 0x{hex}, 0x00, 0x00, 0x00}}");
            match payload(&bad) {
                Err(DialectError::ReturnCode(got)) => assert_eq!(got, code),
                other => panic!("expected return code {code}, got {other:?}"),
            }
        }

        for bad in [
            "",
            "FAIL",
            "PASS",
            "0x50415353",
            "{0x46, 0x41, 0x49, 0x4c}",
            "Error: AE_NOT_FOUND",
        ] {
            assert!(
                matches!(payload(bad), Err(DialectError::Refused(_))),
                "{bad:?}"
            );
        }
    }

    /// The old lightbar check: the letters anywhere counted. A reply with
    /// something in front of them is not the frame this parser reads.
    #[test]
    fn pass_somewhere_other_than_the_start_is_not_success() {
        let shifted = "{0x00, 0x50, 0x41, 0x53, 0x53, 0x00, 0x00, 0x00, 0x00}";
        assert!(matches!(payload(shifted), Err(DialectError::Refused(_))));
    }
}
