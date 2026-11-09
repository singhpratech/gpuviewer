//! AMD Linux backend — hand-rolled `std::fs` readers over sysfs/hwmon/fdinfo. No library
//! linkage at all: librocm_smi64 is explicitly off the table (soname churn broke btop
//! twice — btop #774) and libdrm ioctls are unnecessary for everything v1 needs.
//!
//! Per the domain rules in CLAUDE.md:
//! - Every path derives from a root-dir parameter (`with_root`), so the whole backend runs
//!   against committed fixture trees; `init()` is just `with_root("/")`.
//! - A missing file or unparsable value is `None`, never a failure — an APU without hwmon
//!   or `pp_dpm_*` tables is a normal device, not a broken one (Intel-iGPU-style absence).
//! - Throttle bits live in the `gpu_metrics` binary struct, which is versioned (v1_0–v1_9
//!   dGPU with MI300 churn per release, v2_0–v2_4 APU, v3_0 newer APU) with per-version
//!   field offsets AND units, and therefore needs per-version decoders backed by
//!   fixtures. `decode_gpu_metrics_throttle` parses it directly off the sysfs
//!   blob (offsets derived from the in-tree kernel header — see that function). A blob that
//!   is absent, truncated, of an unknown revision, or whose self-declared `structure_size`
//!   is inconsistent decodes to `None`: that source is unobservable, and an asserted
//!   all-false would fabricate an "observed: not throttling" fact (§5.4) from bytes we
//!   never understood.
//! - Only SMU-backed sysfs is polled (`gpu_busy_percent`, hwmon) — never GRBM registers,
//!   whose polling breaks GFXOFF (the monitor must not change what it measures).
//! - Per-process attribution is DRM fdinfo (kernel 5.14+, standardized 5.19+): cumulative
//!   per-engine busy-ns → delta over wall time = util%; `drm-pdev` ties a client to a
//!   device; keys missing on older kernels degrade to `None` fields, never lost processes.
//!   Other users' fdinfo needs root/CAP_SYS_PTRACE, so unprivileged runs carry an honest
//!   "your processes only" `process_hint` instead of pretending the list is complete.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::{BackendError, GpuBackend};
use crate::model::{
    now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo, ThrottleReasons,
    Vendor,
};

/// Read a sysfs file and parse its trimmed contents; absent file or bad value → `None`.
fn read_parse<T: std::str::FromStr>(path: &Path) -> Option<T> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Read a sysfs file as a trimmed string; absent file → `None`.
fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Read a sysfs PCI id ("0x744c\n" → "744c", lowercased). An empty file is not an id —
/// `None`, so the name fallback says "AMD GPU" rather than "AMD GPU [1002:]".
fn read_hex_id(path: &Path) -> Option<String> {
    let s = read_trim(path)?;
    let s = s.strip_prefix("0x").unwrap_or(&s).to_ascii_lowercase();
    (!s.is_empty()).then_some(s)
}

/// MHz of one `pp_dpm_*` table line ("1: 1138Mhz *"). The unit's casing varies across
/// kernels ("Mhz"/"MHz"), so only the leading digits after the colon are trusted.
fn dpm_line_mhz(line: &str) -> Option<u32> {
    let after = line.split(':').nth(1)?.trim_start();
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The '*'-marked level of a DPM table is the currently selected one.
fn dpm_current_mhz(table: &str) -> Option<u32> {
    table
        .lines()
        .find(|l| l.contains('*'))
        .and_then(dpm_line_mhz)
}

/// Levels are listed ascending — the last line is the hardware maximum.
fn dpm_max_mhz(table: &str) -> Option<u32> {
    table
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(dpm_line_mhz)
}

/// Which hwmon temperature channel `temp_c` reports. amdgpu's channel meanings are fixed
/// and labeled (amdgpu_pm.c: temp1 = "edge", temp2 = "junction", temp3 = "mem"), but not
/// every ASIC exposes every channel: MI300 has NO edge channel at all (junction + mem
/// only), so an edge-only lookup reported `None` despite a real junction reading — the
/// honesty contract violated in the Some-reported-as-None direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TempSensor {
    /// The "edge"-labeled die-edge sensor every vendor tool headlines.
    Edge,
    /// The "junction" (hotspot) sensor — a real reading, but it runs hotter than edge, so
    /// choosing it is surfaced as a caveat rather than left to be compared with edge temps.
    Junction,
    /// `temp1_input` with no label file at all: amdgpu's edge channel by the fixed channel
    /// order, but unconfirmed by a label — surfaced as such.
    Temp1Unlabeled,
}

impl TempSensor {
    /// The `StaticInfo::source_caveat` honesty label naming the chosen sensor whenever it
    /// is not the edge-labeled one. Edge is the default reading and needs no label.
    fn caveat(self) -> Option<&'static str> {
        match self {
            TempSensor::Edge => None,
            TempSensor::Junction => Some(
                "temp is the junction (hotspot) sensor: this device has no edge sensor, and junction reads hotter than edge",
            ),
            TempSensor::Temp1Unlabeled => {
                Some("temp is unlabeled hwmon temp1 (amdgpu's edge channel by channel order)")
            }
        }
    }
}

/// hwmon temps are MILLI-degrees C → °C, from the channel [`pick_temp`] chooses.
fn temp_c(hwmon: &Path) -> Option<f32> {
    let (_, millic) = pick_temp(hwmon)?;
    Some(millic as f32 / 1000.0)
}

/// The fallback chain edge → junction → unlabeled temp1. Edge first: junction/mem run
/// hotter and would overstate. Junction next: on edge-less devices (MI300) it is the only
/// die temperature, and an honest Some beats a false None — the caveat says which sensor
/// it is. temp1 last, and ONLY when it carries no label: a labeled temp1 that is neither
/// edge nor junction (e.g. "mem") is a different physical claim than device temperature,
/// so it is never reported as one. A "mem" sensor is never chosen at any step.
fn pick_temp(hwmon: &Path) -> Option<(TempSensor, i64)> {
    if let Some(m) = labeled_temp_millic(hwmon, "edge") {
        return Some((TempSensor::Edge, m));
    }
    if let Some(m) = labeled_temp_millic(hwmon, "junction") {
        return Some((TempSensor::Junction, m));
    }
    if hwmon.join("temp1_label").exists() {
        return None;
    }
    read_parse(&hwmon.join("temp1_input")).map(|m| (TempSensor::Temp1Unlabeled, m))
}

/// The `tempN_input` of the channel whose `tempN_label` reads exactly `label`. Names are
/// sorted so the choice is deterministic (read_dir order is arbitrary).
fn labeled_temp_millic(hwmon: &Path, label: &str) -> Option<i64> {
    let mut names: Vec<String> = fs::read_dir(hwmon)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names.iter().find_map(|name| {
        let stem = name.strip_suffix("_label")?;
        if !stem.starts_with("temp") || read_trim(&hwmon.join(name)).as_deref() != Some(label) {
            return None;
        }
        read_parse(&hwmon.join(format!("{stem}_input")))
    })
}

/// hwmon power is MICROwatts; the model carries milliwatts. RDNA3 may expose only the
/// instantaneous `power1_input` (kernel 6.7+) and no `power1_average` — probe both.
fn power_mw(hwmon: &Path) -> Option<u32> {
    let uw: u64 = read_parse(&hwmon.join("power1_average"))
        .or_else(|| read_parse(&hwmon.join("power1_input")))?;
    Some((uw / 1000) as u32)
}

/// `power1_cap` is MICROwatts too.
fn power_cap_mw(hwmon: &Path) -> Option<u32> {
    let uw: u64 = read_parse(&hwmon.join("power1_cap"))?;
    Some((uw / 1000) as u32)
}

/// `fan1_input`/`fan1_max` are RPM; percent-of-max is what the model carries.
fn fan_pct(hwmon: &Path) -> Option<f32> {
    let rpm: f32 = read_parse(&hwmon.join("fan1_input"))?;
    let max: f32 = read_parse(&hwmon.join("fan1_max"))?;
    fan_pct_of_max(rpm, max)
}

/// f32's parser happily accepts "nan"/"inf", and a broken sensor can report a negative
/// RPM — none of those is a fan reading. Garbage in must be `None` out, never a
/// confident percentage.
fn fan_pct_of_max(rpm: f32, max: f32) -> Option<f32> {
    if !rpm.is_finite() || !max.is_finite() || rpm < 0.0 || max <= 0.0 {
        return None;
    }
    Some((rpm / max * 100.0).clamp(0.0, 100.0))
}

// ---- gpu_metrics throttle decoding ------------------------------------------------------
//
// The `gpu_metrics` sysfs node is a memcpy of the SMU firmware metrics table into a kernel
// C struct that is then exposed verbatim to userspace. The structs (kgd_pp_interface.h) are
// NOT `__attribute__((packed))`, so the on-disk layout is the firmware's NATURAL-aligned
// layout: every field sits at an offset that is a multiple of its own size (u16→2, u32→4,
// u64→8), and the kernel devs ordered members to minimise — but not always eliminate —
// inter-field padding. All offsets below are derived by walking the member list with that
// rule; the workings are shown inline so a future header bump can be re-verified by hand.
//
// Everything is little-endian (the only architectures amdgpu runs on). Every read is bounds-
// and structure_size-gated: a short, lying, or unknown-revision blob is absence, never an
// error and never a misattributed cause.

/// The common header every `gpu_metrics_v*` opens with (kgd_pp_interface.h
/// `struct metrics_table_header`): `u16 structure_size; u8 format_revision;
/// u8 content_revision;` — 4 bytes, no trailing padding.
const HEADER_LEN: usize = 4;

