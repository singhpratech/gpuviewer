//! AMD Linux backend — hand-rolled `std::fs` readers over sysfs/hwmon/fdinfo. No library
//! linkage at all: librocm_smi64 is explicitly off the table (soname churn broke btop
//! twice — btop #774) and libdrm ioctls are unnecessary for everything v1 needs.
//!
//! Per the domain rules in CLAUDE.md:
//! - Every path derives from a root-dir parameter (`with_root`), so the whole backend runs
//!   against committed fixture trees; `init()` is just `with_root("/")`.
//! - A missing file or unparsable value is `None`, never a failure — an APU without hwmon
//!   or `pp_dpm_*` tables is a normal device, not a broken one (Intel-iGPU-style absence).
//! - Throttle bits live in the `gpu_metrics` packed binary struct, which is versioned
//!   (v1.0–v3.0) with per-version field offsets AND units, and therefore needs per-version
//!   decoders backed by fixtures. Until that lands, throttle stays
//!   `ThrottleReasons::default()`: no signal is honest, faked bits are not.
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
            // VCN (enc/dec) activity lives in gpu_metrics, not plain sysfs — absent until
            // the per-version decoder lands.
            encoder_pct: None,
            decoder_pct: None,
            // Throttle status also lives in the versioned gpu_metrics packed struct.
            // Default = "no throttle signal", which is honest; faked bits are not.
            throttle: ThrottleReasons::default(),
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
