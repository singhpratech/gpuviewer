//! Intel Linux backend — hand-rolled `std::fs` readers over sysfs/fdinfo, no library
//! linkage (no Level Zero, no IGCL: per docs/research/02 their Linux coverage is gappy
//! and root-gated; plain fdinfo+sysfs is the strong path).
//!
//! The defining caveat: **i915 and xe are two different worlds** (research 02). Same
//! vendor, two in-tree drivers, different fdinfo keys AND different sysfs freq layouts —
//! every read below is dispatched on a per-device [`Dialect`] detected from the uevent:
//! - i915 (Gen9 → Meteor Lake, DG2 default): fdinfo `drm-engine-*` cumulative busy-ns
//!   (kernel 5.19+), per-process memory regions named `local0`/`system0` (6.8+),
//!   card-level `gt_*_freq_mhz` files, dGPU-only `lmem_total_bytes`, hwmon package
//!   temp on `temp1_input` (6.12+).
//! - xe (Lunar Lake, Battlemage+): fdinfo `drm-cycles-*` / `drm-total-cycles-*` GT-clock
//!   counters (6.11+), memory regions named `vram0`/`system`/`gtt` (6.8+),
//!   `device/tile0/gt0/freq0/*` freq files, NO VRAM-total sysfs at all, hwmon package
//!   temp on `temp2_input` — there is NO temp1, and temp3 is the VRAM sensor (6.15+);
//!   hwmon power channels by scope, index 1 = card / index 2 = pkg (card preferred, pkg
//!   fallback — see [`resolve_power_channel`]).
//!
//! Per the domain rules in CLAUDE.md:
//! - Every path derives from a root-dir parameter (`with_root`), so the whole backend
//!   runs against committed fixture trees; `init()` is just `with_root("/")`.
//! - A missing file or unparsable value is `None`, never a failure — an iGPU has NO
//!   hwmon at all, so temp/power/fan absence is the NORMAL case, not a broken device.
//!   hwmon itself is effectively dGPU-only and recent-kernel-gated (i915 fan/temp
//!   6.12+; xe temps 6.15+, fans 6.16+).
//! - Device-level utilization is deliberately `None`: the i915/xe perf PMU needs
//!   root/CAP_PERFMON (intel_gpu_top's infamous "Failed to initialize PMU"; the xe PMU
//!   only exists since 6.15), and summing fdinfo across clients we may not be able to
//!   see would understate — an invented number is worse than an honest absence.
//! - The xe per-process utilization math is NOT the i915 math: Δbusy-cycles over
//!   Δtotal-cycles, never over wall time — see [`cycles_util_pct`].
//! - Other users' fdinfo needs root/CAP_SYS_PTRACE, so unprivileged runs carry an
//!   honest "your processes only" `process_hint` (same model as the AMD backend).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::{BackendError, GpuBackend};
use crate::model::{
    now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo, ThrottleReasons,
    Vendor,
};

/// Which in-tree Intel KMS driver owns a device, from the uevent's DRIVER= field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    I915,
    Xe,
}

/// Read a sysfs file and parse its trimmed contents; absent file or bad value → `None`.
fn read_parse<T: std::str::FromStr>(path: &Path) -> Option<T> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Read a sysfs file as a trimmed string; absent file → `None`.
fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Read a sysfs PCI id ("0x56a0\n" → "56a0", lowercased). An empty file is not an id —
/// `None`, so the name fallback says "Intel GPU" rather than "Intel GPU [8086:]".
fn read_hex_id(path: &Path) -> Option<String> {
    let s = read_trim(path)?;
    let s = s.strip_prefix("0x").unwrap_or(&s).to_ascii_lowercase();
    (!s.is_empty()).then_some(s)
}

/// One uevent field by key ("DRIVER=" → "i915").
fn uevent_field(dev_path: &Path, key: &str) -> Option<String> {
    fs::read_to_string(dev_path.join("uevent"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .map(|s| s.trim().to_string())
}

/// Tiny well-known-id table for the discrete Arc parts a dGPU fixture/user is likely to
/// hit; anything else falls back to the honest PCI-id form rather than a guessed name
/// (there is no shipped equivalent of libdrm's `amdgpu.ids` for Intel).
fn known_gpu_name(device: &str) -> Option<&'static str> {
    Some(match device {
        "56a0" => "Intel Arc A770",
        "56a1" => "Intel Arc A750",
        "56a2" => "Intel Arc A580",
        "56a5" => "Intel Arc A380",
        "56a6" => "Intel Arc A310",
        "e20b" => "Intel Arc B580",
        "e20c" => "Intel Arc B570",
        _ => return None,
    })
}

fn gpu_name(dev_path: &Path) -> String {
    match read_hex_id(&dev_path.join("device")) {
        Some(id) => known_gpu_name(&id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Intel GPU [8086:{id}]")),
        None => "Intel GPU".into(),
    }
}

/// hwmon temps are MILLI-degrees C — but the package/GT sensor lives on a DIFFERENT
/// channel per dialect (research 07 §3.2): i915 exposes it as `temp1_input` (gate
/// 6.12+); xe hwmon has NO temp1 — its package temp is `temp2_input` and `temp3_input`
/// is the VRAM sensor (gate 6.15+, ABI-documented). Each dialect reads exactly its
/// documented channel and never falls back to another one: temp3 (VRAM) surfaced as
/// device temp would be a different physical claim, and a wrong-channel read on either
/// dialect presents some other sensor as GPU temp. The designated file being absent
/// (older kernels, the no-hwmon iGPU case) is the normal outcome → `None`.
fn temp_c(hwmon: &Path, dialect: Dialect) -> Option<f32> {
    let file = match dialect {
        Dialect::I915 => "temp1_input",
        Dialect::Xe => "temp2_input",
    };
    let millic: i64 = read_parse(&hwmon.join(file))?;
    Some(millic as f32 / 1000.0)
}

/// Which hwmon power/energy channel index a device's power numbers come from (research
/// 07 §3.2.2). xe numbers its channels by SCOPE: index 1 = card, index 2 = pkg (the GPU
/// package). i915 only ever exposes index 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PowerChannel {
    /// `power1_max` / `energy1_input`.
    Card,
    /// `power2_max` / `energy2_input` — xe only.
    Pkg,
}

impl PowerChannel {
    fn max_file(self) -> &'static str {
        match self {
            PowerChannel::Card => "power1_max",
            PowerChannel::Pkg => "power2_max",
        }
    }

    fn energy_file(self) -> &'static str {
        match self {
            PowerChannel::Card => "energy1_input",
            PowerChannel::Pkg => "energy2_input",
        }
    }
}

