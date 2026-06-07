//! NVIDIA backend — direct NVML calls via `nvml-wrapper` (runtime-loads the driver's own
//! `libnvidia-ml.so.1` / `nvml.dll`). No nvidia-smi anywhere: nvidia-smi is itself just a
//! CLI wrapper over this same library.
//!
//! Per the domain rules in CLAUDE.md:
//! - `NOT_SUPPORTED` (and any other per-metric error) maps to `None`, never a failure.
//! - The `.so.1` path is tried first on Linux: driver-only installs don't ship the
//!   unversioned `.so` symlink (that comes with the CUDA toolkit) — the exact pitfall
//!   bottom hit.
//! - Throttle mapping is edge-honest: GPU idle is deliberately NOT narrated as throttling
//!   (an idle GPU is not slow). Configuration limiters (applications-clocks / display-clock
//!   settings) map to the catch-all `other` so they are surfaced without being mislabeled as
//!   a thermal or power slowdown. See [`map_throttle`].
//! - WSL2 is detected once at init: per-process GPU info is N/A *at the driver level*
//!   there, so `StaticInfo::process_hint` explains the empty process table up front
//!   instead of crashing on it (nvtop #432 is the cautionary tale).
//!
//! Windows (v1.5, docs/design/cross-platform.md §2) — NVML is the device-truth half of the
//! dual-source split; the shared PDH snapshot (`wddm::pdh`, joined per device by the §2.5
//! LUID↔PCI match built at init) is the per-process half:
//! - `nvml.dll` loading: modern drivers (≥461.55, ~2020+) install it into
//!   `C:\Windows\System32`, which is on the default `LoadLibraryExW` search path — plain
//!   `Nvml::init()` is the whole story. We do NOT probe the legacy
//!   `C:\Program Files\NVIDIA Corporation\NVSMI\` directory; the documented driver floor
//!   is R510+ (early 2022). Load failures (no driver, pre-2020 NVSMI-only driver,
//!   `DRIVER_NOT_LOADED`) map to `BackendError::Unavailable` — normal, backend skipped.
//! - WDDM realities: per-metric `NOT_SUPPORTED` → `None` exactly as on Linux; per-process
//!   `usedGpuMemory` is *always* Unavailable under WDDM (the Windows kernel memory manager
//!   owns that accounting, NVML architecturally cannot see it) → `None`, never 0 — the
//!   per-pid `GPU Process Memory\Dedicated Usage` counters from the shared PDH snapshot
//!   fill that column (§2.4), and per-pid engine busy% fills `util_pct`;
//!   `process_utilization_stats` is deliberately not called on Windows (§2.4); throttle
//!   reasons ARE empirically readable under WDDM and keep the same mapping as Linux;
//!   device `mem_used_bytes` prefers PDH's adapter-level Dedicated Usage (VidMm truth)
//!   over NVML's virtualized driver view (§2.3).
//! - Driver model: WDDM is the GeForce default; TCC (WDM) is deprecated Quadro/Tesla-only
//!   non-display mode where NVML per-process accounting works — no special path beyond an
//!   accurate `process_hint`. A future model value (MCDM) surfaces as `UnexpectedVariant`
//!   and is handled, never a crash (§2.6).
//! - MIG never arises on Windows (NVIDIA ships MIG on Linux only) — the MIG
//!   NOT_SUPPORTED-on-device-utilization caveat is a Linux-only concern.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;

use nvml_wrapper::bitmasks::device::ThrottleReasons as NvmlThrottle;
#[cfg(target_os = "windows")]
use nvml_wrapper::enum_wrappers::device::DriverModel;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::enums::device::UsedGpuMemory;
#[cfg(any(target_os = "windows", test))]
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::Nvml;

use crate::backend::{BackendError, GpuBackend};
use crate::model::{
    now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo, ThrottleReasons,
    Vendor,
};

/// Map any per-metric NVML error to `None` — absence is a normal outcome.
fn opt<T, E>(r: Result<T, E>) -> Option<T> {
    r.ok()
}

/// WSL kernels self-identify via the release string (e.g.
/// `5.15.167.4-microsoft-standard-WSL2`; WSL1-era kernels used capital-M `Microsoft`).
#[cfg(any(target_os = "linux", test))]
fn is_wsl(osrelease: &str) -> bool {
    osrelease.to_ascii_lowercase().contains("microsoft")
}

/// `StaticInfo::process_hint` for environments where the process list is known-absent.
/// WSL2 passes the GPU through but exposes no per-process info at the driver level —
/// say so up front instead of rendering a silently-empty table. A failed read just means
/// "nothing to explain"; this must never fail init.
#[cfg(target_os = "linux")]
fn wsl_process_hint() -> Option<String> {
    #[cfg(target_os = "linux")]
    if std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|rel| is_wsl(&rel)) {
        return Some(
            "per-process GPU info is unavailable under WSL2 (driver-level limitation) — \
             device metrics are unaffected"
                .into(),
        );
    }
    None
}

/// Pure mirror of the per-OS library-loading decision in [`NvidiaBackend::init`] (kept in
/// sync by the unit tests, which run on every OS):
/// - Linux: try the versioned soname first — driver-only installs ship only
///   `libnvidia-ml.so.1` (the unversioned `.so` symlink comes with the CUDA toolkit).
/// - Windows: no explicit path — modern drivers (≥461.55) put `nvml.dll` in System32,
///   already on the default loader search path; an explicit relative path would just be
///   one doomed `LoadLibrary` before the same default lookup. The legacy NVSMI directory
///   is deliberately not probed (the documented floor is R510+, §2.2).
#[cfg(test)]
fn explicit_lib_path_for(target_os: &str) -> Option<&'static str> {
    match target_os {
        "linux" => Some("libnvidia-ml.so.1"),
        _ => None,
    }
}

