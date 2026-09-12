//! `sniff` - a raw dump of everything `/dev/input` and the LED class say,
//! for finding out what a key actually emits.
//!
//! This exists because of one question that cannot be answered from a
//! datasheet: **what does the HP keyboard-backlight key do on this
//! machine?** There are three possible answers and they need different
//! code, so the answer has to be measured rather than assumed:
//!
//! 1. It emits a keycode - `KEY_KBDILLUMTOGGLE` (228) or the up/down pair
//!    (230/229), usually from `HP WMI hotkeys`. Then the daemon can watch
//!    for it directly.
//! 2. It emits only a bare `MSC_SCAN` with no keycode, the way this
//!    laptop's Fn+P does (see `devices.rs`). Then the daemon watches the
//!    scancode, and press and release look identical.
//! 3. It emits **nothing at all** - the EC handles the key in firmware and
//!    never tells the kernel. Then the only way to know is to notice the
//!    firmware's own state changing, which is what the sysfs half of this
//!    watches for.
//!
//! Deliberately *not* built on [`pyren_hotkey::devices`]: that module
//! filters - it drops modifiers, swallows releases and pairs scancodes
//! with keycodes into one press. All correct for binding a shortcut, and
//! all of it hides exactly the detail this needs to see. So this reads the
//! 24-byte `struct input_event` itself and prints every one.
//!
//! ```text
//! cargo build -p pyren-hotkey --example sniff
//! sudo ./daemon/target/debug/examples/sniff          # runs until Ctrl-C
//! sudo ./daemon/target/debug/examples/sniff --secs 30
//! ```
//!
//! ## Dim, or off?
//!
//! Knowing *which* key was pressed is only half of it. "The backlight key"
//! can mean two different things and the daemon has to treat them
//! differently:
//!
//! - it **steps the brightness** (100 -> 50 -> 0 -> 100, say), in which
//!   case only the bottom of the cycle is worth reacting to; or
//! - it **switches the lighting off** outright, which is a state the
//!   daemon has no reader for at all today.
//!
//! Neither is visible from the keypress, so this also polls the firmware's
//! own lighting state through `pyren-rgb` - the same dialects the daemon
//! drives - and prints what moved. If the four zone colours scale down
//! together, the key dims. If they drop to black, or some other byte of
//! the 128-byte state buffer flips while the colours stay put, the key is
//! an on/off switch and that byte is the flag the daemon should be
//! reading.
//!
//! Needs root: `/dev/input/event*` is `root:input` and readable by nobody
//! else, and so is `/proc/acpi/call`. It prints keystrokes, so do not type
//! a password into the machine while it runs.
//!
//! **Stop the daemon first** (`sudo systemctl stop pyren`). Lighting reads
//! go through `/proc/acpi/call`, which is one shared file with no locking
//! between processes: a daemon writing frames at the same time as this
//! reads state will give both of them somebody else's reply. Pass
//! `--no-rgb` to watch only the keyboard and leave the firmware alone.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pyren_rgb::{Dialect, Rgb, Selection};

const EV_SYN: u16 = 0x00;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_KEY: u16 = 0x01;
const EV_MSC: u16 = 0x04;
const EV_SW: u16 = 0x05;
const EV_LED: u16 = 0x11;
const MSC_SCAN: u16 = 0x04;
/// `MSC_TIMESTAMP`. The touchpad emits one every few milliseconds while a
/// finger rests on it, and it says nothing about any key.
const MSC_TIMESTAMP: u16 = 0x05;

const EVENT_SIZE: usize = std::mem::size_of::<InputEvent>();

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

/// The keycodes worth naming here: the backlight keys this is looking for,
/// and enough of their neighbours that a near miss is recognisable.
fn key_name(code: u16) -> &'static str {
    match code {
        224 => "KEY_BRIGHTNESSDOWN",
        225 => "KEY_BRIGHTNESSUP",
        228 => "KEY_KBDILLUMTOGGLE  <-- keyboard backlight toggle",
        229 => "KEY_KBDILLUMDOWN    <-- keyboard backlight down",
        230 => "KEY_KBDILLUMUP      <-- keyboard backlight up",
        431 => "KEY_KBD_LCD_MENU1",
        0x1d1 => "KEY_KBDINPUTASSIST_PREV",
        148 => "KEY_PROG1",
        149 => "KEY_PROG2",
        202 => "KEY_PROG3",
        203 => "KEY_PROG4",
        _ => "",
    }
}

