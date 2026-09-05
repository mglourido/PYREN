//! The offsets, through the driver's own library instead of through X.
//!
//! `nvidia-settings` (see [`super::nvidia`]) needs an X screen driven by
//! the NVIDIA X driver, with `Coolbits` enabled on it. On a Wayland
//! desktop there is no such screen to enable anything on: the compositor
//! runs the display and Xwayland is rootless, so
//! `nvidia-settings -q screens` finds nothing and the offset attribute is
//! refused for **everyone**, root included. That is not a permission the
//! user forgot to grant, it is a mechanism that is not present.
//!
//! NVML is the same driver's C API, and it carries the same two knobs:
//!
//! | NVML | what it moves |
//! |---|---|
//! | `nvmlDeviceGetGpcClkVfOffset` / `Set` | the core clock offset |
//! | `nvmlDeviceGetMemClkVfOffset` / `Set` | the memory *transfer rate* offset |
//! | `nvmlDevice{Gpc,Mem}ClkMinMaxVfOffset` | the range the driver will accept |
//!
//! It needs no X, no session and no `Coolbits` - only root, which is what
//! this daemon already is. So it is tried first, and `nvidia-settings`
//! stays as the fallback for the case NVML cannot answer: an older driver
//! whose library predates these symbols.
//!
//! **Loaded at runtime rather than linked.** This daemon has to build and
//! start on machines with no NVIDIA driver at all - most of them - so the
//! library is opened with `dlopen` and every symbol is resolved
//! individually. A missing library is "this machine has no NVML", a
//! missing symbol is "this driver is too old for this knob", and neither
//! is an error worth showing anybody.

use std::ffi::{c_char, c_int, c_uint, c_ulonglong, c_void, CStr, CString};
use std::sync::OnceLock;

use crate::plan::Range;

/// `nvmlReturn_t`. Only the ones whose meaning differs to a caller.
const NVML_SUCCESS: c_int = 0;
const NVML_NOT_SUPPORTED: c_int = 3;
const NVML_NO_PERMISSION: c_int = 4;
/// `nvmlEventSetWait_v2` with nothing queued. The ordinary answer, not an
/// error: it is what "the card has not complained" looks like.
const NVML_ERROR_TIMEOUT: c_int = 10;

/// `nvmlEventTypeXidCriticalError`. The driver's own "this GPU just did
/// something it should not have" - the signal `nvidia-bug-report` reads.
///
/// Deliberately the only bit registered. ECC counters are unsupported on a
/// consumer card, and a *throttle* is usually the card protecting itself
/// rather than failing, so neither belongs on a path whose response is to
/// undo the user's change.
const NVML_EVENT_XID_CRITICAL: c_ulonglong = 0x8;

/// How long a poll waits for an event that is not there. The driver queues
/// events into the set as they happen, so a poll finds anything that
/// arrived since the last one without waiting for it; anything arriving
/// *during* the poll is caught by the next tick 500 ms later. Waiting
/// longer here would only push the watchdog's own deadline check late.
const POLL_TIMEOUT_MS: c_uint = 1;

/// What went wrong, in the terms the caller has to distinguish.
///
/// Deliberately narrower than the driver's own list: the difference that
/// matters upstream is "this cannot be done here" versus "this needs
/// root" versus "the driver said no", and everything else is detail for
/// the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvmlError {
    /// No library, or a driver without these symbols.
    Unavailable,
    /// The card or driver does not offer this knob.
    NotSupported,
    /// The call is right and the caller is not root.
    NeedsRoot,
    /// Anything else the driver reported, with its own words.
    Failed(String),
}