/// `StaticInfo::source_caveat` for an xe device whose power comes from the pkg channel:
/// the number is GPU-package power, not the whole-card figure an NVIDIA/AMD board
/// reports, and must not be read as if it were. (The model has no per-metric scope
/// field; `source_caveat` is its existing honesty-label slot.)
const PKG_POWER_CAVEAT: &str = "power: xe pkg (GPU package) channel, not whole-card power";

/// Resolve the ONE channel both power numbers (limit and derived draw) come from.
///
/// i915: index 1 only — i915 hwmon has no pkg channel, so there is nothing to fall
/// back to (and a stray index-2 file on i915 would be some other claim).
///
/// xe (§3.2.2): prefer card, fall back to pkg, `None` only when both are absent. Card
/// energy is BMG/PMT-gated and pkg is what BMG and DG2-forced-on-xe expose, so a
/// card-only read leaves pkg-only platforms at `None` while a real Some sits in sysfs.
///
/// The channel is chosen ONCE per device and shared by `power_limit_mw` and `power_mw`,
/// never mixed per quantity: the TUI draws power as a fill of "draw / limit", and pkg
/// draw over a card limit is a ratio of two different physical scopes — a
/// confidently-wrong proportion. The energy counter (the live reading) decides the
/// channel; the limit file only decides when neither channel has energy. If the chosen
/// channel lacks a limit file the limit is honestly `None`, never borrowed from the
/// other channel. Which channels exist per tag/SKU is [HW-VERIFY] in research 07
/// §3.2.2 — the fallback order is the documented one, not a capture.
fn resolve_power_channel(hwmon: &Path, dialect: Dialect) -> Option<PowerChannel> {
    match dialect {
        Dialect::I915 => Some(PowerChannel::Card),
        Dialect::Xe => {
            let readable = |file: &str| read_parse::<u64>(&hwmon.join(file)).is_some();
            let by = |file: fn(PowerChannel) -> &'static str| {
                [PowerChannel::Card, PowerChannel::Pkg]
                    .into_iter()
                    .find(|&ch| readable(file(ch)))
            };
            by(PowerChannel::energy_file).or_else(|| by(PowerChannel::max_file))
        }
    }
}

/// `power<N>_max` (the sustained PL1 power limit — Intel hwmon has no `power1_cap`) is
/// MICROwatts; the model carries milliwatts.
fn power_cap_mw(hwmon: &Path, ch: PowerChannel) -> Option<u32> {
    let uw: u64 = read_parse(&hwmon.join(ch.max_file()))?;
    Some((uw / 1000) as u32)
}

/// i915/xe hwmon exposes no instantaneous power reading — only the cumulative
/// `energy<N>_input` MICROjoule counter. Power is the delta between two sightings:
/// ΔµJ / Δms = mW exactly. No baseline or a counter that went backwards → `None`.
fn energy_delta_mw(prev_uj: u64, prev_ts_ms: u64, cur_uj: u64, cur_ts_ms: u64) -> Option<u32> {
    let wall_ms = cur_ts_ms.checked_sub(prev_ts_ms)?;
    if wall_ms == 0 {
        return None;
    }
    let uj = cur_uj.checked_sub(prev_uj)?;
    Some((uj / wall_ms) as u32)
}

/// The fdinfo keys this backend consumes, across BOTH dialects (the key sets are
/// disjoint, so one parser serves both). Anything missing — older kernel (i915 engine
/// busy-ns 5.19+, per-process memory 6.8+, xe cycles 6.11+), non-DRM fd — simply stays
/// `None`: the process is still listed, its absent columns are honest.
#[derive(Default)]
struct FdinfoDrm {
    pdev: Option<String>,
    // i915 dialect: cumulative per-engine-class busy-ns.
    render_ns: Option<u64>,
    video_ns: Option<u64>,
    venh_ns: Option<u64>,
    compute_ns: Option<u64>,
    // `drm-engine-capacity-<class>` for the class utilization is computed from (i915
    // `render`, xe `rcs`). Raw: absent stays `None` (the spec's "assume 1"); a present
    // but zero/unparsable value is stored as `Some(0)` — see [`class_capacity`].
    render_capacity: Option<u64>,
    rcs_capacity: Option<u64>,
    // xe dialect: cumulative busy-cycles and the matching elapsed-cycles base, in
    // GT-clock ticks (NOT time units — see `cycles_util_pct`).
    rcs_cycles: Option<u64>,
    rcs_total_cycles: Option<u64>,
    vcs_cycles: Option<u64>,
    vecs_cycles: Option<u64>,
    ccs_cycles: Option<u64>,
    // Device-local memory, instance 0 only: i915 names the region "local0", xe names
    // it "vram0". System-RAM regions are deliberately NOT read — an iGPU's buffers in
    // system memory must not masquerade as VRAM.
    total_local_bytes: Option<u64>,
    resident_local_bytes: Option<u64>,
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
        mx(&mut self.render_ns, other.render_ns);
        mx(&mut self.video_ns, other.video_ns);
        mx(&mut self.venh_ns, other.venh_ns);
        mx(&mut self.compute_ns, other.compute_ns);
        mx(&mut self.render_capacity, other.render_capacity);
        mx(&mut self.rcs_capacity, other.rcs_capacity);
        mx(&mut self.rcs_cycles, other.rcs_cycles);
        mx(&mut self.rcs_total_cycles, other.rcs_total_cycles);
        mx(&mut self.vcs_cycles, other.vcs_cycles);
        mx(&mut self.vecs_cycles, other.vecs_cycles);
        mx(&mut self.ccs_cycles, other.ccs_cycles);
        mx(&mut self.total_local_bytes, other.total_local_bytes);
        mx(&mut self.resident_local_bytes, other.resident_local_bytes);
    }

    /// `drm-total-<region>` (all buffers) preferred; `drm-resident-<region>` is the
    /// fallback when a kernel exposes only the resident split.
    fn local_mem_bytes(&self) -> Option<u64> {
        self.total_local_bytes.or(self.resident_local_bytes)
    }

    /// Honest kind attribution: a busy video/video-enhance engine is decisively media
    /// (rendered to the table as Graphics); a busy compute engine (i915 CCS class /
    /// xe ccs) is decisively Compute. Render-only activity stays Unknown — the render
    /// engine runs BOTH 3D and pre-CCS GPGPU, so calling it either would be a guess.
    fn kind(&self) -> ProcessKind {
        // Per-field "is any busy" checks, never a sum: these are unvalidated u64s from
        // fdinfo, and summing values near u64::MAX panics debug builds / wraps in
        // release. (Blobs parse before the pdev filter, so one corrupt blob anywhere
        // in /proc would take the whole scan down.)
        let any_busy = |fields: &[Option<u64>]| fields.iter().any(|f| f.is_some_and(|v| v > 0));
        if any_busy(&[
            self.video_ns,
            self.venh_ns,
            self.vcs_cycles,
            self.vecs_cycles,
        ]) {
            ProcessKind::Graphics
        } else if any_busy(&[self.compute_ns, self.ccs_cycles]) {
            ProcessKind::Compute
        } else {
            ProcessKind::Unknown
        }
    }
}