/// Read a little-endian `u32` at `off`, or `None` if it would run past the buffer.
fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    let bytes = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Read a little-endian `u64` at `off`, or `None` if it would run past the buffer.
fn read_u64_le(buf: &[u8], off: usize) -> Option<u64> {
    let bytes = buf.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

// ASIC-INDEPENDENT throttler bits (`indep_throttle_status`), from amdgpu_smu.h
// `SMU_THROTTLER_*_BIT`. These are normalised by the driver across ASICs, so unlike the
// legacy `throttle_status` they are safe to map to specific causes.
//
// Power group (PPT = package power tracking, SPL/FPPT/SPPT = APU power limits):
const SMU_THROTTLER_PPT0_BIT: u32 = 0;
const SMU_THROTTLER_PPT1_BIT: u32 = 1;
const SMU_THROTTLER_PPT2_BIT: u32 = 2;
const SMU_THROTTLER_PPT3_BIT: u32 = 3;
const SMU_THROTTLER_SPL_BIT: u32 = 4;
const SMU_THROTTLER_FPPT_BIT: u32 = 5;
const SMU_THROTTLER_SPPT_BIT: u32 = 6;
const SMU_THROTTLER_SPPT_APU_BIT: u32 = 7;
// Current group (TDC = thermal design current, EDC = electrical design current, APCC):
const SMU_THROTTLER_TDC_GFX_BIT: u32 = 16;
const SMU_THROTTLER_TDC_SOC_BIT: u32 = 17;
const SMU_THROTTLER_TDC_MEM_BIT: u32 = 18;
const SMU_THROTTLER_TDC_VDD_BIT: u32 = 19;
const SMU_THROTTLER_TDC_CVIP_BIT: u32 = 20;
const SMU_THROTTLER_EDC_CPU_BIT: u32 = 21;
const SMU_THROTTLER_EDC_GFX_BIT: u32 = 22;
const SMU_THROTTLER_APCC_BIT: u32 = 23;
// Temperature group:
const SMU_THROTTLER_TEMP_GPU_BIT: u32 = 32;
const SMU_THROTTLER_TEMP_CORE_BIT: u32 = 33;
const SMU_THROTTLER_TEMP_MEM_BIT: u32 = 34;
const SMU_THROTTLER_TEMP_EDGE_BIT: u32 = 35;
const SMU_THROTTLER_TEMP_HOTSPOT_BIT: u32 = 36;
const SMU_THROTTLER_TEMP_SOC_BIT: u32 = 37;
const SMU_THROTTLER_TEMP_VR_GFX_BIT: u32 = 38;
const SMU_THROTTLER_TEMP_VR_SOC_BIT: u32 = 39;
const SMU_THROTTLER_TEMP_VR_MEM0_BIT: u32 = 40;
const SMU_THROTTLER_TEMP_VR_MEM1_BIT: u32 = 41;
const SMU_THROTTLER_TEMP_LIQUID0_BIT: u32 = 42;
const SMU_THROTTLER_TEMP_LIQUID1_BIT: u32 = 43;
const SMU_THROTTLER_VRHOT0_BIT: u32 = 44;
const SMU_THROTTLER_VRHOT1_BIT: u32 = 45;
// PROCHOT (external "processor hot" assertion → hardware-forced slowdown):
const SMU_THROTTLER_PROCHOT_CPU_BIT: u32 = 46;
const SMU_THROTTLER_PROCHOT_GFX_BIT: u32 = 47;
// Other:
const SMU_THROTTLER_PPM_BIT: u32 = 56;
const SMU_THROTTLER_FIT_BIT: u32 = 57;

/// Thermal-group `indep_throttle_status` bits: any of these is a temperature limit. VRHOT
/// (voltage-regulator over-temp) and the liquid-cooling sensors belong here too.
const INDEP_THERMAL_MASK: u64 = (1 << SMU_THROTTLER_TEMP_GPU_BIT)
    | (1 << SMU_THROTTLER_TEMP_CORE_BIT)
    | (1 << SMU_THROTTLER_TEMP_MEM_BIT)
    | (1 << SMU_THROTTLER_TEMP_EDGE_BIT)
    | (1 << SMU_THROTTLER_TEMP_HOTSPOT_BIT)
    | (1 << SMU_THROTTLER_TEMP_SOC_BIT)
    | (1 << SMU_THROTTLER_TEMP_VR_GFX_BIT)
    | (1 << SMU_THROTTLER_TEMP_VR_SOC_BIT)
    | (1 << SMU_THROTTLER_TEMP_VR_MEM0_BIT)
    | (1 << SMU_THROTTLER_TEMP_VR_MEM1_BIT)
    | (1 << SMU_THROTTLER_TEMP_LIQUID0_BIT)
    | (1 << SMU_THROTTLER_TEMP_LIQUID1_BIT)
    | (1 << SMU_THROTTLER_VRHOT0_BIT)
    | (1 << SMU_THROTTLER_VRHOT1_BIT);

/// Power/current-group bits: PPT* (power-cap) and the TDC/EDC current limits all mean "you
/// are being pulled back to stay inside an electrical/power envelope".
const INDEP_POWER_MASK: u64 = (1 << SMU_THROTTLER_PPT0_BIT)
    | (1 << SMU_THROTTLER_PPT1_BIT)
    | (1 << SMU_THROTTLER_PPT2_BIT)
    | (1 << SMU_THROTTLER_PPT3_BIT)
    | (1 << SMU_THROTTLER_SPL_BIT)
    | (1 << SMU_THROTTLER_FPPT_BIT)
    | (1 << SMU_THROTTLER_SPPT_BIT)
    | (1 << SMU_THROTTLER_SPPT_APU_BIT)
    | (1 << SMU_THROTTLER_TDC_GFX_BIT)
    | (1 << SMU_THROTTLER_TDC_SOC_BIT)
    | (1 << SMU_THROTTLER_TDC_MEM_BIT)
    | (1 << SMU_THROTTLER_TDC_VDD_BIT)
    | (1 << SMU_THROTTLER_TDC_CVIP_BIT)
    | (1 << SMU_THROTTLER_EDC_CPU_BIT)
    | (1 << SMU_THROTTLER_EDC_GFX_BIT);

/// PROCHOT is an external slowdown forced on the GPU by the platform — the AMD analogue of
/// NVML's HW_SLOWDOWN.
const INDEP_HW_SLOWDOWN_MASK: u64 =
    (1 << SMU_THROTTLER_PROCHOT_CPU_BIT) | (1 << SMU_THROTTLER_PROCHOT_GFX_BIT);

/// Recognised-but-uncategorised bits (APCC interconnect throttle, PPM, FIT failure-in-time).
/// Kept explicit so the "unknown future bit" branch in [`map_indep_throttle`] only fires for
/// genuinely new bits, not for ones we know about but do not split into a column.
const INDEP_OTHER_MASK: u64 =
    (1 << SMU_THROTTLER_APCC_BIT) | (1 << SMU_THROTTLER_PPM_BIT) | (1 << SMU_THROTTLER_FIT_BIT);

/// Map an `indep_throttle_status` word to [`ThrottleReasons`]. Tolerant like the NVIDIA
/// decoder: any bit outside the masks we recognise lands in `other` rather than being
/// dropped, so a firmware/header that adds a throttler still surfaces "something is
/// throttling" honestly.
fn map_indep_throttle(bits: u64) -> ThrottleReasons {
    let known = INDEP_THERMAL_MASK | INDEP_POWER_MASK | INDEP_HW_SLOWDOWN_MASK | INDEP_OTHER_MASK;
    ThrottleReasons {
        thermal: bits & INDEP_THERMAL_MASK != 0,
        power_cap: bits & INDEP_POWER_MASK != 0,
        hw_slowdown: bits & INDEP_HW_SLOWDOWN_MASK != 0,
        // AMD has no cross-GPU sync-boost concept in this status word.
        sync_boost: false,
        other: (bits & INDEP_OTHER_MASK != 0) || (bits & !known != 0),
    }
}

/// Where the throttle words live in each supported `(format_revision, content_revision)`,
/// plus the struct's own byte length (for the `structure_size` sanity gate).
struct ThrottleLayout {
    /// Offset of `indep_throttle_status` (ASIC-independent u64), when the version carries it.
    indep: Option<usize>,
    /// Offset of the legacy ASIC-specific `throttle_status` (u32). Decoded only as a coarse
    /// "something is throttling" when no `indep` word exists — or when the `indep` word
    /// reads as the firmware's 0xFF "never written" sentinel.
    legacy: Option<usize>,
    /// `sizeof` the struct (natural-aligned). The blob must be at least this big and its
    /// self-declared `structure_size` must match, or it is treated as absence.
    size: usize,
}

/// Resolve the throttle layout for a `(format, content)` revision, or `None` for revisions
/// that carry no throttle data we trust. Offsets are walked from the kernel structs in
/// kgd_pp_interface.h under natural C alignment (see the module note above).
fn throttle_layout(format: u8, content: u8) -> Option<ThrottleLayout> {
    match (format, content) {
        // gpu_metrics_v1_0 (Vega12, Vega20 = Radeon VII / MI50 / MI60) has its OWN prefix
        // (system_clock_counter first, u32 energy):
        //   header(4) → 4 pad bytes (the next member is a u64) → system_clock_counter u64
        //   @8 → @16, 6×u16 temps (edge/hotspot/mem/vrgfx/vrsoc/vrmem) @16 → @28,
        //   3×u16 activity (gfx/umc/mm) @28 → @34, average_socket_power u16 @34 → @36,
        //   energy_accumulator u32 @36 → @40, 7×u16 average clocks @40 → @54,
        //   7×u16 current clocks @54 → @68 → throttle_status u32 @68.
        // The walk lands on exactly the audit's compiled offsetof (07 §2.2: legacy@68).
        (1, 0) => Some(ThrottleLayout {
            indep: None,
            legacy: Some(68),
            // … current_fan_speed u16 @72, pcie_link_width u8 @74, pcie_link_speed u8 @75
            // → 76 data bytes; the u64 member aligns the struct to 8 → sizeof 80 (the
            // audit's compiled sizeof).
            size: 80,
        }),
        // gpu_metrics_v1_1..v1_3 (dGPU). Shared prefix, naturally aligned with no padding:
        //   header(4) + 10×u16 temps/activity/power(20)=24 → energy_accumulator u64 @24,
        //   system_clock_counter u64 @32, then 14×u16 avg+current clocks(28) @40 →
        //   throttle_status u32 @68.
        (1, 1) => Some(ThrottleLayout {
            indep: None,
            legacy: Some(68),
            // … fan_speed/pcie(3×u16)+padding(u16)@72, gfx/mem_activity_acc(2×u32)@80,
            // temperature_hbm[4] u16 @88 → 96.
            size: 96,
        }),
        (1, 2) => Some(ThrottleLayout {
            indep: None,
            legacy: Some(68),
            // … as v1.1 up to @96, then firmware_timestamp u64 @96 → 104.
            size: 104,
        }),
        (1, 3) => Some(ThrottleLayout {
            // … v1.2 up to firmware_timestamp@96(→104), voltage_soc/gfx/mem+padding1
            // (4×u16)@104 → indep_throttle_status u64 @112 → 120.
            indep: Some(112),
            legacy: Some(68),
            size: 120,
        }),
        // gpu_metrics_v1_4 and v1_6 are header-defined but no tagged kernel has been
        // observed emitting them (07 §2.2) — deliberately NOT mapped: an unexercised table
        // is an unverified table. They fall through to `None` (unobservable).
        //
        // gpu_metrics_v1_5 (MI300 series, kernels 6.7–6.12). A different prefix again —
        // no clocks before the throttle word, but the VCN/JPEG activity arrays are:
        //   header(4), 6×u16 (temperature_hotspot/mem/vrsoc, curr_socket_power,
        //   average_gfx/umc_activity) @4 → @16, vcn_activity[NUM_VCN=4] u16 @16 → @24,
        //   jpeg_activity[NUM_JPEG_ENG=32] u16 @24 → @88, energy_accumulator u64 @88 → @96,
        //   system_clock_counter u64 @96 → @104 → throttle_status u32 @104.
        // No indep word in this revision. MI300's throttle_status bits are ASIC-specific
        // (smu_v13_0_6), so like every legacy word it decodes only as a coarse `other`.
        (1, 5) => Some(ThrottleLayout {
            indep: None,
            legacy: Some(104),
            // … gfxclk_lock_status u32 @108, pcie_link_width/speed u16 @112,
            // xgmi_link_width/speed u16 @116, gfx/mem_activity_acc u32 @120 → @128,
            // 5×u64 pcie_bandwidth_acc/inst + l0_to_recov/replay/replay_rover counts
            // @128 → @168, pcie_nak_sent/rcvd u32 @168 → @176,
            // xgmi_read/write_data_acc[NUM_XGMI_LINKS=8] u64 @176 → @304,
            // firmware_timestamp u64 @304 → @312, current_gfxclk[MAX_GFX_CLKS=8] +
            // current_socclk/vclk0/dclk0[MAX_CLKS=4] + current_uclk + padding (22×u16)
            // @312 → @356; u64 alignment → sizeof 360. Both of the audit's compiled
            // numbers (07 §2.2: legacy@104, size 360) fall out of this walk independently
            // — the size check exercises every tail member, not just the prefix.
            size: 360,
        }),
        // gpu_metrics_v1_7 / v1_8 (MI300/MI325/MI350, 6.13+) carry no status word, only
        // residency ACCUMULATORS — recognised outside this table, see
        // `residency_snapshot`. gpu_metrics_v1_9 (attribute-vector format, 6.17-dev) is a
        // WATCH ITEM: its sysfs serialization is unknown (07 §2.2 [HW-VERIFY]), so it is
        // deliberately unmapped and decodes to `None`.
        //
        // gpu_metrics_v2_x (APU). v2.0's prefix differs (system_clock_counter first), so it
        // needs its own offsets; v2.1+ share a prefix where throttle_status lands at @108.
        (2, 0) => Some(ThrottleLayout {
            indep: None,
            // header(4) → pad(4) → system_clock_counter u64 @8; then temps/core/l3
            // (1+1+8+2 u16)@16=24 → @40, activity/power(6 u16)@40 → @52,
            // average_core_power[8](16)@52 → @68, 6 avg + 6 current clocks(12 u16)@68 → @92,
            // current_coreclk[8](16)@92 → @108, current_l3clk[2](4)@108 → @112 →
            // throttle_status u32 @112.
            legacy: Some(112),
            // fan_pwm u16 @116, padding u16 @118 → 120.
            size: 120,
        }),
        (2, 1) => Some(ThrottleLayout {
            indep: None,
            // header(4) → temps/core/l3(1+1+8+2 u16)@4=24 → @28, gfx/mm activity(2 u16) →
            // @32, system_clock_counter u64 @32 → @40, 4 power u16 @40 → @48,
            // average_core_power[8](16)@48 → @64, 6 avg+6 current clocks(12 u16)@64 → @88,
            // current_coreclk[8](16)@88 → @104, current_l3clk[2](4)@104 → @108 →
            // throttle_status u32 @108.
            legacy: Some(108),
            // fan_pwm u16 @112, padding[3] u16 @114 → 120.
            size: 120,
        }),
        (2, 2) => Some(ThrottleLayout {
            // v2.1 prefix → throttle_status@108, fan_pwm@112, padding[3]@114 → @120 →
            // indep_throttle_status u64 @120 → 128. (The kernel struct carries indep from
            // v2.2 onward, one content-rev earlier than some docs claim — header wins.)
            indep: Some(120),
            legacy: Some(108),
            size: 128,
        }),
        (2, 3) => Some(ThrottleLayout {
            // v2.2 up to indep@120(→128), then average_temperature_gfx/soc/core[8]/l3[2]
            // (12 u16)@128 → 152.
            indep: Some(120),
            legacy: Some(108),
            size: 152,
        }),
        (2, 4) => Some(ThrottleLayout {
            // v2.3 up to @152, then average cpu/soc/gfx voltage+current (6 u16)@152 → 164
            // DATA bytes — but `sizeof` is 168, not 164: the struct's u64 members align the
            // whole struct to 8, adding 4 tail-pad bytes, and the kernel sets
            // structure_size = sizeof (smu_cmn.h). Real Van Gogh blobs (Steam Deck,
            // kernel 6.6+ program-6 firmware) declare 168; gating on 164 rejected them all.
            indep: Some(120),
            legacy: Some(108),
            size: 168,
        }),
        // gpu_metrics_v3_0 (newer APU) carries no instantaneous throttle word at all —
        // see the explicit (3, 0) branch in `decode_gpu_metrics_throttle` for why its
        // residency ACCUMULATORS cannot honestly decode from a single sample.
        _ => None,
    }
}

/// Decode AMD throttle status from a raw `gpu_metrics` sysfs blob.
///
/// Offsets are derived from the in-tree kernel headers
/// (`drivers/gpu/drm/amd/include/kgd_pp_interface.h` for the struct layouts and
/// `drivers/gpu/drm/amd/pm/swsmu/inc/amdgpu_smu.h` for the `SMU_THROTTLER_*_BIT` values).
/// The structs are naturally aligned, little-endian.
///
/// The contract is *honest absence on any doubt*: a buffer too short for the header, a
/// `structure_size` that disagrees with the version's known length (or is shorter than the
/// fields we read), an unknown `(format, content)` revision, or an out-of-bounds field all
/// yield `None`. `Some` is returned only off a successfully decoded struct, because under
/// §5.4 an all-false `ThrottleReasons` is an *observation* of quiet — emitting it from a
/// blob we could not decode would fabricate a fact-grade "not throttling" forever on any
/// kernel shipping a revision newer than this table. Garbage content is a broken interface,
/// which is unobservable too — refusing beats guessing. We never wrap, never panic, and
/// never narrate a byte we are not certain of.
///
/// Mapping precedence: prefer `indep_throttle_status` (ASIC-independent bits) and split it
/// into thermal / power_cap / hw_slowdown / other. For older revisions that expose only the
/// legacy ASIC-specific `throttle_status`, the per-bit meaning varies by ASIC and is not
/// safe to map; a nonzero value reliably means "some throttler is active", so it surfaces
/// as `other` alone rather than as a fabricated specific cause.
///
/// 0xFF sentinel: the SMU memsets the whole metrics table to 0xFF before writing the
/// fields this ASIC's firmware supports (smu_cmn.h), so an all-ones word — `u64::MAX`
/// indep / `u32::MAX` legacy — is the firmware's "never wrote this" marker, NOT 64
/// simultaneous throttlers. A sentinel word is unobservable: decoding falls through
/// indep → legacy → `None` as each word in turn proves unwritten. Cyan Skillfish (BC-250
/// class) is the known shipper: it emits v2_2 without ever writing `indep_throttle_status`,
/// and without this guard every sample would narrate thermal+power+hw_slowdown+other,
/// permanently.
///
/// v3_0 is recognised but its per-sample throttle decode is deliberately `None` — see the
/// `(3, 0)` branch below.
pub fn decode_gpu_metrics_throttle(buf: &[u8]) -> Option<ThrottleReasons> {
    // Common header: structure_size u16 @0, format_revision u8 @2, content_revision u8 @3.
    if buf.len() < HEADER_LEN {
        return None;
    }
    let structure_size = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let format = buf[2];
    let content = buf[3];

    // The blob must hold at least as many bytes as it claims, and the claim must be a real
    // header (a zeroed/garbage node reads structure_size 0). Both guards reject truncation.
    if structure_size < HEADER_LEN || buf.len() < structure_size {
        return None;
    }

    if let Some(layout) = throttle_layout(format, content) {
        // structure_size must match the version's known size exactly — a mismatch means we
        // are not looking at the struct we think we are, so we decode nothing.
        if structure_size != layout.size {
            return None;
        }
        if let Some(off) = layout.indep {
            // An OOB read after the size gate would mean a wrong layout table, not a quiet
            // GPU — propagate the doubt as None rather than asserting all-false.
            let bits = read_u64_le(buf, off)?;
            // All-ones is the SMU's 0xFF-memset "field never written" sentinel (see the
            // doc comment): this word is unobservable — fall through to the legacy word
            // rather than asserting every throttler active.
            if bits != u64::MAX {
                return Some(map_indep_throttle(bits));
            }
        }
        if let Some(off) = layout.legacy {
            // Legacy throttle_status is ASIC-specific: nonzero = throttling, cause
            // unmappable. Honest coarse signal beats both silence and a guessed column;
            // a decoded zero is a genuine observation of quiet.
            let v = read_u32_le(buf, off)?;
            // All-ones is the same memset sentinel: "unsupported", not "throttling hard".
            if v != u32::MAX {
                return Some(ThrottleReasons {
                    other: v != 0,
                    ..Default::default()
                });
            }
        }
        // Every throttle word this version carries was either absent or the 0xFF
        // sentinel: this firmware exposes no throttle observation at all.
        return None;
    }

    // gpu_metrics_v3_0 (Strix Point / Strix Halo / Krackan class APUs, kernel 6.7+) is
    // recognised but deliberately NOT decoded into a per-sample throttle state. It drops
    // the status bitfields entirely; what it carries instead are `throttle_residency_*`
    // u32 fields (compiled offsets 228..252, behind a 2-byte pad after
    // `current_gfx_maxfreq` u16 @224) that are firmware ACCUMULATOR counters —
    // "incremented on every metrics table update while X was engaged"
    // (smu14_driver_if_v14_0_0.h). A nonzero accumulator records that the throttler fired
    // at SOME point in the firmware's window, not that it is active now: decoding
    // raw-nonzero as "throttling" would turn one historic PROCHOT into a permanent
    // hw_slowdown claim on every subsequent sample. Deriving a live state needs a
    // watermark DELTA between two reads (the fdinfo engine-ns pattern), which this
    // single-blob pure decoder cannot hold state for — so per the honesty contract the
    // per-sample decode is None (unobservable), never a stale-nonzero false positive.
    // (Any future stateful delta decoder must also treat u32::MAX counters as the 0xFF
    // memset sentinel — unsupported, not saturated.)
    if (format, content) == (3, 0) {
        return None;
    }

    // gpu_metrics_v1_7 / v1_8 (MI300 class, kernel 6.13+) are in the same position: the
    // status word is gone, replaced by residency accumulators plus an
    // `accumulation_counter`. A single blob cannot say what is happening NOW, so the
    // pure single-sample decode is None. Unlike v3_0 these revisions carry the
    // accumulation counter that makes a two-read delta well-defined — see
    // [`decode_gpu_metrics_throttle_delta`], which the backend applies across ticks.
    if matches!((format, content), (1, 7) | (1, 8)) {
        return None;
    }

    // Unknown future revision (including the v1_9 watch item and the never-emitted
    // v1_4/v1_6): we cannot observe throttling through a struct we cannot decode, so this
    // is absence — not an asserted "no throttle".
    None
}

/// End of the v1_7/v1_8 residency block: `hbm_thm_residency_acc` u32 @60 → 64.
const RESIDENCY_PREFIX_LEN: usize = 64;

/// The throttle-residency block of `gpu_metrics_v1_7` / `v1_8`, at offsets walked from
/// kgd_pp_interface.h under natural alignment:
///   header(4), 6×u16 (temperature_hotspot/mem/vrsoc, curr_socket_power,
///   average_gfx/umc_activity) @4 → @16, mem_max_bandwidth u64 @16, energy_accumulator
///   u64 @24, system_clock_counter u64 @32 → accumulation_counter u32 @40, then the
///   accumulated residencies prochot @44, ppt @48, socket_thm @52, vr_thm @56,
///   hbm_thm @60 (u32 each) → 64.
/// This matches the audit's compiled offsets (07 §2.2: accumulation_counter@40,
/// residency-acc @44–60). The two revisions share this prefix and differ only in their
/// version-elastic tail (per-XCP stats arrays), whose `sizeof` the in-repo docs do NOT
/// pin — so these revisions are PREFIX-gated (structure_size must cover the block),
/// never size-matched against a number we cannot verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResidencyAcc {
    /// Incremented by firmware on every metrics-table update — the window denominator.
    accumulation_counter: u32,
    prochot: u32,
    ppt: u32,
    socket_thm: u32,
    vr_thm: u32,
    hbm_thm: u32,
}

