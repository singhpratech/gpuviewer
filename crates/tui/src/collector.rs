//! Collection engine shared by the TUI thread and `--json` mode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpuviewer_core::{
    all_backends, normalize_pci_id, now_ms, Confidence, DeviceId, DynamicSample, Event,
    EventEngine, EventKind, GpuBackend, ProcessSample, Severity, StaticInfo,
};
use gpuviewer_history::{DataSource, HistoryStore, Recorder, SqliteStore};
use serde::Serialize;

/// One collection tick's output for one device.
#[derive(Serialize)]
pub struct FrameDevice {
    pub id: DeviceId,
    pub name: String,
    /// Total device memory, so JSON consumers can compute used/total without a second
    /// query. VRAM on discrete boards; a working-set budget on unified-memory devices
    /// (the spec and the per-device `source_caveat` carry that label).
    pub mem_total_bytes: Option<u64>,
    pub sample: Option<DynamicSample>,
    pub processes: Vec<ProcessSample>,
}

/// One collection tick across all devices. Internal shape only: `--json` serializes the
/// envelope structs in `main.rs` instead (events go out as separate lines, per
/// docs/spec/ndjson-v1.md), so this deliberately does not derive `Serialize`.
pub struct Frame {
    pub ts_ms: u64,
    pub devices: Vec<FrameDevice>,
    pub events: Vec<Event>,
}

// `normalize_pci_id` (cross-backend dedupe key) moved to `gpuviewer-core::model` per
// design cross-platform.md §5.4, so the Windows backends' LUID↔PCI matching and this
// collector share one rule; re-exported from core and imported above.

/// How long a between-tick gap must reach before the next tick narrates a `CollectorStall`:
/// the larger of three intervals or five seconds. A blocked backend probe leaves a hole in
/// the recording, and the flight recorder must say so rather than let the gap read as the
/// GPU having gone quiet. Pure (no clock, no I/O) so the threshold is unit-tested directly.
pub fn stall_threshold(interval: Duration) -> Duration {
    (interval * 3).max(Duration::from_secs(5))
}

/// Whether an observed inter-tick `gap` is long enough to count as a stall, given the
/// configured `interval`. Factored out so the boundary (exactly `3×interval` vs. just over)
/// is testable without spinning a real tick loop.
pub fn is_stall(gap: Duration, interval: Duration) -> bool {
    gap > stall_threshold(interval)
}

/// Consecutive panicked ticks the collector survives before declaring itself dead. One
/// panicked tick is dropped-frame weather (a decoder edge case on one bad sample — the
/// same per-tick family as `NVML_ERROR_NOT_SUPPORTED` or an absent sysfs file); this many
/// in a row is a deterministic fault, and retrying forever would narrate an event per tick
/// while burning a core on a known-broken probe.
pub const MAX_CONSECUTIVE_PANICS: u32 = 3;

/// Consecutive failed dynamic probes (`refresh_dynamic` → `Err`) before a device is
/// declared LOST. Per-metric absence never reaches this counter — `NOT_SUPPORTED` and a
/// missing sysfs file are `None` INSIDE an `Ok` sample (normal weather, rendered
/// "unavailable"); an `Err` is the whole probe producing nothing, the trait's documented
/// "device fell off the bus" shape. One or two of those are still transient (a driver
/// mid-reset drops a few queries and recovers); this many in a row is the device being
/// gone, and the audit's silently-vanishing-device hole (nvtop #459 is the genre's
/// cautionary tale: a device disappearing mid-run must be narrated, not crashed on or
/// ignored) demands the recorder say so as a fact.
///
/// The threshold counts TICKS, not wall-time — deterministic and testable. At the default
/// 1s interval five misses ≈ 5s of silence, matching the 5s floor of [`stall_threshold`]:
/// the recorder's established line for "this gap is worth narrating". A failing probe
/// pins full cadence ([`device_is_idle`] treats `sample: None` as not-idle), so after the
/// first failure the remaining misses are spaced at the configured interval; only the gap
/// *into* the first failure can be backoff-stretched. The event's evidence therefore
/// reports the measured wall-time alongside the tick count rather than assuming one from
/// the other.
pub const DEVICE_LOST_AFTER_FAILED_PROBES: u32 = 5;

/// Per-device dynamic-probe health, parallel to `Engine::devices` (collection order) —
/// the state behind the `device_lost`/`device_returned` facts.
#[derive(Clone, Default)]
struct ProbeHealth {
    /// Consecutive `refresh_dynamic` failures; any success resets it.
    consecutive_failures: u32,
    /// Unix-millis of the current failing streak's first failure (valid while
    /// `consecutive_failures > 0`) — the basis of the measured-wall-time evidence.
    first_fail_ms: u64,
    /// First and last error strings of the current streak (often identical — the
    /// evidence dedupes them, but a cause that *changes* mid-streak is worth keeping).
    first_error: String,
    last_error: String,
    /// `ts_ms` of the last successful sample; `None` until the device first answers.
    last_good_ms: Option<u64>,
    /// `Some(unix ms when declared)` while the device is lost; cleared on return.
    lost_at_ms: Option<u64>,
}

/// What one guarded tick ([`Engine::tick_guarded`]) produced. `Frame` is the normal path;
/// `Panicked` means the tick body panicked and its frame is lost — the carried event
/// narrates that hole (already persisted and `--on-event`-fired where those channels
/// survived), and `fatal` tells the caller whether the panic budget is spent and the
/// loop must stop, loudly.
pub enum TickOutcome {
    Frame(Frame),
    Panicked {
        /// The `CollectorStall` fact narrating this panic.
        event: Event,
        /// One-line panic message, for [`Shared::stopped`] and the stderr trail.
        summary: String,
        /// True once [`MAX_CONSECUTIVE_PANICS`] ticks panicked consecutively: the caller
        /// must stop collecting and say so — never keep rendering as if live.
        fatal: bool,
    },
}

/// Best-effort one-liner from a panic payload: `panic!`, `assert!`, and the built-in
/// index/overflow panics all carry `&str` or `String`; anything else is named rather
/// than displayed — the message must never be swallowed entirely.
fn panic_summary(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".into()
    }
}

/// The `device_lost` fact. Severity is CRITICAL — the per-device analog of the
/// collector's own Critical stop fact: this device's recording ends here (until a
/// return), and a device vanishing under load almost always took its workload with it,
/// which is exactly the incident a flight recorder exists to capture. Confidence is
/// FACT: the claim is precisely "N consecutive probes produced nothing", which was
/// observed; the CAUSE is deliberately not asserted — driver reset, device removal, and
/// library death are indistinguishable from this side of the probe, and a guessed cause
/// would break the two-tier honesty contract.
fn device_lost_event(di: usize, id: &DeviceId, h: &ProbeHealth, at_ms: u64) -> Event {
    let wall = Duration::from_millis(at_ms.saturating_sub(h.first_fail_ms));
    let last_good = match h.last_good_ms {
        Some(ts) => format!("last good data {}", fmt_clock(ts)),
        None => "it never answered this session".to_string(),
    };
    let last_good_ev = match h.last_good_ms {
        Some(ts) => format!("last good sample at {ts} ms ({})", fmt_clock(ts)),
        None => "no good sample this session".to_string(),
    };
    let errors = if h.first_error == h.last_error {
        format!("error: {}", h.last_error)
    } else {
        format!(
            "first error: {}; last error: {}",
            h.first_error, h.last_error
        )
    };
    Event {
        ts_ms: at_ms,
        device: id.clone(),
        kind: EventKind::DeviceLost,
        severity: Severity::Critical,
        confidence: Confidence::Fact,
        title: format!(
            "GPU{di} stopped answering — device lost after {} consecutive failed probes; \
             {last_good}",
            h.consecutive_failures
        ),
        evidence: format!(
            "refresh_dynamic for {id} failed {} consecutive ticks over {} of wall time \
             (the threshold counts ticks; a failing probe pins full cadence, so only the \
             gap into the first failure can be backoff-stretched); {last_good_ev}; \
             {errors}; cause not asserted — driver reset, device removal, and library \
             death look identical from here",
            h.consecutive_failures,
            fmt_dur(wall),
        ),
    }
}

/// The `device_returned` fact — the recovery edge, so the story has both ends. Severity
/// is INFO: good news plainly stated (the Critical loss already alerted; alarming again
/// on recovery would teach users to tune the feed out). Still a FACT: "the probe
/// succeeded again" is observed.
fn device_returned_event(
    di: usize,
    id: &DeviceId,
    h: &ProbeHealth,
    lost_at_ms: u64,
    ts_ms: u64,
) -> Event {
    // The hole as the user experienced it runs from the last GOOD data, not from the
    // (later) loss declaration — the data went quiet before the verdict did.
    let gap_start = h.last_good_ms.unwrap_or(h.first_fail_ms);
    let gap = Duration::from_millis(ts_ms.saturating_sub(gap_start));
    Event {
        ts_ms,
        device: id.clone(),
        kind: EventKind::DeviceReturned,
        severity: Severity::Info,
        confidence: Confidence::Fact,
        title: format!(
            "GPU{di} answering again — device returned; {} of data are missing",
            fmt_dur(gap)
        ),
        evidence: format!(
            "refresh_dynamic for {id} succeeded after {} consecutive failures; declared \
             lost at {}; nothing was collected between {} and {} — the gap stays blank \
             in history, never zero-filled",
            h.consecutive_failures,
            fmt_clock(lost_at_ms),
            fmt_clock(gap_start),
            fmt_clock(ts_ms),
        ),
    }
}

/// The `recording_started` fact — the session-boundary mark on the recording's left edge.
/// WHY: the timeline renders unrecorded time blank, and without boundary marks a blank
/// stretch is ambiguous — "the GPU sat idle from 02:00–08:00" and "gpuviewer wasn't
/// running" look identical, an honesty hole for a product whose thesis is the trustworthy
/// recording. Info severity (routine lifecycle, not an alarm), FACT (the session opening
/// is observed). `dangling_start_ms` is the previous session's start mark when it has no
/// matching stop: that session died without flushing (crash, kill, power loss), and the
/// asymmetry is itself flight-recorder information — narrated here because the dead
/// session, by definition, could not narrate it.
fn recording_started_event(
    ts_ms: u64,
    anchor: DeviceId,
    interval: Duration,
    backend_list: &str,
    n_devices: usize,
    db_name: &str,
    dangling_start_ms: Option<u64>,
) -> Event {
    let title = match dangling_start_ms {
        Some(_) => "recording started — previous session ended without a stop mark \
                    (crash, kill, or power loss)"
            .to_string(),
        None => format!(
            "recording started — gpuviewer {}",
            env!("CARGO_PKG_VERSION")
        ),
    };
    let devices_word = if n_devices == 1 { "device" } else { "devices" };
    let mut evidence = format!(
        "gpuviewer {}; interval {}; backends: {backend_list} ({n_devices} {devices_word}); \
         db {db_name}",
        env!("CARGO_PKG_VERSION"),
        fmt_dur(interval),
    );
    if let Some(prev) = dangling_start_ms {
        evidence.push_str(&format!(
            "; the previous session's start mark at {} has no matching stop — it died \
             without flushing (crash, kill, or power loss), so where its recording \
             actually ends is unknown and the gap size is unknowable",
            fmt_stamp(prev)
        ));
    }
    Event {
        ts_ms,
        device: anchor,
        kind: EventKind::RecordingStarted,
        severity: Severity::Info,
        confidence: Confidence::Fact,
        title,
        evidence,
    }
}

/// A single backend `refresh_dynamic` probe taking longer than this is worth a one-line
/// Info note ("nvidia probe took 1.2s — driver slow to respond"): NVML/sysfs calls can block
/// (PCIe-throughput queries, a sleeping GPU waking) and a slow probe foreshadows a stall.
const SLOW_PROBE: Duration = Duration::from_millis(700);

/// Rate-limit the per-device slow-probe note: at most one per this window per device, so a
/// persistently-slow driver does not flood the story feed.
const SLOW_PROBE_COOLDOWN: Duration = Duration::from_secs(300);

/// Configuration for [`Engine::new`]. Carries the knobs the collector needs that are not
/// derivable from the backend set: whether to force the mock, where (if anywhere) to persist,
/// the configured tick interval (for the stall threshold), and an optional `--on-event` sink.
pub struct EngineConfig {
    /// Use ONLY the simulated GPUs (the mock is the no-real-GPU fallback regardless).
    pub force_mock: bool,
    /// Open the SQLite store and record. Off → live-only, the monitor still runs.
    pub persist: bool,
    /// Override the history database path (else the per-OS default, mock-separated).
    pub db_path: Option<PathBuf>,
    /// The configured tick interval — the basis of the stall-gap threshold.
    pub interval: Duration,
    /// Shell command run for every emitted event (`sh -c CMD`; `cmd /C CMD` on Windows),
    /// or `None`.
    pub on_event: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            force_mock: false,
            persist: false,
            db_path: None,
            interval: Duration::from_millis(1000),
            on_event: None,
        }
    }
}

pub struct Engine {
    backends: Vec<Box<dyn GpuBackend>>,
    /// (backend index, device id, static info)
    devices: Vec<(usize, DeviceId, StaticInfo)>,
    event_engine: EventEngine,
    /// SQLite recorder, present only when persistence opened successfully.
    recorder: Option<Recorder>,
    /// Path of the open store, for the UI/replay (`None` when not persisting).
    db_path: Option<PathBuf>,
    /// The configured interval, for the stall threshold.
    interval: Duration,
    /// Wall instant the last tick finished — the basis for stall-gap detection.
    last_tick_end: Option<Instant>,
    /// Last time we emitted a slow-probe note for a given device.
    last_slow_note: HashMap<DeviceId, Instant>,
    /// A pending `HistoryReset` to fold into the very first tick's events (the store reported
    /// it had to quarantine a corrupt file and start fresh — say so, once).
    pending_reset: Option<Event>,
    /// The session's `recording_started` mark, queued to ride the first tick's frame so the
    /// `--json` stream, `--on-event`, and the story feed all see it like any other event.
    /// It is ALSO inserted into the store at construction (the mark must exist even if the
    /// process dies before its first tick completes — a first probe can block for minutes);
    /// the event log's UNIQUE dedupe index collapses the first tick's re-insert.
    pending_session_start: Option<Event>,
    /// `--on-event` sink, shared with the JSON path so both modes fire it.
    sink: Option<EventSink>,
    /// Consecutive panicked ticks ([`Engine::tick_guarded`]); any clean tick resets it.
    consecutive_panics: u32,
    /// Per-device probe health (parallel to `devices`) — drives device-lost/returned.
    health: Vec<ProbeHealth>,
    /// Unix-millis the session began — the basis of the stop mark's duration evidence.
    session_start_ms: u64,
    /// Clean ticks folded this session, for the stop mark's evidence.
    frames_folded: u64,
    /// Latched once [`Engine::finish`] wrote the stop mark, so the explicit shutdown paths
    /// and `Drop` (which both call it) can never record the session's end twice.
    finished: bool,
}

