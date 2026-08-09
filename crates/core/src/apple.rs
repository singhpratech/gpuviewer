//! macOS Apple Silicon **device-level** backend — `docs/design/cross-platform.md` §4.
//! Design verdict: **conditional GO** (§4.6) — and this module is built to that verdict.
//!
//! ## WWDC26 re-check gate (written 2026-06-07; keynote is 2026-06-08)
//!
//! The design's riskiest dependency is a header-less private dylib (`libIOReport.dylib`)
//! with documented per-chip/per-major churn (the Electron cornerMask/Tahoe incident is the
//! precedent for how a silent private-API break reads as the product lying). Two WWDC26
//! outcomes would invalidate the private tiers: a new public telemetry API (better — adopt
//! it), or an IOReport/IOAccelerator access lockdown in macOS 27 (worse — re-plan).
//! Therefore, per §4.6, this module ships **now**:
//!
//! - **Tier A (public Metal)** — fully implemented: `MTLCreateSystemDefaultDevice` →
//!   name / `hasUnifiedMemory` / `recommendedMaxWorkingSetSize`.
//! - **The None-everywhere skeleton** — `refresh_dynamic` returns honest absence for every
//!   Tier B/C metric via [`tier_bc::sample`], a stub the gated tiers will replace.
//! - **The pure Tier B/C parsing + maths layer** ([`parse`]) — implemented and
//!   fixture-tested on every OS (`tests/fixtures/ioreport/`), so the §4.6 unfreeze is a
//!   fill-in of FFI plumbing, not a rewrite.
//!
//! **Gated until the post-keynote re-check** (scan the June 8–12 session list for
//! Metal/Instruments/observability changes; smoke-test IOReport + AGXAccelerator on
//! macOS 27 beta 1; record the outcome as an addendum to the design doc): the live
//! Tier B (IOAccelerator `PerformanceStatistics` via IOKit) and Tier C (IOReport via
//! dlopen2 `Option<fn>` FFI) plumbing — see [`tier_bc`] for the frozen unfreeze checklist.
//! The one-off CI probe (`examples/macos_probe.rs`, run by the manual `macos-probe` job)
//! establishes paravirt-runner ground truth before any CI absence assertion is baked
//! (design §4.5).
//!
//! ## Honesty contract specifics (design §4.2–§4.4)
//!
//! - **Per-process GPU attribution is OS-prohibited for third parties** (re-verified
//!   June 2026: `powermetrics --show-process-gpu` requires root; Activity Monitor's
//!   "% GPU" uses private plumbing with no public equivalent). `refresh_processes`
//!   returns an empty list and [`PROCESS_HINT`] tells the user it is the OS's doing —
//!   the UI must read "the OS forbids this", never render an empty pane pretending
//!   completeness.
//! - `mem_total_bytes` is Metal's `recommendedMaxWorkingSetSize` — a **unified-memory
//!   working-set budget**, not discrete VRAM (Apple publishes no total-VRAM figure and
//!   unified memory has none). [`tier_a_source_caveat`] is the mandatory label, wired
//!   into `StaticInfo::source_caveat` so the TUI/report render it next to the number.
//! - temp / fan / mem-clock / encoder / decoder stay `None` (no public source; SMC keys
//!   are private with per-chip churn — macmon's M4-temps-wrong-class bugs upstream).
//!   Throttle is unobservable here → `None` (§5.4), never an asserted all-false.
//! - Every Tier B/C metric, once live, is additionally stamped with [`SOURCE_CAVEAT`]
//!   (joined onto the Tier-A caveat in `static_info` at unfreeze time).
//!
//! Prior art: macmon (MIT) — the actively-maintained 2026 proof that the sudoless
//! three-tier stack works on current macOS; its `sources.rs` is the model for the IOReport
//! channel taxonomy, unit labels, and residency maths mirrored in [`parse`].

use crate::model::DeviceId;

/// `StaticInfo::process_hint` for every Apple device. Load-bearing copy (design §4.2):
/// macOS users must read "the OS forbids this", not "this app is worse here".
pub const PROCESS_HINT: &str = "per-process GPU data is not available on macOS: the OS \
     does not expose it to third-party apps (powermetrics requires root; Activity Monitor \
     uses private plumbing) — device-level metrics only";

/// Mandatory caveat for every Tier B/C metric once those tiers go live (design §4.3):
/// joined onto [`tier_a_source_caveat`]'s wording the moment either tier ships.
/// Non-negotiable: a silent private-API break must read as a labeled risk, not a lie.
pub const SOURCE_CAVEAT: &str = "read via undocumented macOS interfaces (IOKit \
     PerformanceStatistics / IOReport); may break on macOS updates";

/// Mandatory label for `mem_total_bytes` on Apple devices (design §4.1 Tier A): the value
/// is a budget, not a capacity — presenting it as "total VRAM" would be fabrication.
/// Wired into `StaticInfo::source_caveat` via [`tier_a_source_caveat`].
pub const MEM_TOTAL_CAVEAT: &str = "memory total is a unified-memory working-set budget \
     (Metal recommendedMaxWorkingSetSize) — Apple publishes no total-VRAM figure and \
     unified memory has none";

/// The Tier-A `StaticInfo::source_caveat` for an Apple device, worded per the hardware's
/// own `hasUnifiedMemory` answer (design §4.1): the "unified memory has none" clause is
/// only honest when the device did not answer `false` (Intel-era discrete parts, some
/// paravirt guests) — the wording must follow the hardware's answer, not our assumption
/// about it. Pure, so it unit-tests on every OS.
pub fn tier_a_source_caveat(has_unified_memory: Option<bool>) -> &'static str {
    match has_unified_memory {
        // The device explicitly says its memory is NOT unified: the budget label stands,
        // the unified-memory rationale does not.
        Some(false) => {
            "memory total is a Metal working-set budget (recommendedMaxWorkingSetSize), \
             not a measured VRAM capacity"
        }
        // true, or unanswered on an Apple Silicon part (where unified memory is the
        // architecture): the full label.
        _ => MEM_TOTAL_CAVEAT,
    }
}

