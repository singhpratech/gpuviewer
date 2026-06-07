//! AMD Linux backend — hand-rolled `std::fs` readers over sysfs/hwmon/fdinfo. No library
//! linkage at all: librocm_smi64 is explicitly off the table (soname churn broke btop
//! twice — btop #774) and libdrm ioctls are unnecessary for everything v1 needs.
//!
//! Per the domain rules in CLAUDE.md:
//! - Every path derives from a root-dir parameter (`with_root`), so the whole backend runs
//!   against committed fixture trees; `init()` is just `with_root("/")`.
//! - A missing file or unparsable value is `None`, never a failure — an APU without hwmon
//!   or `pp_dpm_*` tables is a normal device, not a broken one (Intel-iGPU-style absence).
//! - Throttle bits live in the `gpu_metrics` binary struct, which is versioned (v1.0–v3.0)
//!   with per-version field offsets AND units, and therefore needs per-version decoders
//!   backed by fixtures. `decode_gpu_metrics_throttle` parses it directly off the sysfs
//!   blob (offsets derived from the in-tree kernel header — see that function). A blob that
//!   is absent, truncated, of an unknown revision, or whose self-declared `structure_size`
//!   is inconsistent decodes to `ThrottleReasons::default()`: no signal is honest, a
//!   misread byte narrated as a throttle cause is not.
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

/// hwmon temps are MILLI-degrees C. Prefer the sensor labeled "edge" (the die-edge value
/// every vendor tool headlines); junction/mem run hotter and would overstate. Fall back
/// to `temp1_input` when labels are absent.
fn edge_temp_c(hwmon: &Path) -> Option<f32> {
    let millic = edge_temp_millic(hwmon)?;
    Some(millic as f32 / 1000.0)
}

fn edge_temp_millic(hwmon: &Path) -> Option<i64> {
    if let Ok(entries) = fs::read_dir(hwmon) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix("_label")) else {
                continue;
            };
            if !stem.starts_with("temp") {
                continue;
            }
            if read_trim(&entry.path()).as_deref() == Some("edge") {
                if let Some(millic) = read_parse(&hwmon.join(format!("{stem}_input"))) {
                    return Some(millic);
                }
            }
        }
    }
    read_parse(&hwmon.join("temp1_input"))
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
    /// "something is throttling" when no `indep` word exists.
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
        // gpu_metrics_v1_x (dGPU). Shared prefix, naturally aligned with no padding:
        //   header(4) + 10×u16 temps/activity/power(20)=24 → energy_accumulator u64 @24,
        //   system_clock_counter u64 @32, then 14×u16 avg+current clocks(28) @40 →
        //   throttle_status u32 @68. v1.0 reorders the prefix; we map only v1.1+.
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
            // v2.3 up to @152, then average cpu/soc/gfx voltage+current (6 u16)@152 → 164.
            indep: Some(120),
            legacy: Some(108),
            size: 164,
        }),
        // gpu_metrics_v3_0 (newer APU) drops the status bitfields for per-cause RESIDENCY
        // counters; those are decoded separately by offset+name, not as a bitmask.
        _ => None,
    }
}

/// v3_0 throttle-residency counters (kgd_pp_interface.h `gpu_metrics_v3_0`). Each is a u32
/// accumulator: nonzero means that throttler was active during the firmware's window. A
/// counter is a fact, so these map to causes by name. Offsets walked under natural
/// alignment (the struct's two interior pads — before `system_clock_counter` and before
/// `average_apu_power` — are already accounted for upstream of these fields).
struct V3ResidencyLayout {
    prochot: usize,
    spl: usize,
    fppt: usize,
    sppt: usize,
    thm_core: usize,
    thm_gfx: usize,
    thm_soc: usize,
    size: usize,
}

