//! The fan cleaner: dust removal by spinning the fans **backwards**.
//!
//! Ported from `_cleaner.py` in the `omen-fan-control` project, whose
//! `docs/04-fan-control-logic.md` §"Fan cleaner protocol" is the spec this
//! follows. It is the one feature here that bypasses the kernel driver
//! entirely and talks to the firmware over `/proc/acpi/call`.
//!
//! ## The protocol
//!
//! One ACPI method, `\_SB.WMID.WMAA`, called as `<method> 0 <id> b<hex>`.
//! The buffer is HP's `SECU` header plus a payload - the same dialect the
//! RGB lightbar speaks, which is why [`pyren_core::acpi::wmi_request`]
//! builds it for both. `<id>` selects the numbered WMAA method by output
//! size, exactly like the driver's own `encode_outsize_for_pvsz`: 4-byte
//! buffers go to `WMAA2`, 128-byte ones to `WMAA3`.
//!
//! The reply is a signature (`PASS` / `FAIL`), a little-endian `u32`
//! return code, and the data.
//!
//! There are **two firmware generations**, and a machine may have either:
//!
//! - **modern** ("CleanCreek"): a 128-byte buffer holding a per-fan speed
//!   in hundreds of RPM, with bit 7 meaning *reverse*. Speeds are
//!   commanded, so the cycle can be braked, engaged and ramped down.
//! - **legacy**: a 4-byte control buffer with a single toggle bit. No
//!   speed, no ramp - it is on or it is off.
//!
//! ## Why so much of this file is about stopping
//!
//! Reverse spin is **cooling switched off**, not cooling turned down: for
//! as long as a cycle runs the machine has no working fans. So the timeout
//! is enforced three times over - by the caller's own watchdog thread, by
//! [`Cycle::expired`] on every status read, and by the fan control loop's
//! tick - and every failure path in [`start`] rolls back through
//! [`emergency_stop`]. The original does the same, and for the same
//! reason: the consequence of leaving the fans reversed is not a bad
//! reading, it is hardware.
//!
//! ## What is not known
//!
//! The `(command, command_type)` pairs below are undocumented in the
//! driver source - reverse-engineered by the upstream project, and **not
//! confirmed against hardware by this project**: `acpi_call` is loaded on
//! the development laptop now (it is what drives the lighting - see
//! `dev/FINDINGS.md`), but the capability query has never been put to this
//! machine's firmware, and board 8D2F may have no fan cleaner to answer it
//! anyway. Everything that can be tested
//! without the firmware - the buffers built, the replies parsed, the
//! capability decoding, the guards - is tested at the bottom of this file,
//! so what is left untested when somebody does run it is the firmware's
//! own answer.

use std::time::{Duration, Instant};

use pyren_core::{acpi, msg, Msg};
use serde::{Deserialize, Serialize};

/// The method every one of these calls goes to.
const METHOD: &str = "\\_SB.WMID.WMAA";

/// WMAA method ids, chosen by buffer size (see the module docs).
const ID_LEGACY: u8 = 2;
const ID_MODERN: u8 = 3;

/// `HPWMI_GM`, the modern query/set command.
const CMD_MODERN: u32 = 0x0002_0008;
/// The legacy 4-byte buffer: 1 reads it, 2 writes it back.
const CMD_LEGACY_READ: u32 = 1;
const CMD_LEGACY_WRITE: u32 = 2;

/// Command types. **44 asks, 46 sets** - and the difference is the whole
/// safety story of this file, because sending 46 where 44 was meant turns
/// a capability probe into a fan command.
const TYPE_QUERY: u32 = 44;
const TYPE_WRITE: u32 = 46;

const MODERN_LEN: usize = 128;
const LEGACY_LEN: usize = 4;

/// Bit 7 of a speed byte: spin this fan backwards. The same encoding the
/// driver reports back through `fan?_input` (see `parse_hwmon_rpm`).
const REVERSE: u8 = 0x80;

/// Speeds are hundreds of RPM. These two are OMEN Gaming Hub's own
/// defaults, used when the firmware reports no configured speed of its own.
const DEFAULT_CPU_SPEED: u8 = 37;
const DEFAULT_GPU_SPEED: u8 = 39;
/// The range a caller may ask for. Below 10 the blades barely move; above
/// 39 is past anything the vendor's own tool commands.
pub const MIN_SPEED: u8 = 10;
pub const MAX_SPEED: u8 = 39;

pub const DEFAULT_DURATION_SECS: u64 = 30;
pub const MIN_DURATION_SECS: u64 = 5;
/// A ceiling, not a suggestion: see the module docs on what a running
/// cycle costs.
pub const MAX_DURATION_SECS: u64 = 60;

/// Above this the machine is using the cooling that a cycle would remove.
pub const MAX_START_TEMP_C: i64 = 70;

/// How long to wait for the blades to stop before reversing them, and how
/// often to look. Reversing a fan that is still spinning forwards is the
/// mechanical step this exists to avoid.
const BRAKE_TIMEOUT: Duration = Duration::from_secs(7);
const BRAKE_POLL: Duration = Duration::from_millis(300);
/// What counts as stopped. Not zero: a tachometer on a coasting fan
/// reports single-digit hundreds long after the blades are done.
const STOPPED_RPM: i64 = 300;

/// The ramp down out of reverse, and the pause after releasing the
/// override. `EMERGENCY_STEP` is the same ramp, hurried.
const DECEL_STEP: u8 = 5;
const DECEL_PAUSE: Duration = Duration::from_millis(150);
const EMERGENCY_PAUSE: Duration = Duration::from_millis(120);
/// After the release the firmware gets a fixed moment to act on it, and
/// then the tachometers are watched until neither fan reports reverse.
/// The caller writes a forward speed as soon as a stop returns, so
/// returning while the blades still turn backwards is the same mechanical
/// step [`BRAKE_TIMEOUT`] exists to avoid, from the other side.
const RELEASE_SETTLE: Duration = Duration::from_millis(1500);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const SETTLE_POLL: Duration = Duration::from_millis(250);

