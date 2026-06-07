//! Windows cross-vendor WDDM backend (docs/design/cross-platform.md §3) — AMD and Intel
//! on Windows (and NVIDIA when NVML is absent), entirely from OS surfaces: DXGI for
//! enumeration and VRAM totals, PDH GPU counters for utilization and memory, D3DKMT for
//! the LUID→PCI identity. `pdh.dll`/`gdi32.dll`/`dxgi.dll` are OS system libraries —
//! linking them via the `windows` crate does NOT violate the no-vendor-SDK rule (that rule
//! targets vendor SDKs with soname churn, not the OS itself).
//!
//! Honesty contract, per metric (§3.4 — these are load-bearing semantics, not trivia):
//!
//! - **util_pct is scheduler duty-cycle, not capacity.** The PDH `GPU Engine` counters
//!   report how busy the WDDM scheduler (VidSch) kept each engine — time with work
//!   resident, exactly the same *kind* of number as NVML's duty-cycle "utilization", and
//!   it must be labeled exactly as honestly: a GPU can read 100% here while its execution
//!   units idle. The device headline is the **busiest single engine** (summed across pids
//!   per engine first), which is what Task Manager's Performance tab shows — comparable on
//!   purpose, so users cross-checking against Task Manager see agreement, not a mystery.
//! - **mem_used_bytes comes from `GPU Adapter Memory\Dedicated Usage`** — the VidMm
//!   (Windows video memory manager) number, the adapter-level counter Microsoft confirms
//!   stays correct (KB4490156). DXGI's `QueryVideoMemoryInfo` is **never** used for
//!   device-used: it reports the *calling process's* budget/usage by design and would show
//!   gpuviewer's own ~0 — a silently-wrong number, the worst kind.
//! - **temp_c, power_mw, fan_pct, sm_clock_mhz, mem_clock_mhz are `None` on this
//!   backend** — Windows exposes no public temperature/power/clock API for AMD/Intel GPUs
//!   without installing vendor SDKs (ADLX/IGCL), which the no-vendor-SDK rule forbids.
//!   The UI renders these as unavailable; fabricating them is not an option. The two
//!   known semi-public paths are explicitly out of scope per §3.6: D3DKMT
//!   `KMTQAITYPE_ADAPTERPERFDATA` (driver-optional kernel thunk, deci-°C, power in 0.1%
//!   units) is a future opportunistic probe, and DXCore's QueryState telemetry is still
//!   prerelease. Do not "fix" these in.
//! - **throttle is `None`**: this source cannot observe throttling at all, and the §5.4
//!   `Option<ThrottleReasons>` model makes that unobservability representable — `None`
//!   means "unobserved", and the event engine/UI/rollups treat it as a blind spot. The
//!   all-false struct (a fabricated fact-grade "not throttling") is never emitted here.
//! - **Per-process rows** come from PDH `GPU Engine` / `GPU Process Memory` instances
//!   joined by (pid, LUID). `kind` is `Unknown` — PDH does not distinguish
//!   compute/graphics; an engtype of `Compute`/`Cuda` upgrades to `Compute` as a labeled
//!   heuristic only. `cpu_pct`/`container` are `None` (no `/proc` on Windows).
//! - **Absence is normal.** A GPU-less machine (CI runner, RDP session without vGPU) has
//!   no `GPU Engine` PDH object at all (`PDH_CSTATUS_NO_OBJECT`) and possibly no hardware
//!   DXGI adapter — every such outcome is `None`/empty/skipped-backend, never an error.
//!   The first PDH collection legitimately yields no rate data (rate counters need two
//!   collections) — the first frame is honestly empty.
//!
//! Identity (§3.1): the in-session join key is the adapter **LUID** (matches PDH instance
//! tokens and D3DKMT). A LUID is session-scoped — it changes on reboot/driver update — so
//! it is **never persisted as identity**. The persistent `DeviceId` is the normalized PCI
//! BDF (`"0000:bb:dd.f"`) obtained via D3DKMT `ADAPTERADDRESS`, the same key shape NVML
//! and sysfs produce, so history identity works and the collector's first-wins PCI dedupe
//! lets NVML claim NVIDIA boards ahead of this backend (registry order nvidia → wddm,
//! §3.7). If the D3DKMT thunk fails, the fallback id `wddm:<vendor>:<device>:<ordinal>`
//! deliberately does NOT parse as a PCI address, so `normalize_pci_id` refuses to dedupe
//! it: listing a device twice beats wrongly merging two.
//!
//! Layout: the [`pdh`] and [`adapters`] submodules carry the §9 interface-freeze surface
//! (`pdh::shared()`, `SharedPdh::snapshot`, `parse_instance`, `adapters::enumerate`); the
//! integrator may re-export them as `win::pdh`/`win::adapters` or split them into files
//! later. Everything that touches a Windows API is `#[cfg(target_os = "windows")]`; the
//! counter-instance grammar and all aggregation math are pure functions that compile and
//! unit-test on every OS (CI has no GPUs — the Linux leg runs those tests from string
//! fixtures).

/// PDH GPU counters: instance-name grammar, aggregation math, and (on Windows) the shared
/// process-wide query both Windows backends read from.
pub mod pdh {
    use std::collections::{hash_map::Entry, HashMap};

    // ---- PDH status codes (§3.2 absence-is-normal table) -------------------------------
    //
    // Defined here (hex from pdhmsg.h) rather than imported so the classification is a
    // pure, any-OS-testable contract: future refactors must not turn these into errors.

    /// `ERROR_SUCCESS` — and `PDH_CSTATUS_VALID_DATA`, which shares the value 0.
    pub const PDH_OK: u32 = 0;
    /// `PDH_CSTATUS_NEW_DATA`: valid value, instance appeared since the previous collect.
    pub const PDH_CSTATUS_NEW_DATA: u32 = 0x1;
    /// No such performance object — exactly what a GPU-less CI runner reports for
    /// `GPU Engine` (no WDDM 2.0 GPU/driver in the session).
    pub const PDH_CSTATUS_NO_OBJECT: u32 = 0xC000_0BB8;
    /// Object exists but the counter does not (older WDDM driver).
    pub const PDH_CSTATUS_NO_COUNTER: u32 = 0xC000_0BB9;
    /// Counter exists but its value could not be validated (first-sample case per item).
    pub const PDH_CSTATUS_INVALID_DATA: u32 = 0xC000_0BBA;
    /// No instances right now (e.g. no process currently touches the GPU). Normal.
    pub const PDH_CSTATUS_NO_INSTANCE: u32 = 0x8000_07D1;
    /// Buffer too small — the "call again with a bigger buffer" half of the two-call
    /// pattern, not a failure.
    pub const PDH_MORE_DATA: u32 = 0x8000_07D2;
    /// The query has no data yet (first collection of rate counters). Normal.
    pub const PDH_NO_DATA: u32 = 0x8000_07D5;
    /// Query-level first-sample/invalid-data case. Normal.
    pub const PDH_INVALID_DATA: u32 = 0xC000_0BC6;
    /// The perf-data provider timed out — a transient miss, NOT a lost device.
    pub const PDH_QUERY_PERF_DATA_TIMEOUT: u32 = 0xC000_0BFE;

    /// The §3.2 contract: each of these maps to `None` (plus at most one collector
    /// self-honesty event, emitted upstream), never an `Err`. A refactor that starts
    /// treating any of them as a failure breaks the GPU-less-CI-runner path — that is the
    /// case the any-OS unit test pins.
    pub fn status_is_normal_absence(status: u32) -> bool {
        matches!(
            status,
            PDH_CSTATUS_NO_OBJECT
                | PDH_CSTATUS_NO_COUNTER
                | PDH_CSTATUS_NO_INSTANCE
                | PDH_NO_DATA
                | PDH_CSTATUS_INVALID_DATA
                | PDH_INVALID_DATA
                | PDH_QUERY_PERF_DATA_TIMEOUT
        )
    }

    /// Per-item `CStatus` gate: only VALID/NEW values are trusted (§3.2 "per-item CStatus
    /// checked before trusting any value"). Anything else means *this item* is `None` this
    /// tick — not the whole snapshot.
    pub fn item_value_is_trustworthy(cstatus: u32) -> bool {
        cstatus == PDH_OK || cstatus == PDH_CSTATUS_NEW_DATA
    }

    // ---- Counter-instance grammar (pure — fixture-tested on any OS) --------------------

