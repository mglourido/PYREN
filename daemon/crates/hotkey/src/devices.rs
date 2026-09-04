//! Reading the keyboard the way the kernel offers it: `/dev/input/event*`,
//! `struct input_event`, and `poll(2)`.
//!
//! No evdev crate. The whole of what is needed here is a 24-byte struct, a
//! sysfs read for the device name and a poll loop, and this daemon already
//! talks to the kernel directly everywhere else.
//!
//! ## Two kinds of key, and why the second one is the interesting one
//!
//! A key the kernel has a keycode for produces `EV_MSC/MSC_SCAN` (the raw
//! scancode) followed by `EV_KEY` (the keycode), press and release.
//!
//! A key it has *no* keycode for - `atkbd: Unknown key pressed ... code
//! 0xab`, which is what this laptop's Fn+P looks like - produces the
//! `MSC_SCAN` and **nothing else**. That is good news, because it means the
//! key is reachable without `setkeycodes` or a udev hwdb entry, and the
//! compositor never sees it, so it cannot collide with a user's own
//! bindings.
//!
//! It also brings the one real limitation here: for such a key the kernel
//! emits the same bare `MSC_SCAN` on press *and* on release, with nothing
//! to tell them apart. One physical press therefore arrives as two
//! identical events, which is why [`super::HotkeyConfig::repeat_guard_ms`] coalesces repeats
//! within a short window rather than acting on every edge.
//!
//! ## Privacy
//!
//! This runs as root and it reads keyboards, so it is worth being explicit:
//! a key that matches no trigger is compared and dropped on the spot. It is
//! never stored, never logged and never leaves the process. The one
//! exception is a learn window the user opened deliberately, which lasts
//! seconds and reports exactly one key press - the one they pressed to
//! answer the question.

use std::fs::File;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pyren_core::{msg, Msg};
use serde::{Deserialize, Serialize};

/// Event types we care about, from `linux/input-event-codes.h`.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_MSC: u16 = 0x04;
const MSC_SCAN: u16 = 0x04;
const SYN_REPORT: u16 = 0x00;

/// `BTN_MISC`. Codes below this are keys on a keyboard; from here up they
/// are buttons - mouse, touchpad, tablet, gamepad.
///
/// It is not a clean threshold, because the kernel came back for more
/// `KEY_*` codes after the first block of buttons: `0x100..0x160` and
/// `0x2c0..0x300` are buttons, and everything else is a key. Hence
/// [`is_button`] rather than a comparison.
const BTN_MISC: u16 = 0x100;

/// Whether a keycode names a button on a pointing device.
///
/// This is here because of a real accident: a `hotkey learn` window caught
/// `BTN_TOOL_FINGER` (325) from the touchpad - "a finger is resting here" -
/// and bound the power-mode cycle to touching the trackpad. Nothing about
/// that was a key press, and no hotkey worth having is a mouse button.
pub fn is_button(keycode: u16) -> bool {
    matches!(keycode, 0x100..=0x15f | 0x2c0..=0x2ff)
}

/// The modifier keycodes, from `linux/input-event-codes.h`. Each pair is
/// one modifier: the two Ctrls are Ctrl, and `KEY_RIGHTALT` is AltGr,
/// which is Alt as far as a binding is concerned.
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;

/// Whether a keycode is a modifier rather than a key a binding can end on.
///
/// A modifier is never a trigger by itself. Binding the power-mode cycle to
/// Ctrl would fire on the first half of every copy-paste, and - the reason
/// this exists at all - a learn window that accepted the first key to
/// arrive would catch the `Ctrl` of `Ctrl+Alt+P` and bind that, every time,
/// because the modifier necessarily goes down first.
pub fn is_modifier(keycode: u16) -> bool {
    matches!(
        keycode,
        KEY_LEFTCTRL
            | KEY_RIGHTCTRL
            | KEY_LEFTSHIFT
            | KEY_RIGHTSHIFT
            | KEY_LEFTALT
            | KEY_RIGHTALT
            | KEY_LEFTMETA
            | KEY_RIGHTMETA
    )
}

