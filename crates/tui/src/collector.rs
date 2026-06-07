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

/// Consecutive panicked ticks the collector survives before declaring itself dead. One
/// panicked tick is dropped-frame weather (a decoder edge case on one bad sample — the
/// same per-tick family as `NVML_ERROR_NOT_SUPPORTED` or an absent sysfs file); this many
/// in a row is a deterministic fault, and retrying forever would narrate an event per tick
/// while burning a core on a known-broken probe.
pub const MAX_CONSECUTIVE_PANICS: u32 = 3;

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
    /// Consecutive panicked ticks ([`Engine::tick_guarded`]); any clean tick resets it.
    consecutive_panics: u32,
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
            consecutive_panics: 0,
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
        let anchor = self
            .devices
            .first()
            .map(|(_, id, _)| id.clone())
            .unwrap_or_else(|| DeviceId("collector".into()));
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
                title: "collector tick panicked — frame dropped, the recording has a hole".into(),
                evidence: format!(
                    "panic: {summary}; consecutive panic {n} of {MAX_CONSECUTIVE_PANICS} \
                     tolerated, collection retries next tick; run with RUST_BACKTRACE=1 \
                     for a backtrace"
                ),
            }
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
    if s.throttle.any() {
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
            tick_seq: 0,
            stopped: None,
        }));
        let paused = Arc::new(AtomicBool::new(false));

        let s = Arc::clone(&shared);
        let p = Arc::clone(&paused);
        std::thread::spawn(move || {
            let mut cadence = Backoff::new(backoff, interval, n);
            loop {
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
                                }
                                sh.processes[i] = fd.processes.clone();
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
                std::thread::sleep(effective);
            }
        });

        Self { shared, paused }
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
            })),
            paused: Arc::new(AtomicBool::new(false)),
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
        // Bare sample (everything None, no throttle): no signal at all.
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
        thr.throttle.thermal = true;
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
        for ext in ["-wal", "-shm"] {
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
