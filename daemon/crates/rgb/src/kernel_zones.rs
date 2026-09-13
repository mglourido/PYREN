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

//!
//! ## Guards
//!
//! - The directory is looked up **once per write**, so a driver reload in
//!   the middle of a frame cannot split one write across two directories.
//! - The platform device has to belong to one of the two drivers above
//!   (its `driver` link, or the module being loaded where there is no
//!   link). Something else publishing an `rgb_zones` group is not written.
//! - A write reads all four zones first, writes only the ones that change,
//!   and puts back the ones it already wrote if a later one fails - so a
//!   failure does not leave the keyboard half in the old colours.

use std::path::{Path, PathBuf};

use pyren_core::{acpi, msg};

use crate::color::Rgb;
use crate::dialect::DialectError;

/// The drivers allowed to own the zone files, as their `driver` link and as
/// their `/sys/module` entry.
const DRIVERS: [(&str, &str); 2] = [
    ("hp-wmi", "hp_wmi"),
    ("omen-rgb-keyboard", "omen_rgb_keyboard"),
];

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
    if let Some(from_env) = acpi::test_override("PYREN_RGB_ZONES_DIR") {
        return PathBuf::from(from_env);
    }
    ZONES_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|dir| dir.join("zone00").exists())
        .unwrap_or_else(|| PathBuf::from(ZONES_DIRS[0]))
}

fn zone_path(dir: &Path, zone: usize) -> PathBuf {
    dir.join(format!("zone{zone:02}"))
}

/// Whether the files are there at all. A cheap `stat`, no reads.
pub fn present() -> bool {
    zone_path(&dir(), 0).exists()
}

/// The directory to read and write, checked. See the module docs.
fn checked_dir() -> Result<PathBuf, DialectError> {
    // A fixture has no platform device to check.
    if let Some(from_env) = acpi::test_override("PYREN_RGB_ZONES_DIR") {
        return Ok(PathBuf::from(from_env));
    }
    let dir = dir();
    if !zone_path(&dir, 0).exists() {
        return Err(DialectError::Io(format!(
            "{}: no zone files",
            dir.display()
        )));
    }
    verify_driver(&dir)?;
    Ok(dir)
}

fn verify_driver(dir: &Path) -> Result<(), DialectError> {
    let device = dir.parent().unwrap_or(dir);
    let owner = match std::fs::read_link(device.join("driver")) {
        Ok(link) => link
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        // No driver bound to the device: the module that registered it has
        // to be one of ours, and loaded.
        Err(_) => match DRIVERS
            .iter()
            .find(|(_, module)| Path::new("/sys/module").join(module).exists())
        {
            Some((name, _)) => return driver_matches_device(device, name),
            None => String::new(),
        },
    };
    if DRIVERS.iter().any(|(name, _)| *name == owner) {
        Ok(())
    } else {
        Err(unexpected_driver(device, &owner))
    }
}

/// With no `driver` link, the device directory's own name has to be the
/// loaded module's.
fn driver_matches_device(device: &Path, module_name: &str) -> Result<(), DialectError> {
    let name = device
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name == module_name {
        Ok(())
    } else {
        Err(unexpected_driver(device, ""))
    }
}

fn unexpected_driver(device: &Path, owner: &str) -> DialectError {
    DialectError::Unsafe(msg!(
        "rgb.dialect.kernelZones.driver",
        { "path" => device.display().to_string(), "driver" => owner.to_string() },
        "{path} does not belong to hp-wmi or omen-rgb-keyboard (driver: '{driver}'), so its \
         zone files were not written"
    ))
}

/// Reads the four zones. This is the probe as well as the read: a colour
/// that comes back is proof the interface works, and reading changes
/// nothing.
pub fn read_colors() -> Result<Vec<Rgb>, DialectError> {
    read_zones(&checked_dir()?)
}