impl Engine {
    /// Build the engine from a configuration. Device discovery and dedupe are unchanged from
    /// the live-only path; persistence is best-effort: a store that fails to open (disk full,
    /// permission) logs to stderr and the monitor continues WITHOUT a recorder — losing the
    /// recording is never worth crashing the live view.
    pub fn new(config: EngineConfig) -> Self {
        Self::with_backends(all_backends(config.force_mock), config)
    }

    /// [`Engine::new`] over an explicit backend set instead of the registry — the seam
    /// the panic-firewall tests inject a scripted panicking backend through. The registry
    /// path is exactly this with `all_backends`.
    pub(crate) fn with_backends(
        mut backends: Vec<Box<dyn GpuBackend>>,
        config: EngineConfig,
    ) -> Self {
        let mut devices = Vec::new();
        let mut event_engine = EventEngine::new();
        // Normalized PCI address → name of the backend that registered it first.
        let mut seen_pci: HashMap<String, &'static str> = HashMap::new();

        for (bi, b) in backends.iter_mut().enumerate() {
            for id in b.devices() {
                // Cross-backend dedupe by PCI address (settled CLAUDE.md decision). First
                // backend wins: registry order is nvidia → amd → intel, so the richest
                // source for a device is canonical.
                let pci_key = normalize_pci_id(&id.0);
                if let Some(key) = &pci_key {
                    if let Some(first) = seen_pci.get(key.as_str()) {
                        eprintln!(
                            "gpuviewer: {id} ({}) duplicates a device already registered by {first}; skipping",
                            b.name()
                        );
                        continue;
                    }
                }
                match b.static_info(&id) {
                    Ok(info) => {
                        if let Some(key) = pci_key {
                            seen_pci.insert(key, b.name());
                        }
                        event_engine.register_device(id.clone(), format!("GPU{}", devices.len()));
                        devices.push((bi, id, info));
                    }
                    Err(e) => {
                        eprintln!("gpuviewer: skipping {id} ({}): {e}", b.name());
                    }
                }
            }
        }

        let mock_in_use = backends.iter().any(|b| b.name() == "mock");

        // Open the store (best-effort) and register the discovered devices into it so a replay
        // session can label history even for a GPU later removed.
        //
        // The open is a RECORDING open claiming the data source the backends ACTUALLY are —
        // not what the flags said. main.rs preflights an explicit --db against the flags,
        // but the mock backend is also the automatic no-real-GPU fallback: that session
        // records simulated data into a database the flags called "real". Claiming here,
        // where the engine knows the truth, means a mismatched database gets ZERO writes —
        // not even the device-identity upserts below — instead of leaking rows before a
        // later check could refuse.
        let session_start_ms = now_ms();
        let mut recorder = None;
        let mut db_path = None;
        let mut pending_reset = None;
        let mut pending_session_start = None;
        if config.persist {
            let source = if mock_in_use {
                DataSource::Mock
            } else {
                DataSource::Real
            };
            let opened = match &config.db_path {
                Some(p) => SqliteStore::open_recording(p, source),
                None => SqliteStore::open_default(mock_in_use),
            };
            match opened {
                Ok((store, was_reset)) => {
                    db_path = Some(store.path().to_path_buf());
                    let mut rec = Recorder::new(store);
                    for (_, id, info) in &devices {
                        let _ = rec.store_mut().register_device(
                            id,
                            &info.name,
                            info.vendor,
                            info.mem_total_bytes,
                        );
                    }
                    if was_reset {
                        // Fold a one-shot HistoryReset into the first tick: the gap from a
                        // quarantined-and-recreated file must not masquerade as device idleness.
                        let path_note = db_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        let anchor = devices
                            .first()
                            .map(|(_, id, _)| id.clone())
                            .unwrap_or_else(|| DeviceId("history".into()));
                        pending_reset = Some(Event {
                            ts_ms: now_ms(),
                            device: anchor,
                            kind: EventKind::HistoryReset,
                            severity: Severity::Warning,
                            confidence: Confidence::Fact,
                            title: "history file was corrupt — renamed aside and started fresh"
                                .into(),
                            evidence: format!(
                                "previous file kept at {path_note}.corrupt-<unix_seconds>; \
                                 a fresh database was created at {path_note}"
                            ),
                        });
                    }

                    // Session boundary: detect a dangling start mark BEFORE writing this
                    // session's own, then record `recording_started`. Unclean iff the
                    // newest start mark is strictly newer than every stop mark; a tie
                    // reads clean — a confidently-wrong "previous session crashed" would
                    // damage trust more than a missed one.
                    let prev_start = rec
                        .store()
                        .latest_event_ms(Some(EventKind::RecordingStarted))
                        .ok()
                        .flatten();
                    let prev_stop = rec
                        .store()
                        .latest_event_ms(Some(EventKind::RecordingStopped))
                        .ok()
                        .flatten();
                    let dangling = prev_start.filter(|s| *s > prev_stop.unwrap_or(0));
                    // Backends as the recording will actually see them: only those that
                    // registered at least one device, in collection order.
                    let mut backend_names: Vec<&'static str> = Vec::new();
                    for (bi, _, _) in &devices {
                        let n = backends[*bi].name();
                        if !backend_names.contains(&n) {
                            backend_names.push(n);
                        }
                    }
                    let backend_list = if backend_names.is_empty() {
                        "none".to_string()
                    } else {
                        backend_names.join(", ")
                    };
                    let db_name = db_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "history.db".into());
                    let anchor = devices
                        .first()
                        .map(|(_, id, _)| id.clone())
                        .unwrap_or_else(|| DeviceId("collector".into()));
                    let start_ev = recording_started_event(
                        session_start_ms,
                        anchor,
                        config.interval,
                        &backend_list,
                        devices.len(),
                        &db_name,
                        dangling,
                    );
                    rec.record_events(std::slice::from_ref(&start_ev));
                    pending_session_start = Some(start_ev);
                    recorder = Some(rec);
                }
                Err(e) => {
                    eprintln!("gpuviewer: history persistence disabled (store open failed): {e}");
                }
            }
        }

        let sink = config.on_event.map(EventSink::new);
        let health = vec![ProbeHealth::default(); devices.len()];

        Self {
            backends,
            devices,
            event_engine,
            recorder,
            db_path,
            interval: config.interval,
            last_tick_end: None,
            last_slow_note: HashMap::new(),
            pending_reset,
            pending_session_start,
            sink,
            consecutive_panics: 0,
            health,
            session_start_ms,
            frames_folded: 0,
            finished: false,
        }
    }

    pub fn static_infos(&self) -> Vec<StaticInfo> {
        self.devices.iter().map(|(_, _, i)| i.clone()).collect()
    }

    /// Whether the data on screen is simulated — true when the mock backend is active
    /// (forced via `--mock` or registered as the no-real-GPU fallback; it is exclusive
    /// either way). The UI labels mock data as mock, and must never label live data so.
    pub fn mock_in_use(&self) -> bool {
        self.backends.iter().any(|b| b.name() == "mock")
    }

    /// The on-disk history path, for the UI/replay. `None` when not persisting (or the store
    /// failed to open).
    pub fn db_path(&self) -> Option<PathBuf> {
        self.db_path.clone()
    }

    pub fn tick(&mut self) -> Frame {
        let mut frame_devices = Vec::with_capacity(self.devices.len());
        let mut events = Vec::new();

        // Self-honesty: a between-tick gap longer than the threshold means a backend probe
        // blocked and the recording has a hole. Narrate it on THIS (successful) tick so the
        // gap is recorded as a fact, not left to read as the GPU having gone quiet.
        let now_instant = Instant::now();
        if let Some(prev_end) = self.last_tick_end {
            let gap = now_instant.duration_since(prev_end);
            if is_stall(gap, self.interval) {
                let last_good = fmt_clock(now_ms().saturating_sub(gap.as_millis() as u64));
                let anchor = self.anchor_device();
                events.push(Event {
                    ts_ms: now_ms(),
                    device: anchor,
                    kind: EventKind::CollectorStall,
                    severity: Severity::Warning,
                    confidence: Confidence::Fact,
                    title: format!(
                        "collection stalled {} — a backend probe blocked; the data gap is \
                         recorded, last good frame at {last_good}",
                        fmt_dur(gap)
                    ),
                    evidence: format!(
                        "inter-tick gap {} exceeds the {} stall threshold (interval {})",
                        fmt_dur(gap),
                        fmt_dur(stall_threshold(self.interval)),
                        fmt_dur(self.interval),
                    ),
                });
            }
        }

        // A pending HistoryReset rides out on the first tick, before any device events.
        if let Some(reset) = self.pending_reset.take() {
            events.push(reset);
        }
        // The session's start mark rides the first tick too — already in the store (the
        // dedupe index swallows the re-insert below), but the stream, `--on-event`, and
        // the story feed only see events that travel in a frame.
        if let Some(start) = self.pending_session_start.take() {
            events.push(start);
        }

        for (di, (bi, id, info)) in self.devices.iter().enumerate() {
            let backend = &mut self.backends[*bi];
            let probe_start = Instant::now();
            // Keep the error, not just its absence: when failures accumulate into a
            // `device_lost` fact, the evidence must carry what the driver actually said.
            let (sample, probe_err) = match backend.refresh_dynamic(id) {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let probe_dur = probe_start.elapsed();
            let processes = backend.refresh_processes(id).unwrap_or_default();

            // Device-lost / returned edges — the audit's silently-vanishing-device hole:
            // a device that stops answering (driver reset, NVML dying, eGPU unplug, xe
            // rebind) must enter the recording as a fact with both edges, never vanish
            // or freeze silently. One failed probe is weather; the streak threshold and
            // its rationale live on [`DEVICE_LOST_AFTER_FAILED_PROBES`].
            let h = &mut self.health[di];
            match &sample {
                Some(s) => {
                    if let Some(lost_at) = h.lost_at_ms.take() {
                        events.push(device_returned_event(di, id, h, lost_at, s.ts_ms));
                    }
                    h.consecutive_failures = 0;
                    h.last_good_ms = Some(s.ts_ms);
                }
                None => {
                    let err = probe_err.unwrap_or_else(|| "unknown error".into());
                    h.consecutive_failures += 1;
                    if h.consecutive_failures == 1 {
                        h.first_fail_ms = now_ms();
                        h.first_error = err.clone();
                    }
                    h.last_error = err;
                    if h.lost_at_ms.is_none()
                        && h.consecutive_failures >= DEVICE_LOST_AFTER_FAILED_PROBES
                    {
                        let at = now_ms();
                        h.lost_at_ms = Some(at);
                        events.push(device_lost_event(di, id, h, at));
                    }
                }
            }

            // A slow probe foreshadows a stall — note it (Info, rate-capped per device).
            if probe_dur > SLOW_PROBE {
                let fresh = self
                    .last_slow_note
                    .get(id)
                    .map(|t| now_instant.duration_since(*t) >= SLOW_PROBE_COOLDOWN)
                    .unwrap_or(true);
                if fresh {
                    self.last_slow_note.insert(id.clone(), now_instant);
                    events.push(Event {
                        ts_ms: now_ms(),
                        device: id.clone(),
                        kind: EventKind::CollectorStall,
                        severity: Severity::Info,
                        confidence: Confidence::Fact,
                        title: format!(
                            "{} probe took {} — driver slow to respond",
                            backend.name(),
                            fmt_dur(probe_dur)
                        ),
                        evidence: format!(
                            "refresh_dynamic for {id} took {} (> {} threshold)",
                            fmt_dur(probe_dur),
                            fmt_dur(SLOW_PROBE)
                        ),
                    });
                }
            }

            if let Some(s) = &sample {
                events.extend(self.event_engine.observe(
                    id,
                    s,
                    &processes,
                    info.mem_total_bytes,
                    info.temp_slowdown_c,
                ));
                // Fold this frame into the persistent rollups (best-effort: a disk error
                // never reaches here — the Recorder swallows it and the live view runs on).
                if let Some(rec) = &mut self.recorder {
                    rec.observe(id, s, &processes);
                }
            }
            frame_devices.push(FrameDevice {
                id: id.clone(),
                name: info.name.clone(),
                mem_total_bytes: info.mem_total_bytes,
                sample,
                processes,
            });
        }

        // Persist and side-channel the events derived this tick.
        if let Some(rec) = &mut self.recorder {
            rec.record_events(&events);
        }
        if let Some(sink) = &mut self.sink {
            for e in &events {
                sink.fire(e);
            }
        }

        self.last_tick_end = Some(Instant::now());
        self.frames_folded += 1;

        Frame {
            ts_ms: now_ms(),
            devices: frame_devices,
            events,
        }
    }

    /// Test seam for the inter-tick stall-gap narration: inject the previous tick's end
    /// instant directly. `tick` measures the gap against `Instant::now()` with no clock
    /// abstraction, so without this seam the only way to drive the stall path would be a
    /// real 5s+ sleep — exactly the wall-clock flakiness the suite avoids. The injected
    /// instant is a genuine `Instant` (typically `now - gap`), so the production
    /// comparison code runs unmodified; nothing about the measurement is faked.
    #[cfg(test)]
    pub(crate) fn set_last_tick_end(&mut self, t: Instant) {
        self.last_tick_end = Some(t);
    }

    /// The `device` anchor for collector-scoped events (stall, panic, session boundary):
    /// the first registered device, or a stable placeholder when none exists. One helper
    /// so every recorder-lifecycle fact follows the same convention.
    fn anchor_device(&self) -> DeviceId {
        self.devices
            .first()
            .map(|(_, id, _)| id.clone())
            .unwrap_or_else(|| DeviceId("collector".into()))
    }

    /// [`Engine::tick`] behind a panic firewall — the fix for the audit's tick-panic
    /// frozen-UI hole (docs/research/06-production-platform-deepdive.md §1.3): a panic
    /// anywhere in a tick (a backend parsing edge case, an arithmetic overflow in a
    /// decoder) used to kill the collector thread while the TUI kept rendering the last
    /// `Shared` snapshot forever — stale data masquerading as live, the worst possible
    /// failure for a product whose thesis is honest recording.
    ///
    /// Policy: drop the panicked tick, narrate it as a `CollectorStall` FACT (that kind
    /// asserts exactly "the recording has a hole"), and keep collecting — a one-off panic
    /// is the same per-tick weather as `NVML_ERROR_NOT_SUPPORTED` or a blocked probe, and
    /// losing the whole flight recording to one bad frame would contradict the product.
    /// [`MAX_CONSECUTIVE_PANICS`] panics in a row mean the fault is deterministic, not
    /// weather: the outcome turns `fatal` and the caller must stop, loudly. Any clean
    /// tick resets the count.
    pub fn tick_guarded(&mut self) -> TickOutcome {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick())) {
            Ok(frame) => {
                self.consecutive_panics = 0;
                TickOutcome::Frame(frame)
            }
            Err(payload) => {
                self.consecutive_panics += 1;
                let summary = panic_summary(payload.as_ref());
                let fatal = self.consecutive_panics >= MAX_CONSECUTIVE_PANICS;
                // The default panic hook already printed the thread/location line at
                // unwind time; this adds the collector's own framing. Either way the
                // payload reaches stderr — a swallowed panic message would be its own
                // honesty bug.
                eprintln!(
                    "gpuviewer: collector tick panicked ({}/{MAX_CONSECUTIVE_PANICS} \
                     consecutive): {summary} (run with RUST_BACKTRACE=1 for a backtrace)",
                    self.consecutive_panics
                );
                let event = self.panic_event(&summary, fatal);
                // Record the fact while the recorder is still reachable — behind its own
                // firewall: if the panic originated inside the recorder fold or the sink,
                // recording the narration would panic again, and the narration must not
                // take down the narrator. stderr above remains the floor.
                let recorded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(rec) = &mut self.recorder {
                        rec.record_events(std::slice::from_ref(&event));
                    }
                    if let Some(sink) = &mut self.sink {
                        sink.fire(&event);
                    }
                }));
                if recorded.is_err() {
                    eprintln!(
                        "gpuviewer: recording the panic event panicked too — the event \
                         reaches the live view only; stderr holds the full trail"
                    );
                }
                TickOutcome::Panicked {
                    event,
                    summary,
                    fatal,
                }
            }
        }
    }

    /// The `CollectorStall` fact for one panicked tick — reusing the existing kind (it is
    /// exactly "the recording has a hole") keeps the NDJSON contract untouched. Warning
    /// while collection will retry; Critical when this panic spent the budget and
    /// collection stops here.
    fn panic_event(&self, summary: &str, fatal: bool) -> Event {
        let anchor = self.anchor_device();
        let n = self.consecutive_panics;
        if fatal {
            Event {
                ts_ms: now_ms(),
                device: anchor,
                kind: EventKind::CollectorStall,
                severity: Severity::Critical,
                confidence: Confidence::Fact,
                title: format!("collection STOPPED — collector panicked {n} ticks in a row"),
                evidence: format!(
                    "panic: {summary}; {n} consecutive tick panics spent the \
                     {MAX_CONSECUTIVE_PANICS}-tick budget — collection will not resume \
                     and the recording ends here; run with RUST_BACKTRACE=1 for a backtrace"
                ),
            }
        } else {
            Event {
                ts_ms: now_ms(),
                device: anchor,
                kind: EventKind::CollectorStall,
                severity: Severity::Warning,
                confidence: Confidence::Fact,
                // The count is in the TITLE, not just the evidence: two consecutive
                // panics can land in the same millisecond, and the event log's dedupe
                // key is (ts, device, kind, title) — identical titles would collapse two
                // distinct dropped frames into one narration. Distinct events must
                // narrate distinctly.
                title: format!(
                    "collector tick panicked ({n}/{MAX_CONSECUTIVE_PANICS}) — frame \
                     dropped, the recording has a hole"
                ),
                evidence: format!(
                    "panic: {summary}; consecutive panic {n} of {MAX_CONSECUTIVE_PANICS} \
                     tolerated, collection retries next tick; run with RUST_BACKTRACE=1 \
                     for a backtrace"
                ),
            }
        }
    }

    /// End the recording session cleanly: write the `recording_stopped` mark, fire it
    /// through `--on-event`, and flush the partial rollup tail. Returns the stop event
    /// when one was written by THIS call so the `--json` path can put it on the stream
    /// while stdout is still open; the TUI path records it without emitting (the session
    /// is over, nobody is watching the feed).
    ///
    /// Idempotent — explicit shutdown paths and `Drop` may both arrive here, and the
    /// session's end must land exactly once. A SIGKILL/OOM-kill/power loss skips this
    /// entirely and writes nothing: that is exactly the asymmetry the next session's
    /// unclean-start narration covers.
    pub fn finish(&mut self) -> Option<Event> {
        let stop = if self.recorder.is_some() && !self.finished {
            self.finished = true;
            let ev = self.stop_event();
            if let Some(rec) = &mut self.recorder {
                rec.record_events(std::slice::from_ref(&ev));
            }
            if let Some(sink) = &mut self.sink {
                sink.fire(&ev);
            }
            Some(ev)
        } else {
            None
        };
        self.flush();
        stop
    }

    /// The `recording_stopped` fact — the session-boundary mark on the recording's right
    /// edge, twin of `recording_started`. Info/FACT for the same reasons; the evidence
    /// spells out what the mark means for the timeline: time past it is "gpuviewer not
    /// running", never "the GPU sat idle".
    fn stop_event(&self) -> Event {
        let now = now_ms();
        let dur = Duration::from_millis(now.saturating_sub(self.session_start_ms));
        let frames_word = if self.frames_folded == 1 {
            "frame"
        } else {
            "frames"
        };
        Event {
            ts_ms: now,
            device: self.anchor_device(),
            kind: EventKind::RecordingStopped,
            severity: Severity::Info,
            confidence: Confidence::Fact,
            title: format!("recording stopped — clean shutdown after {}", fmt_dur(dur)),
            evidence: format!(
                "session started {}; {} {frames_word} folded over {}; the partial rollup \
                 tail was flushed; time between this mark and the next recording_started \
                 is gpuviewer not running, not the GPU sitting idle",
                fmt_stamp(self.session_start_ms),
                self.frames_folded,
                fmt_dur(dur),
            ),
        }
    }

    /// Persist every device's partial (uncrossed) bucket. Call on shutdown so the last,
    /// incomplete 10s/1m window is not lost. A no-op when not persisting.
    pub fn flush(&mut self) {
        if let Some(rec) = &mut self.recorder {
            rec.flush();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // The tail of the recording (and its stop mark) would otherwise be lost on a clean
        // exit — `finish` is idempotent, so paths that already called it lose nothing.
        let _ = self.finish();
    }
}

