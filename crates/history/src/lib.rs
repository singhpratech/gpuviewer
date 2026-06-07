//! gpuviewer-history — the flight recorder's storage.
//!
//! Two layers:
//! - RAM ring buffers ([`DeviceHistory`]/[`HistoryStore`]) for the live window + an event log.
//! - SQLite ([`store`]) for the persistent, replayable tail: 10s/1m downsampled rollups +
//!   an append-only event log, with retention pruning. The [`Recorder`] is the bridge — it
//!   folds the raw 1 Hz stream into bucket aggregates and flushes completed buckets to the
//!   store. **Raw 1 Hz samples NEVER reach SQLite** (CLAUDE.md decision; netdata's dbengine
//!   lesson): only the per-bucket min/avg/max do.

use std::collections::{HashMap, VecDeque};

use gpuviewer_core::{DeviceId, DynamicSample, Event, ProcessKind, ProcessSample};

pub mod store;

pub use store::{
    DataSource, DeviceRow, ExportCounts, ProcessRollup, SampleRollup, SqliteStore, StoreError,
    Tier, RETAIN_10S_MS, RETAIN_1M_MS, RETAIN_EVENTS_MS, SCHEMA_VERSION,
};

/// Fixed-capacity ring of samples for one device's live window.
pub struct DeviceHistory {
    samples: VecDeque<DynamicSample>,
    cap: usize,
}