/// Stable history identity from the Metal device name: `"Apple M2 Max"` → `apple:m2-max`.
///
/// WHY the name and not Metal's `registryID`: the registryID is per-boot, useless as
/// history identity. The chip name is stable across reboots, and Apple Silicon machines
/// have exactly one GPU, so it cannot collide with itself. The `apple:` prefix is a
/// non-PCI key shape that the collector's `normalize_pci_id` dedupe correctly refuses to
/// merge (design §4.2).
pub fn device_id_for(metal_name: &str) -> DeviceId {
    let mut slug = String::new();
    for c in metal_name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    // Drop the redundant vendor word ("Apple M2 Max" → "m2-max"); `apple:` already says it.
    let slug = slug.strip_prefix("apple-").unwrap_or(slug);
    let slug = if slug.is_empty() { "unknown" } else { slug };
    DeviceId(format!("apple:{slug}"))
}

pub mod parse {
    //! Pure decode/maths for Tiers B and C — zero OS calls, so this layer compiles and
    //! runs its fixture suite on every OS (design §10; CI has no Macs with real SoC
    //! telemetry either way). Matching rules, unit labels, and residency maths mirror
    //! macmon's `sources.rs` (MIT) — cited per the design's prior-art requirement.
    //!
    //! Everything here is written to the same defensive rule as the AMD `gpu_metrics`
    //! decoders: an input that is absent, malformed, or of an unrecognized shape decodes
    //! to `None`/empty — never a guess, never a panic. A macOS update that breaks the
    //! private interfaces must degrade to absent fields, not wrong ones.