/// Every wait in a cycle, in one place - so the sequence can be run
/// against a stand-in firmware in a test without taking the seconds the
/// real blades need.
#[derive(Debug, Clone, Copy)]
struct Pace {
    brake_timeout: Duration,
    brake_poll: Duration,
    decel_pause: Duration,
    release_settle: Duration,
    settle_timeout: Duration,
    settle_poll: Duration,
}

impl Pace {
    const NORMAL: Self = Self {
        brake_timeout: BRAKE_TIMEOUT,
        brake_poll: BRAKE_POLL,
        decel_pause: DECEL_PAUSE,
        release_settle: RELEASE_SETTLE,
        settle_timeout: SETTLE_TIMEOUT,
        settle_poll: SETTLE_POLL,
    };

    /// The same sequence with the ramp hurried.
    fn hurried(self) -> Self {
        Self {
            decel_pause: self.decel_pause.min(EMERGENCY_PAUSE),
            ..self
        }
    }
}

/// One fan's tachometer, as the driver reports it: a speed and the
/// direction bit (see `parse_hwmon_rpm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tach {
    pub rpm: i64,
    pub reversed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CleanerError {
    #[error(transparent)]
    Acpi(#[from] acpi::AcpiError),
    /// The call went through and the firmware refused, or answered
    /// something that is not one of these replies.
    #[error("the firmware refused the fan-cleaner call (it answered: {0})")]
    Refused(String),
    /// Asked, and this machine has neither generation of the feature.
    #[error("this machine's firmware has no fan cleaner")]
    NotCapable,
    /// A cycle is already running, or one is being set up.
    #[error("a fan-cleaning cycle is already in progress")]
    Busy,
    /// Too hot to remove the cooling.
    #[error("{0} °C is too hot to reverse the fans (the limit is {MAX_START_TEMP_C} °C)")]
    TooHot(i64),
    /// No temperature could be read, so there is no telling whether it is
    /// safe to remove the cooling. `force` is the way past this one.
    #[error("no temperature sensor could be read, so it is not safe to reverse the fans")]
    NoTemperature,
    /// The blades were still turning forwards when the braking step ran
    /// out of time. Reversing them anyway is what braking is there to
    /// prevent, so the cycle is called off instead.
    #[error("the fans did not stop in time ({fan1} and {fan2} RPM), so they were not reversed")]
    BrakeFailed { fan1: i64, fan2: i64 },
}

impl CleanerError {
    /// The sentence a client should show, translatable. The firmware's own
    /// answer bytes are passed through as a param, not translated.
    pub fn to_msg(&self) -> Msg {
        match self {
            Self::Acpi(e) => e.to_msg(),
            Self::Refused(answer) => msg!(
                "fan.cleaner.err.refused",
                { "answer" => answer.clone() },
                "the firmware refused the fan-cleaner call (it answered: {answer})"
            ),
            Self::NotCapable => msg!(
                "fan.cleaner.err.notCapable",
                "this machine's firmware has no fan cleaner"
            ),
            Self::Busy => msg!(
                "fan.cleaner.err.busy",
                "a fan-cleaning cycle is already in progress"
            ),
            Self::TooHot(temp) => msg!(
                "fan.cleaner.err.tooHot",
                { "temp" => temp, "limit" => MAX_START_TEMP_C },
                "{temp} °C is too hot to reverse the fans (the limit is {limit} °C)"
            ),
            Self::NoTemperature => msg!(
                "fan.cleaner.err.noTemperature",
                "no temperature sensor could be read, so it is not safe to reverse the fans"
            ),
            Self::BrakeFailed { fan1, fan2 } => msg!(
                "fan.cleaner.err.brakeFailed",
                { "fan1" => *fan1, "fan2" => *fan2 },
                "the fans did not stop in time ({fan1} and {fan2} RPM), so they were not reversed"
            ),
        }
    }
}

/// Which firmware generation drives the cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Generation {
    Modern,
    Legacy,
}

impl Generation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Modern => "modern",
            Self::Legacy => "legacy",
        }
    }
}

/// What the firmware said it can do, and at what speeds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub cpu: bool,
    pub gpu: bool,
    pub fan3: bool,
    /// The speeds the firmware has configured, in hundreds of RPM. Zero
    /// means "it did not say", and the defaults above stand in.
    pub cpu_speed: u8,
    pub gpu_speed: u8,
    pub fan3_speed: u8,
}

impl Capabilities {
    fn any(&self) -> bool {
        self.cpu || self.gpu || self.fan3
    }
}

/// What was found when the firmware was asked - or why it was not asked.
///
/// The distinction that earns the enum is the same one the lightbar probe
/// makes: **failing to ask is not being told no.** An unprivileged daemon
/// cannot write `/proc/acpi/call` at all, and reporting that as "your
/// laptop has no fan cleaner" sends the user to buy different hardware
/// over a missing `sudo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    /// Whether a cycle can be started at all.
    pub supported: bool,
    /// Which generation would drive it. `None` when nothing was found.
    pub generation: Option<Generation>,
    pub capabilities: Capabilities,
    /// True once the firmware answered either query, whichever way.
    pub answered: bool,
    /// Set when the question could not be put: no `acpi_call`, or no root.
    /// A remedy, not a verdict.
    pub unreachable: Option<Msg>,
    pub acpi_call_loaded: bool,
    pub acpi_call_installed: bool,
    /// One sentence saying which of the above it was.
    pub detail: Msg,
}

impl Probe {
    /// The generation to drive a cycle with. Mirrors the original's
    /// optimistic fallback: with neither query answering, modern is still
    /// attempted, because a firmware that has neither simply ignores the
    /// buffer. It is *not* what [`Probe::supported`] reports, which stays
    /// honest about what was established.
    fn chosen(&self) -> Generation {
        self.generation.unwrap_or(Generation::Modern)
    }
}

