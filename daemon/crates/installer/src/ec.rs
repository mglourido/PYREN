//! Reading the embedded controller, so a board-params variant is decided by
//! measurement rather than guessed from a model name.
//!
//! The four variants in `hp-wmi.c` differ in one thing that cannot be read
//! from DMI: which EC byte holds the current thermal profile. Upstream
//! answers it per board, by hand, from someone with the hardware in front
//! of them. This does the same thing without the hand: read the two
//! candidate offsets and see which one is holding a value the driver's own
//! code path would recognise.
//!
//! **Reads only.** `ec_sys` is loaded read-only (its `write_support`
//! parameter defaults off) and nothing here opens the node for writing. An
//! EC register read is what `ec_read()` does in the driver on every profile
//! query, so this is not a novel thing to do to the hardware - it is the
//! same read, from userspace, before deciding what to compile.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

use serde::Serialize;

/// `ec_sys`'s window onto EC register space, one byte per address.
const EC_IO: &str = "/sys/kernel/debug/ec/ec0/io";

/// The offsets `hp-wmi.c`'s `enum hp_ec_offsets` gives the two code paths
/// that read a thermal profile back from the EC.
pub const VICTUS_S_THERMAL_OFFSET: u64 = 0x59;
pub const OMEN_THERMAL_OFFSET: u64 = 0x95;

/// What `enum hp_thermal_profile_omen_v1` can hold: default, performance,
/// cool. A byte outside this set is not a thermal profile, so an offset
/// holding one is not the offset this board keeps it at.
const OMEN_V1_PROFILE_VALUES: [u8; 3] = [0x30, 0x31, 0x50];

/// Why the EC could not be read, or what it said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum EcProbe {
    /// Both candidate offsets, as the EC currently holds them.
    Read { victus_s: u8, omen: u8 },
    /// `ec_sys` is not loaded and loading it was not asked for.
    ModuleNotLoaded,
    /// `modprobe ec_sys` was tried and failed - the kernel may be built
    /// without `CONFIG_ACPI_EC_DEBUGFS`.
    Unavailable { reason: String },
    /// The node exists but could not be read; the daemon is not root.
    NotPermitted,
    /// Not asked for.
    NotProbed,
}

impl EcProbe {
    /// Reads both offsets, optionally loading `ec_sys` first.
    ///
    /// `load_module` is a parameter rather than an assumption because
    /// `installer.autodetect` is otherwise a pure read of things that are
    /// already there, and loading a kernel module is not that - even a
    /// harmless one. The wizard passes it because clicking "install" is the
    /// authorisation.
    pub fn detect(load_module: bool) -> Self {
        if !Path::new(EC_IO).exists() {
            if !load_module {
                return Self::ModuleNotLoaded;
            }
            match Command::new("modprobe").arg("ec_sys").output() {
                Ok(output) if output.status.success() => {}
                Ok(output) => {
                    return Self::Unavailable {
                        reason: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                    }
                }
                Err(e) => return Self::Unavailable { reason: e.to_string() },
            }
            if !Path::new(EC_IO).exists() {
                return Self::Unavailable {
                    reason: format!("{EC_IO} still absent after loading ec_sys"),
                };
            }
        }

        match (read_byte(VICTUS_S_THERMAL_OFFSET), read_byte(OMEN_THERMAL_OFFSET)) {
            (Some(victus_s), Some(omen)) => Self::Read { victus_s, omen },
            _ => Self::NotPermitted,
        }
    }

    /// Which offset is holding something the OMEN v1 path would recognise
    /// as a thermal profile.
    ///
    /// `None` when neither is, which is a real answer: it means this board
    /// keeps its profile somewhere else, or nowhere the driver can see, and
    /// the variant that reads no EC byte at all is then the right one.
    pub fn omen_offset_in_use(&self) -> Option<u64> {
        let Self::Read { victus_s, omen } = self else { return None };
        let plausible = |byte: &u8| OMEN_V1_PROFILE_VALUES.contains(byte);

        // Checked in this order because a board that answers at both is
        // more likely the classic OMEN layout - 0x95 is the offset the
        // driver's own omen path reads unconditionally.
        match (plausible(omen), plausible(victus_s)) {
            (true, _) => Some(OMEN_THERMAL_OFFSET),
            (false, true) => Some(VICTUS_S_THERMAL_OFFSET),
            (false, false) => None,
        }
    }
}

fn read_byte(offset: u64) -> Option<u8> {
    let mut file = File::open(EC_IO).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).ok()?;
    Some(byte[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offsets_match_the_drivers_own_enum() {
        // `enum hp_ec_offsets` in hp-wmi.c. If upstream moves them, the
        // probe would be reading two unrelated bytes and confidently
        // deciding from them.
        assert_eq!(VICTUS_S_THERMAL_OFFSET, 0x59);
        assert_eq!(OMEN_THERMAL_OFFSET, 0x95);
    }

    #[test]
    fn a_profile_value_at_the_omen_offset_names_that_offset() {
        let probe = EcProbe::Read { victus_s: 0x00, omen: 0x31 };
        assert_eq!(probe.omen_offset_in_use(), Some(OMEN_THERMAL_OFFSET));
    }

    #[test]
    fn a_profile_value_at_the_victus_s_offset_names_that_one() {
        let probe = EcProbe::Read { victus_s: 0x30, omen: 0x07 };
        assert_eq!(probe.omen_offset_in_use(), Some(VICTUS_S_THERMAL_OFFSET));
    }

    /// The answer that matters most: neither byte holds a thermal profile,
    /// so no offset should be claimed. Guessing one here is how a driver
    /// ends up reading an unrelated EC register and reporting it as the
    /// machine's performance mode.
    #[test]
    fn neither_offset_holding_a_profile_is_an_answer_not_a_tie() {
        let probe = EcProbe::Read { victus_s: 0x07, omen: 0xa2 };
        assert_eq!(probe.omen_offset_in_use(), None);
    }

    #[test]
    fn an_unread_ec_claims_nothing() {
        for probe in [
            EcProbe::ModuleNotLoaded,
            EcProbe::NotPermitted,
            EcProbe::NotProbed,
            EcProbe::Unavailable { reason: "no such module".into() },
        ] {
            assert_eq!(probe.omen_offset_in_use(), None);
        }
    }

    /// 0x00 and 0x01 are the Victus S profile values, and they are also two
    /// of the commonest bytes in EC space - so they are deliberately not in
    /// the plausible set. Matching on them would name an offset from noise.
    #[test]
    fn the_victus_profile_values_are_not_treated_as_evidence() {
        let probe = EcProbe::Read { victus_s: 0x01, omen: 0x00 };
        assert_eq!(probe.omen_offset_in_use(), None);
    }
}