    /// One IOReport channel descriptor, as enumerated at runtime (and as serialized in
    /// the `tests/fixtures/ioreport/` files and by `examples/macos_probe.rs` — the two
    /// must stay line-compatible so probe output can be committed verbatim as fixtures).
    ///
    /// Line form: `channel|<group>|<subgroup>|<name>|<unit>` (empty fields allowed; `|`
    /// has never been observed inside a channel/group name — see the fixtures README).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ChannelDesc {
        pub group: String,
        pub subgroup: String,
        pub channel: String,
        pub unit: String,
    }

    impl ChannelDesc {
        pub fn to_line(&self) -> String {
            format!(
                "channel|{}|{}|{}|{}",
                self.group, self.subgroup, self.channel, self.unit
            )
        }

        pub fn from_line(line: &str) -> Option<Self> {
            let rest = line.trim().strip_prefix("channel|")?;
            let mut parts = rest.splitn(4, '|');
            let group = parts.next()?.to_string();
            let subgroup = parts.next()?.to_string();
            let channel = parts.next()?.to_string();
            let unit = parts.next()?.to_string();
            Some(Self {
                group,
                subgroup,
                channel,
                unit,
            })
        }
    }

    /// Parse every `channel|…` line of a fixture/probe dump; other lines (comments,
    /// blanks, `state|…` residency lines) are skipped, so mixed files are fine.
    pub fn parse_channels(text: &str) -> Vec<ChannelDesc> {
        text.lines().filter_map(ChannelDesc::from_line).collect()
    }

    /// Parse `state|<name>|<delta_ticks>` residency lines (the GPUPH fixture form).
    pub fn parse_states(text: &str) -> Vec<(String, u64)> {
        text.lines()
            .filter_map(|l| {
                let rest = l.trim().strip_prefix("state|")?;
                let (name, ticks) = rest.split_once('|')?;
                Some((name.to_string(), ticks.trim().parse().ok()?))
            })
            .collect()
    }

    /// Decode a whitespace/comment-tolerant hex dump (the `voltage-states9-*.hex`
    /// fixture form) into bytes. Malformed input → `None`, never a partial decode.
    pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
        let mut digits = String::new();
        for line in text.lines() {
            let data = line.split('#').next().unwrap_or("");
            digits.extend(data.chars().filter(|c| !c.is_whitespace()));
        }
        if !digits.len().is_multiple_of(2) || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).ok())
            .collect()
    }

    /// Is this the SoC GPU energy channel? Group is exact (`Energy Model` is stable
    /// across every chip macmon supports); the channel name uses *contains*-matching
    /// because Ultra dies prefix it (`DIE_0_GPU Energy`) and per-chip renames are the
    /// documented failure mode — never match by index (design §4.1 Tier C).
    pub fn is_gpu_energy_channel(c: &ChannelDesc) -> bool {
        c.group == "Energy Model" && c.channel.contains("GPU Energy")
    }

    /// Is this the GPU performance-state residency channel (`GPUPH`)? Group/subgroup are
    /// exact per observed inventories; the channel name uses contains-matching for the
    /// same Ultra `DIE_N_` prefix reason as the energy channel.
    pub fn is_gpu_perf_states_channel(c: &ChannelDesc) -> bool {
        c.group == "GPU Stats"
            && c.subgroup == "GPU Performance States"
            && c.channel.contains("GPUPH")
    }

    /// Energy unit as reported by `IOReportChannelGetUnitLabel`. mJ, µJ and nJ have all
    /// been observed across chips/macOS releases (design §4.1) — the unit must come from
    /// the channel itself, per sample, never be assumed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum EnergyUnit {
        Millijoules,
        Microjoules,
        Nanojoules,
    }

    /// Decode a unit label. Unknown labels → `None`: refusing to compute beats guessing
    /// (a mis-assumed unit is a silent 1000× lie — exactly the trust-thesis violation
    /// the AMD `gpu_metrics` per-version decoders exist to prevent).
    pub fn energy_unit(label: &str) -> Option<EnergyUnit> {
        match label.trim() {
            "mJ" => Some(EnergyUnit::Millijoules),
            "uJ" | "µJ" => Some(EnergyUnit::Microjoules),
            "nJ" => Some(EnergyUnit::Nanojoules),
            _ => None,
        }
    }

    /// Power from an interval energy delta (`IOReportCreateSamplesDelta` output — already
    /// a delta, not a cumulative counter): mW = mJ/s. The result is the design's labeled
    /// "SoC rail approximation", not a measured board-power.
    pub fn power_mw_from_energy(delta_energy: u64, unit: EnergyUnit, dt_ms: u64) -> Option<u32> {
        if dt_ms == 0 {
            return None; // a zero-length interval has no rate, and 0 mW would be a lie
        }
        let to_mj = match unit {
            EnergyUnit::Millijoules => 1.0,
            EnergyUnit::Microjoules => 1e-3,
            EnergyUnit::Nanojoules => 1e-6,
        };
        let mw = delta_energy as f64 * to_mj * 1000.0 / dt_ms as f64;
        if !mw.is_finite() || mw < 0.0 {
            return None;
        }
        Some(mw.round().min(f64::from(u32::MAX)) as u32)
    }

    /// Idle-state predicate for GPUPH residency entries. Mirrors macmon: `OFF` plus the
    /// `IDLE`-prefixed states are idle; every other (`P1`…`Pn`) state counts as active.
    /// Unknown future names default to *active* — over-counting idleness would fabricate
    /// an "idle GPU" narration, the worse failure for the event engine.
    pub fn is_idle_state(name: &str) -> bool {
        let n = name.trim();
        n.eq_ignore_ascii_case("off") || n.to_ascii_uppercase().starts_with("IDLE")
    }

    /// Duty-cycle-like utilization from one channel's interval residencies: active/total.
    /// `None` when the interval carries no ticks at all (a blind spot, not 0% — the event
    /// engine treats those differently on purpose).
    pub fn util_pct_from_residency(states: &[(String, u64)]) -> Option<f32> {
        let total: u128 = states.iter().map(|(_, t)| u128::from(*t)).sum();
        if total == 0 {
            return None;
        }
        let active: u128 = states
            .iter()
            .filter(|(n, _)| !is_idle_state(n))
            .map(|(_, t)| u128::from(*t))
            .sum();
        Some((active as f64 / total as f64 * 100.0) as f32)
    }

    /// Residency-weighted DVFS frequency for one channel. The active states (in channel
    /// order) pair index-wise with the DVFS table (ascending, zero rows dropped) — the
    /// pairing macmon uses. A count mismatch returns `None`: pairing by guesswork would
    /// produce a confidently-wrong clock, and per-chip fixtures exist precisely to pin
    /// when an OS/chip changes the state inventory.
    pub fn weighted_freq_mhz(states: &[(String, u64)], dvfs_mhz: &[u32]) -> Option<u32> {
        let active: Vec<u64> = states
            .iter()
            .filter(|(n, _)| !is_idle_state(n))
            .map(|(_, t)| *t)
            .collect();
        if active.is_empty() || active.len() != dvfs_mhz.len() {
            return None;
        }
        let ticks: u128 = active.iter().map(|t| u128::from(*t)).sum();
        if ticks == 0 {
            // The whole interval was spent off/idle: 0 MHz is a measurement, not absence.
            return Some(0);
        }
        let weighted: u128 = active
            .iter()
            .zip(dvfs_mhz)
            .map(|(t, f)| u128::from(*t) * u128::from(*f))
            .sum();
        u32::try_from((weighted + ticks / 2) / ticks).ok()
    }

    /// Decode the IOKit pmgr `voltage-states9` blob (the GPU DVFS table): little-endian
    /// `(u32 frequency, u32 voltage)` rows. Returns frequencies in MHz, ascending as
    /// stored, with zero rows dropped (every observed chip leads with an all-zero "off"
    /// row that has no DVFS meaning).
    pub fn parse_voltage_states(raw: &[u8]) -> Vec<u32> {
        raw.chunks_exact(8)
            .filter_map(|row| {
                let freq_raw = u32::from_le_bytes([row[0], row[1], row[2], row[3]]);
                // row[4..8] is the voltage column — irrelevant to clock reporting.
                if freq_raw == 0 {
                    return None;
                }
                Some(normalize_mhz(freq_raw))
            })
            .collect()
    }

    /// Frequency-unit normalization. WHY: the blob's unit has churned per chip family
    /// (M1-era firmware stores Hz — macmon divides by 1e6; kHz has been observed too),
    /// the same per-version units trap as AMD `gpu_metrics` (C vs centi-C, W vs mW).
    /// Plausible Apple-GPU clocks are ~100..10_000 MHz, so the Hz/kHz/MHz ranges cannot
    /// overlap and the magnitude itself is the discriminator; per-chip fixtures pin it.
    fn normalize_mhz(raw: u32) -> u32 {
        if raw >= 10_000_000 {
            raw / 1_000_000 // Hz
        } else if raw >= 10_000 {
            raw / 1000 // kHz
        } else {
            raw // already MHz
        }
    }

    /// Tier B: the metrics gpuviewer reads from the AGXAccelerator service's
    /// `PerformanceStatistics` dictionary. Every field independently optional — key
    /// inventories differ across chips and macOS releases, and Intel-era keys
    /// (`Temperature(C)` …) must never be assumed present (design §4.1 Tier B).
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct PerfStats {
        /// `Device Utilization %` — duty-cycle-like; same honesty label as NVML's.
        pub device_util_pct: Option<f32>,
        /// `Renderer Utilization %` — UI detail row.
        pub renderer_util_pct: Option<f32>,
        /// `Tiler Utilization %` — UI detail row.
        pub tiler_util_pct: Option<f32>,
        /// `In use system memory` — GPU-mapped system RAM (unified memory), NOT VRAM.
        pub in_use_system_memory_bytes: Option<u64>,
    }

    /// Map raw `PerformanceStatistics` key/value pairs to [`PerfStats`]. Keys are matched
    /// exactly; unknown keys are ignored by construction (open set). Negative or
    /// non-finite values are garbage readings → `None`, never clamped to a fake 0.
    pub fn perf_stats_from_pairs(pairs: &[(String, f64)]) -> PerfStats {
        fn util(v: f64) -> Option<f32> {
            (v.is_finite() && v >= 0.0).then_some(v as f32)
        }
        fn bytes(v: f64) -> Option<u64> {
            (v.is_finite() && v >= 0.0).then_some(v as u64)
        }
        let mut out = PerfStats::default();
        for (key, v) in pairs {
            match key.as_str() {
                // NB: "Device Utilization at cur p-state" is a real key on some chips
                // and deliberately does NOT match — it answers a different question.
                "Device Utilization %" => out.device_util_pct = util(*v),
                "Renderer Utilization %" => out.renderer_util_pct = util(*v),
                "Tiler Utilization %" => out.tiler_util_pct = util(*v),
                // "Alloc system memory" exists too but is allocation, not in-use —
                // reporting it as usage would overstate. Only the in-use key maps.
                "In use system memory" => out.in_use_system_memory_bytes = bytes(*v),
                _ => {}
            }
        }
        out
    }
}