impl DeviceHistory {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, s: DynamicSample) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    pub fn latest(&self) -> Option<&DynamicSample> {
        self.samples.back()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DynamicSample> {
        self.samples.iter()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// All devices' rings + the event log.
pub struct HistoryStore {
    per_device: HashMap<DeviceId, DeviceHistory>,
    events: Vec<Event>,
    sample_cap: usize,
    event_cap: usize,
}

impl HistoryStore {
    pub fn new(sample_cap: usize, event_cap: usize) -> Self {
        Self {
            per_device: HashMap::new(),
            events: Vec::new(),
            sample_cap,
            event_cap,
        }
    }

    pub fn push_sample(&mut self, dev: &DeviceId, s: DynamicSample) {
        self.per_device
            .entry(dev.clone())
            .or_insert_with(|| DeviceHistory::new(self.sample_cap))
            .push(s);
    }

    pub fn push_events(&mut self, evs: impl IntoIterator<Item = Event>) {
        self.events.extend(evs);
        if self.events.len() > self.event_cap {
            let drop = self.events.len() - self.event_cap;
            self.events.drain(..drop);
        }
    }

    pub fn device(&self, dev: &DeviceId) -> Option<&DeviceHistory> {
        self.per_device.get(dev)
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

// ===========================================================================================
// Recorder — folds the raw 1 Hz stream into 10s/1m bucket rollups and flushes to SQLite.
// ===========================================================================================

/// Auto-prune cadence: prune once per ~360 completed 10s buckets (~1h of wall time). Pruning
/// is a few `DELETE`s; doing it hourly keeps the database bounded without churning every tick.
const PRUNE_EVERY_FLUSHES: u64 = 360;

/// Running aggregate of one metric over the raw frames in a bucket. Only frames where the
/// metric was present contribute; an all-absent metric stays `None` and is written as SQL
/// `NULL` (never `0`, which would read as a real measurement on the replay chart).
#[derive(Clone, Copy, Default)]
struct Agg {
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
    /// Count of *present* values (not bucket frames) — the denominator for `avg`.
    n: u32,
}

impl Agg {
    fn push(&mut self, v: Option<f64>) {
        let Some(v) = v else { return };
        self.min = Some(self.min.map_or(v, |m| m.min(v)));
        self.max = Some(self.max.map_or(v, |m| m.max(v)));
        self.sum += v;
        self.n += 1;
    }

    fn avg(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }
}

/// One device's accumulator for a single tier's current bucket. Holds the bucket key plus the
/// per-metric aggregates and throttle counters; flushed and reset when a frame crosses into
/// the next bucket.
#[derive(Default)]
struct SampleAccum {
    /// `None` until the first frame; otherwise the bucket key these aggregates belong to.
    bucket_ms: Option<u64>,
    /// Raw frames folded so far.
    n: u32,
    util: Agg,
    mem: Agg,
    power: Agg,
    temp: Agg,
    fan: Agg,
    sm_clock: Agg,
    throttle_n: u32,
    throttle_thermal_n: u32,
    throttle_power_n: u32,
    throttle_hw_n: u32,
}

impl SampleAccum {
    fn fold(&mut self, s: &DynamicSample) {
        self.n += 1;
        self.util.push(s.util_pct.map(|v| v as f64));
        self.mem.push(s.mem_used_bytes.map(|v| v as f64));
        self.power.push(s.power_mw.map(|v| v as f64));
        self.temp.push(s.temp_c.map(|v| v as f64));
        self.fan.push(s.fan_pct.map(|v| v as f64));
        self.sm_clock.push(s.sm_clock_mhz.map(|v| v as f64));
        // `throttle: None` (source cannot observe — §5.4) increments nothing: the
        // counters are counts of *observed-active* frames, so an unobservable source
        // recording all day still reports zero — a gap, not an asserted "never
        // throttled" tally.
        if let Some(t) = s.throttle {
            let (any, thermal, power_cap, hw) = store::throttle_flags(&t);
            self.throttle_n += any as u32;
            self.throttle_thermal_n += thermal as u32;
            self.throttle_power_n += power_cap as u32;
            self.throttle_hw_n += hw as u32;
        }
    }

    fn to_rollup(&self, device_id: &DeviceId) -> Option<SampleRollup> {
        let bucket_ms = self.bucket_ms?;
        Some(SampleRollup {
            device_id: device_id.clone(),
            bucket_ms,
            n: self.n,
            util_min: self.util.min.map(|v| v as f32),
            util_avg: self.util.avg().map(|v| v as f32),
            util_max: self.util.max.map(|v| v as f32),
            mem_avg: self.mem.avg().map(|v| v as u64),
            mem_max: self.mem.max.map(|v| v as u64),
            power_avg_mw: self.power.avg().map(|v| v as u32),
            power_max_mw: self.power.max.map(|v| v as u32),
            temp_avg_c: self.temp.avg().map(|v| v as f32),
            temp_max_c: self.temp.max.map(|v| v as f32),
            fan_max_pct: self.fan.max.map(|v| v as f32),
            sm_clock_min: self.sm_clock.min.map(|v| v as u32),
            sm_clock_avg: self.sm_clock.avg().map(|v| v as u32),
            sm_clock_max: self.sm_clock.max.map(|v| v as u32),
            throttle_n: self.throttle_n,
            throttle_thermal_n: self.throttle_thermal_n,
            throttle_power_n: self.throttle_power_n,
            throttle_hw_n: self.throttle_hw_n,
        })
    }

    fn reset(&mut self, bucket_ms: u64) {
        *self = SampleAccum::default();
        self.bucket_ms = Some(bucket_ms);
    }
}

/// One process's accumulator within a 10s bucket: peak memory, running means for util/cpu,
/// and the last-seen identity (a name/container can change across a process's life — keep the
/// most recent for the bucket's label). Built from the first frame, so `kind`/`name` always
/// reflect a real observation (no `ProcessKind` default to invent).
struct ProcAccum {
    name: String,
    kind: ProcessKind,
    mem_max: Option<u64>,
    util: Agg,
    cpu: Agg,
    container: Option<String>,
}

impl ProcAccum {
    fn new(p: &ProcessSample) -> Self {
        let mut a = Self {
            name: p.name.clone(),
            kind: p.kind,
            mem_max: None,
            util: Agg::default(),
            cpu: Agg::default(),
            container: None,
        };
        a.fold(p);
        a
    }

    fn fold(&mut self, p: &ProcessSample) {
        self.name = p.name.clone();
        self.kind = p.kind;
        if let Some(m) = p.mem_bytes {
            self.mem_max = Some(self.mem_max.map_or(m, |cur| cur.max(m)));
        }
        self.util.push(p.util_pct.map(|v| v as f64));
        self.cpu.push(p.cpu_pct.map(|v| v as f64));
        self.container = p.container.clone();
    }

    fn to_rollup(&self, device_id: &DeviceId, bucket_ms: u64, pid: u32) -> ProcessRollup {
        ProcessRollup {
            device_id: device_id.clone(),
            bucket_ms,
            pid,
            name: self.name.clone(),
            kind: self.kind,
            mem_max: self.mem_max,
            util_avg: self.util.avg().map(|v| v as f32),
            cpu_avg: self.cpu.avg().map(|v| v as f32),
            container: self.container.clone(),
        }
    }
}

/// The 10s process bucket for one device: its key plus per-pid accumulators.
#[derive(Default)]
struct ProcBucket {
    bucket_ms: Option<u64>,
    procs: HashMap<u32, ProcAccum>,
}

/// Per-device state inside the [`Recorder`]: a 10s and a 1m sample accumulator plus the 10s
/// process bucket. The 1m tier keeps no per-process detail (the long tail is device-level).
#[derive(Default)]
struct DevRecord {
    s10: SampleAccum,
    s1m: SampleAccum,
    p10: ProcBucket,
}

/// Folds the live 1 Hz stream into persistent rollups. Feed it every frame via
/// [`Recorder::observe`]; it accumulates into both tiers' current buckets and, when a frame's
/// timestamp crosses a bucket boundary, flushes the just-completed bucket's rows to the
/// [`SqliteStore`]. Call [`Recorder::flush`] on shutdown to persist the partial tail.
///
/// Cadence invariant (CLAUDE.md): the raw per-tick samples are never written to SQLite — only
/// the per-bucket aggregates this struct computes. The store sees at most one row per device
/// per 10s and one per 60s.
pub struct Recorder {
    store: SqliteStore,
    devices: HashMap<DeviceId, DevRecord>,
    /// Completed-bucket flushes since the last prune, and the newest ts seen (the basis for
    /// the next prune's retention cutoff).
    flushes_since_prune: u64,
    newest_ts_ms: u64,
}

impl Recorder {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            devices: HashMap::new(),
            flushes_since_prune: 0,
            newest_ts_ms: 0,
        }
    }

    /// Borrow the underlying store (for reads/`register_device`/replay queries).
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// Mutable access to the store (the collector registers devices through here).
    pub fn store_mut(&mut self) -> &mut SqliteStore {
        &mut self.store
    }

    /// Fold one frame for `dev` into both tiers. When `s.ts_ms` falls into a later bucket than
    /// the one currently accumulating, the completed bucket is flushed first, then the new
    /// frame starts the next bucket. The 10s process bucket flushes on the same 10s boundary.
    pub fn observe(&mut self, dev: &DeviceId, s: &DynamicSample, procs: &[ProcessSample]) {
        self.newest_ts_ms = self.newest_ts_ms.max(s.ts_ms);
        let b10 = s.ts_ms - s.ts_ms % Tier::TenSec.width_ms();
        let b1m = s.ts_ms - s.ts_ms % Tier::OneMin.width_ms();

        // Collect what to flush without holding the per-device borrow across the store calls.
        let mut sample_flushes: Vec<(Tier, SampleRollup)> = Vec::new();
        let mut proc_flush: Option<Vec<ProcessRollup>> = None;
        let mut crossed_10s = false;
        {
            let rec = self.devices.entry(dev.clone()).or_default();

            if rec.s10.bucket_ms.is_some_and(|b| b10 > b) {
                if let Some(r) = rec.s10.to_rollup(dev) {
                    sample_flushes.push((Tier::TenSec, r));
                }
                rec.s10.reset(b10);
                crossed_10s = true;
            } else if rec.s10.bucket_ms.is_none() {
                rec.s10.bucket_ms = Some(b10);
            }
            rec.s10.fold(s);

            if rec.s1m.bucket_ms.is_some_and(|b| b1m > b) {
                if let Some(r) = rec.s1m.to_rollup(dev) {
                    sample_flushes.push((Tier::OneMin, r));
                }
                rec.s1m.reset(b1m);
            } else if rec.s1m.bucket_ms.is_none() {
                rec.s1m.bucket_ms = Some(b1m);
            }
            rec.s1m.fold(s);

            // Process bucket aligns with the 10s tier.
            if rec.p10.bucket_ms.is_some_and(|b| b10 > b) {
                let bucket = rec.p10.bucket_ms.unwrap();
                let rows: Vec<ProcessRollup> = rec
                    .p10
                    .procs
                    .iter()
                    .map(|(pid, pa)| pa.to_rollup(dev, bucket, *pid))
                    .collect();
                proc_flush = Some(rows);
                rec.p10 = ProcBucket {
                    bucket_ms: Some(b10),
                    procs: HashMap::new(),
                };
            } else if rec.p10.bucket_ms.is_none() {
                rec.p10.bucket_ms = Some(b10);
            }
            for p in procs {
                match rec.p10.procs.get_mut(&p.pid) {
                    Some(acc) => acc.fold(p),
                    None => {
                        rec.p10.procs.insert(p.pid, ProcAccum::new(p));
                    }
                }
            }
        }

        for (tier, rollup) in sample_flushes {
            // A persistence failure must not crash the collector: the live view keeps working
            // even if the disk is full. Drop the rollup and move on.
            let _ = self.store.insert_sample_rollups(tier, &[rollup]);
        }
        if let Some(rows) = proc_flush {
            let _ = self.store.insert_process_rollups(&rows);
        }

        if crossed_10s {
            self.flushes_since_prune += 1;
            if self.flushes_since_prune >= PRUNE_EVERY_FLUSHES {
                self.flushes_since_prune = 0;
                let _ = self.store.prune(self.newest_ts_ms);
            }
        }
    }

    /// Append derived events to the store. Pass-through to [`SqliteStore::insert_events`].
    pub fn record_events(&mut self, events: &[Event]) {
        let _ = self.store.insert_events(events);
    }

    /// Force-write every device's partial buckets (call on shutdown so the last, incomplete
    /// 10s/1m window is not lost). Leaves accumulators empty so a subsequent `observe` starts
    /// clean buckets.
    pub fn flush(&mut self) {
        let mut samples: Vec<(Tier, SampleRollup)> = Vec::new();
        let mut procs: Vec<ProcessRollup> = Vec::new();
        for (dev, rec) in &mut self.devices {
            if let Some(r) = rec.s10.to_rollup(dev) {
                if r.n > 0 {
                    samples.push((Tier::TenSec, r));
                }
            }
            if let Some(r) = rec.s1m.to_rollup(dev) {
                if r.n > 0 {
                    samples.push((Tier::OneMin, r));
                }
            }
            if let Some(bucket) = rec.p10.bucket_ms {
                for (pid, pa) in &rec.p10.procs {
                    procs.push(pa.to_rollup(dev, bucket, *pid));
                }
            }
            rec.s10 = SampleAccum::default();
            rec.s1m = SampleAccum::default();
            rec.p10 = ProcBucket::default();
        }
        let tens: Vec<SampleRollup> = samples
            .iter()
            .filter(|(t, _)| *t == Tier::TenSec)
            .map(|(_, r)| r.clone())
            .collect();
        let mins: Vec<SampleRollup> = samples
            .iter()
            .filter(|(t, _)| *t == Tier::OneMin)
            .map(|(_, r)| r.clone())
            .collect();
        let _ = self.store.insert_sample_rollups(Tier::TenSec, &tens);
        let _ = self.store.insert_sample_rollups(Tier::OneMin, &mins);
        let _ = self.store.insert_process_rollups(&procs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpuviewer_core::ThrottleReasons;

    fn sample(ts: u64) -> DynamicSample {
        DynamicSample {
            ts_ms: ts,
            util_pct: Some(1.0),
            util_engine: None,
            mem_used_bytes: None,
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            throttle: Some(ThrottleReasons::default()),
        }
    }

    #[test]
    fn ring_caps_at_capacity() {
        let mut h = DeviceHistory::new(3);
        for i in 0..10 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.latest().unwrap().ts_ms, 9);
        assert_eq!(h.iter().next().unwrap().ts_ms, 7);
    }

    // ===================================================================================
    // SQLite store + Recorder tests. Each uses a unique scratch path under the temp dir
    // (pid + a counter) and cleans it up; CI has no GPU and no shared fixtures here.
    // ===================================================================================

    use gpuviewer_core::{
        Confidence, Event, EventKind, ProcessKind, ProcessSample, Severity, Vendor,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A unique, non-existent scratch db path; removed (with WAL/SHM/corrupt siblings) by
    /// the returned guard's Drop so a failing test still tidies up.
    struct Scratch {
        path: std::path::PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let name = format!("gpuviewer-hist-test-{}-{}.db", std::process::id(), n);
            Scratch {
                path: std::env::temp_dir().join(name),
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            for ext in ["-wal", "-shm", ".lock"] {
                let mut p = self.path.as_os_str().to_os_string();
                p.push(ext);
                let _ = std::fs::remove_file(p);
            }
            // Sweep any *.corrupt-* the corruption test left behind.
            if let Some(dir) = self.path.parent() {
                if let Some(stem) = self.path.file_name().and_then(|s| s.to_str()) {
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        for e in rd.flatten() {
                            if let Some(n) = e.file_name().to_str() {
                                if n.starts_with(&format!("{stem}.corrupt-")) {
                                    let _ = std::fs::remove_file(e.path());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn dev(id: &str) -> DeviceId {
        DeviceId(id.into())
    }

    /// A fully-populated sample at `ts_ms` with every metric present — the baseline a
    /// broken aggregate would corrupt visibly.
    fn full_sample(
        ts_ms: u64,
        util: f32,
        mem: u64,
        throttle: Option<ThrottleReasons>,
    ) -> DynamicSample {
        DynamicSample {
            ts_ms,
            util_pct: Some(util),
            util_engine: None,
            mem_used_bytes: Some(mem),
            power_mw: Some(100_000),
            temp_c: Some(60.0),
            fan_pct: Some(40.0),
            sm_clock_mhz: Some(1500),
            mem_clock_mhz: Some(7000),
            encoder_pct: None,
            decoder_pct: None,
            throttle,
        }
    }

    /// Rollup math: feed three known frames inside one 10s bucket, force a flush by crossing
    /// the boundary, and assert exact min/avg/max + the bucket key. A wrong aggregate (e.g.
    /// averaging over bucket frames instead of present-values, or off-by-one bucket math)
    /// fails here.
    #[test]
    fn rollup_min_avg_max_and_bucket_boundary() {
        let scratch = Scratch::new();
        let (store, was_reset) = SqliteStore::open(&scratch.path).unwrap();
        assert!(!was_reset);
        let mut rec = Recorder::new(store);
        let d = dev("0000:01:00.0");

        // Bucket [0, 10000): util 10/20/60 -> min 10, max 60, avg 30; mem 1/2/3 GiB.
        rec.observe(
            &d,
            &full_sample(1_000, 10.0, 1 << 30, Default::default()),
            &[],
        );
        rec.observe(
            &d,
            &full_sample(2_000, 20.0, 2 << 30, Default::default()),
            &[],
        );
        rec.observe(
            &d,
            &full_sample(9_000, 60.0, 3 << 30, Default::default()),
            &[],
        );
        // A frame in the next 10s bucket flushes the completed one.
        rec.observe(
            &d,
            &full_sample(11_000, 5.0, 1 << 30, Default::default()),
            &[],
        );

        let rows = rec
            .store()
            .samples_between(&d, 0, 10_000, Tier::TenSec)
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one completed 10s bucket");
        let r = &rows[0];
        assert_eq!(r.bucket_ms, 0);
        assert_eq!(r.n, 3);
        assert_eq!(r.util_min, Some(10.0));
        assert_eq!(r.util_max, Some(60.0));
        assert_eq!(r.util_avg, Some(30.0));
        assert_eq!(r.mem_max, Some(3 << 30));
        assert_eq!(r.mem_avg, Some(2 << 30)); // (1+2+3)/3 GiB
        assert_eq!(r.power_max_mw, Some(100_000));
        assert_eq!(r.sm_clock_min, Some(1500));
    }

    /// Absent metrics across the whole bucket must persist as SQL NULL (`None`), never 0 —
    /// a 0 would read as a real measurement on the replay chart.
    #[test]
    fn absent_metric_is_null_not_zero() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        let d = dev("dev-null");

        // util present; power absent in every frame -> power_* must be None.
        let mut s = full_sample(1_000, 50.0, 1 << 30, Default::default());
        s.power_mw = None;
        s.temp_c = None;
        rec.observe(&d, &s, &[]);
        let mut s2 = full_sample(2_000, 70.0, 2 << 30, Default::default());
        s2.power_mw = None;
        s2.temp_c = None;
        rec.observe(&d, &s2, &[]);
        rec.observe(
            &d,
            &full_sample(11_000, 1.0, 1 << 30, Default::default()),
            &[],
        );

        let r = &rec
            .store()
            .samples_between(&d, 0, 10_000, Tier::TenSec)
            .unwrap()[0];
        assert_eq!(r.power_avg_mw, None, "all-absent power must be NULL");
        assert_eq!(r.power_max_mw, None);
        assert_eq!(r.temp_avg_c, None);
        // util was present -> averaged over the two present frames only.
        assert_eq!(r.util_avg, Some(60.0));
    }

    /// Avg of a partially-present metric averages over the frames where it WAS present,
    /// not over all bucket frames (an absent frame must not pull the mean toward zero).
    #[test]
    fn partial_present_metric_averages_over_present_only() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        let d = dev("partial");

        let mut a = full_sample(1_000, 0.0, 1 << 30, Default::default());
        a.temp_c = Some(80.0); // present
        let mut b = full_sample(2_000, 0.0, 1 << 30, Default::default());
        b.temp_c = None; // absent
        rec.observe(&d, &a, &[]);
        rec.observe(&d, &b, &[]);
        rec.observe(
            &d,
            &full_sample(11_000, 0.0, 1 << 30, Default::default()),
            &[],
        );

        let r = &rec
            .store()
            .samples_between(&d, 0, 10_000, Tier::TenSec)
            .unwrap()[0];
        // Only the one present temp -> avg == that value, not 80/2.
        assert_eq!(r.temp_avg_c, Some(80.0));
        assert_eq!(r.temp_max_c, Some(80.0));
    }

    /// Throttle counters tally per reason across the bucket's frames — counts of
    /// *observed-active* frames only: a `throttle: None` frame (source cannot observe,
    /// §5.4) increments nothing, exactly like the `Default::default()` (= `None`)
    /// frames below. An unobserved frame and an observed-quiet frame both leave the
    /// counters alone; only observed-active frames count.
    #[test]
    fn throttle_counters_tally_per_reason() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        let d = dev("throttle");

        let thermal = ThrottleReasons {
            thermal: true,
            ..Default::default()
        };
        let hw = ThrottleReasons {
            hw_slowdown: true,
            ..Default::default()
        };
        rec.observe(&d, &full_sample(1_000, 90.0, 1 << 30, Some(thermal)), &[]);
        rec.observe(&d, &full_sample(2_000, 90.0, 1 << 30, Some(thermal)), &[]);
        rec.observe(&d, &full_sample(3_000, 90.0, 1 << 30, Some(hw)), &[]);
        // One unobservable frame (None) and one observed-quiet frame (Some(all-false)):
        // neither may increment any counter.
        rec.observe(&d, &full_sample(4_000, 90.0, 1 << 30, None), &[]);
        rec.observe(
            &d,
            &full_sample(5_000, 90.0, 1 << 30, Some(ThrottleReasons::default())),
            &[],
        );
        rec.observe(
            &d,
            &full_sample(11_000, 1.0, 1 << 30, Default::default()),
            &[],
        );

        let r = &rec
            .store()
            .samples_between(&d, 0, 10_000, Tier::TenSec)
            .unwrap()[0];
        assert_eq!(r.throttle_n, 3, "3 frames had any throttle reason");
        assert_eq!(r.throttle_thermal_n, 2);
        assert_eq!(r.throttle_hw_n, 1);
        assert_eq!(r.throttle_power_n, 0);
    }

    /// flush() persists the partial (uncrossed) tail bucket on shutdown.
    #[test]
    fn flush_persists_partial_buckets() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        let d = dev("flush");

        rec.observe(
            &d,
            &full_sample(1_000, 50.0, 1 << 30, Default::default()),
            &[],
        );
        // No boundary crossing yet -> nothing flushed.
        assert!(rec
            .store()
            .samples_between(&d, 0, 10_000, Tier::TenSec)
            .unwrap()
            .is_empty());
        rec.flush();
        let rows = rec
            .store()
            .samples_between(&d, 0, 10_000, Tier::TenSec)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].n, 1);
        assert_eq!(rows[0].util_avg, Some(50.0));
    }

    /// The 1m tier folds six 10s buckets' worth of frames into one row.
    #[test]
    fn one_min_tier_aggregates_across_ten_sec_buckets() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        let d = dev("minute");

        // Frames at 5s, 15s, ..., 55s all in 1m bucket [0,60000); util 0..50 step 10.
        for (i, ts) in (5_000..60_000).step_by(10_000).enumerate() {
            rec.observe(
                &d,
                &full_sample(ts, (i as f32) * 10.0, 1 << 30, Default::default()),
                &[],
            );
        }
        // Cross into the next minute to flush.
        rec.observe(
            &d,
            &full_sample(61_000, 0.0, 1 << 30, Default::default()),
            &[],
        );

        let rows = rec
            .store()
            .samples_between(&d, 0, 60_000, Tier::OneMin)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bucket_ms, 0);
        assert_eq!(rows[0].n, 6, "six frames in the minute");
        assert_eq!(rows[0].util_min, Some(0.0));
        assert_eq!(rows[0].util_max, Some(50.0));
        assert_eq!(rows[0].util_avg, Some(25.0)); // (0+10+20+30+40+50)/6
    }

    /// Events round-trip through the store unchanged: kind/severity/confidence/title/evidence
    /// reconstruct exactly (the honesty contract depends on confidence surviving storage).
    #[test]
    fn event_round_trip_preserves_all_fields() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        let evs = vec![
            Event {
                ts_ms: 5_000,
                device: dev("gpu0"),
                kind: EventKind::ThrottleStart,
                severity: Severity::Critical,
                confidence: Confidence::Fact,
                title: "GPU0 began throttling (hw slowdown)".into(),
                evidence: "throttle bits: [hw slowdown]; 95°C".into(),
            },
            Event {
                ts_ms: 6_000,
                device: dev("gpu0"),
                kind: EventKind::IdleGap,
                severity: Severity::Info,
                confidence: Confidence::Likely,
                title: "GPU0 sat idle 14s — likely a dataloader stall".into(),
                evidence: "util 92% -> mean 2% over 14s".into(),
            },
        ];
        store.insert_events(&evs).unwrap();

        let got = store.events_between(0, 10_000).unwrap();
        assert_eq!(got.len(), 2);
        for (a, b) in evs.iter().zip(&got) {
            assert_eq!(a.ts_ms, b.ts_ms);
            assert_eq!(a.device, b.device);
            assert_eq!(a.kind, b.kind, "kind must round-trip");
            assert_eq!(a.severity, b.severity, "severity must round-trip");
            assert_eq!(
                a.confidence, b.confidence,
                "confidence must round-trip (honesty contract)"
            );
            assert_eq!(a.title, b.title);
            assert_eq!(a.evidence, b.evidence);
        }
    }

    /// record_events on the Recorder is a passthrough insert.
    #[test]
    fn recorder_record_events_passthrough() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        rec.record_events(&[Event {
            ts_ms: 1_000,
            device: dev("g"),
            kind: EventKind::ProcessExited,
            severity: Severity::Info,
            confidence: Confidence::Fact,
            title: "left".into(),
            evidence: "gone".into(),
        }]);
        assert_eq!(rec.store().events_between(0, 2_000).unwrap().len(), 1);
    }

    /// Corruption recovery: garbage bytes at the path -> open renames it aside to
    /// *.corrupt-* and returns a usable, fresh store with was_reset=true.
    #[test]
    fn corruption_recovery_quarantines_and_reopens() {
        let scratch = Scratch::new();
        std::fs::write(
            &scratch.path,
            b"this is not a sqlite database, just junk bytes",
        )
        .unwrap();

        let (mut store, was_reset) = SqliteStore::open(&scratch.path).unwrap();
        assert!(was_reset, "a garbage file must trigger a reset");

        // The corrupt original was preserved, not deleted.
        let dir = scratch.path.parent().unwrap();
        let stem = scratch.path.file_name().unwrap().to_str().unwrap();
        let preserved = std::fs::read_dir(dir).unwrap().flatten().any(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with(&format!("{stem}.corrupt-")))
                .unwrap_or(false)
        });
        assert!(
            preserved,
            "corrupt file must be renamed to *.corrupt-*, not deleted"
        );