/// Parse one fdinfo blob ("key:\tvalue" lines). i915 engine values carry a mandatory
/// "ns" suffix; xe cycle counters are bare uints (no unit is the unit, per
/// drm-usage-stats); memory values follow `drm_print_memory_stats` scaling — bytes by
/// default, "KiB"/"MiB" when the kernel scaled them. Guessing units is the classic
/// fdinfo parsing bug, so each key gets exactly its documented unit handling.
fn parse_fdinfo(contents: &str) -> FdinfoDrm {
    let mut out = FdinfoDrm::default();
    for line in contents.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match key.trim() {
            "drm-pdev" => out.pdev = Some(val.to_ascii_lowercase()),
            "drm-engine-render" => out.render_ns = parse_suffixed(val, "ns"),
            "drm-engine-video" => out.video_ns = parse_suffixed(val, "ns"),
            "drm-engine-video-enhance" => out.venh_ns = parse_suffixed(val, "ns"),
            "drm-engine-compute" => out.compute_ns = parse_suffixed(val, "ns"),
            // A present capacity key that is zero or garbage is kept as Some(0) (invalid),
            // NOT folded into absence: absence legitimately means 1, garbage does not.
            "drm-engine-capacity-render" => out.render_capacity = Some(val.parse().unwrap_or(0)),
            "drm-engine-capacity-rcs" => out.rcs_capacity = Some(val.parse().unwrap_or(0)),
            "drm-cycles-rcs" => out.rcs_cycles = val.parse().ok(),
            "drm-total-cycles-rcs" => out.rcs_total_cycles = val.parse().ok(),
            "drm-cycles-vcs" => out.vcs_cycles = val.parse().ok(),
            "drm-cycles-vecs" => out.vecs_cycles = val.parse().ok(),
            "drm-cycles-ccs" => out.ccs_cycles = val.parse().ok(),
            "drm-total-local0" | "drm-total-vram0" => {
                out.total_local_bytes = parse_mem_bytes(val);
            }
            "drm-resident-local0" | "drm-resident-vram0" => {
                out.resident_local_bytes = parse_mem_bytes(val);
            }
            _ => {}
        }
    }
    out
}

fn parse_suffixed(val: &str, unit: &str) -> Option<u64> {
    val.strip_suffix(unit)?.trim().parse().ok()
}

/// drm-usage-stats memory value: bytes by default, the kernel's print helper scales to
/// "KiB"/"MiB" when evenly divisible. Scaling is overflow-checked: a count that exceeds
/// u64 bytes cannot be real memory, and unchecked `*` would panic a debug build or wrap
/// to a fabricated number in release.
fn parse_mem_bytes(val: &str) -> Option<u64> {
    if let Some(kib) = val.strip_suffix("KiB") {
        return kib.trim().parse::<u64>().ok()?.checked_mul(1024);
    }
    if let Some(mib) = val.strip_suffix("MiB") {
        return mib.trim().parse::<u64>().ok()?.checked_mul(1024 * 1024);
    }
    val.parse().ok()
}

/// Resolve a raw `drm-engine-capacity-<class>` value (drm-usage-stats): the number of
/// identical hardware engines the class's counters are summed across. Absent → 1 (the
/// spec's stated default). Zero is disallowed by the spec and garbage is not a number,
/// so both → `None`: the class's utilization is refused rather than divided by a
/// capacity we made up.
fn class_capacity(raw: Option<u64>) -> Option<u64> {
    match raw {
        None => Some(1),
        Some(0) => None,
        Some(c) => Some(c),
    }
}

/// i915 utilization: engine counters are cumulative busy-NANOSECONDS, so utilization is
/// the busy delta over capacity × WALL time between two sightings — a class of N
/// identical engines can accrue N × wall busy-ns. No baseline (first sighting) or a
/// counter that went backwards (pid reuse re-created the client) → `None`, never a
/// guess. The clamp is a last-resort guard against inconsistent counters, not the math.
fn engine_util_pct(
    prev_ns: u64,
    prev_ts_ms: u64,
    cur_ns: u64,
    cur_ts_ms: u64,
    capacity: u64,
) -> Option<f32> {
    let wall_ms = cur_ts_ms.checked_sub(prev_ts_ms)?;
    if wall_ms == 0 || capacity == 0 {
        return None;
    }
    let busy_ns = cur_ns.checked_sub(prev_ns)?;
    let pct = busy_ns as f64 / (wall_ms as f64 * 1_000_000.0 * capacity as f64) * 100.0;
    Some(pct.min(100.0) as f32)
}