/// Parse the residency block of a v1_7/v1_8 blob, or `None` for any other revision, a
/// short/lying blob, or a block holding the 0xFF memset sentinel in ANY word. All five
/// counters must be observable: a missing one would turn its category's `false` into a
/// fabricated "not throttling for this reason".
fn residency_snapshot(buf: &[u8]) -> Option<ResidencyAcc> {
    if buf.len() < HEADER_LEN || !matches!((buf[2], buf[3]), (1, 7) | (1, 8)) {
        return None;
    }
    let structure_size = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if structure_size < RESIDENCY_PREFIX_LEN || buf.len() < structure_size {
        return None;
    }
    let snap = ResidencyAcc {
        accumulation_counter: read_u32_le(buf, 40)?,
        prochot: read_u32_le(buf, 44)?,
        ppt: read_u32_le(buf, 48)?,
        socket_thm: read_u32_le(buf, 52)?,
        vr_thm: read_u32_le(buf, 56)?,
        hbm_thm: read_u32_le(buf, 60)?,
    };
    let words = [
        snap.accumulation_counter,
        snap.prochot,
        snap.ppt,
        snap.socket_thm,
        snap.vr_thm,
        snap.hbm_thm,
    ];
    (!words.contains(&u32::MAX)).then_some(snap)
}

