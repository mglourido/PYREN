//! `/dev/input/eventN` and `/dev/uinput`, at the level `pyren_hotkey`
//! already reads the kernel at - a 24-byte struct, a handful of ioctls, no
//! evdev crate - but for a different purpose: `hotkey` decodes events into
//! discrete key presses and throws the rest away, which is exactly the
//! information a remapper must forward untouched (releases, autorepeat,
//! `EV_SYN`, `EV_MSC`). So this is a second, smaller reader rather than a
//! shared one.
//!
//! ## The ioctl numbers are derived, not copied
//!
//! `EVIOCGRAB` and the `UI_*` constants below are computed with the same
//! `_IOC` formula `asm-generic/ioctl.h` uses, rather than pasted in as
//! magic hex. A root daemon that gets an ioctl number wrong here does not
//! error cleanly - it grabs the wrong thing or corrupts a struct layout -
//! so the formula is written out and unit-tested against the numbers the
//! kernel headers are known to produce.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const KEY_MAX: u16 = 0x2ff;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub time: libc::timeval,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

pub const EVENT_SIZE: usize = std::mem::size_of::<InputEvent>();

pub fn open_nonblocking(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(path)
}

/// Reads whatever is buffered, decoded into whole events. A trailing
/// partial event (a read landing mid-struct) is dropped; it arrives
/// complete on the next read.
pub fn read_events(file: &mut File, buffer: &mut [u8]) -> std::io::Result<Vec<InputEvent>> {
    let n = file.read(buffer)?;
    Ok(buffer[..n]
        .as_chunks::<EVENT_SIZE>()
        .0
        .iter()
        .map(|chunk| unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) })
        .collect())
}

pub fn write_event(file: &mut File, event: InputEvent) -> std::io::Result<()> {
    // SAFETY: `InputEvent` is `repr(C)` and `EVENT_SIZE` is its own size.
    let bytes = unsafe {
        std::slice::from_raw_parts((&event as *const InputEvent) as *const u8, EVENT_SIZE)
    };
    file.write_all(bytes)
}

/// `_IOC(dir, type, nr, size)` from `asm-generic/ioctl.h`. `dir` and `type`
/// are `u32` rather than the kernel's `_IOC_WRITE`/a `char` so a call site
/// reads as the header does.
const fn ioc(dir: u32, kind: u8, nr: u8, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((kind as u32) << 8) | (nr as u32)
}

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;

/// `EVIOCGRAB`: makes this fd the only one the kernel delivers this
/// device's events to. Every other reader - the compositor included - sees
/// nothing from it until it is released.
pub const EVIOCGRAB: u32 = ioc(IOC_WRITE, b'E', 0x90, std::mem::size_of::<libc::c_int>());

const UINPUT_IOCTL_BASE: u8 = b'U';
pub const UI_SET_EVBIT: u32 = ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 100, std::mem::size_of::<libc::c_int>());
pub const UI_SET_KEYBIT: u32 = ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 101, std::mem::size_of::<libc::c_int>());
pub const UI_DEV_CREATE: u32 = ioc(IOC_NONE, UINPUT_IOCTL_BASE, 1, 0);
pub const UI_DEV_DESTROY: u32 = ioc(IOC_NONE, UINPUT_IOCTL_BASE, 2, 0);
pub const UI_DEV_SETUP: u32 = ioc(IOC_WRITE, UINPUT_IOCTL_BASE, 3, std::mem::size_of::<UinputSetup>());

pub const UINPUT_MAX_NAME_SIZE: usize = 80;

/// `struct uinput_setup` (`linux/uinput.h`), the newer setup path
/// (`UI_DEV_SETUP`, kernel 4.5+) rather than writing a `uinput_user_dev` -
/// the daemon already requires a kernel far newer than that for the fan
/// driver, so there is no older kernel here to keep working.
#[repr(C)]
pub struct UinputSetup {
    pub id: InputId,
    pub name: [u8; UINPUT_MAX_NAME_SIZE],
    pub ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

pub fn grab(file: &File, grab: bool) -> std::io::Result<()> {
    let value: libc::c_int = if grab { 1 } else { 0 };
    ioctl_arg(file, EVIOCGRAB, &value)
}

/// Opens `/dev/uinput` and brings up a virtual keyboard reporting every
/// keycode in `keys` - the union of what every grabbed device can send,
/// since one virtual device stands in for all of them.
pub fn create_uinput(name: &str, keys: &[u16]) -> std::io::Result<File> {
    let path = std::env::var("PYREN_UINPUT_PATH").unwrap_or_else(|_| "/dev/uinput".to_string());
    let file = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(path)?;

    ioctl_arg(&file, UI_SET_EVBIT, &(EV_KEY as libc::c_int))?;
    ioctl_arg(&file, UI_SET_EVBIT, &(EV_SYN as libc::c_int))?;
    for &key in keys {
        ioctl_arg(&file, UI_SET_KEYBIT, &(key as libc::c_int))?;
    }

    let mut setup = UinputSetup {
        id: InputId { bustype: 0x03 /* BUS_USB */, vendor: 0x1209, product: 0x0001, version: 1 },
        name: [0u8; UINPUT_MAX_NAME_SIZE],
        ff_effects_max: 0,
    };
    let bytes = name.as_bytes();
    let len = bytes.len().min(UINPUT_MAX_NAME_SIZE - 1);
    setup.name[..len].copy_from_slice(&bytes[..len]);
    ioctl_arg(&file, UI_DEV_SETUP, &setup)?;
    ioctl_none(&file, UI_DEV_CREATE)?;

    Ok(file)
}

pub fn destroy_uinput(file: &File) {
    let _ = ioctl_none(file, UI_DEV_DESTROY);
}

fn ioctl_arg<T>(file: &File, request: u32, arg: &T) -> std::io::Result<()> {
    // SAFETY: `request` names an ioctl this module derived and size-checked
    // against `arg`'s own type, and `arg` outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg as *const T) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_none(file: &File, request: u32) -> std::io::Result<()> {
    // SAFETY: these requests take no argument.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn device_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir("/dev/input")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")))
        .collect();
    paths.sort();
    paths
}

pub fn device_name(path: &Path) -> Option<String> {
    let event_name = path.file_name()?.to_string_lossy().to_string();
    let sysfs = PathBuf::from("/sys/class/input").join(event_name).join("device/name");
    std::fs::read_to_string(sysfs).ok().map(|n| n.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The formula, checked against the numbers `linux/input.h` and
    /// `linux/uinput.h` are known to expand to - not because the formula
    /// might be wrong in general, but because a transposed shift here would
    /// otherwise only surface as a root daemon jamming a real keyboard.
    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(EVIOCGRAB, 0x40044590);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_KEYBIT, 0x40045565);
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
    }

    #[test]
    fn uinput_setup_is_the_size_the_kernel_expects() {
        // input_id (4 x u16 = 8 bytes) + 80-byte name + u32 = 92 bytes.
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
    }

    #[test]
    fn the_event_struct_is_the_size_the_kernel_writes() {
        assert_eq!(EVENT_SIZE, std::mem::size_of::<libc::timeval>() + 8);
    }
}