/// xe utilization: `drm-cycles-*` are GT-clock ticks, paired with a `drm-total-cycles-*`
/// elapsed base in the SAME ticks. Utilization is Δbusy-cycles / Δtotal-cycles — NOT
/// divided by wall time. Dividing cycle counts by wall nanoseconds is the classic
/// i915→xe porting bug: GT-clock frequency changes with DVFS, so cycles have no fixed
/// time value and only the kernel-provided base is a valid denominator. A multi-engine
/// class (`drm-engine-capacity` > 1) sums busy-cycles across its instances, so the
/// denominator is capacity × Δtotal (research 07 §3.2) — dividing by Δtotal alone and
/// clamping would report a half-busy 2-engine class as 100%. The clamp that remains is
/// a last-resort guard against inconsistent counters, not the math.
fn cycles_util_pct(prev_c: u64, prev_t: u64, cur_c: u64, cur_t: u64, capacity: u64) -> Option<f32> {
    let total = cur_t.checked_sub(prev_t)?;
    if total == 0 || capacity == 0 {
        return None;
    }
    let busy = cur_c.checked_sub(prev_c)?;
    let pct = busy as f64 / (total as f64 * capacity as f64) * 100.0;
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

/// First hwmon dir under the device. hwmon indices are not stable across boots, so it is
/// resolved through the device dir at init; absence (every iGPU, pre-gate kernels,
/// fixtures) is normal.
fn first_hwmon(dev_path: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dev_path.join("hwmon")).ok()?;
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // read_dir order is arbitrary, and a plain string sort puts `hwmon10` before
    // `hwmon5` — order numerically (research 07 §3.2 minor hardening).
    dirs.sort_by_cached_key(|p| hwmon_sort_key(p));
    dirs.into_iter().next()
}

/// Sort key for an hwmon dir: `hwmonN` orders by N numerically, ahead of any entry not
/// shaped `hwmon<digits>` (those fall back to name order, after every numbered dir).
fn hwmon_sort_key(path: &Path) -> (bool, u64, String) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name
        .strip_prefix("hwmon")
        .and_then(|d| d.parse::<u64>().ok())
    {
        Some(n) => (false, n, name),
        None => (true, 0, name),
    }
}

/// One enumerated i915/xe PCI device, resolved at init.
struct IntelDevice {
    id: DeviceId,
    dialect: Dialect,
    /// `{root}/sys/class/drm/cardN` — i915 keeps `gt_*_freq_mhz` and `lmem_*` HERE,
    /// at the card level, not under the PCI dir.
    card_path: PathBuf,
    /// `{root}/sys/class/drm/cardN/device` — the PCI dir; xe's freq tree hangs off it.
    dev_path: PathBuf,
    hwmon: Option<PathBuf>,
    /// The single hwmon channel both power numbers come from, resolved at init (sysfs
    /// attribute visibility is fixed at hwmon registration). Fixed for the session, so
    /// the energy watermark can never difference two different channels' counters.
    power_channel: Option<PowerChannel>,
}

impl IntelDevice {
    /// `power<N>_max` on the resolved channel.
    fn power_limit_mw(&self) -> Option<u32> {
        power_cap_mw(self.hwmon.as_deref()?, self.power_channel?)
    }

    /// Cumulative `energy<N>_input` µJ on the resolved channel.
    fn energy_uj(&self) -> Option<u64> {
        read_parse(
            &self
                .hwmon
                .as_deref()?
                .join(self.power_channel?.energy_file()),
        )
    }

    /// The pkg-scope honesty label, when (and only when) power comes from xe's pkg
    /// channel.
    fn source_caveat(&self) -> Option<String> {
        (self.power_channel == Some(PowerChannel::Pkg)).then(|| PKG_POWER_CAVEAT.to_string())
    }
}

impl IntelDevice {
    /// xe per-GT freq dir (kernel 6.8+ layout; the fixtures model 6.11). tile0/gt0 is
    /// the primary render GT — a media GT's clock is not the "SM clock".
    fn xe_freq(&self) -> PathBuf {
        self.dev_path.join("tile0/gt0/freq0")
    }

    /// Current frequency, from the actual (measured) file. It reads 0 while the GT is
    /// power-gated in RC6 — that is "not clocked right now", not a measured rate, and
    /// in `--json` a literal 0 is indistinguishable from a measurement: per-field
    /// absence is the model's idiom for it. The requested file (`cur`) only covers an
    /// absent `act` file (old kernels) — it is never consulted for a sleeping GT,
    /// because claiming the requested clock while the GT sleeps would be a lie.
    fn act_freq_mhz(&self) -> Option<u32> {
        let (act, cur) = match self.dialect {
            Dialect::I915 => (
                self.card_path.join("gt_act_freq_mhz"),
                self.card_path.join("gt_cur_freq_mhz"),
            ),
            Dialect::Xe => (
                self.xe_freq().join("act_freq"),
                self.xe_freq().join("cur_freq"),
            ),
        };
        match read_parse::<u32>(&act) {
            Some(0) => None,
            Some(mhz) => Some(mhz),
            None => read_parse(&cur),
        }
    }

    /// Hardware maximum (RP0). `gt_max_freq_mhz`/`max_freq` are user-settable caps,
    /// not the hardware limit, so they are deliberately not read here.
    fn max_freq_mhz(&self) -> Option<u32> {
        match self.dialect {
            Dialect::I915 => read_parse(&self.card_path.join("gt_RP0_freq_mhz")),
            Dialect::Xe => read_parse(&self.xe_freq().join("rp0_freq")),
        }
    }