/// Fire-and-forget runner for the `--on-event` shell command. Each emitted event spawns the
/// platform shell (`sh -c CMD`; `cmd /C CMD` on Windows) with the event surfaced through
/// `GPV_EVENT_*` environment variables; the child is reaped lazily (a `try_wait` sweep on
/// the next fire) so a slow hook never blocks a tick.
///
/// Rate-capped at [`SINK_MAX_PER_MIN`] spawns per rolling minute: a throttle-flapping GPU can
/// produce a burst of events, and an unbounded fan-out of curl/notify-send processes would be
/// its own denial of service. Beyond the cap, events are dropped with a one-time warning per
/// minute.
pub struct EventSink {
    cmd: String,
    /// Live children awaiting reap; swept (non-blocking) on each fire.
    children: Vec<Child>,
    /// Spawn timestamps inside the current rolling 60s window.
    window: Vec<Instant>,
    /// Whether the "rate cap hit" warning was already printed in the current over-cap burst.
    warned: bool,
}

/// At most this many `--on-event` spawns per rolling minute.
const SINK_MAX_PER_MIN: usize = 60;

impl EventSink {
    pub fn new(cmd: String) -> Self {
        Self {
            cmd,
            children: Vec::new(),
            window: Vec::new(),
            warned: false,
        }
    }

    /// Spawn the hook for one event, unless the per-minute cap is exceeded.
    pub fn fire(&mut self, event: &Event) {
        self.reap();

        let now = Instant::now();
        self.window
            .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if self.window.len() >= SINK_MAX_PER_MIN {
            if !self.warned {
                self.warned = true;
                eprintln!(
                    "gpuviewer: --on-event rate cap ({SINK_MAX_PER_MIN}/min) hit — dropping \
                     further event hooks this minute"
                );
            }
            return;
        }
        self.warned = false;

        let json = serde_json::to_string(event).unwrap_or_default();
        // The hook runs through the OS's native shell. WHY a dedicated Windows branch
        // instead of `sh` everywhere: Git-for-Windows drops an sh.exe onto the PATH of CI
        // runners and many dev boxes, so a Unix-only dispatch would pass CI green and then
        // fail on exactly the user machines that have no sh at all.
        //
        // WHY raw_arg instead of args(["/C", ..]): std's argument quoting targets
        // CommandLineToArgvW, but cmd.exe parses its line with its own rules. A hook
        // command containing spaces gets wrapped in quotes with its inner quotes
        // backslash-escaped — cmd understands neither, so e.g. a quoted redirect target
        // decays into a broken path (the first Windows CI run failed exactly there).
        // raw_arg hands the user's command to cmd verbatim, as if typed after `cmd /C `.
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut c = Command::new("cmd");
            c.arg("/C");
            c.raw_arg(&self.cmd);
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = Command::new("sh");
            c.args(["-c", &self.cmd]);
            c
        };
        command
            .env("GPV_EVENT_KIND", env_token(event.kind))
            .env("GPV_EVENT_SEVERITY", env_token(event.severity))
            .env("GPV_EVENT_CONFIDENCE", env_token(event.confidence))
            .env("GPV_EVENT_TITLE", &event.title)
            .env("GPV_EVENT_EVIDENCE", &event.evidence)
            .env("GPV_EVENT_DEVICE", &event.device.0)
            .env("GPV_EVENT_TS_MS", event.ts_ms.to_string())
            .env("GPV_EVENT_JSON", json);
        match command.spawn() {
            Ok(child) => {
                self.window.push(now);
                self.children.push(child);
            }
            Err(e) => eprintln!("gpuviewer: --on-event spawn failed: {e}"),
        }
    }

    /// Reap any finished children without blocking — a hook that is still running stays in the
    /// list and is checked again next time.
    fn reap(&mut self) {
        self.children
            .retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        // Best-effort reap on shutdown; we do not block on a long-running hook.
        self.reap();
    }
}

/// Serialize an enum to its bare wire token (the same `snake_case`/`lowercase` spelling the
/// NDJSON contract uses), for the `GPV_EVENT_*` environment. Infallible for these C-like enums.
fn env_token<T: Serialize>(v: T) -> String {
    serde_json::to_string(&v)
        .ok()
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default()
}

/// Format a duration as a compact human string for narration ("1.2s", "8s", "3m 20s").
fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis() as u64;
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let s = (ms + 500) / 1000;
        if s >= 60 {
            format!("{}m {}s", s / 60, s % 60)
        } else {
            format!("{s}s")
        }
    }
}

/// Format a unix-millis instant as a wall HH:MM:SS for the "last good frame at …" narration.
fn fmt_clock(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}

/// Date + time of day, for session-boundary narration: a dangling start mark can be days
/// old, where a bare HH:MM:SS would be ambiguous.
fn fmt_stamp(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "????-??-?? --:--:--".into())
}

/// Whether a single device's frame reads as idle for the adaptive-backoff decision. Pure so
/// the backoff state machine is unit-tested without a tick loop.
///
/// `util_pct`, when reported, is authoritative: below the threshold AND no attached process
/// itself showing real GPU use → idle. When the device CANNOT report utilization — Intel
/// never does (the device-level PMU needs root/CAP_PERFMON; intel.rs hardcodes honest
/// `None`), so the old "None → not idle" rule pinned the whole loop at full cadence forever
/// on any machine containing such a device, killing the bottom #1291 mitigation exactly
/// where it matters (docs/research/06-production-platform-deepdive.md, "cross-cutting
/// defect") — fall back to activity signals the device DOES report, via
/// [`util_less_activity`]. With no signal at all the device is backoff-eligible: unknown
/// must not mean "busy". The worry behind the old rule — backing off through a throttle
/// onset — is covered by the fallback: an onset asserts a throttle reason and moves clocks,
/// both of which snap the cadence back. No utilization value is fabricated anywhere;
/// absence stays absence in everything rendered or recorded.
///
/// A failed probe (`sample` = `None`) still reads as NOT idle: that is a fault to watch at
/// full rate, not an unknown-but-quiet device.
pub fn device_is_idle(
    prev: Option<&DynamicSample>,
    sample: Option<&DynamicSample>,
    procs: &[ProcessSample],
) -> bool {
    let Some(s) = sample else { return false };
    // An attached process provably computing holds full cadence regardless of how (or
    // whether) the device-level story is told.
    if procs
        .iter()
        .any(|p| p.util_pct.map(|u| u >= IDLE_UTIL_PCT).unwrap_or(false))
    {
        return false;
    }
    match s.util_pct {
        Some(util) => util < IDLE_UTIL_PCT,
        None => !util_less_activity(prev, s),
    }
}

