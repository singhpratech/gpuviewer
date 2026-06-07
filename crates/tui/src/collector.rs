//! Collection engine shared by the TUI thread and `--json` mode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpuviewer_core::{
    all_backends, now_ms, Confidence, DeviceId, DynamicSample, Event, EventEngine, EventKind,
    GpuBackend, ProcessSample, Severity, StaticInfo,
};
use gpuviewer_history::{HistoryStore, Recorder, SqliteStore};
use serde::Serialize;

/// One collection tick's output for one device.
#[derive(Serialize)]
pub struct FrameDevice {
    pub id: DeviceId,
    pub name: String,
    /// Total VRAM, so JSON consumers can compute used/total without a second query.
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

/// Normalize a PCI address (`domain:bus:dev.func`) for cross-backend dedupe: NVML reports
/// `00000000:01:00.0` while sysfs reports `0000:01:00.0` — the same physical GPU. Lowercase
/// everything; trim/zero-pad the domain to 4 hex digits (a genuinely >16-bit domain keeps
/// its extra digits — both sources print those the same way). Returns `None` for anything
/// that isn't a PCI address (`mock:…`, `nvml:0` fallback ids) — those are never deduped:
/// wrongly merging two distinct devices is worse than listing one twice.
fn normalize_pci_id(id: &str) -> Option<String> {
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
    /// Override the history database path (else the XDG default, mock-separated).
    pub db_path: Option<PathBuf>,
    /// The configured tick interval — the basis of the stall-gap threshold.
    pub interval: Duration,
    /// Shell command run for every emitted event (`sh -c CMD`), or `None`.
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
    /// `--on-event` sink, shared with the JSON path so both modes fire it.
    sink: Option<EventSink>,
}

impl Engine {
    /// Build the engine from a configuration. Device discovery and dedupe are unchanged from
    /// the live-only path; persistence is best-effort: a store that fails to open (disk full,
    /// permission) logs to stderr and the monitor continues WITHOUT a recorder — losing the
    /// recording is never worth crashing the live view.
    pub fn new(config: EngineConfig) -> Self {
        let mut backends = all_backends(config.force_mock);
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
        let mut recorder = None;
        let mut db_path = None;
        let mut pending_reset = None;
        if config.persist {
            let opened = match &config.db_path {
                Some(p) => SqliteStore::open(p),
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
                    recorder = Some(rec);
                }
                Err(e) => {
                    eprintln!("gpuviewer: history persistence disabled (store open failed): {e}");
                }
            }
        }

        let sink = config.on_event.map(EventSink::new);

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
            sink,
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
                let anchor = self
                    .devices
                    .first()
                    .map(|(_, id, _)| id.clone())
                    .unwrap_or_else(|| DeviceId("collector".into()));
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

        for (bi, id, info) in &self.devices {
            let backend = &mut self.backends[*bi];
            let probe_start = Instant::now();
            let sample = backend.refresh_dynamic(id).ok();
            let probe_dur = probe_start.elapsed();
            let processes = backend.refresh_processes(id).unwrap_or_default();

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

        Frame {
            ts_ms: now_ms(),
            devices: frame_devices,
            events,
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
        // The tail of the recording would otherwise be lost on a clean exit (Ctrl-C / `q`).
        self.flush();
    }
}

/// Fire-and-forget runner for the `--on-event` shell command. Each emitted event spawns
/// `sh -c CMD` with the event surfaced through `GPV_EVENT_*` environment variables; the child
/// is reaped lazily (a `try_wait` sweep on the next fire) so a slow hook never blocks a tick.
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
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.cmd)
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

/// Whether a single device's frame reads as idle for the adaptive-backoff decision: device
/// util below the threshold AND no attached process itself shows real GPU use. Pure so the
/// backoff state machine is unit-tested without a tick loop.
///
/// A device with no util sample is treated as NOT idle: we cannot prove it is asleep, and
/// stretching the cadence on an unknown could miss a throttle onset.
pub fn device_is_idle(sample: Option<&DynamicSample>, procs: &[ProcessSample]) -> bool {
    let Some(s) = sample else { return false };
    let Some(util) = s.util_pct else { return false };
    if util >= IDLE_UTIL_PCT {
        return false;
    }
    !procs
        .iter()
        .any(|p| p.util_pct.map(|u| u >= IDLE_UTIL_PCT).unwrap_or(false))
}

/// A device counts as idle below this util (device or any process). Matches the "low-power
/// cadence" wedge: a desktop GPU at <5% with nothing computing is genuinely asleep.
const IDLE_UTIL_PCT: f32 = 5.0;

/// Consecutive all-idle ticks before the cadence stretches.
pub const BACKOFF_AFTER_IDLE_TICKS: u32 = 60;

/// Idle cadence is this multiple of the configured interval, capped at [`BACKOFF_CAP`].
const BACKOFF_MULTIPLIER: u32 = 5;
const BACKOFF_CAP: Duration = Duration::from_secs(10);

/// The effective sleep for the next loop iteration given how many consecutive all-idle ticks
/// have elapsed. ANY non-idle tick resets `idle_streak` to 0 (the caller's job), which snaps
/// the cadence back to the configured interval instantly. Pure for unit testing.
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
}