    /// Decode the per-GT throttle ("performance limit") reasons that BOTH drivers publish
    /// as one-bit-per-file sysfs flags — the metric `intel_gpu_top` still does not surface
    /// on xe, so this is the displacement story for that hardware. The two dialects expose
    /// the SAME nine reasons under different paths and different filename spellings
    /// (verified against the in-tree drivers, June 2026):
    ///
    /// - i915: `{card}/gt/gt0/throttle_reason_{status,pl1,pl2,pl4,thermal,prochot,ratl,vr_thermalert,vr_tdc}`,
    ///   created per-GT in `intel_gt_sysfs_pm.c`.
    /// - xe: `{dev}/tile0/gt0/freq0/throttle/{status,reason_pl1,reason_pl2,reason_pl4,reason_thermal,reason_prochot,reason_ratl,reason_vr_thermalert,reason_vr_tdc}`,
    ///   the "throttle" attribute group on the freq0 kobject in `xe_gt_throttle.c`.
    ///
    /// Each file contains 0 or 1. tile0/gt0 is the primary render GT, matching the freq
    /// reads above; a media GT throttling is not the render device's story.
    ///
    /// `status` is the GATE, never the union of the reason files: some kernels report a
    /// stale 1 in a reason file while `status` is 0 (the limit cleared but the latched
    /// reason bit lingers). Trusting `status` means we only ever narrate a throttle that
    /// is actually happening now — a confidently-wrong throttle event would kill the
    /// product's trust thesis.
    ///
    /// Observability split (§5.4): an absent/unreadable/garbage `status` file means this
    /// kernel exposes no working throttle interface for the GT — **unobservable**,
    /// `None`, never an asserted "not throttling". An explicit `0` IS an observation
    /// (`Some(default)`: the hardware says no limit is active), REGARDLESS of what the
    /// (possibly stale-latched) reason files say.
    fn throttle(&self) -> Option<ThrottleReasons> {
        // Filenames differ per dialect; the meaning of each does not.
        let (dir, status, pl1, pl2, pl4, thermal, prochot, ratl, vr_thermalert, vr_tdc) =
            match self.dialect {
                Dialect::I915 => (
                    self.card_path.join("gt/gt0"),
                    "throttle_reason_status",
                    "throttle_reason_pl1",
                    "throttle_reason_pl2",
                    "throttle_reason_pl4",
                    "throttle_reason_thermal",
                    "throttle_reason_prochot",
                    "throttle_reason_ratl",
                    "throttle_reason_vr_thermalert",
                    "throttle_reason_vr_tdc",
                ),
                Dialect::Xe => (
                    self.xe_freq().join("throttle"),
                    "status",
                    "reason_pl1",
                    "reason_pl2",
                    "reason_pl4",
                    "reason_thermal",
                    "reason_prochot",
                    "reason_ratl",
                    "reason_vr_thermalert",
                    "reason_vr_tdc",
                ),
            };

        // Missing/unreadable status file: no throttle interface → unobservable (None).
        // An explicit "0": observed not-throttling — the reason files are not even
        // consulted (some report stale latched 1s). Garbage content is a broken
        // interface, which is unobservable too — refusing beats guessing.
        let status_raw = fs::read_to_string(dir.join(status)).ok()?;
        match status_raw.trim() {
            "1" => {}
            "0" => return Some(ThrottleReasons::default()),
            _ => return None,
        }

        // status==1: something IS limiting performance. Map each recognized reason file
        // into the model's vocabulary; unreadable/absent reason files read as not-set.
        let r = |name: &str| read_throttle_bit(&dir.join(name));
        let reasons = ThrottleReasons {
            thermal: r(thermal),
            power_cap: r(pl1) || r(pl2) || r(pl4),
            hw_slowdown: r(prochot),
            // RATL (run-average thermal limit) and the VR alerts have no closer model
            // bucket than "other"; they ARE real limits, just not ones the UI names.
            other: r(ratl) || r(vr_thermalert) || r(vr_tdc),
            sync_boost: false, // an NVIDIA-only concept; no Intel sysfs source.
        };
        // status says we're throttling but no reason file we recognize is set (a reason
        // bit this build doesn't map, or files the kernel didn't expose): assert the
        // honest minimum — throttling, cause unknown — rather than swallow the signal.
        if reasons.any() {
            Some(reasons)
        } else {
            Some(ThrottleReasons {
                other: true,
                ..ThrottleReasons::default()
            })
        }
    }
}

/// Read a 0/1 throttle flag file: `true` only on an explicit "1". Absent, unreadable, or
/// non-"1" (including garbage) → `false`, so a broken/old kernel never fabricates a
/// throttle. Mirrors the "absence is normal" contract used everywhere else here.
fn read_throttle_bit(path: &Path) -> bool {
    read_trim(path).as_deref() == Some("1")
}

/// Enumerate `{root}/sys/class/drm/cardN` dirs whose vendor id is Intel (0x8086).
/// Connector nodes ("card2-DP-1") and render nodes are skipped; a card without a
/// PCI_SLOT_NAME has no stable identity and is skipped rather than guessed; an Intel
/// device bound to neither i915 nor xe speaks neither fdinfo dialect and is skipped.
fn discover(root: &Path) -> Vec<IntelDevice> {
    let Ok(entries) = fs::read_dir(root.join("sys/class/drm")) else {
        return Vec::new();
    };
    let mut cards: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let idx: u32 = name.strip_prefix("card")?.parse().ok()?;
            Some((idx, e.path()))
        })
        .collect();
    cards.sort_by_key(|(idx, _)| *idx); // deterministic device order

    let mut devs = Vec::new();
    for (_, card_path) in cards {
        let dev_path = card_path.join("device");
        if read_trim(&dev_path.join("vendor")).as_deref() != Some("0x8086") {
            continue;
        }
        let dialect = match uevent_field(&dev_path, "DRIVER=").as_deref() {
            Some("i915") => Dialect::I915,
            Some("xe") => Dialect::Xe,
            _ => continue,
        };
        // An empty PCI_SLOT_NAME is no identity at all — skip, per the contract above,
        // rather than registering a ghost device with a blank id.
        let Some(pci) = uevent_field(&dev_path, "PCI_SLOT_NAME=").filter(|s| !s.is_empty()) else {
            continue;
        };
        let hwmon = first_hwmon(&dev_path);
        let power_channel = hwmon
            .as_deref()
            .and_then(|h| resolve_power_channel(h, dialect));
        devs.push(IntelDevice {
            id: DeviceId(pci.to_ascii_lowercase()),
            dialect,
            card_path,
            dev_path,
            hwmon,
            power_channel,
        });
    }
    devs
}