/// The NVML signatures this module uses, named so every `dlsym` is
/// transmuted to a type spelled out in one place rather than inferred at
/// the call. Getting one of these wrong is how an FFI binding corrupts a
/// stack, so they are worth being able to read against `nvml.h`.
type ErrorString = unsafe extern "C" fn(c_int) -> *const c_char;
type Init = unsafe extern "C" fn() -> c_int;
type DeviceByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int;
type GetOffset = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type SetOffset = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type GetRange = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
type EventSetCreate = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type EventSetFree = unsafe extern "C" fn(*mut c_void) -> c_int;
type RegisterEvents = unsafe extern "C" fn(*mut c_void, c_ulonglong, *mut c_void) -> c_int;
type EventSetWait = unsafe extern "C" fn(*mut c_void, *mut EventData, c_uint) -> c_int;
type SupportedEvents = unsafe extern "C" fn(*mut c_void, *mut c_ulonglong) -> c_int;

/// `nvmlEventData_t`, field for field as of the versions that carry
/// `nvmlEventSetWait_v2`. The layout matters more than the contents: the
/// driver writes the whole struct, so a short one would be a stack
/// overwrite. Only `event_type` is ever read.
#[repr(C)]
#[derive(Clone, Copy)]
struct EventData {
    device: *mut c_void,
    event_type: c_ulonglong,
    event_data: c_ulonglong,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

impl EventData {
    fn empty() -> Self {
        Self {
            device: std::ptr::null_mut(),
            event_type: 0,
            event_data: 0,
            gpu_instance_id: 0,
            compute_instance_id: 0,
        }
    }
}

/// The handles `dlsym` found. `None` for a symbol this driver does not
/// have, which is how an older driver is told apart from a broken one.
struct Symbols {
    _library: *mut c_void,
    error_string: ErrorString,
    device_by_index: DeviceByIndex,
    get_core: Option<GetOffset>,
    set_core: Option<SetOffset>,
    get_mem: Option<GetOffset>,
    set_mem: Option<SetOffset>,
    core_range: Option<GetRange>,
    mem_range: Option<GetRange>,
    event_set_create: Option<EventSetCreate>,
    event_set_free: Option<EventSetFree>,
    register_events: Option<RegisterEvents>,
    event_set_wait: Option<EventSetWait>,
    supported_events: Option<SupportedEvents>,
}

// The library is opened once and never closed, and NVML's own API is
// documented thread-safe. The raw pointer is what makes this needed.
unsafe impl Send for Symbols {}
unsafe impl Sync for Symbols {}

/// Opened once per process. `nvmlInit_v2` is reference-counted by the
/// driver, so initialising on every probe would work and would also mean
/// a `dlopen` per status call.
fn symbols() -> Option<&'static Symbols> {
    static SYMBOLS: OnceLock<Option<Symbols>> = OnceLock::new();
    SYMBOLS.get_or_init(load).as_ref()
}

fn load() -> Option<Symbols> {
    // The versioned name, which is what is actually installed; the bare
    // `.so` belongs to a `-devel` package that a user machine will not
    // have.
    let name = CString::new("libnvidia-ml.so.1").ok()?;
    // SAFETY: a NUL-terminated name, and the flags are the documented
    // pair. A failure is a null pointer, which is checked.
    let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if library.is_null() {
        return None;
    }

    // SAFETY: each symbol is resolved by its documented name and
    // transmuted to that function's documented signature, spelled out in
    // the aliases above. A name that is not there yields `None` rather
    // than a call into nothing.
    unsafe {
        let init = sym::<Init>(library, "nvmlInit_v2")?;
        if init() != NVML_SUCCESS {
            return None;
        }
        Some(Symbols {
            error_string: sym::<ErrorString>(library, "nvmlErrorString")?,
            device_by_index: sym::<DeviceByIndex>(library, "nvmlDeviceGetHandleByIndex_v2")?,
            get_core: sym::<GetOffset>(library, "nvmlDeviceGetGpcClkVfOffset"),
            set_core: sym::<SetOffset>(library, "nvmlDeviceSetGpcClkVfOffset"),
            get_mem: sym::<GetOffset>(library, "nvmlDeviceGetMemClkVfOffset"),
            set_mem: sym::<SetOffset>(library, "nvmlDeviceSetMemClkVfOffset"),
            core_range: sym::<GetRange>(library, "nvmlDeviceGetGpcClkMinMaxVfOffset"),
            mem_range: sym::<GetRange>(library, "nvmlDeviceGetMemClkMinMaxVfOffset"),
            event_set_create: sym::<EventSetCreate>(library, "nvmlEventSetCreate"),
            event_set_free: sym::<EventSetFree>(library, "nvmlEventSetFree"),
            register_events: sym::<RegisterEvents>(library, "nvmlDeviceRegisterEvents"),
            event_set_wait: sym::<EventSetWait>(library, "nvmlEventSetWait_v2"),
            supported_events: sym::<SupportedEvents>(library, "nvmlDeviceGetSupportedEventTypes"),
            _library: library,
        })
    }
}

