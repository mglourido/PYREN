//! The two NVIDIA mechanisms, which are not the same mechanism.
//!
//! | | tool | needs | what it can do |
//! |---|---|---|---|
//! | **offsets** | `nvidia-settings` | an X display whose screen has `Coolbits` | move the whole clock curve up or down |
//! | **clock lock** | `nvidia-smi` | root | pin the clocks to a range the card already supports |
//!
//! The first is overclocking. The second is not - it cannot ask for a
//! frequency the card was not shipped able to run - but it is the one that
//! works on a machine with no X server, and on the laptop this was written
//! on it is the *only* one that works, so it is here rather than left out
//! for being unglamorous.
//!
//! ## Probed, never assumed
//!
//! Which of the two exists depends on the driver version, the session and
//! the X configuration, not on the model of the card
//! (`dev/TODO.md`: "the mechanism differs by driver version, so probe
//! rather than assume"). So everything here answers *this machine now*:
//! the attribute is queried before it is written, the offset ranges are
//! read from the driver rather than hardcoded, and the supported clocks
//! come from the card's own list.
//!
//! ## What was seen on the development laptop
//!
//! Driver 610.57.04, RTX 5060 Laptop, Wayland session with XWayland:
//!
//! - both offset attributes **read** fine, advertising -1000..1000 MHz for
//!   the core and -2000..6000 for memory;
//! - writing one back at its current value is refused with *"The current
//!   user does not have permission for operation"*, which is what a screen
//!   without `Coolbits` says. So the offsets are visible and not settable,
//!   and [`Nvidia::probe_writable`] is how that difference is found out
//!   rather than guessed at.
//!
//! And **as root it fails one step earlier**, which is the more useful of
//! the two findings: a daemon started by systemd is in nobody's session, so
//! the X server answers "Authorization required" before `Coolbits` ever
//! comes up. On a Wayland desktop there is not even a cookie to hand it -
//! Hyprland starts `Xwayland` with no `-auth` file at all - so the fix is
//! the user's (`xhost +si:localuser:root`) or the operator's
//! (`PYREN_X_DISPLAY`, `PYREN_XAUTHORITY`), and this module says which.
//!
//! The **clock lock has been run**, as root, on that same laptop: 900-1200
//! MHz took the idle card from 180 MHz / P8 / 7.5 W to 892 MHz / P5 /
//! 9.9 W, and letting the confirmation lapse put it back. Of the two
//! mechanisms here, the one that needs no X server is the one proven
//! against hardware.
//!
//! That is why the offset path is written to be *possible* and reported as
//! unavailable here, in the same spirit as the lightbar in `pyren-rgb`: the
//! code is ready for the machine that has it, and says plainly which of the
//! ways it is unavailable this machine is in.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;

use crate::plan::{ClockLock, Range};

/// `nvidia-settings` attribute for the core-clock offset, applied to every
/// performance level at once. The per-level attribute exists too and is not
/// used: which level a laptop GPU sits in is the driver's business, and an
/// offset that only applies to one of them is an overclock that silently
/// stops existing when the card drops a state.
const CORE_ATTRIBUTE: &str = "GPUGraphicsClockOffsetAllPerformanceLevels";

/// The memory equivalent. Note it is a *transfer rate* offset - what the
/// tools show as "memory clock" is half of it - which is why its range is
/// several times the core's and why the UI must not present the two as
/// interchangeable numbers.
const MEM_ATTRIBUTE: &str = "GPUMemoryTransferRateOffsetAllPerformanceLevels";

const X11_SOCKETS: &str = "/tmp/.X11-unix";