pub struct IntelBackend {
    root: PathBuf,
    devs: Vec<IntelDevice>,
    /// i915: per-(device, pid) render-engine watermark (cumulative busy-ns, wall ms).
    last_render: HashMap<(DeviceId, u32), (u64, u64)>,
    /// xe: per-(device, pid) rcs watermark (cumulative busy-cycles, total-cycles) —
    /// both axes are GT ticks, wall time is deliberately not part of this watermark.
    last_cycles: HashMap<(DeviceId, u32), (u64, u64)>,
    /// Per-device hwmon energy watermark (cumulative µJ, wall ms) for derived power.
    last_energy: HashMap<DeviceId, (u64, u64)>,
    /// Set once at init: explanation for a known-incomplete process list, if any.
    process_hint: Option<String>,
    /// Turns the kernel's cumulative per-PID CPU counter into a per-tick rate. The CPU%/
    /// container columns come from `/proc` (never the device root: a fixture tree's pids are
    /// not real processes), shared with the other Linux backends via `crate::proc_meta`.
    #[cfg(target_os = "linux")]
    cpu: crate::proc_meta::CpuTracker,
}

impl IntelBackend {
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
                "no i915/xe devices under sys/class/drm".into(),
            ));
        }
        let process_hint = fdinfo_process_hint(&root);
        Ok(Self {
            root,
            devs,
            last_render: HashMap::new(),
            last_cycles: HashMap::new(),
            last_energy: HashMap::new(),
            process_hint,
            #[cfg(target_os = "linux")]
            cpu: crate::proc_meta::CpuTracker::new(),
        })
    }

    fn device(&self, dev: &DeviceId) -> Result<&IntelDevice, BackendError> {
        self.devs
            .iter()
            .find(|d| &d.id == dev)
            .ok_or_else(|| BackendError::DeviceNotFound(dev.clone()))
    }
}

