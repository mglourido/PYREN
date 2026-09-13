//! `/proc/acpi/call`: one file, one lock.
//!
//! `acpi_call` exposes a *single global* interface. A call is a write
//! followed by a read of the same file, and the two are not tied together
//! by anything - if a second process writes between our write and our
//! read, we read its answer and it reads ours. A short-lived CLI gets away
//! with that; a daemon with a control loop does not.
//!
//! So every use of the file in this process goes through [`call`], which
//! hands the write/read pair to one worker thread that does them one at a
//! time. This lives in `core` rather than in a module because more than one
//! module needs it: the RGB lightbar drives the light strip through it, and
//! the fan cleaner drives reverse spin through it. Two modules serialising
//! against two different locks would be two modules not serialising at all.
//!
//! The two also speak the *same* dialect over it - HP's `SECU` buffer
//! protocol - so [`wmi_request`] builds the argument and [`parse_bytes`]
//! reads the reply for both. They lived in the lightbar until the cleaner
//! needed them, and a second copy of a hex parser is a second copy of its
//! bugs.
//!
//! Other programs are covered as far as they cooperate: each pair is also
//! taken under an advisory `flock(2)` on [`LOCK_PATH`], and
//! `tools/pyren-check.sh` takes the same one. A program that ignores the
//! lock is still outside it, and nothing short of the kernel could fix
//! that - which is why the four-zone writer also checks the shape of what
//! it read before sending anything back (see `pyren_rgb::fourzone`).
//!
//! ## Guards
//!
//! - **A deadline.** A call that has not come back within [`CALL_TIMEOUT`]
//!   is reported as failed, and while it is still stuck in the kernel no
//!   further call is sent: a firmware that hangs on one request is not
//!   handed a queue of them.
//! - **No late writes.** A request whose caller has already given up is
//!   dropped rather than sent.
//! - **A minimum gap** of [`MIN_GAP`] between calls, so no caller - an
//!   animation, say - can hammer the firmware.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The interface, when the kernel module is loaded.
pub const CALL_PATH: &str = "/proc/acpi/call";

/// What to tell a user whose machine has no `acpi_call`. Written once
/// because it is the entire remedy, and an error that names the hardware
/// but not the package sends people to the wrong place.
pub const INSTALL_HINT: &str = "install the acpi_call kernel module: \
     'sudo pacman -S acpi_call-dkms' on Arch, \
     'sudo apt install acpi_call-dkms' on Debian/Ubuntu, \
     'sudo dnf install akmod-acpi_call' on Fedora";

/// What to tell a user whose machine *has* the module and has not loaded
/// it. A different sentence from [`INSTALL_HINT`] because it is a
/// different fix, and offering a package install to somebody who already
/// ran it is how a two-minute problem becomes an evening.
pub const MODPROBE_HINT: &str = "the acpi_call module is installed but not loaded: \
     'sudo modprobe acpi_call' (a daemon running as root loads it itself)";

/// The remedy for a missing `/proc/acpi/call` **on this machine**, which is
/// the package or the `modprobe` depending on what is already here.
///
/// Costs one `modinfo`, and is only ever reached on an error path. Every
/// other place in this project that reports a missing interface already
/// tells the two apart; this exists so the error type does too.
pub fn missing_hint() -> &'static str {
    if is_module_installed() {
        MODPROBE_HINT
    } else {
        INSTALL_HINT
    }
}

/// The advisory lock every write/read pair is taken under, shared with
/// `tools/pyren-check.sh`. `/run/lock` is the FHS place for it and is
/// writable on every systemd distribution.
pub const LOCK_PATH: &str = "/run/lock/pyren-acpi-call.lock";

/// The longest one call may take. A healthy call is a few milliseconds.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The shortest time between the end of one call and the start of the next.
pub const MIN_GAP: Duration = Duration::from_millis(8);

/// Set while a call has outlived its deadline and is still inside the
/// kernel. See the module docs.
static STUCK: AtomicBool = AtomicBool::new(false);

/// The worker's inbox. Replaced if the worker ever goes away.
static WORKER: Mutex<Option<mpsc::Sender<Job>>> = Mutex::new(None);