/// Windows driver model reduced to the cases that change our messaging. Mirrors NVML's
/// `nvmlDriverModel_t`: WDDM (display GPUs — the GeForce default), WDM a.k.a. TCC
/// (non-display compute; Quadro/Tesla-only and deprecated), plus a future-proofing bucket
/// for values nvml-wrapper 0.12 cannot decode (MCDM = 2, Microsoft's compute-only driver
/// model, is missing from its enum and surfaces as `NvmlError::UnexpectedVariant`).
#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DriverModelClass {
    Wddm,
    Tcc,
    /// The driver reported a model this build does not recognize (e.g. future MCDM).
    /// Treated like TCC-class (non-display) for messaging, per §2.6 — never a crash.
    UnknownVariant,
}

/// Classify a *failed* `driver_model()` query. Pure so the policy unit-tests on any OS:
/// - `UnexpectedVariant` is NVML successfully reporting a driver model newer than this
///   build's enum (MCDM) — keep the device, flag the unknown model.
/// - Any other error → assume WDDM: it is the default for every display-attached GPU on
///   Windows, and the WDDM hint is the one that matters. Under a TCC board this default
///   is merely redundant next to a populated per-process column; a missing hint under
///   WDDM would leave an empty column unexplained — the worse honesty failure.
#[cfg(any(target_os = "windows", test))]
fn classify_driver_model_err(e: &NvmlError) -> DriverModelClass {
    match e {
        NvmlError::UnexpectedVariant(_) => DriverModelClass::UnknownVariant,
        _ => DriverModelClass::Wddm,
    }
}

/// Whether the §2.4 PDH fill of per-process columns is actually available for a device.
/// The hint and caveat builders take this so they only ever name a source that the code
/// in this build genuinely delivers — naming an undelivered source is a lie with a
/// citation attached.
#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// A Windows build WITHOUT the wddm feature only ever constructs NoPdh; the other
// variants still appear in the (exhaustive, honest) match arms — not dead design.
#[cfg_attr(all(target_os = "windows", not(feature = "wddm")), allow(dead_code))]
enum PdhAttribution {
    /// The shared PDH snapshot is live and this device's LUID↔PCI match (§2.5)
    /// succeeded: per-process VRAM/util rows are filled from Windows GPU performance
    /// counters every tick.
    Available,
    /// PDH has GPU counters but no enumerated adapter's PCI address matched this
    /// device — the honest terminal state of §2.5: device metrics keep flowing,
    /// per-process columns stay None, and the hint says exactly that.
    NoLuidMatch,
    /// No `GPU Engine` PDH object in this session (no WDDM 2.0 GPU/driver — GPU-less
    /// VMs, ancient drivers), or this build carries no PDH plumbing at all (built
    /// without the `wddm` feature): nothing exists to attribute from.
    NoPdh,
}

/// `StaticInfo::process_hint` for a Windows device, from its driver model (§2.7) and the
/// real availability of PDH attribution. Pure so it unit-tests on any OS.
#[cfg(any(target_os = "windows", test))]
fn windows_process_hint(model: DriverModelClass, pdh: PdhAttribution) -> Option<String> {
    match model {
        // Under WDDM the Windows kernel owns per-process VRAM accounting; NVML's
        // usedGpuMemory is architecturally always Unavailable. What we say depends on
        // whether the PDH fill is actually delivering those columns on this device.
        DriverModelClass::Wddm => Some(match pdh {
            PdhAttribution::Available => {
                "per-process VRAM/utilization come from Windows (WDDM) GPU performance \
                 counters, not the NVIDIA driver — NVML cannot see them under WDDM"
                    .into()
            }
            PdhAttribution::NoLuidMatch => {
                "could not attribute per-process GPU data (LUID\u{2194}PCI match failed) \
                 — per-process VRAM/utilization unavailable; device metrics unaffected"
                    .into()
            }
            PdhAttribution::NoPdh => "per-process GPU stats unavailable: no WDDM 2.0 GPU/driver \
                 (Windows exposes them via GPU performance counters)"
                .into(),
        }),
        // TCC (non-display): NVML's own per-process accounting works — nothing to explain.
        DriverModelClass::Tcc => None,
        DriverModelClass::UnknownVariant => Some(
            "driver reports an unknown compute driver model (newer than this build) — \
             per-process GPU data is shown as NVML reports it and may be incomplete"
                .into(),
        ),
    }
}

/// `StaticInfo::source_caveat` for a Windows device (§2.3/§5.4): names where the
/// memory-used number really comes from, per the device's actual PDH attribution state.
/// Pure so it unit-tests on any OS.
#[cfg(any(target_os = "windows", test))]
fn windows_source_caveat(pdh: PdhAttribution) -> Option<String> {
    Some(match pdh {
        PdhAttribution::Available => {
            "memory used is Windows' VidMm dedicated usage (PDH), falling back to the \
             NVIDIA driver's view of WDDM-virtualized memory when PDH has no sample"
                .into()
        }
        // No PDH source on this device: every used-memory number is the driver's view
        // of a virtualized space, which can diverge from the OS (VidMm) number.
        PdhAttribution::NoLuidMatch | PdhAttribution::NoPdh => {
            "memory used is the NVIDIA driver's view of WDDM-virtualized memory and can \
             diverge from the OS (VidMm) number"
                .into()
        }
    })
}