#[derive(Debug, thiserror::Error)]
pub enum NvidiaError {
    #[error("{0} is not installed")]
    NotInstalled(&'static str),
    /// `nvidia-settings` speaks to an X server, and a daemon started by
    /// systemd is not in anybody's session. Its own kind because the fix is
    /// neither "install something" nor "become root" - running as root is
    /// in fact how a daemon *gets* into this state.
    #[error("nvidia-settings has no X display to talk to: {0}")]
    NoDisplay(String),
    /// The tool ran and the driver said no. The message is the driver's.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NeedsRoot(String),
    #[error("could not read {what}: {detail}")]
    Unreadable { what: &'static str, detail: String },
}

/// One card as `nvidia-smi` lists it.
#[derive(Debug, Clone)]
pub struct NvGpu {
    pub index: u32,
    pub name: String,
    /// The highest graphics clock the card lists, which is the top of the
    /// range a clock lock may ask for.
    pub max_core_mhz: Option<i32>,
}

/// Where an X display was found, and what to authenticate to it with.
#[derive(Debug, Clone)]
pub struct XDisplay {
    pub name: String,
    pub xauthority: Option<PathBuf>,
    /// Who owns the socket. Reported so a failure can say *whose* session
    /// the daemon was trying to reach.
    pub owner_uid: u32,
}

pub struct Nvidia {
    pub smi: bool,
    pub settings: bool,
    pub display: Option<XDisplay>,
}

impl Nvidia {
    pub fn detect() -> Self {
        Self { smi: which("nvidia-smi"), settings: which("nvidia-settings"), display: find_display() }
    }

    // --- nvidia-smi: listing, supported clocks, clock locks -------------

    pub fn gpus(&self) -> Vec<NvGpu> {
        if !self.smi {
            return Vec::new();
        }
        let Some(out) = run_ok(Command::new("nvidia-smi").args([
            "--query-gpu=index,name,clocks.max.gr",
            "--format=csv,noheader,nounits",
        ])) else {
            return Vec::new();
        };
        parse_gpu_list(&out)
    }

    /// The clocks this card lists as supported, ends inclusive. `None` when
    /// the driver will not enumerate them, which is not the same as a card
    /// that cannot be pinned - so the caller reports it as unknown rather
    /// than as a refusal.
    pub fn supported_clocks(&self, index: u32) -> Option<Range> {
        let out = run_ok(Command::new("nvidia-smi").args([
            "-i",
            &index.to_string(),
            "--query-supported-clocks=gr",
            "--format=csv,noheader,nounits",
        ]))?;
        parse_clock_list(&out)
    }

    pub fn lock_clocks(&self, index: u32, lock: ClockLock) -> Result<(), NvidiaError> {
        self.smi_write(index, &format!("--lock-gpu-clocks={},{}", lock.min_mhz, lock.max_mhz))
    }

    pub fn reset_clocks(&self, index: u32) -> Result<(), NvidiaError> {
        self.smi_write(index, "--reset-gpu-clocks")
    }

    fn smi_write(&self, index: u32, argument: &str) -> Result<(), NvidiaError> {
        if !self.smi {
            return Err(NvidiaError::NotInstalled("nvidia-smi"));
        }
        let output = Command::new("nvidia-smi")
            .args(["-i", &index.to_string(), argument])
            .output()
            .map_err(|e| NvidiaError::Unreadable { what: "nvidia-smi", detail: e.to_string() })?;
        if output.status.success() {
            return Ok(());
        }
        let message = message_of(&output.stdout, &output.stderr);
        // nvidia-smi says "Insufficient Permissions" for the one failure a
        // UI should offer to fix rather than report as a dead end.
        if message.to_lowercase().contains("permission") {
            return Err(NvidiaError::NeedsRoot(format!("nvidia-smi refused: {message}")));
        }
        Err(NvidiaError::Refused(format!("nvidia-smi refused: {message}")))
    }

    // --- nvidia-settings: the offsets -----------------------------------

    /// The core offset as it is now, with the range the driver advertises.
    pub fn core_offset(&self, index: u32) -> Result<(i32, Option<Range>), NvidiaError> {
        self.read_attribute(index, CORE_ATTRIBUTE)
    }

    pub fn mem_offset(&self, index: u32) -> Result<(i32, Option<Range>), NvidiaError> {
        self.read_attribute(index, MEM_ATTRIBUTE)
    }

    pub fn set_core_offset(&self, index: u32, mhz: i32) -> Result<(), NvidiaError> {
        self.write_attribute(index, CORE_ATTRIBUTE, mhz)
    }

    pub fn set_mem_offset(&self, index: u32, mhz: i32) -> Result<(), NvidiaError> {
        self.write_attribute(index, MEM_ATTRIBUTE, mhz)
    }

    /// Whether the offsets can actually be *set*, found out by setting the
    /// core offset to the value it already has.
    ///
    /// Reading an attribute proves nothing about writing it: on the machine
    /// this was developed on both offsets read fine and neither can be
    /// written, because the X screen has no `Coolbits`. A no-op assignment
    /// is the only way to tell those apart, and it is a write, so it
    /// happens only when a caller asks for it (`overclock.probe` with
    /// `allowWrites`, mirroring `fan.diagnose`).
    pub fn probe_writable(&self, index: u32) -> Result<bool, NvidiaError> {
        let (current, _) = self.core_offset(index)?;
        match self.set_core_offset(index, current) {
            Ok(()) => Ok(true),
            Err(NvidiaError::Refused(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn settings_command(&self) -> Result<Command, NvidiaError> {
        if !self.settings {
            return Err(NvidiaError::NotInstalled("nvidia-settings"));
        }
        let display = self.display.as_ref().ok_or_else(|| {
            NvidiaError::NoDisplay(format!("no socket in {X11_SOCKETS}"))
        })?;
        let mut command = Command::new("nvidia-settings");
        command.env("DISPLAY", &display.name);
        match &display.xauthority {
            Some(path) => {
                command.env("XAUTHORITY", path);
            }
            // Left unset rather than guessed: an XAUTHORITY pointing at a
            // file that is not a cookie file fails in a way that reads like
            // the driver refusing, and the session may well admit us
            // without one.
            None => {
                command.env_remove("XAUTHORITY");
            }
        }
        Ok(command)
    }

    /// Turns what the driver said into what it means.
    ///
    /// Three refusals are worth translating, and all three were met on the
    /// development laptop in one afternoon:
    ///
    /// - *"the current user does not have permission"* - a missing
    ///   `Coolbits`, not a missing `sudo`: root does not fix it;
    /// - *"Authorization required, but no authorization protocol
    ///   specified"* - the display exists and would not let this process
    ///   in, which is the normal answer to a **root** daemon knocking on a
    ///   desktop session's X server;
    /// - *"the control display is undefined"* - nvidia-settings found no
    ///   display it could open at all, which is also what the one above
    ///   turns into once it gives up.
    ///
    /// The middle one is only visible in the raw output, because
    /// nvidia-settings prints it without the `ERROR:` prefix - so the whole
    /// text is searched and not only the line [`first_error`] picked.
    fn classify(&self, message: String, output: &str) -> NvidiaError {
        if output.to_lowercase().contains("authorization required") {
            return NvidiaError::NoDisplay(self.refused_hint());
        }
        if message.to_lowercase().contains("control display") {
            return NvidiaError::NoDisplay(self.display_hint());
        }
        NvidiaError::Refused(explain(message))
    }

    /// The X server said no. Names the fix that actually exists, which is
    /// on the *user's* side rather than in this daemon.
    ///
    /// A compositor like Hyprland starts `Xwayland` with no `-auth` file at
    /// all - it hands it sockets it has already opened - so there is no
    /// cookie anywhere for a root daemon to be pointed at, and the server
    /// falls back to admitting the uid that owns it. `PYREN_XAUTHORITY`
    /// cannot help with a cookie that does not exist; one line typed inside
    /// the session can.
    fn refused_hint(&self) -> String {
        let display = self.display.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| "?".into());
        format!(
            "the X server at {display} refused this process. It admits the user that owns it, \
             and a daemon running as root is not that user - so either that user lets us in \
             from inside their session (xhost +si:localuser:root), or PYREN_X_DISPLAY and \
             PYREN_XAUTHORITY point us at a display we may open. There may be no cookie file \
             to point at: a Wayland compositor's Xwayland is often started without one"
        )
    }

    /// Which display was tried, whose it is, and what to do about it. The
    /// case where nvidia-settings found nothing it could open at all, as
    /// opposed to [`Self::refused_hint`], where it found one that said no.
    fn display_hint(&self) -> String {
        let Some(display) = &self.display else {
            return format!("no socket in {X11_SOCKETS}");
        };
        // The case that looks permanent and is not: a daemon started by
        // systemd probes at boot, when the only X server on the machine is
        // the display manager's and nobody has logged in yet. Saying so is
        // the difference between "this machine cannot" and "ask again".
        if display.owner_uid == 0 {
            return format!(
                "could not open {} ({}). No desktop session was running when this was asked - \
                 a daemon started at boot looks before anybody has logged in - so ask again \
                 with overclock.probe now that one is up",
                display.name,
                whose(display.owner_uid),
            );
        }
        let cookie = match &display.xauthority {
            Some(path) => format!("with {}", path.display()),
            None => "with no cookie file to authenticate with".to_string(),
        };
        format!(
            "could not open {} ({}) {cookie}. A daemon running as root is not in anybody's \
             session; point it at one with PYREN_X_DISPLAY and PYREN_XAUTHORITY, or set the \
             offsets from a process that is already inside that session",
            display.name,
            whose(display.owner_uid),
        )
    }

    fn read_attribute(
        &self,
        index: u32,
        attribute: &str,
    ) -> Result<(i32, Option<Range>), NvidiaError> {
        let mut command = self.settings_command()?;
        let output = command
            .arg("-q")
            .arg(format!("[gpu:{index}]/{attribute}"))
            .output()
            .map_err(|e| NvidiaError::Unreadable { what: "nvidia-settings", detail: e.to_string() })?;

        // nvidia-settings exits 0 even when it prints only an error, so the
        // status says nothing and the text is what has to be read.
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        parse_attribute(&text, attribute).ok_or_else(|| {
            let detail = first_error(&text)
                .or_else(|| first_error(&stderr))
                .unwrap_or_else(|| format!("nvidia-settings printed no value for {attribute}"));
            self.classify(detail, &format!("{text}\n{stderr}"))
        })
    }

    fn write_attribute(&self, index: u32, attribute: &str, value: i32) -> Result<(), NvidiaError> {
        let mut command = self.settings_command()?;
        let output = command
            .arg("-a")
            .arg(format!("[gpu:{index}]/{attribute}={value}"))
            .output()
            .map_err(|e| NvidiaError::Unreadable { what: "nvidia-settings", detail: e.to_string() })?;

        let text = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        match first_error(&text).or_else(|| first_error(&stderr)) {
            None => Ok(()),
            Some(message) => Err(self.classify(message, &format!("{text}\n{stderr}"))),
        }
    }
}

// --- parsing, kept pure so the shapes can be tested off the hardware ----

/// `0, NVIDIA GeForce RTX 5060 Laptop GPU, 3090` - one line per card.
pub fn parse_gpu_list(text: &str) -> Vec<NvGpu> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let index = fields.next()?.parse().ok()?;
            let name = fields.next()?.to_string();
            let max_core_mhz = fields.next().and_then(|v| v.parse().ok());
            Some(NvGpu { index, name, max_core_mhz })
        })
        .collect()
}

/// The supported-clock list, which arrives one frequency per line, highest
/// first. Only its ends are of interest.
pub fn parse_clock_list(text: &str) -> Option<Range> {
    let clocks: Vec<i32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
    let min = *clocks.iter().min()?;
    let max = *clocks.iter().max()?;
    Some(Range::new(min, max))
}

/// Pulls the value, and the advertised range when there is one, out of what
/// `nvidia-settings -q` prints:
///
/// ```text
///   Attribute 'GPUGraphicsClockOffsetAllPerformanceLevels' ([gpu:0]): 0.
///     The valid values for '...' are in the range -1000 - 1000 (inclusive).
/// ```
pub fn parse_attribute(text: &str, attribute: &str) -> Option<(i32, Option<Range>)> {
    let needle = format!("Attribute '{attribute}'");
    let value_line = text.lines().find(|line| line.contains(&needle))?;
    let value: i32 = value_line.rsplit_once("): ")?.1.trim_end_matches('.').trim().parse().ok()?;

    let range = text
        .lines()
        .find(|line| line.contains("are in the range"))
        .and_then(parse_range);
    Some((value, range))
}

/// `... are in the range -1000 - 1000 (inclusive).`
///
/// Both ends can be negative, so this cannot split on `-`: it takes the
/// signed numbers the line contains, of which the range is the last two.
fn parse_range(line: &str) -> Option<Range> {
    let tail = line.split("are in the range").nth(1)?;
    let numbers: Vec<i32> = tail
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .filter(|token| !token.is_empty() && *token != "-")
        .filter_map(|token| token.parse().ok())
        .collect();
    match numbers.as_slice() {
        [min, max, ..] => Some(Range::new(*min, *max)),
        _ => None,
    }
}

/// Says what the driver's refusal means, where it means something other
/// than what it says.
///
/// "The current user does not have permission for operation" reads like a
/// privilege problem and is not one: running as root does not fix it. It is
/// what an X screen with no `Coolbits` says, and sending somebody to `sudo`
/// over it costs them an afternoon.
fn explain(message: String) -> String {
    if !message.to_lowercase().contains("permission") {
        return message;
    }
    format!(
        "{message} - this is what an X screen with no Coolbits says, and root does not \
         change it: the screen needs Option \"Coolbits\" \"28\" in its X configuration, \
         and a session with no X screen of its own has none to configure"
    )
}

/// The first `ERROR: ...` line, which is how nvidia-settings reports a
/// refusal it still exits 0 for.
fn first_error(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("ERROR:"))
        .map(|line| line.trim_start_matches("ERROR:").trim().to_string())
}

// --- finding somebody's X session --------------------------------------

/// The X display a root daemon should try, or `None` when there is none.
///
/// A daemon started by systemd has no session of its own, so the display
/// has to be found by looking: `/tmp/.X11-unix/X1` means display `:1`, and
/// the socket's owner is whose session it is. This is best-effort by
/// nature, which is why every failure it produces is reported as
/// [`NvidiaError::NoDisplay`] and never as "this machine cannot overclock".
pub fn find_display() -> Option<XDisplay> {
    // Told, rather than guessed. The escape hatch for what guessing cannot
    // solve - a root daemon that has to reach one particular session, whose
    // cookie is somewhere no convention puts it - and what a systemd unit
    // should set on a machine where the offsets actually work.
    if let Some(name) = std::env::var_os("PYREN_X_DISPLAY") {
        let name = name.to_string_lossy().to_string();
        if !name.is_empty() {
            return Some(XDisplay {
                name,
                xauthority: std::env::var_os("PYREN_XAUTHORITY").map(PathBuf::from),
                owner_uid: own_uid(),
            });
        }
    }

    let sockets = x11_sockets();
    let env_display = std::env::var("DISPLAY").ok().filter(|name| !name.is_empty());
    let (name, owner_uid) = pick_display(env_display.as_deref(), &sockets, own_uid())?;

    // The cookie is looked up for whoever owns the display, not for
    // whoever we are: the whole difficulty is that those are different.
    let xauthority = if Some(name.as_str()) == env_display.as_deref() && owner_uid == own_uid() {
        std::env::var_os("XAUTHORITY").map(PathBuf::from).or_else(|| find_xauthority(owner_uid))
    } else {
        find_xauthority(owner_uid)
    };
    Some(XDisplay { xauthority, name, owner_uid })
}

/// Which display to try, given what the environment says and what sockets
/// exist. Pure, because the ordering is the whole decision and it is not
/// something to find out by starting a daemon as root.
///
/// The order, and why:
///
/// 1. **A desktop session's socket** - one owned by somebody who is not
///    root. That is where an NVIDIA X screen with `Coolbits` lives, and it
///    is what a root daemon is looking for.
/// 2. **`$DISPLAY`**, if we are that user ourselves. A developer running
///    the daemon unprivileged inside their own session.
/// 3. **`$DISPLAY`** otherwise, as a last guess.
/// 4. Any socket at all, root's included.
///
/// `$DISPLAY` used to come first, and on the development laptop that was
/// wrong in exactly the way that matters: the root daemon inherited
/// `DISPLAY=:0` - the display manager's X server, owned by root - while
/// the desktop it wanted was `:1`. An inherited `DISPLAY` in a process
/// that belongs to no session is not a session; it is a leftover.
pub fn pick_display(
    env_display: Option<&str>,
    sockets: &[(String, u32)],
    our_uid: u32,
) -> Option<(String, u32)> {
    if let Some(session) = sockets.iter().find(|(_, uid)| *uid != 0) {
        return Some(session.clone());
    }
    if let Some(name) = env_display {
        let owner = sockets
            .iter()
            .find(|(socket, _)| socket == name)
            .map(|(_, uid)| *uid)
            .unwrap_or(our_uid);
        return Some((name.to_string(), owner));
    }
    sockets.first().cloned()
}

/// The X sockets this machine has, as `(":1", owner uid)`, sorted.
fn x11_sockets() -> Vec<(String, u32)> {
    let Ok(entries) = fs::read_dir(X11_SOCKETS) else {
        return Vec::new();
    };
    let mut sockets: Vec<(String, u32)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            // "X1" is a display; "X1_" is a lock file that is not one.
            let number = name.strip_prefix('X')?;
            if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some((format!(":{number}"), entry.metadata().ok()?.uid()))
        })
        .collect();
    sockets.sort();
    sockets
}