/// Asks the firmware what it can do. **Read-only**: both calls are
/// `TYPE_QUERY`, which is the same class of question the lightbar probe
/// puts, and neither moves a fan.
///
/// Never loads the kernel module - probing is a question, and a question
/// should not change the machine.
pub fn probe() -> Probe {
    let loaded = acpi::is_loaded();
    let installed = loaded || acpi::is_module_installed();

    if !loaded {
        let detail = if installed {
            msg!(
                "fan.cleaner.probe.notLoaded",
                "acpi_call is installed but not loaded, so the firmware was not asked"
            )
        } else {
            msg!(
                "fan.cleaner.probe.noAcpiCall",
                { "hint" => acpi::INSTALL_HINT },
                "/proc/acpi/call is missing, so the firmware was not asked; {hint}"
            )
        };
        return Probe {
            supported: false,
            generation: None,
            capabilities: Capabilities::default(),
            answered: false,
            unreachable: Some(detail.clone()),
            acpi_call_loaded: false,
            acpi_call_installed: installed,
            detail,
        };
    }

    let modern = query_modern();
    let legacy = query_legacy();

    // One unreachable is enough to explain the whole probe: both queries
    // go through the same file, so if one could not be written neither
    // could.
    let unreachable = match (&modern, &legacy) {
        (Err(CleanerError::Acpi(e)), _) | (_, Err(CleanerError::Acpi(e))) => Some(e.to_msg()),
        _ => None,
    };

    let capabilities = modern.as_ref().ok().copied().unwrap_or_default();
    let legacy_ok = legacy.as_ref().copied().unwrap_or(false);
    let answered = unreachable.is_none() && (modern.is_ok() || legacy.is_ok());

    let generation = if capabilities.any() {
        Some(Generation::Modern)
    } else if legacy_ok {
        Some(Generation::Legacy)
    } else {
        None
    };

    let detail = match (&unreachable, generation) {
        (Some(why), _) => why.clone(),
        (None, Some(Generation::Modern)) => msg!(
            "fan.cleaner.probe.modern",
            { "fans" => describe_fans(&capabilities) },
            "the firmware answered: reverse spin is available on {fans}"
        ),
        (None, Some(Generation::Legacy)) => msg!(
            "fan.cleaner.probe.legacy",
            "the firmware answered: the older single-speed fan cleaner is available"
        ),
        (None, None) => msg!(
            "fan.cleaner.probe.refused",
            "the firmware was asked and has no fan cleaner on this machine"
        ),
    };

    Probe {
        supported: generation.is_some(),
        generation,
        capabilities,
        answered,
        unreachable,
        acpi_call_loaded: true,
        acpi_call_installed: true,
        detail,
    }
}

/// Which fans the modern capability bitmask named, for a sentence a person
/// reads. Not translated - it is a param of one that is.
fn describe_fans(caps: &Capabilities) -> String {
    let mut named = Vec::new();
    if caps.cpu {
        named.push("the CPU fan");
    }
    if caps.gpu {
        named.push("the GPU fan");
    }
    if caps.fan3 {
        named.push("a third fan");
    }
    match named.len() {
        0 => "no fan".to_string(),
        1 => named[0].to_string(),
        _ => format!(
            "{} and {}",
            named[..named.len() - 1].join(", "),
            named[named.len() - 1]
        ),
    }
}

// --- the wire ----------------------------------------------------------

/// One call, and the data behind a `PASS`.
///
/// A `FAIL` signature or a non-zero return code is a refusal: the call
/// reached the firmware and the firmware said no.
fn call(
    id: u8,
    command: u32,
    command_type: u32,
    size: usize,
    payload: &[u8],
) -> Result<Vec<u8>, CleanerError> {
    send(id, &acpi::wmi_request(command, command_type, size, payload))
}

/// The same, given a request built elsewhere - which is how the capability
/// queries the parity test checks are the ones actually sent, rather than
/// a second copy of the same numbers.
fn send(id: u8, request: &str) -> Result<Vec<u8>, CleanerError> {
    let reply = acpi::call(METHOD, &format!("0 {id} {request}"))?;

    let bytes = acpi::parse_bytes(&reply).ok_or_else(|| CleanerError::Refused(reply.clone()))?;
    // Signature and return code are eight bytes; anything shorter is not
    // one of these replies at all.
    if bytes.len() < 8 {
        return Err(CleanerError::Refused(reply));
    }
    let code = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);

    // The return code is what both generations agree on; the signature is
    // only ever *disqualifying*. The original checks `PASS` on the modern
    // query and the code alone on the legacy one, and a firmware that
    // answers zero under some third signature is answering yes.
    if &bytes[0..4] == b"FAIL" || code != 0 {
        return Err(CleanerError::Refused(reply));
    }
    Ok(bytes[8..].to_vec())
}

/// The modern capability query. Byte 8 of the data is the bitmask; bytes
/// 0-2 are the configured speeds, masked because bit 7 is the direction.
fn query_modern() -> Result<Capabilities, CleanerError> {
    let data = send(ID_MODERN, &modern_query_request())?;
    Ok(decode_capabilities(&data))
}

pub(crate) fn decode_capabilities(data: &[u8]) -> Capabilities {
    let Some(&mask) = data.get(8) else {
        return Capabilities::default();
    };
    Capabilities {
        cpu: mask & 1 != 0,
        gpu: mask & 2 != 0,
        fan3: mask & 4 != 0,
        cpu_speed: data.first().copied().unwrap_or(0) & !REVERSE,
        gpu_speed: data.get(1).copied().unwrap_or(0) & !REVERSE,
        fan3_speed: data.get(2).copied().unwrap_or(0) & !REVERSE,
    }
}

/// The legacy query. Bit 5 of the first byte is the feature.
fn query_legacy() -> Result<bool, CleanerError> {
    let data = send(ID_LEGACY, &legacy_query_request())?;
    Ok(data.first().is_some_and(|b| b & 0x20 != 0))
}

