//! The vendor backend abstraction — nvtop's `struct gpu_vendor` vtable, translated to Rust.
//!
//! Contract:
//! - `init()` failing is normal (driver/library absent) — the registry drops the backend
//!   silently and the rest of the tool keeps working.
//! - Per-metric absence is `None` in the sample, never an `Err`. `Err` from a refresh means
//!   "this refresh produced nothing usable" (device fell off the bus, etc.) and is survivable.

use crate::model::{DeviceId, DynamicSample, ProcessSample, StaticInfo};

#[derive(Debug)]
pub enum BackendError {
    /// Backend or device unavailable (missing library/driver/permission). Normal outcome.
    Unavailable(String),
    /// Unknown device id passed in.
    DeviceNotFound(DeviceId),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unavailable(why) => write!(f, "unavailable: {why}"),
            BackendError::DeviceNotFound(id) => write!(f, "device not found: {id}"),
        }
    }
}

impl std::error::Error for BackendError {}

pub trait GpuBackend: Send {
    fn name(&self) -> &'static str;

    /// Devices this backend can see. Called once after init.
    fn devices(&mut self) -> Vec<DeviceId>;

    /// Queried once per device.
    fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError>;

    /// Called every tick.
    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError>;

    /// Called every tick, separately from dynamic info (some sources differ).
    fn refresh_processes(&mut self, dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError>;
}

/// Explicit registry — no constructor/inventory magic. Each backend's failed init is
/// logged and skipped.
///
/// Registry order is load-bearing (design cross-platform.md §3.7/§9):
/// **nvidia → amd → intel → wddm → apple**. The collector dedupes devices across
/// backends by normalized PCI address, first backend wins — so NVML (the richer source)
/// must register before wddm to claim NVIDIA boards, and AMD/Intel adapters — plus
/// NVIDIA boards on driverless/broken-NVML machines — fall through to wddm. No new
/// dedupe mechanism here: synthetic non-PCI ids (`wddm:…`, `apple:…`, `mock:…`) never
/// dedupe by design — a double listing is visible and honest, a wrong merge is not.
///
/// `force_mock` returns ONLY the mock backend — its purpose is deterministic CI/demo
/// output, so real devices must not leak in. Otherwise the mock is the fallback when no
/// real backend initialized (so the TUI always has something to show, clearly labeled).
pub fn all_backends(force_mock: bool) -> Vec<Box<dyn GpuBackend>> {
    if force_mock {
        return vec![Box::new(crate::mock::MockBackend::new())];
    }

    let mut backends: Vec<Box<dyn GpuBackend>> = Vec::new();

    #[cfg(all(feature = "nvidia", any(target_os = "linux", target_os = "windows")))]
    match crate::nvidia::NvidiaBackend::init() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => eprintln!("gpuviewer: nvidia backend skipped: {e}"),
    }

    #[cfg(target_os = "linux")]
    match crate::amd::AmdBackend::init() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => eprintln!("gpuviewer: amd backend skipped: {e}"),
    }

    #[cfg(target_os = "linux")]
    match crate::intel::IntelBackend::init() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => eprintln!("gpuviewer: intel backend skipped: {e}"),
    }

    // wddm registers LAST among Windows backends (§3.7): the collector's first-wins PCI
    // dedupe lets NVML claim NVIDIA boards first; AMD/Intel adapters — and NVIDIA boards
    // on driverless/broken-NVML machines — fall through to wddm. Synthetic `wddm:` ids
    // never dedupe by design.
    #[cfg(all(feature = "wddm", target_os = "windows"))]
    match crate::wddm::WddmBackend::init() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => eprintln!("gpuviewer: wddm backend skipped: {e}"),
    }

    // Registry order per design §9: nvidia → amd → intel → (wddm) → apple.
    #[cfg(all(feature = "apple", target_os = "macos"))]
    match crate::apple::AppleBackend::init() {
        Ok(b) => backends.push(Box::new(b)),
        Err(e) => eprintln!("gpuviewer: apple backend skipped: {e}"),
    }

    if backends.is_empty() {
        backends.push(Box::new(crate::mock::MockBackend::new()));
    }
    backends
}
