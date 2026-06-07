//! Event derivation — the "story" layer.
//!
//! Two-tier honesty contract (non-negotiable, see docs/research/04-synthesis.md §5 risk 2):
//! - `Confidence::Fact` events assert observed state transitions plainly (throttle bit set,
//!   process exited). They carry the raw evidence that produced them.
//! - `Confidence::Likely` events are inferences (extrapolated OOM ETA, suspected dataloader
//!   stall) and must always read as hedged.

use std::collections::{HashMap, VecDeque};

use crate::model::{fmt_bytes, DeviceId, DynamicSample, ProcessSample, ThrottleReasons};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Fact,
    Likely,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    ThrottleStart,
    ThrottleEnd,
    ProcessAttached,
    ProcessExited,
    VramPressure,
    IdleGap,
    /// The collector itself fell behind its tick cadence — the recording has a hole, and
    /// the recorder must say so rather than let the gap masquerade as device idleness.
    /// Emitted by the tui collector (the engine owns tick timing, not this module).
    CollectorStall,
    /// History was truncated or restarted (ring wrap on resize, store re-init); consumers
    /// must not treat the discontinuity as device behavior.
    HistoryReset,
    /// Device stopped answering queries while a workload was attached — possibly a hung
    /// kernel or driver. An inference by nature: always `Confidence::Likely`.
    HangSuspected,
    /// A registered device stopped answering its dynamic probe entirely (consecutive
    /// whole-probe failures, not per-metric absence — `NOT_SUPPORTED` stays `None` in the
    /// sample and is never this). Driver reset, NVML dying, eGPU unplug, and an xe rebind
    /// all look identical from this side of the probe, so the kind asserts only the
    /// observed silence — the CAUSE is never claimed. Emitted by the tui collector (it
    /// owns the probe loop and its tick counting). Always `Confidence::Fact`.
    DeviceLost,
    /// A device previously declared lost answered again. The samples between loss and
    /// return were never collected; that gap stays blank in history — a hole, never
    /// zeros. Always `Confidence::Fact`.
    DeviceReturned,
    /// A GPU-attached process is burning CPU while the GPU sits idle — the classic
    /// CPU-bound dataloader. An inference: always `Confidence::Likely`.
    CpuSpillover,
    /// A recording session began folding history into the database — the flight
    /// recorder's own power-on mark. Without it (and its stop twin) the timeline cannot
    /// distinguish "the GPU sat idle" from "gpuviewer wasn't running": unrecorded time
    /// renders blank, and the boundary events are what make that blank legible. Emitted
    /// by the tui collector (it owns the recorder lifecycle). Always `Confidence::Fact`.
    RecordingStarted,
    /// The recording session ended cleanly (the stop mark and the partial rollup tail
    /// reached the store). A session that dies without this mark — SIGKILL, OOM kill,
    /// power loss — writes nothing, which is itself information: the NEXT session's
    /// start mark narrates the missing stop. Always `Confidence::Fact`.
    RecordingStopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub ts_ms: u64,
    pub device: DeviceId,
    pub kind: EventKind,
    pub severity: Severity,
    pub confidence: Confidence,
    /// One-line human narration ("GPU0 thermal throttling began — clocks 2520→1815 MHz").
    pub title: String,
    /// The raw evidence behind the narration, always auditable.
    pub evidence: String,
}

/// VRAM-trend window length and event cooldown.
const VRAM_WINDOW_MS: u64 = 180_000;
const VRAM_MIN_SPAN_MS: u64 = 60_000;
const VRAM_PRESSURE_FRAC: f64 = 0.85;
const VRAM_MIN_SLOPE_BYTES_PER_MIN: f64 = 16.0 * 1024.0 * 1024.0;
const VRAM_COOLDOWN_MS: u64 = 90_000;