/// The 128-byte payload that commands three reverse speeds. `fan3` is left
/// at zero when the firmware did not claim a third fan: the byte means
/// something on a machine that has none.
pub(crate) fn reverse_payload(cpu: u8, gpu: u8, fan3: Option<u8>) -> [u8; MODERN_LEN] {
    let mut payload = [0u8; MODERN_LEN];
    payload[0] = cpu | REVERSE;
    payload[1] = gpu | REVERSE;
    payload[2] = fan3.map(|s| s | REVERSE).unwrap_or(0);
    payload
}

/// The two capability queries, as the exact hex `acpi_call` is given.
///
/// Public because `tools/pyren-check.sh` has to send the identical bytes -
/// a shell script cannot derive them, and `daemon/check/tests/parity.rs`
/// compares these against what is written there. The payload is all
/// zeroes, so the whole request is the header plus padding.
pub fn modern_query_request() -> String {
    acpi::wmi_request(CMD_MODERN, TYPE_QUERY, MODERN_LEN, &[0u8; MODERN_LEN])
}

pub fn legacy_query_request() -> String {
    acpi::wmi_request(CMD_LEGACY_READ, TYPE_QUERY, LEGACY_LEN, &[0u8; LEGACY_LEN])
}

/// The header of a modern *write*, for the one assertion worth making
/// about a read-only tool: that it does not contain this.
pub fn modern_write_header() -> String {
    acpi::wmi_request(CMD_MODERN, TYPE_WRITE, MODERN_LEN, &[0u8; MODERN_LEN])[..33].to_string()
}

fn write_modern(payload: &[u8; MODERN_LEN]) -> Result<(), CleanerError> {
    call(ID_MODERN, CMD_MODERN, TYPE_WRITE, MODERN_LEN, payload).map(|_| ())
}

/// The speed a query reply says the firmware is commanding, when it is a
/// reverse one. Used by the stop sequence to know where to ramp down from.
pub(crate) fn reverse_speed_in(data: &[u8]) -> Option<u8> {
    let first = data.first().copied()?;
    (first & REVERSE != 0).then_some(first & !REVERSE)
}

/// Flips the legacy toggle. `on` sets bit 7 (reverse) alongside bit 1;
/// `off` clears bit 7 and leaves bit 1 - the pattern the original writes,
/// byte for byte.
fn toggle_legacy(on: bool) -> Result<(), CleanerError> {
    let data = call(
        ID_LEGACY,
        CMD_LEGACY_READ,
        TYPE_QUERY,
        LEGACY_LEN,
        &[0u8; LEGACY_LEN],
    )?;
    let mut buffer = [0u8; LEGACY_LEN];
    for (slot, byte) in buffer.iter_mut().zip(data.iter()) {
        *slot = *byte;
    }
    buffer[3] = if on {
        buffer[3] | 0x82
    } else {
        (buffer[3] | 0x02) & !REVERSE
    };
    call(ID_LEGACY, CMD_LEGACY_WRITE, TYPE_QUERY, LEGACY_LEN, &buffer).map(|_| ())
}

// --- a cycle -----------------------------------------------------------

/// A cycle in flight. Held by the module so a status read can say how long
/// is left and a second `start` can be refused.
#[derive(Debug, Clone)]
pub struct Cycle {
    pub generation: Generation,
    /// Ticks from when reverse spin actually *began* - braking does not
    /// count against the cycle, which is what the original does too.
    pub started: Instant,
    pub duration: Duration,
    /// Distinguishes one run from the next, so a watchdog armed for a
    /// finished cycle cannot stop the one after it.
    pub id: u64,
    /// The commanded speeds, for the status read.
    pub cpu_speed: u8,
    pub gpu_speed: u8,
}