    /// The two hex DWORDs of an instance-name `luid` token, **in printed order**.
    ///
    /// WHY not "high, low": the HighPart-then-LowPart order is inferred from observation,
    /// not documented anywhere by Microsoft. Matching therefore verifies BOTH parts
    /// against an enumerated adapter LUID (either order) and treats no-match as
    /// "unattributed" (§3.2) — a wrong attribution is worse than an honest gap.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct InstanceLuid(pub u32, pub u32);

    impl InstanceLuid {
        /// Does this instance belong to the adapter with (`HighPart`, `LowPart`)?
        /// Accepts the pair in either order — see the type-level WHY. A mirrored
        /// collision between two real adapters would require LUIDs (x, y) and (y, x)
        /// alive in one session; the kernel allocates LowPart monotonically with
        /// HighPart almost always 0, so the ambiguity is theoretical.
        pub fn matches(&self, high: i32, low: u32) -> bool {
            let h = high as u32;
            (self.0 == h && self.1 == low) || (self.1 == h && self.0 == low)
        }
    }

    /// One parsed PDH GPU counter-instance name. Every field optional: the three counter
    /// families share one grammar but carry different token subsets
    /// (`GPU Engine` = pid+luid+phys+eng+engtype, `GPU Process Memory` = pid+luid+phys,
    /// `GPU Adapter Memory` = luid+phys).
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct ParsedInstance {
        pub pid: Option<u32>,
        pub luid: Option<InstanceLuid>,
        pub phys: Option<u32>,
        pub part: Option<u32>,
        pub eng: Option<u32>,
        /// Engine type — an **opaque string from an open set**, never an exhaustive enum:
        /// drivers and HAGS rename/add engtypes across releases (§3.2), so any
        /// match against it is by name and tolerant of strangers.
        pub engtype: Option<String>,
    }

    /// Parse one hex DWORD token (`0x0000C739`). The `0x` prefix is mandatory — a token
    /// without it is not a luid part, and accepting it would let a malformed name parse
    /// into a wrong (never-matching, but noise-generating) LUID.
    fn parse_hex_dword(tok: &str) -> Option<u32> {
        let hex = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"))?;
        if hex.is_empty() || hex.len() > 8 {
            return None;
        }
        u32::from_str_radix(hex, 16).ok()
    }

    /// Parse a PDH GPU counter-instance name, e.g.
    /// `pid_1234_luid_0x00000000_0x0000C739_phys_0_eng_3_engtype_3D`.
    ///
    /// Grammar (mirrors windows_exporter's production parser, §3.2): split on `_`;
    /// keyword tokens consume their values — `pid`/`phys`/`part`/`eng` one decimal,
    /// `luid` two hex DWORDs, `engtype` the **rest of the string** (underscores
    /// preserved — the engtype set is open and future names may contain them). The
    /// grammar is keyword-driven, not positional, so token reordering still parses.
    /// Anything malformed → `None`: an instance we cannot read is an instance we must
    /// not guess about (it counts as unattributed, same as a LUID mismatch).
    pub fn parse_instance(name: &str) -> Option<ParsedInstance> {
        let mut out = ParsedInstance::default();
        let mut toks = name.split('_');
        while let Some(tok) = toks.next() {
            match tok {
                "pid" => out.pid = Some(toks.next()?.parse().ok()?),
                "luid" => {
                    let a = parse_hex_dword(toks.next()?)?;
                    let b = parse_hex_dword(toks.next()?)?;
                    out.luid = Some(InstanceLuid(a, b));
                }
                "phys" => out.phys = Some(toks.next()?.parse().ok()?),
                "part" => out.part = Some(toks.next()?.parse().ok()?),
                "eng" => out.eng = Some(toks.next()?.parse().ok()?),
                "engtype" => {
                    let rest = toks.collect::<Vec<_>>().join("_");
                    if rest.is_empty() {
                        return None;
                    }
                    out.engtype = Some(rest);
                    break;
                }
                // Unknown keyword (or a non-GPU instance name entirely): refuse to guess.
                _ => return None,
            }
        }
        // A name of nothing but separators ("___") would fold to the all-None default;
        // that is not a GPU counter instance.
        if out == ParsedInstance::default() {
            None
        } else {
            Some(out)
        }
    }

    // ---- Aggregation math (pure — scripted-snapshot-tested on any OS) ------------------

    /// One adapter's busiest engine: the device-headline utilization (§3.4).
    #[derive(Clone, Debug, PartialEq)]
    pub struct EngineHeadline {
        /// Name of the busiest engine (opaque engtype, surfaced in the UI so the headline
        /// is self-explaining: "3D 97%" reads differently from "Copy 97%").
        pub engtype: String,
        /// Busy % of that engine. NOT clamped to 100: reads use `PDH_FMT_NOCAP100`
        /// (mandatory — silent capping would make summed numbers quietly wrong) and
        /// sampling skew can push a sum slightly past 100. Honest > tidy.
        pub pct: f64,
    }

    /// Engine identity within one adapter: (phys, part, eng, engtype).
    type EngineKey = (Option<u32>, Option<u32>, Option<u32>, String);

    /// Per-engine busy% for one adapter: instances filtered to the adapter's LUID, then
    /// summed per engine — keyed by (phys, part, eng, engtype) — **across pids** (§3.4).
    /// Instances whose LUID does not match (or did not parse) are unattributed: skipped.
    fn engine_busy(
        engine_util: &[(ParsedInstance, f64)],
        high: i32,
        low: u32,
    ) -> HashMap<EngineKey, f64> {
        let mut per_engine = HashMap::new();
        for (inst, v) in engine_util {
            if !inst.luid.is_some_and(|l| l.matches(high, low)) {
                continue;
            }
            let key = (
                inst.phys,
                inst.part,
                inst.eng,
                inst.engtype.clone().unwrap_or_default(),
            );
            *per_engine.entry(key).or_insert(0.0) += v;
        }
        per_engine
    }