/// One symbol, as the function type the caller names.
///
/// SAFETY: `library` must be a handle from `dlopen`, and `T` must be the
/// signature `name` actually has in NVML - which is why every `T` used
/// here is one of the aliases above rather than an inferred type.
unsafe fn sym<T: Copy>(library: *mut c_void, name: &str) -> Option<T> {
    debug_assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut c_void>(),
        "only a function pointer may be resolved this way"
    );
    let name = CString::new(name).ok()?;
    let pointer = libc::dlsym(library, name.as_ptr());
    (!pointer.is_null()).then(|| std::mem::transmute_copy(&pointer))
}

/// Whether this machine has an NVML that can move an offset at all.
///
/// Both directions are asked for: a driver with the getters and not the
/// setters would report a current offset it could never change, which is
/// a worse answer than admitting it cannot.
pub fn available() -> bool {
    symbols().is_some_and(|s| s.set_core.is_some() || s.set_mem.is_some())
}

fn device(symbols: &Symbols, index: u32) -> Result<*mut c_void, NvmlError> {
    let mut handle: *mut c_void = std::ptr::null_mut();
    // SAFETY: `handle` is a valid out-pointer for the duration of the call.
    let rc = unsafe { (symbols.device_by_index)(index as c_uint, &mut handle) };
    if rc != NVML_SUCCESS {
        return Err(translate(symbols, rc));
    }
    Ok(handle)
}

fn translate(symbols: &Symbols, rc: c_int) -> NvmlError {
    match rc {
        NVML_NOT_SUPPORTED => NvmlError::NotSupported,
        NVML_NO_PERMISSION => NvmlError::NeedsRoot,
        // SAFETY: the driver returns a pointer to one of its own static
        // strings, valid for the life of the process.
        other => NvmlError::Failed(unsafe {
            CStr::from_ptr((symbols.error_string)(other)).to_string_lossy().into_owned()
        }),
    }
}

/// One offset, and the range the driver says it will accept.
fn read(
    index: u32,
    get: fn(&Symbols) -> Option<GetOffset>,
    range: fn(&Symbols) -> Option<GetRange>,
) -> Result<(i32, Option<Range>), NvmlError> {
    let symbols = symbols().ok_or(NvmlError::Unavailable)?;
    let get = get(symbols).ok_or(NvmlError::Unavailable)?;
    let handle = device(symbols, index)?;

    let mut value: c_int = 0;
    // SAFETY: a handle the driver gave us, and a valid out-pointer.
    let rc = unsafe { get(handle, &mut value) };
    if rc != NVML_SUCCESS {
        return Err(translate(symbols, rc));
    }

    // The range is a bonus: a driver that will not state one is not a
    // driver that cannot move the offset, so its absence is `None`
    // rather than an error.
    let range = range(symbols).and_then(|range| {
        let (mut min, mut max): (c_int, c_int) = (0, 0);
        // SAFETY: as above, with two out-pointers.
        let rc = unsafe { range(handle, &mut min, &mut max) };
        (rc == NVML_SUCCESS).then(|| Range::new(min, max))
    });

    Ok((value, range))
}

fn write(
    index: u32,
    set: fn(&Symbols) -> Option<SetOffset>,
    mhz: i32,
) -> Result<(), NvmlError> {
    let symbols = symbols().ok_or(NvmlError::Unavailable)?;
    let set = set(symbols).ok_or(NvmlError::Unavailable)?;
    let handle = device(symbols, index)?;

    // SAFETY: a handle the driver gave us, and a plain integer.
    let rc = unsafe { set(handle, mhz as c_int) };
    if rc == NVML_SUCCESS {
        return Ok(());
    }
    Err(translate(symbols, rc))
}