        // The fresh store is usable.
        store
            .insert_events(&[Event {
                ts_ms: 1,
                device: dev("d"),
                kind: EventKind::HistoryReset,
                severity: Severity::Info,
                confidence: Confidence::Fact,
                title: "reset".into(),
                evidence: "fresh".into(),
            }])
            .unwrap();
        assert_eq!(store.events_between(0, 10).unwrap().len(), 1);
    }

    /// A clean reopen of a healthy database does NOT report a reset (quick_check passes).
    #[test]
    fn healthy_reopen_is_not_a_reset() {
        let scratch = Scratch::new();
        {
            let (store, was_reset) = SqliteStore::open(&scratch.path).unwrap();
            assert!(!was_reset);
            store
                .register_device(&dev("d"), "GPU", Vendor::Nvidia, Some(1 << 30))
                .unwrap();
        }
        let (store, was_reset) = SqliteStore::open(&scratch.path).unwrap();
        assert!(!was_reset, "reopening a healthy db must not reset it");
        // Data from the first session survives.
        assert_eq!(store.devices().unwrap().len(), 1);
    }

    /// Retention pruning: rows older than the window vanish; newer rows survive. Uses a
    /// large `now_ms` so "old" rows are genuinely past the 48h/30d cutoffs.
    #[test]
    fn prune_drops_only_expired_rows() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        let d = dev("retain");
        let now: u64 = 100 * RETAIN_1M_MS; // far in the future relative to any window

        let old_10s = now - RETAIN_10S_MS - 60_000; // past 48h
        let new_10s = now - 60_000; // well within 48h
        let old_1m = now - RETAIN_1M_MS - 60_000; // past 30d
        let new_1m = now - RETAIN_10S_MS - 120_000; // past 48h but within 30d

        let mk = |bucket: u64| SampleRollup {
            device_id: d.clone(),
            bucket_ms: bucket,
            n: 1,
            util_min: Some(1.0),
            util_avg: Some(1.0),
            util_max: Some(1.0),
            mem_avg: None,
            mem_max: None,
            power_avg_mw: None,
            power_max_mw: None,
            temp_avg_c: None,
            temp_max_c: None,
            fan_max_pct: None,
            sm_clock_min: None,
            sm_clock_avg: None,
            sm_clock_max: None,
            throttle_n: 0,
            throttle_thermal_n: 0,
            throttle_power_n: 0,
            throttle_hw_n: 0,
        };
        store
            .insert_sample_rollups(Tier::TenSec, &[mk(old_10s), mk(new_10s)])
            .unwrap();
        store
            .insert_sample_rollups(Tier::OneMin, &[mk(old_1m), mk(new_1m)])
            .unwrap();
        store
            .insert_process_rollups(&[ProcessRollup {
                device_id: d.clone(),
                bucket_ms: old_10s,
                pid: 1,
                name: "old".into(),
                kind: ProcessKind::Compute,
                mem_max: None,
                util_avg: None,
                cpu_avg: None,
                container: None,
            }])
            .unwrap();
        store
            .insert_events(&[
                Event {
                    ts_ms: old_1m,
                    device: d.clone(),
                    kind: EventKind::ThrottleEnd,
                    severity: Severity::Info,
                    confidence: Confidence::Fact,
                    title: "old".into(),
                    evidence: "old".into(),
                },
                Event {
                    ts_ms: new_1m,
                    device: d.clone(),
                    kind: EventKind::ThrottleEnd,
                    severity: Severity::Info,
                    confidence: Confidence::Fact,
                    title: "new".into(),
                    evidence: "new".into(),
                },
            ])
            .unwrap();

        store.prune(now).unwrap();

        let tens = store.samples_between(&d, 0, now, Tier::TenSec).unwrap();
        assert_eq!(tens.len(), 1, "only the within-48h 10s row remains");
        assert_eq!(tens[0].bucket_ms, new_10s);

        let mins = store.samples_between(&d, 0, now, Tier::OneMin).unwrap();
        assert_eq!(mins.len(), 1, "only the within-30d 1m row remains");
        assert_eq!(mins[0].bucket_ms, new_1m);

        // The old process row (past 48h) is gone.
        assert!(store.processes_at(&d, old_10s).unwrap().is_empty());

        let evs = store.events_between(0, now).unwrap();
        assert_eq!(evs.len(), 1, "only the within-30d event remains");
        assert_eq!(evs[0].title, "new");
    }

    /// processes_at returns the bucket covering the cursor, and falls back to the nearest
    /// earlier bucket within 60s when the exact bucket is empty.
    #[test]
    fn processes_at_picks_right_bucket_with_fallback() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        let d = dev("procs");

        let mk = |bucket: u64, pid: u32, name: &str, mem: u64| ProcessRollup {
            device_id: d.clone(),
            bucket_ms: bucket,
            pid,
            name: name.into(),
            kind: ProcessKind::Compute,
            mem_max: Some(mem),
            util_avg: Some(10.0),
            cpu_avg: None,
            container: None,
        };
        // Buckets at 100000 and 110000; nothing at 120000.
        store
            .insert_process_rollups(&[mk(100_000, 1, "a", 1 << 30), mk(110_000, 2, "b", 2 << 30)])
            .unwrap();

        // Cursor inside the 110000 bucket -> that exact bucket.
        let at = store.processes_at(&d, 115_000).unwrap();
        assert_eq!(at.len(), 1);
        assert_eq!(at[0].pid, 2);
        assert_eq!(at[0].bucket_ms, 110_000);

        // Cursor in the empty 120000 bucket -> falls back to 110000 (within 60s).
        let fb = store.processes_at(&d, 125_000).unwrap();
        assert_eq!(fb.len(), 1);
        assert_eq!(fb[0].pid, 2, "falls back to nearest earlier bucket");
        assert_eq!(fb[0].bucket_ms, 110_000);

        // Cursor far past the last bucket (>60s) -> no fallback, empty.
        let none = store.processes_at(&d, 300_000).unwrap();
        assert!(
            none.is_empty(),
            "no bucket within 60s -> empty, not stale data"
        );
    }

    /// The Recorder folds processes into 10s buckets: peak memory, mean util/cpu, last name.
    #[test]
    fn recorder_process_rollup_aggregates() {
        let scratch = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        let mut rec = Recorder::new(store);
        let d = dev("procagg");

        let proc = |mem: u64, util: f32, cpu: f32| ProcessSample {
            pid: 42,
            name: "python".into(),
            kind: ProcessKind::Compute,
            mem_bytes: Some(mem),
            util_pct: Some(util),
            cpu_pct: Some(cpu),
            container: None,
        };
        rec.observe(
            &d,
            &full_sample(1_000, 50.0, 1 << 30, Default::default()),
            &[proc(1 << 30, 10.0, 100.0)],
        );
        rec.observe(
            &d,
            &full_sample(2_000, 50.0, 1 << 30, Default::default()),
            &[proc(3 << 30, 30.0, 200.0)],
        );
        // Cross 10s boundary to flush the process bucket.
        rec.observe(
            &d,
            &full_sample(11_000, 1.0, 1 << 30, Default::default()),
            &[],
        );

        let rows = rec.store().processes_at(&d, 5_000).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.pid, 42);
        assert_eq!(r.mem_max, Some(3 << 30), "peak memory across the bucket");
        assert_eq!(r.util_avg, Some(20.0)); // (10+30)/2
        assert_eq!(r.cpu_avg, Some(150.0)); // (100+200)/2
    }

    // ===================================================================================
    // Data-dir resolution — env-driven on EVERY OS (the design reason: hermetic tests and
    // child-process redirection must work without a `--db` flag). Process env is
    // process-global and the default test harness is multi-threaded, so every test that
    // touches these variables holds `DataDirEnv` (lock + snapshot + restore-on-drop).
    // ===================================================================================

    /// Every variable any OS's `default_data_dir` chain reads. All are snapshotted and
    /// restored together so a test for one OS's chain can never leak into another test.
    const DATA_DIR_VARS: [&str; 4] = ["XDG_DATA_HOME", "HOME", "LOCALAPPDATA", "USERPROFILE"];

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serializes env-mutating tests and restores the previous values on drop (also on
    /// panic — a failed assertion must not poison the environment for the next test).
    struct DataDirEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl DataDirEnv {
        fn new() -> Self {
            // A previous test panicking while holding the lock poisons it; the env was
            // still restored by its Drop, so the poison carries no information here.
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = DATA_DIR_VARS
                .iter()
                .map(|v| (*v, std::env::var_os(v)))
                .collect();
            Self { _lock: lock, saved }
        }

        fn set(&self, var: &str, value: impl AsRef<std::ffi::OsStr>) {
            // SAFETY: ENV_LOCK serializes every env-mutating test in this binary, and no
            // non-test code in this crate reads these variables concurrently.
            unsafe { std::env::set_var(var, value) };
        }

        fn clear(&self, var: &str) {
            // SAFETY: as in `set`.
            unsafe { std::env::remove_var(var) };
        }

        /// Point every per-OS data-dir root at `dir` — the multi-var treatment: the
        /// variables the local OS ignores are inert, and one helper keeps the redirect
        /// correct on the whole CI matrix.
        fn redirect_all(&self, dir: &std::path::Path) {
            for var in DATA_DIR_VARS {
                self.set(var, dir);
            }
        }
    }

    impl Drop for DataDirEnv {
        fn drop(&mut self) {
            for (var, value) in &self.saved {
                // SAFETY: as in `set`; still under the held lock.
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(var, v),
                        None => std::env::remove_var(var),
                    }
                }
            }
        }
    }

    /// The Linux/other chain, BYTE-IDENTICAL to shipped v1 (existing users' history.db
    /// must not move): XDG_DATA_HOME first, HOME/.local/share second, refusal third —
    /// with empty meaning unset, never "rooted at the current directory".
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn data_dir_linux_prefers_xdg_then_home_then_errors() {
        let env = DataDirEnv::new();
        env.set("XDG_DATA_HOME", "/scratch/xdg");
        env.set("HOME", "/scratch/home"); // decoy: must lose to XDG
        assert_eq!(
            store::default_data_dir().unwrap(),
            std::path::Path::new("/scratch/xdg/gpuviewer")
        );
        env.set("XDG_DATA_HOME", ""); // empty counts as unset
        assert_eq!(
            store::default_data_dir().unwrap(),
            std::path::Path::new("/scratch/home/.local/share/gpuviewer")
        );
        env.clear("XDG_DATA_HOME");
        env.set("HOME", "");
        assert!(
            matches!(store::default_data_dir(), Err(StoreError::NoDataDir)),
            "with nothing to resolve from, the only honest answer is a refusal"
        );
    }

    /// The Windows chain: LOCALAPPDATA first, USERPROFILE\AppData\Local second (service
    /// accounts can lack LOCALAPPDATA), refusal third. XDG must be ignored — it is set on
    /// plenty of Windows dev boxes (MSYS2/WSL spillover) and means nothing there.
    #[cfg(target_os = "windows")]
    #[test]
    fn data_dir_windows_prefers_localappdata_then_userprofile_then_errors() {
        let env = DataDirEnv::new();
        env.set("XDG_DATA_HOME", "C:\\decoy-xdg"); // must be ignored on Windows
        env.set("LOCALAPPDATA", "C:\\scratch\\Local");
        env.set("USERPROFILE", "C:\\scratch\\profile"); // decoy: must lose to LOCALAPPDATA
        assert_eq!(
            store::default_data_dir().unwrap(),
            std::path::Path::new("C:\\scratch\\Local\\gpuviewer")
        );
        env.set("LOCALAPPDATA", ""); // empty counts as unset…
        assert_eq!(
            store::default_data_dir().unwrap(),
            std::path::Path::new("C:\\scratch\\profile\\AppData\\Local\\gpuviewer")
        );
        env.clear("LOCALAPPDATA"); // …and so does genuinely absent
        assert_eq!(
            store::default_data_dir().unwrap(),
            std::path::Path::new("C:\\scratch\\profile\\AppData\\Local\\gpuviewer")
        );
        env.set("USERPROFILE", "");
        assert!(matches!(
            store::default_data_dir(),
            Err(StoreError::NoDataDir)
        ));
    }

    /// The macOS chain: HOME/Library/Application Support, else refusal. XDG must be
    /// ignored — a half-XDG layout would scatter the history across two conventions.
    #[cfg(target_os = "macos")]
    #[test]
    fn data_dir_macos_uses_home_library_application_support() {
        let env = DataDirEnv::new();
        env.set("XDG_DATA_HOME", "/decoy-xdg"); // must be ignored on macOS
        env.set("HOME", "/scratch/home");
        assert_eq!(
            store::default_data_dir().unwrap(),
            std::path::Path::new("/scratch/home/Library/Application Support/gpuviewer")
        );
        env.set("HOME", ""); // empty counts as unset…
        assert!(matches!(
            store::default_data_dir(),
            Err(StoreError::NoDataDir)
        ));
        env.clear("HOME"); // …and so does genuinely absent
        assert!(matches!(
            store::default_data_dir(),
            Err(StoreError::NoDataDir)
        ));
    }

    /// open_default selects a separate file for mock so simulated data can never contaminate
    /// real history. We only assert the filename, never touching the real history.db.
    #[test]
    fn mock_default_path_is_separate_from_real() {
        // Redirect every per-OS data-dir root at a scratch dir so the test never opens
        // the user's real history — on Linux, macOS, or Windows alike.
        let scratch_dir = std::env::temp_dir().join(format!(
            "gpuviewer-xdg-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let env = DataDirEnv::new();
        env.redirect_all(&scratch_dir);

        let (mock_store, _) = SqliteStore::open_default(true).unwrap();
        let (real_store, _) = SqliteStore::open_default(false).unwrap();
        // open_default doubles as a recording open, so the default files carry the stamp
        // their filename encodes — pre-marker defaults get retro-stamped the same way.
        assert_eq!(mock_store.data_source().unwrap(), Some(DataSource::Mock));
        assert_eq!(real_store.data_source().unwrap(), Some(DataSource::Real));
        let mock_name = mock_store.path().file_name().unwrap().to_str().unwrap();
        let real_name = real_store.path().file_name().unwrap().to_str().unwrap();
        assert!(
            mock_name.contains("-mock"),
            "mock db filename must contain -mock, got {mock_name}"
        );
        assert!(
            !real_name.contains("-mock"),
            "real db filename must not contain -mock, got {real_name}"
        );
        assert_ne!(
            mock_name, real_name,
            "mock and real must be different files"
        );
        // Both must live under our scratch dir (whatever per-OS subpath the resolution
        // appends), never the user's home.
        assert!(mock_store.path().starts_with(&scratch_dir));

        drop(mock_store);
        drop(real_store);
        drop(env); // restores the saved variables
        let _ = std::fs::remove_dir_all(&scratch_dir);
    }

    // ===================================================================================
    // Data-source stamp — the mock/--db contamination guard.
    // ===================================================================================

    /// A fresh database is stamped with the session's own source (both flavors), and the
    /// normal next session — same source — keeps recording without friction.
    #[test]
    fn fresh_db_is_stamped_with_session_source() {
        for source in [DataSource::Real, DataSource::Mock] {
            let scratch = Scratch::new();
            let (store, was_reset) = SqliteStore::open_recording(&scratch.path, source).unwrap();
            assert!(!was_reset);
            assert_eq!(store.data_source().unwrap(), Some(source));
            drop(store);
            let (store, _) = SqliteStore::open_recording(&scratch.path, source).unwrap();
            assert_eq!(
                store.data_source().unwrap(),
                Some(source),
                "a same-source reopen must keep the stamp"
            );
        }
    }

    /// Mock recording into a real-stamped database is refused with the exact actionable
    /// message — naming the file, the mismatch, and the escape hatches — and the refused
    /// open must leave the stamp untouched.
    #[test]
    fn mock_recording_into_real_db_is_refused() {
        let scratch = Scratch::new();
        drop(SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap());

        let err = SqliteStore::open_recording(&scratch.path, DataSource::Mock).unwrap_err();
        assert!(
            matches!(err, StoreError::DataSourceMismatch { .. }),
            "must refuse with DataSourceMismatch, got: {err}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "refusing to record mock data into {} — it contains real history \
                 (use --db elsewhere or --no-persist)",
                scratch.path.display()
            ),
        );
        // The refusal is a no-op on the file: still stamped real, still openable as real.
        let reader = SqliteStore::open_readonly(&scratch.path).unwrap();
        assert_eq!(reader.data_source().unwrap(), Some(DataSource::Real));
    }

    /// The reverse direction is just as forbidden: real samples hiding inside a mock file
    /// would mislabel both recordings.
    #[test]
    fn real_recording_into_mock_db_is_refused() {
        let scratch = Scratch::new();
        drop(SqliteStore::open_recording(&scratch.path, DataSource::Mock).unwrap());

        let err = SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("refusing to record real data into"),
            "wrong direction in message: {msg}"
        );
        assert!(
            msg.contains("it contains mock history"),
            "must name the db's flavor: {msg}"
        );
        assert!(
            msg.contains(scratch.path.to_str().unwrap()),
            "must name the file: {msg}"
        );
    }

    /// Read-only opens never consult the stamp: replaying/reporting mock history is fine
    /// (the UI labels it "(mock data)") — only co-mingled writes are forbidden. The reader
    /// can still ASK what the file holds, without claiming it.
    #[test]
    fn readonly_open_ignores_the_data_source_stamp() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open_recording(&scratch.path, DataSource::Mock).unwrap();
        store
            .insert_events(&[event_at(
                &dev("sim"),
                5,
                EventKind::ThrottleStart,
                "likely simulated",
            )])
            .unwrap();
        drop(store);

        // open_readonly takes no source at all — that is the point: reads are unconditional.
        let reader = SqliteStore::open_readonly(&scratch.path).unwrap();
        assert_eq!(reader.events_between(0, 10).unwrap().len(), 1);
        assert_eq!(reader.data_source().unwrap(), Some(DataSource::Mock));
    }

    /// A pre-marker (legacy) database — created before the stamp existed — is adopted by
    /// the next recording session's own source with its contents intact, then enforced
    /// from that stamp onward.
    #[test]
    fn legacy_unstamped_db_adopts_next_session_source() {
        let scratch = Scratch::new();
        {
            // Plain `open` writes no stamp — exactly what a pre-marker gpuviewer left behind.
            let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
            assert_eq!(store.data_source().unwrap(), None, "legacy db has no stamp");
            store
                .insert_events(&[event_at(
                    &dev("old"),
                    3,
                    EventKind::ThrottleEnd,
                    "likely legacy",
                )])
                .unwrap();
        }

        let (store, _) = SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap();
        assert_eq!(store.data_source().unwrap(), Some(DataSource::Real));
        assert_eq!(
            store.events_between(0, 10).unwrap().len(),
            1,
            "adoption must not disturb the legacy contents"
        );
        drop(store);

        // From the stamp on, the other flavor is refused like any other mismatch.
        let err = SqliteStore::open_recording(&scratch.path, DataSource::Mock).unwrap_err();
        assert!(matches!(err, StoreError::DataSourceMismatch { .. }));
    }

    /// open_readonly lets a second connection read while the writer holds the db (WAL).
    #[test]
    fn readonly_reader_sees_committed_writes() {
        let scratch = Scratch::new();
        let (mut writer, _) = SqliteStore::open(&scratch.path).unwrap();
        writer
            .insert_events(&[Event {
                ts_ms: 7,
                device: dev("d"),
                kind: EventKind::VramPressure,
                severity: Severity::Warning,
                confidence: Confidence::Likely,
                title: "likely full soon".into(),
                evidence: "slope".into(),
            }])
            .unwrap();

        let reader = SqliteStore::open_readonly(&scratch.path).unwrap();
        assert_eq!(reader.events_between(0, 10).unwrap().len(), 1);
    }

    /// earliest/latest span both tiers.
    #[test]
    fn bucket_extremes_span_both_tiers() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        assert_eq!(store.earliest_bucket_ms().unwrap(), None);
        let d = dev("span");
        let mk = |bucket: u64| SampleRollup {
            device_id: d.clone(),
            bucket_ms: bucket,
            n: 1,
            util_min: Some(1.0),
            util_avg: Some(1.0),
            util_max: Some(1.0),
            mem_avg: None,
            mem_max: None,
            power_avg_mw: None,
            power_max_mw: None,
            temp_avg_c: None,
            temp_max_c: None,
            fan_max_pct: None,
            sm_clock_min: None,
            sm_clock_avg: None,
            sm_clock_max: None,
            throttle_n: 0,
            throttle_thermal_n: 0,
            throttle_power_n: 0,
            throttle_hw_n: 0,
        };
        store
            .insert_sample_rollups(Tier::TenSec, &[mk(50_000)])
            .unwrap();
        store
            .insert_sample_rollups(Tier::OneMin, &[mk(600_000)])
            .unwrap();
        assert_eq!(store.earliest_bucket_ms().unwrap(), Some(50_000));
        assert_eq!(store.latest_bucket_ms().unwrap(), Some(600_000));
    }

    // ===================================================================================
    // Export (.gpvr) + latest-event seek anchors.
    // ===================================================================================

    /// A minimal all-present 10s/1m rollup at `bucket` — the export tests only care about
    /// which buckets cross the window, not the metric values.
    fn rollup_at(d: &DeviceId, bucket: u64) -> SampleRollup {
        SampleRollup {
            device_id: d.clone(),
            bucket_ms: bucket,
            n: 1,
            util_min: Some(1.0),
            util_avg: Some(1.0),
            util_max: Some(1.0),
            mem_avg: None,
            mem_max: None,
            power_avg_mw: None,
            power_max_mw: None,
            temp_avg_c: None,
            temp_max_c: None,
            fan_max_pct: None,
            sm_clock_min: None,
            sm_clock_avg: None,
            sm_clock_max: None,
            throttle_n: 0,
            throttle_thermal_n: 0,
            throttle_power_n: 0,
            throttle_hw_n: 0,
        }
    }

    fn event_at(d: &DeviceId, ts_ms: u64, kind: EventKind, title: &str) -> Event {
        Event {
            ts_ms,
            device: d.clone(),
            kind,
            severity: Severity::Warning,
            confidence: Confidence::Likely,
            title: title.into(),
            evidence: "likely evidence".into(),
        }
    }

    /// export_to copies meta/devices whole and ONLY the window's sample/process/event rows
    /// into a fresh file. Decoy rows sit on both sides of the window so an unfiltered copy
    /// (or an off-by-one window) fails; the export then opens standalone — with the source
    /// file DELETED — and round-trips its events, confidence included.
    #[test]
    fn export_copies_only_the_window_into_a_standalone_file() {
        let scratch = Scratch::new();
        let out = Scratch::new(); // never created until export_to runs
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        let d = dev("0000:01:00.0");
        store
            .register_device(&d, "Exported GPU", Vendor::Amd, Some(16 << 30))
            .unwrap();

        // Window [60_000, 179_999]: two in-window rows per tier, decoys outside both edges.
        store
            .insert_sample_rollups(
                Tier::TenSec,
                &[
                    rollup_at(&d, 50_000),  // decoy: before the window
                    rollup_at(&d, 60_000),  // in (left edge)
                    rollup_at(&d, 170_000), // in
                    rollup_at(&d, 180_000), // decoy: past the window
                ],
            )
            .unwrap();
        store
            .insert_sample_rollups(
                Tier::OneMin,
                &[
                    rollup_at(&d, 0),       // decoy
                    rollup_at(&d, 60_000),  // in
                    rollup_at(&d, 120_000), // in
                    rollup_at(&d, 180_000), // decoy
                ],
            )
            .unwrap();
        let proc_at = |bucket: u64, name: &str| ProcessRollup {
            device_id: d.clone(),
            bucket_ms: bucket,
            pid: 9,
            name: name.into(),
            kind: ProcessKind::Compute,
            mem_max: Some(1 << 30),
            util_avg: Some(50.0),
            cpu_avg: Some(120.0),
            container: Some("docker:abc123".into()),
        };
        store
            .insert_process_rollups(&[proc_at(50_000, "decoy-proc"), proc_at(70_000, "kept-proc")])
            .unwrap();
        store
            .insert_events(&[
                event_at(&d, 59_999, EventKind::ThrottleStart, "decoy just before"),
                event_at(&d, 60_000, EventKind::ThrottleStart, "likely edge start"),
                event_at(&d, 179_999, EventKind::VramPressure, "likely edge end"),
                event_at(&d, 180_000, EventKind::ThrottleEnd, "decoy just after"),
            ])
            .unwrap();

        let counts = store.export_to(&out.path, 60_000, 179_999).unwrap();
        assert_eq!(counts.devices, 1);
        assert_eq!(counts.samples_10s, 2, "only the in-window 10s buckets copy");
        assert_eq!(counts.samples_1m, 2, "only the in-window 1m buckets copy");
        assert_eq!(counts.processes_10s, 1, "the decoy process bucket stays");
        assert_eq!(counts.events, 2, "edge rows in, decoys out");

        // Standalone proof: delete the source, then read the export on its own.
        drop(store);
        std::fs::remove_file(&scratch.path).unwrap();
        let exported = SqliteStore::open_readonly(&out.path).unwrap();
        let devs = exported.devices().unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].name, "Exported GPU");
        assert_eq!(devs[0].mem_total_bytes, Some(16 << 30));
        let tens = exported
            .samples_between(&d, 0, 1_000_000, Tier::TenSec)
            .unwrap();
        assert_eq!(
            tens.iter().map(|r| r.bucket_ms).collect::<Vec<_>>(),
            vec![60_000, 170_000]
        );
        let procs = exported.processes_at(&d, 75_000).unwrap();
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].name, "kept-proc");
        assert_eq!(procs[0].container.as_deref(), Some("docker:abc123"));
        let evs = exported.events_between(0, 1_000_000).unwrap();
        assert_eq!(evs.len(), 2);
        assert!(
            evs.iter().all(|e| !e.title.contains("decoy")),
            "no decoy may cross the window: {evs:?}"
        );
        // The honesty contract survives the copy: inferences stay Likely in the export.
        assert!(evs.iter().all(|e| e.confidence == Confidence::Likely));
    }

    /// An existing output file is never clobbered — and stays byte-identical after the
    /// refused attempt.
    #[test]
    fn export_refuses_to_overwrite_an_existing_file() {
        let scratch = Scratch::new();
        let out = Scratch::new();
        let (store, _) = SqliteStore::open(&scratch.path).unwrap();
        std::fs::write(&out.path, b"someone's shared incident file").unwrap();

        let err = store.export_to(&out.path, 0, 1_000).unwrap_err();
        assert!(
            matches!(err, StoreError::OutputExists(_)),
            "must refuse with OutputExists, got: {err}"
        );
        assert_eq!(
            std::fs::read(&out.path).unwrap(),
            b"someone's shared incident file",
            "the existing file must be untouched"
        );
    }

    // ===================================================================================
    // Instance lock — one recording instance per database file. The audit's
    // duplicate-narration blocker: two live instances folding frames into the SAME
    // history.db double-count every rollup bucket and insert every narrated event twice.
    // ===================================================================================

    /// While one handle holds the lock, every further WRITE open — `open_recording` (the
    /// stamped path) and plain `open` (the engine's `--db` path) alike — is refused with
    /// the distinct `Locked` error, whose message names the file and the escape hatches.
    #[test]
    fn second_recording_open_is_refused_while_lock_is_held() {
        let scratch = Scratch::new();
        let _held = SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap();

        let err = SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap_err();
        assert!(
            matches!(err, StoreError::Locked { .. }),
            "a second recording open must fail with Locked, got: {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("another gpuviewer instance is already recording"),
            "the refusal must say who has it: {msg}"
        );
        assert!(
            msg.contains(scratch.path.to_str().unwrap()),
            "the refusal must name the file: {msg}"
        );
        assert!(
            msg.contains("--no-persist"),
            "the refusal must offer the live-only escape hatch: {msg}"
        );

        let err = SqliteStore::open(&scratch.path).unwrap_err();
        assert!(
            matches!(err, StoreError::Locked { .. }),
            "the plain write open must be refused identically, got: {err}"
        );
    }

    /// Losing the lock race must never harm the database: no quarantine (`Locked` is
    /// "busy", not "corrupt"), no data loss, and the holder keeps writing afterwards.
    #[test]
    fn losing_the_lock_race_never_quarantines_the_database() {
        let scratch = Scratch::new();
        let (mut held, _) = SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap();
        held.insert_events(&[event_at(&dev("g"), 5, EventKind::ThrottleStart, "kept")])
            .unwrap();

        assert!(SqliteStore::open(&scratch.path).is_err());

        // No *.corrupt-* sibling may appear: the loser must return before the corruption
        // machinery, or it would rename a healthy, busy database out from under the holder.
        let dir = scratch.path.parent().unwrap();
        let stem = scratch.path.file_name().unwrap().to_str().unwrap();
        let quarantined = std::fs::read_dir(dir).unwrap().flatten().any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&format!("{stem}.corrupt-")))
        });
        assert!(
            !quarantined,
            "a busy database must never be quarantined as corrupt"
        );

        // The holder is unharmed: old rows intact, new writes still land.
        held.insert_events(&[event_at(
            &dev("g"),
            6,
            EventKind::ThrottleEnd,
            "still writing",
        )])
        .unwrap();
        assert_eq!(held.events_between(0, 10).unwrap().len(), 2);
    }

    /// Read paths run concurrently with a recording instance: `report`/`view`/replay all
    /// open read-only, which takes no lock (only co-writers double-record).
    #[test]
    fn readonly_open_succeeds_alongside_a_held_lock() {
        let scratch = Scratch::new();
        let (mut held, _) = SqliteStore::open_recording(&scratch.path, DataSource::Mock).unwrap();
        held.insert_events(&[event_at(
            &dev("sim"),
            7,
            EventKind::VramPressure,
            "likely visible to readers",
        )])
        .unwrap();

        let reader = SqliteStore::open_readonly(&scratch.path).unwrap();
        assert_eq!(
            reader.events_between(0, 10).unwrap().len(),
            1,
            "a reader must work while the recording lock is held"
        );
        // And a second reader too — readers never exclude each other.
        let reader2 = SqliteStore::open_readonly(&scratch.path).unwrap();
        assert_eq!(reader2.events_between(0, 10).unwrap().len(), 1);
    }

    /// Dropping the holding store releases the lock (the `File` closes with it), so the
    /// next instance records normally — crash-release is the same mechanism, exercised by
    /// the kernel instead of Drop. The stale `.lock` sidecar left on disk must not matter.
    #[test]
    fn lock_released_on_drop_lets_the_next_instance_record() {
        let scratch = Scratch::new();
        drop(SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap());

        // The sidecar file still exists (release is the kernel lock dying, not the file
        // being deleted) — and relocking it must succeed regardless.
        let mut lock_file = scratch.path.as_os_str().to_os_string();
        lock_file.push(".lock");
        assert!(
            std::path::Path::new(&lock_file).exists(),
            "the sidecar persists; only the kernel lock is released"
        );

        let (mut store, was_reset) =
            SqliteStore::open_recording(&scratch.path, DataSource::Real).unwrap();
        assert!(!was_reset, "relocking must not look like a reset");
        store
            .insert_events(&[event_at(&dev("g"), 1, EventKind::ProcessExited, "next run")])
            .unwrap();
    }

    // ===================================================================================
    // Event dedupe — defense in depth behind the instance lock: the log itself refuses
    // the same narration twice, so even a pre-lock binary racing a new one (or a future
    // double-feed bug) cannot produce "GPU0 began throttling" twice at the same second.
    // ===================================================================================

    /// The same event inserted twice — across calls or within one batch — lands once.
    /// A genuinely different event at the same instant (other device, other title) is NOT
    /// collapsed: the key is (ts, device, kind, title), not just the timestamp.
    #[test]
    fn same_event_inserted_twice_lands_once() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        let e = event_at(
            &dev("gpu0"),
            5_000,
            EventKind::ThrottleStart,
            "began throttling",
        );

        store.insert_events(std::slice::from_ref(&e)).unwrap();
        // A second writer replaying the tick, then a duplicate within one batch.
        store.insert_events(std::slice::from_ref(&e)).unwrap();
        store.insert_events(&[e.clone(), e.clone()]).unwrap();
        assert_eq!(
            store.events_between(0, 10_000).unwrap().len(),
            1,
            "one narration, no matter how many times it is fed"
        );

        // Same instant, different device / different title: distinct narrations, all kept.
        store
            .insert_events(&[
                event_at(
                    &dev("gpu1"),
                    5_000,
                    EventKind::ThrottleStart,
                    "began throttling",
                ),
                event_at(
                    &dev("gpu0"),
                    5_000,
                    EventKind::ThrottleStart,
                    "another story",
                ),
            ])
            .unwrap();
        assert_eq!(
            store.events_between(0, 10_000).unwrap().len(),
            3,
            "the dedupe key must not over-collapse distinct events"
        );
    }

    /// Migration: a pre-v2 database (built by hand exactly as two double-recording
    /// pre-lock binaries left it — v1 events table, no dedupe index, duplicate rows) opens
    /// cleanly, with duplicates collapsed to the FIRST insertion and the constraint live
    /// from then on. No reset, no loss of distinct rows.
    #[test]
    fn migration_collapses_preexisting_duplicate_events() {
        let scratch = Scratch::new();
        {
            // The v1 shape verbatim: same table, only idx_events_ts, user_version 1.
            let conn = rusqlite::Connection::open(&scratch.path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (
                     id         INTEGER PRIMARY KEY AUTOINCREMENT,
                     ts_ms      INTEGER NOT NULL,
                     device_id  TEXT NOT NULL,
                     kind       TEXT NOT NULL,
                     severity   TEXT NOT NULL,
                     confidence TEXT NOT NULL,
                     title      TEXT NOT NULL,
                     evidence   TEXT NOT NULL
                 );
                 CREATE INDEX idx_events_ts ON events (ts_ms);
                 PRAGMA user_version = 1;
                 -- Three copies of one narration (double-recorded, then some), with
                 -- evidence differing so the keep-MIN(id) rule is observable; one
                 -- distinct event that must survive untouched.
                 INSERT INTO events (ts_ms, device_id, kind, severity, confidence, title, evidence)
                 VALUES
                   (5000, 'gpu0', 'throttle_start', 'warning', 'fact', 'GPU0 began throttling', 'original'),
                   (5000, 'gpu0', 'throttle_start', 'warning', 'fact', 'GPU0 began throttling', 'echo one'),
                   (5000, 'gpu0', 'throttle_start', 'warning', 'fact', 'GPU0 began throttling', 'echo two'),
                   (6000, 'gpu0', 'throttle_end',   'info',    'fact', 'GPU0 stopped throttling', 'distinct');",
            )
            .unwrap();
        }

        let (mut store, was_reset) = SqliteStore::open(&scratch.path).unwrap();
        assert!(
            !was_reset,
            "a healthy pre-v2 database migrates in place, never resets"
        );

        let evs = store.events_between(0, 10_000).unwrap();
        assert_eq!(evs.len(), 2, "3 copies -> 1, plus the distinct event");
        assert_eq!(
            evs[0].evidence, "original",
            "MIN(id) — the FIRST insertion — must be the survivor"
        );
        assert_eq!(evs[1].title, "GPU0 stopped throttling");

        // The constraint is live from the migration on: re-feeding the duplicate is a no-op.
        store
            .insert_events(&[Event {
                ts_ms: 5_000,
                device: dev("gpu0"),
                kind: EventKind::ThrottleStart,
                severity: Severity::Warning,
                confidence: Confidence::Fact,
                title: "GPU0 began throttling".into(),
                evidence: "post-migration echo".into(),
            }])
            .unwrap();
        assert_eq!(store.events_between(0, 10_000).unwrap().len(), 2);
    }

    /// Reopening an already-migrated database must not rerun the collapse (the probe on
    /// the index keeps every later open scan-free) — and must keep the constraint.
    #[test]
    fn migration_is_idempotent_across_reopens() {
        let scratch = Scratch::new();
        let e = event_at(&dev("g"), 1_000, EventKind::IdleGap, "likely a stall");
        {
            let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
            store.insert_events(std::slice::from_ref(&e)).unwrap();
        }
        let (mut store, was_reset) = SqliteStore::open(&scratch.path).unwrap();
        assert!(!was_reset);
        store.insert_events(std::slice::from_ref(&e)).unwrap();
        assert_eq!(store.events_between(0, 2_000).unwrap().len(), 1);
    }

    /// latest_event_ms: overall max, per-kind max, and None for an absent kind — the demo
    /// must find the last ThrottleStart, not just the last anything.
    #[test]
    fn latest_event_ms_overall_and_by_kind() {
        let scratch = Scratch::new();
        let (mut store, _) = SqliteStore::open(&scratch.path).unwrap();
        let d = dev("anchors");
        assert_eq!(store.latest_event_ms(None).unwrap(), None);

        store
            .insert_events(&[
                event_at(&d, 5_000, EventKind::ThrottleStart, "likely a"),
                event_at(&d, 9_000, EventKind::ProcessExited, "likely b"),
                event_at(&d, 7_000, EventKind::ThrottleStart, "likely c"),
            ])
            .unwrap();
        assert_eq!(store.latest_event_ms(None).unwrap(), Some(9_000));
        assert_eq!(
            store
                .latest_event_ms(Some(EventKind::ThrottleStart))
                .unwrap(),
            Some(7_000),
            "per-kind max must ignore newer events of other kinds"
        );
        assert_eq!(
            store
                .latest_event_ms(Some(EventKind::VramPressure))
                .unwrap(),
            None
        );
    }
}
