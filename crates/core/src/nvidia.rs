//! NVIDIA backend — direct NVML calls via `nvml-wrapper` (runtime-loads the driver's own
//! `libnvidia-ml.so.1` / `nvml.dll`). No nvidia-smi anywhere: nvidia-smi is itself just a
//! CLI wrapper over this same library.
//!
//! Per the domain rules in CLAUDE.md:
//! - `NOT_SUPPORTED` (and any other per-metric error) maps to `None`, never a failure.
//! - The `.so.1` path is tried first: driver-only installs don't ship the unversioned
//!   `.so` symlink (that comes with the CUDA toolkit) — the exact pitfall bottom hit.
//! - Throttle mapping is edge-honest: GPU idle is deliberately NOT narrated as throttling
//!   (an idle GPU is not slow). Configuration limiters (applications-clocks / display-clock
//!   settings) map to the catch-all `other` so they are surfaced without being mislabeled as
//!   a thermal or power slowdown. See [`map_throttle`].
//! - WSL2 is detected once at init: per-process GPU info is N/A *at the driver level*
//!   there, so `StaticInfo::process_hint` explains the empty process table up front
//!   instead of crashing on it (nvtop #432 is the cautionary tale).

use std::collections::HashMap;
use std::ffi::OsStr;

use nvml_wrapper::bitmasks::device::ThrottleReasons as NvmlThrottle;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor, TemperatureThreshold};
use nvml_wrapper::enums::device::UsedGpuMemory;
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

pub struct NvidiaBackend {
    nvml: Nvml,
    /// (nvml index, stable id) established at init.
    devs: Vec<(u32, DeviceId)>,
    /// Per-device watermark for `process_utilization_stats` sampling.
    last_util_ts: HashMap<u32, u64>,
    /// Set once at init: explanation for a known-incomplete process list (WSL2), if any.
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
        // default loader (which also handles nvml.dll on Windows).
        let nvml = Nvml::builder()
            .lib_path(OsStr::new("libnvidia-ml.so.1"))
            .init()
            .or_else(|_| Nvml::init())
            .map_err(|e| BackendError::Unavailable(format!("NVML unavailable: {e}")))?;

        let count = nvml
            .device_count()
            .map_err(|e| BackendError::Unavailable(format!("NVML device count: {e}")))?;

        let mut devs = Vec::new();
        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else {
                continue;
            };
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

        Ok(Self {
            nvml,
            devs,
            last_util_ts: HashMap::new(),
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

    fn process_name(&self, pid: u32) -> String {
        if let Ok(name) = self.nvml.sys_process_name(pid, 128) {
            // NVML returns the full path; keep the basename for readability.
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

        Ok(StaticInfo {
            id: dev.clone(),
            vendor: Vendor::Nvidia,
            name: opt(d.name()).unwrap_or_else(|| "NVIDIA GPU".into()),
            backend: "nvidia".into(),
            mem_total_bytes: opt(d.memory_info()).map(|m| m.total),
            power_limit_mw: opt(d.enforced_power_limit()),
            max_sm_clock_mhz: opt(d.max_clock_info(Clock::SM)),
            temp_slowdown_c: opt(d.temperature_threshold(TemperatureThreshold::Slowdown))
                .map(|t| t as f32),
            driver_version: opt(self.nvml.sys_driver_version()),
            process_hint: self.process_hint.clone(),
        })
    }

    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        let i = self.index_of(dev)?;
        let d = self
            .nvml
            .device_by_index(i)
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        let util = opt(d.utilization_rates());
        let mem = opt(d.memory_info());
        let throttle = opt(d.current_throttle_reasons())
            .map(map_throttle)
            .unwrap_or_default();

        Ok(DynamicSample {
            ts_ms: now_ms(),
            util_pct: util.as_ref().map(|u| u.gpu as f32),
            mem_used_bytes: mem.map(|m| m.used),
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
        let compute = d.running_compute_processes().unwrap_or_default();
        let graphics = d.running_graphics_processes().unwrap_or_default();

        let mut by_pid: HashMap<u32, ProcessSample> = HashMap::new();
        for (list, kind) in [
            (compute, ProcessKind::Compute),
            (graphics, ProcessKind::Graphics),
        ] {
            for p in list {
                let mem = match p.used_gpu_memory {
                    UsedGpuMemory::Used(b) => Some(b),
                    // WDDM / WSL2: VRAM legitimately unavailable — show the process anyway.
                    UsedGpuMemory::Unavailable => None,
                };
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
        // these populate a column, never headline numbers.
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

        Ok(by_pid.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{is_wsl, map_throttle, NvmlThrottle};

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
}