/// Resolve the v3_0 residency block, or `None` for any other revision.
fn v3_residency_layout(format: u8, content: u8) -> Option<V3ResidencyLayout> {
    if (format, content) != (3, 0) {
        return None;
    }
    // Walk to current_gfx_maxfreq u16 @224 (the field just before the residency block):
    //   header(4) temps gfx/soc(4)@4 core[16](32)@8 skin(2)@40 gfx_act/vcn(4)@42
    //   ipu_act[8](16)@46 core_c0[16](32)@62 dram/ipu rd/wr(8)@94 → @102
    //   pad(2)@102 system_clock_counter u64 @104 → @112
    //   socket_power u32 @112 ipu_power u16 @116 pad(2)@118
    //   apu/gfx/dgpu/all_core power(4×u32)@120 → @136
    //   core_power[16](32)@136 sys_power/stapm/cur_stapm(6)@168 → @174
    //   8 avg clocks(16)@174 → @190 coreclk[16](32)@190 → @222
    //   core_maxfreq u16 @222, gfx_maxfreq u16 @224 → residency block @226.
    Some(V3ResidencyLayout {
        prochot: 226,
        spl: 230,
        fppt: 234,
        sppt: 238,
        thm_core: 242,
        thm_gfx: 246,
        thm_soc: 250,
        // time_filter_alphavalue u32 @254 → 258; struct aligns to u64 → 264.
        size: 264,
    })
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
/// yield `ThrottleReasons::default()`. We never wrap, never panic, and never narrate a byte
/// we are not certain of.
///
/// Mapping precedence: prefer `indep_throttle_status` (ASIC-independent bits) and split it
/// into thermal / power_cap / hw_slowdown / other. v3_0 has no status word — its per-cause
/// residency counters are used instead. For older revisions that expose only the legacy
/// ASIC-specific `throttle_status`, the per-bit meaning varies by ASIC and is not safe to
/// map; a nonzero value reliably means "some throttler is active", so it surfaces as `other`
/// alone rather than as a fabricated specific cause.
pub fn decode_gpu_metrics_throttle(buf: &[u8]) -> ThrottleReasons {
    // Common header: structure_size u16 @0, format_revision u8 @2, content_revision u8 @3.
    if buf.len() < HEADER_LEN {
        return ThrottleReasons::default();
    }
    let structure_size = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    let format = buf[2];
    let content = buf[3];

    // The blob must hold at least as many bytes as it claims, and the claim must be a real
    // header (a zeroed/garbage node reads structure_size 0). Both guards reject truncation.
    if structure_size < HEADER_LEN || buf.len() < structure_size {
        return ThrottleReasons::default();
    }

    if let Some(layout) = throttle_layout(format, content) {
        // structure_size must match the version's known size exactly — a mismatch means we
        // are not looking at the struct we think we are, so we decode nothing.
        if structure_size != layout.size {
            return ThrottleReasons::default();
        }
        if let Some(off) = layout.indep {
            if let Some(bits) = read_u64_le(buf, off) {
                return map_indep_throttle(bits);
            }
            return ThrottleReasons::default();
        }
        if let Some(off) = layout.legacy {
            return match read_u32_le(buf, off) {
                // Legacy throttle_status is ASIC-specific: nonzero = throttling, cause
                // unmappable. Honest coarse signal beats both silence and a guessed column.
                Some(v) if v != 0 => ThrottleReasons {
                    other: true,
                    ..Default::default()
                },
                _ => ThrottleReasons::default(),
            };
        }
        return ThrottleReasons::default();
    }

    if let Some(r) = v3_residency_layout(format, content) {
        if structure_size != r.size {
            return ThrottleReasons::default();
        }
        let any = |off: usize| read_u32_le(buf, off).is_some_and(|v| v != 0);
        return ThrottleReasons {
            thermal: any(r.thm_core) || any(r.thm_gfx) || any(r.thm_soc),
            // SPL/FPPT/SPPT are the APU power limits.
            power_cap: any(r.spl) || any(r.fppt) || any(r.sppt),
            hw_slowdown: any(r.prochot),
            sync_boost: false,
            other: false,
        };
    }

    // Unknown future revision: decode nothing rather than guess offsets.
    ThrottleReasons::default()
}

/// The fdinfo keys this backend consumes. Anything missing (older kernel, non-DRM fd)
/// simply stays `None` — kernel gates: engine busy-ns 5.14+, standardized keys 5.19+.
#[derive(Default)]
struct FdinfoDrm {
    pdev: Option<String>,
    vram_kib: Option<u64>,
    gfx_ns: Option<u64>,
    compute_ns: Option<u64>,
}

impl FdinfoDrm {
    /// Max-merge across one pid's many fds on the same device: the fds describe the same
    /// client's buffers, so summing would double-count — max keeps the fullest view.
    fn merge_max(&mut self, other: FdinfoDrm) {
        fn mx(a: &mut Option<u64>, b: Option<u64>) {
            *a = match (*a, b) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (x, y) => x.or(y),
            };
        }
        mx(&mut self.vram_kib, other.vram_kib);
        mx(&mut self.gfx_ns, other.gfx_ns);
        mx(&mut self.compute_ns, other.compute_ns);
    }
}

/// Parse one fdinfo blob ("key:\tvalue" lines). Unit suffixes are part of the fdinfo ABI
/// ("<n> ns", "<n> KiB") and are required — guessing units is exactly the classic
/// AMD-parsing bug this module's tests exist to prevent.
fn parse_fdinfo(contents: &str) -> FdinfoDrm {
    let mut out = FdinfoDrm::default();
    for line in contents.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match key.trim() {
            "drm-pdev" => out.pdev = Some(val.to_ascii_lowercase()),
            "drm-memory-vram" => out.vram_kib = parse_suffixed(val, "KiB"),
            "drm-engine-gfx" => out.gfx_ns = parse_suffixed(val, "ns"),
            "drm-engine-compute" => out.compute_ns = parse_suffixed(val, "ns"),
            _ => {}
        }
    }
    out
}