    /// Device-headline utilization for one adapter: the **busiest single engine** after
    /// per-engine pid-summing — deliberately Task-Manager-Performance-tab-comparable
    /// (§3.4). `None` when no instance matched: GPU-less runner, first PDH sample, or an
    /// unmatched LUID — all normal.
    pub fn device_util(
        engine_util: &[(ParsedInstance, f64)],
        high: i32,
        low: u32,
    ) -> Option<EngineHeadline> {
        engine_busy(engine_util, high, low)
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|((_, _, _, engtype), pct)| EngineHeadline { engtype, pct })
    }

    /// Busy% of the busiest engine of one engtype (e.g. `VideoEncode`/`VideoDecode` for
    /// the encoder/decoder fields), after per-engine pid-summing. Name comparison is
    /// case-insensitive (the engtype set is open; drivers vary casing). Absent engtype →
    /// `None` (§3.4) — never 0, which would claim an idle encoder this GPU may not have.
    pub fn engtype_util(
        engine_util: &[(ParsedInstance, f64)],
        high: i32,
        low: u32,
        engtype: &str,
    ) -> Option<f64> {
        engine_busy(engine_util, high, low)
            .into_iter()
            .filter(|((_, _, _, ty), _)| ty.eq_ignore_ascii_case(engtype))
            .map(|(_, v)| v)
            .max_by(f64::total_cmp)
    }

    /// One pid's utilization on one adapter (§2.4/§3.5).
    #[derive(Clone, Debug, PartialEq)]
    pub struct PidUtil {
        /// Max across the pid's engine instances — the Task-Manager-comparable per-process
        /// number. Any *summed* per-engtype figure a UI shows instead must be labeled
        /// "engine-sum, can exceed 100%".
        pub pct: f64,
        /// Which engine was busiest (UI tooltip/evidence — names the claim's source).
        pub busiest_engtype: String,
        /// True if any of the pid's engines was named `Compute`/`Cuda` — a **heuristic
        /// only** (§3.5): PDH does not distinguish compute from graphics clients.
        pub compute_hint: bool,
    }

    /// Per-pid utilization on one adapter: max across each pid's engine instances on this
    /// LUID. Unattributed instances (LUID mismatch/unparsed) are skipped.
    pub fn per_pid_util(
        engine_util: &[(ParsedInstance, f64)],
        high: i32,
        low: u32,
    ) -> HashMap<u32, PidUtil> {
        let mut out: HashMap<u32, PidUtil> = HashMap::new();
        for (inst, v) in engine_util {
            if !inst.luid.is_some_and(|l| l.matches(high, low)) {
                continue;
            }
            let Some(pid) = inst.pid else { continue };
            let engtype = inst.engtype.clone().unwrap_or_default();
            let compute =
                engtype.eq_ignore_ascii_case("compute") || engtype.eq_ignore_ascii_case("cuda");
            match out.entry(pid) {
                Entry::Occupied(mut o) => {
                    let e = o.get_mut();
                    if *v > e.pct {
                        e.pct = *v;
                        e.busiest_engtype = engtype;
                    }
                    e.compute_hint |= compute;
                }
                Entry::Vacant(slot) => {
                    slot.insert(PidUtil {
                        pct: *v,
                        busiest_engtype: engtype,
                        compute_hint: compute,
                    });
                }
            }
        }
        out
    }

    /// Per-pid byte totals (for the `GPU Process Memory` Dedicated/Shared Usage streams)
    /// on one adapter, summed across a pid's phys/part instances. Values are raw counter
    /// doubles; negatives (a provider glitch) clamp to 0 rather than wrap.
    pub fn per_pid_bytes(
        readings: &[(ParsedInstance, f64)],
        high: i32,
        low: u32,
    ) -> HashMap<u32, u64> {
        let mut out: HashMap<u32, u64> = HashMap::new();
        for (inst, v) in readings {
            if !inst.luid.is_some_and(|l| l.matches(high, low)) {
                continue;
            }
            let Some(pid) = inst.pid else { continue };
            *out.entry(pid).or_insert(0) += v.max(0.0) as u64;
        }
        out
    }

    /// Adapter-level byte total (for the `GPU Adapter Memory` streams) for one adapter,
    /// summed across its phys/part instances. `None` when nothing matched — a GPU with no
    /// counter is "unknown", not "0 bytes used".
    pub fn adapter_bytes(readings: &[(ParsedInstance, f64)], high: i32, low: u32) -> Option<u64> {
        let mut total: Option<u64> = None;
        for (inst, v) in readings {
            if !inst.luid.is_some_and(|l| l.matches(high, low)) {
                continue;
            }
            *total.get_or_insert(0) += v.max(0.0) as u64;
        }
        total
    }

    /// One formatted collection of every GPU counter stream — pure data, so the
    /// aggregation functions above are testable from scripted values on any OS. Built on
    /// Windows by [`SharedPdh::snapshot`]; both Windows backends read the same snapshot.
    ///
    /// Empty vectors are the *normal* GPU-less/first-sample state, not an error.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct PdhSnapshot {
        /// When this collection happened (unix millis) — one timestamp per frame.
        pub at_ms: u64,
        /// `\GPU Engine(*)\Utilization Percentage` — busy % per (pid, engine) instance.
        pub engine_util: Vec<(ParsedInstance, f64)>,
        /// `\GPU Process Memory(*)\Dedicated Usage` — bytes per (pid, adapter).
        pub proc_dedicated: Vec<(ParsedInstance, f64)>,
        /// `\GPU Process Memory(*)\Shared Usage` — bytes per (pid, adapter). Carried
        /// separately to the UI; **never** added into `mem_bytes` (dedicated vs shared is
        /// the honest split, §2.4).
        pub proc_shared: Vec<(ParsedInstance, f64)>,
        /// `\GPU Adapter Memory(*)\Dedicated Usage` — bytes per adapter (VidMm truth).
        pub adapter_dedicated: Vec<(ParsedInstance, f64)>,
        /// `\GPU Adapter Memory(*)\Shared Usage` — bytes per adapter, shown separately.
        pub adapter_shared: Vec<(ParsedInstance, f64)>,
    }

    // ---- Shared process-wide query (Windows-only from here down) -----------------------

    #[cfg(target_os = "windows")]
    pub use windows_impl::{shared, SharedPdh};

    #[cfg(target_os = "windows")]
    mod windows_impl {
        use std::sync::{Mutex, OnceLock};

        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::System::Performance::{
            PdhAddCounterW, PdhAddEnglishCounterW, PdhCollectQueryData, PdhExpandWildCardPathW,
            PdhGetFormattedCounterArrayW, PdhOpenQueryW, PdhRemoveCounter, PDH_FMT,
            PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
        };

        /// `PDH_FMT_NOCAP100` (pdh.h) is missing from windows-rs 0.62.2's metadata, so
        /// the header value is restated here. It is mandatory (§3.2): without it PDH
        /// silently caps values at 100, which would make summed multi-engine numbers
        /// quietly wrong — a trust-thesis violation, not a crash.
        const PDH_FMT_NOCAP100: PDH_FMT = PDH_FMT(0x0000_8000);

        use super::{
            item_value_is_trustworthy, parse_instance, status_is_normal_absence, ParsedInstance,
            PdhSnapshot, PDH_CSTATUS_NO_COUNTER, PDH_CSTATUS_NO_OBJECT, PDH_MORE_DATA, PDH_OK,
            PDH_QUERY_PERF_DATA_TIMEOUT,
        };

        /// Snapshot cache lifetime: the nvidia and wddm backends polling in the same
        /// Engine tick must share ONE `PdhCollectQueryData` (§3.2) — 250 ms comfortably
        /// covers a tick's worth of refresh calls while never spanning two 1 Hz ticks.
        const SNAPSHOT_REUSE_MS: u64 = 250;

        /// In expanded-path (non-English-locale fallback) mode, instances churn with
        /// process lifecycle and explicitly-added paths do NOT pick up newcomers the way
        /// a wildcard does — so re-expand at this cadence. Two seconds bounds both the
        /// staleness (a new process appears in ≤2 s) and the cost (expansion walks the
        /// registry provider).
        const REEXPAND_MS: u64 = 2_000;

        /// The five counter streams, by English wildcard path. `PdhAddEnglishCounterW` is
        /// the localization-safe entry point; the expansion fallback below exists because
        /// wildcard-add is only proven on English Windows (§3.2).
        const STREAM_PATHS: [&str; 5] = [
            r"\GPU Engine(*)\Utilization Percentage",
            r"\GPU Process Memory(*)\Dedicated Usage",
            r"\GPU Process Memory(*)\Shared Usage",
            r"\GPU Adapter Memory(*)\Dedicated Usage",
            r"\GPU Adapter Memory(*)\Shared Usage",
        ];

        fn to_wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        enum StreamMode {
            /// One wildcard counter handle — new instances arrive automatically.
            Wildcard(PDH_HCOUNTER),
            /// Per-instance handles from `PdhExpandWildCardPathW` (locale fallback);
            /// requires periodic re-expansion to see instance churn.
            Expanded {
                handles: Vec<PDH_HCOUNTER>,
                last_expand_ms: u64,
            },
            /// The object/counter does not exist (`NO_OBJECT`/`NO_COUNTER`) — the normal
            /// GPU-less outcome. The stream stays permanently empty; never an error.
            Absent,
        }

        struct Stream {
            /// English wildcard path, kept for re-expansion in fallback mode.
            wildcard: &'static str,
            mode: StreamMode,
        }

        struct QueryState {
            query: PDH_HQUERY,
            streams: Vec<Stream>,
            cache: Option<PdhSnapshot>,
        }

        /// The process-wide PDH query (§3.2): opened once, shared by every backend.
        pub struct SharedPdh {
            /// `None` = `PdhOpenQueryW` itself failed — every snapshot is `None`.
            state: Mutex<Option<QueryState>>,
            /// Whether the `GPU Engine` object existed at open time. False on GPU-less
            /// machines (`PDH_CSTATUS_NO_OBJECT`) — drives the "no WDDM 2.0 GPU/driver"
            /// process hint, not an error.
            engine_object_present: bool,
        }

        // SAFETY: the raw PDH handles inside QueryState are only ever touched while
        // holding the Mutex; PDH itself is documented thread-safe per-query with
        // external synchronization, which the Mutex provides.
        unsafe impl Send for SharedPdh {}
        unsafe impl Sync for SharedPdh {}

        /// The one process-wide query. `OnceLock` + `Mutex`: opened on first use, shared
        /// by the nvidia and wddm backends so one tick costs one `PdhCollectQueryData`.
        pub fn shared() -> &'static SharedPdh {
            static SHARED: OnceLock<SharedPdh> = OnceLock::new();
            SHARED.get_or_init(SharedPdh::open)
        }

        impl SharedPdh {
            fn open() -> Self {
                let mut query = PDH_HQUERY::default();
                // No data source (live machine), no user data.
                let status = unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) };
                if status != PDH_OK {
                    return SharedPdh {
                        state: Mutex::new(None),
                        engine_object_present: false,
                    };
                }
                let streams: Vec<Stream> = STREAM_PATHS
                    .iter()
                    .map(|path| Stream {
                        wildcard: path,
                        mode: add_stream(query, path),
                    })
                    .collect();
                let engine_object_present = !matches!(streams[0].mode, StreamMode::Absent);
                SharedPdh {
                    state: Mutex::new(Some(QueryState {
                        query,
                        streams,
                        cache: None,
                    })),
                    engine_object_present,
                }
            }

            /// Whether the `GPU Engine` PDH object exists in this session. False means
            /// "no WDDM 2.0 GPU/driver" — the per-process hint, not a failure.
            pub fn engine_object_present(&self) -> bool {
                self.engine_object_present
            }

            /// The shared formatted snapshot, re-collected only if the cache is older
            /// than ~250 ms (§3.2). `None` only when PDH itself never opened; every
            /// counter-level absence is an *empty stream inside* `Some` — callers map
            /// that to per-metric `None`s.
            pub fn snapshot(&self, now_ms: u64) -> Option<PdhSnapshot> {
                let mut guard = self.state.lock().ok()?;
                let st = guard.as_mut()?;

                if let Some(cache) = &st.cache {
                    if now_ms.saturating_sub(cache.at_ms) < SNAPSHOT_REUSE_MS {
                        return Some(cache.clone());
                    }
                }

                // Locale-fallback streams: re-expand on cadence so instance churn
                // (process start/exit) is visible despite the explicit paths.
                for stream in &mut st.streams {
                    if let StreamMode::Expanded {
                        handles,
                        last_expand_ms,
                    } = &mut stream.mode
                    {
                        if now_ms.saturating_sub(*last_expand_ms) >= REEXPAND_MS {
                            for h in handles.drain(..) {
                                // Best effort: a failed remove leaks one stale counter,
                                // which is preferable to aborting the snapshot.
                                let _ = unsafe { PdhRemoveCounter(h) };
                            }
                            *handles = expand_and_add(st.query, stream.wildcard);
                            *last_expand_ms = now_ms;
                        }
                    }
                }

                let status = unsafe { PdhCollectQueryData(st.query) };
                if status != PDH_OK {
                    if status == PDH_QUERY_PERF_DATA_TIMEOUT {
                        // Transient provider miss (§3.2): reuse the stale snapshot if
                        // one exists — NOT a device_lost, NOT an error.
                        return Some(st.cache.clone().unwrap_or_default());
                    }
                    if status_is_normal_absence(status) {
                        // e.g. PDH_NO_DATA: nothing in the query has data (GPU-less, or
                        // very first collection). An honestly-empty frame.
                        let snap = PdhSnapshot {
                            at_ms: now_ms,
                            ..Default::default()
                        };
                        st.cache = Some(snap.clone());
                        return Some(snap);
                    }
                    return None;
                }

                let mut snap = PdhSnapshot {
                    at_ms: now_ms,
                    ..Default::default()
                };
                for (i, stream) in st.streams.iter().enumerate() {
                    let out = match i {
                        0 => &mut snap.engine_util,
                        1 => &mut snap.proc_dedicated,
                        2 => &mut snap.proc_shared,
                        3 => &mut snap.adapter_dedicated,
                        _ => &mut snap.adapter_shared,
                    };
                    let handles: &[PDH_HCOUNTER] = match &stream.mode {
                        StreamMode::Wildcard(h) => std::slice::from_ref(h),
                        StreamMode::Expanded { handles, .. } => handles,
                        StreamMode::Absent => continue,
                    };
                    for &h in handles {
                        read_formatted_array(h, out);
                    }
                }
                st.cache = Some(snap.clone());
                Some(snap)
            }
        }

        /// Add one stream: localization-safe English wildcard first; `NO_OBJECT`/
        /// `NO_COUNTER` is the permanent normal absence; any other failure goes through
        /// the documented non-English-locale fallback chain (expand → add per path).
        fn add_stream(query: PDH_HQUERY, path: &'static str) -> StreamMode {
            let wide = to_wide(path);
            let mut handle = PDH_HCOUNTER::default();
            let status =
                unsafe { PdhAddEnglishCounterW(query, PCWSTR(wide.as_ptr()), 0, &mut handle) };
            if status == PDH_OK {
                return StreamMode::Wildcard(handle);
            }
            if status == PDH_CSTATUS_NO_OBJECT || status == PDH_CSTATUS_NO_COUNTER {
                return StreamMode::Absent;
            }
            // Wildcard-add is only proven on English Windows (§3.2): expand the wildcard
            // into concrete instance paths and add each one.
            let handles = expand_and_add(query, path);
            if handles.is_empty() {
                StreamMode::Absent
            } else {
                StreamMode::Expanded {
                    handles,
                    last_expand_ms: 0,
                }
            }
        }

        /// `PdhExpandWildCardPathW` two-call pattern → `PdhAddCounterW` per expanded
        /// path. Empty on any failure — absence over error, always.
        fn expand_and_add(query: PDH_HQUERY, path: &str) -> Vec<PDH_HCOUNTER> {
            let wide = to_wide(path);
            let mut len: u32 = 0;
            let status = unsafe {
                PdhExpandWildCardPathW(PCWSTR::null(), PCWSTR(wide.as_ptr()), None, &mut len, 0)
            };
            if status != PDH_MORE_DATA || len == 0 {
                return Vec::new();
            }
            let mut buf = vec![0u16; len as usize];
            let status = unsafe {
                PdhExpandWildCardPathW(
                    PCWSTR::null(),
                    PCWSTR(wide.as_ptr()),
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    0,
                )
            };
            if status != PDH_OK {
                return Vec::new();
            }
            // MULTI_SZ: NUL-separated strings, double-NUL terminated.
            let mut handles = Vec::new();
            for entry in buf.split(|&c| c == 0) {
                if entry.is_empty() {
                    continue;
                }
                let mut entry_z: Vec<u16> = entry.to_vec();
                entry_z.push(0);
                let mut h = PDH_HCOUNTER::default();
                let status = unsafe { PdhAddCounterW(query, PCWSTR(entry_z.as_ptr()), 0, &mut h) };
                if status == PDH_OK {
                    handles.push(h);
                }
            }
            handles
        }

        /// Read one counter's formatted instance array (two-call buffer pattern,
        /// `PDH_FMT_DOUBLE | PDH_FMT_NOCAP100`) and append the parseable items.
        ///
        /// NOCAP100 is mandatory (§3.2): without it values are silently capped at 100,
        /// which would make summed multi-engine numbers quietly wrong — a trust-thesis
        /// violation, not a crash. Per-item `CStatus` is checked before trusting any
        /// value; an untrusted item is skipped (that item is None this tick), and an
        /// unparseable instance name is skipped (unattributed) — neither aborts the read.
        fn read_formatted_array(counter: PDH_HCOUNTER, out: &mut Vec<(ParsedInstance, f64)>) {
            let fmt = PDH_FMT(PDH_FMT_DOUBLE.0 | PDH_FMT_NOCAP100.0);
            let mut buf_bytes: u32 = 0;
            let mut count: u32 = 0;
            let status = unsafe {
                PdhGetFormattedCounterArrayW(counter, fmt, &mut buf_bytes, &mut count, None)
            };
            if status != PDH_MORE_DATA {
                // Includes the per-counter absence statuses (NO_INSTANCE, first-sample
                // INVALID_DATA, ...) — normal: this stream is simply empty this tick.
                return;
            }
            // u64-backed buffer: PDH_FMT_COUNTERVALUE_ITEM_W contains an f64 union and
            // needs 8-byte alignment, which Vec<u8> does not guarantee.
            let mut buf = vec![0u64; (buf_bytes as usize).div_ceil(8)];
            let items_ptr = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter,
                    fmt,
                    &mut buf_bytes,
                    &mut count,
                    Some(items_ptr),
                )
            };
            if status != PDH_OK {
                return;
            }
            let items = unsafe { std::slice::from_raw_parts(items_ptr, count as usize) };
            for item in items {
                if !item_value_is_trustworthy(item.FmtValue.CStatus) {
                    continue;
                }
                let name = match unsafe { item.szName.to_string() } {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if let Some(parsed) = parse_instance(&name) {
                    // SAFETY: PDH_FMT_DOUBLE was requested, so the union's doubleValue
                    // arm is the one PDH populated.
                    let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                    out.push((parsed, value));
                }
            }
        }
    }
}