/// Tier A — public Metal static info, the floor that always renders (design §4.1).
#[cfg(all(feature = "apple", target_os = "macos"))]
mod metal {
    use objc2::runtime::NSObjectProtocol;
    use objc2::sel;
    use objc2_metal::MTLDevice as _;

    pub(super) struct MetalInfo {
        pub name: Option<String>,
        pub has_unified_memory: Option<bool>,
        pub working_set_bytes: Option<u64>,
    }

    /// Probe the default Metal device. Every selector is verified with
    /// `respondsToSelector:` before it is called: on GitHub's paravirt runners a missing
    /// selector raises `NSInvalidArgumentException` instead of returning an error
    /// (Godot #101773), and `MTLDevice` methods must not be assumed total — an
    /// unanswered selector is a `None` field, never a crash (design §4.3).
    ///
    /// WHY guards instead of objc2's `exception`-feature `catch`: that feature pulls
    /// `objc2-exception-helper`, which compiles an Objective-C shim at build time —
    /// rejected under the all-Rust dependency rule, and its host-cc build breaks the
    /// Linux-host `cargo check --target aarch64-apple-darwin` gate (decision recorded in
    /// crates/core/Cargo.toml and the design-doc addendum). `respondsToSelector:` guards
    /// the exact documented hazard (missing selectors) in pure Rust.
    pub(super) fn probe() -> Option<MetalInfo> {
        let dev = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let name = dev
            .respondsToSelector(sel!(name))
            .then(|| dev.name().to_string());
        let has_unified_memory = dev
            .respondsToSelector(sel!(hasUnifiedMemory))
            .then(|| dev.hasUnifiedMemory());
        // A zero working-set budget is not a budget — absence, not a 0-byte GPU.
        let working_set_bytes = dev
            .respondsToSelector(sel!(recommendedMaxWorkingSetSize))
            .then(|| dev.recommendedMaxWorkingSetSize())
            .filter(|&b| b > 0);
        Some(MetalInfo {
            name,
            has_unified_memory,
            working_set_bytes,
        })
    }
}

/// Tiers B and C — **gated stub** (design §4.6; see the module header). Returns total
/// absence today so the backend is an honest None-everywhere skeleton.
///
/// Frozen unfreeze checklist (fill-in only — the maths/matching below it already ships,
/// fixture-tested, in [`parse`]):
///
/// - **Tier B** (IOKit, public OS framework — hand-rolled
///   `#[link(name = "IOKit", kind = "framework")]` externs are fine under the
///   no-vendor-SDK rule): `IOServiceMatching("IOAccelerator")` →
///   `IOServiceGetMatchingServices` → `IORegistryEntryCreateCFProperties` →
///   `PerformanceStatistics` CFDictionary → [`parse::perf_stats_from_pairs`].
/// - **Tier C** (private dylib — must go through dlopen2 with `Option<fn>` fields, never
///   a hard link; a missing symbol degrades that metric to `None`, never fails init):
///   `/usr/lib/libIOReport.dylib`, symbol inventory mirroring macmon `sources.rs` (MIT):
///   `IOReportCopyAllChannels`, `IOReportCreateSubscription`, `IOReportCreateSamples`,
///   `IOReportCreateSamplesDelta`, `IOReportChannelGetGroup` / `GetSubGroup` /
///   `GetChannelName` / `GetUnitLabel`, `IOReportStateGetCount` / `GetNameForIndex` /
///   `GetResidency`, `IOReportSimpleGetIntegerValue`. Channels found by enumeration +
///   [`parse::is_gpu_energy_channel`] / [`parse::is_gpu_perf_states_channel`] name
///   matching — never index assumptions. Energy delta/Δt → [`parse::power_mw_from_energy`]
///   with the unit from the channel's own label; GPUPH residencies →
///   [`parse::util_pct_from_residency`] (Tier B fallback) and, with the pmgr
///   `voltage-states9` table via [`parse::parse_voltage_states`],
///   [`parse::weighted_freq_mhz`] → `sm_clock_mhz` (+ table max → `max_sm_clock_mhz`).
/// - Stamp [`super::SOURCE_CAVEAT`] on the device the moment either tier goes live.
#[cfg(all(feature = "apple", target_os = "macos"))]
mod tier_bc {
    pub(super) struct Sample {
        pub util_pct: Option<f32>,
        pub mem_used_bytes: Option<u64>,
        pub power_mw: Option<u32>,
        pub sm_clock_mhz: Option<u32>,
    }

