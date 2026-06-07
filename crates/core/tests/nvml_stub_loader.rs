//! NVML loader-plumbing test against a stub `.so` (CLAUDE.md testing strategy, research
//! 07 §P1-11). The stub source lives in `tests/nvml_stub/stub.rs` and is compiled HERE,
//! at test time, with a bare `rustc --crate-type cdylib` (no deps, no build.rs) into
//! `CARGO_TARGET_TMPDIR` — so the GPU-less Linux CI leg builds and runs this with plain
//! `cargo test`.
//!
//! What this pins is the contract `nvidia.rs` is built on, without a GPU or driver:
//! - `Nvml::builder().lib_path(...)` loads a library that is not on the default loader
//!   path — the exact mechanism `NvidiaBackend::init` uses for `libnvidia-ml.so.1`;
//! - a partial library still initializes: per-symbol lookups are lazy, and a MISSING
//!   symbol surfaces per call as `FailedToLoadSymbol` (the documented pre-R510 floor for
//!   `memory_info`), which `opt()` maps to `None` — never an init failure;
//! - an exported symbol returning `NVML_ERROR_NOT_SUPPORTED` surfaces as
//!   `NvmlError::NotSupported` — the normal per-metric outcome, also `None` via `opt()`;
//! - dropping `Nvml` calls `nvmlShutdown` and must not panic.
//!
//! Linux-only: the stub is an ELF cdylib and the lib_path-first loading contract is the
//! Linux half of the per-OS decision (Windows relies on the System32 default search).

#![cfg(all(target_os = "linux", feature = "nvidia"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::Nvml;

/// Compile the stub cdylib once per test binary. `RUSTC` (set by cargo for the running
/// toolchain) keeps the spawn hermetic under rustup; plain `rustc` is the fallback.
fn stub_so() -> &'static Path {
    static STUB: OnceLock<PathBuf> = OnceLock::new();
    STUB.get_or_init(|| {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/nvml_stub/stub.rs");
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("libgpv_nvml_stub.so");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .arg("--edition=2021")
            .arg("--crate-type=cdylib")
            .arg("-o")
            .arg(&out)
            .arg(&src)
            .output()
            .expect("spawn rustc to build the NVML stub cdylib");
        assert!(
            output.status.success(),
            "rustc failed to build the NVML stub:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        out
    })
}

/// The backend's per-metric rule (`nvidia.rs::opt`): any error is absence, never failure.
fn opt<T, E>(r: Result<T, E>) -> Option<T> {
    r.ok()
}

#[test]
fn stub_init_via_lib_path_and_graceful_per_symbol_degradation() {
    // The lib_path mechanism NvidiaBackend::init uses for `libnvidia-ml.so.1`, pointed
    // at a library the default loader search would never find.
    let nvml = Nvml::builder()
        .lib_path(stub_so().as_os_str())
        .init()
        .expect("stub must initialize through the lib_path plumbing");

    // String plumbing through the NVML buffer ABI.
    assert_eq!(nvml.sys_driver_version().unwrap(), "999.99-gpv-stub");

    // Device enumeration: exactly the init-time sequence of NvidiaBackend.
    assert_eq!(nvml.device_count().unwrap(), 1);
    let dev = nvml.device_by_index(0).unwrap();
    assert_eq!(dev.name().unwrap(), "GPV-STUB");

    // Error-code mapping through the FFI: stub returns NVML_ERROR_INVALID_ARGUMENT (2)
    // for any index but 0.
    assert!(matches!(
        nvml.device_by_index(7),
        Err(NvmlError::InvalidArg)
    ));

    // Degradation mode 1 — exported symbol, NOT_SUPPORTED return: the normal per-metric
    // outcome. Must surface as NotSupported and map to None via opt(), never a failure.
    assert!(matches!(dev.fan_speed(0), Err(NvmlError::NotSupported)));
    assert_eq!(opt(dev.fan_speed(0)).map(|f| f as f32), None);

    // Degradation mode 2 — MISSING symbol: per-symbol lookups are lazy, so a partial
    // library fails per call with FailedToLoadSymbol, not at init.
    assert!(matches!(
        dev.temperature(TemperatureSensor::Gpu),
        Err(NvmlError::FailedToLoadSymbol(_))
    ));
    assert_eq!(opt(dev.temperature(TemperatureSensor::Gpu)), None);

    // memory_info binds nvmlDeviceGetMemoryInfo_v2 — the documented pre-R510 driver
    // floor renders as "unavailable" through this exact error, never an error path.
    assert!(matches!(
        dev.memory_info(),
        Err(NvmlError::FailedToLoadSymbol(_))
    ));
    assert!(opt(dev.memory_info()).map(|m| m.total).is_none());

    // Drop calls nvmlShutdown through the loaded library — must not panic. (`dev`
    // borrows `nvml`, so it falls out of scope first; `nvml`'s Drop is the assertion.)
    drop(nvml);
}

#[test]
fn missing_library_is_an_error_not_a_panic() {
    // The no-driver case: load failure is a normal, reportable outcome (the backend maps
    // it to BackendError::Unavailable), never a crash.
    let missing = std::ffi::OsStr::new("/nonexistent/libgpv-no-such-nvml.so.1");
    let r = Nvml::builder().lib_path(missing).init();
    assert!(r.is_err(), "a missing library must be Err, got Ok");
}