/// Which modifiers were held when a key went down.
///
/// Left and right are deliberately not distinguished: nobody means "the
/// right Shift specifically" when they choose a shortcut, and a binding
/// that fires on one Shift and not the other is a bug report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Modifiers {
    fn set(&mut self, keycode: u16, down: bool) {
        match keycode {
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.ctrl = down,
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift = down,
            KEY_LEFTALT | KEY_RIGHTALT => self.alt = down,
            KEY_LEFTMETA | KEY_RIGHTMETA => self.meta = down,
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `Ctrl+Alt+`, ready to have a key name appended. Ordered the way
    /// every desktop writes it, so a user comparing this against their
    /// compositor's config sees the same string.
    pub fn prefix(&self) -> String {
        let mut out = String::new();
        for (held, name) in
            [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.meta, "Super")]
        {
            if held {
                out.push_str(name);
                out.push('+');
            }
        }
        out
    }
}

/// `struct input_event`. `timeval` comes from libc so the layout follows
/// the platform's own ABI rather than an assumption about 64-bit time.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

const EVENT_SIZE: usize = std::mem::size_of::<InputEvent>();

/// One key going down, as the rest of this crate cares about it.
///
/// Both codes are optional because both cases happen: a mapped key has a
/// keycode and (usually) a scancode, an unmapped key has only a scancode,
/// and a virtual device like `HP WMI hotkeys` reports a keycode with no
/// scancode at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyPress {
    pub device: String,
    pub keycode: Option<u16>,
    pub scancode: Option<u32>,
    /// What was being held down at the moment this key went down.
    pub modifiers: Modifiers,
}

impl KeyPress {
    /// How this reads in a CLI or a UI: `keycode 148 (scancode 0xe02b)`.
    pub fn describe(&self) -> String {
        let codes = match (self.keycode, self.scancode) {
            (Some(key), Some(scan)) => format!("keycode {key}, scancode 0x{scan:x}"),
            (Some(key), None) => format!("keycode {key}"),
            (None, Some(scan)) => format!("scancode 0x{scan:x} (no keycode assigned)"),
            (None, None) => "nothing identifiable".to_string(),
        };
        match self.modifiers.is_empty() {
            true => codes,
            false => format!("{} held, {codes}", self.modifiers.prefix().trim_end_matches('+')),
        }
    }

    /// The shortcut as somebody would write it down: `Ctrl+Alt+P`.
    ///
    /// Falls back to the numbers for a key with no name here, which is the
    /// case that matters most - a vendor key the kernel has no keycode for
    /// is exactly what this daemon was written to catch.
    pub fn label(&self) -> String {
        let key = self
            .keycode
            .and_then(key_name)
            .map(str::to_string)
            .or_else(|| self.scancode.map(|scan| format!("scancode 0x{scan:x}")))
            .or_else(|| self.keycode.map(|key| format!("keycode {key}")))
            .unwrap_or_else(|| "?".into());
        format!("{}{key}", self.modifiers.prefix())
    }
}

/// A name for the keycodes a person is likely to choose, and nothing more.
///
/// Deliberately not the whole of `input-event-codes.h`: this exists so a
/// settings page can show `Ctrl+Alt+P` instead of `keycode 25`, and every
/// key it cannot name still shows its number rather than nothing.
pub fn key_name(keycode: u16) -> Option<&'static str> {
    const LETTERS: [(u16, &str); 26] = [
        (30, "A"), (48, "B"), (46, "C"), (32, "D"), (18, "E"), (33, "F"), (34, "G"), (35, "H"),
        (23, "I"), (36, "J"), (37, "K"), (38, "L"), (50, "M"), (49, "N"), (24, "O"), (25, "P"),
        (16, "Q"), (19, "R"), (31, "S"), (20, "T"), (22, "U"), (47, "V"), (17, "W"), (45, "X"),
        (21, "Y"), (44, "Z"),
    ];
    const OTHERS: [(u16, &str); 30] = [
        (1, "Esc"), (2, "1"), (3, "2"), (4, "3"), (5, "4"), (6, "5"), (7, "6"), (8, "7"),
        (9, "8"), (10, "9"), (11, "0"), (14, "Backspace"), (15, "Tab"), (28, "Enter"),
        (57, "Space"), (59, "F1"), (60, "F2"), (61, "F3"), (62, "F4"), (63, "F5"), (64, "F6"),
        (65, "F7"), (66, "F8"), (67, "F9"), (68, "F10"), (87, "F11"), (88, "F12"),
        (110, "Insert"), (111, "Delete"), (119, "Pause"),
    ];
    LETTERS
        .iter()
        .chain(OTHERS.iter())
        .find(|(code, _)| *code == keycode)
        .map(|(_, name)| *name)
}