/// Idle-gap (training stall) thresholds: a gap only narrates after sustained activity,
/// only once it lasted long enough to matter, and only if a real allocation stayed
/// attached throughout — otherwise it is just an idle GPU, not a stall.
const IDLE_ACTIVE_UTIL_PCT: f32 = 50.0;
const IDLE_ACTIVE_MIN_MS: u64 = 30_000;
const IDLE_GAP_UTIL_PCT: f32 = 10.0;
const IDLE_GAP_MIN_MS: u64 = 10_000;
const IDLE_HOLDER_MIN_BYTES: u64 = 256 * 1024 * 1024;

/// Hang-suspicion thresholds. The most confidently-wrong-prone inference in the product, so
/// the bar is deliberately steep: VRAM held but engines flat-dead, the holder's own util
/// also flat, and the whole pattern sustained for ten unbroken minutes before we dare say
/// "likely hung". A live trough that recovers, or any GPU activity, must not trip it.
const HANG_DEVICE_UTIL_PCT: f32 = 2.0;
const HANG_PROC_UTIL_PCT: f32 = 2.0;
const HANG_HOLDER_MIN_BYTES: u64 = 1024 * 1024 * 1024;
const HANG_RESET_UTIL_PCT: f32 = 10.0;
const HANG_MIN_MS: u64 = 600_000;

/// CPU-spillover thresholds. A freshly-loaded model that sits on a near-idle GPU while its
/// own process pegs multiple cores is the signature of a partial CPU offload (the model did
/// not fit in VRAM). We assess over a fixed window so a model still warming up is not judged
/// prematurely, and demand a high CPU bar plus a near-dead GPU before claiming it.
const SPILLOVER_HOLDER_MIN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SPILLOVER_WINDOW_MS: u64 = 90_000;
const SPILLOVER_MAX_MEAN_UTIL_PCT: f64 = 15.0;
const SPILLOVER_BUSY_UTIL_PCT: f32 = 30.0;
const SPILLOVER_MIN_MEAN_CPU_PCT: f64 = 150.0;
const SPILLOVER_MIN_CPU_SAMPLES: u32 = 3;

#[derive(Default)]
struct DevState {
    prev: Option<DynamicSample>,
    procs: HashMap<u32, ProcessSample>,
    seen_first_procs: bool,
    throttle_since: Option<u64>,
    /// Clock just before throttling began, for the "2520→1815 MHz" narration.
    pre_throttle_clock: Option<u32>,
    vram_window: VecDeque<(u64, u64)>,
    last_pressure_evt: Option<u64>,
    /// ts when util first crossed `IDLE_ACTIVE_UTIL_PCT`; None while below it.
    active_since: Option<u64>,
    /// Latched after `IDLE_ACTIVE_MIN_MS` of sustained activity: a trough only reads
    /// as a training stall if real work preceded it (a desktop idling is no story).
    idle_eligible: bool,
    idle_gap: Option<IdleGap>,
    hang: Option<HangEpisode>,
    /// Open CPU-spillover assessments keyed by the holder's pid; one per new big-memory
    /// process, closed (and judged) when its window elapses or it is cancelled.
    spillovers: HashMap<u32, Spillover>,
}

/// An idle gap in flight: narrated (or discarded) only once util recovers and the
/// gap's duration is actually known.
struct IdleGap {
    start_ms: u64,
    /// Util of the sample just before the drop, for the "92% → 2%" evidence.
    pre_util_pct: f32,
    /// Running mean of util inside the gap (sum and sample count).
    util_sum: f64,
    util_n: u32,
    /// pid → (name, mem at gap start) for processes holding ≥ `IDLE_HOLDER_MIN_BYTES`
    /// when the gap opened; pruned as they exit. Empty at gap end means nobody stayed
    /// attached, so the process_exited event already tells the story.
    holders: HashMap<u32, (String, u64)>,
    /// Set when a `HangSuspected` event fired while this gap was open. A hang is just an
    /// idle gap that lasted long enough to look dead; narrating both for one trough would
    /// double-count the same incident, so the gap stays silent on recovery.
    hang_narrated: bool,
}

