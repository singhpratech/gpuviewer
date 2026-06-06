//! Core data model. Every metric is `Option<T>`: absence (`NOT_SUPPORTED`, missing sysfs
//! file, privilege wall) is a normal per-metric outcome, never an error.

use serde::{Deserialize, Serialize};

/// Stable device identity. For PCI devices this is the PCI address (`0000:01:00.0`) so the
/// same physical GPU dedupes across backends; platform devices (Apple) use a platform key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Unknown,
}

impl Vendor {
    pub fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
            Vendor::Apple => "Apple",
            Vendor::Unknown => "GPU",
        }
    }
}

/// Queried once per device at startup (nvtop's `populate_static_info` split).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaticInfo {
    pub id: DeviceId,
    pub vendor: Vendor,
    pub name: String,
    pub backend: String,
    pub mem_total_bytes: Option<u64>,
    pub power_limit_mw: Option<u32>,
    pub max_sm_clock_mhz: Option<u32>,
    /// Temperature at which the driver starts thermal slowdown, if exposed.
    pub temp_slowdown_c: Option<f32>,
    pub driver_version: Option<String>,
    /// One-line explanation of why the process list may be incomplete or absent (WSL2
    /// driver limitation, privilege wall); `None` when there is nothing to explain.
    pub process_hint: Option<String>,
}

/// Decoded throttle/clocks-event reasons. Decoding is tolerant: unknown future bits land in
/// `other` instead of failing (NVML renamed/extended these bits across versions).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleReasons {
    pub thermal: bool,
    pub power_cap: bool,
    pub hw_slowdown: bool,
    pub sync_boost: bool,
    pub other: bool,
}

impl ThrottleReasons {
    /// True if any *performance-limiting* reason is active (idle is not throttling).
    pub fn any(&self) -> bool {
        self.thermal || self.power_cap || self.hw_slowdown || self.sync_boost || self.other
    }

    pub fn labels(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.thermal {
            v.push("thermal");
        }
        if self.power_cap {
            v.push("power cap");
        }
        if self.hw_slowdown {
            v.push("hw slowdown");
        }
        if self.sync_boost {
            v.push("sync boost");
        }
        if self.other {
            v.push("other");
        }
        v
    }
}

/// One per-tick sample of a device (nvtop's `refresh_dynamic_info` split).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DynamicSample {
    /// Unix millis; one timestamp per collection frame (charts jitter otherwise).
    pub ts_ms: u64,
    pub util_pct: Option<f32>,
    pub mem_used_bytes: Option<u64>,
    pub power_mw: Option<u32>,
    pub temp_c: Option<f32>,
    pub fan_pct: Option<f32>,
    pub sm_clock_mhz: Option<u32>,
    pub mem_clock_mhz: Option<u32>,
    pub encoder_pct: Option<f32>,
    pub decoder_pct: Option<f32>,
    pub throttle: ThrottleReasons,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessKind {
    Compute,
    Graphics,
    Both,
    Unknown,
}

impl ProcessKind {
    /// nvidia-smi-style short code for the TUI process table.
    pub fn label(self) -> &'static str {
        match self {
            ProcessKind::Compute => "C",
            ProcessKind::Graphics => "G",
            ProcessKind::Both => "C+G",
            ProcessKind::Unknown => "?",
        }
    }

    /// Prose form for event evidence ("new compute client", not "new C client").
    pub fn prose(self) -> &'static str {
        match self {
            ProcessKind::Compute => "compute",
            ProcessKind::Graphics => "graphics",
            ProcessKind::Both => "compute+graphics",
            ProcessKind::Unknown => "unknown-type",
        }
    }
}

/// Per-process attribution (nvtop's `refresh_running_processes` split).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSample {
    pub pid: u32,
    pub name: String,
    pub kind: ProcessKind,
    pub mem_bytes: Option<u64>,
    pub util_pct: Option<f32>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Human formatting helpers shared by frontends.
pub fn fmt_bytes(b: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = b as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else {
        format!("{:.0} MiB", b / MIB)
    }
}