impl GpuBackend for IntelBackend {
    fn name(&self) -> &'static str {
        "intel"
    }

    fn devices(&mut self) -> Vec<DeviceId> {
        self.devs.iter().map(|d| d.id.clone()).collect()
    }

    fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
        let d = self.device(dev)?;

        Ok(StaticInfo {
            id: dev.clone(),
            vendor: Vendor::Intel,
            name: gpu_name(&d.dev_path),
            backend: "intel".into(),
            // Dedicated VRAM only. i915 dGPUs expose it as card-level lmem_total_bytes;
            // an iGPU has no such file and shares system RAM, which must NOT be reported
            // as VRAM. xe has no VRAM-total sysfs at all (research 02) — honest None.
            mem_total_bytes: match d.dialect {
                Dialect::I915 => read_parse(&d.card_path.join("lmem_total_bytes")),
                Dialect::Xe => None,
            },
            // On the one resolved channel (card preferred, xe pkg fallback) — the same
            // channel `power_mw` derives from, so draw/limit is one physical scope.
            power_limit_mw: d.power_limit_mw(),
            max_sm_clock_mhz: d.max_freq_mhz(),
            // No sysfs source for the thermal-slowdown knee; claiming one would
            // mis-narrate throttle events. Honest absence.
            temp_slowdown_c: None,
            // i915/xe are in-tree drivers: there is no driver version distinct from the
            // kernel, and the uevent DRIVER= field is a name, not a version.
            driver_version: None,
            process_hint: self.process_hint.clone(),
            // sysfs/fdinfo numbers carry their plain meanings — except xe power read from
            // the pkg channel, which is package-scope, not whole-card, and says so.
            source_caveat: d.source_caveat(),
            // The PCI link files under the card describe the dGPU's internal switch port,
            // not the slot link — comparing against them would be a lie. Honest absence.
            pcie_max_gen: None,
            pcie_max_width: None,
        })
    }

    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        let d = self.device(dev)?;
        let sm_clock_mhz = d.act_freq_mhz();
        // The cap on the same resolved channel as power_mw, re-read per tick so a changed
        // PL1 is narrated (power_limit_changed).
        let power_limit_mw = d.power_limit_mw();
        let temp_c = d.hwmon.as_deref().and_then(|h| temp_c(h, d.dialect));
        let energy_uj = d.energy_uj();
        // Decode throttle reasons before any &mut self borrow below releases `d`.
        let throttle = d.throttle();

        // One wall timestamp for the whole frame (per CLAUDE.md).
        let ts = now_ms();
        // Derived power from the cumulative energy counter; first sighting → None.
        let power_mw = energy_uj.and_then(|cur| {
            let prev = self.last_energy.insert(dev.clone(), (cur, ts));
            prev.and_then(|(p_uj, p_ts)| energy_delta_mw(p_uj, p_ts, cur, ts))
        });

        Ok(DynamicSample {
            ts_ms: ts,
            // Device-level busyness is not directly exposed: the perf PMU needs
            // root/CAP_PERFMON (xe PMU 6.15+), and summing fdinfo across clients we may
            // not be able to see would understate. None is the honest answer.
            util_pct: None,
            util_engine: None,
            // No reliable per-device counter: an iGPU has no VRAM, and xe exposes no
            // device-wide VRAM-used sysfs — per-process fdinfo totals are not a device
            // total under a privilege wall.
            mem_used_bytes: None,
            power_mw,
            temp_c,
            // hwmon fan (i915 6.12+/xe 6.16+) is RPM-only with no fan1_max; the model
            // carries percent-of-max, so there is no honest value to derive.
            fan_pct: None,
            sm_clock_mhz,
            // No unprivileged sysfs for the memory clock on either driver.
            mem_clock_mhz: None,
            // Video-engine busyness exists only per-process (fdinfo); the device-level
            // numbers live behind the same PMU privilege wall as util.
            encoder_pct: None,
            decoder_pct: None,
            // Per-GT throttle ("performance limit") reasons, decoded per dialect from the
            // one-bit-per-file sysfs flags (i915 gt/gt0/throttle_reason_*, xe
            // freq0/throttle/*). `status` is the gate — absent file → None (this kernel
            // exposes no throttle interface: unobservable, §5.4); status==0 →
            // Some(default): observed honest silence. This is the metric intel_gpu_top
            // still does not surface on xe.
            throttle,
            power_limit_mw,
            // See static_info: the card's link files describe its internal switch port.
            pcie_gen: None,
            pcie_width: None,
            ecc_uncorrected: None,
        })
    }

    fn refresh_processes(&mut self, dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError> {
        let (pci, dialect) = {
            let d = self.device(dev)?;
            (d.id.0.clone(), d.dialect)
        };
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
            // Per-dialect watermark math; first sighting has no baseline → None. A
            // kernel below the dialect's gate never produces the keys, so util stays
            // None while the process itself is still listed.
            // The watermark is still advanced when the capacity is invalid, so a later
            // sane capacity has a baseline; only the reported rate is refused.
            let util_pct = match dialect {
                Dialect::I915 => agg.render_ns.and_then(|cur| {
                    let prev = self.last_render.insert((dev.clone(), pid), (cur, ts));
                    let cap = class_capacity(agg.render_capacity);
                    prev.zip(cap)
                        .and_then(|((p_ns, p_ts), cap)| engine_util_pct(p_ns, p_ts, cur, ts, cap))
                }),
                Dialect::Xe => match (agg.rcs_cycles, agg.rcs_total_cycles) {
                    (Some(c), Some(t)) => {
                        let prev = self.last_cycles.insert((dev.clone(), pid), (c, t));
                        let cap = class_capacity(agg.rcs_capacity);
                        prev.zip(cap)
                            .and_then(|((p_c, p_t), cap)| cycles_util_pct(p_c, p_t, c, t, cap))
                    }
                    _ => None,
                },
            };
            out.push(ProcessSample {
                pid,
                name: comm_name(&self.root, pid),
                kind: agg.kind(),
                mem_bytes: agg.local_mem_bytes(),
                util_pct,
                cpu_pct: None,
                container: None,
                cmdline: None,
            });
        }
        // Drop watermarks for pids that vanished from this device (exited processes).
        self.last_render
            .retain(|(d, pid), _| d != dev || by_pid.contains_key(pid));
        self.last_cycles
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
    fn fdinfo_i915_keys_require_ns_suffix() {
        let blob = "drm-pdev:\t0000:03:00.0\ndrm-engine-render:\t123 ns\n\
                    drm-engine-video:\t456 ns\ndrm-total-local0:\t786432 KiB\n";
        let f = parse_fdinfo(blob);
        assert_eq!(f.pdev.as_deref(), Some("0000:03:00.0"));
        assert_eq!(f.render_ns, Some(123));
        assert_eq!(f.video_ns, Some(456));
        assert_eq!(f.total_local_bytes, Some(786_432 * 1024));
        assert_eq!(f.compute_ns, None, "absent key stays None");
        assert_eq!(f.rcs_cycles, None, "no xe keys in an i915 blob");
        // A value with the wrong/missing suffix is a key we do not understand.
        assert_eq!(parse_suffixed("123", "ns"), None);
        assert_eq!(parse_suffixed("123 ms", "ns"), None);
    }

    #[test]
    fn fdinfo_xe_keys_are_bare_cycle_counts() {
        let blob = "drm-pdev:\t0000:03:00.0\ndrm-cycles-rcs:\t1000000\n\
                    drm-total-cycles-rcs:\t50000000\ndrm-cycles-ccs:\t8000000\n\
                    drm-total-vram0:\t2097152 KiB\ndrm-resident-vram0:\t1048576 KiB\n";
        let f = parse_fdinfo(blob);
        assert_eq!(f.rcs_cycles, Some(1_000_000));
        assert_eq!(f.rcs_total_cycles, Some(50_000_000));
        assert_eq!(f.ccs_cycles, Some(8_000_000));
        assert_eq!(f.total_local_bytes, Some(2_147_483_648));
        assert_eq!(f.resident_local_bytes, Some(1_073_741_824));
        assert_eq!(f.render_ns, None, "no i915 keys in an xe blob");
        // total preferred over resident.
        assert_eq!(f.local_mem_bytes(), Some(2_147_483_648));
    }

    #[test]
    fn mem_values_scale_per_drm_print_memory_stats() {
        assert_eq!(parse_mem_bytes("4096"), Some(4096)); // default unit is bytes
        assert_eq!(parse_mem_bytes("4096 KiB"), Some(4_194_304));
        assert_eq!(parse_mem_bytes("12 MiB"), Some(12_582_912));
        assert_eq!(parse_mem_bytes("12 GiB"), None); // unknown suffix is not a guess
    }

    #[test]
    fn hostile_fdinfo_values_cannot_panic_or_wrap() {
        // u64::MAX KiB/MiB cannot be a real byte count: None — never a debug panic,
        // never a release-mode wrap to a fabricated number.
        assert_eq!(parse_mem_bytes("18446744073709551615 KiB"), None);
        assert_eq!(parse_mem_bytes("18446744073709551615 MiB"), None);
        // kind() must not overflow either: near-MAX engine counters used to panic the
        // video/compute sums in debug builds.
        let f = parse_fdinfo(
            "drm-engine-video:\t18446744073709551615 ns\ndrm-engine-video-enhance:\t1 ns\n",
        );
        assert_eq!(f.kind(), ProcessKind::Graphics);
        let f = parse_fdinfo(
            "drm-engine-compute:\t18446744073709551615 ns\ndrm-cycles-ccs:\t18446744073709551615\n",
        );
        assert_eq!(f.kind(), ProcessKind::Compute);
    }

    #[test]
    fn i915_engine_util_needs_baseline_and_handles_resets() {
        // 500ms of busy-ns over 1000ms of wall = 50%.
        assert_eq!(engine_util_pct(0, 0, 500_000_000, 1_000, 1), Some(50.0));
        // Counter went backwards (pid reuse re-created the client): no claim.
        assert_eq!(engine_util_pct(900, 0, 100, 1_000, 1), None);
        // Zero wall delta cannot produce a rate.
        assert_eq!(engine_util_pct(0, 1_000, 100, 1_000, 1), None);
        // More busy-ns than capacity × wall (inconsistent counters) clamps, never exceeds.
        assert_eq!(engine_util_pct(0, 0, 10_000_000_000, 1_000, 1), Some(100.0));
    }

    #[test]
    fn engine_capacity_divides_instead_of_clamping() {
        // A 2-engine class accrues up to 2 × wall busy-ns: 1000ms busy over 1000ms wall
        // is HALF its capacity, not a clamped 100%.
        assert_eq!(engine_util_pct(0, 0, 1_000_000_000, 1_000, 2), Some(50.0));
        // xe: 1.2M busy-cycles summed over 2 instances across 1.2M elapsed = 50%.
        assert_eq!(cycles_util_pct(0, 0, 1_200_000, 1_200_000, 2), Some(50.0));
        // Zero capacity cannot be a denominator.
        assert_eq!(engine_util_pct(0, 0, 1, 1_000, 0), None);
        assert_eq!(cycles_util_pct(0, 0, 1, 1_000, 0), None);
    }

    #[test]
    fn capacity_key_absent_means_one_and_garbage_is_refused() {
        // drm-usage-stats: absent capacity → assume 1.
        assert_eq!(class_capacity(parse_fdinfo("").render_capacity), Some(1));
        let f = parse_fdinfo("drm-engine-capacity-render:\t2\ndrm-engine-capacity-rcs:\t4\n");
        assert_eq!(class_capacity(f.render_capacity), Some(2));
        assert_eq!(class_capacity(f.rcs_capacity), Some(4));
        // Zero is disallowed by the spec and garbage is not a number: refuse, never
        // silently fall back to the absent-key default of 1.
        let f = parse_fdinfo("drm-engine-capacity-render:\t0\ndrm-engine-capacity-rcs:\tzz\n");
        assert_eq!(class_capacity(f.render_capacity), None);
        assert_eq!(class_capacity(f.rcs_capacity), None);
        // Other classes' capacities are not the utilization class's.
        let f = parse_fdinfo("drm-engine-capacity-video:\t2\ndrm-engine-capacity-ccs:\t4\n");
        assert_eq!((f.render_capacity, f.rcs_capacity), (None, None));
    }

    #[test]
    fn hwmon_dirs_sort_numerically() {
        let mut v: Vec<PathBuf> = ["hwmon10", "hwmon5", "weird", "hwmon2"]
            .iter()
            .map(PathBuf::from)
            .collect();
        v.sort_by_cached_key(|p| hwmon_sort_key(p));
        let names: Vec<_> = v.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(names, ["hwmon2", "hwmon5", "hwmon10", "weird"]);
    }

    #[test]
    fn xe_cycles_util_is_over_total_cycles_not_wall_time() {
        // 600k busy cycles over 1.2M elapsed cycles = 50%, regardless of wall time.
        assert_eq!(
            cycles_util_pct(1_000_000, 50_000_000, 1_600_000, 51_200_000, 1),
            Some(50.0)
        );
        // Either counter going backwards (client re-created): no claim.
        assert_eq!(cycles_util_pct(900, 0, 100, 1_000, 1), None);
        assert_eq!(cycles_util_pct(0, 900, 100, 100, 1), None);
        // Zero elapsed-cycles base cannot produce a rate.
        assert_eq!(cycles_util_pct(0, 500, 100, 500, 1), None);
        // Busy past capacity × base (inconsistent counters): clamps, never exceeds.
        assert_eq!(cycles_util_pct(0, 0, 4_000, 1_000, 1), Some(100.0));
    }

    #[test]
    fn energy_delta_is_microjoules_over_milliseconds() {
        // 5,000,000 µJ over 1000 ms = 5 J/s = 5 W = 5000 mW.
        assert_eq!(energy_delta_mw(0, 0, 5_000_000, 1_000), Some(5_000));
        // Counter reset or zero wall delta: no claim.
        assert_eq!(energy_delta_mw(900, 0, 100, 1_000), None);
        assert_eq!(energy_delta_mw(0, 1_000, 100, 1_000), None);
    }

    #[test]
    fn kind_is_honest_about_render_only_clients() {
        // Render-only could be 3D or pre-CCS GPGPU — Unknown, not a guess.
        let render_only = parse_fdinfo("drm-engine-render:\t100 ns\n");
        assert_eq!(render_only.kind(), ProcessKind::Unknown);
        // A busy video engine is decisively media.
        let media = parse_fdinfo("drm-engine-render:\t100 ns\ndrm-engine-video:\t5 ns\n");
        assert_eq!(media.kind(), ProcessKind::Graphics);
        // A busy compute engine is decisively compute — in either dialect.
        let ccs_i915 = parse_fdinfo("drm-engine-compute:\t5 ns\n");
        assert_eq!(ccs_i915.kind(), ProcessKind::Compute);
        let ccs_xe = parse_fdinfo("drm-cycles-ccs:\t5\ndrm-cycles-rcs:\t9\n");
        assert_eq!(ccs_xe.kind(), ProcessKind::Compute);
        // No engine signal at all: Unknown.
        assert_eq!(parse_fdinfo("").kind(), ProcessKind::Unknown);
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

    #[test]
    fn known_arc_names_resolve_and_unknowns_do_not() {
        assert_eq!(known_gpu_name("56a0"), Some("Intel Arc A770"));
        assert_eq!(known_gpu_name("e20b"), Some("Intel Arc B580"));
        assert_eq!(known_gpu_name("56a2"), Some("Intel Arc A580"));
        assert_eq!(known_gpu_name("56a6"), Some("Intel Arc A310"));
        assert_eq!(
            known_gpu_name("46a6"),
            None,
            "iGPUs fall back to the PCI id"
        );
    }
}