/// A hang suspicion in flight: VRAM held with both device and holder engines flat-dead,
/// anchored to the largest qualifying holder. Emits once the pattern survives `HANG_MIN_MS`
/// unbroken; reset (without emitting) the moment activity returns, the holder exits, or
/// util goes unobservable.
struct HangEpisode {
    start_ms: u64,
    /// The largest qualifying holder when the episode opened — the anchor of the narration.
    /// A different (or larger) holder appearing later does not move the anchor; the claim is
    /// about *this* allocation having gone quiet.
    holder_pid: u32,
    holder_name: String,
    holder_mem: u64,
    /// Running mean of device util across the episode, for the evidence line.
    util_sum: f64,
    util_n: u32,
    /// Latched once the event has been emitted, so a sustained hang narrates exactly once.
    fired: bool,
}

/// A CPU-spillover assessment in flight for one freshly-attached big-memory process.
struct Spillover {
    name: String,
    mem_bytes: u64,
    start_ms: u64,
    util_sum: f64,
    util_n: u32,
    cpu_sum: f64,
    cpu_n: u32,
}

#[derive(Default)]
pub struct EventEngine {
    state: HashMap<DeviceId, DevState>,
    short_names: HashMap<DeviceId, String>,
}

impl EventEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a friendly short name ("GPU0") used in narration.
    pub fn register_device(&mut self, id: DeviceId, short_name: String) {
        self.short_names.insert(id, short_name);
    }

    fn short(&self, id: &DeviceId) -> String {
        self.short_names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.0.clone())
    }

    pub fn observe(
        &mut self,
        device: &DeviceId,
        sample: &DynamicSample,
        processes: &[ProcessSample],
        mem_total: Option<u64>,
        temp_slowdown_c: Option<f32>,
    ) -> Vec<Event> {
        let name = self.short(device);
        let st = self.state.entry(device.clone()).or_default();
        let mut out = Vec::new();

        throttle_events(st, device, &name, sample, temp_slowdown_c, &mut out);
        // Before process_events flips `seen_first_procs` / overwrites `st.procs`, so the
        // newness diff that opens a spillover window sees this tick's arrivals.
        spillover_events(st, device, &name, sample, processes, &mut out);
        process_events(st, device, &name, sample.ts_ms, processes, &mut out);
        // After process_events, so holder tracking sees this tick's process list.
        hang_events(st, device, &name, sample, &mut out);
        // After hang_events, so a hang that just fired can suppress the gap it lived in.
        idle_gap_events(st, device, &name, sample, &mut out);
        vram_pressure_events(st, device, &name, sample, mem_total, &mut out);

        st.prev = Some(sample.clone());
        out
    }
}