pub fn core_offset(index: u32) -> Result<(i32, Option<Range>), NvmlError> {
    read(index, |s| s.get_core, |s| s.core_range)
}

pub fn mem_offset(index: u32) -> Result<(i32, Option<Range>), NvmlError> {
    read(index, |s| s.get_mem, |s| s.mem_range)
}

pub fn set_core_offset(index: u32, mhz: i32) -> Result<(), NvmlError> {
    write(index, |s| s.set_core, mhz)
}

pub fn set_mem_offset(index: u32, mhz: i32) -> Result<(), NvmlError> {
    write(index, |s| s.set_mem, mhz)
}

// --- the fault signal ---------------------------------------------------

/// Whether this driver has the whole event API this module needs.
///
/// All five or none: a set that can be created and never waited on is a
/// leak, and a wait with no way to register anything never fires.
fn event_symbols(symbols: &Symbols) -> Option<(EventSetCreate, EventSetFree, RegisterEvents, EventSetWait, SupportedEvents)> {
    Some((
        symbols.event_set_create?,
        symbols.event_set_free?,
        symbols.register_events?,
        symbols.event_set_wait?,
        symbols.supported_events?,
    ))
}

/// Whether this GPU will report a critical fault at all.
///
/// A card that does not advertise the bit is not broken and is not worth a
/// message: it simply has one fewer signal than the reference card, and
/// everything else keeps working exactly as before.
pub fn supports_fault_events(index: u32) -> bool {
    let Some(symbols) = symbols() else { return false };
    let Some((.., supported)) = event_symbols(symbols) else { return false };
    let Ok(handle) = device(symbols, index) else { return false };

    let mut mask: c_ulonglong = 0;
    // SAFETY: a handle the driver gave us, and a valid out-pointer.
    let rc = unsafe { supported(handle, &mut mask) };
    rc == NVML_SUCCESS && mask & NVML_EVENT_XID_CRITICAL != 0
}

/// A registration for one GPU's critical faults, open for as long as an
/// unconfirmed offset is on that card.
///
/// The point of the type is that [`spawn_watchdog`](crate::OverclockModule)
/// never touches a raw NVML handle: it creates one of these when it arms a
/// change, asks [`poll`](Self::poll) once a tick, and drops it when the
/// change is confirmed or undone - the same way `Symbols`/`sym` keep the
/// rest of the FFI edge inside this file.
pub struct EventWatch {
    set: *mut c_void,
    free: EventSetFree,
    wait: EventSetWait,
}

// The set is only ever used from the watchdog thread that owns it, and
// NVML's own API is documented thread-safe. The raw pointer is what makes
// this needed - the same reason `Symbols` needs it.
unsafe impl Send for EventWatch {}
unsafe impl Sync for EventWatch {}

impl EventWatch {
    /// Registers for critical faults on one GPU, or `None` for every
    /// reason that is not an error: no NVML, a driver without the event
    /// API, a card that does not advertise the bit, or a driver that
    /// simply refuses the registration.
    ///
    /// There is no `Err` on purpose. Nothing upstream would do anything
    /// different with one: this is an *extra* signal on top of the timer
    /// that has always been there, so its absence has to be the old
    /// behaviour rather than a failed apply.
    pub fn create(index: u32) -> Option<Self> {
        let symbols = symbols()?;
        let (create, free, register, wait, _) = event_symbols(symbols)?;
        if !supports_fault_events(index) {
            return None;
        }
        let handle = device(symbols, index).ok()?;

        let mut set: *mut c_void = std::ptr::null_mut();
        // SAFETY: a valid out-pointer, filled in only on success.
        let rc = unsafe { create(&mut set) };
        if rc != NVML_SUCCESS || set.is_null() {
            return None;
        }

        // SAFETY: a handle and a set the driver just gave us.
        let rc = unsafe { register(handle, NVML_EVENT_XID_CRITICAL, set) };
        if rc != NVML_SUCCESS {
            // SAFETY: freeing the set we just created and are abandoning.
            unsafe { free(set) };
            return None;
        }
        Some(Self { set, free, wait })
    }