pub struct Collector {
    pub shared: Arc<Mutex<Shared>>,
    pub paused: Arc<AtomicBool>,
}

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
        }));
        let paused = Arc::new(AtomicBool::new(false));

        let s = Arc::clone(&shared);
        let p = Arc::clone(&paused);
        std::thread::spawn(move || {
            // Consecutive all-idle ticks; any non-idle tick snaps it back to 0.
            let mut idle_streak: u32 = 0;
            loop {
                let mut all_idle = true;
                if !p.load(Ordering::Relaxed) {
                    let frame = engine.tick();
                    let mut sh = s.lock().unwrap();
                    for (i, fd) in frame.devices.iter().enumerate() {
                        if let Some(sample) = &fd.sample {
                            sh.history.push_sample(&fd.id, sample.clone());
                            sh.latest[i] = Some(sample.clone());
                        }
                        if !device_is_idle(fd.sample.as_ref(), &fd.processes) {
                            all_idle = false;
                        }
                        sh.processes[i] = fd.processes.clone();
                    }
                    sh.history.push_events(frame.events);
                } else {
                    // While paused we are not observing anything, so do not let the idle
                    // streak grow (it would back off on resume against stale state).
                    all_idle = false;
                }

                // Update the streak and the effective cadence for the next sleep.
                let effective = if backoff {
                    if all_idle {
                        idle_streak = idle_streak.saturating_add(1);
                    } else {
                        idle_streak = 0;
                    }
                    effective_interval(interval, idle_streak)
                } else {
                    interval
                };
                effective_interval_ms.store(effective.as_millis() as u64, Ordering::Relaxed);
                std::thread::sleep(effective);
            }
        });

        Self { shared, paused }
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
    let infos = engine.static_infos();
    let n = infos.len();
    Collector {
        shared: Arc::new(Mutex::new(Shared {
            infos,
            latest: vec![None; n],
            processes: vec![Vec::new(); n],
            history: HistoryStore::new(1800, 5000),
            mock: true,
            db_path,
            interval_ms: 1000,
            effective_interval_ms: Arc::new(AtomicU64::new(1000)),
        })),
        paused: Arc::new(AtomicBool::new(false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpuviewer_core::{ProcessKind, ThrottleReasons};

    fn sample(util: Option<f32>) -> DynamicSample {
        DynamicSample {
            ts_ms: 1_000,
            util_pct: util,
            mem_used_bytes: None,
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            throttle: ThrottleReasons::default(),
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

    #[test]
    fn normalize_pci_id_unifies_nvml_and_sysfs_forms() {
        // NVML's 8-hex-digit domain and sysfs's 4-digit domain are the same device.
        assert_eq!(
            normalize_pci_id("00000000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            normalize_pci_id("0000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        // NVML historically uppercases hex; normalization is case-insensitive.
        assert_eq!(
            normalize_pci_id("00000000:0A:00.0").as_deref(),
            Some("0000:0a:00.0")
        );
        // A non-zero domain survives the trim/pad in both spellings.
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
        // Mock and index-fallback ids must never dedupe against anything.
        assert_eq!(normalize_pci_id("mock:0000:01:00.0"), None);
        assert_eq!(normalize_pci_id("nvml:0"), None);
        assert_eq!(normalize_pci_id(""), None);
        assert_eq!(normalize_pci_id("0000:01:00"), None); // no function part
        assert_eq!(normalize_pci_id("0000:01:00.0.1"), None); // trailing junk
        assert_eq!(normalize_pci_id("0000:01:02:00.0"), None); // extra segment
    }

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
        assert!(device_is_idle(Some(&sample(Some(2.0))), &[proc(Some(1.0))]));
        // Device busy → not idle.
        assert!(!device_is_idle(Some(&sample(Some(40.0))), &[]));
        // Device idle but a process is computing → not idle (work is queued/running).
        assert!(!device_is_idle(
            Some(&sample(Some(1.0))),
            &[proc(Some(80.0))]
        ));
        // Unknown device util → never assume idle (could mask a throttle onset).
        assert!(!device_is_idle(Some(&sample(None)), &[]));
        // No sample at all → not idle.
        assert!(!device_is_idle(None, &[]));
    }

    // ---- --on-event sink ----

    /// The hook receives the event through the GPV_EVENT_* environment. We run
    /// `sh -c 'printenv GPV_EVENT_KIND > <tmpfile>'` for a synthetic event and read the file
    /// back, after waiting for the (detached) child — proving the env wiring, not just spawn.
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

        let mut sink = EventSink::new(format!("printenv GPV_EVENT_KIND > {}", out.display()));
        sink.fire(&Event {
            ts_ms: 42,
            device: DeviceId("0000:01:00.0".into()),
            kind: EventKind::ThrottleStart,
            severity: Severity::Warning,
            confidence: Confidence::Fact,
            title: "t".into(),
            evidence: "e".into(),
        });

        // Wait (bounded) for the detached child to write the file.
        let mut got = String::new();
        for _ in 0..200 {
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