    /// Total absence, by design, until the §4.6 gate clears. This stub is also the
    /// contract for the real tiers: whatever a macOS update breaks must land back here —
    /// per-field `None`, never an error, never a fabricated number.
    pub(super) fn sample() -> Sample {
        Sample {
            util_pct: None,
            mem_used_bytes: None,
            power_mw: None,
            sm_clock_mhz: None,
        }
    }
}

#[cfg(all(feature = "apple", target_os = "macos"))]
mod backend_impl {
    use super::{device_id_for, metal, tier_a_source_caveat, tier_bc, PROCESS_HINT};
    use crate::backend::{BackendError, GpuBackend};
    use crate::model::{now_ms, DeviceId, DynamicSample, ProcessSample, StaticInfo, Vendor};

    /// Device-level Apple Silicon backend. One device — Apple Silicon machines have
    /// exactly one GPU; there is no enumeration loop to get wrong.
    pub struct AppleBackend {
        id: DeviceId,
        name: String,
        mem_total_bytes: Option<u64>,
        has_unified_memory: Option<bool>,
    }

    impl AppleBackend {
        /// Failing init is a normal outcome (headless box without Metal, future lockdown):
        /// the registry logs and skips, and the mock fallback keeps the TUI rendering.
        pub fn init() -> Result<Self, BackendError> {
            let m = metal::probe().ok_or_else(|| {
                BackendError::Unavailable(
                    "no default Metal device (Metal is this backend's floor)".into(),
                )
            })?;
            // A Metal device with an unreadable name still renders — under a generic
            // name, with a per-boot-stable id derived from that same generic name.
            let name = m.name.unwrap_or_else(|| "Apple GPU".to_string());
            Ok(Self {
                id: device_id_for(&name),
                name,
                mem_total_bytes: m.working_set_bytes,
                has_unified_memory: m.has_unified_memory,
            })
        }

        /// Metal's `hasUnifiedMemory` for this device, when readable. Exposed for the
        /// integrator's §5.4 caveat wiring: [`super::MEM_TOTAL_CAVEAT`]'s unified-memory
        /// wording is only honest when this is not `Some(false)` (Intel-era discrete
        /// parts; some paravirt guests) — the wording must follow the hardware's answer,
        /// not our assumption about it.
        pub fn unified_memory(&self) -> Option<bool> {
            self.has_unified_memory
        }

        fn check(&self, dev: &DeviceId) -> Result<(), BackendError> {
            if *dev == self.id {
                Ok(())
            } else {
                Err(BackendError::DeviceNotFound(dev.clone()))
            }
        }
    }

    impl GpuBackend for AppleBackend {
        fn name(&self) -> &'static str {
            "apple"
        }

        fn devices(&mut self) -> Vec<DeviceId> {
            vec![self.id.clone()]
        }

        fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
            self.check(dev)?;
            Ok(StaticInfo {
                id: self.id.clone(),
                vendor: Vendor::Apple,
                name: self.name.clone(),
                backend: "apple".into(),
                // Working-set budget, NOT total VRAM — labeled by source_caveat below,
                // which the TUI/report render next to this number (§5.4).
                mem_total_bytes: self.mem_total_bytes,
                power_limit_mw: None,
                // Tier C's DVFS table max — None until the §4.6 gate clears.
                max_sm_clock_mhz: None,
                // Apple exposes no public slowdown threshold (and no temperature).
                temp_slowdown_c: None,
                driver_version: None,
                process_hint: Some(PROCESS_HINT.to_string()),
                // The §4.1 mandatory mem-total label, worded per the hardware's own
                // unified-memory answer. SOURCE_CAVEAT (private interfaces) joins this
                // the moment Tier B/C goes live (§4.3).
                source_caveat: Some(tier_a_source_caveat(self.has_unified_memory).to_string()),
            })
        }

        fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
            self.check(dev)?;
            // Tier B/C live behind the WWDC26 gate; until then this is the honest
            // None-everywhere skeleton (design §4.6).
            let t = tier_bc::sample();
            Ok(DynamicSample {
                ts_ms: now_ms(),
                util_pct: t.util_pct,
                util_engine: None, // Tier B util, once live, is device-wide — no headline engine
                mem_used_bytes: t.mem_used_bytes,
                power_mw: t.power_mw,
                temp_c: None, // no public source; SMC keys private + per-chip churn (§4.4)
                fan_pct: None, // often fanless; same SMC story (§4.4)
                sm_clock_mhz: t.sm_clock_mhz,
                mem_clock_mhz: None, // unified memory: no separately clocked VRAM to report
                encoder_pct: None,
                decoder_pct: None,
                // Throttle is UNOBSERVABLE here — `None` is the §5.4 spelling of that.
                // An all-false struct would read as a fact-grade "not throttling": a
                // fabricated negative this source has no basis to assert.
                throttle: None,
            })
        }

        fn refresh_processes(
            &mut self,
            dev: &DeviceId,
        ) -> Result<Vec<ProcessSample>, BackendError> {
            self.check(dev)?;
            // Empty BY OS PROHIBITION, not by failure — PROCESS_HINT (in static_info)
            // is the explanation the UI must show instead of an empty pane (§4.2).
            Ok(Vec::new())
        }
    }
}