/// Whether an event is worth printing at all.
///
/// The first run of this drowned in the touchpad: a finger resting on it
/// produces a steady stream of `EV_ABS` positions, `MSC_TIMESTAMP` and
/// `BTN_TOOL_FINGER`, and four real key events were buried in it. None of
/// that can ever be the key being looked for - a backlight key is a key -
/// so it is dropped unless `--all` asks for the unfiltered firehose.
fn worth_printing(kind: u16, code: u16) -> bool {
    /// The button ranges, spelled out here rather than borrowed: the
    /// daemon's own `devices::is_button` is private to the crate, and
    /// widening its API for one example is the wrong trade. Kept in step
    /// with `devices.rs`, which explains why it is two ranges and not a
    /// threshold.
    fn is_button(code: u16) -> bool {
        (0x100..0x160).contains(&code) || (0x2c0..0x300).contains(&code)
    }

    match kind {
        EV_SYN | EV_REL | EV_ABS => false,
        EV_MSC => code != MSC_TIMESTAMP,
        EV_KEY => !is_button(code),
        _ => true,
    }
}

fn kind_name(kind: u16) -> &'static str {
    match kind {
        EV_SYN => "EV_SYN",
        EV_KEY => "EV_KEY",
        EV_MSC => "EV_MSC",
        EV_SW => "EV_SW",
        EV_LED => "EV_LED",
        0x02 => "EV_REL",
        0x03 => "EV_ABS",
        _ => "EV_?",
    }
}

/// `1` is a press, `2` the kernel's auto-repeat, `0` a release. Spelled out
/// because "which edge was that?" is the whole question for case 2 above.
fn value_name(kind: u16, value: i32) -> String {
    if kind != EV_KEY {
        return format!("{value} (0x{value:x})");
    }
    match value {
        0 => "0 release".into(),
        1 => "1 PRESS".into(),
        2 => "2 repeat".into(),
        other => format!("{other}"),
    }
}

struct Device {
    name: String,
    path: PathBuf,
    file: File,
}

/// Opens every `/dev/input/event*` that can be opened, naming each from
/// sysfs. Unlike the daemon's own opener this filters nothing: a device
/// that turns out to be the one emitting the key must not have been
/// skipped for looking like a mouse.
fn open_all() -> Vec<Device> {
    let mut devices = Vec::new();
    let entries = match fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("cannot read /dev/input: {e}");
            return devices;
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    paths.sort();

    for path in paths {
        // O_NONBLOCK so one silent device cannot block the read of another
        // after poll(2) has named it ready.
        let file = match open_nonblocking(&path) {
            Some(file) => file,
            None => continue,
        };
        let name = device_name(&path).unwrap_or_else(|| "?".into());
        devices.push(Device { name, path, file });
    }
    devices
}

fn open_nonblocking(path: &Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()
}

/// `/sys/class/input/eventN/device/name`, the device's own name rather
/// than the number, so the output says which key you pressed and on what.
fn device_name(path: &Path) -> Option<String> {
    let node = path.file_name()?.to_str()?;
    let sysfs = format!("/sys/class/input/{node}/device/name");
    Some(fs::read_to_string(sysfs).ok()?.trim().to_string())
}

/// Every LED-class brightness file, and the hp-wmi attributes that are
/// plain integers. This is the net for case 3: a key handled entirely in
/// firmware still tends to move a number somewhere under /sys.
fn sysfs_watchlist() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/leds") {
        for entry in entries.flatten() {
            paths.push(entry.path().join("brightness"));
        }
    }
    for name in ["als", "display", "dock", "postcode", "tablet"] {
        let path = PathBuf::from("/sys/devices/platform/hp-wmi").join(name);
        if path.exists() {
            paths.push(path);
        }
    }
    paths.retain(|p| fs::read_to_string(p).is_ok());
    paths.sort();
    paths
}