/// Throttle state over the window between two residency snapshots: a throttler whose
/// residency counter advanced was engaged during the window (the watermark-delta pattern
/// of the fdinfo engine-ns logic; amd-smi derives its violation status the same way).
/// Mapping: prochot → hw_slowdown, ppt → power_cap, socket/VR/HBM thermal → thermal.
///
/// `None` (no claim) when the firmware did not advance `accumulation_counter` (no new
/// window), when any counter went backwards (driver reload / reset / wrap — re-baseline
/// rather than guess), or when a residency advanced by MORE than the accumulation counter
/// (impossible for an honest counter pair: garbage, not an observation).
fn residency_delta(prev: &ResidencyAcc, cur: &ResidencyAcc) -> Option<ThrottleReasons> {
    let window = cur
        .accumulation_counter
        .checked_sub(prev.accumulation_counter)?;
    if window == 0 {
        return None;
    }
    let d = |p: u32, c: u32| c.checked_sub(p).filter(|&x| x <= window);
    let prochot = d(prev.prochot, cur.prochot)?;
    let ppt = d(prev.ppt, cur.ppt)?;
    let socket_thm = d(prev.socket_thm, cur.socket_thm)?;
    let vr_thm = d(prev.vr_thm, cur.vr_thm)?;
    let hbm_thm = d(prev.hbm_thm, cur.hbm_thm)?;
    Some(ThrottleReasons {
        thermal: socket_thm > 0 || vr_thm > 0 || hbm_thm > 0,
        power_cap: ppt > 0,
        hw_slowdown: prochot > 0,
        sync_boost: false,
        // Every residency counter maps to a named column; none is uncategorised.
        other: false,
    })
}

/// Decode AMD throttle state from TWO successive `gpu_metrics` v1_7/v1_8 blobs (MI300
/// class, kernel 6.13+), whose throttle data is residency accumulators rather than a
/// status word. `Some` reports which throttlers were engaged between the two reads (an
/// all-false `Some` is an observed quiet window); `None` whenever either blob is not a
/// decodable v1_7/v1_8 residency block or the pair does not form an honest window — see
/// [`residency_delta`]. Every other revision returns `None` here: use
/// [`decode_gpu_metrics_throttle`] for those.
pub fn decode_gpu_metrics_throttle_delta(prev: &[u8], cur: &[u8]) -> Option<ThrottleReasons> {
    residency_delta(&residency_snapshot(prev)?, &residency_snapshot(cur)?)
}

/// amdgpu's fdinfo engine names (amdgpu_fdinfo.c `amdgpu_ip_name`), in the index order the
/// per-pid engine arrays use. UVD and VCN decode both print "dec" (an ASIC has one or the
/// other); "enc_1" is the separate UVD-encode ring on UVD-era parts; "jpeg" is VCN's JPEG
/// engine; "vpe" (video processing) exists from GFX 11.5. The kernel omits an engine line
/// whose counter is still zero.
const ENGINES: [&str; 8] = [
    "gfx", "compute", "dma", "dec", "enc", "enc_1", "jpeg", "vpe",
];
/// Index of "compute" in [`ENGINES`] — the one engine that decides `ProcessKind`.
const ENGINE_COMPUTE: usize = 1;

/// Per-engine cumulative busy-ns, indexed like [`ENGINES`].
type EngineNs = [Option<u64>; ENGINES.len()];

fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

/// The fdinfo keys this backend consumes. Anything missing (older kernel, non-DRM fd)
/// simply stays `None` — kernel gates: the fdinfo node exists from 5.14, but the
/// parseable `drm-*` dialect (and so `drm-pdev`) is 5.19+.
#[derive(Default)]
struct FdinfoDrm {
    pdev: Option<String>,
    /// The 5.14–5.18 dialect's bare `pdev:` key. Only used to RECOGNISE an attached client
    /// whose data this backend does not parse — never to attribute one.
    pre_5_19_pdev: Option<String>,
    /// `drm-memory-vram` — amdgpu's legacy, documented-deprecated alias of
    /// `drm-resident-vram`, always in KiB.
    vram_legacy_kib: Option<u64>,
    /// `drm-resident-vram` — the standardized key (`drm_print_memory_stats`), in bytes
    /// after unit scaling. Preferred over the legacy alias when both are present.
    vram_resident_bytes: Option<u64>,
    engine_ns: EngineNs,
}

impl FdinfoDrm {
    /// Max-merge across one pid's many fds on the same device: the fds describe the same
    /// client's buffers, so summing would double-count — max keeps the fullest view.
    fn merge_max(&mut self, other: FdinfoDrm) {
        self.vram_legacy_kib = max_opt(self.vram_legacy_kib, other.vram_legacy_kib);
        self.vram_resident_bytes = max_opt(self.vram_resident_bytes, other.vram_resident_bytes);
        for (a, b) in self.engine_ns.iter_mut().zip(other.engine_ns) {
            *a = max_opt(*a, b);
        }
    }

    /// VRAM held by the client: the standardized `drm-resident-vram` when the kernel
    /// prints it, else the legacy KiB alias. Only the VRAM region is read — GTT/CPU
    /// regions, `drm-total-*`, and `amd-requested-*` are different claims and are never
    /// substituted or summed in.
    fn vram_bytes(&self) -> Option<u64> {
        self.vram_resident_bytes
            .or_else(|| self.vram_legacy_kib.and_then(kib_to_bytes))
    }
}

/// Parse one fdinfo blob ("key:\tvalue" lines). Unit handling is per key and strict,
/// because guessing units is exactly the classic AMD-parsing bug this module's tests
/// exist to prevent: engine counters require "ns"; the legacy `drm-memory-vram` requires
/// "KiB"; the standardized `drm-resident-vram` follows [`parse_drm_size_bytes`].
fn parse_fdinfo(contents: &str) -> FdinfoDrm {
    let mut out = FdinfoDrm::default();
    for line in contents.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match key.trim() {
            "drm-pdev" => out.pdev = Some(val.to_ascii_lowercase()),
            "pdev" => out.pre_5_19_pdev = Some(val.to_ascii_lowercase()),
            "drm-memory-vram" => out.vram_legacy_kib = parse_suffixed(val, "KiB"),
            "drm-resident-vram" => out.vram_resident_bytes = parse_drm_size_bytes(val),
            key => {
                let Some(i) = key
                    .strip_prefix("drm-engine-")
                    .and_then(|name| ENGINES.iter().position(|e| *e == name))
                else {
                    continue;
                };
                // A repeated key ("dec" from both UVD and VCN rings) keeps the max, the
                // same no-double-count rule as the cross-fd merge.
                out.engine_ns[i] = max_opt(out.engine_ns[i], parse_suffixed(val, "ns"));
            }
        }
    }
    out
}

fn parse_suffixed(val: &str, unit: &str) -> Option<u64> {
    val.strip_suffix(unit)?.trim().parse().ok()
}

/// A standardized DRM memory-stat value (`drm_print_memory_stats` → `print_size` in
/// drm_file.c; drm-usage-stats.rst: "Default unit shall be bytes with optional unit
/// specifiers of 'KiB' or 'MiB'"). The kernel scales a value to KiB/MiB only when it is
/// exactly divisible, so all three spellings occur in the wild: "12345" (bytes),
/// "1536 KiB", "512 MiB". Any other suffix (GiB is not in the ABI) or a non-number is a
/// value we do not understand → `None`; overflow is `None`, never a wrapped number.
fn parse_drm_size_bytes(val: &str) -> Option<u64> {
    let (num, mult) = if let Some(n) = val.strip_suffix("MiB") {
        (n, 1024 * 1024)
    } else if let Some(n) = val.strip_suffix("KiB") {
        (n, 1024)
    } else {
        (val, 1)
    };
    let n: u64 = num.trim().parse().ok()?;
    n.checked_mul(mult)
}

/// KiB → bytes, overflow-checked: a count that exceeds u64 bytes cannot be real memory.
/// Unchecked `* 1024` would panic a debug build (taking down the scan for EVERY device,
/// since blobs parse before the pdev filter) and silently wrap to a fabricated number in
/// release — exactly the confidently-wrong output this product must never emit.
fn kib_to_bytes(kib: u64) -> Option<u64> {
    kib.checked_mul(1024)
}

/// fdinfo engine counters are cumulative busy-ns; utilization is the delta between two
/// sightings over wall time. No baseline (first sighting) or a counter that went
/// backwards (pid reuse re-created the client) → `None`, never a guess.
fn engine_util_pct(prev_ns: u64, prev_ts_ms: u64, cur_ns: u64, cur_ts_ms: u64) -> Option<f32> {
    let wall_ms = cur_ts_ms.checked_sub(prev_ts_ms)?;
    if wall_ms == 0 {
        return None;
    }
    let busy_ns = cur_ns.checked_sub(prev_ns)?;
    let pct = busy_ns as f64 / (wall_ms as f64 * 1_000_000.0) * 100.0;
    Some(pct.min(100.0) as f32)
}