/// DXGI adapter enumeration + D3DKMT LUID→PCI identity (§3.1/§3.3).
pub mod adapters {
    use crate::model::Vendor;

    /// Everything the backend needs about one DXGI adapter — pure data, any OS.
    #[derive(Clone, Debug, PartialEq)]
    pub struct AdapterInfo {
        /// DXGI enumeration ordinal (only used in the synthetic-id fallback).
        pub ordinal: u32,
        /// `DXGI_ADAPTER_DESC1.Description`.
        pub name: String,
        pub vendor_id: u32,
        pub device_id: u32,
        /// Adapter LUID — the in-session join key for PDH instances and D3DKMT.
        /// Session-scoped (changes on reboot/driver update): NEVER persisted as identity.
        pub luid_high: i32,
        pub luid_low: u32,
        /// `DedicatedVideoMemory` — real VRAM on discrete boards; on iGPU/UMA a small
        /// carve-out (~0 is normal) that must not be summed with the shared budget.
        pub dedicated_video_bytes: u64,
        /// `SharedSystemMemory` — the UMA/shared budget, carried separately so the UI can
        /// label it as such (never folded into "VRAM total", §3.4).
        pub shared_system_bytes: u64,
        /// Normalized PCI BDF (`"0000:bb:dd.f"`) from D3DKMT `ADAPTERADDRESS`, when the
        /// thunk worked. `None` → the synthetic-id fallback (which refuses to dedupe).
        pub pci_bdf: Option<String>,
    }