/// An input device this crate has open.
pub struct Device {
    pub path: PathBuf,
    pub name: String,
    file: File,
    /// Scancode seen since the last `SYN_REPORT`, waiting to be paired with
    /// a keycode - or reported on its own if none arrives.
    pending_scan: Option<u32>,
    /// Whether a key went down in the packet being accumulated.
    pending_key: Option<u16>,
    /// Modifiers currently held on *this* device.
    ///
    /// Per device rather than global: a modifier held on the built-in
    /// keyboard is not held on a USB one, and the kernel reports each
    /// device's own keys. A shortcut is therefore pressed on one keyboard,
    /// which is how anybody presses one anyway.
    held: Modifiers,
    /// Set when the packet being accumulated is a modifier going down or
    /// up. Such a packet updates `held` and produces no press of its own.
    pending_modifier: bool,
}

impl Device {
    /// Decodes one read from the device into whatever key presses it held.
    ///
    /// Separate from the reading so it can be tested without a keyboard,
    /// which is the only way this gets tested in CI at all.
    fn absorb(&mut self, buffer: &[u8]) -> Vec<KeyPress> {
        let mut presses = Vec::new();
        // `.0` drops a trailing partial event: a read can land mid-struct,
        // and the rest of it arrives on the next one.
        for chunk in buffer.as_chunks::<EVENT_SIZE>().0 {
            // repr(C) POD out of a byte buffer that may not be aligned.
            let event: InputEvent =
                unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };

            match (event.kind, event.code, event.value) {
                (EV_MSC, MSC_SCAN, value) => self.pending_scan = Some(value as u32),
                // A modifier is state, not a press. It updates what is
                // held and produces nothing of its own - on the way down
                // and on the way up, and on auto-repeat, which is a key
                // being held and so leaves `held` exactly as it is.
                (EV_KEY, code, value) if is_modifier(code) => {
                    if value != 2 {
                        self.held.set(code, value == 1);
                    }
                    self.pending_modifier = true;
                }
                // 1 is a press; 2 is the kernel's auto-repeat and 0 is the
                // release, and neither should re-trigger an action.
                (EV_KEY, code, 1) => self.pending_key = Some(code),
                (EV_KEY, _, _) => {
                    // A release still closes the packet: its scancode must
                    // not be left to attach itself to the next key.
                    self.pending_scan = None;
                    self.pending_key = None;
                }
                (EV_SYN, SYN_REPORT, _) => {
                    let (key, scan) = (self.pending_key.take(), self.pending_scan.take());
                    // A modifier's own packet carries a scancode too, and
                    // letting that through would report `scancode 0x1d` as
                    // a nameless key every time somebody held Ctrl.
                    let was_modifier = std::mem::take(&mut self.pending_modifier);
                    if !was_modifier && (key.is_some() || scan.is_some()) {
                        presses.push(KeyPress {
                            device: self.name.clone(),
                            keycode: key,
                            scancode: scan,
                            modifiers: self.held,
                        });
                    }
                }
                _ => {}
            }
        }
        presses
    }

    /// Drains everything the device has buffered. `Ok(None)` means the
    /// device went away - unplugged, or the driver unbound.
    fn read_presses(&mut self) -> std::io::Result<Option<Vec<KeyPress>>> {
        let mut buffer = [0u8; EVENT_SIZE * 32];
        match self.file.read(&mut buffer) {
            Ok(0) => Ok(Some(Vec::new())),
            Ok(n) => Ok(Some(self.absorb(&buffer[..n]))),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Some(Vec::new())),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(Some(Vec::new())),
            // ENODEV on a device that has been removed. Anything else is
            // treated the same way: stop reading it, and let the next scan
            // decide whether it is back.
            Err(_) => Ok(None),
        }
    }
}

