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
    /// Mandatory honesty label for sources whose numbers need qualification to be read
    /// correctly (design cross-platform.md §5.4): macOS's `mem_total_bytes` is a
    /// unified-memory working-set budget, not VRAM; WDDM's `util_pct` is the busiest
    /// engine's scheduler duty-cycle, not whole-device capacity. Surfaced TUI/report-side
    /// only — deliberately NOT part of the NDJSON device object for now. `None` when the
    /// source needs no caveat. `serde(default)` so older serialized infos still decode.
    #[serde(default)]
    pub source_caveat: Option<String>,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DynamicSample {
    /// Unix millis; one timestamp per collection frame (charts jitter otherwise).
    pub ts_ms: u64,
    pub util_pct: Option<f32>,
    /// Name of the engine whose busy% `util_pct` reports, when utilization is an
    /// engine-headline rather than a whole-device number (Windows WDDM: the busiest
    /// single engine, Task-Manager-comparable — the name makes the headline
    /// self-explaining: "Copy 97%" reads differently from "3D 97%"). `None` where
    /// utilization is device-wide. `serde(default)` for frames recorded before this
    /// field existed.
    #[serde(default)]
    pub util_engine: Option<String>,
    pub mem_used_bytes: Option<u64>,
    pub power_mw: Option<u32>,
    pub temp_c: Option<f32>,
    pub fan_pct: Option<f32>,
    pub sm_clock_mhz: Option<u32>,
    pub mem_clock_mhz: Option<u32>,
    pub encoder_pct: Option<f32>,
    pub decoder_pct: Option<f32>,
    /// `Some(reasons)` only when the source can actually observe throttling (NVML
    /// clocks-event bitmask, AMD `gpu_metrics`, Intel `throttle_reason_*` sysfs).
    /// **`None` means "unobservable", never "not throttling"** — sources with no
    /// throttle interface (Windows WDDM counters, macOS) must not assert the all-false
    /// negative as fact (design cross-platform.md §5.4). `serde(default)` so frames
    /// recorded before this change deserialize (missing field → `None`).
    #[serde(default)]
    pub throttle: Option<ThrottleReasons>,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSample {
    pub pid: u32,
    pub name: String,
    pub kind: ProcessKind,
    pub mem_bytes: Option<u64>,
    pub util_pct: Option<f32>,
    /// Process CPU usage as % of one core (100.0 = one full core); `None` when unknown.
    /// `serde(default)` so frames recorded before this field existed still deserialize.
    #[serde(default)]
    pub cpu_pct: Option<f32>,
    /// Container identity if the process runs in one (e.g. `docker:1a2b3c4d5e6f`);
    /// `None` for host processes or when unknown. `serde(default)` for old recordings.
    #[serde(default)]
    pub container: Option<String>,
}

/// Normalize a PCI address (`domain:bus:dev.func`) for cross-backend dedupe: NVML reports
/// `00000000:01:00.0` while sysfs and D3DKMT-derived ids report `0000:01:00.0` — the same
/// physical GPU. Lowercase everything; trim/zero-pad the domain to 4 hex digits (a
/// genuinely >16-bit domain keeps its extra digits — all sources print those the same
/// way). Returns `None` for anything that isn't a PCI address (`mock:…`, `wddm:…`,
/// `apple:…`, `nvml:0` fallback ids) — those are never deduped: wrongly merging two
/// distinct devices is worse than listing one twice.
///
/// Lives in core (moved from the tui collector per design cross-platform.md §5.4) so the
/// collector's dedupe and the Windows backends' LUID↔PCI matching share one rule.
pub fn normalize_pci_id(id: &str) -> Option<String> {
    let id = id.to_ascii_lowercase();
    let (domain, rest) = id.split_once(':')?;
    let (bus, devfn) = rest.split_once(':')?;
    let (dev, func) = devfn.split_once('.')?;
    // Each segment must be pure hex of plausible width (catches embedded extra `:`/`.`
    // too, since those aren't hex digits).
    let hex = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_hexdigit())
    };
    if !hex(domain, 8) || !hex(bus, 2) || !hex(dev, 2) || !hex(func, 1) {
        return None;
    }
    let domain = format!("{:0>4}", domain.trim_start_matches('0'));
    Some(format!("{domain}:{bus:0>2}:{dev:0>2}.{func}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pci_id_unifies_nvml_and_sysfs_forms() {
        // NVML's 8-hex-digit-domain form, sysfs/D3DKMT's 4-digit form, and case all fold
        // to one key — that equality IS the cross-backend dedupe and the Windows
        // LUID↔PCI match (design §2.5).
        assert_eq!(
            normalize_pci_id("00000000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            normalize_pci_id("0000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            normalize_pci_id("00000000:0A:00.0").as_deref(),
            Some("0000:0a:00.0")
        );
        // Non-zero domains survive normalization in both widths.
        assert_eq!(
            normalize_pci_id("00000001:03:00.0").as_deref(),
            Some("0001:03:00.0")
        );
        assert_eq!(
            normalize_pci_id("0001:03:00.0").as_deref(),
            Some("0001:03:00.0")
        );
    }

    #[test]
    fn normalize_pci_id_rejects_non_pci_ids() {
        // Refuse-to-dedupe cases: synthetic ids must never merge with a real device.
        assert_eq!(normalize_pci_id("mock:0000:01:00.0"), None);
        assert_eq!(normalize_pci_id("nvml:0"), None);
        assert_eq!(normalize_pci_id("wddm:10de:2684:0"), None);
        assert_eq!(normalize_pci_id("apple:m2-max"), None);
        assert_eq!(normalize_pci_id(""), None);
        assert_eq!(normalize_pci_id("0000:01:00"), None); // no function part
        assert_eq!(normalize_pci_id("0000:01:00.0.1"), None); // trailing junk
        assert_eq!(normalize_pci_id("0000:01:02:00.0"), None); // extra segment
    }

    #[test]
    fn throttle_none_is_a_distinct_state_from_observed_all_false() {
        // The honesty pivot of the §5.4 model change: `None` (source cannot observe
        // throttling) and `Some(all-false)` (observed: not throttling) are different
        // claims and must never compare equal. Wire-level assertions (None → JSON null,
        // missing field → None) live in the NDJSON conformance suite
        // (crates/tui/tests/ndjson_contract.rs) — core stays serde_json-free.
        let unobservable: Option<ThrottleReasons> = None;
        let observed_quiet = Some(ThrottleReasons::default());
        assert_ne!(unobservable, observed_quiet);
        // And neither state reads as "throttling".
        assert!(!unobservable.is_some_and(|t| t.any()));
        assert!(!observed_quiet.is_some_and(|t| t.any()));
    }
}