#[cfg(all(feature = "apple", target_os = "macos"))]
pub use backend_impl::AppleBackend;

#[cfg(test)]
mod tests {
    use super::parse::*;
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/ioreport/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
    }

    #[test]
    fn device_id_is_stable_slug_of_chip_name() {
        assert_eq!(device_id_for("Apple M2 Max").0, "apple:m2-max");
        assert_eq!(device_id_for("Apple M1").0, "apple:m1");
        // GitHub's paravirt runner device name (design §4.5) — must not collide shapes.
        assert_eq!(
            device_id_for("Apple Paravirtual device").0,
            "apple:paravirtual-device"
        );
        // Degenerate names still produce a usable, prefixed id.
        assert_eq!(device_id_for("").0, "apple:unknown");
        assert_eq!(device_id_for("  --  ").0, "apple:unknown");
        // The apple: shape must never look like a PCI BDF (dedupe refusal relies on it).
        assert!(
            !device_id_for("Apple M3 Pro").0.contains(':')
                || device_id_for("Apple M3 Pro").0.starts_with("apple:")
        );
    }

    #[test]
    fn process_hint_blames_the_os_not_the_app() {
        // Load-bearing copy (§4.2): the user must read "the OS forbids this".
        assert!(PROCESS_HINT.contains("macOS"));
        assert!(PROCESS_HINT.contains("does not expose"));
        assert!(PROCESS_HINT.contains("device-level"));
    }

    #[test]
    fn tier_a_caveat_follows_the_hardwares_unified_memory_answer() {
        // Unified (or unanswered): the full label — budget, not VRAM, and why.
        for unified in [Some(true), None] {
            let c = tier_a_source_caveat(unified);
            assert!(c.contains("working-set budget"), "{c}");
            assert!(c.contains("unified memory"), "{c}");
        }
        // The hardware explicitly answered "not unified": the budget label stands but
        // the unified-memory rationale must not — the wording follows the hardware's
        // answer, never our assumption about it.
        let c = tier_a_source_caveat(Some(false));
        assert!(c.contains("working-set budget"), "{c}");
        assert!(!c.contains("unified"), "{c}");
    }

    #[test]
    fn energy_unit_labels_decode_and_unknown_unit_is_none() {
        assert_eq!(energy_unit("mJ"), Some(EnergyUnit::Millijoules));
        assert_eq!(energy_unit(" uJ "), Some(EnergyUnit::Microjoules));
        assert_eq!(energy_unit("µJ"), Some(EnergyUnit::Microjoules));
        assert_eq!(energy_unit("nJ"), Some(EnergyUnit::Nanojoules));
        // Unknown unit: refuse, don't guess (a wrong unit is a silent 1000× lie).
        assert_eq!(energy_unit("J"), None);
        assert_eq!(energy_unit("mW"), None);
        assert_eq!(energy_unit(""), None);
    }

    #[test]
    fn power_math_converts_mj_uj_nj_to_milliwatts() {
        // 5000 mJ over 1 s = 5 W = 5000 mW — same answer through every unit label.
        assert_eq!(
            power_mw_from_energy(5_000, EnergyUnit::Millijoules, 1_000),
            Some(5_000)
        );
        assert_eq!(
            power_mw_from_energy(5_000_000, EnergyUnit::Microjoules, 1_000),
            Some(5_000)
        );
        assert_eq!(
            power_mw_from_energy(5_000_000_000, EnergyUnit::Nanojoules, 1_000),
            Some(5_000)
        );
        // Sub-watt readings keep precision through the f64 path.
        assert_eq!(
            power_mw_from_energy(250, EnergyUnit::Millijoules, 2_000),
            Some(125)
        );
    }

    #[test]
    fn power_math_refuses_zero_interval_and_survives_huge_deltas() {
        assert_eq!(
            power_mw_from_energy(5_000, EnergyUnit::Millijoules, 0),
            None
        );
        // A garbage-huge delta saturates instead of wrapping or panicking.
        assert_eq!(
            power_mw_from_energy(u64::MAX, EnergyUnit::Millijoules, 1),
            Some(u32::MAX)
        );
        assert_eq!(
            power_mw_from_energy(0, EnergyUnit::Nanojoules, 1_000),
            Some(0)
        );
    }

    #[test]
    fn gpu_energy_channel_matching_handles_die_prefixes_and_skips_decoys() {
        let single = parse_channels(&fixture("channels-m2.txt"));
        let gpu: Vec<&ChannelDesc> = single.iter().filter(|c| is_gpu_energy_channel(c)).collect();
        assert_eq!(
            gpu.len(),
            1,
            "exactly one GPU energy channel on a single die"
        );
        assert_eq!(gpu[0].channel, "GPU Energy");
        assert_eq!(energy_unit(&gpu[0].unit), Some(EnergyUnit::Millijoules));

        // Ultra: two dies, DIE_N_ prefixes — contains-matching finds both.
        let ultra = parse_channels(&fixture("channels-m2-ultra.txt"));
        let dies: Vec<&str> = ultra
            .iter()
            .filter(|c| is_gpu_energy_channel(c))
            .map(|c| c.channel.as_str())
            .collect();
        assert_eq!(dies, vec!["DIE_0_GPU Energy", "DIE_1_GPU Energy"]);

        // A future chip reporting nJ decodes by its own label, never by assumption.
        let nj = parse_channels(&fixture("channels-m4-nj.txt"));
        let gpu_nj: Vec<&ChannelDesc> = nj.iter().filter(|c| is_gpu_energy_channel(c)).collect();
        assert_eq!(gpu_nj.len(), 1);
        assert_eq!(energy_unit(&gpu_nj[0].unit), Some(EnergyUnit::Nanojoules));
    }

    /// The one REAL capture in the fixture set (macos-15 CI guest, design §4.5). Its job
    /// is to keep the Tier C selectors honest against a machine nobody hand-wrote: on the
    /// paravirt GPU neither selector matches anything, so None is the CORRECT reading
    /// there, not a parser bug. If a future runner image starts exposing these channels
    /// this test fails loudly — which is the signal to tighten the CI assertions to match
    /// reality rather than keep asserting None (§4.5's explicit instruction).
    #[test]
    fn real_paravirt_capture_exposes_no_tier_c_gpu_channels() {
        let chans = parse_channels(&fixture("channels-paravirt-macos15.txt"));
        assert_eq!(chans.len(), 116, "verbatim capture: 116 channels");

        let energy: Vec<&str> = chans
            .iter()
            .filter(|c| is_gpu_energy_channel(c))
            .map(|c| c.channel.as_str())
            .collect();
        assert!(
            energy.is_empty(),
            "paravirt guest exposes no GPU energy channel, got {energy:?}"
        );

        let ph: Vec<&str> = chans
            .iter()
            .filter(|c| is_gpu_perf_states_channel(c))
            .map(|c| c.channel.as_str())
            .collect();
        assert!(
            ph.is_empty(),
            "paravirt guest exposes no GPUPH residency channel, got {ph:?}"
        );

        // Tier B is a different story and must not be lumped in with Tier C: the
        // Internal Statistics group IS present, and it is where macOS memory comes from.
        let internal: Vec<&str> = chans
            .iter()
            .filter(|c| c.group == "Internal Statistics")
            .map(|c| c.channel.as_str())
            .collect();
        assert!(
            internal.contains(&"In use system memory"),
            "Tier B memory channel is present on the guest, got {internal:?}"
        );
    }

    #[test]
    fn gpuph_channel_requires_exact_group_and_subgroup() {
        let chans = parse_channels(&fixture("channels-m2.txt"));
        let ph: Vec<&ChannelDesc> = chans
            .iter()
            .filter(|c| is_gpu_perf_states_channel(c))
            .collect();
        // The fixture carries a decoy GPUPH under the wrong subgroup — exactly one match.
        assert_eq!(ph.len(), 1);
        assert_eq!(ph[0].subgroup, "GPU Performance States");

        let ultra = parse_channels(&fixture("channels-m2-ultra.txt"));
        let dies: Vec<&str> = ultra
            .iter()
            .filter(|c| is_gpu_perf_states_channel(c))
            .map(|c| c.channel.as_str())
            .collect();
        assert_eq!(dies, vec!["DIE_0_GPUPH", "DIE_1_GPUPH"]);
    }

    #[test]
    fn residency_math_computes_active_fraction_from_fixture() {
        let states = parse_states(&fixture("gpuph-m2.txt"));
        assert_eq!(states.len(), 6, "fixture carries OFF + five P-states");
        // OFF=600k of 1M total ticks → 40% active.
        let util = util_pct_from_residency(&states).expect("fixture has ticks");
        assert!((util - 40.0).abs() < 0.01, "got {util}");
    }

    #[test]
    fn residency_math_is_none_when_total_is_zero() {
        // No ticks at all is a blind spot, not 0% — None, never a fabricated idle GPU.
        let empty: Vec<(String, u64)> = vec![];
        assert_eq!(util_pct_from_residency(&empty), None);
        let zeros = vec![("OFF".to_string(), 0), ("P1".to_string(), 0)];
        assert_eq!(util_pct_from_residency(&zeros), None);
    }

    #[test]
    fn idle_state_names_match_macmon_inventory() {
        assert!(is_idle_state("OFF"));
        assert!(is_idle_state("off"));
        assert!(is_idle_state("IDLE"));
        assert!(is_idle_state("IDLE2"));
        assert!(!is_idle_state("P1"));
        // Unknown future names count as ACTIVE: fabricating idleness is the worse error.
        assert!(!is_idle_state("TURBO"));
    }

    #[test]
    fn weighted_freq_weights_active_states_from_fixtures() {
        let states = parse_states(&fixture("gpuph-m2.txt"));
        let raw = parse_hex(&fixture("voltage-states9-m2.hex")).expect("valid hex fixture");
        let dvfs = parse_voltage_states(&raw);
        // The fixture blob stores Hz and leads with an all-zero row — both normalized away.
        assert_eq!(dvfs, vec![444, 612, 808, 1064, 1398]);
        // Hand-computed: Σ(ticks·MHz)/Σticks = 319_300_000 / 400_000 ≈ 798.25 → 798.
        assert_eq!(weighted_freq_mhz(&states, &dvfs), Some(798));
    }

    #[test]
    fn weighted_freq_refuses_dvfs_state_count_mismatch() {
        let states = parse_states(&fixture("gpuph-m2.txt"));
        // 4-entry table against 5 active states: pairing by guesswork is forbidden.
        assert_eq!(weighted_freq_mhz(&states, &[444, 612, 808, 1064]), None);
        assert_eq!(weighted_freq_mhz(&states, &[]), None);
        assert_eq!(weighted_freq_mhz(&[], &[444]), None);
    }

    #[test]
    fn weighted_freq_is_zero_when_gpu_measured_fully_idle() {
        // All ticks on OFF, P-states present-but-zero: 0 MHz is a measurement.
        let states = vec![
            ("OFF".to_string(), 1_000_000u64),
            ("P1".to_string(), 0),
            ("P2".to_string(), 0),
        ];
        assert_eq!(weighted_freq_mhz(&states, &[444, 612]), Some(0));
    }

    #[test]
    fn voltage_states9_decoder_normalizes_hz_khz_and_drops_zero_rows() {
        // Hz fixture covered in weighted_freq test; this one is the synthetic kHz/MHz mix.
        let raw = parse_hex(&fixture("voltage-states9-khz-synthetic.hex")).expect("valid hex");
        assert_eq!(parse_voltage_states(&raw), vec![389, 722, 998]);
        // Truncated trailing bytes are ignored, not misread.
        assert_eq!(parse_voltage_states(&[0x01, 0x02, 0x03]), Vec::<u32>::new());
        // Malformed hex text refuses to decode at all.
        assert_eq!(parse_hex("zz"), None);
        assert_eq!(parse_hex("abc"), None); // odd digit count
    }

    #[test]
    fn perf_stats_every_key_optional_and_decoy_keys_ignored() {
        // Empty dictionary (paravirt guest, future lockdown): total absence, no error.
        assert_eq!(perf_stats_from_pairs(&[]), PerfStats::default());

        let pairs = vec![
            ("Device Utilization %".to_string(), 37.0),
            // Decoy: real key on some chips, answers a different question — must not map.
            ("Device Utilization at cur p-state".to_string(), 99.0),
            ("In use system memory".to_string(), 3_221_225_472.0),
            // Decoy: allocation is not usage — mapping it would overstate.
            ("Alloc system memory".to_string(), 9_999_999_999.0),
            // Intel-era key that must never be assumed (or matched) on Apple Silicon.
            ("Temperature(C)".to_string(), 55.0),
        ];
        let stats = perf_stats_from_pairs(&pairs);
        assert_eq!(stats.device_util_pct, Some(37.0));
        assert_eq!(stats.renderer_util_pct, None, "absent key stays None");
        assert_eq!(stats.tiler_util_pct, None);
        assert_eq!(stats.in_use_system_memory_bytes, Some(3 << 30));
    }

    #[test]
    fn perf_stats_negative_values_are_garbage_not_zero() {
        let pairs = vec![
            ("Device Utilization %".to_string(), -1.0),
            ("In use system memory".to_string(), -4096.0),
            ("Renderer Utilization %".to_string(), f64::NAN),
        ];
        let stats = perf_stats_from_pairs(&pairs);
        // A negative reading is a broken interface, not a 0% GPU — None, never clamped.
        assert_eq!(stats.device_util_pct, None);
        assert_eq!(stats.in_use_system_memory_bytes, None);
        assert_eq!(stats.renderer_util_pct, None);
    }

    #[test]
    fn channel_fixture_lines_roundtrip() {
        // The probe emits to_line(); fixtures are committed verbatim; from_line reads
        // them back — the three must agree or probe output can't become fixtures.
        let c = ChannelDesc {
            group: "Energy Model".into(),
            subgroup: String::new(),
            channel: "DIE_0_GPU Energy".into(),
            unit: "mJ".into(),
        };
        assert_eq!(ChannelDesc::from_line(&c.to_line()), Some(c.clone()));
        // Non-channel lines (comments, residency lines, junk) parse to nothing.
        assert_eq!(ChannelDesc::from_line("# comment"), None);
        assert_eq!(ChannelDesc::from_line("state|OFF|123"), None);
        assert_eq!(ChannelDesc::from_line("channel|too|few"), None);
    }

    /// macOS-only smoke (design §4.5): must hold on a paravirt runner exactly as on real
    /// hardware. Asserts presence/shape only — NEVER real-hardware telemetry values.
    /// The Tier B/C absence assertions are safe today because the tiers are stubbed
    /// (§4.6 gate), independent of what the guest exposes.
    #[cfg(all(feature = "apple", target_os = "macos"))]
    #[test]
    fn apple_backend_smoke_device_level_only() {
        use crate::backend::GpuBackend;
        let Ok(mut b) = AppleBackend::init() else {
            // No Metal device (headless CI oddity): absence is a normal outcome — the
            // registry would skip to the mock. Nothing further to assert.
            return;
        };
        let devs = b.devices();
        assert_eq!(devs.len(), 1, "Apple Silicon has exactly one GPU");
        assert!(devs[0].0.starts_with("apple:"));

        let info = b.static_info(&devs[0]).expect("static info for own device");
        assert!(!info.name.is_empty(), "paravirt device still has a name");
        assert_eq!(
            info.process_hint.as_deref(),
            Some(PROCESS_HINT),
            "the OS-prohibition explainer must always ship"
        );
        assert_eq!(
            info.source_caveat.as_deref(),
            Some(tier_a_source_caveat(b.unified_memory())),
            "the mem-total budget label must always ship (§4.1)"
        );

        let s = b.refresh_dynamic(&devs[0]).expect("dynamic refresh");
        assert!(s.ts_ms > 0);
        // Gated tiers: absence is guaranteed by the stub, not by the hardware.
        assert!(s.util_pct.is_none());
        assert!(s.mem_used_bytes.is_none());
        assert!(s.power_mw.is_none());
        assert!(s.sm_clock_mhz.is_none());
        assert!(s.temp_c.is_none() && s.fan_pct.is_none());
        // Throttle is unobservable on this source: None, never an asserted all-false.
        assert_eq!(s.throttle, None);

        let procs = b.refresh_processes(&devs[0]).expect("process refresh");
        assert!(procs.is_empty(), "per-process is OS-prohibited on macOS");

        // Wrong device id is DeviceNotFound, not a panic or a wrong device's data.
        let bogus = crate::model::DeviceId("apple:not-this-one".into());
        assert!(b.static_info(&bogus).is_err());
    }
}
