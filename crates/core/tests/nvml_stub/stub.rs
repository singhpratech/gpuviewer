//! Stub `libnvidia-ml.so.1` for the loader-plumbing test (`tests/nvml_stub_loader.rs`).
//!
//! NOT a cargo target: this file is compiled at test time by the test itself
//! (`rustc --crate-type cdylib`, no dependencies) into `CARGO_TARGET_TMPDIR`, so plain
//! `cargo test` on the GPU-less Linux CI leg builds and exercises it with no extra
//! workflow step and no new deps.
//!
//! It deliberately exports only a SUBSET of NVML so the test can pin both degradation
//! modes the nvidia backend relies on (CLAUDE.md domain rules):
//! - an exported symbol returning `NVML_ERROR_NOT_SUPPORTED` (`nvmlDeviceGetFanSpeed_v2`)
//!   → the per-metric NOT_SUPPORTED-is-normal path;
//! - a MISSING symbol (`nvmlDeviceGetTemperature`, `nvmlDeviceGetMemoryInfo_v2` — the
//!   latter is the documented pre-R510 driver floor) → `FailedToLoadSymbol`, which the
//!   backend's `opt()` must map to `None`, never a failure.
//!
//! `nvmlShutdown` is load-bearing: nvml-wrapper's `Drop for Nvml` calls it through a
//! generated binding that panics if the symbol is absent.

use core::ffi::{c_char, c_uint, c_void};

const NVML_SUCCESS: c_uint = 0;
const NVML_ERROR_INVALID_ARGUMENT: c_uint = 2;
const NVML_ERROR_NOT_SUPPORTED: c_uint = 3;

/// Backing storage for the one fake device handle — any stable non-null pointer; the
/// stub never dereferences handles it hands out.
static DEVICE_HANDLE_ANCHOR: u8 = 0;

/// NUL-terminated copy of `s` into a caller buffer of `len` bytes (NVML string ABI).
///
/// # Safety
/// `buf` must point to at least `len` writable bytes.
unsafe fn write_c_string(s: &str, buf: *mut c_char, len: c_uint) -> c_uint {
    if buf.is_null() || len == 0 {
        return NVML_ERROR_INVALID_ARGUMENT;
    }
    let n = s.len().min(len as usize - 1);
    for (i, b) in s.as_bytes()[..n].iter().enumerate() {
        unsafe { *buf.add(i) = *b as c_char };
    }
    unsafe { *buf.add(n) = 0 };
    NVML_SUCCESS
}

#[no_mangle]
pub extern "C" fn nvmlInit_v2() -> c_uint {
    NVML_SUCCESS
}

#[no_mangle]
pub extern "C" fn nvmlShutdown() -> c_uint {
    NVML_SUCCESS
}

/// # Safety
/// `count` must be a valid pointer or null.
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetCount_v2(count: *mut c_uint) -> c_uint {
    if count.is_null() {
        return NVML_ERROR_INVALID_ARGUMENT;
    }
    unsafe { *count = 1 };
    NVML_SUCCESS
}

/// # Safety
/// `device` must be a valid pointer or null.
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetHandleByIndex_v2(
    index: c_uint,
    device: *mut *mut c_void,
) -> c_uint {
    if device.is_null() || index != 0 {
        return NVML_ERROR_INVALID_ARGUMENT;
    }
    unsafe { *device = &DEVICE_HANDLE_ANCHOR as *const u8 as *mut c_void };
    NVML_SUCCESS
}

/// # Safety
/// `buf` must point to at least `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn nvmlSystemGetDriverVersion(buf: *mut c_char, len: c_uint) -> c_uint {
    unsafe { write_c_string("999.99-gpv-stub", buf, len) }
}

/// # Safety
/// `buf` must point to at least `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetName(
    _device: *mut c_void,
    buf: *mut c_char,
    len: c_uint,
) -> c_uint {
    unsafe { write_c_string("GPV-STUB", buf, len) }
}

/// Exported but unsupported: the per-metric NOT_SUPPORTED-is-normal path.
#[no_mangle]
pub extern "C" fn nvmlDeviceGetFanSpeed_v2(
    _device: *mut c_void,
    _fan: c_uint,
    _speed: *mut c_uint,
) -> c_uint {
    NVML_ERROR_NOT_SUPPORTED
}

// Deliberately NOT exported (do not add without updating the loader test's
// FailedToLoadSymbol assertions): nvmlDeviceGetTemperature, nvmlDeviceGetMemoryInfo_v2,
// and everything else.