fn throttle_events(
    st: &mut DevState,
    device: &DeviceId,
    name: &str,
    sample: &DynamicSample,
    temp_slowdown_c: Option<f32>,
    out: &mut Vec<Event>,
) {
    // Throttle unobservable on this source (`None` ≠ "not throttling" — design §5.4):
    // neither a start nor an end can be asserted, so drop the open episode silently —
    // the same blind-spot rule util uses for idle gaps and hangs. Narrating an "end"
    // off a blind spot would be a fabricated fact.
    let Some(throttle) = sample.throttle else {
        st.throttle_since = None;
        st.pre_throttle_clock = None;
        return;
    };
    let prev_any = st
        .prev
        .as_ref()
        .and_then(|p| p.throttle)
        .map(|t| t.any())
        .unwrap_or(false);
    let now_any = throttle.any();

    if !prev_any && now_any {
        let labels = throttle.labels().join(", ");
        let pre_clock = st.prev.as_ref().and_then(|p| p.sm_clock_mhz);
        st.pre_throttle_clock = pre_clock;
        st.throttle_since = Some(sample.ts_ms);

        let clocks = match (pre_clock, sample.sm_clock_mhz) {
            (Some(a), Some(b)) if b < a => format!(" — clocks {a}→{b} MHz"),
            _ => String::new(),
        };
        let temp_part = match (sample.temp_c, temp_slowdown_c) {
            (Some(t), Some(thr)) => format!("; {t:.0}°C vs {thr:.0}°C slowdown threshold"),
            (Some(t), None) => format!("; {t:.0}°C"),
            _ => String::new(),
        };
        out.push(Event {
            ts_ms: sample.ts_ms,
            device: device.clone(),
            kind: EventKind::ThrottleStart,
            severity: severity_for(&throttle),
            confidence: Confidence::Fact,
            title: format!("{name} began throttling ({labels}){clocks}"),
            evidence: format!("throttle bits: [{labels}]{temp_part}"),
        });
    } else if prev_any && !now_any {
        let dur = st
            .throttle_since
            .take()
            .map(|t0| format!(" after {}", fmt_dur_ms(sample.ts_ms.saturating_sub(t0))))
            .unwrap_or_default();
        // Only claim "recovered" when clocks are actually back near pre-throttle levels;
        // a throttle that ends because the GPU went idle is not a recovery.
        let clocks = match (st.pre_throttle_clock.take(), sample.sm_clock_mhz) {
            (Some(a), Some(b)) if b as f64 >= a as f64 * 0.9 => {
                format!("; clocks recovered to {b} MHz")
            }
            (Some(a), Some(b)) => format!("; clocks now {b} MHz ({a} MHz pre-throttle)"),
            _ => String::new(),
        };
        out.push(Event {
            ts_ms: sample.ts_ms,
            device: device.clone(),
            kind: EventKind::ThrottleEnd,
            severity: Severity::Info,
            confidence: Confidence::Fact,
            title: format!("{name} stopped throttling{dur}"),
            evidence: format!("throttle bits cleared{clocks}"),
        });
    }
}

fn process_events(
    st: &mut DevState,
    device: &DeviceId,
    name: &str,
    ts_ms: u64,
    processes: &[ProcessSample],
    out: &mut Vec<Event>,
) {
    let now: HashMap<u32, &ProcessSample> = processes.iter().map(|p| (p.pid, p)).collect();

    // Suppress the attach-flood on the very first observation: those processes were already
    // there; narrating them as new would be a lie.
    if st.seen_first_procs {
        for (pid, p) in &now {
            if !st.procs.contains_key(pid) {
                let mem = p
                    .mem_bytes
                    .map(|b| format!(", using {}", fmt_bytes(b)))
                    .unwrap_or_default();
                out.push(Event {
                    ts_ms,
                    device: device.clone(),
                    kind: EventKind::ProcessAttached,
                    severity: Severity::Info,
                    confidence: Confidence::Fact,
                    title: format!("{} (pid {}) attached to {name}{mem}", p.name, pid),
                    evidence: format!("new {} client in process list", p.kind.prose()),
                });
            }
        }
        let gone: Vec<ProcessSample> = st
            .procs
            .values()
            .filter(|p| !now.contains_key(&p.pid))
            .cloned()
            .collect();
        for p in gone {
            let freed = p
                .mem_bytes
                .map(|b| format!(", freeing {}", fmt_bytes(b)))
                .unwrap_or_default();
            out.push(Event {
                ts_ms,
                device: device.clone(),
                kind: EventKind::ProcessExited,
                severity: Severity::Info,
                confidence: Confidence::Fact,
                title: format!("{} (pid {}) left {name}{freed}", p.name, p.pid),
                evidence: format!(
                    "pid {} no longer in process list; last seen holding {}",
                    p.pid,
                    p.mem_bytes
                        .map(fmt_bytes)
                        .unwrap_or_else(|| "unknown memory".into())
                ),
            });
        }
    }
    st.seen_first_procs = true;
    st.procs = now.into_iter().map(|(k, v)| (k, v.clone())).collect();
}