struct Job {
    path: String,
    request: String,
    deadline: Instant,
    reply: mpsc::Sender<Result<String, AcpiError>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AcpiError {
    /// `/proc/acpi/call` is not there. Installable, not permanent.
    #[error(
        "the acpi_call kernel module is not loaded, so {} does not exist; {}",
        CALL_PATH,
        missing_hint()
    )]
    NotLoaded,
    /// The file exists and this process may not write to it.
    #[error("writing {CALL_PATH} needs root")]
    PermissionDenied,
    #[error("{CALL_PATH}: {0}")]
    Io(String),
}

impl AcpiError {
    /// The same sentence, translatable. The `Io` variant carries a raw OS
    /// error, which is passed through as a param rather than translated.
    pub fn to_msg(&self) -> crate::Msg {
        match self {
            Self::NotLoaded => crate::msg!(
                "acpi.notLoaded",
                { "path" => CALL_PATH, "hint" => missing_hint() },
                "the acpi_call kernel module is not loaded, so {path} does not exist; {hint}"
            ),
            Self::PermissionDenied => crate::msg!(
                "acpi.needsRoot",
                { "path" => CALL_PATH },
                "writing {path} needs root"
            ),
            Self::Io(e) => crate::msg!(
                "acpi.io",
                { "path" => CALL_PATH, "error" => e.clone() },
                "{path}: {error}"
            ),
        }
    }
}

/// Where the interface is. `PYREN_ACPI_CALL` redirects it at a plain file,
/// which is how the request framing is exercised in a test on a machine
/// with no `acpi_call` - including in CI.
pub fn call_path() -> String {
    test_override("PYREN_ACPI_CALL").unwrap_or_else(|| CALL_PATH.to_string())
}

/// A `PYREN_*` variable that points hardware access at a fixture.
///
/// Honoured for an unprivileged process - a test, the parity check - and
/// **ignored for root** unless `PYREN_TEST_OVERRIDES=1` is set as well. A
/// root daemon that inherited a stray `PYREN_ACPI_CALL` would otherwise
/// truncate whatever file it names, and one with a stray
/// `PYREN_RGB_ZONES_DIR` would write colours into it.
pub fn test_override(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    if !is_root() || std::env::var("PYREN_TEST_OVERRIDES").is_ok_and(|v| v == "1") {
        return Some(value);
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        crate::log_warn!(
            "ignoring {name} and the other PYREN_* test overrides: this process is root \
             (set PYREN_TEST_OVERRIDES=1 as well if that is really meant)"
        );
    }
    None
}

fn is_root() -> bool {
    // SAFETY: geteuid(2) cannot fail and touches no memory.
    unsafe { libc::geteuid() == 0 }
}

/// The lock file for `path`: [`LOCK_PATH`] for the real interface, and a
/// sibling of a redirected one, so a test never touches `/run/lock`.
fn lock_path_for(path: &str) -> String {
    if path == CALL_PATH {
        LOCK_PATH.to_string()
    } else {
        format!("{path}.lock")
    }
}

/// Whether the interface is there *now*. Never loads anything: probing is
/// a question, and a question should not change the answer.
pub fn is_loaded() -> bool {
    Path::new(&call_path()).exists()
}