/// Activity signals for a device that cannot report utilization. Every check is a genuine
/// observation already carried by the model — nothing here invents a util number.
fn util_less_activity(prev: Option<&DynamicSample>, s: &DynamicSample) -> bool {
    // Video engines busy: encode/decode is real work even with zero 3D/compute load.
    if s.encoder_pct.map(|u| u >= IDLE_UTIL_PCT).unwrap_or(false)
        || s.decoder_pct.map(|u| u >= IDLE_UTIL_PCT).unwrap_or(false)
    {
        return true;
    }
    // An asserted throttle reason means the device is being limited, hence working.
    // (`None` — throttle unobservable on this source — asserts nothing either way.)
    if s.throttle.is_some_and(|t| t.any()) {
        return true;
    }
    // The remaining signals are deltas against the last good sample; with no baseline yet
    // there is nothing to compare (the streak threshold absorbs the first tick anyway).
    let Some(p) = prev else { return false };
    // VRAM moved since we last looked: something allocated or freed.
    if let (Some(a), Some(b)) = (p.mem_used_bytes, s.mem_used_bytes) {
        if a.abs_diff(b) >= IDLE_MEM_DELTA_BYTES {
            return true;
        }
    }
    // Clocks moved past wobble — or the device left a parked state where the actual
    // frequency reads as absent (Intel act-freq is None inside RC6): it woke up to work.
    match (p.sm_clock_mhz, s.sm_clock_mhz) {
        (Some(a), Some(b)) => a.abs_diff(b) > IDLE_CLOCK_JITTER_MHZ,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// A device counts as idle below this util (device or any process). Matches the "low-power
/// cadence" wedge: a desktop GPU at <5% with nothing computing is genuinely asleep.
const IDLE_UTIL_PCT: f32 = 5.0;

/// VRAM movement between consecutive good samples below this is allocator churn (a desktop
/// compositor recycles small buffers constantly), not activity worth full cadence.
const IDLE_MEM_DELTA_BYTES: u64 = 4 * 1024 * 1024;

/// Clock movement at or below this is sensor wobble around a parked frequency. Any real
/// load (or throttle onset) shifts clocks by hundreds of MHz, so the signal still catches
/// a wake-up within one tick.
const IDLE_CLOCK_JITTER_MHZ: u32 = 25;

/// Consecutive all-idle ticks before the cadence stretches.
pub const BACKOFF_AFTER_IDLE_TICKS: u32 = 60;

/// Idle cadence is this multiple of the configured interval, capped at [`BACKOFF_CAP`].
const BACKOFF_MULTIPLIER: u32 = 5;
const BACKOFF_CAP: Duration = Duration::from_secs(10);

/// The effective sleep for the next loop iteration given how many consecutive all-idle ticks
/// have elapsed. ANY non-idle tick resets `idle_streak` to 0 ([`Backoff::observe`]'s job),
/// which snaps the cadence back to the configured interval instantly. Pure for unit testing.
///
/// WHY this exists (CLAUDE.md / bottom #1291): polling has side effects — NVIDIA temp polling
/// keeps GPUs awake and AMD GRBM register reads break GFXOFF. On a genuinely idle GPU the
/// fast 1 Hz cadence is pure harm: it prevents the deep sleep states it is trying to observe.
/// Stretching to 5× (≤10s) once a GPU has sat idle a full minute lets it sleep while still
/// catching a wake-up within one stretched tick.
pub fn effective_interval(interval: Duration, idle_streak: u32) -> Duration {
    if idle_streak >= BACKOFF_AFTER_IDLE_TICKS {
        (interval * BACKOFF_MULTIPLIER).min(BACKOFF_CAP)
    } else {
        interval
    }
}

/// The adaptive-backoff state machine: consecutive all-idle ticks stretch the cadence
/// ([`effective_interval`]); any non-idle tick snaps it back instantly. Owns the
/// previous-sample baseline that [`device_is_idle`]'s util-less fallback diffs against —
/// kept off `Shared` so the whole policy is a plain value a scripted-stream test drives
/// without threads, locks, or a clock. The footer never lies about the result: it renders
/// the same `effective_interval_ms` atomic the loop actually sleeps on.
pub struct Backoff {
    /// `--no-backoff` → false: always the configured interval, streak irrelevant.
    enabled: bool,
    interval: Duration,
    /// Consecutive all-idle ticks; any non-idle tick resets it to 0.
    idle_streak: u32,
    /// Last good sample per device (collection order) — the delta baseline. Carried across
    /// failed-probe ticks: "VRAM moved since we last could look" is still activity.
    prev: Vec<Option<DynamicSample>>,
}

impl Backoff {
    pub fn new(enabled: bool, interval: Duration, devices: usize) -> Self {
        Self {
            enabled,
            interval,
            idle_streak: 0,
            prev: vec![None; devices],
        }
    }

    /// Fold one tick's frame in; returns the sleep before the next tick.
    pub fn observe(&mut self, frame: &Frame) -> Duration {
        let mut all_idle = true;
        for (i, fd) in frame.devices.iter().enumerate() {
            let prev = self.prev.get(i).and_then(Option::as_ref);
            if !device_is_idle(prev, fd.sample.as_ref(), &fd.processes) {
                all_idle = false;
            }
            if let (Some(slot), Some(s)) = (self.prev.get_mut(i), &fd.sample) {
                *slot = Some(s.clone());
            }
        }
        if !self.enabled {
            return self.interval;
        }
        if all_idle {
            self.idle_streak = self.idle_streak.saturating_add(1);
        } else {
            self.idle_streak = 0;
        }
        effective_interval(self.interval, self.idle_streak)
    }

    /// A paused tick observes nothing: do not let the idle streak grow (it would back off
    /// on resume against stale state) and poll the pause flag at the configured interval.
    pub fn skip(&mut self) -> Duration {
        self.idle_streak = 0;
        self.interval
    }
}

/// Why and when the collector died, for the UI. Once set, the live panes can only show
/// the last folded frame, and they must say so — stale data masquerading as live is the
/// audit's worst failure shape (the tick-panic frozen-UI hole).
#[derive(Clone)]
pub struct CollectorStop {
    /// Unix-millis instant the collector declared itself dead.
    pub at_ms: u64,
    /// One-line panic summary (the full trail is on stderr and in the event evidence).
    pub reason: String,
}

/// State shared between the collector thread and the UI.
pub struct Shared {
    pub infos: Vec<StaticInfo>,
    pub latest: Vec<Option<DynamicSample>>,
    pub processes: Vec<Vec<ProcessSample>>,
    pub history: HistoryStore,
    /// True when the data is simulated (mock backend active) — drives the footer's
    /// "(mock data)" tag, which must track the actual data source.
    pub mock: bool,
    /// The on-disk history path (`None` when not persisting), for the replay view.
    pub db_path: Option<PathBuf>,
    /// The configured tick interval in millis — the footer compares the effective cadence
    /// against this to decide whether low-power backoff is active.
    pub interval_ms: u64,
    /// Current effective tick interval in millis — stretched under low-power backoff so the
    /// footer can show "low-power cadence 5s". Published via an atomic so the UI reads it
    /// without taking the (already heavily-held) Shared lock on the collector's hot path.
    pub effective_interval_ms: Arc<AtomicU64>,
    /// Collector ticks folded into this state so far, bumped once per frame (a panicked
    /// tick bumps it too — its narration event is new state). The timeline overview
    /// re-queries its cached window when this advances — and only then, keeping SQLite
    /// off the render path. Stays 0 forever in [`Collector::stationary`] sessions:
    /// a recording does not grow.
    pub tick_seq: u64,
    /// `Some` once the collector thread is dead ([`MAX_CONSECUTIVE_PANICS`] consecutive
    /// tick panics): the UI must state collection STOPPED — with time and cause — and
    /// mark every live pane stale, never current.
    pub stopped: Option<CollectorStop>,
    /// Per-device loss marker, parallel to `infos`: `Some(unix ms the device was declared
    /// lost)` while its probe is dead ([`DEVICE_LOST_AFTER_FAILED_PROBES`] consecutive
    /// failures), cleared when it answers again. Drives the per-device STALE affordances:
    /// a lost device's panes are frozen at its last good data and must say so. This is
    /// deliberately NOT [`Shared::stopped`] — the other devices are still collecting, so
    /// the footer must not claim collection stopped.
    pub lost: Vec<Option<u64>>,
}

pub struct Collector {
    pub shared: Arc<Mutex<Shared>>,
    pub paused: Arc<AtomicBool>,
    /// Cooperative stop flag for the collection thread ([`Collector::shutdown`]). The
    /// thread exits its loop on the next slice boundary, dropping the [`Engine`] — whose
    /// `Drop` writes the session's `recording_stopped` mark and flushes the rollup tail.
    stop: Arc<AtomicBool>,
    /// The collection thread's handle (`None` for [`Collector::stationary`] sessions),
    /// so `shutdown` can wait — boundedly — for the recording tail to land.
    handle: Option<std::thread::JoinHandle<()>>,
}

/// The collection thread sleeps its (possibly backoff-stretched, up to 10s) cadence in
/// slices of this size so a shutdown request is noticed within one slice instead of one
/// whole cadence — quitting the TUI must not hang for seconds on a sleeping collector.
const SLEEP_SLICE: Duration = Duration::from_millis(100);

/// How long [`Collector::shutdown`] waits for the collection thread to finish before
/// giving up. The thread normally reacts within one [`SLEEP_SLICE`]; the budget exists
/// for a tick blocked inside a backend probe (the stall scenario) — quit must not wedge
/// on a hung driver, and a missing stop mark is exactly what the next session's
/// unclean-start narration reports.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

impl Collector {
    /// Spawn the background collection thread. `backoff` enables the adaptive idle cadence
    /// (`--no-backoff` disables it for users who want a fixed, predictable poll rate).
    pub fn start(mut engine: Engine, interval: Duration, backoff: bool) -> Self {
        let infos = engine.static_infos();
        let n = infos.len();
        let mock = engine.mock_in_use();
        let db_path = engine.db_path();
        let effective_interval_ms = Arc::new(AtomicU64::new(interval.as_millis() as u64));
        let shared = Arc::new(Mutex::new(Shared {
            infos,
            latest: vec![None; n],
            processes: vec![Vec::new(); n],
            // Live window: 30 min at 1s ticks; event log capped generously.
            history: HistoryStore::new(1800, 5000),
            mock,
            db_path,
            interval_ms: interval.as_millis() as u64,
            effective_interval_ms: Arc::clone(&effective_interval_ms),
            tick_seq: 0,
            stopped: None,
            lost: vec![None; n],
        }));
        let paused = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let s = Arc::clone(&shared);
        let p = Arc::clone(&paused);
        let st = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut cadence = Backoff::new(backoff, interval, n);
            loop {
                // Cooperative shutdown: returning drops `engine`, whose Drop writes the
                // `recording_stopped` mark and flushes the recording tail.
                if st.load(Ordering::Relaxed) {
                    return;
                }
                let effective = if !p.load(Ordering::Relaxed) {
                    // The guarded tick runs BEFORE the Shared lock is taken (as tick()
                    // always did), so even a panic that somehow escaped the firewall
                    // could never poison the mutex under the UI.
                    match engine.tick_guarded() {
                        TickOutcome::Frame(frame) => {
                            // Classify before the lock — Backoff carries its own delta
                            // baseline, so the idle decision never touches (or contends
                            // on) Shared.
                            let effective = cadence.observe(&frame);
                            let mut sh = s.lock().unwrap();
                            for (i, fd) in frame.devices.iter().enumerate() {
                                if let Some(sample) = &fd.sample {
                                    sh.history.push_sample(&fd.id, sample.clone());
                                    sh.latest[i] = Some(sample.clone());
                                    // The process list only updates alongside a good
                                    // probe: a failed refresh_dynamic almost certainly
                                    // took the process probe down with it (same dead
                                    // device), and replacing the table with that empty
                                    // result would render "no processes" as if current —
                                    // the same lie as a frozen chart. The kept list is
                                    // the device's last good data, which the per-device
                                    // stale affordances label once the device is lost.
                                    sh.processes[i] = fd.processes.clone();
                                }
                            }
                            // Device-lost edges ride in the frame's own events; fold
                            // them into the per-device markers the UI renders from.
                            for e in &frame.events {
                                let mark = match e.kind {
                                    EventKind::DeviceLost => Some(Some(e.ts_ms)),
                                    EventKind::DeviceReturned => Some(None),
                                    _ => None,
                                };
                                if let Some(mark) = mark {
                                    if let Some(i) =
                                        sh.infos.iter().position(|inf| inf.id == e.device)
                                    {
                                        sh.lost[i] = mark;
                                    }
                                }
                            }
                            sh.history.push_events(frame.events);
                            // Publish the tick AFTER its data is folded in, so a reader
                            // seeing the new seq always sees the new frame too.
                            sh.tick_seq += 1;
                            effective
                        }
                        TickOutcome::Panicked {
                            event,
                            summary,
                            fatal,
                        } => {
                            let mut sh = s.lock().unwrap();
                            sh.history.push_events([event]);
                            sh.tick_seq += 1;
                            if fatal {
                                // Dead, and saying so: `stopped` drives the UI's stop
                                // banner and stale tags, the Critical fact above sits in
                                // the story feed and the store, and ending the thread
                                // drops the Engine — whose Drop flushes the recording
                                // tail, so history is complete up to the stop.
                                sh.stopped = Some(CollectorStop {
                                    at_ms: now_ms(),
                                    reason: summary,
                                });
                                return;
                            }
                            // A panicked tick observed nothing — same cadence rule as a
                            // paused one, which also keeps a faulting collector watched
                            // at the full configured rate rather than backed off.
                            cadence.skip()
                        }
                    }
                } else {
                    cadence.skip()
                };
                effective_interval_ms.store(effective.as_millis() as u64, Ordering::Relaxed);
                // Sleep the cadence in slices so shutdown is noticed promptly (see
                // [`SLEEP_SLICE`]); the effective cadence itself is unchanged.
                let deadline = Instant::now() + effective;
                loop {
                    if st.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    std::thread::sleep((deadline - now).min(SLEEP_SLICE));
                }
            }
        });

        Self {
            shared,
            paused,
            stop,
            handle: Some(handle),
        }
    }

    /// Ask the collection thread to stop and wait — boundedly — for it to finish, so a
    /// clean quit ends with the `recording_stopped` mark and the rollup tail on disk.
    /// The wait is capped at [`SHUTDOWN_GRACE`]: a tick wedged inside a hung backend
    /// probe must not wedge quit with it (the thread still drops the engine — and writes
    /// the mark — whenever the probe finally returns; if the process dies first, the next
    /// session narrates the missing stop mark, which is the honest record of what
    /// happened). A no-op for [`Collector::stationary`] sessions, which have no thread.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            // Joining a finished thread cannot block; a panicked collector already
            // narrated itself (panic firewall), so the result carries nothing new.
            let _ = handle.join();
        } else {
            eprintln!(
                "gpuviewer: the collector did not stop within {}s (a backend probe is \
                 likely blocked); exiting without the recording's stop mark — the next \
                 session will report it",
                SHUTDOWN_GRACE.as_secs()
            );
        }
    }

    /// A collector with NO engine and NO background thread, for the file viewer
    /// (`gpuviewer view`): the recording is the only data source, so nothing ticks,
    /// nothing records, and no backend is probed — an exported incident replays on a
    /// machine with no GPU at all. `Shared` stays at its empty initial state; the replay
    /// view reads the store at `db_path` directly.
    pub fn stationary(infos: Vec<StaticInfo>, db_path: Option<PathBuf>) -> Self {
        let n = infos.len();
        Self {
            shared: Arc::new(Mutex::new(Shared {
                infos,
                latest: vec![None; n],
                processes: vec![Vec::new(); n],
                history: HistoryStore::new(1800, 5000),
                // A recording's provenance is unstated — never label it mock (or live).
                mock: false,
                db_path,
                interval_ms: 1000,
                effective_interval_ms: Arc::new(AtomicU64::new(1000)),
                tick_seq: 0,
                stopped: None,
                lost: vec![None; n],
            })),
            paused: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

/// A collector with NO background thread, for UI tests: `Collector::start` ticks the mock
/// once at spawn, which would race assertions that mutate `Shared` directly. Tests own the
/// entire `Shared` state instead and drive draws/keys by hand.
#[cfg(test)]
pub(crate) fn test_collector(db_path: Option<PathBuf>) -> Collector {
    let engine = Engine::new(EngineConfig {
        force_mock: true,
        ..Default::default()
    });
    let collector = Collector::stationary(engine.static_infos(), db_path);
    // The infos ARE mock devices; the footer tests assert the "(mock data)" label.
    collector.shared.lock().unwrap().mock = true;
    collector
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpuviewer_core::{BackendError, ProcessKind, ThrottleReasons, Vendor};
    use gpuviewer_history::Tier;

    fn sample(util: Option<f32>) -> DynamicSample {
        DynamicSample {
            ts_ms: 1_000,
            util_pct: util,
            util_engine: None,
            mem_used_bytes: None,
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            // Most scripted ticks model an observing source that sees no throttle;
            // the bare-sample "no signal at all" test overrides this to None.
            throttle: Some(ThrottleReasons::default()),
        }
    }

    fn proc(util: Option<f32>) -> ProcessSample {
        ProcessSample {
            pid: 1,
            name: "x".into(),
            kind: ProcessKind::Compute,
            mem_bytes: None,
            util_pct: util,
            cpu_pct: None,
            container: None,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// Script one tick's frame for [`Backoff`] from per-device (sample, processes) pairs —
    /// the scripted-stream harness for the cadence tests, same spirit as the MockBackend's
    /// scripted streams but with full control over every Option field.
    fn frame_of(devs: Vec<(Option<DynamicSample>, Vec<ProcessSample>)>) -> Frame {
        Frame {
            ts_ms: 0,
            devices: devs
                .into_iter()
                .enumerate()
                .map(|(i, (sample, processes))| FrameDevice {
                    id: DeviceId(format!("test:{i}")),
                    name: format!("dev{i}"),
                    mem_total_bytes: None,
                    sample,
                    processes,
                })
                .collect(),
            events: Vec::new(),
        }
    }

    /// The dev machine's Intel iGPU, as the audit describes it: util NEVER reported, no
    /// mem-used, clock parked at the minimum tick after tick, a quiet compositor attached.
    fn quiet_igpu() -> (Option<DynamicSample>, Vec<ProcessSample>) {
        let mut s = sample(None);
        s.sm_clock_mhz = Some(300);
        (Some(s), vec![proc(Some(1.0))])
    }

    // normalize_pci_id's unit tests moved to gpuviewer-core::model with the function
    // (design §5.4) — extended there with the wddm:/apple: refuse-to-dedupe cases.

    // ---- stall-gap threshold (pure) ----

    #[test]
    fn stall_threshold_is_max_of_three_intervals_and_five_seconds() {
        // Fast cadence: the 5s floor dominates (3×100ms = 300ms < 5s).
        assert_eq!(
            stall_threshold(Duration::from_millis(100)),
            Duration::from_secs(5)
        );
        // Slow cadence: 3×interval dominates (3×3s = 9s > 5s).
        assert_eq!(
            stall_threshold(Duration::from_secs(3)),
            Duration::from_secs(9)
        );
        // Exactly at the crossover (3×~1.667s ≈ 5s).
        assert_eq!(
            stall_threshold(Duration::from_secs(2)),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn is_stall_fires_only_strictly_past_the_threshold() {
        let interval = Duration::from_secs(1); // threshold = 5s
        assert!(
            !is_stall(Duration::from_secs(5), interval),
            "at threshold: not yet"
        );
        assert!(!is_stall(Duration::from_secs(4), interval));
        assert!(
            is_stall(Duration::from_millis(5_001), interval),
            "just over: stall"
        );
        assert!(is_stall(Duration::from_secs(30), interval));
    }

    // ---- adaptive backoff (pure) ----

    #[test]
    fn effective_interval_holds_until_the_idle_streak_threshold() {
        let interval = Duration::from_secs(1);
        // Below the threshold: configured interval unchanged.
        assert_eq!(effective_interval(interval, 0), interval);
        assert_eq!(
            effective_interval(interval, BACKOFF_AFTER_IDLE_TICKS - 1),
            interval
        );
        // At/after the threshold: stretched to 5× (within the 10s cap).
        assert_eq!(
            effective_interval(interval, BACKOFF_AFTER_IDLE_TICKS),
            Duration::from_secs(5)
        );
        assert_eq!(
            effective_interval(interval, BACKOFF_AFTER_IDLE_TICKS + 1000),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn effective_interval_caps_at_ten_seconds() {
        // 5×3s = 15s would exceed the cap, so it clamps to 10s.
        assert_eq!(
            effective_interval(Duration::from_secs(3), BACKOFF_AFTER_IDLE_TICKS),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn device_is_idle_requires_low_device_and_process_util() {
        // Device below 5% and no busy process → idle.
        assert!(device_is_idle(
            None,
            Some(&sample(Some(2.0))),
            &[proc(Some(1.0))]
        ));
        // Device busy → not idle.
        assert!(!device_is_idle(None, Some(&sample(Some(40.0))), &[]));
        // Device idle but a process is computing → not idle (work is queued/running).
        assert!(!device_is_idle(
            None,
            Some(&sample(Some(1.0))),
            &[proc(Some(80.0))]
        ));
        // No sample at all (failed probe) → not idle: a fault is watched at full rate.
        assert!(!device_is_idle(None, None, &[]));
    }

    /// The audit's cross-cutting defect (06-production-platform-deepdive.md): Intel reports
    /// `util_pct: None` on every tick, and the old "None → not idle" rule made such a device
    /// pin the loop at full cadence forever. A util-less device with no other activity
    /// signal must be backoff-eligible — unknown is not "busy".
    #[test]
    fn util_less_device_with_no_activity_signal_is_backoff_eligible() {
        // Bare sample (everything None, throttle unobservable too — the wddm shape):
        // no signal at all.
        let mut bare = sample(None);
        bare.throttle = None;
        assert!(device_is_idle(None, Some(&bare), &[]));
        // An observing source reporting "no throttle" is equally signal-less.
        assert!(device_is_idle(None, Some(&sample(None)), &[]));
        // The ubiquitous iGPU shape: a quiet compositor attached is not activity…
        assert!(device_is_idle(
            Some(&sample(None)),
            Some(&sample(None)),
            &[proc(Some(1.0))]
        ));
        // …and a process whose own util is unknown is not proof of work either (the old
        // process check already treated unknown process util as not-busy).
        assert!(device_is_idle(None, Some(&sample(None)), &[proc(None)]));
    }

    /// Every fallback signal is a real observation; each one alone must hold full cadence
    /// on a device that cannot report utilization.
    #[test]
    fn util_less_device_with_genuine_activity_is_not_idle() {
        // fdinfo engine-busy on an attached process.
        assert!(!device_is_idle(
            None,
            Some(&sample(None)),
            &[proc(Some(80.0))]
        ));

        // VRAM moved at least the churn threshold since the last good sample…
        let mut prev = sample(None);
        prev.mem_used_bytes = Some(GIB);
        let mut cur = sample(None);
        cur.mem_used_bytes = Some(GIB + IDLE_MEM_DELTA_BYTES);
        assert!(!device_is_idle(Some(&prev), Some(&cur), &[]));
        // …but sub-threshold churn (compositor buffer recycling) is not activity.
        cur.mem_used_bytes = Some(GIB + IDLE_MEM_DELTA_BYTES - 1);
        assert!(device_is_idle(Some(&prev), Some(&cur), &[]));

        // Clocks moved past wobble.
        let mut prev = sample(None);
        prev.sm_clock_mhz = Some(300);
        let mut cur = sample(None);
        cur.sm_clock_mhz = Some(900);
        assert!(!device_is_idle(Some(&prev), Some(&cur), &[]));
        // Single-bin wobble around a parked frequency is not activity.
        cur.sm_clock_mhz = Some(300 + IDLE_CLOCK_JITTER_MHZ);
        assert!(device_is_idle(Some(&prev), Some(&cur), &[]));
        // Waking out of a parked/RC6 state (clock absent → present) is, even with no
        // delta to compute.
        prev.sm_clock_mhz = None;
        cur.sm_clock_mhz = Some(300);
        assert!(!device_is_idle(Some(&prev), Some(&cur), &[]));

        // A busy video engine.
        let mut enc = sample(None);
        enc.encoder_pct = Some(60.0);
        assert!(!device_is_idle(None, Some(&enc), &[]));

        // An asserted throttle reason — the throttle-onset worry behind the old rule.
        let mut thr = sample(None);
        thr.throttle = Some(ThrottleReasons {
            thermal: true,
            ..Default::default()
        });
        assert!(!device_is_idle(None, Some(&thr), &[]));
    }

    // ---- backoff state machine (scripted streams) ----

    /// Regression for the audit's backoff-vs-Intel defect: a device that can NEVER report
    /// utilization but is otherwise quiet must not pin the loop at full cadence — after the
    /// idle-streak threshold the cadence stretches exactly as for a measurably-idle GPU.
    #[test]
    fn always_none_util_device_does_not_pin_cadence() {
        let interval = Duration::from_secs(1);
        let mut cadence = Backoff::new(true, interval, 1);
        for i in 1..BACKOFF_AFTER_IDLE_TICKS {
            assert_eq!(
                cadence.observe(&frame_of(vec![quiet_igpu()])),
                interval,
                "tick {i}: configured cadence until the streak threshold"
            );
        }
        assert_eq!(
            cadence.observe(&frame_of(vec![quiet_igpu()])),
            Duration::from_secs(5),
            "a quiet always-None-util device must reach the low-power cadence"
        );
    }

    /// A device with real activity holds full cadence forever — both the measured form
    /// (util reported high) and the util-less-but-provably-active form (clocks moving
    /// under a load we cannot measure directly).
    #[test]
    fn active_device_holds_full_cadence() {
        let interval = Duration::from_secs(1);

        let mut cadence = Backoff::new(true, interval, 1);
        for _ in 0..BACKOFF_AFTER_IDLE_TICKS * 2 {
            let f = frame_of(vec![(Some(sample(Some(90.0))), vec![])]);
            assert_eq!(
                cadence.observe(&f),
                interval,
                "measured load: never stretch"
            );
        }

        let mut cadence = Backoff::new(true, interval, 1);
        let mut clock = 600u32;
        for _ in 0..BACKOFF_AFTER_IDLE_TICKS * 2 {
            clock = if clock == 600 { 1_100 } else { 600 };
            let mut s = sample(None);
            s.sm_clock_mhz = Some(clock);
            assert_eq!(
                cadence.observe(&frame_of(vec![(Some(s), vec![])])),
                interval,
                "util-less device with moving clocks: never stretch"
            );
        }
    }

    /// Backoff requires ALL devices idle: the util-less iGPU no longer pins the cadence,
    /// but it must not unpin a machine whose other GPU is genuinely busy (the NVIDIA+Intel
    /// hybrid from the audit, with the dGPU under load this time).
    #[test]
    fn busy_device_next_to_util_less_one_holds_full_cadence() {
        let interval = Duration::from_secs(1);
        let mut cadence = Backoff::new(true, interval, 2);
        for _ in 0..BACKOFF_AFTER_IDLE_TICKS * 2 {
            let f = frame_of(vec![(Some(sample(Some(95.0))), vec![]), quiet_igpu()]);
            assert_eq!(cadence.observe(&f), interval);
        }
    }

    /// Once stretched, a wake on the util-less device — visible only through a process
    /// turning busy — snaps the cadence back within one tick.
    #[test]
    fn wake_on_util_less_device_snaps_cadence_back() {
        let interval = Duration::from_secs(1);
        let mut cadence = Backoff::new(true, interval, 1);
        for _ in 0..BACKOFF_AFTER_IDLE_TICKS + 5 {
            cadence.observe(&frame_of(vec![quiet_igpu()]));
        }
        assert_eq!(
            cadence.observe(&frame_of(vec![quiet_igpu()])),
            Duration::from_secs(5),
            "precondition: well into low-power cadence"
        );
        let (s, _) = quiet_igpu();
        let woke = frame_of(vec![(s, vec![proc(Some(40.0))])]);
        assert_eq!(
            cadence.observe(&woke),
            interval,
            "wake snaps back instantly"
        );
    }

    /// `--no-backoff` means exactly that: the configured interval regardless of idleness.
    #[test]
    fn disabled_backoff_never_stretches() {
        let interval = Duration::from_secs(1);
        let mut cadence = Backoff::new(false, interval, 1);
        for _ in 0..BACKOFF_AFTER_IDLE_TICKS * 2 {
            assert_eq!(cadence.observe(&frame_of(vec![quiet_igpu()])), interval);
        }
    }

    /// Paused ticks observe nothing, so they must reset the streak rather than let it
    /// ride into a stretch that would apply on resume against stale state.
    #[test]
    fn paused_ticks_reset_the_streak() {
        let interval = Duration::from_secs(1);
        let mut cadence = Backoff::new(true, interval, 1);
        for _ in 0..BACKOFF_AFTER_IDLE_TICKS - 1 {
            cadence.observe(&frame_of(vec![quiet_igpu()]));
        }
        assert_eq!(cadence.skip(), interval);
        // One more idle tick would have crossed the threshold pre-pause; post-pause the
        // streak starts over.
        assert_eq!(cadence.observe(&frame_of(vec![quiet_igpu()])), interval);
    }

    // ---- --on-event sink ----

    /// The hook receives the event through the GPV_EVENT_* environment. We echo
    /// GPV_EVENT_KIND to a tmpfile through this OS's real shell dispatch (`sh -c` /
    /// `cmd /C`) for a synthetic event and read the file back, after waiting for the
    /// (detached) child — proving the env wiring, not just spawn.
    #[test]
    fn event_sink_passes_event_through_environment() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let out = std::env::temp_dir().join(format!(
            "gpuviewer-onevent-test-{}-{n}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&out);

        // Per-OS hook command: each side must only ever run under its own dispatch (`cmd`
        // expands %VAR%, `sh` expands via printenv). No space before cmd's `>` — cmd's
        // echo would copy a trailing space into the file.
        #[cfg(windows)]
        let cmd = format!("echo %GPV_EVENT_KIND%> \"{}\"", out.display());
        #[cfg(not(windows))]
        let cmd = format!("printenv GPV_EVENT_KIND > \"{}\"", out.display());
        let mut sink = EventSink::new(cmd);
        sink.fire(&Event {
            ts_ms: 42,
            device: DeviceId("0000:01:00.0".into()),
            kind: EventKind::ThrottleStart,
            severity: Severity::Warning,
            confidence: Confidence::Fact,
            title: "t".into(),
            evidence: "e".into(),
        });

        // Wait (bounded) for the detached child to write the file. The budget is ~10s of
        // 10ms polls: Windows CI runners spawn cmd.exe slowly enough that a 2s budget is
        // flake territory, and a real wiring regression must fail THIS assertion (with
        // the diff below), never decay into a timeout that reads as infrastructure noise.
        // The fast path is unchanged — the loop exits on the first poll that sees output.
        let mut got = String::new();
        for _ in 0..1000 {
            if let Ok(s) = std::fs::read_to_string(&out) {
                if !s.trim().is_empty() {
                    got = s;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&out);
        assert_eq!(
            got.trim(),
            "throttle_start",
            "the hook must see the event kind as the wire token in GPV_EVENT_KIND"
        );
    }

    // ---- panic firewall (tick_guarded) ----

    /// A backend whose `refresh_dynamic` panics on scripted tick indices — the audit's
    /// "tick-panic frozen-UI hole" made flesh: the code path least exercised by the
    /// mock-based suite is a backend decoder edge case blowing up mid-tick.
    struct PanickyBackend<F: Fn(u32) -> bool + Send> {
        tick: u32,
        panics: F,
    }

    impl<F: Fn(u32) -> bool + Send> GpuBackend for PanickyBackend<F> {
        fn name(&self) -> &'static str {
            "panicky"
        }
        fn devices(&mut self) -> Vec<DeviceId> {
            vec![DeviceId("panic:0".into())]
        }
        fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
            Ok(StaticInfo {
                id: dev.clone(),
                vendor: Vendor::Unknown,
                name: "Panicky GPU".into(),
                backend: "panicky".into(),
                mem_total_bytes: Some(8 << 30),
                power_limit_mw: None,
                max_sm_clock_mhz: None,
                temp_slowdown_c: None,
                driver_version: None,
                process_hint: None,
                source_caveat: None,
            })
        }
        fn refresh_dynamic(&mut self, _dev: &DeviceId) -> Result<DynamicSample, BackendError> {
            let t = self.tick;
            self.tick += 1;
            if (self.panics)(t) {
                panic!("decoder choked on tick {t}");
            }
            Ok(sample(Some(50.0)))
        }
        fn refresh_processes(
            &mut self,
            _dev: &DeviceId,
        ) -> Result<Vec<ProcessSample>, BackendError> {
            Ok(Vec::new())
        }
    }

    /// An engine over a [`PanickyBackend`] with the given panic script, persisting to
    /// `db` when given (otherwise live-only).
    fn panicky_engine(
        db: Option<PathBuf>,
        panics: impl Fn(u32) -> bool + Send + 'static,
    ) -> Engine {
        Engine::with_backends(
            vec![Box::new(PanickyBackend { tick: 0, panics })],
            EngineConfig {
                persist: db.is_some(),
                db_path: db,
                ..Default::default()
            },
        )
    }

    fn scratch_db() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gpuviewer-panic-test-{}-{n}.db",
            std::process::id()
        ))
    }

    fn cleanup_db(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        for ext in ["-wal", "-shm", ".lock"] {
            let mut p = path.as_os_str().to_os_string();
            p.push(ext);
            let _ = std::fs::remove_file(p);
        }
    }

    /// Design (b)'s documented behavior for ONE transient panic: the tick is dropped and
    /// narrated as a `CollectorStall` FACT carrying the payload, and the very next clean
    /// tick collects normally — one decoder edge case must not end the flight recording.
    #[test]
    fn transient_tick_panic_narrates_and_collection_resumes() {
        let mut engine = panicky_engine(None, |t| t == 0);

        let TickOutcome::Panicked {
            event,
            summary,
            fatal,
        } = engine.tick_guarded()
        else {
            panic!("the scripted first tick must panic");
        };
        assert!(!fatal, "one panic is weather, not death");
        assert_eq!(event.kind, EventKind::CollectorStall);
        assert_eq!(event.confidence, Confidence::Fact);
        assert_eq!(event.severity, Severity::Warning);
        assert!(
            summary.contains("decoder choked on tick 0"),
            "the panic payload must survive into the summary: {summary}"
        );
        assert!(
            event.evidence.contains("decoder choked on tick 0"),
            "the payload must reach the event evidence: {}",
            event.evidence
        );
        assert!(
            event.evidence.contains("RUST_BACKTRACE"),
            "the evidence must carry the backtrace hint: {}",
            event.evidence
        );

        let TickOutcome::Frame(frame) = engine.tick_guarded() else {
            panic!("the next tick is clean and must produce a frame");
        };
        assert!(
            frame.devices[0].sample.is_some(),
            "collection resumed with real data"
        );
    }

    /// The budget counts CONSECUTIVE panics: a clean tick resets it, so an intermittent
    /// fault keeps collecting indefinitely; only [`MAX_CONSECUTIVE_PANICS`] in a row die.
    #[test]
    fn panic_budget_resets_on_a_clean_tick() {
        // Script: panic, clean, panic, panic, panic — fatal only at the third CONSECUTIVE
        // panic (tick 4); a total-panic counter would have died at tick 3.
        let mut engine = panicky_engine(None, |t| t != 1);
        let fatals: Vec<bool> = (0..5)
            .map(|_| match engine.tick_guarded() {
                TickOutcome::Frame(_) => false,
                TickOutcome::Panicked { fatal, .. } => fatal,
            })
            .collect();
        assert_eq!(
            fatals,
            vec![false, false, false, false, true],
            "fatal exactly when {MAX_CONSECUTIVE_PANICS} panics run consecutively"
        );
    }

    /// Spending the budget produces the Critical stop fact, still under the existing
    /// `CollectorStall` kind — the NDJSON contract (spec/schema/suite) is untouched.
    #[test]
    fn deterministic_panic_turns_fatal_with_a_critical_fact() {
        let mut engine = panicky_engine(None, |_| true);
        for i in 1..MAX_CONSECUTIVE_PANICS {
            match engine.tick_guarded() {
                TickOutcome::Panicked { fatal, .. } => {
                    assert!(!fatal, "panic {i} is within the budget")
                }
                TickOutcome::Frame(_) => panic!("scripted to panic"),
            }
        }
        let TickOutcome::Panicked { event, fatal, .. } = engine.tick_guarded() else {
            panic!("scripted to panic");
        };
        assert!(
            fatal,
            "the {MAX_CONSECUTIVE_PANICS}th consecutive panic is fatal"
        );
        assert_eq!(event.kind, EventKind::CollectorStall);
        assert_eq!(event.severity, Severity::Critical);
        assert_eq!(event.confidence, Confidence::Fact);
        assert!(
            event.title.contains("STOPPED"),
            "the stop fact must say collection stopped: {}",
            event.title
        );
    }

    /// The panic facts reach the persistent event log while the recorder is reachable:
    /// the hole survives into replay and `report`, not just the live session.
    #[test]
    fn panic_events_are_recorded_to_the_store() {
        let path = scratch_db();
        let mut engine = panicky_engine(Some(path.clone()), |_| true);
        for _ in 0..MAX_CONSECUTIVE_PANICS {
            assert!(matches!(
                engine.tick_guarded(),
                TickOutcome::Panicked { .. }
            ));
        }
        drop(engine); // Drop flushes the recording tail.

        let store = SqliteStore::open_readonly(&path).unwrap();
        let events = store.events_between(0, now_ms() + 60_000).unwrap();
        let stalls: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::CollectorStall)
            .collect();
        assert_eq!(
            stalls.len(),
            MAX_CONSECUTIVE_PANICS as usize,
            "every panicked tick must persist its narration"
        );
        assert!(
            stalls.iter().any(|e| e.severity == Severity::Critical),
            "the stop itself is a Critical fact in the recording"
        );
        cleanup_db(&path);
    }

    /// The end-to-end shape of the fix: through [`Collector::start`]'s real thread, a
    /// deterministic backend panic must surface as `Shared::stopped` (the UI's stop
    /// banner) plus `CollectorStall` narration in the story feed — and the process keeps
    /// running rather than aborting (the old behavior froze the UI with zero signal).
    #[test]
    fn collector_thread_survives_panics_and_marks_shared_stopped() {
        let engine = panicky_engine(None, |_| true);
        let collector = Collector::start(engine, Duration::from_millis(5), false);

        // Bounded wait for the thread to spend the panic budget and declare death.
        let mut stopped = None;
        for _ in 0..400 {
            if let Some(stop) = collector.shared.lock().unwrap().stopped.clone() {
                stopped = Some(stop);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let stop = stopped.expect("the collector must declare itself dead, never vanish silently");
        assert!(
            stop.reason.contains("decoder choked"),
            "the stop reason carries the panic summary: {}",
            stop.reason
        );
        assert!(stop.at_ms > 0);

        let sh = collector.shared.lock().unwrap();
        let stalls: Vec<_> = sh
            .history
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::CollectorStall)
            .collect();
        assert_eq!(
            stalls.len(),
            MAX_CONSECUTIVE_PANICS as usize,
            "every panicked tick narrates into the story feed"
        );
        assert_eq!(
            stalls.last().unwrap().severity,
            Severity::Critical,
            "the final narration is the Critical stop fact"
        );
        assert!(
            sh.latest[0].is_none(),
            "no fabricated sample may appear for a device that never answered"
        );
        // tick_seq advanced per narration, so an open timeline re-queries and sees them.
        assert!(sh.tick_seq >= MAX_CONSECUTIVE_PANICS as u64);
    }

    // ---- device lost / returned (scripted flaky backend) ----

    /// A backend whose `refresh_dynamic` errors on scripted tick indices — the audit's
    /// silently-disappearing device (driver reset, eGPU unplug, xe rebind) made flesh.
    /// Sample timestamps advance 10s per tick so the persistence test lands each good
    /// tick in its own 10s rollup bucket. The process probe mirrors the dynamic one (a
    /// dead device cannot list processes either), with one process on good ticks so the
    /// frozen-process-list assertion has something to freeze.
    struct FlakyBackend<F: Fn(u32) -> bool + Send> {
        tick: u32,
        fails: F,
    }

    impl<F: Fn(u32) -> bool + Send> GpuBackend for FlakyBackend<F> {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn devices(&mut self) -> Vec<DeviceId> {
            vec![DeviceId("flaky:0".into())]
        }
        fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
            Ok(StaticInfo {
                id: dev.clone(),
                vendor: Vendor::Unknown,
                name: "Flaky GPU".into(),
                backend: "flaky".into(),
                mem_total_bytes: Some(8 << 30),
                power_limit_mw: None,
                max_sm_clock_mhz: None,
                temp_slowdown_c: None,
                driver_version: None,
                process_hint: None,
                source_caveat: None,
            })
        }
        fn refresh_dynamic(&mut self, _dev: &DeviceId) -> Result<DynamicSample, BackendError> {
            let t = self.tick;
            self.tick += 1;
            if (self.fails)(t) {
                Err(BackendError::Unavailable("probe timed out".into()))
            } else {
                let mut s = sample(Some(50.0));
                s.ts_ms = 10_000 * (t as u64 + 1);
                Ok(s)
            }
        }
        fn refresh_processes(
            &mut self,
            _dev: &DeviceId,
        ) -> Result<Vec<ProcessSample>, BackendError> {
            // `tick` was already advanced by this tick's refresh_dynamic.
            if self.tick > 0 && (self.fails)(self.tick - 1) {
                Err(BackendError::Unavailable("probe timed out".into()))
            } else {
                Ok(vec![proc(Some(10.0))])
            }
        }
    }

    /// An engine over a [`FlakyBackend`] with the given failure script, persisting to
    /// `db` when given (otherwise live-only).
    fn flaky_engine(db: Option<PathBuf>, fails: impl Fn(u32) -> bool + Send + 'static) -> Engine {
        Engine::with_backends(
            vec![Box::new(FlakyBackend { tick: 0, fails })],
            EngineConfig {
                persist: db.is_some(),
                db_path: db,
                ..Default::default()
            },
        )
    }

    /// A never-failing persisting [`flaky_engine`] over `path`, retrying (bounded) while
    /// the store's instance lock is transiently held. WHY: the lock is `flock`-based and
    /// lives on the open file DESCRIPTION — when an unrelated test's `Command::spawn`
    /// forks while a previous holder's lock fd is open, the child inherits the fd and
    /// keeps the lock alive until exec closes it (CLOEXEC). The spawning thread is
    /// vfork-blocked through that window, but OTHER test threads (this one) keep running
    /// into it, so a test that re-acquires a lock its own process just dropped can lose
    /// the race for a few microseconds. Transient by construction; a REAL lock leak
    /// still fails the budget loudly. Tests opening a path's lock for the FIRST time
    /// are not exposed (a never-held lock cannot have been inherited) and use
    /// [`flaky_engine`] directly.
    fn flaky_engine_persisting(path: &std::path::Path) -> Engine {
        for _ in 0..500 {
            let engine = flaky_engine(Some(path.to_path_buf()), |_| false);
            if engine.db_path().is_some() {
                return engine;
            }
            drop(engine);
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "instance lock on {} still held after 5s — a real lock leak, not the \
             fork-inheritance window",
            path.display()
        );
    }

    /// The new kinds' wire tokens must match the spec/schema/suite spelling exactly.
    /// The conformance suite cannot see them on a mock run (the mock never loses a
    /// device), so the token spelling is pinned here instead.
    #[test]
    fn device_lifecycle_kinds_use_documented_wire_tokens() {
        assert_eq!(env_token(EventKind::DeviceLost), "device_lost");
        assert_eq!(env_token(EventKind::DeviceReturned), "device_returned");
    }

    /// The detection policy end to end: failures below the threshold are silent weather
    /// (the frame's `sample: null` already shows the gap on the wire), the Nth
    /// consecutive failure declares the loss as a Critical FACT with auditable evidence,
    /// and a dead device stays listed — and stays lost without re-narrating.
    #[test]
    fn device_lost_fires_exactly_at_the_threshold_with_evidence() {
        const N: u32 = DEVICE_LOST_AFTER_FAILED_PROBES;
        // Two good ticks, then the device falls off the bus for good.
        let mut engine = flaky_engine(None, |t| t >= 2);
        let lost_tick = 2 + N - 1;
        for t in 0..(lost_tick + 4) {
            let TickOutcome::Frame(frame) = engine.tick_guarded() else {
                panic!("flaky ticks never panic");
            };
            assert_eq!(frame.devices.len(), 1, "a lost device must stay listed");
            assert_eq!(frame.devices[0].sample.is_some(), t < 2);
            let lost: Vec<&Event> = frame
                .events
                .iter()
                .filter(|e| e.kind == EventKind::DeviceLost)
                .collect();
            if t == lost_tick {
                assert_eq!(lost.len(), 1, "the {N}th consecutive failure declares it");
                let e = lost[0];
                assert_eq!(e.severity, Severity::Critical);
                assert_eq!(e.confidence, Confidence::Fact, "the silence is observed");
                assert_eq!(e.device.0, "flaky:0");
                assert!(
                    e.title.contains("GPU0") && e.title.contains("device lost"),
                    "title must name the device and the loss: {}",
                    e.title
                );
                assert!(
                    !e.title.contains("likely"),
                    "a fact must not read hedged: {}",
                    e.title
                );
                assert!(
                    e.evidence.contains(&format!("{N} consecutive")),
                    "evidence must carry the tick count: {}",
                    e.evidence
                );
                // The last good sample was tick 1 → ts 20000 ms.
                assert!(
                    e.evidence.contains("20000 ms"),
                    "evidence must carry the last-good timestamp: {}",
                    e.evidence
                );
                assert!(
                    e.evidence.contains("probe timed out"),
                    "evidence must carry the driver's error string: {}",
                    e.evidence
                );
                assert!(
                    e.evidence.contains("cause not asserted"),
                    "the cause must be explicitly unclaimed: {}",
                    e.evidence
                );
            } else {
                assert!(
                    lost.is_empty(),
                    "tick {t}: device_lost only at the threshold tick"
                );
            }
            assert!(
                frame
                    .events
                    .iter()
                    .all(|e| e.kind != EventKind::DeviceReturned),
                "the device never recovers in this script"
            );
        }
    }

    /// Below the threshold a failing streak is per-tick weather: the frame's
    /// `sample: null` shows the gap, but neither lifecycle event may narrate (recovering
    /// from an undeclared loss is no story either).
    #[test]
    fn transient_probe_failures_below_threshold_stay_silent() {
        const N: u32 = DEVICE_LOST_AFTER_FAILED_PROBES;
        // N-1 consecutive failures, then the device answers again.
        let mut engine = flaky_engine(None, move |t| (2..2 + N - 1).contains(&t));
        for _ in 0..12 {
            let TickOutcome::Frame(frame) = engine.tick_guarded() else {
                panic!("flaky ticks never panic");
            };
            assert!(
                frame.events.iter().all(|e| {
                    e.kind != EventKind::DeviceLost && e.kind != EventKind::DeviceReturned
                }),
                "a sub-threshold blip must not narrate device loss"
            );
        }
    }

    /// The recovery edge: a lost device answering again narrates `device_returned`
    /// (Info, FACT) — and a second outage re-narrates `device_lost`, so a flapping
    /// device tells each chapter exactly once.
    #[test]
    fn device_returned_closes_the_story_and_a_second_outage_reopens_it() {
        const N: u32 = DEVICE_LOST_AFTER_FAILED_PROBES;
        assert_eq!(
            N, 5,
            "the scripted outage windows below assume the threshold"
        );
        // Outage 1: ticks 2..=6 (lost at 6); good 7..=8 (returned at 7);
        // outage 2: ticks 9..=13 (lost at 13); good from 14 (returned at 14).
        let mut engine = flaky_engine(None, |t| matches!(t, 2..=6 | 9..=13));
        let mut lost = Vec::new();
        let mut returned = Vec::new();
        for _ in 0..16 {
            let TickOutcome::Frame(frame) = engine.tick_guarded() else {
                panic!("flaky ticks never panic");
            };
            for e in frame.events {
                match e.kind {
                    EventKind::DeviceLost => lost.push(e),
                    EventKind::DeviceReturned => returned.push(e),
                    _ => {}
                }
            }
        }
        assert_eq!(lost.len(), 2, "each outage narrates its own loss");
        assert_eq!(returned.len(), 2, "each recovery narrates its own return");
        let r = &returned[0];
        assert_eq!(
            r.severity,
            Severity::Info,
            "recovery is good news, not an alarm"
        );
        assert_eq!(r.confidence, Confidence::Fact);
        assert!(
            r.title.contains("GPU0") && r.title.contains("returned"),
            "return title must name the device: {}",
            r.title
        );
        assert!(
            r.title.contains("data are missing"),
            "the return must size the hole it closes: {}",
            r.title
        );
        assert!(
            r.evidence.contains("never zero-filled"),
            "the return must state the gap stays blank: {}",
            r.evidence
        );
    }

    /// The full pipeline through [`Collector::start`]'s real thread: a device that stops
    /// answering surfaces as a per-device `Shared::lost` marker (the UI's stale
    /// affordances) WITHOUT tripping `Shared::stopped` (collection itself is fine, other
    /// devices would still be live), keeps its last process list frozen rather than
    /// flashing empty, and the marker clears when the device answers again — with both
    /// edges in the story feed.
    #[test]
    fn lost_device_marks_shared_and_recovery_clears_it() {
        // Good ticks 0..=2 (so a last process list exists), a long outage (declared lost
        // at tick 3 + threshold - 1), recovery from tick 100.
        let engine = flaky_engine(None, |t| (3..100).contains(&t));
        let collector = Collector::start(engine, Duration::from_millis(5), false);

        // Bounded wait for the loss to be declared.
        let mut lost_seen = false;
        for _ in 0..1000 {
            let sh = collector.shared.lock().unwrap();
            if sh.lost[0].is_some() {
                lost_seen = true;
                assert!(sh.stopped.is_none(), "device loss is not a collector stop");
                assert!(
                    !sh.processes[0].is_empty(),
                    "the last good process list stays frozen, never flashes empty"
                );
                assert!(sh.latest[0].is_some(), "the last good sample is kept");
                break;
            }
            drop(sh);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(lost_seen, "the device loss must reach Shared");

        // Bounded wait for the recovery to clear the marker.
        let mut cleared = false;
        for _ in 0..2000 {
            let sh = collector.shared.lock().unwrap();
            if sh.lost[0].is_none() {
                cleared = true;
                break;
            }
            drop(sh);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cleared, "recovery must clear the per-device marker");

        let sh = collector.shared.lock().unwrap();
        let kinds: Vec<EventKind> = sh.history.events().iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::DeviceLost));
        assert!(kinds.contains(&EventKind::DeviceReturned));
    }

    /// The recording half: both lifecycle facts reach the persistent event log, and the
    /// lost stretch leaves NO sample rollups — the gap stays blank (a hole), never
    /// zero-filled (CLAUDE.md's "never write raw zeros for absence" rule applied to an
    /// absent device).
    #[test]
    fn device_lifecycle_events_persist_and_the_gap_has_no_rollups() {
        const N: u32 = DEVICE_LOST_AFTER_FAILED_PROBES;
        let path = scratch_db();
        // Good tick 0 (bucket 10000), outage ticks 1..=N (lost at tick N), good from
        // tick N+1 (bucket 10000·(N+2)).
        let mut engine = flaky_engine(Some(path.clone()), move |t| (1..=N).contains(&t));
        for _ in 0..(N + 2) {
            assert!(matches!(engine.tick_guarded(), TickOutcome::Frame(_)));
        }
        drop(engine); // Drop flushes the recording tail.

        let store = SqliteStore::open_readonly(&path).unwrap();
        let events = store.events_between(0, now_ms() + 60_000).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::DeviceLost)
                .count(),
            1,
            "the loss is recorded exactly once"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::DeviceReturned)
                .count(),
            1,
            "the return is recorded exactly once"
        );
        let buckets: Vec<u64> = store
            .samples_between(&DeviceId("flaky:0".into()), 0, 1_000_000, Tier::TenSec)
            .unwrap()
            .iter()
            .map(|r| r.bucket_ms)
            .collect();
        assert_eq!(
            buckets,
            vec![10_000, 10_000 * (N as u64 + 2)],
            "only the good ticks produced rollups — the lost stretch is a blank gap"
        );
        cleanup_db(&path);
    }

    // ---- collector self-honesty: stall gap + slow probe (CollectorStall) ----

    /// A backend whose `refresh_dynamic` genuinely takes longer than [`SLOW_PROBE`] on
    /// scripted tick indices — the slow-probe note's trigger (NVML PCIe-throughput calls
    /// and a sleeping GPU waking really do block like this). The probe duration is
    /// measured with a real `Instant` inside `tick` (no clock seam exists there), so the
    /// scripted slowness must be real elapsed time inside the probe; sleeping 50ms past
    /// the threshold means scheduler jitter can only ADD time — the test can never flake
    /// toward "not slow".
    struct SluggishBackend<F: Fn(u32) -> bool + Send> {
        tick: u32,
        slow: F,
    }

    impl<F: Fn(u32) -> bool + Send> GpuBackend for SluggishBackend<F> {
        fn name(&self) -> &'static str {
            "sluggish"
        }
        fn devices(&mut self) -> Vec<DeviceId> {
            vec![DeviceId("sluggish:0".into())]
        }
        fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
            Ok(StaticInfo {
                id: dev.clone(),
                vendor: Vendor::Unknown,
                name: "Sluggish GPU".into(),
                backend: "sluggish".into(),
                mem_total_bytes: Some(8 << 30),
                power_limit_mw: None,
                max_sm_clock_mhz: None,
                temp_slowdown_c: None,
                driver_version: None,
                process_hint: None,
                source_caveat: None,
            })
        }
        fn refresh_dynamic(&mut self, _dev: &DeviceId) -> Result<DynamicSample, BackendError> {
            let t = self.tick;
            self.tick += 1;
            if (self.slow)(t) {
                std::thread::sleep(SLOW_PROBE + Duration::from_millis(50));
            }
            Ok(sample(Some(50.0)))
        }
        fn refresh_processes(
            &mut self,
            _dev: &DeviceId,
        ) -> Result<Vec<ProcessSample>, BackendError> {
            Ok(Vec::new())
        }
    }

    /// An engine over a [`SluggishBackend`] with the given slowness script, live-only.
    fn sluggish_engine(slow: impl Fn(u32) -> bool + Send + 'static) -> Engine {
        Engine::with_backends(
            vec![Box::new(SluggishBackend { tick: 0, slow })],
            EngineConfig::default(),
        )
    }

    /// The inter-tick stall gap — the collector narrating its OWN hole: a between-tick
    /// gap past [`stall_threshold`] must surface as a `CollectorStall` WARNING fact on
    /// the next successful tick, anchored to the first device, with the threshold in the
    /// auditable evidence — and a normal next gap must NOT re-narrate (the stall is a
    /// per-occurrence fact, not a latched state). Driven through the `set_last_tick_end`
    /// seam: `tick` reads `Instant::now()` directly, and sleeping a real 5s+ gap would
    /// be exactly the wall-clock flake the suite forbids.
    #[test]
    fn inter_tick_gap_past_threshold_narrates_a_collector_stall() {
        let mut engine = flaky_engine(None, |_| false);

        // First tick: no previous tick end exists, so no gap can be claimed.
        let TickOutcome::Frame(first) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        assert!(
            first
                .events
                .iter()
                .all(|e| e.kind != EventKind::CollectorStall),
            "no stall may be narrated before a gap was ever measurable"
        );

        // Inject a previous-tick end 6s in the past — past the 5s threshold at the
        // default 1s interval (stall_threshold = max(3×1s, 5s)).
        let past = Instant::now()
            .checked_sub(Duration::from_secs(6))
            .expect("system uptime exceeds 6s");
        engine.set_last_tick_end(past);
        let TickOutcome::Frame(stalled) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        let stalls: Vec<&Event> = stalled
            .events
            .iter()
            .filter(|e| e.kind == EventKind::CollectorStall)
            .collect();
        assert_eq!(stalls.len(), 1, "the gap must be narrated exactly once");
        let e = stalls[0];
        assert_eq!(
            e.severity,
            Severity::Warning,
            "a recording hole is a warning"
        );
        assert_eq!(
            e.confidence,
            Confidence::Fact,
            "the gap was measured, not inferred"
        );
        assert_eq!(
            e.device.0, "flaky:0",
            "anchored to the first device like every collector-scoped fact"
        );
        assert!(
            e.title.contains("collection stalled"),
            "title must say collection stalled: {}",
            e.title
        );
        assert!(
            e.title.contains("last good frame at"),
            "title must place the last good frame in time: {}",
            e.title
        );
        assert!(
            e.evidence.contains("inter-tick gap") && e.evidence.contains("stall threshold"),
            "evidence must carry the measured gap and the threshold it crossed: {}",
            e.evidence
        );
        assert!(
            stalled.devices[0].sample.is_some(),
            "the narrating tick itself collected normally — the hole is behind it"
        );

        // Recovery: the next tick's gap is normal and must stay silent.
        let TickOutcome::Frame(after) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        assert!(
            after
                .events
                .iter()
                .all(|e| e.kind != EventKind::CollectorStall),
            "a normal gap after a narrated stall must not re-narrate"
        );
    }

    /// The slow-probe foreshadowing note: one probe past [`SLOW_PROBE`] narrates an INFO
    /// `CollectorStall` fact against the slow device itself; a second slow probe inside
    /// the cooldown stays silent (a persistently-slow driver must not flood the feed);
    /// a fast probe afterwards narrates nothing — and every tick, slow or not, still
    /// collects real data (slow is not lost).
    #[test]
    fn slow_probe_notes_once_per_cooldown_and_recovers_silently() {
        // Ticks 0 and 1 are slow; tick 2 is fast.
        let mut engine = sluggish_engine(|t| t <= 1);

        let TickOutcome::Frame(first) = engine.tick_guarded() else {
            panic!("sluggish ticks never panic");
        };
        let notes: Vec<&Event> = first
            .events
            .iter()
            .filter(|e| e.kind == EventKind::CollectorStall)
            .collect();
        assert_eq!(notes.len(), 1, "a slow probe must be noted");
        let e = notes[0];
        assert_eq!(
            e.severity,
            Severity::Info,
            "a slow probe is a foreshadowing note, not an alarm"
        );
        assert_eq!(
            e.confidence,
            Confidence::Fact,
            "the probe duration was measured"
        );
        assert_eq!(
            e.device.0, "sluggish:0",
            "the note is anchored to the slow device itself"
        );
        assert!(
            e.title.contains("probe took") && e.title.contains("driver slow to respond"),
            "title must name the slowness and its shape: {}",
            e.title
        );
        assert!(
            e.title.contains("sluggish"),
            "title must name which backend was slow: {}",
            e.title
        );
        assert!(
            e.evidence.contains("refresh_dynamic") && e.evidence.contains("threshold"),
            "evidence must carry the probe and the threshold it crossed: {}",
            e.evidence
        );
        assert!(
            first.devices[0].sample.is_some(),
            "the slow tick still collected — slow is not lost"
        );

        // A second slow probe within the cooldown: rate-capped, no second note.
        let TickOutcome::Frame(second) = engine.tick_guarded() else {
            panic!("sluggish ticks never panic");
        };
        assert!(
            second
                .events
                .iter()
                .all(|e| e.kind != EventKind::CollectorStall),
            "a persistently-slow driver must not flood the story feed"
        );
        assert!(second.devices[0].sample.is_some());

        // Recovery: a fast probe narrates nothing and collection is normal.
        let TickOutcome::Frame(third) = engine.tick_guarded() else {
            panic!("sluggish ticks never panic");
        };
        assert!(
            third
                .events
                .iter()
                .all(|e| e.kind != EventKind::CollectorStall),
            "a recovered (fast) probe must stay silent"
        );
        assert!(third.devices[0].sample.is_some());
    }

    // ---- history reset (corrupt-db quarantine narration) ----

    /// The recorder narrating its own amnesia: a corrupt history file is quarantined at
    /// open and the store starts fresh — the collector must say so as a one-shot
    /// `HistoryReset` WARNING fact riding the FIRST tick (before the session's start
    /// mark), and the fresh store must work normally underneath it. Without the
    /// narration, the vanished history would masquerade as the GPU having no past.
    #[test]
    fn corrupt_history_file_narrates_history_reset_on_the_first_tick() {
        const GARBAGE: &[u8] = b"definitely not a sqlite database";
        let path = scratch_db();
        std::fs::write(&path, GARBAGE).unwrap();

        let mut engine = flaky_engine(Some(path.clone()), |_| false);
        let TickOutcome::Frame(first) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        let resets: Vec<usize> = first
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == EventKind::HistoryReset)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            resets.len(),
            1,
            "the quarantine must be narrated exactly once"
        );
        let e = &first.events[resets[0]];
        assert_eq!(
            e.severity,
            Severity::Warning,
            "lost history is a warning, not routine lifecycle"
        );
        assert_eq!(
            e.confidence,
            Confidence::Fact,
            "the quarantine happened — observed, not inferred"
        );
        assert_eq!(
            e.device.0, "flaky:0",
            "anchored to the first device like every collector-scoped fact"
        );
        assert!(
            e.title.contains("corrupt") && e.title.contains("started fresh"),
            "title must state the loss and the recovery: {}",
            e.title
        );
        assert!(
            e.evidence.contains(".corrupt-"),
            "evidence must point at the preserved quarantine file: {}",
            e.evidence
        );
        assert!(
            e.evidence.contains("fresh database"),
            "evidence must state a fresh database was created: {}",
            e.evidence
        );
        let start_pos = first
            .events
            .iter()
            .position(|ev| ev.kind == EventKind::RecordingStarted)
            .expect("the session's start mark rides the first frame too");
        assert!(
            resets[0] < start_pos,
            "the reset narrates before this session's start mark — the amnesia \
             predates the session"
        );

        // One-shot: the second tick must not repeat it.
        let TickOutcome::Frame(second) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        assert!(
            second
                .events
                .iter()
                .all(|e| e.kind != EventKind::HistoryReset),
            "the reset is narrated once, on the first tick only"
        );
        drop(engine); // Drop flushes the recording tail.

        // Downstream coherence: the fresh database is real and working — the narration
        // itself persisted into it (so replay and `report` see the reset), the start
        // mark landed, and the devices registered.
        let store = SqliteStore::open_readonly(&path).unwrap();
        let events = store.events_between(0, now_ms() + 60_000).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::HistoryReset)
                .count(),
            1,
            "the reset fact survives into replay and report"
        );
        assert!(
            events.iter().any(|e| e.kind == EventKind::RecordingStarted),
            "the fresh store carries this session's start mark"
        );
        assert!(
            !store.devices().unwrap().is_empty(),
            "the fresh store registered the devices"
        );
        drop(store);

        // The corrupt original was preserved (renamed aside, never deleted) — find it,
        // verify the bytes survived, and clean it up with the db.
        let dir = path.parent().unwrap();
        let prefix = format!("{}.corrupt-", path.file_name().unwrap().to_string_lossy());
        let quarantined: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|d| d.ok())
            .map(|d| d.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        assert!(
            !quarantined.is_empty(),
            "the corrupt file must be preserved for manual recovery, not deleted"
        );
        assert_eq!(
            std::fs::read(&quarantined[0]).unwrap(),
            GARBAGE,
            "preservation means the original bytes survive untouched"
        );
        for q in quarantined {
            let _ = std::fs::remove_file(q);
        }
        cleanup_db(&path);
    }

    // ---- session boundaries (recording_started / recording_stopped) ----

    /// The new kinds' wire tokens must match the spec/schema/suite spelling exactly —
    /// pinned here like the device-lifecycle tokens, since the conformance suite covers
    /// the mock run's shape, not every spelling source.
    #[test]
    fn session_boundary_kinds_use_documented_wire_tokens() {
        assert_eq!(env_token(EventKind::RecordingStarted), "recording_started");
        assert_eq!(env_token(EventKind::RecordingStopped), "recording_stopped");
    }

    /// The audit's recording-visibility hole, closed end to end: one Engine
    /// create→tick→finish cycle lands exactly one start mark (with auditable evidence:
    /// version, interval, backends, device count, db name) and exactly one stop mark
    /// (duration + frames folded) — and `finish` stays idempotent across the Drop that
    /// follows it. A second, clean session must then start WITHOUT indicting its
    /// predecessor.
    #[test]
    fn session_start_and_stop_marks_land_in_the_store() {
        let path = scratch_db();
        let mut engine = flaky_engine(Some(path.clone()), |_| false);
        for _ in 0..3 {
            assert!(matches!(engine.tick_guarded(), TickOutcome::Frame(_)));
        }
        assert!(
            engine.finish().is_some(),
            "finish must return the stop mark it wrote"
        );
        drop(engine); // Drop calls finish again — the mark must not double.

        let store = SqliteStore::open_readonly(&path).unwrap();
        let events = store.events_between(0, now_ms() + 60_000).unwrap();
        let starts: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::RecordingStarted)
            .collect();
        let stops: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::RecordingStopped)
            .collect();
        assert_eq!(
            starts.len(),
            1,
            "exactly one start mark (the first tick's re-insert is deduped)"
        );
        assert_eq!(
            stops.len(),
            1,
            "exactly one stop mark (finish is idempotent across Drop)"
        );

        let start = starts[0];
        assert_eq!(start.severity, Severity::Info);
        assert_eq!(start.confidence, Confidence::Fact);
        assert_eq!(start.device.0, "flaky:0", "anchored like CollectorStall");
        assert!(
            start.title.contains("recording started"),
            "title: {}",
            start.title
        );
        assert!(
            !start.title.contains("without a stop mark"),
            "a fresh database has no predecessor to indict: {}",
            start.title
        );
        assert!(
            start.evidence.contains(env!("CARGO_PKG_VERSION")),
            "evidence must carry the binary version: {}",
            start.evidence
        );
        assert!(
            start.evidence.contains("interval 1.0s"),
            "evidence must carry the tick interval: {}",
            start.evidence
        );
        assert!(
            start.evidence.contains("flaky (1 device)"),
            "evidence must carry backend names and device count: {}",
            start.evidence
        );
        let db_name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            start.evidence.contains(db_name),
            "evidence must carry the db basename: {}",
            start.evidence
        );

        let stop = stops[0];
        assert_eq!(stop.severity, Severity::Info);
        assert_eq!(stop.confidence, Confidence::Fact);
        assert_eq!(stop.device.0, "flaky:0");
        assert!(
            stop.title.contains("recording stopped"),
            "title: {}",
            stop.title
        );
        assert!(
            stop.evidence.contains("3 frames folded"),
            "evidence must count the session's frames: {}",
            stop.evidence
        );
        assert!(stop.ts_ms >= start.ts_ms, "the stop mark follows the start");
        drop(store);

        // A clean predecessor: the next session must start plainly. (The 2ms separator
        // is not a race wait — it only guarantees the two start marks land on distinct
        // millisecond timestamps, so the dedupe index cannot collapse them.)
        std::thread::sleep(Duration::from_millis(2));
        drop(flaky_engine_persisting(&path));
        let store = SqliteStore::open_readonly(&path).unwrap();
        let events = store.events_between(0, now_ms() + 60_000).unwrap();
        let second_start = events
            .iter()
            .filter(|e| e.kind == EventKind::RecordingStarted)
            .max_by_key(|e| e.ts_ms)
            .unwrap();
        assert!(
            !second_start.title.contains("without a stop mark"),
            "a cleanly-stopped predecessor must not be narrated as a crash: {}",
            second_start.title
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::RecordingStopped)
                .count(),
            2,
            "both sessions wrote their stop marks"
        );
        cleanup_db(&path);
    }

    /// The unclean-shutdown asymmetry: a session that died without its stop mark
    /// (SIGKILL, OOM kill, power loss — simulated by inserting a dangling start mark)
    /// is narrated by the NEXT session's start event, title and evidence both, because
    /// the dead session by definition could not narrate itself.
    #[test]
    fn unclean_shutdown_is_narrated_by_the_next_start() {
        let path = scratch_db();
        {
            // Simulate the kill: the previous session's start mark exists, its stop
            // mark never landed. (The store handle drops here, releasing the lock.)
            let (mut store, _) = SqliteStore::open(&path).unwrap();
            store
                .insert_events(&[Event {
                    ts_ms: 1_000,
                    device: DeviceId("flaky:0".into()),
                    kind: EventKind::RecordingStarted,
                    severity: Severity::Info,
                    confidence: Confidence::Fact,
                    title: "recording started — gpuviewer (previous session)".into(),
                    evidence: "test fixture".into(),
                }])
                .unwrap();
        }

        drop(flaky_engine_persisting(&path));

        let store = SqliteStore::open_readonly(&path).unwrap();
        let events = store.events_between(0, now_ms() + 60_000).unwrap();
        let new_start = events
            .iter()
            .filter(|e| e.kind == EventKind::RecordingStarted)
            .max_by_key(|e| e.ts_ms)
            .expect("the new session must write its own start mark");
        assert!(new_start.ts_ms > 1_000, "must be the NEW session's mark");
        assert_eq!(
            new_start.confidence,
            Confidence::Fact,
            "the missing stop mark is observed, not inferred"
        );
        assert!(
            new_start
                .title
                .contains("previous session ended without a stop mark"),
            "the title must narrate the unclean predecessor: {}",
            new_start.title
        );
        assert!(
            new_start.title.contains("crash, kill, or power loss"),
            "the title names the possible causes without asserting one: {}",
            new_start.title
        );
        assert!(
            new_start.evidence.contains("gap size is unknowable"),
            "the evidence must state the gap cannot be sized: {}",
            new_start.evidence
        );
        cleanup_db(&path);
    }

    /// Stream visibility: the start mark rides the FIRST tick's frame (that is how the
    /// `--json` stream, `--on-event`, and the story feed see it) and never repeats; a
    /// live-only session (no recorder) has no boundary marks at all — the marks describe
    /// the recording, and there is none.
    #[test]
    fn start_mark_rides_the_first_frame_only() {
        let path = scratch_db();
        let mut engine = flaky_engine(Some(path.clone()), |_| false);
        let TickOutcome::Frame(first) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        assert_eq!(
            first
                .events
                .iter()
                .filter(|e| e.kind == EventKind::RecordingStarted)
                .count(),
            1,
            "the start mark must ride the first frame"
        );
        let TickOutcome::Frame(second) = engine.tick_guarded() else {
            panic!("scripted to never fail");
        };
        assert!(
            second
                .events
                .iter()
                .all(|e| e.kind != EventKind::RecordingStarted),
            "the start mark must not repeat"
        );
        drop(engine);
        cleanup_db(&path);

        let mut live_only = flaky_engine(None, |_| false);
        let TickOutcome::Frame(frame) = live_only.tick_guarded() else {
            panic!("scripted to never fail");
        };
        assert!(
            frame.events.iter().all(|e| {
                e.kind != EventKind::RecordingStarted && e.kind != EventKind::RecordingStopped
            }),
            "a live-only session records nothing, so it must not claim a recording began"
        );
        assert!(
            live_only.finish().is_none(),
            "no recorder, no stop mark to write"
        );
    }

    /// The TUI quit path: `Collector::shutdown` stops the thread cooperatively, which
    /// drops the engine and writes the stop mark — a quit must read as a clean session
    /// end, never as a crash, to the next session.
    #[test]
    fn collector_shutdown_writes_the_stop_mark() {
        let path = scratch_db();
        let engine = flaky_engine(Some(path.clone()), |_| false);
        let mut collector = Collector::start(engine, Duration::from_millis(5), false);

        // Bounded wait for at least one folded frame, so the session resembles a real one.
        let mut ticked = false;
        for _ in 0..1000 {
            if collector.shared.lock().unwrap().tick_seq > 0 {
                ticked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(ticked, "the collector must tick before the shutdown");

        collector.shutdown();

        // The mark lands when the engine drops. `shutdown` waits boundedly; poll the
        // store rather than assume the join won the race (a wedged thread writes the
        // mark whenever it finally exits).
        let mut found = false;
        for _ in 0..1000 {
            let store = SqliteStore::open_readonly(&path).unwrap();
            let events = store.events_between(0, now_ms() + 60_000).unwrap();
            if events.iter().any(|e| e.kind == EventKind::RecordingStopped) {
                found = true;
                break;
            }
            drop(store);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            found,
            "a TUI-style quit must write the recording_stopped mark"
        );
        cleanup_db(&path);
    }

    // ---- engine-side data-source claim (the mock-fallback --db hole) ----

    /// The narrow no-GPU→mock-fallback case: the engine opens the store knowing its
    /// backends are mock, so a real-stamped --db is refused AT THE OPEN — zero writes,
    /// including the device-identity upserts that used to leak before main.rs's
    /// post-construction check could refuse.
    #[test]
    fn mock_fallback_engine_refuses_a_real_stamped_db_with_zero_writes() {
        let path = scratch_db();
        drop(SqliteStore::open_recording(&path, DataSource::Real).unwrap());

        let engine = Engine::with_backends(
            vec![Box::new(gpuviewer_core::mock::MockBackend::new())],
            EngineConfig {
                persist: true,
                db_path: Some(path.clone()),
                ..Default::default()
            },
        );
        assert!(engine.mock_in_use(), "precondition: the fallback shape");
        assert!(
            engine.db_path().is_none(),
            "the engine must not hold a recorder on a mismatched database"
        );
        drop(engine);

        let store = SqliteStore::open_readonly(&path).unwrap();
        assert_eq!(
            store.data_source().unwrap(),
            Some(DataSource::Real),
            "the stamp survives untouched"
        );
        assert!(
            store.devices().unwrap().is_empty(),
            "ZERO writes — not even device-identity rows may leak"
        );
        assert!(
            store
                .events_between(0, now_ms() + 60_000)
                .unwrap()
                .is_empty(),
            "no events either — including session marks"
        );
        cleanup_db(&path);
    }

    /// The engine stamps a fresh --db with what the backends ACTUALLY are, not what the
    /// flags said: mock backends claim mock, real (non-mock) backends claim real.
    #[test]
    fn engine_stamps_a_fresh_db_with_the_backends_actual_source() {
        let path = scratch_db();
        let engine = Engine::with_backends(
            vec![Box::new(gpuviewer_core::mock::MockBackend::new())],
            EngineConfig {
                persist: true,
                db_path: Some(path.clone()),
                ..Default::default()
            },
        );
        assert!(engine.db_path().is_some());
        drop(engine);
        let store = SqliteStore::open_readonly(&path).unwrap();
        assert_eq!(store.data_source().unwrap(), Some(DataSource::Mock));
        drop(store);
        cleanup_db(&path);

        let path = scratch_db();
        drop(flaky_engine(Some(path.clone()), |_| false));
        let store = SqliteStore::open_readonly(&path).unwrap();
        assert_eq!(
            store.data_source().unwrap(),
            Some(DataSource::Real),
            "a non-mock backend set claims real"
        );
        drop(store);
        cleanup_db(&path);
    }

    #[test]
    fn event_sink_rate_caps_spawns_per_minute() {
        let mut sink = EventSink::new("true".into());
        let ev = Event {
            ts_ms: 1,
            device: DeviceId("d".into()),
            kind: EventKind::ProcessExited,
            severity: Severity::Info,
            confidence: Confidence::Fact,
            title: "t".into(),
            evidence: "e".into(),
        };
        // Fire well past the cap; the window must never exceed SINK_MAX_PER_MIN.
        for _ in 0..(SINK_MAX_PER_MIN + 25) {
            sink.fire(&ev);
        }
        assert_eq!(
            sink.window.len(),
            SINK_MAX_PER_MIN,
            "spawns within the minute must be capped"
        );
        assert!(
            sink.warned,
            "exceeding the cap must set the one-time warning"
        );
    }
}