fn idle_gap_events(
    st: &mut DevState,
    device: &DeviceId,
    name: &str,
    sample: &DynamicSample,
    out: &mut Vec<Event>,
) {
    let Some(util) = sample.util_pct else {
        // Utilization went unavailable: we can no longer see activity or idleness, so
        // any gap claim from here on would be guesswork. Drop all tracking instead.
        st.active_since = None;
        st.idle_eligible = false;
        st.idle_gap = None;
        return;
    };

    if let Some(mut gap) = st.idle_gap.take() {
        // Holders must stay attached for the WHOLE gap; one that exits mid-gap is
        // already narrated by process_exited — an idle_gap on top would double-count.
        gap.holders.retain(|pid, _| st.procs.contains_key(pid));

        if util < IDLE_ACTIVE_UTIL_PCT {
            gap.util_sum += util as f64;
            gap.util_n += 1;
            st.idle_gap = Some(gap);
            return;
        }

        // Gap over — its duration is finally known, so decide whether it narrates.
        let dur_ms = sample.ts_ms.saturating_sub(gap.start_ms);
        let holder = gap
            .holders
            .iter()
            .max_by_key(|(_, (_, mem))| *mem)
            .map(|(pid, (pname, mem))| (*pid, pname.clone(), *mem));
        if dur_ms >= IDLE_GAP_MIN_MS && !gap.hang_narrated {
            if let Some((pid, pname, mem)) = holder {
                let dur = fmt_dur_ms(dur_ms);
                let mean_util = gap.util_sum / gap.util_n.max(1) as f64;
                out.push(Event {
                    ts_ms: sample.ts_ms,
                    device: device.clone(),
                    kind: EventKind::IdleGap,
                    severity: Severity::Info,
                    confidence: Confidence::Likely,
                    title: format!(
                        "{name} sat idle {dur} while {pname} (pid {pid}) stayed attached \
                         — likely a dataloader or checkpoint stall"
                    ),
                    evidence: format!(
                        "util {:.0}% → mean {mean_util:.1}% over {dur} ({}..{} ms); \
                         {pname} (pid {pid}) held {} for the whole gap",
                        gap.pre_util_pct,
                        gap.start_ms,
                        sample.ts_ms,
                        fmt_bytes(mem),
                    ),
                });
            }
        }
        // Recovery starts a fresh activity clock: the next gap only narrates after the
        // device has re-earned IDLE_ACTIVE_MIN_MS of sustained work.
        st.active_since = Some(sample.ts_ms);
        st.idle_eligible = false;
        return;
    }

    if util >= IDLE_ACTIVE_UTIL_PCT {
        let since = *st.active_since.get_or_insert(sample.ts_ms);
        if sample.ts_ms.saturating_sub(since) >= IDLE_ACTIVE_MIN_MS {
            st.idle_eligible = true;
        }
        return;
    }

    st.active_since = None;
    if util >= IDLE_GAP_UTIL_PCT || !st.idle_eligible {
        return;
    }
    // Gap opens. Capture who is attached with a real allocation right now; only they
    // can anchor the "stayed attached" claim when the gap ends.
    let holders: HashMap<u32, (String, u64)> = st
        .procs
        .values()
        .filter_map(|p| {
            let mem = p.mem_bytes?;
            (mem >= IDLE_HOLDER_MIN_BYTES).then(|| (p.pid, (p.name.clone(), mem)))
        })
        .collect();
    st.idle_gap = Some(IdleGap {
        start_ms: sample.ts_ms,
        pre_util_pct: st.prev.as_ref().and_then(|p| p.util_pct).unwrap_or(util),
        util_sum: util as f64,
        util_n: 1,
        holders,
        hang_narrated: false,
    });
}