/// Whether the module is installed but not loaded - the state a machine is
/// in between `pacman -S acpi_call-dkms` and the next `modprobe`.
///
/// Told apart from "not installed at all" because they have different
/// remedies, and a message offering the wrong one costs an evening.
///
/// Remembered for [`MODINFO_TTL`]: it is reached from status reads that a
/// UI polls, and each answer is a process start.
pub fn is_module_installed() -> bool {
    static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, answer)) = *cache {
        if at.elapsed() < MODINFO_TTL {
            return answer;
        }
    }
    let answer = std::process::Command::new("modinfo")
        .args(["-n", "acpi_call"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    *cache = Some((Instant::now(), answer));
    answer
}

/// How long [`is_module_installed`] trusts its last answer.
pub const MODINFO_TTL: Duration = Duration::from_secs(5);

/// Loads `acpi_call` if it is installed and we are root.
///
/// Only ever called from a path where the user has asked for something
/// that needs it. The daemon does not modprobe at startup: loading a
/// kernel module is a change to the machine, and this project does not
/// make those on its own (see `dev/TODO.md` §3, "the daemon does not touch
/// the fans until asked").
pub fn ensure_loaded() -> Result<(), AcpiError> {
    if is_loaded() {
        return Ok(());
    }
    let _ = std::process::Command::new("modprobe")
        .arg("acpi_call")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    if is_loaded() {
        Ok(())
    } else {
        Err(AcpiError::NotLoaded)
    }
}

/// One ACPI call: write `<method> <args>`, then read the reply.
///
/// Both halves run on the one worker thread, under the shared file lock,
/// which is the whole reason this function exists rather than each caller
/// opening the file itself. See the module docs for the guards.
pub fn call(method: &str, args: &str) -> Result<String, AcpiError> {
    call_at(&call_path(), method, args, CALL_TIMEOUT)
}

fn call_at(path: &str, method: &str, args: &str, timeout: Duration) -> Result<String, AcpiError> {
    if STUCK.load(Ordering::SeqCst) {
        return Err(AcpiError::Io(
            "an earlier firmware call has not come back yet, so no new one was sent".into(),
        ));
    }
    let (reply, answer) = mpsc::channel();
    let job = Job {
        path: path.to_string(),
        request: format!("{method} {args}"),
        deadline: Instant::now() + timeout,
        reply,
    };
    submit(job)?;
    match answer.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            STUCK.store(true, Ordering::SeqCst);
            Err(AcpiError::Io(format!(
                "the firmware call did not finish within {} ms",
                timeout.as_millis()
            )))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(AcpiError::Io("the firmware call worker went away".into()))
        }
    }
}

/// Hands a job to the worker, starting one if there is none.
fn submit(job: Job) -> Result<(), AcpiError> {
    let mut worker = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    let job = match worker.as_ref() {
        Some(sender) => match sender.send(job) {
            Ok(()) => return Ok(()),
            // The worker died (a panic); its job comes back to be re-sent.
            Err(mpsc::SendError(job)) => job,
        },
        None => job,
    };
    let (sender, inbox) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("pyren-acpi-call".into())
        .spawn(move || work(inbox))
        .map_err(|e| AcpiError::Io(format!("could not start the firmware call worker: {e}")))?;
    sender
        .send(job)
        .map_err(|_| AcpiError::Io("the firmware call worker went away".into()))?;
    *worker = Some(sender);
    Ok(())
}

fn work(inbox: mpsc::Receiver<Job>) {
    let mut last_done: Option<Instant> = None;
    for job in inbox {
        if let Some(done) = last_done {
            let ready = done + MIN_GAP;
            let now = Instant::now();
            if ready > now {
                std::thread::sleep(ready - now);
            }
        }
        // The caller has already been told this failed; sending it now
        // would be a write nobody is waiting for.
        if Instant::now() >= job.deadline {
            let _ = job.reply.send(Err(AcpiError::Io(
                "the firmware call was dropped: its caller had stopped waiting".into(),
            )));
            continue;
        }
        let result = exchange(&job.path, &job.request, job.deadline);
        last_done = Some(Instant::now());
        STUCK.store(false, Ordering::SeqCst);
        let _ = job.reply.send(result);
    }
}

/// The write/read pair itself, under the cross-process lock.
fn exchange(path: &str, request: &str, deadline: Instant) -> Result<String, AcpiError> {
    let _lock = FileLock::acquire(&lock_path_for(path), deadline)?;
    let redirected = path != CALL_PATH;

    let mut file = fs::OpenOptions::new()
        .write(true)
        // procfs ignores truncation; a redirect target is a real file and
        // would otherwise keep the tail of a longer previous request.
        .truncate(redirected)
        .create(redirected)
        .open(path)
        .map_err(map_open_error)?;
    file.write_all(request.as_bytes())
        .map_err(|e| map_io_error(&e))?;
    drop(file);

    let response = read_reply(path)?;
    // acpi_call terminates its reply with a NUL, which `trim` does not
    // remove and `str::parse` chokes on.
    Ok(response
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string())
}