    /// Whether the driver has reported a critical fault since the last
    /// ask.
    ///
    /// Only a real, positively identified XID is `true`. A timeout is the
    /// ordinary "nothing happened", and any *other* error is also `false`:
    /// a broken query must not undo a change the user made, because "we
    /// could not tell" is not "the card is failing".
    pub fn poll(&self) -> bool {
        let mut data = EventData::empty();
        // SAFETY: a set this type owns, and an out-parameter of exactly
        // the struct the driver writes.
        let rc = unsafe { (self.wait)(self.set, &mut data, POLL_TIMEOUT_MS) };
        if rc == NVML_ERROR_TIMEOUT {
            return false;
        }
        rc == NVML_SUCCESS && data.event_type & NVML_EVENT_XID_CRITICAL != 0
    }
}

impl Drop for EventWatch {
    fn drop(&mut self) {
        // SAFETY: a set this type created and has not freed; the field is
        // never null after `create` returned `Some`.
        unsafe { (self.free)(self.set) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `dlopen`: on a machine with no NVIDIA driver
    /// this has to answer "no" rather than fail to start. Both answers
    /// are correct depending on where the test runs, and neither may
    /// panic - which is what this actually asserts.
    #[test]
    fn asking_whether_nvml_exists_is_safe_on_any_machine() {
        let _ = available();
    }

    /// A card index no machine has. The call must come back as an error
    /// rather than reaching into a handle the driver never filled in.
    #[test]
    fn a_gpu_that_does_not_exist_is_an_error_not_a_crash() {
        if !available() {
            return;
        }
        assert!(core_offset(u32::MAX).is_err());
    }

    /// Reading is not writing. On a machine with NVML the offsets read
    /// without root; the setters are what need it, and that difference is
    /// the one `probe_writable` is built on.
    #[test]
    fn the_offsets_can_be_read_without_being_writable() {
        if !available() {
            return;
        }
        // GPU 0 exists wherever NVML loaded at all.
        if let Ok((offset, Some(range))) = core_offset(0) {
            assert!(range.min <= offset && offset <= range.max, "{offset} outside {range:?}");
            assert!(range.min <= 0 && range.max >= 0, "stock must be inside the range");
        }
    }

    /// Same contract as `available()`, for the second half of this file:
    /// asking about a GPU that may not exist, on a machine that may have
    /// no driver, must answer rather than fall over.
    #[test]
    fn asking_whether_a_gpu_reports_faults_is_safe_on_any_machine() {
        let _ = supports_fault_events(0);
        assert!(!supports_fault_events(u32::MAX), "a GPU that is not there reports nothing");
    }

    /// The gate the whole feature hangs on: no NVML, no old driver, no
    /// card without the bit ever produces a watch. And where one *is*
    /// produced, creating and dropping it must not leak or panic.
    #[test]
    fn a_fault_watch_exists_only_where_the_card_advertises_the_signal() {
        assert!(EventWatch::create(u32::MAX).is_none(), "no GPU, no watch");
        assert_eq!(
            EventWatch::create(0).is_some(),
            available() && supports_fault_events(0),
            "a watch must appear exactly when the card says it can report faults"
        );
    }

    /// The live latency check, where there is hardware for it: a poll on a
    /// healthy card answers "nothing happened", and answers *fast* - the
    /// watchdog calls this every 500 ms and must not be blocked by it.
    #[test]
    fn polling_a_healthy_card_is_prompt_and_reports_nothing() {
        let Some(watch) = EventWatch::create(0) else { return };
        let start = std::time::Instant::now();
        assert!(!watch.poll(), "a healthy GPU must not report a critical fault");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "a poll took {:?}, which would push the watchdog's own tick late",
            start.elapsed()
        );
    }
}