/// Why no keyboard could be opened, in the words the caller should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// `/dev/input` has no key devices at all. A container, usually.
    NoDevices,
    /// They are there and this process may not read them. The daemon is
    /// running unprivileged; the systemd unit is the fix.
    NeedsRoot,
}

impl Unavailable {
    /// Why no keyboard could be opened, in the words the caller should show.
    pub fn to_msg(&self) -> Msg {
        match self {
            Self::NoDevices => msg!(
                "hotkey.unavailable.noDevices",
                "no keyboard devices in /dev/input, so no hotkey can be heard here"
            ),
            Self::NeedsRoot => msg!(
                "hotkey.unavailable.needsRoot",
                "reading /dev/input needs root; the systemd unit runs the daemon as root"
            ),
        }
    }
}

/// Whether a `capabilities/key` bitmask contains any key a keyboard has.
///
/// The mask is printed as 64-bit words, **most significant first**, so the
/// keyboard range (0 to `BTN_MISC`) is the last four words. A device with
/// nothing set there reports only buttons, and is a mouse or a touchpad
/// however it is named.
///
/// This is the filter that keeps the touchpad out of the watcher entirely,
/// rather than relying on catching its buttons later. On the test laptop:
///
/// | device | `capabilities/key` | keys below 0x100? |
/// |---|---|---|
/// | AT Translated Set 2 keyboard | `20000 6000…20 0 0 10500f02100007 …` | yes |
/// | HP WMI hotkeys | `180000 20000 0 4000000000 0 1010007… 2302400 0 0` | yes |
/// | Touchpad | `e520 10000 0 0 0 0` | **no** |
/// | Mouse | `30000 0 0 0 0` | **no** |
fn has_keyboard_keys(mask: &str) -> bool {
    let words_for_the_key_range = (BTN_MISC as usize) / 64;
    mask.split_whitespace()
        .rev()
        .take(words_for_the_key_range)
        .filter_map(|word| u64::from_str_radix(word, 16).ok())
        .any(|bits| bits != 0)
}

fn is_keyboard(sysfs: &Path) -> bool {
    std::fs::read_to_string(sysfs.join("device/capabilities/key"))
        .is_ok_and(|mask| has_keyboard_keys(&mask))
}

/// Whether this device reports key events at all, from sysfs rather than an
/// ioctl: `capabilities/ev` is a hex bitmask and `EV_KEY` is bit 1.
///
/// Worth the extra read - it keeps the daemon from holding open a dozen
/// audio-jack and lid-switch descriptors it will never learn anything from.
fn reports_keys(sysfs: &Path) -> bool {
    std::fs::read_to_string(sysfs.join("device/capabilities/ev"))
        .ok()
        .and_then(|value| {
            // The mask can be several space-separated words for high bits;
            // EV_KEY is in the last one.
            let last = value.trim().rsplit(' ').next()?.to_string();
            u64::from_str_radix(&last, 16).ok()
        })
        .is_some_and(|mask| mask & (1 << EV_KEY) != 0)
}

fn device_name(sysfs: &Path) -> Option<String> {
    std::fs::read_to_string(sysfs.join("device/name")).ok().map(|n| n.trim().to_string())
}

/// Opens every keyboard-ish device in `/dev/input`.
///
/// Returns `Err` only when *nothing* could be opened, so that one
/// unreadable device does not hide the rest.
pub fn open_all() -> Result<Vec<Device>, Unavailable> {
    let mut devices = Vec::new();
    let mut saw_permission_denied = false;

    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(_) => return Err(Unavailable::NoDevices),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("event") {
            continue;
        }

        let sysfs = PathBuf::from("/sys/class/input").join(name.as_ref());
        // Both, and in this order: the first asks whether it speaks keys at
        // all, the second whether any of them is a *keyboard* key. A mouse
        // and a touchpad pass the first and fail the second.
        if !reports_keys(&sysfs) || !is_keyboard(&sysfs) {
            continue;
        }

        match open_nonblocking(&path) {
            Ok(file) => {
                let name = device_name(&sysfs).unwrap_or_else(|| name.to_string());
                devices.push(Device {
                    path,
                    name,
                    file,
                    pending_scan: None,
                    pending_key: None,
                    held: Modifiers::default(),
                    pending_modifier: false,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                saw_permission_denied = true;
            }
            Err(_) => {}
        }
    }

    if devices.is_empty() {
        return Err(if saw_permission_denied {
            Unavailable::NeedsRoot
        } else {
            Unavailable::NoDevices
        });
    }
    devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(devices)
}

fn open_nonblocking(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)
}