/// Where that user's X cookie is likely to be, most-specific first. Every
/// one of these is somebody's convention rather than a standard, so a miss
/// is normal and is not an error.
fn find_xauthority(uid: u32) -> Option<PathBuf> {
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    if let Ok(entries) = fs::read_dir(&runtime) {
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with("xauth") || name.contains("Xwaylandauth")
            })
            .map(|e| e.path())
            .collect();
        candidates.sort();
        if let Some(path) = candidates.into_iter().next() {
            return Some(path);
        }
    }
    let home = home_of(uid)?;
    let dot = home.join(".Xauthority");
    dot.exists().then_some(dot)
}

/// That user's home directory, from `/etc/passwd`. Read rather than looked
/// up through libc so this crate keeps its dependency list at nothing.
fn home_of(uid: u32) -> Option<PathBuf> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() >= 6 && fields[2].parse::<u32>().ok()? == uid)
            .then(|| PathBuf::from(fields[5]))
    })
}

/// Who this process is, read from `/proc/self` rather than through libc:
/// this crate has no C dependency and one number is not a reason to gain
/// one.
/// Whose display this is, in words. Root's own X server is almost always
/// the display manager's greeter rather than anybody's desktop, and saying
/// "uid 0" alone has sent more than one person looking for their session in
/// the wrong place.
fn whose(owner_uid: u32) -> String {
    match owner_uid {
        0 => "owned by root, so probably the display manager rather than a desktop".to_string(),
        uid if uid == own_uid() => format!("our own session, uid {uid}"),
        uid => format!("the session of uid {uid}"),
    }
}