    /// PCI vendor-id → vendor. Pure, tested on any OS. Unknown ids (including 0x1414
    /// Microsoft, whose software adapters are skipped before this is ever consulted) map
    /// to `Unknown` — which renders as the honest generic "GPU".
    pub fn vendor_of(vendor_id: u32) -> Vendor {
        match vendor_id {
            0x10DE => Vendor::Nvidia,
            0x1002 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Unknown,
        }
    }

    /// Format a D3DKMT `ADAPTERADDRESS` as the normalized BDF the whole tool keys on:
    /// `"0000:bb:dd.f"`, lowercase hex. The struct has no PCI-domain field — client
    /// Windows is effectively always domain 0 (§2.5), hence the literal `0000`. This is
    /// byte-compatible with what `normalize_pci_id` produces from NVML's
    /// 8-hex-digit-domain form, which is exactly what makes first-wins dedupe work.
    ///
    /// Out-of-range parts (bus > 0xff, device > 0x1f, function > 7 — not expressible in a
    /// PCI BDF) mean the thunk returned something we do not understand: `None`, so the
    /// caller falls back to the synthetic id instead of fabricating a plausible-looking
    /// address that might wrongly dedupe against a real device.
    pub fn bdf_string(bus: u32, device: u32, function: u32) -> Option<String> {
        if bus > 0xFF || device > 0x1F || function > 0x7 {
            return None;
        }
        Some(format!("0000:{bus:02x}:{device:02x}.{function:x}"))
    }

    /// Fallback `DeviceId` when the D3DKMT address query fails (§3.1):
    /// `wddm:<vendor>:<device>:<ordinal>`. The `wddm:` prefix is not hex, so
    /// `normalize_pci_id` correctly refuses to dedupe it — listing a device twice beats
    /// wrongly merging two. Stable within a session only; good enough for a degraded
    /// "device-level only with synthetic id" state.
    pub fn synthetic_device_id(vendor_id: u32, device_id: u32, ordinal: u32) -> String {
        format!("wddm:{vendor_id:04x}:{device_id:04x}:{ordinal}")
    }

    #[cfg(target_os = "windows")]
    pub use windows_impl::enumerate;

    #[cfg(target_os = "windows")]
    mod windows_impl {
        use windows::Win32::Foundation::LUID;
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
        };

        use super::{bdf_string, AdapterInfo};

        /// Enumerate hardware adapters: `CreateDXGIFactory1` → `EnumAdapters1` until
        /// `DXGI_ERROR_NOT_FOUND` → `GetDesc1` (§3.1). Software adapters (WARP /
        /// Microsoft Basic Render) are skipped — they are not GPUs and monitoring them
        /// would be noise. Empty on any failure: a machine where DXGI cannot enumerate
        /// is a machine where this backend honestly has nothing to show.
        pub fn enumerate() -> Vec<AdapterInfo> {
            let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
                Ok(f) => f,
                Err(_) => return Vec::new(),
            };
            let mut out = Vec::new();
            for ordinal in 0.. {
                // Any enumeration error ends the loop: DXGI_ERROR_NOT_FOUND is the
                // documented terminator, and anything else means no further adapters
                // are reachable either.
                let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(ordinal) } {
                    Ok(a) => a,
                    Err(_) => break,
                };
                let desc = match unsafe { adapter.GetDesc1() } {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = {
                    let len = desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len());
                    String::from_utf16_lossy(&desc.Description[..len])
                };
                out.push(AdapterInfo {
                    ordinal,
                    name,
                    vendor_id: desc.VendorId,
                    device_id: desc.DeviceId,
                    luid_high: desc.AdapterLuid.HighPart,
                    luid_low: desc.AdapterLuid.LowPart,
                    dedicated_video_bytes: desc.DedicatedVideoMemory as u64,
                    shared_system_bytes: desc.SharedSystemMemory as u64,
                    pci_bdf: pci_bdf_of(desc.AdapterLuid),
                });
            }
            out
        }

        /// LUID → PCI BDF via the WDK-documented gdi32 thunks (Windows 8+, §3.3):
        /// `D3DKMTOpenAdapterFromLuid` → `D3DKMTQueryAdapterInfo(ADAPTERADDRESS)` →
        /// `D3DKMTCloseAdapter`. This is the least-contractual API in the chain, so it is
        /// isolated here and every failure degrades to `None` (synthetic id), never a
        /// crash. `D3DKMTQueryStatistics` is "Reserved for system use" — never used.
        fn pci_bdf_of(luid: LUID) -> Option<String> {
            use windows::Wdk::Graphics::Direct3D::{
                D3DKMTCloseAdapter, D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo,
                D3DKMT_ADAPTERADDRESS, D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMLUID,
                D3DKMT_QUERYADAPTERINFO, KMTQAITYPE_ADAPTERADDRESS,
            };

            let mut open = D3DKMT_OPENADAPTERFROMLUID {
                AdapterLuid: luid,
                hAdapter: 0,
            };
            if unsafe { D3DKMTOpenAdapterFromLuid(&mut open) }.is_err() {
                return None;
            }
            let mut addr = D3DKMT_ADAPTERADDRESS::default();
            let mut query = D3DKMT_QUERYADAPTERINFO {
                hAdapter: open.hAdapter,
                Type: KMTQAITYPE_ADAPTERADDRESS,
                pPrivateDriverData: &mut addr as *mut _ as *mut core::ffi::c_void,
                PrivateDriverDataSize: std::mem::size_of::<D3DKMT_ADAPTERADDRESS>() as u32,
            };
            let status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };
            let close = D3DKMT_CLOSEADAPTER {
                hAdapter: open.hAdapter,
            };
            // Best-effort close: a leak of one kernel adapter handle is survivable; a
            // panic here is not.
            let _ = unsafe { D3DKMTCloseAdapter(&close) };
            if status.is_err() {
                return None;
            }
            bdf_string(addr.BusNumber, addr.DeviceNumber, addr.FunctionNumber)
        }
    }
}

/// Trim a process image path to its basename — same rule as nvidia.rs (both separators,
/// because NVML on Windows reports `C:\...\foo.exe` while other sources may use `/`).
/// Pure so it unit-tests on any OS.
#[cfg(any(target_os = "windows", test))]
fn image_basename(path: &str) -> &str {
    match path.rsplit(['/', '\\']).next() {
        Some(base) if !base.is_empty() => base,
        _ => path,
    }
}

/// Resolve a pid to its executable basename via the OS process query (§3.5), shared with
/// the nvidia backend's §2.4 PDH-only-pid rows. `OpenProcess` legitimately fails for other
/// users' / protected processes when unprivileged — the row still renders, named by pid:
/// an unnamed process is honest, a dropped one is not.
#[cfg(target_os = "windows")]
pub(crate) fn os_process_name(pid: u32) -> String {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(_) => return format!("pid {pid}"),
    };
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let queried = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // Best-effort close — see pci_bdf_of for the rationale.
    let _ = unsafe { CloseHandle(handle) };
    if queried.is_ok() && len > 0 {
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let base = image_basename(&full);
        if !base.is_empty() {
            return base.to_string();
        }
    }
    format!("pid {pid}")
}

#[cfg(target_os = "windows")]
pub use backend_impl::WddmBackend;

#[cfg(target_os = "windows")]
mod backend_impl {
    use super::adapters::{self, AdapterInfo};
    use super::{os_process_name, pdh};
    use crate::backend::{BackendError, GpuBackend};
    use crate::model::{now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo};