/// Reduce a `driver_model()` result to our class. `Ok` handles WDDM/TCC; decode failures
/// and query errors go through [`classify_driver_model_err`] (the pure, tested half).
#[cfg(target_os = "windows")]
fn device_driver_model(d: &nvml_wrapper::Device<'_>) -> DriverModelClass {
    match d.driver_model() {
        Ok(state) => match state.current {
            DriverModel::WDDM => DriverModelClass::Wddm,
            // NVML calls TCC "WDM" for historical reasons.
            DriverModel::WDM => DriverModelClass::Tcc,
        },
        Err(e) => classify_driver_model_err(&e),
    }
}

/// Pre-R510 drivers (before early 2022) lack the `_v3` process-list symbols nvml-wrapper
/// 0.12 binds; that surfaces as `FailedToLoadSymbol`, and ONLY that error warrants the
/// one-shot `_v2` retry (§2.2). Anything else (`NOT_SUPPORTED`, GPU lost, ...) already
/// means "no list this tick" and must not trigger a second call.
#[cfg(any(target_os = "windows", test))]
fn should_retry_v2(e: &NvmlError) -> bool {
    matches!(e, NvmlError::FailedToLoadSymbol(_))
}

/// Per-process VRAM honesty, shared by Linux and Windows and *load-bearing* on Windows:
/// under WDDM, NVML reports `Unavailable` for every process (the Windows kernel memory
/// manager owns that accounting — NVML architecturally cannot see it), and the only honest
/// mapping is `None` — never 0, which would render as "this process uses no VRAM". The
/// wddm backend's PDH `GPU Process Memory\Dedicated Usage` counters are the fallback
/// source for this field on Windows. `Unavailable` is also the legitimate WSL2 outcome.
/// (TCC-mode boards report real values through the same `Used` arm — no special path.)
fn used_gpu_memory_bytes(m: &UsedGpuMemory) -> Option<u64> {
    match m {
        UsedGpuMemory::Used(b) => Some(*b),
        UsedGpuMemory::Unavailable => None,
    }
}

pub struct NvidiaBackend {
    nvml: Nvml,
    /// (nvml index, stable id) established at init.
    devs: Vec<(u32, DeviceId)>,
    /// Per-device adapter LUID from the §2.5 LUID↔PCI match, parallel to `devs` —
    /// the key that filters the shared PDH snapshot for this device. `None` = the match
    /// failed: an **honest terminal state** (§2.5) — the device keeps its NVML metrics,
    /// per-process columns stay `None`, and `process_hint` says why. Never force a match.
    #[cfg(all(target_os = "windows", feature = "wddm"))]
    luids: Vec<Option<(i32, u32)>>,
    /// Per-device watermark for `process_utilization_stats` sampling. Linux-only: that
    /// API is deliberately not called on Windows (§2.4 — see `refresh_processes`).
    #[cfg(target_os = "linux")]
    last_util_ts: HashMap<u32, u64>,
    /// Set once at init: explanation for a known-incomplete process list (WSL2), if any.
    /// Linux-only: the Windows hint depends on the per-device driver model and is
    /// computed per device in `static_info` instead.
    #[cfg(target_os = "linux")]
    process_hint: Option<String>,
    /// Turns the kernel's cumulative per-PID CPU counter into a per-tick rate. Linux-only:
    /// the CPU%/container columns come from `/proc`, which Windows does not have — both stay
    /// `None` there. Shared with the other Linux backends via `crate::proc_meta`.
    #[cfg(target_os = "linux")]
    cpu: crate::proc_meta::CpuTracker,
}

impl NvidiaBackend {
    pub fn init() -> Result<Self, BackendError> {
        // Driver-only Linux installs ship only libnvidia-ml.so.1; fall back to the
        // default loader.
        #[cfg(target_os = "linux")]
        let nvml = Nvml::builder()
            .lib_path(OsStr::new("libnvidia-ml.so.1"))
            .init()
            .or_else(|_| Nvml::init())
            .map_err(|e| BackendError::Unavailable(format!("NVML unavailable: {e}")))?;

        // Windows: nvml.dll lives in System32 (drivers ≥461.55), on the default
        // LoadLibraryExW search path — the plain default load is the whole story (§2.1).
        // Failure (no NVIDIA driver, pre-2020 NVSMI-only layout, DRIVER_NOT_LOADED) is a
        // normal outcome: Unavailable, backend skipped, the wddm backend covers the GPU.
        #[cfg(target_os = "windows")]
        let nvml = Nvml::init()
            .map_err(|e| BackendError::Unavailable(format!("NVML unavailable: {e}")))?;

        let count = nvml
            .device_count()
            .map_err(|e| BackendError::Unavailable(format!("NVML device count: {e}")))?;

        let mut devs = Vec::new();
        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else {
                continue;
            };
            // The PCI-address id is what cross-backend dedupe keys on; on Windows it is
            // also the value the wddm backend's LUID→BDF map joins against (§2.5).
            let id = match dev.pci_info() {
                Ok(pci) => DeviceId(pci.bus_id.to_lowercase()),
                Err(_) => DeviceId(format!("nvml:{i}")),
            };
            devs.push((i, id));
        }
        if devs.is_empty() {
            return Err(BackendError::Unavailable(
                "NVML loaded but no devices".into(),
            ));
        }