fn parse_suffixed(val: &str, unit: &str) -> Option<u64> {
    val.strip_suffix(unit)?.trim().parse().ok()
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
    /// Per-(device, pid) fdinfo gfx-engine watermark: (cumulative busy-ns, wall ms).
    last_gfx: HashMap<(DeviceId, u32), (u64, u64)>,
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
            last_gfx: HashMap::new(),
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
        })
    }

    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        let d = self.device(dev)?;
        let p = &d.dev_path;
        let hwmon = d.hwmon.as_deref();

        Ok(DynamicSample {
            ts_ms: now_ms(),
            // SMU activity metric — duty-cycle-flavored like every vendor's "util".
            util_pct: read_parse(&p.join("gpu_busy_percent")),
            mem_used_bytes: read_parse(&p.join("mem_info_vram_used")),
            power_mw: hwmon.and_then(power_mw),
            temp_c: hwmon.and_then(edge_temp_c),
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
            // Throttle status lives in the versioned `gpu_metrics` binary node. An absent
            // file (APUs/older kernels without the node) or anything the decoder cannot
            // trust reads back as the default "no throttle signal" — honest, never faked.
            throttle: fs::read(p.join("gpu_metrics"))
                .map(|buf| decode_gpu_metrics_throttle(&buf))
                .unwrap_or_default(),
        })
    }

    fn refresh_processes(&mut self, dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError> {
        let pci = self.device(dev)?.id.0.clone();
        // One wall timestamp for the whole scan (one timestamp per frame, per CLAUDE.md).
        let ts = now_ms();

        // pid → max-merged fdinfo values across that pid's fds on this device.
        let mut by_pid: HashMap<u32, FdinfoDrm> = HashMap::new();
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
                    if info.pdev.as_deref() != Some(pci.as_str()) {
                        continue;
                    }
                    by_pid.entry(pid).or_default().merge_max(info);
                }
            }
        }

        let mut out: Vec<ProcessSample> = Vec::with_capacity(by_pid.len());
        for (&pid, agg) in &by_pid {
            // Engine-ns watermark: busy-ns delta over wall time = util%. First sighting
            // has no baseline → None.
            let util_pct = agg.gfx_ns.and_then(|cur| {
                let prev = self.last_gfx.insert((dev.clone(), pid), (cur, ts));
                prev.and_then(|(p_ns, p_ts)| engine_util_pct(p_ns, p_ts, cur, ts))
            });
            out.push(ProcessSample {
                pid,
                name: comm_name(&self.root, pid),
                // A nonzero compute engine is decisive. (ROCm/KFD compute is known to
                // show ~0 engine-ns in fdinfo — the /sys/class/kfd cover comes later.)
                kind: if agg.compute_ns.unwrap_or(0) > 0 {
                    ProcessKind::Compute
                } else {
                    ProcessKind::Graphics
                },
                mem_bytes: agg.vram_kib.and_then(kib_to_bytes),
                util_pct,
                cpu_pct: None,
                container: None,
            });
        }
        // Drop watermarks for pids that vanished from this device (exited processes).
        self.last_gfx
            .retain(|(d, pid), _| d != dev || by_pid.contains_key(pid));

        // CPU% and container identity come from /proc, mirroring the NVIDIA backend. The
        // CpuTracker holds per-PID state, so prune it to the PIDs we still see to keep it
        // from growing across a long session; container_of is stateless.
        #[cfg(target_os = "linux")]
        {
            for p in &mut out {
                p.cpu_pct = self.cpu.sample(p.pid);
                p.container = crate::proc_meta::container_of(p.pid);
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
        assert_eq!(f.gfx_ns, Some(123));
        assert_eq!(f.vram_kib, Some(456));
        assert_eq!(f.compute_ns, None, "absent key stays None");
        // A value with the wrong/missing suffix is a key we do not understand.
        assert_eq!(parse_suffixed("456", "KiB"), None);
        assert_eq!(parse_suffixed("456 MiB", "KiB"), None);
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

    /// Build a `gpu_metrics_v3_0` blob (size 264) with the named residency counters set.
    fn build_v3_0(prochot: u32, spl: u32, thm_gfx: u32) -> Vec<u8> {
        let mut b = vec![0u8; 264];
        b[0..2].copy_from_slice(&264u16.to_le_bytes());
        b[2] = 3;
        b[3] = 0;
        // decoy in current_gfx_maxfreq (@224), just before the residency block:
        b[224..226].copy_from_slice(&0xBEEFu16.to_le_bytes());
        b[226..230].copy_from_slice(&prochot.to_le_bytes());
        b[230..234].copy_from_slice(&spl.to_le_bytes());
        b[242..246].copy_from_slice(&thm_gfx.to_le_bytes()); // throttle_residency_thm_gfx
        b
    }

    #[test]
    fn indep_thermal_bit_decodes_to_thermal_only() {
        // TEMP_HOTSPOT (bit 36) is purely thermal.
        let t = decode_gpu_metrics_throttle(&build_v1_3(1 << SMU_THROTTLER_TEMP_HOTSPOT_BIT));
        assert!(t.thermal);
        assert!(!t.power_cap && !t.hw_slowdown && !t.sync_boost && !t.other);
    }

    #[test]
    fn indep_ppt_bit_decodes_to_power_cap_only() {
        // PPT0 (bit 0) is a package-power limit; the legacy/off-by-8 decoys must be ignored.
        let t = decode_gpu_metrics_throttle(&build_v1_3(1 << SMU_THROTTLER_PPT0_BIT));
        assert!(t.power_cap);
        assert!(!t.thermal && !t.hw_slowdown && !t.sync_boost && !t.other);
    }

    #[test]
    fn indep_prochot_bit_decodes_to_hw_slowdown() {
        let t = decode_gpu_metrics_throttle(&build_v1_3(1 << SMU_THROTTLER_PROCHOT_GFX_BIT));
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
        let t = decode_gpu_metrics_throttle(&build_v2_3(bits));
        assert!(t.thermal && t.power_cap && t.hw_slowdown);
        // The unrecognised bit lands in `other` (tolerant decoding), not dropped.
        assert!(t.other);
        assert!(!t.sync_boost);
    }

    #[test]
    fn legacy_only_nonzero_is_coarse_other_never_a_guessed_cause() {
        // v2_1 has no indep word: a nonzero ASIC-specific throttle_status is real, but its
        // per-bit meaning is unknowable cross-ASIC, so it surfaces as `other` alone.
        let t = decode_gpu_metrics_throttle(&build_v2_1(0x0000_00FF));
        assert!(t.other);
        assert!(!t.thermal && !t.power_cap && !t.hw_slowdown && !t.sync_boost);
        // Zero legacy status = not throttling.
        assert_eq!(
            decode_gpu_metrics_throttle(&build_v2_1(0)),
            ThrottleReasons::default()
        );
    }

    #[test]
    fn v3_residency_counters_map_by_name() {
        // prochot residency → hw_slowdown, spl (power limit) → power_cap.
        let t = decode_gpu_metrics_throttle(&build_v3_0(5, 9, 0));
        assert!(t.hw_slowdown && t.power_cap);
        assert!(!t.thermal && !t.other);
        // thm_gfx residency → thermal.
        let t = decode_gpu_metrics_throttle(&build_v3_0(0, 0, 3));
        assert!(t.thermal);
        assert!(!t.hw_slowdown && !t.power_cap);
        // All-zero residency = not throttling.
        assert_eq!(
            decode_gpu_metrics_throttle(&build_v3_0(0, 0, 0)),
            ThrottleReasons::default()
        );
    }

    #[test]
    fn unknown_revision_decodes_to_default() {
        // A well-formed header with a future (format=9, content=9) revision: no offsets we
        // trust → default, never a guess at byte positions.
        let mut b = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        b[2] = 9;
        b[3] = 9;
        assert_eq!(
            decode_gpu_metrics_throttle(&b),
            ThrottleReasons::default(),
            "unknown revision must decode nothing"
        );
    }

    #[test]
    fn truncated_blob_decodes_to_default_without_panic() {
        // The corrupt/short-sysfs honesty case: every prefix length must yield default,
        // never a panic and never a half-read word.
        let full = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        for len in 0..full.len() {
            assert_eq!(
                decode_gpu_metrics_throttle(&full[..len]),
                ThrottleReasons::default(),
                "a {len}-byte prefix of a v1_3 blob must decode to default"
            );
        }
        // An empty buffer is the extreme of the same case.
        assert_eq!(decode_gpu_metrics_throttle(&[]), ThrottleReasons::default());
    }

    #[test]
    fn lying_structure_size_decodes_to_default() {
        // structure_size claims a smaller struct than the version's real length: we are not
        // looking at the struct we think we are, so decode nothing even though a PPT bit is
        // physically present at @112.
        let mut b = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        b[0..2].copy_from_slice(&100u16.to_le_bytes()); // real v1_3 is 120
        assert_eq!(
            decode_gpu_metrics_throttle(&b),
            ThrottleReasons::default(),
            "a structure_size that disagrees with the version size is not trusted"
        );
        // The mirror case: structure_size larger than the buffer (claims bytes we lack).
        let mut b = build_v1_3(1 << SMU_THROTTLER_PPT0_BIT);
        b[0..2].copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(decode_gpu_metrics_throttle(&b), ThrottleReasons::default());
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
