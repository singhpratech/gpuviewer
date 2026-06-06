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
        self.short_names.get(id).cloned().unwrap_or_else(|| id.0.clone())
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
        process_events(st, device, &name, sample.ts_ms, processes, &mut out);
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
    let prev_any = st.prev.as_ref().map(|p| p.throttle.any()).unwrap_or(false);
    let now_any = sample.throttle.any();

    if !prev_any && now_any {
        let labels = sample.throttle.labels().join(", ");
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
            severity: severity_for(&sample.throttle),
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
                let mem = p.mem_bytes.map(|b| format!(", using {}", fmt_bytes(b))).unwrap_or_default();
                out.push(Event {
                    ts_ms,
                    device: device.clone(),
                    kind: EventKind::ProcessAttached,
                    severity: Severity::Info,
                    confidence: Confidence::Fact,
                    title: format!("{} (pid {}) attached to {name}{mem}", p.name, pid),
                    evidence: format!("new {} client in process list", p.kind.label()),
                });
            }
        }
        let gone: Vec<ProcessSample> =
            st.procs.values().filter(|p| !now.contains_key(&p.pid)).cloned().collect();
        for p in gone {
            let freed = p.mem_bytes.map(|b| format!(", freeing {}", fmt_bytes(b))).unwrap_or_default();
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
                    p.mem_bytes.map(fmt_bytes).unwrap_or_else(|| "unknown memory".into())
                ),
            });
        }
    }
    st.seen_first_procs = true;
    st.procs = now.into_iter().map(|(k, v)| (k, v.clone())).collect();
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
    let grower = st
        .procs
        .values()
        .max_by_key(|p| p.mem_bytes.unwrap_or(0))
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
    let s = ms / 1000;
    if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