        // §2.5: build the session's LUID↔PCI map once at init (LUIDs are session-scoped
        // but stable within one) by matching each NVML device's normalized PCI BDF
        // against the D3DKMT-derived BDF of every DXGI adapter. Both sides go through
        // the one shared `normalize_pci_id` rule — the same equality the collector's
        // first-wins dedupe uses. Also prime the shared PDH query so the first Engine
        // tick is the *second* collection and rate counters can already produce values.
        #[cfg(all(target_os = "windows", feature = "wddm"))]
        let luids: Vec<Option<(i32, u32)>> = {
            let _ = crate::wddm::pdh::shared().snapshot(now_ms());
            let adapters = crate::wddm::adapters::enumerate();
            devs.iter()
                .map(|(_, id)| {
                    let key = crate::model::normalize_pci_id(&id.0)?;
                    adapters
                        .iter()
                        .find(|a| {
                            a.pci_bdf
                                .as_deref()
                                .and_then(crate::model::normalize_pci_id)
                                .as_deref()
                                == Some(key.as_str())
                        })
                        .map(|a| (a.luid_high, a.luid_low))
                })
                .collect()
        };

        Ok(Self {
            nvml,
            devs,
            #[cfg(all(target_os = "windows", feature = "wddm"))]
            luids,
            #[cfg(target_os = "linux")]
            last_util_ts: HashMap::new(),
            #[cfg(target_os = "linux")]
            process_hint: wsl_process_hint(),
            #[cfg(target_os = "linux")]
            cpu: crate::proc_meta::CpuTracker::new(),
        })
    }

    fn index_of(&self, dev: &DeviceId) -> Result<u32, BackendError> {
        self.devs
            .iter()
            .find(|(_, id)| id == dev)
            .map(|(i, _)| *i)
            .ok_or_else(|| BackendError::DeviceNotFound(dev.clone()))
    }

    /// This device's matched adapter LUID, if the §2.5 LUID↔PCI match succeeded.
    #[cfg(all(target_os = "windows", feature = "wddm"))]
    fn luid_of(&self, dev: &DeviceId) -> Option<(i32, u32)> {
        self.devs
            .iter()
            .position(|(_, id)| id == dev)
            .and_then(|p| self.luids.get(p).copied().flatten())
    }

    fn process_name(&self, pid: u32) -> String {
        if let Ok(name) = self.nvml.sys_process_name(pid, 128) {
            // NVML returns the full path; keep the basename for readability (both
            // separators: Linux `/`, Windows `\`).
            if let Some(base) = name.rsplit(['/', '\\']).next() {
                if !base.is_empty() {
                    return base.to_string();
                }
            }
            return name;
        }
        // Fallback: /proc/<pid>/comm on Linux.
        #[cfg(target_os = "linux")]
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            let comm = comm.trim();
            if !comm.is_empty() {
                return comm.to_string();
            }
        }
        format!("pid {pid}")
    }
}

/// Map NVML's clocks-event/throttle reason bitmask to our category struct. Pure (takes the
/// raw bitflags, touches no device) so it unit-tests without a GPU.
///
/// The mapping is deliberately exhaustive — rivals (research 05 competitive analysis) skip
/// SW_POWER_CAP and SW_THERMAL_SLOWDOWN, which are the *dominant* causes on power-limited
/// consumer parts (a 4090 Laptop spends most of its life power-capped, not at the silicon's
/// hardware brake), so a monitor that only reports HW slowdown silently misses why the card
/// is slow:
/// - SW_POWER_CAP → `power_cap` (the software power-scaling algorithm reducing clocks).
/// - SW/HW_THERMAL_SLOWDOWN → `thermal` (over GPU/memory temp; either tier is "thermal").
/// - HW_SLOWDOWN / HW_POWER_BRAKE_SLOWDOWN → `hw_slowdown` (the 2x+ hardware brake — temp,
///   external power-brake assertion, or fast-trigger overcurrent).
/// - SYNC_BOOST → `sync_boost` (held down by another GPU in the sync-boost group).
/// - APPLICATIONS_CLOCKS_SETTING, DISPLAY_CLOCK_SETTING, and any future/unrecognized bit →
///   `other`. These are real clock limiters worth surfacing, but they are user/display
///   *configuration*, not a slowdown event — they land in the catch-all rather than masquerade
///   as thermal or power throttling.
/// - GPU_IDLE and NONE set nothing: an idle GPU is not throttling, and narrating idle as a
///   throttle event would be a confidently-wrong story. (See `ThrottleReasons::any`.)
fn map_throttle(bits: NvmlThrottle) -> ThrottleReasons {
    // Bits that map to a specific category, plus the two we intentionally treat as "nothing"
    // (GPU_IDLE, NONE). Anything outside this set is an unrecognized/future bit → `other`,
    // so tolerant decoding never silently drops a reason.
    let categorized = NvmlThrottle::SW_POWER_CAP
        | NvmlThrottle::SW_THERMAL_SLOWDOWN
        | NvmlThrottle::HW_THERMAL_SLOWDOWN
        | NvmlThrottle::HW_SLOWDOWN
        | NvmlThrottle::HW_POWER_BRAKE_SLOWDOWN
        | NvmlThrottle::SYNC_BOOST
        | NvmlThrottle::APPLICATIONS_CLOCKS_SETTING
        | NvmlThrottle::DISPLAY_CLOCK_SETTING
        | NvmlThrottle::GPU_IDLE
        | NvmlThrottle::NONE;

    // `other` covers the two configuration limiters AND any bit we do not know about.
    let other = bits.intersects(
        NvmlThrottle::APPLICATIONS_CLOCKS_SETTING | NvmlThrottle::DISPLAY_CLOCK_SETTING,
    ) || !(bits - categorized).is_empty();

    ThrottleReasons {
        thermal: bits
            .intersects(NvmlThrottle::SW_THERMAL_SLOWDOWN | NvmlThrottle::HW_THERMAL_SLOWDOWN),
        power_cap: bits.contains(NvmlThrottle::SW_POWER_CAP),
        hw_slowdown: bits
            .intersects(NvmlThrottle::HW_SLOWDOWN | NvmlThrottle::HW_POWER_BRAKE_SLOWDOWN),
        sync_boost: bits.contains(NvmlThrottle::SYNC_BOOST),
        other,
    }
}

