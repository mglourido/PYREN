//! Zone colours through the kernel's own sysfs files.
//!
//! Some builds of `hp-wmi` - and the out-of-tree modules that predate the
//! in-tree support - publish the four zones as
//! `/sys/devices/platform/<driver>/rgb_zones/zone00 … zone03`, each holding
//! one `RRGGBB` hex colour. Which `<driver>` depends on who published them:
//! a patched `hp-wmi` uses its own name, while `omen-rgb-keyboard` registers
//! a platform device under *its* name and hangs the same `rgb_zones` group
//! off that. Both are the same dialect, so this looks for either.
//!
//! This is the dialect to prefer wherever it exists, and the reason is not
//! taste: it is the only one of the three that does not need `acpi_call`,
//! does not hand-build a firmware buffer, and cannot send the firmware a
//! command it did not expect. Where the kernel has already done the
//! reverse engineering, doing it again in userspace is strictly worse.
//!
//! Brightness is **not** a field here. The kernel driver that owns these
//! files scales the colours in software instead, and so does this: see
//! [`crate::scale`].

use std::path::{Path, PathBuf};

use crate::color::Rgb;
use crate::dialect::DialectError;

/// Where the kernel might publish them, best-known first. Both entries are
/// the same interface under a different platform-device name - see the
/// module docs - so the first one that is actually there wins.
const ZONES_DIRS: [&str; 2] = [
    "/sys/devices/platform/hp-wmi/rgb_zones",
    "/sys/devices/platform/omen-rgb-keyboard/rgb_zones",
];

/// The directory to talk to. `PYREN_RGB_ZONES_DIR` overrides the search and
/// points this at a fixture directory, which is the only way to exercise the
/// dialect on a machine whose kernel does not expose it.
///
/// With nothing found, this answers the first candidate rather than nothing:
/// the caller is then about to fail, and a failure that names a path reads
/// better than one that cannot say where it looked.
pub fn dir() -> PathBuf {
    if let Ok(from_env) = std::env::var("PYREN_RGB_ZONES_DIR") {
        return PathBuf::from(from_env);
    }
    ZONES_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|dir| dir.join("zone00").exists())
        .unwrap_or_else(|| PathBuf::from(ZONES_DIRS[0]))
}

fn zone_path(zone: usize) -> PathBuf {
    dir().join(format!("zone{zone:02}"))
}

/// Whether the files are there at all. A cheap `stat`, no reads.
pub fn present() -> bool {
    Path::new(&zone_path(0)).exists()
}

/// Reads the four zones. This is the probe as well as the read: a colour
/// that comes back is proof the interface works, and reading changes
/// nothing.
pub fn read_colors() -> Result<Vec<Rgb>, DialectError> {
    (0..crate::ZONES)
        .map(|zone| {
            let path = zone_path(zone);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| DialectError::Io(format!("{}: {e}", path.display())))?;
            parse_hex(text.trim())
                .ok_or_else(|| DialectError::Unreadable(format!("{}: {:?}", path.display(), text.trim())))
        })
        .collect()
}

pub fn write_colors(colors: &[Rgb]) -> Result<(), DialectError> {
    for (zone, color) in colors.iter().take(crate::ZONES).enumerate() {
        let path = zone_path(zone);
        // No newline: the kernel attribute parses a bare hex string, and
        // some builds of it are strict about the trailing byte.
        std::fs::write(&path, format!("{:02X}{:02X}{:02X}", color.r, color.g, color.b)).map_err(
            |e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => DialectError::NeedsRoot,
                _ => DialectError::Io(format!("{}: {e}", path.display())),
            },
        )?;
    }
    Ok(())
}

/// `RRGGBB`, with or without a `#`, upper or lower case. Deliberately not
/// [`Rgb`]'s own parser: that one accepts the three-digit CSS short form,
/// and a kernel attribute answering `fff` would be six bits of colour read
/// as twelve.
fn parse_hex(text: &str) -> Option<Rgb> {
    let text = text.strip_prefix('#').unwrap_or(text);
    if text.len() != 6 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&text[at..at + 2], 16).ok();
    Some(Rgb::new(byte(0)?, byte(2)?, byte(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel writes six digits and this must not accept anything
    /// else: a short form silently read as a long one is a wrong colour
    /// reported as a right one.
    #[test]
    fn only_the_six_digit_form_is_a_colour() {
        assert_eq!(parse_hex("FF9900"), Some(Rgb::new(0xff, 0x99, 0x00)));
        assert_eq!(parse_hex("ff9900"), Some(Rgb::new(0xff, 0x99, 0x00)));
        assert_eq!(parse_hex("#ff9900"), Some(Rgb::new(0xff, 0x99, 0x00)));
        for bad in ["fff", "", "ff99000", "gg9900", "0x9900ff"] {
            assert_eq!(parse_hex(bad), None, "{bad:?} is not a zone colour");
        }
    }

    /// The whole dialect, against a fixture directory - which is how it is
    /// exercised at all on a machine whose kernel publishes nothing.
    #[test]
    fn a_round_trip_through_the_sysfs_files() {
        let dir = std::env::temp_dir().join(format!("pyren-zones-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir is writable");
        for zone in 0..crate::ZONES {
            std::fs::write(dir.join(format!("zone{zone:02}")), "000000").unwrap();
        }

        // Not `set_var`: the tests in this crate share a process, and a
        // scoped guard is not a thing std offers. This test is the only
        // one that touches the variable.
        unsafe { std::env::set_var("PYREN_RGB_ZONES_DIR", &dir) };

        assert!(present());
        let colors = vec![Rgb::new(255, 0, 0), Rgb::new(0, 255, 0), Rgb::new(0, 0, 255), Rgb::new(1, 2, 3)];
        write_colors(&colors).expect("a temp dir is writable");
        assert_eq!(read_colors().expect("just written"), colors);

        unsafe { std::env::remove_var("PYREN_RGB_ZONES_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