impl Cycle {
    pub fn remaining(&self) -> Duration {
        self.duration.saturating_sub(self.started.elapsed())
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

/// What a caller has to give [`start`], because this module cannot read it
/// off the machine itself: it does not own the sysfs paths.
pub struct Request {
    /// `None` uses the firmware's own configured speeds, falling back to
    /// OGH's defaults. `Some` is clamped to [`MIN_SPEED`]..=[`MAX_SPEED`]
    /// and used for every fan.
    pub speed: Option<u8>,
    pub duration: Duration,
    /// The reference temperature, when there is a sensor. `None` skips the
    /// guard rather than refusing - a machine with no sensor is not a
    /// machine that is too hot.
    pub temp_c: Option<i64>,
}

/// Starts a cycle: brake, reverse, and hand back a [`Cycle`].
///
/// **Blocks for up to [`BRAKE_TIMEOUT`]** while the blades stop - there is
/// nothing to return until a physical process finishes, and returning
/// before it would be reporting a cycle that has not begun. It does *not*
/// block for the cycle's duration; the caller arms the watchdog.
///
/// `tach` is polled for the braking step. It is a closure rather than a
/// path because the sysfs discovery lives in the parent module, and a
/// second copy of it here would be a second thing to keep in step.
pub fn start(
    probe: &Probe,
    request: &Request,
    tach: impl Fn() -> [Tach; 2],
) -> Result<Cycle, CleanerError> {
    if let Some(temp) = request.temp_c {
        if temp > MAX_START_TEMP_C {
            return Err(CleanerError::TooHot(temp));
        }
    }
    if !probe.acpi_call_loaded {
        acpi::ensure_loaded()?;
    }

    let generation = probe.chosen();
    let duration = request.duration.clamp(
        Duration::from_secs(MIN_DURATION_SECS),
        Duration::from_secs(MAX_DURATION_SECS),
    );

    match generation {
        // No speed and no ramp: one toggle, and it is running.
        Generation::Legacy => {
            toggle_legacy(true)?;
            Ok(Cycle {
                generation,
                started: Instant::now(),
                duration,
                id: cycle_id(),
                cpu_speed: 0,
                gpu_speed: 0,
            })
        }
        Generation::Modern => start_modern(probe, request, duration, tach, Pace::NORMAL),
    }
}

fn start_modern(
    probe: &Probe,
    request: &Request,
    duration: Duration,
    tach: impl Fn() -> [Tach; 2],
    pace: Pace,
) -> Result<Cycle, CleanerError> {
    let caps = probe.capabilities;
    let (cpu_speed, gpu_speed, fan3_speed) = target_speeds(caps, request.speed);
    let fan3 = caps.fan3.then_some(fan3_speed);

    // Step 1: reverse direction, magnitude zero. The blades have to stop
    // before they can turn the other way, and this is what stops them.
    let braked = reverse_payload(0, 0, caps.fan3.then_some(0));
    if let Err(e) = write_modern(&braked) {
        // Nothing is spinning backwards yet, but something was commanded;
        // release it rather than leaving a half-written override.
        let _ = ramp_down(pace.hurried(), &tach);
        return Err(e);
    }

    if !wait_until(pace.brake_timeout, pace.brake_poll, || stopped(tach())) {
        // Out of time with the blades still turning. Engaging now would
        // reverse a fan at speed, so the brake is released and the cycle
        // refused - a clean that did not happen costs nothing.
        let [fan1, fan2] = tach();
        let _ = ramp_down(pace.hurried(), &tach);
        return Err(CleanerError::BrakeFailed {
            fan1: fan1.rpm,
            fan2: fan2.rpm,
        });
    }
    std::thread::sleep(pace.brake_poll);

    // Step 2: engage. The clock starts *here* - braking is setup, and
    // charging it against a 30-second cycle would make short cycles
    // shorter still on a machine whose fans take longer to stop.
    let started = Instant::now();
    if let Err(e) = write_modern(&reverse_payload(cpu_speed, gpu_speed, fan3)) {
        let _ = ramp_down(pace.hurried(), &tach);
        return Err(e);
    }

    Ok(Cycle {
        generation: Generation::Modern,
        started,
        duration,
        id: cycle_id(),
        cpu_speed,
        gpu_speed,
    })
}

/// The speeds to command. The firmware's own configured values win; where
/// it reported none, OGH's defaults stand in. An explicit request replaces
/// all three, clamped - including the `>100` case, which is somebody
/// passing whole RPM to a field that counts hundreds.
pub(crate) fn target_speeds(caps: Capabilities, requested: Option<u8>) -> (u8, u8, u8) {
    if let Some(speed) = requested {
        let speed = speed.clamp(MIN_SPEED, MAX_SPEED);
        return (speed, speed, speed);
    }

    // 33 is the value the original singles out: firmware that reports it
    // is reporting a floor rather than a choice, and the vendor's own tool
    // uses 37.
    let cpu = match caps.cpu_speed {
        0 | 33 => DEFAULT_CPU_SPEED,
        other => other,
    };
    let gpu = match caps.gpu_speed {
        0 => DEFAULT_GPU_SPEED,
        other => other,
    };
    let fan3 = match caps.fan3_speed {
        0 => DEFAULT_GPU_SPEED,
        other => other,
    };
    (cpu, gpu, fan3)
}

/// Ends a cycle: ramp the reverse speed down, then release the override
/// and hand the fans back to the firmware.
///
/// Every step is best-effort past the first failure. A stop that gave up
/// halfway would leave the fans reversed, which is the one outcome this
/// whole file is arranged to prevent - so a failure is reported *after*
/// the remaining steps have been tried, not instead of them.
///
/// `tach` is the same closure [`start`] takes: it says where to ramp down
/// from when the firmware will not, and when the blades have come round.
pub fn stop(generation: Generation, tach: impl Fn() -> [Tach; 2]) -> Result<(), CleanerError> {
    match generation {
        Generation::Legacy => toggle_legacy(false),
        Generation::Modern => ramp_down(Pace::NORMAL, tach),
    }
}

/// The stop sequence, hurried, for a failure partway through starting and
/// for a cycle found still running at startup.
pub fn emergency_stop(
    generation: Generation,
    tach: impl Fn() -> [Tach; 2],
) -> Result<(), CleanerError> {
    match generation {
        Generation::Legacy => toggle_legacy(false),
        Generation::Modern => ramp_down(Pace::NORMAL.hurried(), tach),
    }
}

fn ramp_down(pace: Pace, tach: impl Fn() -> [Tach; 2]) -> Result<(), CleanerError> {
    let mut first_error = None;

    // One query answers both questions the ramp has: what speed is being
    // commanded, and whether there is a third fan to command. Unanswered,
    // the third byte stays zero - it means something else on a machine
    // with two fans (see `reverse_payload`).
    let answer = send(ID_MODERN, &modern_query_request()).ok();
    let commanded = answer.as_deref().and_then(reverse_speed_in);
    let fan3 = answer
        .as_deref()
        .is_some_and(|data| decode_capabilities(data).fan3);

    // No reverse speed anywhere means there is nothing to ramp: this is
    // also the stop for a start that failed while braking, and for fans
    // somebody only suspected. Ramping from an assumed speed there would
    // *command* reverse spin on fans that were turning forwards.
    if let Some(from) = ramp_start(commanded, tach()) {
        let mut speed = from;
        loop {
            // Still a *reverse* payload: the fans are slowed in the
            // direction they are turning and only then released.
            // Commanding forward motion here would be asking the blades to
            // reverse at speed.
            if let Err(e) = write_modern(&reverse_payload(speed, speed, fan3.then_some(speed))) {
                first_error.get_or_insert(e);
            }
            if speed == 0 {
                break;
            }
            speed = speed.saturating_sub(DECEL_STEP);
            std::thread::sleep(pace.decel_pause);
        }
    }

    // All zero: the override is released and the firmware has the fans
    // back, forwards.
    if let Err(e) = write_modern(&[0u8; MODERN_LEN]) {
        first_error.get_or_insert(e);
    }
    std::thread::sleep(pace.release_settle);
    // Best-effort: a tachometer that never clears the bit must not keep
    // the fans from being handed back, so running out of time here is not
    // an error.
    wait_until(pace.settle_timeout, pace.settle_poll, || {
        tach().iter().all(|fan| !fan.reversed)
    });

    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Where a ramp down starts, or `None` when nothing is turning backwards.
///
/// The firmware's own commanded speed wins. When it does not answer, the
/// tachometers stand in: a fan the driver reports as reversed is ramped
/// from the speed it reads, in the firmware's hundreds of RPM.
pub(crate) fn ramp_start(commanded: Option<u8>, tach: [Tach; 2]) -> Option<u8> {
    commanded.filter(|speed| *speed > 0).or_else(|| {
        tach.iter()
            .filter(|fan| fan.reversed)
            .map(|fan| (fan.rpm / 100).clamp(0, MAX_SPEED as i64) as u8)
            .max()
    })
}

/// Whether both fans have stopped, for the braking step.
fn stopped(tach: [Tach; 2]) -> bool {
    tach.iter().all(|fan| fan.rpm < STOPPED_RPM)
}

/// Polls `done` until it holds or `timeout` passes, and says which.
fn wait_until(timeout: Duration, poll: Duration, done: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if done() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// Monotonic and unique within the process, which is all a cycle id has to
/// be: it exists to tell one run from the next.
fn cycle_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability byte is the only thing standing between "this
    /// machine has a fan cleaner" and "it does not", and it is one byte
    /// deep in a 128-byte reply. Decoding it wrong is not visible on
    /// hardware either - the firmware just refuses later.
    #[test]
    fn the_capability_bitmask_and_the_speeds_beside_it() {
        let mut data = vec![0u8; 32];
        data[0] = 37;
        data[1] = 39;
        data[2] = 0;
        data[8] = 0b011; // CPU and GPU, no third fan

        let caps = decode_capabilities(&data);
        assert!(caps.cpu && caps.gpu && !caps.fan3);
        assert_eq!((caps.cpu_speed, caps.gpu_speed), (37, 39));

        // Bit 7 of a speed byte is the direction, not part of the number.
        // Reading it as one turns 37 into 165 - a speed nothing accepts.
        data[0] = 37 | REVERSE;
        assert_eq!(decode_capabilities(&data).cpu_speed, 37);

        // A reply too short to hold the mask claims nothing rather than
        // reading whatever byte happens to be there.
        assert_eq!(decode_capabilities(&[1, 2, 3]), Capabilities::default());
    }

    #[test]
    fn a_reverse_payload_sets_the_direction_bit_on_every_commanded_fan() {
        let payload = reverse_payload(37, 39, Some(20));
        assert_eq!(payload[0], 37 | 0x80);
        assert_eq!(payload[1], 39 | 0x80);
        assert_eq!(payload[2], 20 | 0x80);
        assert!(
            payload[3..].iter().all(|&b| b == 0),
            "the rest of the buffer is zero"
        );

        // A machine with no third fan gets a plain zero there, not 0x80:
        // the byte means something else on those boards.
        assert_eq!(reverse_payload(37, 39, None)[2], 0);
    }

    /// The braking step is `reverse_payload(0, 0, ..)`, and it must stay
    /// distinguishable from "no override": 0x80 is magnitude zero in
    /// reverse, 0x00 is the firmware taking the fans back.
    #[test]
    fn braking_is_a_reverse_command_not_a_release() {
        assert_eq!(reverse_payload(0, 0, Some(0))[0], 0x80);
        assert_ne!(reverse_payload(0, 0, Some(0))[0], 0);
    }

    #[test]
    fn the_firmwares_own_speeds_win_and_a_request_replaces_them() {
        let caps = Capabilities {
            cpu_speed: 30,
            gpu_speed: 32,
            fan3_speed: 28,
            ..Default::default()
        };
        assert_eq!(target_speeds(caps, None), (30, 32, 28));

        // Nothing configured, and the vendor's defaults stand in - 33 is
        // the firmware reporting a floor, not a choice.
        let unset = Capabilities {
            cpu_speed: 33,
            gpu_speed: 0,
            fan3_speed: 0,
            ..Default::default()
        };
        assert_eq!(
            target_speeds(unset, None),
            (DEFAULT_CPU_SPEED, DEFAULT_GPU_SPEED, DEFAULT_GPU_SPEED)
        );

        // A request applies to every fan, and is clamped at both ends: a
        // speed the firmware will not take is worse than a slow clean.
        assert_eq!(target_speeds(caps, Some(25)), (25, 25, 25));
        assert_eq!(
            target_speeds(caps, Some(200)),
            (MAX_SPEED, MAX_SPEED, MAX_SPEED)
        );
        assert_eq!(
            target_speeds(caps, Some(1)),
            (MIN_SPEED, MIN_SPEED, MIN_SPEED)
        );
    }

    /// A request built by hand carries the header the firmware checks. If
    /// the *type* is wrong this stops being a query and becomes a fan
    /// command, which no test on hardware could tell you safely.
    #[test]
    fn the_query_and_the_write_are_different_commands() {
        let query = acpi::wmi_request(CMD_MODERN, TYPE_QUERY, MODERN_LEN, &[0u8; MODERN_LEN]);
        let write = acpi::wmi_request(CMD_MODERN, TYPE_WRITE, MODERN_LEN, &[0u8; MODERN_LEN]);
        assert_ne!(
            query, write,
            "44 asks and 46 sets; they must not build the same buffer"
        );

        // "SECU", command 0x00020008, type 44, size 128 - little-endian.
        assert!(
            query.starts_with("b53454355080002002c00000080000000"),
            "got: {}",
            &query[..34]
        );
        assert!(
            write.starts_with("b53454355080002002e00000080000000"),
            "got: {}",
            &write[..34]
        );
        // Header plus payload, hex, plus the 'b'.
        assert_eq!(query.len(), 1 + (acpi::HEADER_LEN + MODERN_LEN) * 2);
    }

    #[test]
    fn a_cycle_reports_what_is_left_and_notices_when_nothing_is() {
        let cycle = Cycle {
            generation: Generation::Modern,
            started: Instant::now(),
            duration: Duration::from_secs(30),
            id: 1,
            cpu_speed: 37,
            gpu_speed: 39,
        };
        assert!(!cycle.expired());
        assert!(cycle.remaining() <= Duration::from_secs(30));

        let done = Cycle {
            duration: Duration::ZERO,
            ..cycle
        };
        assert!(done.expired(), "an elapsed cycle is expired, whoever asks");
        assert!(done.remaining().is_zero());
    }

    #[test]
    fn cycle_ids_are_never_reused_within_a_process() {
        let ids: Vec<u64> = (0..8).map(|_| cycle_id()).collect();
        let mut sorted = ids.clone();
        sorted.dedup();
        assert_eq!(
            ids.len(),
            sorted.len(),
            "a stale watchdog must not match a new cycle"
        );
    }

    /// The stop sequence runs for fans nobody has confirmed are reversed -
    /// a start that failed while braking, a daemon on its way out. With
    /// nothing reporting reverse it must not ramp at all: ramping from an
    /// assumed speed is a reverse command.
    #[test]
    fn a_ramp_down_starts_from_what_is_turning_backwards_or_not_at_all() {
        let forwards = Tach {
            rpm: 2400,
            reversed: false,
        };
        let backwards = Tach {
            rpm: 3700,
            reversed: true,
        };

        assert_eq!(ramp_start(None, [forwards, forwards]), None);
        assert_eq!(ramp_start(Some(0), [forwards, forwards]), None);

        // The firmware's own answer wins over the tachometers.
        assert_eq!(ramp_start(Some(30), [backwards, backwards]), Some(30));

        // A firmware that will not say, and a fan that is reversed anyway:
        // ramp from what it reads, the faster of the two.
        assert_eq!(ramp_start(None, [forwards, backwards]), Some(37));
        let runaway = Tach {
            rpm: 9900,
            reversed: true,
        };
        assert_eq!(ramp_start(None, [runaway, backwards]), Some(MAX_SPEED));
    }

    #[test]
    fn braking_is_done_only_when_both_fans_have_stopped() {
        let still = Tach {
            rpm: 200,
            reversed: true,
        };
        let coasting = Tach {
            rpm: STOPPED_RPM,
            reversed: false,
        };
        assert!(stopped([still, still]));
        assert!(!stopped([still, coasting]));
    }

    /// Running out of time has to be *reported*: it is the difference
    /// between reversing stopped blades and reversing spinning ones.
    #[test]
    fn a_wait_says_whether_it_ended_by_the_condition_or_the_clock() {
        let tick = Duration::from_millis(1);
        assert!(wait_until(Duration::from_millis(50), tick, || true));
        assert!(!wait_until(Duration::from_millis(5), tick, || false));

        // Checked before the first sleep and once more at the deadline.
        let calls = std::cell::Cell::new(0);
        assert!(wait_until(Duration::from_secs(5), tick, || {
            calls.set(calls.get() + 1);
            calls.get() == 3
        }));
        assert_eq!(calls.get(), 3);
        assert!(wait_until(Duration::ZERO, tick, || true));
    }

    #[test]
    fn a_reverse_speed_is_read_only_off_a_byte_with_the_direction_bit() {
        assert_eq!(reverse_speed_in(&[37 | REVERSE, 0]), Some(37));
        assert_eq!(reverse_speed_in(&[37, 0]), None, "forwards is not a ramp");
        assert_eq!(reverse_speed_in(&[]), None);
    }

    /// A stand-in for the firmware behind `acpi_call`: a FIFO that hands
    /// every request to a thread, which records it and answers `PASS` with
    /// a fixed query reply. Enough to run a whole start or stop sequence
    /// and read back what was commanded, in order.
    struct Firmware {
        dir: std::path::PathBuf,
        fifo: std::path::PathBuf,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        _env: crate::testenv::NoAcpiCall,
    }

    impl Firmware {
        /// `data` is what a query is answered with, after the `PASS` and
        /// the zero return code.
        fn answering(name: &str, data: &[u8]) -> Self {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};

            let dir =
                std::env::temp_dir().join(format!("pyren-firmware-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let fifo = dir.join("call");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo runs");
            assert!(made.success());

            let reply = [b"PASS".as_slice(), &[0u8; 4], data]
                .concat()
                .iter()
                .map(|byte| format!("0x{byte:02x}"))
                .collect::<Vec<_>>()
                .join(", ");
            let reply = format!("{{{reply}}}");

            let seen = Arc::new(Mutex::new(Vec::new()));
            let done = Arc::new(AtomicBool::new(false));
            {
                let (fifo, seen, done) = (fifo.clone(), Arc::clone(&seen), Arc::clone(&done));
                std::thread::spawn(move || {
                    while let Ok(request) = std::fs::read_to_string(&fifo) {
                        if done.load(Ordering::Relaxed) {
                            break;
                        }
                        seen.lock().unwrap().push(request);
                        if std::fs::write(&fifo, &reply).is_err() {
                            break;
                        }
                    }
                });
            }

            let env = crate::testenv::at(&fifo);
            Self {
                dir,
                fifo,
                seen,
                done,
                _env: env,
            }
        }

        /// The payload bytes of every *write* sent so far, in order - the
        /// queries are the firmware being asked, not told.
        fn writes(&self) -> Vec<Vec<u8>> {
            let write = modern_write_header();
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter_map(|request| {
                    let hex = request.rsplit(' ').next()?;
                    hex.starts_with(&write)
                        .then(|| acpi::parse_bytes(hex))
                        .flatten()
                        .map(|bytes| bytes[acpi::HEADER_LEN..].to_vec())
                })
                .collect()
        }
    }

    impl Drop for Firmware {
        fn drop(&mut self) {
            // Opening the write end and closing it is an empty request,
            // which is what lets the thread see the flag and leave.
            self.done.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = std::fs::OpenOptions::new().write(true).open(&self.fifo);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const QUICK: Pace = Pace {
        brake_timeout: Duration::from_millis(60),
        brake_poll: Duration::from_millis(5),
        decel_pause: Duration::from_millis(1),
        release_settle: Duration::from_millis(1),
        settle_timeout: Duration::from_millis(20),
        settle_poll: Duration::from_millis(5),
    };

    fn two_fan_probe() -> Probe {
        Probe {
            supported: true,
            generation: Some(Generation::Modern),
            capabilities: Capabilities {
                cpu: true,
                gpu: true,
                ..Default::default()
            },
            answered: true,
            unreachable: None,
            acpi_call_loaded: true,
            acpi_call_installed: true,
            detail: msg!("test", "test"),
        }
    }

    fn request() -> Request {
        Request {
            speed: None,
            duration: Duration::from_secs(30),
            temp_c: Some(40),
        }
    }

    fn both(rpm: i64, reversed: bool) -> [Tach; 2] {
        [Tach { rpm, reversed }; 2]
    }

    /// The whole start, as the firmware sees it: brake, then engage - and
    /// nothing else, on fans that stopped.
    #[test]
    fn a_start_brakes_and_then_engages() {
        // Two fans, nothing commanded.
        let mut data = [0u8; 16];
        data[8] = 0b011;
        let firmware = Firmware::answering("start", &data);

        let cycle = start_modern(
            &two_fan_probe(),
            &request(),
            Duration::from_secs(30),
            || both(0, false),
            QUICK,
        )
        .expect("stopped fans are reversed");
        assert_eq!((cycle.cpu_speed, cycle.gpu_speed), (37, 39));

        let writes = firmware.writes();
        assert_eq!(writes.len(), 2, "brake and engage: {writes:?}");
        assert_eq!(writes[0][..3], [REVERSE, REVERSE, 0]);
        assert_eq!(writes[1][..3], [37 | REVERSE, 39 | REVERSE, 0]);
    }

    /// Fans that keep turning are never engaged: the brake is released and
    /// the caller told. No write in the whole exchange carries a reverse
    /// speed, which is the property this exists for.
    #[test]
    fn fans_that_do_not_stop_are_released_and_never_reversed() {
        let mut data = [0u8; 16];
        data[8] = 0b011;
        let firmware = Firmware::answering("brake", &data);

        let refused = start_modern(
            &two_fan_probe(),
            &request(),
            Duration::from_secs(30),
            || both(2400, false),
            QUICK,
        );
        assert!(
            matches!(
                refused,
                Err(CleanerError::BrakeFailed {
                    fan1: 2400,
                    fan2: 2400
                })
            ),
            "got: {refused:?}"
        );

        let writes = firmware.writes();
        assert_eq!(writes.len(), 2, "brake and release: {writes:?}");
        assert_eq!(writes[0][..3], [REVERSE, REVERSE, 0]);
        assert!(
            writes[1].iter().all(|byte| *byte == 0),
            "the override is released"
        );
        assert!(
            writes
                .iter()
                .all(|w| w[..3].iter().all(|b| b & !REVERSE == 0)),
            "no reverse speed was ever commanded: {writes:?}"
        );
    }

    /// The ramp down follows the firmware's own speed, leaves the third
    /// byte alone on a machine with two fans, and ends on a release.
    #[test]
    fn a_stop_ramps_down_from_the_commanded_speed_and_releases() {
        let mut data = [0u8; 16];
        data[0] = 12 | REVERSE;
        data[1] = 12 | REVERSE;
        data[8] = 0b011;
        let firmware = Firmware::answering("stop", &data);

        ramp_down(QUICK, || both(0, false)).expect("every write was accepted");

        let speeds: Vec<[u8; 3]> = firmware
            .writes()
            .iter()
            .map(|w| [w[0], w[1], w[2]])
            .collect();
        assert_eq!(
            speeds,
            vec![
                [12 | REVERSE, 12 | REVERSE, 0],
                [7 | REVERSE, 7 | REVERSE, 0],
                [2 | REVERSE, 2 | REVERSE, 0],
                [REVERSE, REVERSE, 0],
                [0, 0, 0],
            ]
        );
    }

    /// A third fan the firmware claims is ramped with the other two.
    #[test]
    fn a_stop_ramps_the_third_fan_only_where_there_is_one() {
        let mut data = [0u8; 16];
        data[0] = 5 | REVERSE;
        data[8] = 0b111;
        let firmware = Firmware::answering("fan3", &data);

        ramp_down(QUICK, || both(0, false)).expect("every write was accepted");

        let writes = firmware.writes();
        assert_eq!(writes[0][..3], [5 | REVERSE; 3]);
        assert_eq!(writes[1][..3], [REVERSE; 3]);
    }

    /// With no `acpi_call`, the probe must say so and offer the remedy -
    /// never report the hardware as lacking the feature, which is a
    /// different sentence with a different consequence.
    #[test]
    fn a_machine_that_was_never_asked_is_not_reported_as_incapable() {
        let dir = std::env::temp_dir().join(format!("pyren-cleaner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _no_acpi = crate::testenv::without_acpi_call(&dir);

        let probe = probe();
        assert!(!probe.supported);
        assert!(
            !probe.answered,
            "nothing was asked, so nothing was answered"
        );
        assert!(
            probe.unreachable.is_some(),
            "not being able to ask comes with a remedy"
        );
        assert!(!probe.detail.contains("has no fan cleaner"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fans_a_machine_can_clean_are_named_rather_than_counted() {
        let both = Capabilities {
            cpu: true,
            gpu: true,
            ..Default::default()
        };
        assert_eq!(describe_fans(&both), "the CPU fan and the GPU fan");
        let one = Capabilities {
            cpu: true,
            ..Default::default()
        };
        assert_eq!(describe_fans(&one), "the CPU fan");
        let all = Capabilities {
            cpu: true,
            gpu: true,
            fan3: true,
            ..Default::default()
        };
        assert_eq!(
            describe_fans(&all),
            "the CPU fan, the GPU fan and a third fan"
        );
    }
}