/// `HangSuspected` — VRAM held, engines flat-dead, holder alive: the job has likely hung.
///
/// An inference of the riskiest kind, so the gate is steep: the device must be effectively
/// idle (`≤ HANG_DEVICE_UTIL_PCT`), a holder must be sitting on ≥ 1 GiB while its *own*
/// engine activity is also flat (or unreported), and that exact pattern must survive a full
/// `HANG_MIN_MS` without a break before we narrate. We anchor to the largest qualifying
/// holder at episode start and never re-anchor: the claim is that *this* allocation went
/// quiet. The episode is dropped (never narrated) the instant any premise stops holding —
/// the device wakes up, the holder exits, util goes unobservable, or even a sub-throttle
/// flicker of activity — because a hang we cannot stand fully behind is worse than silence.
fn hang_events(
    st: &mut DevState,
    device: &DeviceId,
    name: &str,
    sample: &DynamicSample,
    out: &mut Vec<Event>,
) {
    let Some(util) = sample.util_pct else {
        // Util unobservable: we cannot see "zero engine activity", so we cannot claim a
        // hang. Drop the episode rather than freeze a stale window across the blind spot.
        st.hang = None;
        return;
    };

    // The largest holder that is itself quiet: ≥ 1 GiB resident with its own util flat or
    // simply not reported (a hung kernel reports no per-process util — absence is expected).
    let candidate = st
        .procs
        .values()
        .filter(|p| p.mem_bytes.unwrap_or(0) >= HANG_HOLDER_MIN_BYTES)
        .filter(|p| p.util_pct.map(|u| u <= HANG_PROC_UTIL_PCT).unwrap_or(true))
        .max_by_key(|p| p.mem_bytes.unwrap_or(0));
    let condition = util <= HANG_DEVICE_UTIL_PCT && candidate.is_some();

    if let Some(mut ep) = st.hang.take() {
        // The anchored holder must still be alive; if it exited, `process_exited` already
        // told the story and the premise ("process still alive") is gone.
        let holder_alive = st.procs.contains_key(&ep.holder_pid);
        if util > HANG_RESET_UTIL_PCT || !holder_alive || !condition {
            // Any break ends the episode silently — continuity is the whole claim.
            return;
        }
        ep.util_sum += util as f64;
        ep.util_n += 1;
        let elapsed = sample.ts_ms.saturating_sub(ep.start_ms);
        if elapsed >= HANG_MIN_MS && !ep.fired {
            ep.fired = true;
            let mean_util = ep.util_sum / ep.util_n.max(1) as f64;
            let dur = fmt_dur_ms(elapsed);
            out.push(Event {
                ts_ms: sample.ts_ms,
                device: device.clone(),
                kind: EventKind::HangSuspected,
                severity: Severity::Warning,
                confidence: Confidence::Likely,
                title: format!(
                    "{name}: {} (pid {}) likely hung — held {} for {dur} with zero GPU \
                     activity, process still alive",
                    ep.holder_name,
                    ep.holder_pid,
                    fmt_bytes(ep.holder_mem),
                ),
                evidence: format!(
                    "device util mean {mean_util:.1}% over {dur} ({}..{} ms); \
                     {} (pid {}) held {} throughout while its own engine activity stayed flat",
                    ep.start_ms,
                    sample.ts_ms,
                    ep.holder_name,
                    ep.holder_pid,
                    fmt_bytes(ep.holder_mem),
                ),
            });
            // A hang is an idle gap that lasted too long to look alive; if a gap is still
            // open over this same trough, mute it so one incident is narrated once.
            if let Some(gap) = st.idle_gap.as_mut() {
                gap.hang_narrated = true;
            }
        }
        st.hang = Some(ep);
        return;
    }

    if condition {
        let holder = candidate.expect("condition implies a candidate");
        st.hang = Some(HangEpisode {
            start_ms: sample.ts_ms,
            holder_pid: holder.pid,
            holder_name: holder.name.clone(),
            holder_mem: holder.mem_bytes.unwrap_or(0),
            util_sum: util as f64,
            util_n: 1,
            fired: false,
        });
    }
}