fn snapshot(paths: &[PathBuf]) -> BTreeMap<PathBuf, String> {
    paths
        .iter()
        .filter_map(|p| {
            fs::read_to_string(p)
                .ok()
                .map(|v| (p.clone(), v.trim().to_string()))
        })
        .collect()
}

/// The firmware's own lighting state, polled.
///
/// Whichever dialect answers is the one the daemon would use. For
/// `fourZone` the whole 128-byte state buffer is kept, not just the
/// colours: if the key sets an on/off flag rather than the colours, that
/// flag is one of the other 116 bytes and this is the only way to see it.
/// One firmware buffer this polls, by the name it is printed under.
type Surface = (&'static str, Vec<u8>);

/// Every raw reply the lighting firmware will give up, read fresh.
///
/// All of them, not just the resolved dialect's: on the test laptop both
/// `fourZone` and `lightbar` answer, and the whole question is *which*
/// buffer - if any - reflects a brightness the EC changed on its own. A
/// dialect that refuses simply contributes nothing.
///
/// `lightbar`'s is the interesting one. Its payload has a brightness
/// field at byte 3 (`lightbar::BRIGHTNESS_OFFSET`) where the other two
/// dialects have no brightness at all, and `read_colors` throws all but
/// three bytes of the reply away - so if the backlight key is legible
/// anywhere, it is here.
fn surfaces() -> Vec<Surface> {
    let mut found: Vec<Surface> = Vec::new();
    if let Ok(state) = pyren_rgb::fourzone::read_state() {
        found.push(("fourZone COLOR_GET", state));
    }
    if let Ok(info) = pyren_rgb::fourzone::platform_info() {
        found.push(("fourZone PLATFORM_INFO", info));
    }
    if let Ok(reply) = pyren_rgb::lightbar::raw_read(0) {
        found.push(("lightbar GET zone 0", reply));
    }
    found
}

/// `PASS`, the firmware's success sentinel. A reply's real payload starts
/// after it, so an offset is reported both ways: from the start of the
/// buffer, and from the sentinel, because only the second one lines up
/// with the documented layout.
const PASS: &[u8; 4] = b"PASS";

fn pass_at(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == PASS)
}

/// A byte offset, said both ways, plus a name where one is known.
fn offset_label(buffer: &[u8], index: usize) -> String {
    let relative = match pass_at(buffer) {
        Some(pass) if index >= pass => format!(" (PASS+{})", index - pass),
        _ => String::new(),
    };
    format!("[{index}]{relative}")
}