    pub struct WddmBackend {
        /// (adapter, persistent id) established at init. The LUID inside `AdapterInfo`
        /// is the session-scoped PDH/D3DKMT join key; the `DeviceId` is the persistent
        /// identity (PCI BDF, or the non-deduping `wddm:` synthetic fallback).
        devs: Vec<(AdapterInfo, DeviceId)>,
    }

    impl WddmBackend {
        pub fn init() -> Result<Self, BackendError> {
            let infos = adapters::enumerate();
            if infos.is_empty() {
                // Normal on GPU-less machines (CI runners, some VMs): only software
                // adapters (or none) exist. Backend skipped, mock fallback covers the UI.
                return Err(BackendError::Unavailable(
                    "no hardware DXGI adapters (software/WARP only)".into(),
                ));
            }
            // Touch the shared PDH query once at init so the first Engine tick is the
            // *second* collection and rate counters can already produce values.
            let _ = pdh::shared().snapshot(now_ms());
            let devs = infos
                .into_iter()
                .map(|a| {
                    let id = DeviceId(a.pci_bdf.clone().unwrap_or_else(|| {
                        adapters::synthetic_device_id(a.vendor_id, a.device_id, a.ordinal)
                    }));
                    (a, id)
                })
                .collect();
            Ok(Self { devs })
        }

        fn adapter_of(&self, dev: &DeviceId) -> Result<&AdapterInfo, BackendError> {
            self.devs
                .iter()
                .find(|(_, id)| id == dev)
                .map(|(a, _)| a)
                .ok_or_else(|| BackendError::DeviceNotFound(dev.clone()))
        }
    }

    impl GpuBackend for WddmBackend {
        fn name(&self) -> &'static str {
            "wddm"
        }

        fn devices(&mut self) -> Vec<DeviceId> {
            self.devs.iter().map(|(_, id)| id.clone()).collect()
        }

        fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
            let a = self.adapter_of(dev)?;
            // §2.7-analog hint for this backend: with no GPU Engine PDH object there is
            // no per-process attribution at all — say why the table is empty up front.
            let process_hint = if pdh::shared().engine_object_present() {
                None
            } else {
                Some(
                    "per-process GPU stats unavailable: no WDDM 2.0 GPU/driver \
                     (Windows exposes them via GPU performance counters)"
                        .into(),
                )
            };
            Ok(StaticInfo {
                id: dev.clone(),
                vendor: adapters::vendor_of(a.vendor_id),
                name: if a.name.is_empty() {
                    "WDDM adapter".into()
                } else {
                    a.name.clone()
                },
                backend: "wddm".into(),
                // DXGI DedicatedVideoMemory. On iGPU/UMA adapters a dedicated segment of
                // 0 is normal — but reporting Some(0) as "total VRAM" would turn every
                // usage percentage into a division-by-zero lie, so 0 maps to None until
                // the model can carry the shared budget (AdapterInfo.shared_system_bytes)
                // as the separately-labeled number it must be (§3.4: never sum them).
                mem_total_bytes: (a.dedicated_video_bytes > 0).then_some(a.dedicated_video_bytes),
                // No public API without vendor SDKs (§3.4) — same story as the dynamic
                // power/temp/clock fields below.
                power_limit_mw: None,
                max_sm_clock_mhz: None,
                temp_slowdown_c: None,
                // DXCore DriverVersion is a follow-up (§3.4); DXGI has no driver version.
                driver_version: None,
                process_hint,
                // §3.4/§5.4: the mandatory honesty label for this source's headline —
                // utilization is the busiest single engine's scheduler duty-cycle (the
                // per-tick engine name rides in DynamicSample::util_engine), and the
                // missing power/temp/clock columns are an OS gap, not a device gap.
                source_caveat: Some(
                    "utilization is the busiest engine's WDDM scheduler (VidSch) \
                     duty-cycle, not whole-GPU capacity; Windows exposes no public \
                     power/temperature/clock API for this GPU"
                        .into(),
                ),
            })
        }

        fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
            let a = self.adapter_of(dev)?;
            // PDH unavailable / first sample / GPU-less: every PDH-sourced field is None.
            // Absence is a normal outcome, never an Err (the device did not fall off the
            // bus — we just cannot observe it this tick).
            let snap = pdh::shared().snapshot(now_ms()).unwrap_or_default();
            let (high, low) = (a.luid_high, a.luid_low);

            // Busiest-single-engine headline — scheduler (VidSch) duty-cycle, labeled by
            // the static source_caveat and deliberately Task-Manager-comparable. The
            // busiest engine's *name* travels with the number (util_engine) so the UI
            // can say WHICH engine made the claim: "Copy 97%" ≠ "3D 97%".
            let headline = pdh::device_util(&snap.engine_util, high, low);

            Ok(DynamicSample {
                ts_ms: now_ms(),
                util_pct: headline.as_ref().map(|h| h.pct as f32),
                util_engine: headline.map(|h| h.engtype),
                // Adapter-level Dedicated Usage: the VidMm number (KB4490156). NEVER
                // QueryVideoMemoryInfo — that is the calling process's own view (§3.4).
                mem_used_bytes: pdh::adapter_bytes(&snap.adapter_dedicated, high, low),
                // Windows exposes no public power/temperature/fan/clock API for AMD and
                // Intel GPUs without vendor SDKs (which the no-vendor-SDK rule forbids):
                // honest None, rendered as "unavailable" by the UI. The driver-optional
                // D3DKMT ADAPTERPERFDATA probe and DXCore telemetry are explicitly out of
                // scope for v1.5 (§3.6) — do not "fix" these in.
                power_mw: None,
                temp_c: None,
                fan_pct: None,
                sm_clock_mhz: None,
                mem_clock_mhz: None,
                // Per-engtype busy% — engtype names are an open set matched by name; a
                // GPU without that engine type yields None, not 0 (§3.4).
                encoder_pct: pdh::engtype_util(&snap.engine_util, high, low, "VideoEncode")
                    .map(|v| v as f32),
                decoder_pct: pdh::engtype_util(&snap.engine_util, high, low, "VideoDecode")
                    .map(|v| v as f32),
                // This source cannot observe throttling — `None` is the §5.4 spelling of
                // "unobserved". An all-false struct here would be a fabricated negative
                // ("not throttling" asserted as fact), which this model change exists to
                // make unrepresentable.
                throttle: None,
            })
        }

        fn refresh_processes(
            &mut self,
            dev: &DeviceId,
        ) -> Result<Vec<ProcessSample>, BackendError> {
            let a = self.adapter_of(dev)?;
            let snap = pdh::shared().snapshot(now_ms()).unwrap_or_default();
            let (high, low) = (a.luid_high, a.luid_low);

            // Join GPU Engine (util) and GPU Process Memory (dedicated bytes) by pid on
            // this LUID. Shared Usage is in the snapshot for the UI's dedicated-vs-shared
            // split, but is NEVER added into mem_bytes (§2.4).
            let util = pdh::per_pid_util(&snap.engine_util, high, low);
            let mem = pdh::per_pid_bytes(&snap.proc_dedicated, high, low);

            let mut pids: Vec<u32> = util.keys().chain(mem.keys()).copied().collect();
            pids.sort_unstable();
            pids.dedup();

            Ok(pids
                .into_iter()
                .map(|pid| {
                    let u = util.get(&pid);
                    ProcessSample {
                        pid,
                        name: os_process_name(pid),
                        // PDH does not distinguish compute from graphics clients; a
                        // Compute/Cuda engtype upgrades to Compute as a heuristic only
                        // (§3.5). Everything else is honestly Unknown.
                        kind: if u.is_some_and(|u| u.compute_hint) {
                            ProcessKind::Compute
                        } else {
                            ProcessKind::Unknown
                        },
                        mem_bytes: mem.get(&pid).copied(),
                        // Max-across-engines, scheduler duty-cycle (module docs). A pid
                        // seen only by the memory counters has no engine instance yet:
                        // util unknown, not 0.
                        util_pct: u.map(|u| u.pct as f32),
                        // No /proc on Windows: both honestly None (§3.5).
                        cpu_pct: None,
                        container: None,
                    }
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::adapters::{bdf_string, synthetic_device_id, vendor_of};
    use super::image_basename;
    use super::pdh::{
        adapter_bytes, device_util, engtype_util, item_value_is_trustworthy, parse_instance,
        per_pid_bytes, per_pid_util, status_is_normal_absence, InstanceLuid, ParsedInstance,
        PdhSnapshot, PDH_CSTATUS_INVALID_DATA, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_NO_COUNTER,
        PDH_CSTATUS_NO_INSTANCE, PDH_CSTATUS_NO_OBJECT, PDH_INVALID_DATA, PDH_MORE_DATA,
        PDH_NO_DATA, PDH_OK, PDH_QUERY_PERF_DATA_TIMEOUT,
    };
    use crate::model::Vendor;

    // ---- counter-instance grammar (string fixtures — these run on every OS) ----

    #[test]
    fn parse_engine_instance_full_form() {
        // The canonical \GPU Engine(*) instance shape, as Task Manager's data source
        // emits it on real Windows 10/11 machines.
        let p = parse_instance("pid_1234_luid_0x00000000_0x0000C739_phys_0_eng_3_engtype_3D")
            .expect("canonical engine instance must parse");
        assert_eq!(p.pid, Some(1234));
        assert_eq!(p.luid, Some(InstanceLuid(0x0000_0000, 0x0000_C739)));
        assert_eq!(p.phys, Some(0));
        assert_eq!(p.eng, Some(3));
        assert_eq!(p.engtype.as_deref(), Some("3D"));
        assert_eq!(p.part, None);
    }

    #[test]
    fn parse_process_memory_instance_without_engine_tokens() {
        // \GPU Process Memory(*) instances carry pid+luid+phys but no eng/engtype.
        let p = parse_instance("pid_8232_luid_0x00000000_0x0000C32D_phys_0")
            .expect("process-memory instance must parse");
        assert_eq!(p.pid, Some(8232));
        assert_eq!(p.luid, Some(InstanceLuid(0, 0x0000_C32D)));
        assert_eq!(p.phys, Some(0));
        assert_eq!(p.eng, None);
        assert_eq!(p.engtype, None);
    }

    #[test]
    fn parse_adapter_memory_instance_has_no_pid() {
        // \GPU Adapter Memory(*) instances are device-level: luid+phys only.
        let p = parse_instance("luid_0x00000000_0x0000C32D_phys_0")
            .expect("adapter-memory instance must parse");
        assert_eq!(p.pid, None);
        assert_eq!(p.luid, Some(InstanceLuid(0, 0x0000_C32D)));
        assert_eq!(p.phys, Some(0));
    }

    #[test]
    fn parse_part_token_and_multi_word_engtype() {
        // The optional part_N token, and an engtype containing an underscore: engtype is
        // "rest of string", underscores preserved — the engtype set is open (§3.2) and a
        // parser that split it would silently rename future engine types.
        let p = parse_instance(
            "pid_4_luid_0x00000000_0x0000ABCD_phys_0_part_1_eng_2_engtype_Video_Decode",
        )
        .expect("part + multi-token engtype must parse");
        assert_eq!(p.part, Some(1));
        assert_eq!(p.eng, Some(2));
        assert_eq!(p.engtype.as_deref(), Some("Video_Decode"));
    }

    #[test]
    fn parse_is_keyword_driven_not_positional() {
        // Decoy for a positional parser: tokens reordered. The grammar is keyword-driven
        // (§3.2), so this still parses — a parser that assumed pid-first would read the
        // luid bytes as a pid here.
        let p = parse_instance("luid_0x00000000_0x0000C739_pid_77_phys_0")
            .expect("keyword-driven grammar must accept reordered tokens");
        assert_eq!(p.pid, Some(77));
        assert_eq!(p.luid, Some(InstanceLuid(0, 0xC739)));
    }

    #[test]
    fn parse_rejects_malformed_decoys() {
        // Each decoy is a string a wrong code path would happily mis-parse. Refusing to
        // guess means the instance counts as unattributed — never a wrong attribution.
        for (decoy, why) in [
            ("", "empty string"),
            ("_Total", "classic non-GPU PDH instance name"),
            ("Processor Information", "non-GPU object instance"),
            ("pid_12x4_luid_0x0_0x1_phys_0", "non-numeric pid"),
            ("pid_1234_luid_0x00000000", "luid missing its second DWORD"),
            (
                "pid_1234_luid_00000000_0000C739_phys_0",
                "luid parts without 0x prefix",
            ),
            ("pid_luid_0x0_0x1", "keyword where a value belongs"),
            (
                "pid_1234_luid_0x0_0x1_phys_0_eng_0_engtype_",
                "empty engtype",
            ),
            ("pid_1234_bogus_7", "unknown keyword"),
            (
                "pid_1234_luid_0x123456789_0x1_phys_0",
                "luid DWORD wider than 32 bits",
            ),
            ("___", "nothing but separators"),
        ] {
            assert_eq!(
                parse_instance(decoy),
                None,
                "must reject: {why} ({decoy:?})"
            );
        }
    }

    #[test]
    fn luid_matching_verifies_both_parts_in_either_order() {
        // The printed HighPart/LowPart order is inferred from observation, not
        // documented — so matching accepts either order but always verifies BOTH parts.
        let l = InstanceLuid(0x0000_0000, 0x0000_C739);
        assert!(l.matches(0, 0xC739), "observed order must match");
        assert!(
            InstanceLuid(0x0000_C739, 0x0000_0000).matches(0, 0xC739),
            "swapped printed order must also match (both parts verified)"
        );
        assert!(!l.matches(0, 0xC740), "one wrong part must not match");
        assert!(!l.matches(1, 0xC739), "one wrong part must not match");
        // HighPart is a signed LONG: a negative bit-pattern must round-trip.
        assert!(InstanceLuid(0xFFFF_FFFF, 0x10).matches(-1, 0x10));
    }

    // ---- aggregation math (scripted snapshots — any OS) ----

    /// Engine-utilization reading fixture: (pid, luid, eng, engtype, value).
    fn eng(pid: u32, luid: (u32, u32), engn: u32, ty: &str, v: f64) -> (ParsedInstance, f64) {
        (
            ParsedInstance {
                pid: Some(pid),
                luid: Some(InstanceLuid(luid.0, luid.1)),
                phys: Some(0),
                part: None,
                eng: Some(engn),
                engtype: Some(ty.into()),
            },
            v,
        )
    }

    /// Memory reading fixture: (pid or device-level, luid, bytes).
    fn memr(pid: Option<u32>, luid: (u32, u32), v: f64) -> (ParsedInstance, f64) {
        (
            ParsedInstance {
                pid,
                luid: Some(InstanceLuid(luid.0, luid.1)),
                phys: Some(0),
                ..Default::default()
            },
            v,
        )
    }

    const LUID_A: (u32, u32) = (0, 0xC739);
    const LUID_B: (u32, u32) = (0, 0xBEEF);

    #[test]
    fn device_util_headline_is_busiest_engine_after_pid_sum() {
        // eng0/3D: 30+25=55 across two pids; eng1/Copy: 70 — headline must be the
        // busiest single ENGINE (Task-Manager-comparable), not the busiest pid or a
        // device-wide sum.
        let readings = vec![
            eng(1, LUID_A, 0, "3D", 30.0),
            eng(2, LUID_A, 0, "3D", 25.0),
            eng(1, LUID_A, 1, "Copy", 70.0),
        ];
        let h = device_util(&readings, 0, 0xC739).expect("matching instances → headline");
        assert_eq!(h.engtype, "Copy");
        assert_eq!(h.pct, 70.0);
    }

    #[test]
    fn device_util_preserves_nocap100_sums_over_100() {
        // PDH_FMT_NOCAP100 semantics: a per-engine sum across pids may exceed 100 from
        // sampling skew. Capping would be a silent lie (§3.2) — the value passes through.
        let readings = vec![eng(1, LUID_A, 0, "3D", 60.0), eng(2, LUID_A, 0, "3D", 55.0)];
        let h = device_util(&readings, 0, 0xC739).unwrap();
        assert_eq!(h.pct, 115.0, "NOCAP100 sums must not be clamped to 100");
    }

    #[test]
    fn luid_grouping_excludes_other_adapters() {
        // A second adapter's 99%-busy engine must not leak into adapter A's headline.
        let readings = vec![eng(1, LUID_A, 0, "3D", 40.0), eng(9, LUID_B, 0, "3D", 99.0)];
        let h = device_util(&readings, 0, 0xC739).unwrap();
        assert_eq!(h.pct, 40.0);
        // And adapter B sees only its own.
        let h = device_util(&readings, 0, 0xBEEF).unwrap();
        assert_eq!(h.pct, 99.0);
    }

    #[test]
    fn unmatched_luid_instances_are_unattributed() {
        // No reading matches the queried adapter: everything is "unattributed" — None /
        // empty, never a guessed attribution (§3.2).
        let readings = vec![eng(1, LUID_A, 0, "3D", 40.0)];
        assert_eq!(device_util(&readings, 7, 0x1234), None);
        assert!(per_pid_util(&readings, 7, 0x1234).is_empty());
        let mem = vec![memr(Some(1), LUID_A, 1024.0)];
        assert!(per_pid_bytes(&mem, 7, 0x1234).is_empty());
        assert_eq!(adapter_bytes(&mem, 7, 0x1234), None);
    }

    #[test]
    fn engtype_sums_for_encoder_decoder_and_absent_is_none() {
        let readings = vec![
            // Two VideoEncode engines: eng4 sums to 30+20=50, eng5 holds 60 → max 60.
            eng(1, LUID_A, 4, "VideoEncode", 30.0),
            eng(2, LUID_A, 4, "VideoEncode", 20.0),
            eng(3, LUID_A, 5, "VideoEncode", 60.0),
            eng(1, LUID_A, 0, "3D", 90.0),
        ];
        assert_eq!(
            engtype_util(&readings, 0, 0xC739, "VideoEncode"),
            Some(60.0)
        );
        // Engtype matching is by name, case-insensitively — the set is open (§3.2).
        assert_eq!(
            engtype_util(&readings, 0, 0xC739, "videoencode"),
            Some(60.0)
        );
        // No VideoDecode engine on this GPU: None, never a fabricated 0 (§3.4).
        assert_eq!(engtype_util(&readings, 0, 0xC739, "VideoDecode"), None);
    }

    #[test]
    fn per_pid_util_is_max_across_engines_and_names_the_busiest() {
        let readings = vec![
            eng(1, LUID_A, 0, "3D", 30.0),
            eng(1, LUID_A, 1, "Copy", 70.0),
            eng(2, LUID_A, 6, "Cuda", 15.0),
        ];
        let m = per_pid_util(&readings, 0, 0xC739);
        let p1 = &m[&1];
        // Max-across-engines is the Task-Manager-comparable number; the engine name
        // travels with it so the UI can say WHICH engine made the claim.
        assert_eq!(p1.pct, 70.0);
        assert_eq!(p1.busiest_engtype, "Copy");
        assert!(!p1.compute_hint);
        // Cuda/Compute engtype presence is the (heuristic-only) compute upgrade signal.
        assert!(m[&2].compute_hint);
    }

    #[test]
    fn per_pid_dedicated_bytes_join_by_pid() {
        let readings = vec![
            memr(Some(8232), LUID_A, 1_073_741_824.0),
            memr(Some(444), LUID_A, 52_428_800.0),
            memr(Some(8232), LUID_B, 999.0), // other adapter — excluded
        ];
        let m = per_pid_bytes(&readings, 0, 0xC739);
        assert_eq!(m.get(&8232), Some(&1_073_741_824));
        assert_eq!(m.get(&444), Some(&52_428_800));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn adapter_bytes_sums_this_adapters_instances_only() {
        // Linked adapters can expose one instance per phys for the same LUID — they sum.
        let a_phys1 = {
            let mut r = memr(None, LUID_A, 1_000.0);
            r.0.phys = Some(1);
            r
        };
        let readings = vec![
            memr(None, LUID_A, 2_147_483_648.0),
            a_phys1,
            memr(None, LUID_B, 7.0),
        ];
        assert_eq!(adapter_bytes(&readings, 0, 0xC739), Some(2_147_484_648));
    }

    #[test]
    fn absent_counters_yield_empty_aggregation() {
        // THE GPU-less CI runner case (§3.2): no GPU Engine object exists, the snapshot
        // is honestly empty, and every aggregate is None/empty — a normal outcome a
        // future refactor must not turn into an error.
        let snap = PdhSnapshot::default();
        assert_eq!(device_util(&snap.engine_util, 0, 0xC739), None);
        assert_eq!(
            engtype_util(&snap.engine_util, 0, 0xC739, "VideoEncode"),
            None
        );
        assert!(per_pid_util(&snap.engine_util, 0, 0xC739).is_empty());
        assert!(per_pid_bytes(&snap.proc_dedicated, 0, 0xC739).is_empty());
        assert_eq!(adapter_bytes(&snap.adapter_dedicated, 0, 0xC739), None);
    }

    #[test]
    fn pdh_absence_status_codes_are_normal_outcomes() {
        // The §3.2 absence-is-normal table, pinned: each code maps to None + at most one
        // self-honesty event upstream — never an error.
        for code in [
            PDH_CSTATUS_NO_OBJECT,       // no WDDM 2.0 GPU — GPU-less CI runners
            PDH_CSTATUS_NO_COUNTER,      // object without this counter
            PDH_CSTATUS_NO_INSTANCE,     // nothing currently touches the GPU
            PDH_NO_DATA,                 // first collection
            PDH_CSTATUS_INVALID_DATA,    // per-item first-sample
            PDH_INVALID_DATA,            // query-level first-sample
            PDH_QUERY_PERF_DATA_TIMEOUT, // transient provider miss, NOT device_lost
        ] {
            assert!(
                status_is_normal_absence(code),
                "0x{code:08X} is a normal absence, not an error"
            );
        }
        // Success and "more data" are not absences — a classifier that swallowed them
        // would hide real values.
        assert!(!status_is_normal_absence(PDH_OK));
        assert!(!status_is_normal_absence(PDH_MORE_DATA));
        // Per-item trust gate: only VALID (0) and NEW_DATA pass.
        assert!(item_value_is_trustworthy(PDH_OK));
        assert!(item_value_is_trustworthy(PDH_CSTATUS_NEW_DATA));
        assert!(!item_value_is_trustworthy(PDH_CSTATUS_INVALID_DATA));
    }

    // ---- identity (any OS) ----

    #[test]
    fn bdf_string_matches_collector_normalization() {
        // Must be byte-identical to what normalize_pci_id produces from NVML's form —
        // that equality IS the cross-backend dedupe (§2.5). Domain is the literal 0000:
        // D3DKMT has no domain field and client Windows is effectively domain 0.
        assert_eq!(bdf_string(1, 0, 0).as_deref(), Some("0000:01:00.0"));
        // Lowercase hex, zero-padded 2/2/1 — same shape as sysfs/NVML-normalized ids.
        assert_eq!(bdf_string(0x0A, 2, 0).as_deref(), Some("0000:0a:02.0"));
        assert_eq!(bdf_string(0xFF, 0x1F, 7).as_deref(), Some("0000:ff:1f.7"));
        // Values a PCI BDF cannot express mean the thunk returned something we do not
        // understand — None (synthetic fallback), never a fabricated plausible address.
        assert_eq!(bdf_string(0x100, 0, 0), None);
        assert_eq!(bdf_string(0, 0x20, 0), None);
        assert_eq!(bdf_string(0, 0, 8), None);
    }

    #[test]
    fn synthetic_id_refuses_pci_shape() {
        // The fallback id must NOT parse as a PCI address: normalize_pci_id requires a
        // hex first segment, and "wddm" is not hex — so the collector never dedupes it
        // (listing a device twice beats wrongly merging two, §3.1).
        let id = synthetic_device_id(0x10DE, 0x2684, 0);
        assert_eq!(id, "wddm:10de:2684:0");
        let first_segment = id.split(':').next().unwrap();
        assert!(
            first_segment.bytes().any(|b| !b.is_ascii_hexdigit()),
            "first segment must not be pure hex, or it could dedupe as PCI: {id}"
        );
    }

    #[test]
    fn vendor_of_maps_pci_vendor_ids() {
        assert_eq!(vendor_of(0x10DE), Vendor::Nvidia);
        assert_eq!(vendor_of(0x1002), Vendor::Amd);
        assert_eq!(vendor_of(0x8086), Vendor::Intel);
        // 0x1414 (Microsoft) adapters are software and skipped before vendor mapping;
        // anything unrecognized renders as the honest generic "GPU".
        assert_eq!(vendor_of(0x1414), Vendor::Unknown);
        assert_eq!(vendor_of(0xABCD), Vendor::Unknown);
    }

    #[test]
    fn image_basename_trims_both_separator_kinds() {
        // Same trim rule as nvidia.rs: Windows paths use `\`, other sources may use `/`.
        assert_eq!(image_basename(r"C:\Windows\System32\dwm.exe"), "dwm.exe");
        assert_eq!(image_basename("/usr/bin/python3"), "python3");
        assert_eq!(image_basename("bare.exe"), "bare.exe");
        // A path ending in a separator must not yield an empty name.
        assert_eq!(image_basename(r"C:\odd\"), r"C:\odd\");
    }
}