fn read_zones(dir: &Path) -> Result<Vec<Rgb>, DialectError> {
    (0..crate::ZONES)
        .map(|zone| {
            let path = zone_path(dir, zone);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| DialectError::Io(format!("{}: {e}", path.display())))?;
            parse_hex(text.trim()).ok_or_else(|| {
                DialectError::Unreadable(format!("{}: {:?}", path.display(), text.trim()))
            })
        })
        .collect()
}

fn write_zone(dir: &Path, zone: usize, color: Rgb) -> Result<(), DialectError> {
    let path = zone_path(dir, zone);
    // No newline: the kernel attribute parses a bare hex string, and some
    // builds of it are strict about the trailing byte.
    std::fs::write(
        &path,
        format!("{:02X}{:02X}{:02X}", color.r, color.g, color.b),
    )
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => DialectError::NeedsRoot,
        _ => DialectError::Io(format!("{}: {e}", path.display())),
    })
}

/// Reads first, writes what changed, and on a failure puts back what it
/// had already written. See the module docs.
pub fn write_colors(colors: &[Rgb]) -> Result<(), DialectError> {
    let dir = checked_dir()?;
    let before = read_zones(&dir)?;
    let mut written = Vec::new();
    for (zone, (&color, &old)) in colors.iter().zip(&before).enumerate() {
        if color == old {
            continue;
        }
        if let Err(e) = write_zone(&dir, zone, color) {
            let restored = written
                .iter()
                .all(|&z: &usize| write_zone(&dir, z, before[z]).is_ok());
            return Err(match e {
                DialectError::Io(detail) => DialectError::Io(format!(
                    "{detail}; {}",
                    if restored {
                        "the zones before it were put back"
                    } else {
                        "the zones before it could not all be put back"
                    }
                )),
                other => other,
            });
        }
        written.push(zone);
    }
    Ok(())
}

/// What an animation writes a frame with: only the zones whose colour
/// differs from what `written` says the file holds. A zone that fails is
/// forgotten, so the next frame writes it again.
pub fn write_changed(
    colors: &[Rgb],
    written: &mut [Option<Rgb>; crate::ZONES],
) -> Result<(), DialectError> {
    let dir = checked_dir()?;
    for (zone, &color) in colors.iter().take(crate::ZONES).enumerate() {
        if written[zone] == Some(color) {
            continue;
        }
        written[zone] = None;
        write_zone(&dir, zone, color)?;
        written[zone] = Some(color);
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
        crate::testenv::zone_files(&dir);
        let _env = crate::testenv::fixture(&dir.join("no-acpi-call"), &dir);

        assert!(present());
        let colors = vec![
            Rgb::new(255, 0, 0),
            Rgb::new(0, 255, 0),
            Rgb::new(0, 0, 255),
            Rgb::new(1, 2, 3),
        ];
        write_colors(&colors).expect("a temp dir is writable");
        assert_eq!(read_colors().expect("just written"), colors);

        // A frame writes only the zones that changed.
        let mut cache = [Some(colors[0]), Some(colors[1]), None, Some(colors[3])];
        std::fs::write(dir.join("zone00"), "ABCDEF").unwrap();
        let mut frame = colors.clone();
        frame[2] = Rgb::new(7, 7, 7);
        write_changed(&frame, &mut cache).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("zone00")).unwrap(),
            "ABCDEF",
            "zone 0 was cached as unchanged and not rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("zone02")).unwrap(),
            "070707"
        );

        // A write that fails part-way puts the earlier zones back.
        write_colors(&colors).unwrap();
        let locked = dir.join("zone02");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
        // Root writes through a read-only mode, so there is nothing to test.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&locked)
            .is_err()
        {
            let target = vec![Rgb::new(9, 9, 9); crate::ZONES];
            assert!(write_colors(&target).is_err());
            assert_eq!(
                read_colors().unwrap(),
                colors,
                "zones 0 and 1 went back to what they were"
            );
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