/// `CpuSpillover` — a freshly-loaded model whose GPU stays idle while its process burns
/// CPU: the signature of a partial CPU offload (the model did not fit in VRAM).
///
/// We open a fixed `SPILLOVER_WINDOW_MS` assessment when a *new* process attaches holding
/// ≥ 2 GiB, then judge at window close: narrate only if the GPU averaged near-idle while the
/// process averaged multiple busy cores, with enough CPU samples to mean it. The assessment
/// is cancelled silently — never narrated — if the process exits mid-window, the GPU shows
/// real use at any point, or (honesty rule) we never once saw its CPU: with no CPU
/// visibility we cannot claim it is "burning CPU", so we say nothing rather than guess.
fn spillover_events(
    st: &mut DevState,
    device: &DeviceId,
    name: &str,
    sample: &DynamicSample,
    processes: &[ProcessSample],
    out: &mut Vec<Event>,
) {
    let now: HashMap<u32, &ProcessSample> = processes.iter().map(|p| (p.pid, p)).collect();

    // Open a window for each newly-attached big-memory holder. Skip the first observation:
    // those processes were already resident, not freshly loaded, so they are no story.
    if st.seen_first_procs {
        for (pid, p) in &now {
            if st.procs.contains_key(pid) || st.spillovers.contains_key(pid) {
                continue;
            }
            if p.mem_bytes.unwrap_or(0) >= SPILLOVER_HOLDER_MIN_BYTES {
                st.spillovers.insert(
                    *pid,
                    Spillover {
                        name: p.name.clone(),
                        mem_bytes: p.mem_bytes.unwrap_or(0),
                        start_ms: sample.ts_ms,
                        util_sum: 0.0,
                        util_n: 0,
                        cpu_sum: 0.0,
                        cpu_n: 0,
                    },
                );
            }
        }
    }

    if st.spillovers.is_empty() {
        return;
    }

    // A device showing real use cancels every open assessment at once: the premise of the
    // whole inference is that the GPU is idle, and one busy reading refutes it.
    let gpu_busy = sample
        .util_pct
        .map(|u| u >= SPILLOVER_BUSY_UTIL_PCT)
        .unwrap_or(false);

    let mut to_emit: Vec<Event> = Vec::new();
    st.spillovers.retain(|pid, sp| {
        if gpu_busy {
            return false;
        }
        let Some(p) = now.get(pid) else {
            // Exited mid-window: cancelled silently (its `process_exited` fact stands).
            return false;
        };
        if let Some(u) = sample.util_pct {
            sp.util_sum += u as f64;
            sp.util_n += 1;
        }
        if let Some(c) = p.cpu_pct {
            sp.cpu_sum += c as f64;
            sp.cpu_n += 1;
        }

        if sample.ts_ms.saturating_sub(sp.start_ms) < SPILLOVER_WINDOW_MS {
            return true; // window still open
        }

        // Window closed — judge. Means require samples; no CPU sample at all means no CPU
        // visibility, and we refuse to claim a CPU burn we never observed.
        let mean_util = sp.util_sum / sp.util_n.max(1) as f64;
        let mean_cpu = sp.cpu_sum / sp.cpu_n.max(1) as f64;
        if sp.cpu_n >= SPILLOVER_MIN_CPU_SAMPLES
            && mean_util < SPILLOVER_MAX_MEAN_UTIL_PCT
            && mean_cpu >= SPILLOVER_MIN_MEAN_CPU_PCT
        {
            let span = fmt_dur_ms(sample.ts_ms.saturating_sub(sp.start_ms));
            to_emit.push(Event {
                ts_ms: sample.ts_ms,
                device: device.clone(),
                kind: EventKind::CpuSpillover,
                severity: Severity::Warning,
                confidence: Confidence::Likely,
                title: format!(
                    "{} (pid {pid}) loaded {} but {name} is ~idle while its CPU runs hot \
                     — likely partial CPU offload (model may not fit in VRAM)",
                    sp.name,
                    fmt_bytes(sp.mem_bytes),
                ),
                evidence: format!(
                    "over {span} ({}..{} ms): {name} util mean {mean_util:.1}%, \
                     {} (pid {pid}) CPU mean {mean_cpu:.0}% of one core ({} samples)",
                    sp.start_ms, sample.ts_ms, sp.name, sp.cpu_n,
                ),
            });
        }
        false // window done either way
    });
    out.extend(to_emit);
}

