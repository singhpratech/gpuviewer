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
    DeviceRow, ProcessRollup, SampleRollup, SqliteStore, StoreError, Tier, RETAIN_10S_MS,
    RETAIN_1M_MS, RETAIN_EVENTS_MS, SCHEMA_VERSION,
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
        let (any, thermal, power_cap, hw) = store::throttle_flags(&s.throttle);
        self.throttle_n += any as u32;
        self.throttle_thermal_n += thermal as u32;
        self.throttle_power_n += power_cap as u32;
        self.throttle_hw_n += hw as u32;
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
            for ext in ["-wal", "-shm"] {
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
    fn full_sample(ts_ms: u64, util: f32, mem: u64, throttle: ThrottleReasons) -> DynamicSample {
        DynamicSample {
            ts_ms,
            util_pct: Some(util),
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

    /// Throttle counters tally per reason across the bucket's frames.
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
        rec.observe(&d, &full_sample(1_000, 90.0, 1 << 30, thermal), &[]);
        rec.observe(&d, &full_sample(2_000, 90.0, 1 << 30, thermal), &[]);
        rec.observe(&d, &full_sample(3_000, 90.0, 1 << 30, hw), &[]);
        rec.observe(
            &d,
            &full_sample(4_000, 90.0, 1 << 30, Default::default()),
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

    /// open_default selects a separate file for mock so simulated data can never contaminate
    /// real history. We only assert the filename, never touching the real history.db.
    #[test]
    fn mock_default_path_is_separate_from_real() {
        // Point XDG at a scratch dir so the test never opens the user's real history.
        let scratch_dir = std::env::temp_dir().join(format!(
            "gpuviewer-xdg-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let prev = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: single-threaded test; restored before returning.
        unsafe { std::env::set_var("XDG_DATA_HOME", &scratch_dir) };

        let (mock_store, _) = SqliteStore::open_default(true).unwrap();
        let (real_store, _) = SqliteStore::open_default(false).unwrap();
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
        // Both must live under our scratch XDG dir (gpuviewer subdir), never the user's home.
        assert!(mock_store.path().starts_with(&scratch_dir));

        drop(mock_store);
        drop(real_store);
        // SAFETY: single-threaded test.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&scratch_dir);
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
}