impl GpuBackend for NvidiaBackend {
    fn name(&self) -> &'static str {
        "nvidia"
    }

    fn devices(&mut self) -> Vec<DeviceId> {
        self.devs.iter().map(|(_, id)| id.clone()).collect()
    }

    fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
        let i = self.index_of(dev)?;
        let d = self
            .nvml
            .device_by_index(i)
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        // Why the process list/columns may be incomplete differs per OS: Linux = WSL2
        // (detected once at init); Windows = the per-device driver model (§2.7 — WDDM is
        // where NVML cannot see per-process VRAM; mixed WDDM/TCC multi-GPU boxes exist,
        // hence per-device rather than per-backend) crossed with whether the PDH fill
        // (§2.4) actually attributes this device.
        #[cfg(target_os = "linux")]
        let process_hint = self.process_hint.clone();
        #[cfg(target_os = "linux")]
        let source_caveat = None;
        #[cfg(target_os = "windows")]
        let (process_hint, source_caveat) = {
            #[cfg(feature = "wddm")]
            let pdh = if !crate::wddm::pdh::shared().engine_object_present() {
                PdhAttribution::NoPdh
            } else if self.luid_of(dev).is_some() {
                PdhAttribution::Available
            } else {
                PdhAttribution::NoLuidMatch
            };
            #[cfg(not(feature = "wddm"))]
            let pdh = PdhAttribution::NoPdh;
            (
                windows_process_hint(device_driver_model(&d), pdh),
                windows_source_caveat(pdh),
            )
        };

        Ok(StaticInfo {
            id: dev.clone(),
            vendor: Vendor::Nvidia,
            name: opt(d.name()).unwrap_or_else(|| "NVIDIA GPU".into()),
            backend: "nvidia".into(),
            // memory_info binds nvmlDeviceGetMemoryInfo_v2 (R510+, early 2022). On a
            // pre-R510 driver it fails with FailedToLoadSymbol → None via opt(): the
            // documented driver floor (§2.2) renders as "unavailable", never an error.
            mem_total_bytes: opt(d.memory_info()).map(|m| m.total),
            power_limit_mw: opt(d.enforced_power_limit()),
            max_sm_clock_mhz: opt(d.max_clock_info(Clock::SM)),
            temp_slowdown_c: opt(d.temperature_threshold(TemperatureThreshold::Slowdown))
                .map(|t| t as f32),
            driver_version: opt(self.nvml.sys_driver_version()),
            process_hint,
            source_caveat,
        })
    }

    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        let i = self.index_of(dev)?;
        let d = self
            .nvml
            .device_by_index(i)
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        // Device-level duty-cycle utilization. MIG-enabled GPUs legitimately return
        // NOT_SUPPORTED here → None (Linux-only concern: MIG does not exist on Windows).
        let util = opt(d.utilization_rates());
        // Pre-R510: FailedToLoadSymbol → None, same floor as static_info.
        let nvml_mem_used = opt(d.memory_info()).map(|m| m.used);
        // §2.3: under WDDM, NVML's `used` is the driver's view of a *virtualized* space
        // (VidMm can page VRAM to system RAM) and can diverge from the OS's number. The
        // PDH adapter-level Dedicated Usage (the VidMm truth — KB4490156) is the primary
        // Windows source; the NVML view is the fallback, named as "driver view" by the
        // static source_caveat. Elsewhere the NVML number IS the device truth.
        #[cfg(all(target_os = "windows", feature = "wddm"))]
        let mem_used_bytes = self
            .luid_of(dev)
            .and_then(|(h, l)| {
                crate::wddm::pdh::shared()
                    .snapshot(now_ms())
                    .and_then(|s| crate::wddm::pdh::adapter_bytes(&s.adapter_dedicated, h, l))
            })
            .or(nvml_mem_used);
        #[cfg(not(all(target_os = "windows", feature = "wddm")))]
        let mem_used_bytes = nvml_mem_used;
        // Throttle reasons are empirically readable under WDDM — same mapping as Linux.
        // A failed query (NOT_SUPPORTED, GPU lost, ...) is an unobservable tick → `None`,
        // NEVER the all-false default: that would assert "not throttling" as a fact this
        // tick did not observe (§5.4 — the fabricated negative the model change exists
        // to make unrepresentable).
        let throttle = opt(d.current_throttle_reasons()).map(map_throttle);

        Ok(DynamicSample {
            ts_ms: now_ms(),
            util_pct: util.as_ref().map(|u| u.gpu as f32),
            // NVML utilization is a whole-device duty-cycle, not an engine headline.
            util_engine: None,
            mem_used_bytes,
            power_mw: opt(d.power_usage()),
            temp_c: opt(d.temperature(TemperatureSensor::Gpu)).map(|t| t as f32),
            fan_pct: opt(d.fan_speed(0)).map(|f| f as f32),
            sm_clock_mhz: opt(d.clock_info(Clock::SM)),
            mem_clock_mhz: opt(d.clock_info(Clock::Memory)),
            encoder_pct: opt(d.encoder_utilization()).map(|e| e.utilization as f32),
            decoder_pct: opt(d.decoder_utilization()).map(|e| e.utilization as f32),
            throttle,
        })
    }

    fn refresh_processes(&mut self, dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError> {
        let i = self.index_of(dev)?;
        let d = self
            .nvml
            .device_by_index(i)
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        // PIDs + VRAM from the compute and graphics lists; a PID in both is "C+G".
        #[cfg(target_os = "linux")]
        let (compute, graphics) = (
            d.running_compute_processes().unwrap_or_default(),
            d.running_graphics_processes().unwrap_or_default(),
        );
        // Windows: same lists, plus the §2.2 pre-R510 fallback. nvml-wrapper 0.12 binds
        // the _v3 symbols (R510+); a pre-2022 driver yields FailedToLoadSymbol, and only
        // then is the _v2 variant retried once (requires nvml-wrapper's "legacy-functions"
        // feature) before giving up to an empty list. PID enumeration is NVML's job even
        // under WDDM — it is the per-process *memory* that WDDM hides (see
        // `used_gpu_memory_bytes`).
        #[cfg(target_os = "windows")]
        let (compute, graphics) = (
            match d.running_compute_processes() {
                Ok(v) => v,
                Err(e) if should_retry_v2(&e) => {
                    d.running_compute_processes_v2().unwrap_or_default()
                }
                Err(_) => Vec::new(),
            },
            match d.running_graphics_processes() {
                Ok(v) => v,
                Err(e) if should_retry_v2(&e) => {
                    d.running_graphics_processes_v2().unwrap_or_default()
                }
                Err(_) => Vec::new(),
            },
        );

        let mut by_pid: HashMap<u32, ProcessSample> = HashMap::new();
        for (list, kind) in [
            (compute, ProcessKind::Compute),
            (graphics, ProcessKind::Graphics),
        ] {
            for p in list {
                // WDDM (always) / WSL2: VRAM legitimately unavailable → None, never 0;
                // show the process anyway. See `used_gpu_memory_bytes` for the full story.
                let mem = used_gpu_memory_bytes(&p.used_gpu_memory);
                by_pid
                    .entry(p.pid)
                    .and_modify(|e| {
                        e.kind = ProcessKind::Both;
                        if e.mem_bytes.is_none() {
                            e.mem_bytes = mem;
                        }
                    })
                    .or_insert_with(|| ProcessSample {
                        pid: p.pid,
                        name: self.process_name(p.pid),
                        kind,
                        mem_bytes: mem,
                        util_pct: None,
                        cpu_pct: None,
                        container: None,
                    });
            }
        }

        // Per-PID utilization samples since our last watermark. NOT_FOUND when nothing ran
        // is normal; semantics are weak under concurrency (documented NVML limitation), so
        // these populate a column, never headline numbers. Linux-only: per NVIDIA's own
        // forum guidance the samples are only meaningful when a single process owns the
        // GPU, and on Windows the PDH per-pid `GPU Engine` counters are strictly better —
        // the §2.4 join below fills `util_pct` from the shared PDH snapshot instead, so
        // this API is deliberately never called there.
        #[cfg(target_os = "linux")]
        {
            let since = self.last_util_ts.get(&i).copied().unwrap_or(0);
            if let Ok(samples) = d.process_utilization_stats(since) {
                let mut newest = since;
                for s in samples {
                    newest = newest.max(s.timestamp);
                    if let Some(p) = by_pid.get_mut(&s.pid) {
                        p.util_pct = Some(s.sm_util as f32);
                    }
                }
                self.last_util_ts.insert(i, newest);
            }
        }

        // CPU% and container identity come from /proc on Linux (Windows has neither — both
        // stay None there). The CpuTracker holds per-PID state, so prune it to the PIDs we
        // still see to keep it from growing across a long session. container_of is stateless.
        #[cfg(target_os = "linux")]
        {
            let live: Vec<u32> = by_pid.keys().copied().collect();
            for (pid, p) in by_pid.iter_mut() {
                p.cpu_pct = self.cpu.sample(*pid);
                p.container = crate::proc_meta::container_of(*pid);
            }
            self.cpu.prune(&live);
        }

        // §2.4: fill the WDDM-hidden columns from the shared PDH snapshot, joined by
        // (pid, this device's LUID). NVML stays the spine — it knows compute vs graphics —
        // and PDH supplies what WDDM hides from it: per-process dedicated VRAM
        // (`GPU Process Memory\Dedicated Usage`; Shared Usage is NEVER folded in) and the
        // per-pid max-across-engines busy% (the Task-Manager-comparable number, scheduler
        // duty-cycle). Pids only PDH sees (e.g. dwm.exe when the graphics list misses it)
        // are appended; their kind is Unknown unless the Compute/Cuda engtype heuristic
        // (§3.5) upgrades it. A failed LUID match (`luid_of` → None) leaves every column
        // honestly None — `process_hint` already explains why.
        #[cfg(all(target_os = "windows", feature = "wddm"))]
        if let Some((h, l)) = self.luid_of(dev) {
            if let Some(snap) = crate::wddm::pdh::shared().snapshot(now_ms()) {
                let util = crate::wddm::pdh::per_pid_util(&snap.engine_util, h, l);
                let mem = crate::wddm::pdh::per_pid_bytes(&snap.proc_dedicated, h, l);
                for (pid, p) in by_pid.iter_mut() {
                    if p.mem_bytes.is_none() {
                        p.mem_bytes = mem.get(pid).copied();
                    }
                    if p.util_pct.is_none() {
                        p.util_pct = util.get(pid).map(|u| u.pct as f32);
                    }
                }
                let mut pdh_only: Vec<u32> = util
                    .keys()
                    .chain(mem.keys())
                    .copied()
                    .filter(|pid| !by_pid.contains_key(pid))
                    .collect();
                pdh_only.sort_unstable();
                pdh_only.dedup();
                for pid in pdh_only {
                    by_pid.insert(
                        pid,
                        ProcessSample {
                            pid,
                            name: crate::wddm::os_process_name(pid),
                            kind: if util.get(&pid).is_some_and(|u| u.compute_hint) {
                                ProcessKind::Compute
                            } else {
                                ProcessKind::Unknown
                            },
                            mem_bytes: mem.get(&pid).copied(),
                            util_pct: util.get(&pid).map(|u| u.pct as f32),
                            cpu_pct: None,
                            container: None,
                        },
                    );
                }
            }
        }

        Ok(by_pid.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_driver_model_err, explicit_lib_path_for, is_wsl, map_throttle, should_retry_v2,
        used_gpu_memory_bytes, windows_process_hint, windows_source_caveat, DriverModelClass,
        NvmlError, NvmlThrottle, PdhAttribution, UsedGpuMemory,
    };

    #[test]
    fn is_wsl_matches_real_kernel_release_strings() {
        // Current WSL2 naming and the WSL1-era capital-M variant must both match.
        assert!(is_wsl("5.15.167.4-microsoft-standard-WSL2"));
        assert!(is_wsl("4.4.0-19041-Microsoft"));
        // Regular distro kernels must not.
        assert!(!is_wsl("6.17.0-35-generic"));
        assert!(!is_wsl(""));
    }

    #[test]
    fn throttle_sw_power_cap_maps_to_power_cap() {
        // The dominant cause on power-limited consumer parts (e.g. a 4090 Laptop) that rivals
        // skip — it must read as power_cap and nothing else.
        let r = map_throttle(NvmlThrottle::SW_POWER_CAP);
        assert!(r.power_cap);
        assert!(!r.thermal && !r.hw_slowdown && !r.sync_boost && !r.other);
    }

    #[test]
    fn throttle_both_thermal_tiers_map_to_thermal() {
        // SW thermal (over operating temp) and HW thermal (the 2x brake) both → thermal.
        assert!(map_throttle(NvmlThrottle::SW_THERMAL_SLOWDOWN).thermal);
        assert!(map_throttle(NvmlThrottle::HW_THERMAL_SLOWDOWN).thermal);
        let both =
            map_throttle(NvmlThrottle::SW_THERMAL_SLOWDOWN | NvmlThrottle::HW_THERMAL_SLOWDOWN);
        assert!(both.thermal && !both.power_cap && !both.hw_slowdown);
    }

    #[test]
    fn throttle_hw_slowdown_and_power_brake_map_to_hw_slowdown() {
        assert!(map_throttle(NvmlThrottle::HW_SLOWDOWN).hw_slowdown);
        assert!(map_throttle(NvmlThrottle::HW_POWER_BRAKE_SLOWDOWN).hw_slowdown);
        let both = map_throttle(NvmlThrottle::HW_SLOWDOWN | NvmlThrottle::HW_POWER_BRAKE_SLOWDOWN);
        assert!(both.hw_slowdown && !both.thermal && !both.power_cap);
    }

    #[test]
    fn throttle_sync_boost_maps_to_sync_boost() {
        let r = map_throttle(NvmlThrottle::SYNC_BOOST);
        assert!(r.sync_boost);
        assert!(!r.thermal && !r.power_cap && !r.hw_slowdown && !r.other);
    }

    #[test]
    fn throttle_clock_config_bits_map_to_other() {
        // Applications-clocks and display-clock settings are real limiters but are user/display
        // configuration, not a slowdown event — they belong in `other`, never thermal/power.
        let app = map_throttle(NvmlThrottle::APPLICATIONS_CLOCKS_SETTING);
        assert!(app.other);
        assert!(!app.thermal && !app.power_cap && !app.hw_slowdown && !app.sync_boost);
        let disp = map_throttle(NvmlThrottle::DISPLAY_CLOCK_SETTING);
        assert!(disp.other);
        assert!(!disp.thermal && !disp.power_cap && !disp.hw_slowdown && !disp.sync_boost);
    }

    #[test]
    fn throttle_unknown_future_bit_maps_to_other() {
        // A bit NVML adds in a future driver that this build does not recognize must not be
        // silently dropped — tolerant decoding lands it in `other`. Pick a high bit not used
        // by any current reason.
        let future = NvmlThrottle::from_bits_retain(1 << 40);
        let r = map_throttle(future);
        assert!(
            r.other,
            "unrecognized bit must surface as other, not vanish"
        );
        assert!(!r.thermal && !r.power_cap && !r.hw_slowdown && !r.sync_boost);
    }

    #[test]
    fn throttle_idle_and_none_set_nothing() {
        // Idle is not throttling, and NONE is the explicit "clocks unrestricted" sentinel.
        assert!(!map_throttle(NvmlThrottle::GPU_IDLE).any());
        assert!(!map_throttle(NvmlThrottle::NONE).any());
        assert!(!map_throttle(NvmlThrottle::empty()).any());
        // GPU_IDLE alongside a real reason must not mask that reason.
        let mixed = map_throttle(NvmlThrottle::GPU_IDLE | NvmlThrottle::SW_POWER_CAP);
        assert!(mixed.power_cap);
    }

    #[test]
    fn throttle_combined_reasons_set_all_relevant() {
        // A power-capped card that is also thermally limited: both, plus other is clean.
        let r = map_throttle(NvmlThrottle::SW_POWER_CAP | NvmlThrottle::HW_THERMAL_SLOWDOWN);
        assert!(r.power_cap && r.thermal);
        assert!(!r.hw_slowdown && !r.sync_boost && !r.other);
    }

    // ---- Windows-path logic (pure functions — these tests run on every OS) ----

    #[test]
    fn lib_path_linux_tries_versioned_soname_first() {
        // Driver-only installs ship only the versioned soname; the unversioned .so symlink
        // comes with the CUDA toolkit. This pins the documented loading contract.
        assert_eq!(explicit_lib_path_for("linux"), Some("libnvidia-ml.so.1"));
    }

    #[test]
    fn lib_path_windows_relies_on_system32_default_search() {
        // nvml.dll sits in System32 on R510+ drivers — already on the default loader
        // search path. No explicit path, and no probe of the legacy NVSMI directory.
        assert_eq!(explicit_lib_path_for("windows"), None);
    }

    #[test]
    fn v2_process_list_retry_only_on_missing_v3_symbol() {
        // FailedToLoadSymbol is the pre-R510 "driver too old for _v3" signature — the only
        // error that earns the one-shot _v2 retry.
        assert!(should_retry_v2(&NvmlError::FailedToLoadSymbol(
            "nvmlDeviceGetComputeRunningProcesses_v3".into()
        )));
        // Everything else already means "no list this tick" — a retry would just repeat
        // the same failure (or worse, double-poll a dying device).
        assert!(!should_retry_v2(&NvmlError::NotSupported));
        assert!(!should_retry_v2(&NvmlError::DriverNotLoaded));
        assert!(!should_retry_v2(&NvmlError::GpuLost));
        assert!(!should_retry_v2(&NvmlError::Unknown));
        assert!(!should_retry_v2(&NvmlError::UnexpectedVariant(2)));
    }

    #[test]
    fn wddm_per_process_memory_unavailable_is_none_never_zero() {
        // Under WDDM, NVML's usedGpuMemory is ALWAYS Unavailable (the Windows kernel owns
        // that accounting). The honest mapping is None — a fabricated 0 would render as
        // "this process uses no VRAM", exactly the lie the trust thesis forbids.
        assert_eq!(used_gpu_memory_bytes(&UsedGpuMemory::Unavailable), None);
        // A real reported value (Linux, TCC) passes through untouched.
        assert_eq!(
            used_gpu_memory_bytes(&UsedGpuMemory::Used(123_456_789)),
            Some(123_456_789)
        );
    }

    #[test]
    fn driver_model_hint_wddm_names_only_a_source_that_delivers() {
        // With the PDH fill live, the hint may (and must) name Windows GPU performance
        // counters as the per-process source — that claim is now backed by the §2.4 join.
        let hint = windows_process_hint(DriverModelClass::Wddm, PdhAttribution::Available)
            .expect("WDDM must come with an explanation");
        assert!(
            hint.contains("WDDM") && hint.contains("NVML"),
            "hint must name the driver model and why NVML is not the source: {hint}"
        );
        // A failed LUID↔PCI match is an honest terminal state (§2.5): the hint must say
        // the data is UNAVAILABLE — never name a source nothing delivers.
        let hint = windows_process_hint(DriverModelClass::Wddm, PdhAttribution::NoLuidMatch)
            .expect("a failed match must be explained");
        assert!(
            hint.contains("unavailable") && hint.contains("match failed"),
            "failed match must read as unavailable, not as a working source: {hint}"
        );
        // No PDH at all (no WDDM 2.0 GPU/driver): same rule.
        let hint = windows_process_hint(DriverModelClass::Wddm, PdhAttribution::NoPdh)
            .expect("missing PDH must be explained");
        assert!(hint.contains("unavailable"), "{hint}");
    }

    #[test]
    fn driver_model_hint_tcc_has_nothing_to_explain() {
        // TCC (non-display): NVML's per-process accounting works — a hint here would
        // wrongly disclaim data that is actually present.
        assert_eq!(
            windows_process_hint(DriverModelClass::Tcc, PdhAttribution::Available),
            None
        );
        assert_eq!(
            windows_process_hint(DriverModelClass::Tcc, PdhAttribution::NoPdh),
            None
        );
    }

    #[test]
    fn driver_model_hint_unknown_model_is_flagged_not_fatal() {
        // Future MCDM (= 2, missing from nvml-wrapper 0.12's enum) must surface as an
        // honest "unknown model" hint — never a crash, never silence.
        let hint =
            windows_process_hint(DriverModelClass::UnknownVariant, PdhAttribution::Available)
                .expect("an unknown driver model must be flagged");
        assert!(
            hint.contains("unknown"),
            "hint must say the model is unknown: {hint}"
        );
    }

    #[test]
    fn windows_source_caveat_names_the_real_memory_source() {
        // PDH attributable: VidMm dedicated usage is primary, driver view the fallback.
        let c = windows_source_caveat(PdhAttribution::Available).unwrap();
        assert!(c.contains("VidMm") && c.contains("PDH"), "{c}");
        // No PDH for this device: the number IS the driver's virtualized view — the
        // caveat must say so instead of borrowing the VidMm label (§2.3 "driver view").
        for pdh in [PdhAttribution::NoLuidMatch, PdhAttribution::NoPdh] {
            let c = windows_source_caveat(pdh).unwrap();
            assert!(
                c.contains("driver's view") && !c.contains("PDH"),
                "fallback caveat must label the driver view, not PDH: {c}"
            );
        }
    }

    #[test]
    fn driver_model_classification_handles_mcdm_and_query_failure() {
        // UnexpectedVariant is NVML reporting a model newer than this build (MCDM = 2):
        // keep the device, flag the unknown model (§2.6).
        assert_eq!(
            classify_driver_model_err(&NvmlError::UnexpectedVariant(2)),
            DriverModelClass::UnknownVariant
        );
        // Any other query failure defaults to WDDM — the Windows default driver model,
        // and the case where a missing hint would leave an empty column unexplained.
        assert_eq!(
            classify_driver_model_err(&NvmlError::NotSupported),
            DriverModelClass::Wddm
        );
        assert_eq!(
            classify_driver_model_err(&NvmlError::Unknown),
            DriverModelClass::Wddm
        );
    }
}