/// Per-process util = the BUSIEST engine's busy-time fraction over the window (max, never
/// a sum: each engine is an independent ring, and summing would double-count and exceed
/// 100). Media engines count, so a VAAPI/AMF-only client (dec/enc/jpeg/vpe) gets a real
/// number instead of `None`. Only engines with a baseline on both sides contribute — an
/// engine line that first appears this tick is a baseline, not a guess. Any counter that
/// went backwards means the client was re-created (pid reuse): no claim at all. No
/// engine with two sightings (first sighting, or a client with no engine lines) → `None`,
/// never a fabricated 0%.
fn busiest_engine_util_pct(
    prev: &EngineNs,
    prev_ts_ms: u64,
    cur: &EngineNs,
    cur_ts_ms: u64,
) -> Option<f32> {
    let mut best: Option<f32> = None;
    for (p, c) in prev.iter().zip(cur) {
        let (Some(p), Some(c)) = (*p, *c) else {
            continue;
        };
        if c < p {
            return None;
        }
        let u = engine_util_pct(p, prev_ts_ms, c, cur_ts_ms)?;
        best = Some(best.map_or(u, |b| b.max(u)));
    }
    best
}

/// `/proc/self/status` says whether the fdinfo scan can see every user's processes:
/// euid 0, or CAP_SYS_PTRACE (bit 19) in the effective capability mask.
fn status_grants_full_proc_scan(status: &str) -> bool {
    const CAP_SYS_PTRACE: u32 = 19;
    for line in status.lines() {
        if let Some(uids) = line.strip_prefix("Uid:") {
            // Fields: real, effective, saved, fs — effective is what access checks use.
            if uids.split_whitespace().nth(1) == Some("0") {
                return true;
            }
        }
        if let Some(mask) = line.strip_prefix("CapEff:") {
            if let Ok(bits) = u64::from_str_radix(mask.trim(), 16) {
                if bits & (1 << CAP_SYS_PTRACE) != 0 {
                    return true;
                }
            }
        }
    }
    false
}

/// `StaticInfo::process_hint` for unprivileged runs: other users' fdinfo is unreadable
/// without root/CAP_SYS_PTRACE, so the process table is honestly incomplete — say so up
/// front instead of pretending it covers the machine. An unreadable status file reads as
/// unprivileged: overstating incompleteness is safe, understating it would be a lie.
fn fdinfo_process_hint(root: &Path) -> Option<String> {
    let full = fs::read_to_string(root.join("proc/self/status"))
        .is_ok_and(|s| status_grants_full_proc_scan(&s));
    (!full)
        .then(|| "showing your processes only — others need root or CAP_SYS_PTRACE (fdinfo)".into())
}