/// An advisory `flock(2)`, released on drop.
///
/// Best effort where the lock file cannot be opened at all - an
/// unprivileged run with no `/run/lock` - because the in-process worker
/// still serialises this daemon's own calls. A lock that *exists* and is
/// held past the deadline is a refusal: somebody else is mid-call.
struct FileLock(Option<fs::File>);

impl FileLock {
    fn acquire(path: &str, deadline: Instant) -> Result<Self, AcpiError> {
        use std::os::fd::AsRawFd;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .or_else(|_| fs::File::open(path));
        let Ok(file) = file else {
            return Ok(Self(None));
        };
        loop {
            // SAFETY: a valid, open descriptor owned by `file`.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(Some(file)));
            }
            if Instant::now() >= deadline {
                return Err(AcpiError::Io(format!(
                    "another program is using the firmware interface (lock {path} is held)"
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        if let Some(file) = &self.0 {
            // SAFETY: as above. Closing the file would release it too; this
            // just does not wait for the drop order to get there.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// How big a first read to ask for. Comfortably past `acpi_call`'s own
/// result buffer, which is a few hundred bytes.
const REPLY_CAPACITY: usize = 8192;

/// Reads the reply `acpi_call` has waiting.
///
/// **Not `fs::read_to_string`, and that is the whole point of this
/// function.** `/proc/acpi/call` reports a size of zero, like most of
/// procfs, so `read_to_string` has no hint to size its buffer with and
/// opens by probing with a very small one. This interface answers a small
/// read with *nothing at all* rather than with the first few bytes, so
/// `read_to_string` sees zero bytes, reads that as end-of-file, and returns
/// an empty string - for a call the firmware answered perfectly well.
///
/// Every symptom of that is a lie about the hardware: an empty reply is not
/// `PASS`, so it is reported as the firmware refusing, and a refusal reads
/// as "this machine cannot do it". It cost this project a wrong entry in
/// `dev/FINDINGS.md` about a lightbar, and the fan cleaner - which speaks
/// through the same file - was reporting "this machine has no fan cleaner"
/// for the same reason.
///
/// So: one big read, and keep reading only while bytes keep arriving.
fn read_reply(path: &str) -> Result<String, AcpiError> {
    let mut file = fs::File::open(path).map_err(map_open_error)?;
    let mut buffer = vec![0u8; REPLY_CAPACITY];
    let mut filled = 0;

    loop {
        if filled == buffer.len() {
            buffer.resize(buffer.len() * 2, 0);
        }
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(map_io_error(&e)),
        }
    }

    buffer.truncate(filled);
    // Lossy on purpose: the reply is ASCII, and a stray byte in it is worth
    // reporting as a bad reply rather than as an I/O failure - the two send
    // a reader to different places.
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// HP's WMI buffer protocol, as the hex argument `acpi_call` takes.
///
/// One 16-byte little-endian header - the ASCII signature `SECU`, the
/// command, the command type and the payload size - followed by the
/// payload itself, zero-padded or truncated to `size`. Both the lightbar
/// and the fan cleaner send exactly this; only the numbers differ.
///
/// The whole thing is prefixed `b`, which is how `acpi_call` is told the
/// argument is a buffer rather than an integer.
pub fn wmi_request(command: u32, command_type: u32, size: usize, payload: &[u8]) -> String {
    let mut buffer = Vec::with_capacity(HEADER_LEN + size);
    buffer.extend_from_slice(SIGNATURE);
    buffer.extend_from_slice(&command.to_le_bytes());
    buffer.extend_from_slice(&command_type.to_le_bytes());
    buffer.extend_from_slice(&(size as u32).to_le_bytes());

    buffer.extend_from_slice(&payload[..payload.len().min(size)]);
    buffer.resize(HEADER_LEN + size, 0);

    let mut hex = String::with_capacity(1 + buffer.len() * 2);
    hex.push('b');
    for byte in &buffer {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The ACPI method every HP WMI BIOS call goes through.
///
/// One method, called with an instance and a *numbered* method id: the
/// firmware exposes several `WMAA` entry points that differ only in how
/// much they are willing to hand back, and picking the wrong one is a
/// refusal rather than a short read.
pub const WMI_METHOD: &str = "\\_SB.WMID.WMAA";

/// Which numbered WMI method to call for a given expected output size.
///
/// This is the kernel driver's own `encode_outsize_for_pvsz`, byte for
/// byte (`drivers/platform/x86/hp/hp-wmi.c`). It is not a detail worth
/// re-deriving per caller: a write that expects nothing back is method 1,
/// and asking for method 3 - which promises up to 128 bytes - is a
/// different request as far as the firmware is concerned.
pub fn method_for_outsize(outsize: usize) -> u32 {
    match outsize {
        0 => 1,
        1..=4 => 2,
        5..=128 => 3,
        129..=1024 => 4,
        _ => 5,
    }
}

/// One HP WMI BIOS call: build the buffer, pick the method, send it.
///
/// `insize` is the `datasize` header field - what the firmware is told the
/// payload is - and `outsize` is how much of an answer is expected, which
/// is what chooses the method. Both come from the reference driver for the
/// call being made; they are not free parameters.
pub fn wmi_call(
    command: u32,
    command_type: u32,
    payload: &[u8],
    insize: usize,
    outsize: usize,
) -> Result<String, AcpiError> {
    let request = wmi_request(command, command_type, insize, payload);
    call(
        WMI_METHOD,
        &format!("0 {} {request}", method_for_outsize(outsize)),
    )
}

/// The ASCII signature every one of these buffers starts with. It is the
/// driver's own `bios_args.signature` (`0x55434553`) written as letters.
pub const SIGNATURE: &[u8; 4] = b"SECU";

/// The header [`wmi_request`] writes before the payload.
pub const HEADER_LEN: usize = 16;

/// The bytes behind an `acpi_call` reply.
///
/// Three shapes turn up depending on the kernel and the `acpi_call`
/// version, and all three are accepted: a bare hex blob, a
/// `{0x50, 0x41, ...}` list, and the same list without spaces.
///
/// **The prefix is stripped once, on purpose.** Upstream (both projects
/// this was ported from) uses `lstrip("b0x")`, and `str.lstrip` takes a
/// *character set* rather than a prefix: `'0xb0b0aa'.lstrip('b0x')` is
/// `'aa'`, three bytes of real data gone, and any reply whose first byte
/// is zero loses that byte too.
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
        let parsed: Option<Vec<u8>> = tokens
            .iter()
            .map(|t| u8::from_str_radix(t, 16).ok())
            .collect();
        if let Some(bytes) = parsed {
            return Some(bytes);
        }
    }

    let blob = text.trim_matches(|c| c == '{' || c == '}').trim();
    let blob = blob
        .strip_prefix("0x")
        .or_else(|| blob.strip_prefix('b'))
        .unwrap_or(blob);
    let blob: String = blob
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\0')
        .collect();
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
    if text.is_empty()
        || !text.len().is_multiple_of(2)
        || !text.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let bytes: Vec<u8> = text
        .as_bytes()
        .chunks(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect();
    (bytes.len() == text.len() / 2).then_some(bytes)
}

fn map_open_error(e: std::io::Error) -> AcpiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AcpiError::NotLoaded,
        std::io::ErrorKind::PermissionDenied => AcpiError::PermissionDenied,
        _ => AcpiError::Io(e.to_string()),
    }
}

fn map_io_error(e: &std::io::Error) -> AcpiError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AcpiError::NotLoaded,
        std::io::ErrorKind::PermissionDenied => AcpiError::PermissionDenied,
        _ => AcpiError::Io(e.to_string()),
    }
}

#[cfg(test)]
mod tests {

    /// The bug that cost this project a wrong finding about its hardware.
    ///
    /// `/proc/acpi/call` answers a small read with nothing at all, and
    /// `fs::read_to_string` opens by probing with a small buffer because
    /// procfs reports a size of zero. The result was an empty reply for a
    /// call the firmware had answered - reported, all the way up, as the
    /// machine refusing. A redirected file cannot reproduce the kernel's
    /// half of that, so what this pins is the half that is ours: the reply
    /// comes back whole, however big it is.
    #[test]
    fn a_reply_longer_than_a_probe_read_comes_back_whole() {
        let dir = std::env::temp_dir().join(format!("pyren-acpi-reply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp dir is writable");
        let path = dir.join("call");

        // Longer than REPLY_CAPACITY, so the growth path is exercised too.
        let reply: String = std::iter::repeat_n("{0x50, 0x41, 0x53, 0x53}", 1000).collect();
        std::fs::write(&path, &reply).expect("a temp file is writable");

        let got = read_reply(path.to_str().unwrap()).expect("a readable file");
        assert_eq!(got.len(), reply.len(), "the reply must not be cut short");
        assert_eq!(got, reply);

        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    /// One test, not three: `PYREN_ACPI_CALL` is process-global and the
    /// test harness runs threads in parallel, so cases that set it have to
    /// be cases that cannot run at the same time as each other.
    #[test]
    fn the_request_framing_and_the_absent_interface() {
        let dir = std::env::temp_dir().join(format!("pyren-acpi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("call");
        // Restored rather than removed at the end: on a machine where
        // `acpi_call` is loaded, deleting the variable does not put the
        // test's redirection back, it exposes the real firmware
        // interface to whatever runs next.
        let previous = std::env::var_os("PYREN_ACPI_CALL");
        std::env::set_var("PYREN_ACPI_CALL", &path);

        // The request is one line, `<method> <args>`, with exactly one
        // space: acpi_call parses positionally, and a stray separator
        // shifts every argument along by one. Reading a regular file back
        // returns the request itself, which is the thing under test.
        let echoed = call("\\_SB.WMID.WMAA", "0 3 b53454355").expect("a plain file accepts it");
        assert_eq!(echoed, "\\_SB.WMID.WMAA 0 3 b53454355");

        // A shorter request must not leave the tail of a longer one behind.
        assert_eq!(call("\\_SB", "0").unwrap(), "\\_SB 0");

        std::env::set_var("PYREN_ACPI_CALL", dir.join("definitely-not-here/call"));
        assert!(
            matches!(call("\\_SB", "0"), Err(AcpiError::NotLoaded)),
            "a missing interface is 'not loaded', which names a fix, not a bare io error"
        );

        match previous {
            Some(previous) => std::env::set_var("PYREN_ACPI_CALL", previous),
            None => std::env::remove_var("PYREN_ACPI_CALL"),
        }

        // A call that hangs. Opening a FIFO for writing blocks until
        // somebody opens it for reading, which is as close to a firmware
        // that never answers as a test can get.
        let fifo = dir.join("stuck");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let fifo_path = fifo.to_str().unwrap().to_string();
        let started = Instant::now();
        let hung = call_at(&fifo_path, "\\_SB", "0", Duration::from_millis(200));
        assert!(
            matches!(&hung, Err(AcpiError::Io(e)) if e.contains("did not finish")),
            "a hung call is reported, not waited on forever: {hung:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let refused = call_at(&fifo_path, "\\_SB", "0", Duration::from_millis(200));
        assert!(
            matches!(&refused, Err(AcpiError::Io(e)) if e.contains("has not come back")),
            "nothing more is sent while one call is stuck: {refused:?}"
        );

        // Let the stuck call through: read its request, then answer it.
        let request = std::fs::read_to_string(&fifo).unwrap();
        assert_eq!(request, "\\_SB 0");
        std::fs::write(&fifo, "PASS").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while STUCK.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!STUCK.load(Ordering::SeqCst), "the worker recovers");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A test's redirected interface never takes the machine's real lock,
    /// and the real interface always does.
    #[test]
    fn a_lock_file_sits_next_to_a_redirected_interface() {
        assert_eq!(lock_path_for(CALL_PATH), LOCK_PATH);
        assert_eq!(lock_path_for("/tmp/x/call"), "/tmp/x/call.lock");
    }
}