fn vram_pressure_events(
    st: &mut DevState,
    device: &DeviceId,
    name: &str,
    sample: &DynamicSample,
    mem_total: Option<u64>,
    out: &mut Vec<Event>,
) {
    let (Some(used), Some(total)) = (sample.mem_used_bytes, mem_total) else {
        return;
    };
    if total == 0 {
        return;
    }

    // A sharp drop (process exit, allocator reset) invalidates the trend: an endpoint
    // slope over a window straddling the old peak would understate the *current* climb
    // rate — wrong in the dangerous direction. Restart the window instead.
    if let Some(&(_, last_used)) = st.vram_window.back() {
        if last_used.saturating_sub(used) > total / 20 {
            st.vram_window.clear();
        }
    }

    st.vram_window.push_back((sample.ts_ms, used));
    while let Some(&(t0, _)) = st.vram_window.front() {
        if sample.ts_ms.saturating_sub(t0) > VRAM_WINDOW_MS {
            st.vram_window.pop_front();
        } else {
            break;
        }
    }

    let frac = used as f64 / total as f64;
    if frac < VRAM_PRESSURE_FRAC {
        return;
    }
    let (&(t0, b0), &(t1, b1)) = match (st.vram_window.front(), st.vram_window.back()) {
        (Some(a), Some(b)) if t_span(a, b) >= VRAM_MIN_SPAN_MS => (a, b),
        _ => return,
    };
    let span_min = (t1 - t0) as f64 / 60_000.0;
    let slope_per_min = (b1 as f64 - b0 as f64) / span_min;
    if slope_per_min < VRAM_MIN_SLOPE_BYTES_PER_MIN {
        return;
    }
    if let Some(last) = st.last_pressure_evt {
        if sample.ts_ms.saturating_sub(last) < VRAM_COOLDOWN_MS {
            return;
        }
    }
    st.last_pressure_evt = Some(sample.ts_ms);

    let headroom = total.saturating_sub(used) as f64;
    let eta_min = headroom / slope_per_min;
    // Only name a "largest holder" when at least one process has a *known* size —
    // with mem_bytes all-None (WSL2, unprivileged fdinfo) max_by_key would crown an
    // arbitrary process on zero evidence.
    let grower = st
        .procs
        .values()
        .filter(|p| p.mem_bytes.is_some())
        .max_by_key(|p| p.mem_bytes)
        .map(|p| format!(" (largest holder: {} pid {})", p.name, p.pid))
        .unwrap_or_default();

    out.push(Event {
        ts_ms: sample.ts_ms,
        device: device.clone(),
        kind: EventKind::VramPressure,
        severity: Severity::Warning,
        confidence: Confidence::Likely,
        title: format!(
            "{name} VRAM {:.0}% and climbing ~{}/min — likely full in ~{:.0} min{grower}",
            frac * 100.0,
            fmt_bytes(slope_per_min as u64),
            eta_min
        ),
        evidence: format!(
            "used {}/{} ({:.1}%); slope +{}/min over last {:.1} min (linear extrapolation)",
            fmt_bytes(used),
            fmt_bytes(total),
            frac * 100.0,
            fmt_bytes(slope_per_min as u64),
            span_min
        ),
    });
}

fn severity_for(t: &ThrottleReasons) -> Severity {
    if t.hw_slowdown {
        Severity::Critical
    } else {
        Severity::Warning
    }
}

fn t_span(a: &(u64, u64), b: &(u64, u64)) -> u64 {
    b.0.saturating_sub(a.0)
}

fn fmt_dur_ms(ms: u64) -> String {
    // Round to nearest second — a 3.9s episode is "4s", not "3s".
    let s = (ms + 500) / 1000;
    if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