/// One `amdgpu.ids` line is `DEVICE_ID,\tREV_ID,\tname` (hex ids, no 0x); comment and
/// version lines have no commas and fall through.
fn amdgpu_ids_name(ids: &str, device: &str, revision: &str) -> Option<String> {
    for line in ids.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(3, ',');
        let (Some(dev), Some(rev), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if dev.trim().eq_ignore_ascii_case(device) && rev.trim().eq_ignore_ascii_case(revision) {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Marketing name from libdrm's `amdgpu.ids` (keyed by device id + revision id) when the
/// file ships on the system; otherwise a recognizable PCI-id fallback — never an error.
fn gpu_name(root: &Path, dev_path: &Path) -> String {
    let device = read_hex_id(&dev_path.join("device"));
    if let (Some(dev_id), Some(rev_id)) = (&device, read_hex_id(&dev_path.join("revision"))) {
        if let Ok(ids) = fs::read_to_string(root.join("usr/share/libdrm/amdgpu.ids")) {
            if let Some(name) = amdgpu_ids_name(&ids, dev_id, &rev_id) {
                return name;
            }
        }
    }
    match device {
        Some(id) => format!("AMD GPU [1002:{id}]"),
        None => "AMD GPU".into(),
    }
}

/// Process name from `{root}/proc/<pid>/comm` (kernel-truncated to 15 chars); a pid
/// placeholder when even that is unreadable.
fn comm_name(root: &Path, pid: u32) -> String {
    if let Ok(comm) = fs::read_to_string(root.join(format!("proc/{pid}/comm"))) {
        let comm = comm.trim();
        if !comm.is_empty() {
            return comm.to_string();
        }
    }
    format!("pid {pid}")
}

/// PCI address from the uevent's PCI_SLOT_NAME, lowercased — the same identity fdinfo's
/// `drm-pdev` carries and the registry dedupes on. An empty value is no identity at all
/// (`None`), so the card is skipped per `discover`'s contract rather than registered as
/// a ghost device with a blank id.
fn pci_slot_name(dev_path: &Path) -> Option<String> {
    fs::read_to_string(dev_path.join("uevent"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("PCI_SLOT_NAME="))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// First hwmon dir under the device. hwmon indices are not stable across boots, so it is
/// resolved through the device dir at init; absence (APUs, fixtures) is normal.
fn first_hwmon(dev_path: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dev_path.join("hwmon")).ok()?;
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort(); // read_dir order is arbitrary
    dirs.into_iter().next()
}

/// One enumerated amdgpu PCI device, resolved at init.
struct AmdDevice {
    id: DeviceId,
    /// `{root}/sys/class/drm/cardN/device` — the PCI dir all metric files hang off.
    dev_path: PathBuf,
    hwmon: Option<PathBuf>,
}

/// Enumerate `{root}/sys/class/drm/cardN/device` dirs whose vendor id is AMD (0x1002).
/// Connector nodes ("card1-DP-1") and render nodes are skipped; a card without a
/// PCI_SLOT_NAME has no stable identity and is skipped rather than guessed.
fn discover(root: &Path) -> Vec<AmdDevice> {
    let Ok(entries) = fs::read_dir(root.join("sys/class/drm")) else {
        return Vec::new();
    };
    let mut cards: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let idx: u32 = name.strip_prefix("card")?.parse().ok()?;
            Some((idx, e.path().join("device")))
        })
        .collect();
    cards.sort_by_key(|(idx, _)| *idx); // deterministic device order

    let mut devs = Vec::new();
    for (_, dev_path) in cards {
        if read_trim(&dev_path.join("vendor")).as_deref() != Some("0x1002") {
            continue;
        }
        let Some(pci) = pci_slot_name(&dev_path) else {
            continue;
        };
        let hwmon = first_hwmon(&dev_path);
        devs.push(AmdDevice {
            id: DeviceId(pci),
            dev_path,
            hwmon,
        });
    }
    devs
}

pub struct AmdBackend {
    root: PathBuf,
    devs: Vec<AmdDevice>,
    /// Per-(device, pid) fdinfo engine watermark: (wall ms, cumulative busy-ns per engine).
    last_engines: HashMap<(DeviceId, u32), (u64, EngineNs)>,
    /// Per-device v1_7/v1_8 residency watermark — the previous read the throttle delta
    /// is taken against. Cleared whenever the device stops yielding a residency block.
    last_residency: HashMap<DeviceId, ResidencyAcc>,
    /// Set once at init: explanation for a known-incomplete process list, if any.
    process_hint: Option<String>,
    /// Turns the kernel's cumulative per-PID CPU counter into a per-tick rate. The CPU%/
    /// container columns come from `/proc` (never the device root: a fixture tree's pids are
    /// not real processes), shared with the other Linux backends via `crate::proc_meta`.
    #[cfg(target_os = "linux")]
    cpu: crate::proc_meta::CpuTracker,
}

impl AmdBackend {
    /// Production entry point: the live sysfs/procfs under `/`.
    pub fn init() -> Result<Self, BackendError> {
        Self::with_root("/")
    }

    /// Fixture entry point: every path below derives from `root`, so tests run against
    /// committed trees (see `tests/fixtures/`).
    pub fn with_root(root: impl Into<PathBuf>) -> Result<Self, BackendError> {
        let root = root.into();
        let devs = discover(&root);
        if devs.is_empty() {
            return Err(BackendError::Unavailable(
                "no amdgpu devices under sys/class/drm".into(),
            ));
        }
        let process_hint = fdinfo_process_hint(&root);
        Ok(Self {
            root,
            devs,
            last_engines: HashMap::new(),
            last_residency: HashMap::new(),
            process_hint,
            #[cfg(target_os = "linux")]
            cpu: crate::proc_meta::CpuTracker::new(),
        })
    }

    fn device(&self, dev: &DeviceId) -> Result<&AmdDevice, BackendError> {
        self.devs
            .iter()
            .find(|d| &d.id == dev)
            .ok_or_else(|| BackendError::DeviceNotFound(dev.clone()))
    }

    /// Throttle observation from one `gpu_metrics` read. Status-word revisions decode
    /// per sample; v1_7/v1_8 residency revisions decode as a delta against the previous
    /// read of the same device (first read = baseline only → `None`). An absent node or a
    /// blob that is neither drops the residency baseline, so a stale watermark can never
    /// pair with a later read across a gap in observability.
    fn decode_throttle(&mut self, dev: &DeviceId, blob: Option<&[u8]>) -> Option<ThrottleReasons> {
        let Some(buf) = blob else {
            self.last_residency.remove(dev);
            return None;
        };
        if let Some(t) = decode_gpu_metrics_throttle(buf) {
            self.last_residency.remove(dev);
            return Some(t);
        }
        let Some(cur) = residency_snapshot(buf) else {
            self.last_residency.remove(dev);
            return None;
        };
        match self.last_residency.get(dev).copied() {
            // The firmware has not refreshed the table since our last read: no new
            // window, so no claim — and keep the older baseline so the next real window
            // is measured in full.
            Some(prev) if prev.accumulation_counter == cur.accumulation_counter => None,
            prev => {
                self.last_residency.insert(dev.clone(), cur);
                prev.and_then(|p| residency_delta(&p, &cur))
            }
        }
    }
}

impl GpuBackend for AmdBackend {
    fn name(&self) -> &'static str {
        "amd"
    }

    fn devices(&mut self) -> Vec<DeviceId> {
        self.devs.iter().map(|d| d.id.clone()).collect()
    }

    fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
        let d = self.device(dev)?;
        let p = &d.dev_path;

        Ok(StaticInfo {
            id: dev.clone(),
            vendor: Vendor::Amd,
            name: gpu_name(&self.root, p),
            backend: "amd".into(),
            mem_total_bytes: read_parse(&p.join("mem_info_vram_total")),
            power_limit_mw: d.hwmon.as_deref().and_then(power_cap_mw),
            max_sm_clock_mhz: fs::read_to_string(p.join("pp_dpm_sclk"))
                .ok()
                .as_deref()
                .and_then(dpm_max_mhz),
            // hwmon's temp1_crit is the shutdown-adjacent critical point, not the knee
            // where the SMU starts pulling clocks — claiming it as the slowdown threshold
            // would mis-narrate throttle events. Honest absence until the gpu_metrics
            // decoder provides the real limit.
            temp_slowdown_c: None,
            // amdgpu is an in-tree driver: there is no driver version distinct from the
            // kernel, and the uevent DRIVER= field is a name, not a version.
            driver_version: None,
            process_hint: self.process_hint.clone(),
            // sysfs numbers carry their plain meanings, with one exception: when the
            // temperature comes from a channel other than the edge-labeled one (MI300's
            // junction-only hwmon, an unlabeled temp1), the label says which sensor it is.
            source_caveat: d
                .hwmon
                .as_deref()
                .and_then(pick_temp)
                .and_then(|(sensor, _)| sensor.caveat())
                .map(String::from),
            // The PCI link files under the card describe the dGPU's internal switch port
            // (Navi/MI parts sit behind one), not the slot link — comparing against them
            // would be a lie. Honest absence.
            pcie_max_gen: None,
            pcie_max_width: None,
        })
    }

    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        let d = self.device(dev)?;
        let p = &d.dev_path;
        let hwmon = d.hwmon.as_deref();
        let metrics_blob = fs::read(p.join("gpu_metrics")).ok();

        let mut sample = DynamicSample {
            ts_ms: now_ms(),
            // SMU activity metric — duty-cycle-flavored like every vendor's "util".
            util_pct: read_parse(&p.join("gpu_busy_percent")),
            util_engine: None, // device-wide number, not an engine headline
            mem_used_bytes: read_parse(&p.join("mem_info_vram_used")),
            power_mw: hwmon.and_then(power_mw),
            temp_c: hwmon.and_then(temp_c),
            fan_pct: hwmon.and_then(fan_pct),
            sm_clock_mhz: fs::read_to_string(p.join("pp_dpm_sclk"))
                .ok()
                .as_deref()
                .and_then(dpm_current_mhz),
            mem_clock_mhz: fs::read_to_string(p.join("pp_dpm_mclk"))
                .ok()
                .as_deref()
                .and_then(dpm_current_mhz),
            // VCN (enc/dec) activity also lives in gpu_metrics, but its per-version offsets
            // and units are a separate job — absent until that decoder lands.
            encoder_pct: None,
            decoder_pct: None,
            // Throttle status lives in the versioned `gpu_metrics` binary node. An
            // absent/unreadable file (APUs/older kernels without the node) means the
            // source cannot observe throttling at all → `None` (§5.4), never an asserted
            // all-false. A present blob the decoder cannot trust (truncated, lying
            // structure_size, unknown future revision) is equally unobservable — the
            // decoder returns None for those, so Some here always means "decoded".
            // Filled below: residency revisions need the per-device watermark.
            throttle: None,
            // hwmon power1_cap, re-read per tick so a changed cap is narrated
            // (power_limit_changed) — the same file static_info reads once.
            power_limit_mw: hwmon.and_then(power_cap_mw),
            // See static_info: the card's link files describe its internal switch port.
            pcie_gen: None,
            pcie_width: None,
            // amdgpu exposes RAS ECC counts only on datacenter parts, through a separate
            // ras/ tree whose semantics are not decoded here yet — absent, never zero.
            ecc_uncorrected: None,
        };
        sample.throttle = self.decode_throttle(dev, metrics_blob.as_deref());
        Ok(sample)
    }

    fn refresh_processes(&mut self, dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError> {
        let pci = self.device(dev)?.id.0.clone();
        // One wall timestamp for the whole scan (one timestamp per frame, per CLAUDE.md).
        let ts = now_ms();

        // pid → max-merged fdinfo values across that pid's fds on this device.
        let mut by_pid: HashMap<u32, FdinfoDrm> = HashMap::new();
        let mut saw_pre_5_19_client = false;
        if let Ok(entries) = fs::read_dir(self.root.join("proc")) {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                // Other users' fdinfo is unreadable without root/CAP_SYS_PTRACE — skip
                // silently; the static-info hint already explains the incompleteness.
                let Ok(fds) = fs::read_dir(entry.path().join("fdinfo")) else {
                    continue;
                };
                for fd in fds.flatten() {
                    let Ok(contents) = fs::read_to_string(fd.path()) else {
                        continue;
                    };
                    let info = parse_fdinfo(&contents);
                    if info.pdev.is_none() && info.pre_5_19_pdev.as_deref() == Some(&pci) {
                        saw_pre_5_19_client = true;
                    }
                    if info.pdev.as_deref() != Some(pci.as_str()) {
                        continue;
                    }
                    by_pid.entry(pid).or_default().merge_max(info);
                }
            }
        }
        // A client of THIS device in the 5.14–5.18 fdinfo dialect ("pdev:", "vram mem:",
        // "gfx0: 37.5%") is attached, but that dialect is not parsed (the parse gate is
        // 5.19+). Returning the (empty) list would be a positive claim that nothing is
        // attached; the honest answer is that the process list is unknown.
        if saw_pre_5_19_client {
            return Err(BackendError::Unavailable(
                "amdgpu fdinfo uses the pre-5.19 dialect (kernel 5.14–5.18) — per-process data needs kernel 5.19+".into(),
            ));
        }

        let mut out: Vec<ProcessSample> = Vec::with_capacity(by_pid.len());
        for (&pid, agg) in &by_pid {
            // Engine-ns watermark: busy-ns delta over wall time = util%, busiest engine
            // wins. First sighting has no baseline → None.
            let prev = self
                .last_engines
                .insert((dev.clone(), pid), (ts, agg.engine_ns));
            let util_pct = prev
                .and_then(|(p_ts, p_ns)| busiest_engine_util_pct(&p_ns, p_ts, &agg.engine_ns, ts));
            out.push(ProcessSample {
                pid,
                name: comm_name(&self.root, pid),
                // A nonzero compute engine is decisive. (ROCm/KFD compute is known to
                // show ~0 engine-ns in fdinfo — the /sys/class/kfd cover comes later.)
                // Media engines (dec/enc/jpeg/vpe) are graphics-side work, as on Intel.
                kind: if agg.engine_ns[ENGINE_COMPUTE].unwrap_or(0) > 0 {
                    ProcessKind::Compute
                } else {
                    ProcessKind::Graphics
                },
                mem_bytes: agg.vram_bytes(),
                util_pct,
                cpu_pct: None,
                container: None,
                cmdline: None,
            });
        }
        // Drop watermarks for pids that vanished from this device (exited processes).
        self.last_engines
            .retain(|(d, pid), _| d != dev || by_pid.contains_key(pid));

        // CPU% and container identity come from /proc, mirroring the NVIDIA backend. The
        // CpuTracker holds per-PID state, so prune it to the PIDs we still see to keep it
        // from growing across a long session; container_of is stateless.
        #[cfg(target_os = "linux")]
        {
            for p in &mut out {
                p.cpu_pct = self.cpu.sample(p.pid);
                p.container = crate::proc_meta::container_of(p.pid);
                p.cmdline = crate::proc_meta::cmdline_of(p.pid);
            }
            let live: Vec<u32> = out.iter().map(|p| p.pid).collect();
            self.cpu.prune(&live);
        }

        out.sort_by_key(|p| p.pid); // deterministic order for the table and tests
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpm_table_parses_current_and_max_levels() {
        let table = "0: 500Mhz\n1: 1138Mhz *\n2: 2890Mhz\n";
        assert_eq!(dpm_current_mhz(table), Some(1138));
        assert_eq!(dpm_max_mhz(table), Some(2890));
        assert_eq!(dpm_current_mhz("garbage"), None);
        assert_eq!(dpm_max_mhz(""), None);
    }

    #[test]
    fn fdinfo_unit_suffixes_are_mandatory() {
        let blob = "drm-pdev:\t0000:03:00.0\ndrm-engine-gfx:\t123 ns\ndrm-memory-vram:\t456 KiB\n";
        let f = parse_fdinfo(blob);
        assert_eq!(f.pdev.as_deref(), Some("0000:03:00.0"));
        assert_eq!(f.engine_ns[0], Some(123)); // gfx
        assert_eq!(f.vram_legacy_kib, Some(456));
        assert_eq!(f.engine_ns[ENGINE_COMPUTE], None, "absent key stays None");
        // A value with the wrong/missing suffix is a key we do not understand.
        assert_eq!(parse_suffixed("456", "KiB"), None);
        assert_eq!(parse_suffixed("456 MiB", "KiB"), None);
        // The legacy key is KiB-only: a standardized-style spelling on it is not read.
        assert_eq!(
            parse_fdinfo("drm-memory-vram:\t512 MiB\n").vram_legacy_kib,
            None
        );
    }

    #[test]
    fn standardized_memory_values_scale_bytes_kib_mib() {
        // drm_file.c print_size: bare = bytes, " KiB", " MiB" — exactly three spellings.
        assert_eq!(parse_drm_size_bytes("12345"), Some(12_345));
        assert_eq!(parse_drm_size_bytes("1536 KiB"), Some(1_572_864));
        assert_eq!(parse_drm_size_bytes("512 MiB"), Some(536_870_912));
        // Not in the ABI, or not a number: a value we do not understand.
        assert_eq!(parse_drm_size_bytes("2 GiB"), None);
        assert_eq!(parse_drm_size_bytes("12 ns"), None);
        assert_eq!(parse_drm_size_bytes("MiB"), None);
        assert_eq!(parse_drm_size_bytes("-5 KiB"), None);
        assert_eq!(parse_drm_size_bytes(""), None);
        // Overflow is None, never a wrapped number.
        assert_eq!(parse_drm_size_bytes(&format!("{} MiB", u64::MAX)), None);
    }

    #[test]
    fn resident_vram_is_preferred_over_the_legacy_alias_and_other_regions_are_ignored() {
        let f = parse_fdinfo(
            "drm-memory-vram:\t4096 KiB\n\
             drm-total-vram:\t768 MiB\n\
             drm-resident-gtt:\t64 MiB\n\
             amd-requested-vram:\t900 MiB\n\
             drm-resident-vram:\t512 MiB\n",
        );
        assert_eq!(f.vram_bytes(), Some(536_870_912));
        // Legacy-only (pre-6.13 kernels) still reads through the KiB alias.
        let f = parse_fdinfo("drm-memory-vram:\t4096 KiB\n");
        assert_eq!(f.vram_bytes(), Some(4_194_304));
        // GTT-only client: no VRAM claim at all — never the GTT number.
        let f = parse_fdinfo("drm-memory-gtt:\t524288 KiB\ndrm-resident-gtt:\t512 MiB\n");
        assert_eq!(f.vram_bytes(), None);
    }

    #[test]
    fn media_engine_keys_are_parsed_and_repeated_keys_keep_the_max() {
        let f = parse_fdinfo(
            "drm-engine-dec:\t100 ns\n\
             drm-engine-dec:\t900 ns\n\
             drm-engine-enc:\t7 ns\n\
             drm-engine-enc_1:\t8 ns\n\
             drm-engine-jpeg:\t9 ns\n\
             drm-engine-vpe:\t10 ns\n\
             drm-engine-dma:\t11 ns\n\
             drm-engine-capacity-gfx:\t2\n\
             drm-engine-mystery:\t5 ns\n",
        );
        assert_eq!(
            f.engine_ns,
            [
                None,
                None,
                Some(11),
                Some(900),
                Some(7),
                Some(8),
                Some(9),
                Some(10)
            ]
        );
    }

    #[test]
    fn busiest_engine_util_is_a_max_never_a_sum() {
        let mut prev: EngineNs = [None; ENGINES.len()];
        let mut cur = prev;
        // gfx 30% and dec 60% over 1000 ms → 60 (the busiest), never 90.
        prev[0] = Some(0);
        cur[0] = Some(300_000_000);
        prev[3] = Some(0);
        cur[3] = Some(600_000_000);
        assert_eq!(busiest_engine_util_pct(&prev, 0, &cur, 1_000), Some(60.0));
        // An engine appearing for the first time is a baseline, not a contribution.
        cur[6] = Some(990_000_000);
        assert_eq!(busiest_engine_util_pct(&prev, 0, &cur, 1_000), Some(60.0));
        // Any counter running backwards = re-created client → no claim at all.
        prev[4] = Some(500);
        cur[4] = Some(100);
        assert_eq!(busiest_engine_util_pct(&prev, 0, &cur, 1_000), None);
        // No engine lines on either side → None, never 0%.
        let none: EngineNs = [None; ENGINES.len()];
        assert_eq!(busiest_engine_util_pct(&none, 0, &none, 1_000), None);
    }

    #[test]
    fn engine_util_needs_baseline_and_handles_resets() {
        // 500ms of busy-ns over 1000ms of wall = 50%.
        assert_eq!(engine_util_pct(0, 0, 500_000_000, 1_000), Some(50.0));
        // Counter went backwards (pid reuse re-created the client): no claim.
        assert_eq!(engine_util_pct(900, 0, 100, 1_000), None);
        // Zero wall delta cannot produce a rate.
        assert_eq!(engine_util_pct(0, 1_000, 100, 1_000), None);
        // More busy-ns than wall time (multi-queue accounting) clamps, never exceeds.
        assert_eq!(engine_util_pct(0, 0, 10_000_000_000, 1_000), Some(100.0));
    }

    #[test]
    fn hostile_fdinfo_vram_cannot_panic_or_wrap() {
        assert_eq!(kib_to_bytes(456), Some(466_944));
        // u64::MAX KiB cannot be a real byte count: None — never a debug panic, never
        // a release-mode wrap to a fabricated number.
        assert_eq!(kib_to_bytes(u64::MAX), None);
    }

    #[test]
    fn fan_pct_rejects_non_physical_readings() {
        assert_eq!(fan_pct_of_max(1650.0, 3300.0), Some(50.0));
        // Faster than fan1_max (worn sensor, boost) clamps, never exceeds.
        assert_eq!(fan_pct_of_max(4000.0, 3300.0), Some(100.0));
        // "nan"/"inf" parse as f32 but are not fan readings.
        assert_eq!(fan_pct_of_max(f32::NAN, f32::NAN), None);
        assert_eq!(fan_pct_of_max(f32::INFINITY, 3300.0), None);
        assert_eq!(fan_pct_of_max(1650.0, f32::NAN), None);
        // A negative RPM is a broken sensor, not a negative percentage.
        assert_eq!(fan_pct_of_max(-500.0, 3300.0), None);
        assert_eq!(fan_pct_of_max(1650.0, 0.0), None);
    }

    #[test]
    fn proc_status_privilege_detection() {
        // euid is the second Uid field — root euid grants the full scan.
        assert!(status_grants_full_proc_scan(
            "Uid:\t1000\t0\t1000\t1000\nCapEff:\t0000000000000000"
        ));
        // CAP_SYS_PTRACE (bit 19) alone suffices.
        assert!(status_grants_full_proc_scan(
            "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t0000000000080000"
        ));
        assert!(!status_grants_full_proc_scan(
            "Uid:\t1000\t1000\t1000\t1000\nCapEff:\t0000000000000000"
        ));
        assert!(!status_grants_full_proc_scan(""));
    }

    // ---- gpu_metrics throttle decoding -------------------------------------------------
    //
    // The blob builders below write every field at the SAME offset the decoder reads, and
    // deliberately plant a DECOY nonzero value in the field adjacent to each word we care
    // about. An off-by-N decoder would read the decoy and produce the wrong category, so
    // these tests fail loudly on an offset slip rather than silently passing.

    /// Build a naturally-aligned `gpu_metrics_v1_3` blob (size 120) with the given
    /// `indep_throttle_status`. Plants a thermal-bit decoy 8 bytes BEFORE indep (the voltage
    /// block) and a nonzero legacy throttle_status, so a correct decoder must read indep at
    /// exactly @112 and must prefer it over the legacy word.
    fn build_v1_3(indep: u64) -> Vec<u8> {
        let mut b = vec![0u8; 120];
        b[0..2].copy_from_slice(&120u16.to_le_bytes()); // structure_size
        b[2] = 1; // format_revision
        b[3] = 3; // content_revision
        b[68..72].copy_from_slice(&0x0000_0080u32.to_le_bytes()); // legacy decoy (nonzero)
        b[104..112].copy_from_slice(&(1u64 << 32).to_le_bytes()); // off-by-8 thermal decoy
        b[112..120].copy_from_slice(&indep.to_le_bytes()); // the real word
        b
    }

    /// Build a `gpu_metrics_v2_3` blob (size 152, APU) with the given indep word. indep lives
    /// at @120; the off-by-8 decoy goes at @112 (fan_pwm/padding region) and a legacy decoy
    /// at @108.
    fn build_v2_3(indep: u64) -> Vec<u8> {
        let mut b = vec![0u8; 152];
        b[0..2].copy_from_slice(&152u16.to_le_bytes());
        b[2] = 2;
        b[3] = 3;
        b[108..112].copy_from_slice(&0x0000_0080u32.to_le_bytes()); // legacy decoy
        b[112..120].copy_from_slice(&(1u64 << 32).to_le_bytes()); // off-by-8 thermal decoy
        b[120..128].copy_from_slice(&indep.to_le_bytes());
        b
    }

    /// Build a legacy-only `gpu_metrics_v2_1` blob (size 120, no indep word): throttle_status
    /// at @108, with a nonzero decoy in the adjacent fan_pwm/padding bytes.
    fn build_v2_1(throttle_status: u32) -> Vec<u8> {
        let mut b = vec![0u8; 120];
        b[0..2].copy_from_slice(&120u16.to_le_bytes());
        b[2] = 2;
        b[3] = 1;
        b[112..114].copy_from_slice(&0xBEEFu16.to_le_bytes()); // fan_pwm decoy (adjacent)
        b[108..112].copy_from_slice(&throttle_status.to_le_bytes());
        b
    }

    /// Build a `gpu_metrics_v2_2` blob (size 128): legacy@108, indep@120. Cyan Skillfish
    /// (BC-250 class) ships exactly this revision with `indep_throttle_status` never
    /// written by firmware — it reads as the SMU's 0xFF-memset sentinel — so the builder
    /// takes both words to let tests model that hardware.
    fn build_v2_2(indep: u64, legacy: u32) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0..2].copy_from_slice(&128u16.to_le_bytes());
        b[2] = 2;
        b[3] = 2;
        b[108..112].copy_from_slice(&legacy.to_le_bytes());
        b[112..114].copy_from_slice(&0xBEEFu16.to_le_bytes()); // fan_pwm decoy (adjacent)
        b[120..128].copy_from_slice(&indep.to_le_bytes());
        b
    }

    /// Build a `gpu_metrics_v2_4` blob at the kernel's real `sizeof` = **168**: 164 data
    /// bytes (v2.3's 152 + 6×u16 avg voltage/current @152) plus 4 tail-pad bytes from the
    /// struct's u64 alignment, with structure_size = sizeof (smu_cmn.h). Offsets are
    /// hardcoded literals cross-checked against the audit's compiled offsetof — never
    /// derived from `throttle_layout()`. Kernel-true decoys: the tail pad @164-167 is 0xFF
    /// (the SMU memset) and the voltage/current block is nonzero, so a decoder gating on
    /// 164 or misreading an offset fails loudly.
    fn build_v2_4(indep: u64, legacy: u32) -> Vec<u8> {
        let mut b = vec![0u8; 168];
        b[0..2].copy_from_slice(&168u16.to_le_bytes());
        b[2] = 2;
        b[3] = 4;
        b[108..112].copy_from_slice(&legacy.to_le_bytes()); // legacy throttle_status
        b[112..120].copy_from_slice(&(1u64 << 32).to_le_bytes()); // off-by-8 thermal decoy
        b[120..128].copy_from_slice(&indep.to_le_bytes()); // the real indep word
        b[152..164].copy_from_slice(&[0xAB; 12]); // avg voltage/current decoys (nonzero)
        b[164..168].copy_from_slice(&[0xFF; 4]); // 0xFF'd tail pad — kernel-true
        b
    }

    /// Build a `gpu_metrics_v3_0` blob (sizeof 264) with the named residency ACCUMULATORS
    /// set at the compiler-verified offsets 228..252 — a 2-byte pad follows
    /// `current_gfx_maxfreq` u16 @224. Kernel-true decoy: that pad @226-227 is 0xFF (the
    /// SMU memset), exactly what an off-by−2 decoder would misread as a 0xFFFF prochot.
    fn build_v3_0(prochot: u32, spl: u32, thm_gfx: u32) -> Vec<u8> {
        let mut b = vec![0u8; 264];
        b[0..2].copy_from_slice(&264u16.to_le_bytes());
        b[2] = 3;
        b[3] = 0;
        b[224..226].copy_from_slice(&2900u16.to_le_bytes()); // current_gfx_maxfreq decoy
        b[226..228].copy_from_slice(&[0xFF, 0xFF]); // 0xFF'd pad — the killer decoy
        b[228..232].copy_from_slice(&prochot.to_le_bytes()); // throttle_residency_prochot
        b[232..236].copy_from_slice(&spl.to_le_bytes()); // throttle_residency_spl
        b[248..252].copy_from_slice(&thm_gfx.to_le_bytes()); // throttle_residency_thm_gfx
        b
    }

    #[test]
    fn indep_thermal_bit_decodes_to_thermal_only() {
        // TEMP_HOTSPOT (bit 36) is purely thermal.
        let t = decode_gpu_metrics_throttle(&build_v1_3(1 << SMU_THROTTLER_TEMP_HOTSPOT_BIT))
            .expect("well-formed v1_3 must decode");
        assert!(t.thermal);
        assert!(!t.power_cap && !t.hw_slowdown && !t.sync_boost && !t.other);
    }

    #[test]
    fn indep_ppt_bit_decodes_to_power_cap_only() {
        // PPT0 (bit 0) is a package-power limit; the legacy/off-by-8 decoys must be ignored.
        let t = decode_gpu_metrics_throttle(&build_v1_3(1 << SMU_THROTTLER_PPT0_BIT))
            .expect("well-formed v1_3 must decode");
        assert!(t.power_cap);
        assert!(!t.thermal && !t.hw_slowdown && !t.sync_boost && !t.other);
    }

    #[test]
    fn indep_prochot_bit_decodes_to_hw_slowdown() {
        let t = decode_gpu_metrics_throttle(&build_v1_3(1 << SMU_THROTTLER_PROCHOT_GFX_BIT))
            .expect("well-formed v1_3 must decode");
        assert!(t.hw_slowdown);
        assert!(!t.thermal && !t.power_cap && !t.sync_boost && !t.other);
    }

    #[test]
    fn indep_combined_bits_decode_to_multiple_reasons() {
        // Thermal + power + prochot + an unknown future bit (bit 60) at once.
        let bits = (1 << SMU_THROTTLER_TEMP_MEM_BIT)
            | (1 << SMU_THROTTLER_TDC_GFX_BIT)
            | (1 << SMU_THROTTLER_PROCHOT_CPU_BIT)
            | (1u64 << 60);
        let t =
            decode_gpu_metrics_throttle(&build_v2_3(bits)).expect("well-formed v2_3 must decode");
        assert!(t.thermal && t.power_cap && t.hw_slowdown);
        // The unrecognised bit lands in `other` (tolerant decoding), not dropped.
        assert!(t.other);
        assert!(!t.sync_boost);
    }

    #[test]
    fn legacy_only_nonzero_is_coarse_other_never_a_guessed_cause() {
        // v2_1 has no indep word: a nonzero ASIC-specific throttle_status is real, but its
        // per-bit meaning is unknowable cross-ASIC, so it surfaces as `other` alone.
        let t = decode_gpu_metrics_throttle(&build_v2_1(0x0000_00FF))
            .expect("well-formed v2_1 must decode");
        assert!(t.other);
        assert!(!t.thermal && !t.power_cap && !t.hw_slowdown && !t.sync_boost);
        // Zero legacy status in a decoded struct is an OBSERVED quiet — Some(all-false).
        assert_eq!(
            decode_gpu_metrics_throttle(&build_v2_1(0)),
            Some(ThrottleReasons::default())
        );
    }

    #[test]
    fn v2_4_decodes_at_kernel_sizeof_168() {
        // SPPT_APU (bit 7) is an APU power limit — the Van Gogh / Steam Deck case. The
        // legacy decoy, off-by-8 thermal decoy, and 0xFF tail pad must all be ignored.
        let t = decode_gpu_metrics_throttle(&build_v2_4(1 << SMU_THROTTLER_SPPT_APU_BIT, 0x40))
            .expect("a real 168-byte v2_4 blob must decode");
        assert!(t.power_cap);
        assert!(!t.thermal && !t.hw_slowdown && !t.sync_boost && !t.other);
    }

    #[test]
    fn v2_4_blob_claiming_164_is_rejected() {
        // 164 was this decoder's old (wrong) gate: the kernel's sizeof is 168 because the
        // struct's u64 members add 4 tail-pad bytes and structure_size = sizeof
        // (smu_cmn.h). A blob declaring 164 is therefore NOT the struct we know —
        // exact-size honesty rejects it (None), never best-effort decodes it.
        let mut b = build_v2_4(1 << SMU_THROTTLER_SPPT_APU_BIT, 0);
        b.truncate(164);
        b[0..2].copy_from_slice(&164u16.to_le_bytes());
        assert_eq!(
            decode_gpu_metrics_throttle(&b),
            None,
            "a structure_size of 164 does not match v2_4's real sizeof"
        );
    }

    #[test]
    fn indep_all_ff_sentinel_falls_through_to_legacy() {
        // Cyan Skillfish: v2_2 with indep_throttle_status never written → 0xFF…FF from
        // the SMU memset. That means "word unsupported", NOT 64 simultaneous throttlers.
        // With a quiet legacy word the decode is an OBSERVED quiet — Some(all-false) —
        // never a permanent all-reasons-throttling narration.
        assert_eq!(
            decode_gpu_metrics_throttle(&build_v2_2(u64::MAX, 0)),
            Some(ThrottleReasons::default())
        );
        // A nonzero legacy word behind the sentinel still surfaces as coarse `other`.
        let t = decode_gpu_metrics_throttle(&build_v2_2(u64::MAX, 0x2))
            .expect("legacy word must still decode behind an indep sentinel");
        assert!(t.other);
        assert!(!t.thermal && !t.power_cap && !t.hw_slowdown && !t.sync_boost);
        // The dGPU path falls through the same way: v1_3 with sentinel indep reads the
        // legacy word at @68 (nonzero in the builder) → coarse `other`.
        let t = decode_gpu_metrics_throttle(&build_v1_3(u64::MAX))
            .expect("v1_3 legacy word must decode behind an indep sentinel");
        assert!(t.other);
        assert!(!t.thermal && !t.power_cap && !t.hw_slowdown && !t.sync_boost);
    }

    #[test]
    fn all_words_sentinel_decodes_to_none() {
        // Both words carry the 0xFF memset sentinel: this firmware exposes no throttle
        // observation at all — unobservable (None), never Some(anything).
        assert_eq!(
            decode_gpu_metrics_throttle(&build_v2_2(u64::MAX, u32::MAX)),
            None
        );
    }

    #[test]
    fn legacy_only_sentinel_decodes_to_none() {
        // v2_1 has no indep word; a 0xFF'd throttle_status means "unsupported", not
        // "every ASIC-specific throttler active".
        assert_eq!(decode_gpu_metrics_throttle(&build_v2_1(u32::MAX)), None);
    }

    #[test]
    fn v3_residency_accumulators_never_assert_per_sample_throttle() {
        // v3_0 carries only firmware accumulator counters (no status word): nonzero means
        // "fired at some point in the firmware's window", which a single sample cannot
        // turn into a live state. The per-sample decode is None (unobservable) — a
        // historic PROCHOT must never narrate as a current hw_slowdown.
        assert_eq!(decode_gpu_metrics_throttle(&build_v3_0(37, 9, 12)), None);
        // All-zero accumulators are equally non-instantaneous — still None, not an
        // asserted "observed quiet" fabricated from counters we cannot interpret
        // per-sample. (The 0xFF'd pad @226-227 must never leak into any decode either.)
        assert_eq!(decode_gpu_metrics_throttle(&build_v3_0(0, 0, 0)), None);
        // Counters that are themselves the 0xFF memset sentinel change nothing: None.
        assert_eq!(
            decode_gpu_metrics_throttle(&build_v3_0(u32::MAX, u32::MAX, u32::MAX)),
            None
        );
    }

    #[test]
    fn unknown_revision_decodes_to_none() {
        // A well-formed header with a future (format=9, content=9) revision: no offsets we
        // trust → None, never a guess at byte positions and never a fabricated "observed
        // quiet" (an asserted all-false here would be a permanent fact-grade lie on every
        // kernel newer than the layout table — §5.4).
        let mut b = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        b[2] = 9;
        b[3] = 9;
        assert_eq!(
            decode_gpu_metrics_throttle(&b),
            None,
            "unknown revision must be unobservable, not quiet"
        );
    }

    #[test]
    fn truncated_blob_decodes_to_none_without_panic() {
        // The corrupt/short-sysfs honesty case: every prefix length must yield None,
        // never a panic, never a half-read word, never an asserted all-false.
        let full = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        for len in 0..full.len() {
            assert_eq!(
                decode_gpu_metrics_throttle(&full[..len]),
                None,
                "a {len}-byte prefix of a v1_3 blob must decode to None"
            );
        }
        // An empty buffer is the extreme of the same case.
        assert_eq!(decode_gpu_metrics_throttle(&[]), None);
    }

    #[test]
    fn lying_structure_size_decodes_to_none() {
        // structure_size claims a smaller struct than the version's real length: we are not
        // looking at the struct we think we are, so decode nothing even though a PPT bit is
        // physically present at @112.
        let mut b = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        b[0..2].copy_from_slice(&100u16.to_le_bytes()); // real v1_3 is 120
        assert_eq!(
            decode_gpu_metrics_throttle(&b),
            None,
            "a structure_size that disagrees with the version size is not trusted"
        );
        // The mirror case: structure_size larger than the buffer (claims bytes we lack).
        let mut b = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        b[0..2].copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(decode_gpu_metrics_throttle(&b), None);
    }

    fn residency(acc: u32, prochot: u32, ppt: u32, sock: u32, vr: u32, hbm: u32) -> ResidencyAcc {
        ResidencyAcc {
            accumulation_counter: acc,
            prochot,
            ppt,
            socket_thm: sock,
            vr_thm: vr,
            hbm_thm: hbm,
        }
    }

    #[test]
    fn residency_delta_maps_each_counter_to_its_column() {
        let base = residency(1000, 5, 5, 5, 5, 5);
        // Nothing advanced over a 100-cycle window: an OBSERVED quiet.
        assert_eq!(
            residency_delta(&base, &residency(1100, 5, 5, 5, 5, 5)),
            Some(ThrottleReasons::default())
        );
        // PROCHOT advanced → hw_slowdown only.
        assert_eq!(
            residency_delta(&base, &residency(1100, 6, 5, 5, 5, 5)),
            Some(ThrottleReasons {
                hw_slowdown: true,
                ..Default::default()
            })
        );
        // PPT advanced → power_cap only.
        assert_eq!(
            residency_delta(&base, &residency(1100, 5, 6, 5, 5, 5)),
            Some(ThrottleReasons {
                power_cap: true,
                ..Default::default()
            })
        );
        // Socket / VR / HBM thermal each → thermal only.
        for thermal in [
            residency(1100, 5, 5, 6, 5, 5),
            residency(1100, 5, 5, 5, 6, 5),
            residency(1100, 5, 5, 5, 5, 6),
        ] {
            assert_eq!(
                residency_delta(&base, &thermal),
                Some(ThrottleReasons {
                    thermal: true,
                    ..Default::default()
                })
            );
        }
    }

    #[test]
    fn residency_delta_refuses_dishonest_windows() {
        let base = residency(1000, 5, 5, 5, 5, 5);
        // No new firmware window.
        assert_eq!(residency_delta(&base, &base), None);
        // Accumulation counter went backwards (driver reload / wrap).
        assert_eq!(residency_delta(&base, &residency(10, 0, 0, 0, 0, 0)), None);
        // A residency counter went backwards while the window advanced.
        assert_eq!(
            residency_delta(&base, &residency(1100, 4, 5, 5, 5, 5)),
            None
        );
        // A residency advanced by more than the window itself: garbage, not throttling.
        assert_eq!(
            residency_delta(&base, &residency(1100, 500, 5, 5, 5, 5)),
            None
        );
    }

    #[test]
    fn amdgpu_ids_lookup_is_keyed_by_device_and_revision() {
        let ids = "# header\n1.0.0\n744C,\tC8,\tAMD Radeon RX 7900 XTX\n744C,\tCC,\tAMD Radeon RX 7900 XT\n";
        assert_eq!(
            amdgpu_ids_name(ids, "744c", "c8").as_deref(),
            Some("AMD Radeon RX 7900 XTX")
        );
        assert_eq!(
            amdgpu_ids_name(ids, "744c", "cc").as_deref(),
            Some("AMD Radeon RX 7900 XT")
        );
        assert_eq!(amdgpu_ids_name(ids, "744c", "ff"), None);
    }
}