/// Waits for any of `devices` to have something to say, then returns every
/// key press that arrived.
///
/// A device that goes away is dropped from the list; the caller rescans on
/// its own schedule, which is how a keyboard plugged in later is picked up.
pub fn wait_for_presses(devices: &mut Vec<Device>, timeout: Duration) -> Vec<KeyPress> {
    if devices.is_empty() {
        std::thread::sleep(timeout);
        return Vec::new();
    }

    let mut fds: Vec<libc::pollfd> = devices
        .iter()
        .map(|d| libc::pollfd { fd: d.file.as_raw_fd(), events: libc::POLLIN, revents: 0 })
        .collect();

    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: fds is a valid slice for the length passed, and poll writes
    // only into revents.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    if ready <= 0 {
        return Vec::new();
    }

    let mut presses = Vec::new();
    let mut gone = Vec::new();
    for (index, fd) in fds.iter().enumerate() {
        if fd.revents == 0 {
            continue;
        }
        // POLLERR/POLLHUP/POLLNVAL all mean the same thing here.
        if fd.revents & libc::POLLIN == 0 {
            gone.push(index);
            continue;
        }
        match devices[index].read_presses() {
            Ok(Some(mut found)) => presses.append(&mut found),
            Ok(None) | Err(_) => gone.push(index),
        }
    }

    for index in gone.into_iter().rev() {
        devices.remove(index);
    }
    presses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Device {
        Device {
            path: PathBuf::from("/dev/input/event0"),
            name: "AT Translated Set 2 keyboard".into(),
            // Any readable file works: `absorb` never touches it.
            file: File::open("/dev/null").expect("/dev/null must be openable"),
            pending_scan: None,
            pending_key: None,
            held: Modifiers::default(),
            pending_modifier: false,
        }
    }

    fn bytes(events: &[(u16, u16, i32)]) -> Vec<u8> {
        let mut buffer = Vec::new();
        for (kind, code, value) in events {
            let event = InputEvent {
                time: libc::timeval { tv_sec: 0, tv_usec: 0 },
                kind: *kind,
                code: *code,
                value: *value,
            };
            let raw: [u8; EVENT_SIZE] = unsafe { std::mem::transmute(event) };
            buffer.extend_from_slice(&raw);
        }
        buffer
    }

    /// An ordinary key: scancode, then keycode, then the sync that ends the
    /// packet. One press, carrying both numbers.
    #[test]
    fn a_mapped_key_arrives_as_one_press_with_both_codes() {
        let mut device = device();
        let presses = device.absorb(&bytes(&[
            (EV_MSC, MSC_SCAN, 0x19),
            (EV_KEY, 25, 1),
            (EV_SYN, SYN_REPORT, 0),
        ]));

        assert_eq!(presses.len(), 1);
        assert_eq!(presses[0].keycode, Some(25));
        assert_eq!(presses[0].scancode, Some(0x19));
    }

    /// The case this whole crate exists for: the kernel has no keycode, so
    /// the scancode arrives alone and there is no `EV_KEY` at all.
    #[test]
    fn an_unmapped_key_is_still_a_press_even_with_no_keycode() {
        let mut device = device();
        let presses =
            device.absorb(&bytes(&[(EV_MSC, MSC_SCAN, 0xe02b), (EV_SYN, SYN_REPORT, 0)]));

        assert_eq!(presses.len(), 1);
        assert_eq!(presses[0].keycode, None);
        assert_eq!(presses[0].scancode, Some(0xe02b));
        assert!(presses[0].describe().contains("no keycode"));
    }

    /// Auto-repeat and release must not each count as a fresh press, or
    /// holding the key down would walk through every power mode.
    #[test]
    fn a_release_or_a_repeat_is_not_a_press() {
        let mut device = device();
        let presses = device.absorb(&bytes(&[
            (EV_MSC, MSC_SCAN, 0x19),
            (EV_KEY, 25, 0),
            (EV_SYN, SYN_REPORT, 0),
            (EV_MSC, MSC_SCAN, 0x19),
            (EV_KEY, 25, 2),
            (EV_SYN, SYN_REPORT, 0),
        ]));

        assert!(presses.is_empty(), "got {presses:?}");
    }

    /// A read can land mid-packet; the scancode of one key must not be
    /// glued onto the keycode of the next.
    #[test]
    fn a_packet_split_across_two_reads_still_pairs_correctly() {
        let mut device = device();
        assert!(device.absorb(&bytes(&[(EV_MSC, MSC_SCAN, 0xe02b)])).is_empty());

        let presses = device.absorb(&bytes(&[(EV_KEY, 148, 1), (EV_SYN, SYN_REPORT, 0)]));
        assert_eq!(presses.len(), 1);
        assert_eq!(presses[0].keycode, Some(148));
        assert_eq!(presses[0].scancode, Some(0xe02b));
    }

    /// A virtual hotkey device (`HP WMI hotkeys`) reports a keycode and no
    /// scancode; the pair must not be required.
    #[test]
    fn a_keycode_with_no_scancode_is_a_press_too() {
        let mut device = device();
        let presses = device.absorb(&bytes(&[(EV_KEY, 148, 1), (EV_SYN, SYN_REPORT, 0)]));

        assert_eq!(presses.len(), 1);
        assert_eq!(presses[0], KeyPress {
            device: "AT Translated Set 2 keyboard".into(),
            keycode: Some(148),
            scancode: None,
            modifiers: Modifiers::default(),
        });
    }

    /// The case the settings page exists for: a combination. The modifier
    /// goes down first in its own packet, and must not be reported as a
    /// key - it is state, and the key that follows carries it.
    #[test]
    fn a_combination_is_one_press_carrying_what_was_held() {
        let mut device = device();
        let presses = device.absorb(&bytes(&[
            // Ctrl down: scancode, keycode, sync. Reports nothing.
            (EV_MSC, MSC_SCAN, 0x1d),
            (EV_KEY, KEY_LEFTCTRL, 1),
            (EV_SYN, SYN_REPORT, 0),
            // Alt down.
            (EV_MSC, MSC_SCAN, 0x38),
            (EV_KEY, KEY_LEFTALT, 1),
            (EV_SYN, SYN_REPORT, 0),
            // P down: the only press in the whole sequence.
            (EV_MSC, MSC_SCAN, 0x19),
            (EV_KEY, 25, 1),
            (EV_SYN, SYN_REPORT, 0),
        ]));

        assert_eq!(presses.len(), 1, "only the key is a press; the modifiers are not");
        assert_eq!(presses[0].keycode, Some(25));
        assert!(presses[0].modifiers.ctrl && presses[0].modifiers.alt);
        assert!(!presses[0].modifiers.shift && !presses[0].modifiers.meta);
        assert_eq!(presses[0].label(), "Ctrl+Alt+P");
    }

    /// Letting go has to be seen, or every key pressed for the rest of the
    /// session would claim Ctrl was held and no plain binding would match.
    #[test]
    fn releasing_a_modifier_stops_it_being_held() {
        let mut device = device();
        device.absorb(&bytes(&[(EV_KEY, KEY_LEFTCTRL, 1), (EV_SYN, SYN_REPORT, 0)]));
        device.absorb(&bytes(&[(EV_KEY, KEY_LEFTCTRL, 0), (EV_SYN, SYN_REPORT, 0)]));
        let presses = device.absorb(&bytes(&[(EV_KEY, 25, 1), (EV_SYN, SYN_REPORT, 0)]));

        assert_eq!(presses.len(), 1);
        assert!(presses[0].modifiers.is_empty(), "Ctrl was let go before P");
    }

    /// Holding a modifier auto-repeats it, and a repeat is neither a new
    /// press nor a release.
    #[test]
    fn a_held_modifier_repeating_is_still_held() {
        let mut device = device();
        device.absorb(&bytes(&[(EV_KEY, KEY_LEFTSHIFT, 1), (EV_SYN, SYN_REPORT, 0)]));
        device.absorb(&bytes(&[(EV_KEY, KEY_LEFTSHIFT, 2), (EV_SYN, SYN_REPORT, 0)]));
        let presses = device.absorb(&bytes(&[(EV_KEY, 25, 1), (EV_SYN, SYN_REPORT, 0)]));

        assert!(presses[0].modifiers.shift);
    }

    /// Left and right are the same modifier: a binding learned with the
    /// left Shift has to fire on the right one.
    #[test]
    fn the_two_sides_of_a_modifier_are_the_same_modifier() {
        let mut left = device();
        left.absorb(&bytes(&[(EV_KEY, KEY_LEFTSHIFT, 1), (EV_SYN, SYN_REPORT, 0)]));
        let from_left = left.absorb(&bytes(&[(EV_KEY, 25, 1), (EV_SYN, SYN_REPORT, 0)]));

        let mut right = device();
        right.absorb(&bytes(&[(EV_KEY, KEY_RIGHTSHIFT, 1), (EV_SYN, SYN_REPORT, 0)]));
        let from_right = right.absorb(&bytes(&[(EV_KEY, 25, 1), (EV_SYN, SYN_REPORT, 0)]));

        assert_eq!(from_left[0].modifiers, from_right[0].modifiers);
    }

    /// A key with no name still has to be describable, because the vendor
    /// key this daemon was written for is exactly that.
    #[test]
    fn a_key_with_no_name_falls_back_to_its_numbers() {
        let press = KeyPress {
            device: "AT Translated Set 2 keyboard".into(),
            keycode: None,
            scancode: Some(0xab),
            modifiers: Modifiers::default(),
        };
        assert_eq!(press.label(), "scancode 0xab");
    }

    #[test]
    fn a_trailing_partial_event_is_ignored_rather_than_misread() {
        let mut device = device();
        let mut buffer = bytes(&[(EV_KEY, 148, 1), (EV_SYN, SYN_REPORT, 0)]);
        buffer.extend_from_slice(&[0u8; 5]);

        assert_eq!(device.absorb(&buffer).len(), 1);
    }

    /// The masks are copied from the test laptop's own sysfs, which is
    /// where the accident happened that this filter exists to prevent.
    #[test]
    fn a_pointing_device_is_not_a_keyboard() {
        assert!(
            !has_keyboard_keys("e520 10000 0 0 0 0"),
            "the touchpad reports BTN_TOOL_FINGER and no key at all"
        );
        assert!(!has_keyboard_keys("30000 0 0 0 0"), "the mouse reports two buttons");
        assert!(!has_keyboard_keys("0"), "a lid switch reports no keys");
        assert!(!has_keyboard_keys(""), "an unreadable mask is not a keyboard");
    }

    #[test]
    fn a_keyboard_and_a_hotkey_device_both_are() {
        assert!(has_keyboard_keys(
            "20000 6000000000000020 0 0 10500f02100007 ff803078f900d401 \
             feffffdfffcfffff fffffffffffffffe"
        ));
        // The virtual device the hp-wmi driver creates. Its keys live high
        // in the keyboard range, which is exactly the case a naive "is the
        // last word non-zero" test would get wrong.
        assert!(has_keyboard_keys("180000 20000 0 4000000000 0 101000700000000 2302400 0 0"));
    }

    /// The two button ranges, and the block of `KEY_*` codes the kernel
    /// wedged between them - which is why this is not a threshold.
    #[test]
    fn buttons_are_told_apart_from_keys_by_range_not_by_size() {
        assert!(is_button(0x110), "BTN_LEFT");
        assert!(is_button(325), "BTN_TOOL_FINGER, the one that caused this");
        assert!(is_button(0x2c0), "BTN_TRIGGER_HAPPY");

        assert!(!is_button(1), "KEY_ESC");
        assert!(!is_button(148), "KEY_PROG1");
        assert!(!is_button(0x160), "KEY_OK sits above the first button block");
        assert!(!is_button(0x264), "KEY_KBDINPUTASSIST_CANCEL, higher still");
    }

    /// 24 bytes on a 64-bit kernel. The number is not the point - taking
    /// it from libc's `timeval` rather than assuming one is.
    #[test]
    fn the_event_struct_is_the_size_the_kernel_writes() {
        assert_eq!(EVENT_SIZE, std::mem::size_of::<libc::timeval>() + 8);
    }
}
