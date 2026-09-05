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

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::OnceLock;

use crate::plan::Range;

/// `nvmlReturn_t`. Only the ones whose meaning differs to a caller.
const NVML_SUCCESS: c_int = 0;
const NVML_NOT_SUPPORTED: c_int = 3;
const NVML_NO_PERMISSION: c_int = 4;

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
}