fn own_uid() -> u32 {
    fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

fn which(program: &str) -> bool {
    std::env::var("PATH")
        .map(|path| {
            path.split(':').any(|dir| std::path::Path::new(dir).join(program).is_file())
        })
        .unwrap_or(false)
}

fn run_ok(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

fn message_of(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(stdout).trim(),
        String::from_utf8_lossy(stderr).trim()
    );
    let text = text.trim().to_string();
    if text.is_empty() {
        "no output".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from the development laptop: driver 610.57.04,
    /// RTX 5060 Laptop GPU, Wayland session.
    const CORE_QUERY: &str = "\
  Attribute 'GPUGraphicsClockOffsetAllPerformanceLevels' ([gpu:0]): 0.
    The valid values for 'GPUGraphicsClockOffsetAllPerformanceLevels' are in the range -1000 - 1000 (inclusive).
    'GPUGraphicsClockOffsetAllPerformanceLevels' can use the following target types: GPU.
";

    const REFUSED: &str = "

ERROR: The current user does not have permission for operation


ERROR: Error assigning value 0 to attribute 'GPUGraphicsClockOffsetAllPerformanceLevels' ([gpu:0]) as specified in assignment '[gpu:0]/GPUGraphicsClockOffsetAllPerformanceLevels=0' (Operation not permitted for the current user).
";

    #[test]
    fn an_attribute_query_yields_the_value_and_the_range() {
        let (value, range) = parse_attribute(CORE_QUERY, CORE_ATTRIBUTE).expect("a value");
        assert_eq!(value, 0);
        assert_eq!(range, Some(Range::new(-1000, 1000)));
    }

    /// The range's ends can both be negative, which is why the line is not
    /// split on the dash between them.
    #[test]
    fn a_range_with_negative_ends_is_read_correctly() {
        let text = "  Attribute 'X' ([gpu:0]): -50.\n    The valid values for 'X' are in the range -2000 - -100 (inclusive).";
        let (value, range) = parse_attribute(text, "X").expect("a value");
        assert_eq!(value, -50);
        assert_eq!(range, Some(Range::new(-2000, -100)));
    }

    /// A refusal prints no `Attribute` line at all, and nvidia-settings
    /// still exits 0 - so "no value" is the only signal there is, and it
    /// must not be read as an offset of zero.
    #[test]
    fn a_refusal_is_not_read_as_a_value() {
        assert!(parse_attribute(REFUSED, CORE_ATTRIBUTE).is_none());
        assert!(first_error(REFUSED).unwrap().contains("does not have permission"));
    }

    /// The refusal that costs the most time to misread: it is a missing
    /// Coolbits, not a missing `sudo`, and the message has to say so.
    #[test]
    fn a_permission_refusal_from_the_x_screen_names_coolbits() {
        let explained = explain(first_error(REFUSED).unwrap());
        assert!(explained.contains("Coolbits"));
        assert!(explained.contains("root does not"));
        assert_eq!(explain("no such attribute".to_string()), "no such attribute");
    }

    fn nvidia_with(display: Option<XDisplay>) -> Nvidia {
        Nvidia { smi: false, settings: false, display }
    }

    /// What a root daemon really gets on a Wayland desktop: the server
    /// admits the uid that owns it and nobody else, and there is no cookie
    /// file anywhere to be pointed at. The reply has to say so, because the
    /// fix is a line typed *inside the session*, not a setting here.
    #[test]
    fn an_x_server_refusing_root_names_the_fix_that_exists() {
        let nvidia = nvidia_with(Some(XDisplay {
            name: ":1".into(),
            xauthority: None,
            owner_uid: 1000,
        }));
        let e = nvidia.classify(
            "The control display is undefined".into(),
            "Authorization required, but no authorization protocol specified\n\nERROR: The control display is undefined",
        );
        assert!(matches!(e, NvidiaError::NoDisplay(_)));
        let message = e.to_string();
        assert!(message.contains("xhost +si:localuser:root"), "{message}");
        assert!(message.contains(":1"));
    }

    /// The other way it fails: nothing to open at all. Names the display it
    /// tried, whose session that is, and the two variables that override it.
    #[test]
    fn a_display_that_could_not_be_opened_is_reported_as_having_no_display() {
        let nvidia = nvidia_with(Some(XDisplay {
            name: ":1".into(),
            xauthority: None,
            owner_uid: 1000,
        }));
        let e = nvidia.classify("The control display is undefined".into(), "");
        assert!(matches!(e, NvidiaError::NoDisplay(_)));
        let message = e.to_string();
        assert!(message.contains(":1") && message.contains("1000"));
        assert!(message.contains("PYREN_X_DISPLAY"));
        assert!(message.contains("no cookie file"), "the missing cookie is half the diagnosis");
    }

    /// ...and a refusal that is about neither keeps its own explanation, so
    /// Coolbits and the X server are never mistaken for one another.
    #[test]
    fn a_coolbits_refusal_is_not_turned_into_a_display_problem() {
        let e = nvidia_with(None).classify(first_error(REFUSED).unwrap(), REFUSED);
        assert!(matches!(e, NvidiaError::Refused(_)));
        assert!(e.to_string().contains("Coolbits"));
    }

    /// The bug this ordering exists for, as it happened: a root daemon
    /// with `DISPLAY=:0` inherited from whatever started it, while the
    /// desktop it wants is `:1`. The session wins over the leftover.
    #[test]
    fn a_desktop_session_beats_an_inherited_display() {
        let sockets = vec![(":0".to_string(), 0), (":1".to_string(), 1000)];
        assert_eq!(pick_display(Some(":0"), &sockets, 0), Some((":1".to_string(), 1000)));
    }

    /// A developer running the daemon unprivileged inside their own
    /// session still lands on their own display - it is the session socket
    /// too, so the first rule already covers them.
    #[test]
    fn an_unprivileged_run_lands_on_its_own_session() {
        let sockets = vec![(":1".to_string(), 1000)];
        assert_eq!(pick_display(Some(":1"), &sockets, 1000), Some((":1".to_string(), 1000)));
    }

    /// With no session socket to be found, `$DISPLAY` is still a better
    /// guess than nothing - and than root's own greeter.
    #[test]
    fn an_inherited_display_is_used_when_there_is_no_session() {
        let sockets = vec![(":0".to_string(), 0)];
        assert_eq!(pick_display(Some(":7"), &sockets, 0), Some((":7".to_string(), 0)));
        assert_eq!(pick_display(None, &sockets, 0), Some((":0".to_string(), 0)));
        assert_eq!(pick_display(None, &[], 0), None);
    }

    /// A daemon started by systemd probes before anybody has logged in, so
    /// the only X server it can find is the display manager's. That is a
    /// "come back later", not a verdict on the machine, and the reply has
    /// to say which of the two it is.
    #[test]
    fn a_probe_taken_before_anyone_logged_in_says_to_ask_again() {
        let nvidia = nvidia_with(Some(XDisplay {
            name: ":0".into(),
            xauthority: None,
            owner_uid: 0,
        }));
        let message = nvidia.classify("The control display is undefined".into(), "").to_string();
        assert!(message.contains("overclock.probe"), "{message}");
        assert!(message.contains("No desktop session was running"));
    }

    /// Root's display is not "ours" in any useful sense, and calling it
    /// that sent somebody looking for their session in the wrong place.
    #[test]
    fn a_root_owned_display_is_never_called_our_session() {
        assert!(whose(0).contains("display manager"));
        assert!(!whose(0).contains("our own"));
    }

    #[test]
    fn the_gpu_list_survives_a_name_with_no_clock_beside_it() {
        let gpus = parse_gpu_list("0, NVIDIA GeForce RTX 5060 Laptop GPU, 3090\n1, Quadro, [N/A]\n");
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].max_core_mhz, Some(3090));
        assert_eq!(gpus[1].max_core_mhz, None, "an unreadable clock is absent, not zero");
    }

    #[test]
    fn the_supported_clocks_become_a_range() {
        assert_eq!(parse_clock_list("3090\n3082\n3075\n210\n"), Some(Range::new(210, 3090)));
        assert_eq!(parse_clock_list(""), None);
    }
}