fn hex(buffer: &[u8]) -> String {
    buffer
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The firmware's own lighting state, polled.
struct RgbWatch {
    dialect: Dialect,
    colors: Vec<Rgb>,
    surfaces: Vec<Surface>,
}

impl RgbWatch {
    fn start() -> Option<Self> {
        let probes: Vec<_> = [Dialect::KernelZones, Dialect::FourZone, Dialect::Lightbar]
            .into_iter()
            .map(Dialect::probe)
            .collect();
        for probe in &probes {
            let verdict = match (probe.available, probe.asked) {
                (true, _) => "answers",
                (false, true) => "refused",
                (false, false) => "not asked",
            };
            println!("  dialect {:<12} {verdict}", probe.id);
        }

        let dialect = Selection::Auto.resolve(&probes)?;
        let colors = dialect.read_colors().ok()?;
        let surfaces = surfaces();

        // The full dump, once. Two things are only visible here: how long
        // each reply actually is - `acpi_call` truncates, and a byte past
        // the cut can never be seen changing - and what the buffer looks
        // like before anything is pressed, to diff against by eye.
        for (name, buffer) in &surfaces {
            println!("\n  {name}: {} bytes", buffer.len());
            for (row, chunk) in buffer.chunks(16).enumerate() {
                println!("    {:>3}  {}", row * 16, hex(chunk));
            }
            if let Some(pass) = pass_at(buffer) {
                let at = pass + 4 + pyren_rgb::lightbar::BRIGHTNESS_OFFSET;
                if let Some(value) = buffer.get(at) {
                    println!(
                        "    if this reply echoes the request's layout, brightness is [{at}] = {value}"
                    );
                }
            }
        }

        Some(Self {
            dialect,
            colors,
            surfaces,
        })
    }

    /// Prints whatever changed since the last poll.
    fn poll(&mut self, at: f32) -> bool {
        let mut changed = false;

        if let Ok(colors) = self.dialect.read_colors() {
            if colors != self.colors {
                changed = true;
                println!(
                    "[{at:>7.3}s] RGB   zones : {} -> {}",
                    describe(&self.colors),
                    describe(&colors)
                );
                // The question the whole example exists to answer, said
                // out loud so nobody has to compare hex by eye.
                println!("             {}", verdict(&self.colors, &colors));
                self.colors = colors;
            }
        }

        // Every buffer, byte by byte, nothing excluded. The colours are
        // reported above as well, and that duplication is the point: a
        // byte that moves *with* them is brightness scaled into the
        // colours, and a byte that moves without them is a field of its
        // own. Telling those apart is the whole exercise.
        for (name, previous) in std::mem::take(&mut self.surfaces) {
            let current = surfaces()
                .into_iter()
                .find(|(other, _)| *other == name)
                .map_or_else(Vec::new, |(_, buffer)| buffer);

            if current.is_empty() || current == previous {
                self.surfaces.push((
                    name,
                    if current.is_empty() {
                        previous
                    } else {
                        current
                    },
                ));
                continue;
            }

            let moved: Vec<String> = current
                .iter()
                .enumerate()
                .zip(previous.iter())
                .filter(|((_, new), old)| new != old)
                .map(|((index, new), old)| {
                    format!("{} {old:#04x} -> {new:#04x}", offset_label(&current, index))
                })
                .collect();
            if !moved.is_empty() {
                changed = true;
                println!("[{at:>7.3}s] RAW   {name} : {}", moved.join(", "));
            }
            if current.len() != previous.len() {
                changed = true;
                println!(
                    "[{at:>7.3}s] RAW   {name} : length {} -> {}",
                    previous.len(),
                    current.len()
                );
            }
            self.surfaces.push((name, current));
        }

        changed
    }
}

fn describe(colors: &[Rgb]) -> String {
    colors
        .iter()
        .map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Dim, off, or something else - in words.
fn verdict(before: &[Rgb], after: &[Rgb]) -> &'static str {
    let sum = |colors: &[Rgb]| -> u32 {
        colors
            .iter()
            .map(|c| u32::from(c.r) + u32::from(c.g) + u32::from(c.b))
            .sum()
    };
    let (was, now) = (sum(before), sum(after));
    if now == 0 && was > 0 {
        "every zone went black: the key switches the lighting OFF through the colours"
    } else if now < was {
        "the zones scaled down together: the key DIMS, and the daemon can watch for zero"
    } else if now > was {
        "the zones came back up: the key restored the lighting"
    } else {
        "the colours changed without changing the total - not a brightness step"
    }
}

fn main() {
    let seconds = std::env::args()
        .skip_while(|a| a != "--secs")
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok());

    let everything = std::env::args().any(|a| a == "--all");
    let mut devices = open_all();
    if devices.is_empty() {
        eprintln!(
            "No input device could be opened. /dev/input/event* is root:input - run this with sudo."
        );
        std::process::exit(1);
    }

    println!("Watching {} input devices:", devices.len());
    for device in &devices {
        println!("  {:<22} {}", device.path.display(), device.name);
    }

    let watch_rgb = !std::env::args().any(|a| a == "--no-rgb");
    let mut rgb = if watch_rgb {
        println!("\nProbing the lighting dialects the daemon uses:");
        let watch = RgbWatch::start();
        match &watch {
            Some(w) => println!(
                "  watching firmware state through {}: {}",
                w.dialect.id(),
                describe(&w.colors)
            ),
            None => {
                println!("  no dialect answered - nothing to read, so dim-or-off stays unknown")
            }
        }
        watch
    } else {
        println!("\n--no-rgb: not touching the firmware's lighting state.");
        None
    };

    let watched = sysfs_watchlist();
    let mut previous = snapshot(&watched);
    println!(
        "\nWatching {} sysfs values for firmware-side changes:",
        watched.len()
    );
    for (path, value) in &previous {
        println!("  {} = {value}", path.display());
    }

    println!(
        "\nNow press the keyboard-backlight key (Fn + the lit-keyboard F-key), a few times,\n\
         slowly. Press an ordinary key too - if that shows up and the backlight key does not,\n\
         the answer is case 3 and the EC is keeping it to itself.\n"
    );
    if !everything {
        println!(
            "Pointer noise (touchpad motion, mouse buttons, MSC_TIMESTAMP) is filtered out.\n             Pass --all to see every event instead.\n"
        );
    }
    match seconds {
        Some(s) => println!("Listening for {s}s. --- events follow ---\n"),
        None => println!("Listening until Ctrl-C. --- events follow ---\n"),
    }

    let started = Instant::now();
    let mut buffer = [0u8; EVENT_SIZE * 64];
    let mut quiet = true;

    loop {
        if let Some(limit) = seconds {
            if started.elapsed() >= Duration::from_secs(limit) {
                break;
            }
        }

        let mut fds: Vec<libc::pollfd> = devices
            .iter()
            .map(|d| libc::pollfd {
                fd: d.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: fds is valid for the length passed and poll writes only
        // into revents.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 200) };

        if ready > 0 {
            let mut gone = Vec::new();
            for (index, fd) in fds.iter().enumerate() {
                if fd.revents == 0 {
                    continue;
                }
                if fd.revents & libc::POLLIN == 0 {
                    gone.push(index);
                    continue;
                }
                let device = &mut devices[index];
                let read = match device.file.read(&mut buffer) {
                    Ok(0) => continue,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        gone.push(index);
                        continue;
                    }
                };

                // `.0` drops a trailing partial event: a read can land
                // mid-struct, and the rest arrives on the next one.
                for chunk in buffer[..read].as_chunks::<EVENT_SIZE>().0 {
                    // repr(C) POD out of a byte buffer that may not be aligned.
                    let event: InputEvent =
                        unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
                    if !everything && !worth_printing(event.kind, event.code) {
                        continue;
                    }
                    // SYN_REPORT only ends a packet, and printing it buries
                    // the events that matter even in --all.
                    if event.kind == EV_SYN {
                        continue;
                    }
                    quiet = false;
                    let label = if event.kind == EV_MSC && event.code == MSC_SCAN {
                        "MSC_SCAN".to_string()
                    } else if event.kind == EV_KEY {
                        format!("code {:<4} {}", event.code, key_name(event.code))
                    } else {
                        format!("code {}", event.code)
                    };
                    println!(
                        "[{:>7.3}s] {:<28} {:<7} {:<40} value {}",
                        started.elapsed().as_secs_f32(),
                        device.name,
                        kind_name(event.kind),
                        label,
                        value_name(event.kind, event.value),
                    );
                }
            }
            for index in gone.into_iter().rev() {
                let device = devices.remove(index);
                println!("--- {} went away ---", device.path.display());
            }
        }

        let current = snapshot(&watched);
        for (path, value) in &current {
            if previous.get(path) != Some(value) {
                quiet = false;
                let before = previous.get(path).map_or("?", String::as_str);
                println!(
                    "[{:>7.3}s] SYSFS {} : {before} -> {value}",
                    started.elapsed().as_secs_f32(),
                    path.display(),
                );
            }
        }
        previous = current;

        if let Some(watch) = rgb.as_mut() {
            if watch.poll(started.elapsed().as_secs_f32()) {
                quiet = false;
            }
        }
    }

    if quiet {
        println!(
            "\nNothing at all arrived. Either no key was pressed, or every key on this machine\n\
             is being grabbed by something else."
        );
    }
    println!("\n--- done ---");
}
