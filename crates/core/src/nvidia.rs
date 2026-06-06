//! NVIDIA backend — direct NVML calls via `nvml-wrapper` (runtime-loads the driver's own
//! `libnvidia-ml.so.1` / `nvml.dll`). No nvidia-smi anywhere: nvidia-smi is itself just a
//! CLI wrapper over this same library.
//!
//! Per the domain rules in CLAUDE.md:
//! - `NOT_SUPPORTED` (and any other per-metric error) maps to `None`, never a failure.
//! - The `.so.1` path is tried first: driver-only installs don't ship the unversioned
//!   `.so` symlink (that comes with the CUDA toolkit) — the exact pitfall bottom hit.
//! - Throttle mapping is edge-honest: persistent configuration states (GPU idle,
//!   applications-clocks setting, display clocks) are deliberately NOT narrated as
//!   throttling — only real slowdown reasons are.
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

fn map_throttle(bits: NvmlThrottle) -> ThrottleReasons {
    // Deliberately excluded from narration: GPU_IDLE (not throttling),
    // APPLICATIONS_CLOCKS_SETTING / DISPLAY_CLOCK_SETTING (persistent user config —
    // narrating them as a throttle "event" at startup would be a lie).
    let known_other = NvmlThrottle::GPU_IDLE
        | NvmlThrottle::APPLICATIONS_CLOCKS_SETTING
        | NvmlThrottle::DISPLAY_CLOCK_SETTING
        | NvmlThrottle::SW_POWER_CAP
        | NvmlThrottle::HW_SLOWDOWN
        | NvmlThrottle::HW_POWER_BRAKE_SLOWDOWN
        | NvmlThrottle::SW_THERMAL_SLOWDOWN
        | NvmlThrottle::HW_THERMAL_SLOWDOWN
        | NvmlThrottle::SYNC_BOOST
        | NvmlThrottle::NONE;
    ThrottleReasons {
        thermal: bits
            .intersects(NvmlThrottle::SW_THERMAL_SLOWDOWN | NvmlThrottle::HW_THERMAL_SLOWDOWN),
        power_cap: bits.contains(NvmlThrottle::SW_POWER_CAP),
        hw_slowdown: bits
            .intersects(NvmlThrottle::HW_SLOWDOWN | NvmlThrottle::HW_POWER_BRAKE_SLOWDOWN),
        sync_boost: bits.contains(NvmlThrottle::SYNC_BOOST),
        // Tolerant decoding: any future/unknown bit lands here instead of being dropped.
        other: !(bits - known_other).is_empty(),
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

        Ok(by_pid.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::is_wsl;

    #[test]
    fn is_wsl_matches_real_kernel_release_strings() {
        // Current WSL2 naming and the WSL1-era capital-M variant must both match.
        assert!(is_wsl("5.15.167.4-microsoft-standard-WSL2"));
        assert!(is_wsl("4.4.0-19041-Microsoft"));
        // Regular distro kernels must not.
        assert!(!is_wsl("6.17.0-35-generic"));
        assert!(!is_wsl(""));
    }
}
