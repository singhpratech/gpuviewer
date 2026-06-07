//! The SQLite tier of the flight recorder — the wedge feature: persistent, replayable
//! history. RAM rings (see `lib.rs`) cover the live window; this layer keeps the long tail
//! so a user can scroll back to "02:14 last Tuesday" and see why a run stalled.
//!
//! Storage shape follows the CLAUDE.md decisions verbatim:
//! - **Never** the raw 1 Hz stream — only 10s and 1m downsampled rollups (`samples_10s`,
//!   `samples_1m`) plus an append-only `events` log. The `Recorder` (lib.rs) does the
//!   folding; this module is pure persistence.
//! - One timestamp per collection frame, carried as the bucket key.
//! - Nullable metric columns hold `NULL` when the source `Option` was `None`. A `0` would
//!   read as a real measurement and quietly lie on the replay chart — never do that.
//! - WAL mode so the TUI's replay view can open a second read-only connection
//!   ([`SqliteStore::open_readonly`]) while the collector keeps writing.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpuviewer_core::{
    Confidence, DeviceId, Event, EventKind, ProcessKind, Severity, ThrottleReasons, Vendor,
};
use rusqlite::{params, Connection, OpenFlags};

/// Schema version stamped into `PRAGMA user_version` and `meta.schema_version`. Bump when
/// the table shape changes so a future migration step can branch on it.
pub const SCHEMA_VERSION: u32 = 1;

/// Retention windows. Public because the UI tells the user how far back history reaches
/// ("10s detail for 48h, 1m for 30 days").
pub const RETAIN_10S_MS: u64 = 48 * 60 * 60 * 1000;
pub const RETAIN_1M_MS: u64 = 30 * 24 * 60 * 60 * 1000;
pub const RETAIN_EVENTS_MS: u64 = RETAIN_1M_MS;

/// Which rollup tier a read/write targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// 10-second buckets — fine detail for the recent past (`RETAIN_10S_MS`).
    TenSec,
    /// 1-minute buckets — the long tail (`RETAIN_1M_MS`).
    OneMin,
}

impl Tier {
    /// Bucket width in millis. The `Recorder` uses these to align frames onto bucket keys.
    pub const fn width_ms(self) -> u64 {
        match self {
            Tier::TenSec => 10_000,
            Tier::OneMin => 60_000,
        }
    }

    fn samples_table(self) -> &'static str {
        match self {
            Tier::TenSec => "samples_10s",
            Tier::OneMin => "samples_1m",
        }
    }
}

/// A downsampled sample bucket for one device. Min/avg/max are over the raw frames that fell
/// in the bucket; every metric is `Option` because any of them can be absent on real
/// hardware, and `n` records how many raw frames the aggregate is built from.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleRollup {
    pub device_id: DeviceId,
    /// Bucket start in unix millis (`ts_ms - ts_ms % width`).
    pub bucket_ms: u64,
    /// Raw frames folded into this bucket.
    pub n: u32,
    pub util_min: Option<f32>,
    pub util_avg: Option<f32>,
    pub util_max: Option<f32>,
    pub mem_avg: Option<u64>,
    pub mem_max: Option<u64>,
    pub power_avg_mw: Option<u32>,
    pub power_max_mw: Option<u32>,
    pub temp_avg_c: Option<f32>,
    pub temp_max_c: Option<f32>,
    pub fan_max_pct: Option<f32>,
    pub sm_clock_min: Option<u32>,
    pub sm_clock_avg: Option<u32>,
    pub sm_clock_max: Option<u32>,
    /// Frames in this bucket where any throttle reason was active.
    pub throttle_n: u32,
    pub throttle_thermal_n: u32,
    pub throttle_power_n: u32,
    pub throttle_hw_n: u32,
}

/// A downsampled per-process bucket. Keyed by (device, bucket, pid); a process spanning
/// several buckets produces one row per bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRollup {
    pub device_id: DeviceId,
    pub bucket_ms: u64,
    pub pid: u32,
    pub name: String,
    pub kind: ProcessKind,
    pub mem_max: Option<u64>,
    pub util_avg: Option<f32>,
    pub cpu_avg: Option<f32>,
    pub container: Option<String>,
}

/// A device's static identity as persisted in `devices`. Returned by [`SqliteStore::devices`]
/// so a replay session can label history even for a GPU no longer present.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceRow {
    pub device_id: DeviceId,
    pub name: String,
    pub vendor: Vendor,
    pub mem_total_bytes: Option<u64>,
}

/// Persistence failures. Per-metric absence is never one of these (it is `NULL` in the row);
/// an error here means the database itself is unusable.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying SQLite/IO failure.
    Sqlite(rusqlite::Error),
    /// Could not resolve a data directory for `open_default`.
    NoDataDir,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::NoDataDir => {
                write!(f, "no data directory (set $XDG_DATA_HOME or $HOME)")
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// The writer connection plus its file path. The `Recorder` owns one of these; the replay
/// view opens its own read-only connection via [`SqliteStore::open_readonly`].
pub struct SqliteStore {
    conn: Connection,
    path: PathBuf,
}

impl SqliteStore {
    /// Open (creating if absent) the history database at `path`.
    ///
    /// Corruption-tolerant: an existing file is `PRAGMA quick_check`-ed first; if that fails,
    /// or if opening/initializing it errors, the file is renamed aside to
    /// `<path>.corrupt-<unix_seconds>` (never deleted — a user may want to recover it) and a
    /// fresh database is created. The returned `bool` is `was_reset`: `true` iff such a
    /// recovery happened, so the caller can emit a `HistoryReset` event and stop the gap
    /// masquerading as device behavior.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, bool), StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|_| StoreError::NoDataDir)?;
            }
        }

        let existed = path.exists();
        match Self::try_open_init(&path) {
            Ok(store) if !existed => Ok((store, false)),
            Ok(store) if store.quick_check_ok() => Ok((store, false)),
            // Open succeeded but the integrity check failed, or open/init itself failed:
            // either way the existing file is unusable. Rename it aside and start fresh.
            _ => {
                Self::quarantine(&path)?;
                let store = Self::try_open_init(&path)?;
                Ok((store, true))
            }
        }
    }

    /// Resolve the default history path and open it. `mock=true` selects a SEPARATE file
    /// (`history-mock.db`) so simulated runs can NEVER contaminate the real recording —
    /// the demo and CI must not pollute a user's flight history.
    pub fn open_default(mock: bool) -> Result<(Self, bool), StoreError> {
        let dir = default_data_dir()?;
        let file = if mock {
            "history-mock.db"
        } else {
            "history.db"
        };
        Self::open(dir.join(file))
    }

    /// A second, read-only connection to an existing database. WAL mode lets this reader run
    /// concurrently with the writer connection — the TUI replay view uses it while the
    /// collector keeps appending. Does not create or modify the file.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(Self { conn, path })
    }

    /// The on-disk path of this store's database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ---- open helpers ----

    fn try_open_init(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn,
            path: path.to_path_buf(),
        };
        store.init_pragmas()?;
        store.init_schema()?;
        Ok(store)
    }

    fn init_pragmas(&self) -> Result<(), StoreError> {
        // WAL for concurrent reader; NORMAL is the WAL-recommended durability/speed balance
        // (a crash can lose the last transaction, never corrupt the file). busy_timeout so a
        // writer waiting on the reader's checkpoint blocks briefly instead of erroring.
        self.conn.pragma_update(None, "journal_mode", "WAL")?;
        self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(())
    }

    fn quick_check_ok(&self) -> bool {
        // quick_check is the cheap integrity probe (skips the full index cross-check); a
        // healthy database returns the single row "ok".
        self.conn
            .query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .map(|s| s == "ok")
            .unwrap_or(false)
    }

    /// Move a corrupt/unreadable file aside, preserving it for manual recovery. Best-effort
    /// on the sidecar WAL/SHM files (they are regenerated on the fresh open).
    fn quarantine(path: &Path) -> Result<(), StoreError> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut corrupt = path.as_os_str().to_os_string();
        corrupt.push(format!(".corrupt-{secs}"));
        std::fs::rename(path, &corrupt).map_err(|_| StoreError::NoDataDir)?;
        for ext in ["-wal", "-shm"] {
            let mut side = path.as_os_str().to_os_string();
            side.push(ext);
            let _ = std::fs::remove_file(side);
        }
        Ok(())
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(SCHEMA_SQL)?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let now = now_ms_wall();
        // Stamp metadata only on a fresh database; `INSERT OR IGNORE` keeps `created_ms`
        // pinned to first creation across reopens.
        self.conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('created_ms', ?1)",
            params![now.to_string()],
        )?;
        Ok(())
    }

    // ---- writes (each batch is one transaction) ----

    /// Upsert a device's static identity. Re-registration refreshes name/vendor/mem.
    pub fn register_device(
        &self,
        device_id: &DeviceId,
        name: &str,
        vendor: Vendor,
        mem_total_bytes: Option<u64>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO devices(device_id, name, vendor, mem_total_bytes) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_id) DO UPDATE SET name=?2, vendor=?3, mem_total_bytes=?4",
            params![
                device_id.0,
                name,
                vendor_str(vendor),
                mem_total_bytes.map(|v| v as i64)
            ],
        )?;
        Ok(())
    }

    /// Batch-insert sample rollups into the given tier (one transaction). Re-inserting a
    /// bucket replaces it — a forced partial-bucket flush followed by the real flush of the
    /// same completed bucket must not duplicate the row.
    pub fn insert_sample_rollups(
        &mut self,
        tier: Tier,
        rows: &[SampleRollup],
    ) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let sql = format!(
            "INSERT OR REPLACE INTO {} (device_id, bucket_ms, n, util_min, util_avg, util_max, \
             mem_avg, mem_max, power_avg_mw, power_max_mw, temp_avg_c, temp_max_c, fan_max_pct, \
             sm_clock_min, sm_clock_avg, sm_clock_max, throttle_n, throttle_thermal_n, \
             throttle_power_n, throttle_hw_n) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            tier.samples_table()
        );
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(&sql)?;
            for r in rows {
                stmt.execute(params![
                    r.device_id.0,
                    r.bucket_ms as i64,
                    r.n as i64,
                    r.util_min,
                    r.util_avg,
                    r.util_max,
                    r.mem_avg.map(|v| v as i64),
                    r.mem_max.map(|v| v as i64),
                    r.power_avg_mw.map(|v| v as i64),
                    r.power_max_mw.map(|v| v as i64),
                    r.temp_avg_c,
                    r.temp_max_c,
                    r.fan_max_pct,
                    r.sm_clock_min.map(|v| v as i64),
                    r.sm_clock_avg.map(|v| v as i64),
                    r.sm_clock_max.map(|v| v as i64),
                    r.throttle_n as i64,
                    r.throttle_thermal_n as i64,
                    r.throttle_power_n as i64,
                    r.throttle_hw_n as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Batch-insert process rollups (one transaction). 10s tier only — the long tail does not
    /// keep per-process detail.
    pub fn insert_process_rollups(&mut self, rows: &[ProcessRollup]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO processes_10s (device_id, bucket_ms, pid, name, kind, \
                 mem_max, util_avg, cpu_avg, container) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.device_id.0,
                    r.bucket_ms as i64,
                    r.pid as i64,
                    r.name,
                    kind_str(r.kind),
                    r.mem_max.map(|v| v as i64),
                    r.util_avg,
                    r.cpu_avg,
                    r.container,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Append events (one transaction). The `evidence` and reconstruction fields are stored
    /// as serde strings so [`SqliteStore::events_between`] can rebuild the exact `Event`.
    pub fn insert_events(&mut self, events: &[Event]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO events (ts_ms, device_id, kind, severity, confidence, title, \
                 evidence) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for e in events {
                stmt.execute(params![
                    e.ts_ms as i64,
                    e.device.0,
                    kind_to_str(e.kind),
                    severity_to_str(e.severity),
                    confidence_to_str(e.confidence),
                    e.title,
                    e.evidence,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- reads ----

    /// Sample rollups for one device in `[from_ms, to_ms]` at the given tier, oldest first.
    pub fn samples_between(
        &self,
        device: &DeviceId,
        from_ms: u64,
        to_ms: u64,
        tier: Tier,
    ) -> Result<Vec<SampleRollup>, StoreError> {
        let sql = format!(
            "SELECT device_id, bucket_ms, n, util_min, util_avg, util_max, mem_avg, mem_max, \
             power_avg_mw, power_max_mw, temp_avg_c, temp_max_c, fan_max_pct, sm_clock_min, \
             sm_clock_avg, sm_clock_max, throttle_n, throttle_thermal_n, throttle_power_n, \
             throttle_hw_n FROM {} WHERE device_id = ?1 AND bucket_ms >= ?2 AND bucket_ms <= ?3 \
             ORDER BY bucket_ms ASC",
            tier.samples_table()
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![device.0, from_ms as i64, to_ms as i64],
                sample_rollup_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Events in `[from_ms, to_ms]`, oldest first. The stored kind/severity/confidence
    /// strings are parsed back through serde so the reconstructed `Event` is identical to the
    /// one inserted (a corrupt/unknown enum string drops that one row rather than failing the
    /// whole query — old recordings must still replay).
    pub fn events_between(&self, from_ms: u64, to_ms: u64) -> Result<Vec<Event>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT ts_ms, device_id, kind, severity, confidence, title, evidence FROM events \
             WHERE ts_ms >= ?1 AND ts_ms <= ?2 ORDER BY ts_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![from_ms as i64, to_ms as i64], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (ts_ms, device, kind, severity, confidence, title, evidence) = row?;
            let (Some(kind), Some(severity), Some(confidence)) = (
                kind_from_str(&kind),
                severity_from_str(&severity),
                confidence_from_str(&confidence),
            ) else {
                continue;
            };
            out.push(Event {
                ts_ms,
                device: DeviceId(device),
                kind,
                severity,
                confidence,
                title,
                evidence,
            });
        }
        Ok(out)
    }

    /// Per-process rollups for the 10s bucket covering `at_ms`. If that exact bucket is empty,
    /// fall back to the nearest earlier bucket within 60s (a replay cursor between buckets
    /// should still show the processes that were attached just before it).
    pub fn processes_at(
        &self,
        device: &DeviceId,
        at_ms: u64,
    ) -> Result<Vec<ProcessRollup>, StoreError> {
        let bucket = at_ms - at_ms % Tier::TenSec.width_ms();
        // The newest bucket at or before `bucket` but no older than 60s — one query so we
        // pick the right bucket even when intervening buckets are missing.
        let floor = bucket.saturating_sub(60_000);
        let chosen: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(bucket_ms) FROM processes_10s WHERE device_id = ?1 \
                 AND bucket_ms <= ?2 AND bucket_ms >= ?3",
                params![device.0, bucket as i64, floor as i64],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let Some(chosen) = chosen else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT device_id, bucket_ms, pid, name, kind, mem_max, util_avg, cpu_avg, container \
             FROM processes_10s WHERE device_id = ?1 AND bucket_ms = ?2 ORDER BY pid ASC",
        )?;
        let rows = stmt
            .query_map(params![device.0, chosen], process_rollup_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All registered devices, ordered by id for stable UI.
    pub fn devices(&self) -> Result<Vec<DeviceRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT device_id, name, vendor, mem_total_bytes FROM devices ORDER BY device_id ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DeviceRow {
                    device_id: DeviceId(row.get(0)?),
                    name: row.get(1)?,
                    vendor: vendor_from_str(&row.get::<_, String>(2)?),
                    mem_total_bytes: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Oldest bucket across both sample tiers, or `None` if no samples are stored. Defines the
    /// left edge of the replay timeline.
    pub fn earliest_bucket_ms(&self) -> Result<Option<u64>, StoreError> {
        self.bucket_extreme("MIN")
    }

    /// Newest bucket across both sample tiers, or `None` if empty.
    pub fn latest_bucket_ms(&self) -> Result<Option<u64>, StoreError> {
        self.bucket_extreme("MAX")
    }

    fn bucket_extreme(&self, agg: &str) -> Result<Option<u64>, StoreError> {
        // Combine the per-tier extreme; MIN-of-MINs / MAX-of-MAXs is correct for both aggs.
        let sql = format!(
            "SELECT {agg}(b) FROM (SELECT {agg}(bucket_ms) AS b FROM samples_10s \
             UNION ALL SELECT {agg}(bucket_ms) AS b FROM samples_1m)"
        );
        let v: Option<i64> = self.conn.query_row(&sql, [], |r| r.get(0))?;
        Ok(v.map(|v| v as u64))
    }

    /// Drop history past its retention window: 10s samples and per-process rows older than
    /// `RETAIN_10S_MS`, 1m samples and events older than their longer windows. `now_ms` is
    /// passed in (not read from the clock) so the caller can prune relative to the newest
    /// sample it has seen — and so tests are deterministic.
    pub fn prune(&mut self, now_ms: u64) -> Result<(), StoreError> {
        let cut_10s = now_ms.saturating_sub(RETAIN_10S_MS) as i64;
        let cut_1m = now_ms.saturating_sub(RETAIN_1M_MS) as i64;
        let cut_events = now_ms.saturating_sub(RETAIN_EVENTS_MS) as i64;
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM samples_10s WHERE bucket_ms < ?1",
            params![cut_10s],
        )?;
        tx.execute(
            "DELETE FROM processes_10s WHERE bucket_ms < ?1",
            params![cut_10s],
        )?;
        tx.execute(
            "DELETE FROM samples_1m WHERE bucket_ms < ?1",
            params![cut_1m],
        )?;
        tx.execute("DELETE FROM events WHERE ts_ms < ?1", params![cut_events])?;
        tx.commit()?;
        Ok(())
    }
}

// ---- row decoders ----

fn sample_rollup_from_row(row: &rusqlite::Row) -> rusqlite::Result<SampleRollup> {
    Ok(SampleRollup {
        device_id: DeviceId(row.get(0)?),
        bucket_ms: row.get::<_, i64>(1)? as u64,
        n: row.get::<_, i64>(2)? as u32,
        util_min: row.get(3)?,
        util_avg: row.get(4)?,
        util_max: row.get(5)?,
        mem_avg: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        mem_max: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        power_avg_mw: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        power_max_mw: row.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        temp_avg_c: row.get(10)?,
        temp_max_c: row.get(11)?,
        fan_max_pct: row.get(12)?,
        sm_clock_min: row.get::<_, Option<i64>>(13)?.map(|v| v as u32),
        sm_clock_avg: row.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        sm_clock_max: row.get::<_, Option<i64>>(15)?.map(|v| v as u32),
        throttle_n: row.get::<_, i64>(16)? as u32,
        throttle_thermal_n: row.get::<_, i64>(17)? as u32,
        throttle_power_n: row.get::<_, i64>(18)? as u32,
        throttle_hw_n: row.get::<_, i64>(19)? as u32,
    })
}

fn process_rollup_from_row(row: &rusqlite::Row) -> rusqlite::Result<ProcessRollup> {
    Ok(ProcessRollup {
        device_id: DeviceId(row.get(0)?),
        bucket_ms: row.get::<_, i64>(1)? as u64,
        pid: row.get::<_, i64>(2)? as u32,
        name: row.get(3)?,
        kind: kind_from_proc_str(&row.get::<_, String>(4)?),
        mem_max: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        util_avg: row.get(6)?,
        cpu_avg: row.get(7)?,
        container: row.get(8)?,
    })
}

// ---- enum <-> string helpers ----
//
// Events round-trip through serde (the NDJSON contract is the source of truth for those
// spellings, so the stored strings match the wire format). Vendor/ProcessKind use the same
// serde spellings for consistency.

fn serde_str<T: serde::Serialize>(v: &T) -> String {
    // These enums serialize to a bare JSON string (`"lowercase"`); strip the quotes so the
    // column holds the plain token. Infallible for these `#[serde(rename_all)]` C-like enums.
    serde_json::to_string(v)
        .ok()
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default()
}

fn serde_parse<T: for<'de> serde::Deserialize<'de>>(s: &str) -> Option<T> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

fn kind_to_str(k: EventKind) -> String {
    serde_str(&k)
}
fn kind_from_str(s: &str) -> Option<EventKind> {
    serde_parse(s)
}
fn severity_to_str(s: Severity) -> String {
    serde_str(&s)
}
fn severity_from_str(s: &str) -> Option<Severity> {
    serde_parse(s)
}
fn confidence_to_str(c: Confidence) -> String {
    serde_str(&c)
}
fn confidence_from_str(s: &str) -> Option<Confidence> {
    serde_parse(s)
}
fn vendor_str(v: Vendor) -> String {
    serde_str(&v)
}
fn vendor_from_str(s: &str) -> Vendor {
    serde_parse(s).unwrap_or(Vendor::Unknown)
}
fn kind_str(k: ProcessKind) -> String {
    serde_str(&k)
}
fn kind_from_proc_str(s: &str) -> ProcessKind {
    serde_parse(s).unwrap_or(ProcessKind::Unknown)
}

/// Count active throttle reasons across an aggregate — exposed for the `Recorder` so the
/// per-reason bucket counters live in one place.
pub fn throttle_flags(t: &ThrottleReasons) -> (bool, bool, bool, bool) {
    (t.any(), t.thermal, t.power_cap, t.hw_slowdown)
}

fn now_ms_wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `$XDG_DATA_HOME/gpuviewer` or `~/.local/share/gpuviewer`, creating nothing here (the
/// caller's `open` makes the dir). Errors only when neither variable is set.
fn default_data_dir() -> Result<PathBuf, StoreError> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("gpuviewer"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join(".local/share/gpuviewer"));
        }
    }
    Err(StoreError::NoDataDir)
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
CREATE TABLE IF NOT EXISTS devices (
    device_id       TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    vendor          TEXT NOT NULL,
    mem_total_bytes INTEGER
);
CREATE TABLE IF NOT EXISTS samples_10s (
    device_id          TEXT NOT NULL,
    bucket_ms          INTEGER NOT NULL,
    n                  INTEGER NOT NULL,
    util_min           REAL,
    util_avg           REAL,
    util_max           REAL,
    mem_avg            INTEGER,
    mem_max            INTEGER,
    power_avg_mw       INTEGER,
    power_max_mw       INTEGER,
    temp_avg_c         REAL,
    temp_max_c         REAL,
    fan_max_pct        REAL,
    sm_clock_min       INTEGER,
    sm_clock_avg       INTEGER,
    sm_clock_max       INTEGER,
    throttle_n         INTEGER NOT NULL DEFAULT 0,
    throttle_thermal_n INTEGER NOT NULL DEFAULT 0,
    throttle_power_n   INTEGER NOT NULL DEFAULT 0,
    throttle_hw_n      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_id, bucket_ms)
);
CREATE TABLE IF NOT EXISTS samples_1m (
    device_id          TEXT NOT NULL,
    bucket_ms          INTEGER NOT NULL,
    n                  INTEGER NOT NULL,
    util_min           REAL,
    util_avg           REAL,
    util_max           REAL,
    mem_avg            INTEGER,
    mem_max            INTEGER,
    power_avg_mw       INTEGER,
    power_max_mw       INTEGER,
    temp_avg_c         REAL,
    temp_max_c         REAL,
    fan_max_pct        REAL,
    sm_clock_min       INTEGER,
    sm_clock_avg       INTEGER,
    sm_clock_max       INTEGER,
    throttle_n         INTEGER NOT NULL DEFAULT 0,
    throttle_thermal_n INTEGER NOT NULL DEFAULT 0,
    throttle_power_n   INTEGER NOT NULL DEFAULT 0,
    throttle_hw_n      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (device_id, bucket_ms)
);
CREATE TABLE IF NOT EXISTS processes_10s (
    device_id TEXT NOT NULL,
    bucket_ms INTEGER NOT NULL,
    pid       INTEGER NOT NULL,
    name      TEXT NOT NULL,
    kind      TEXT NOT NULL,
    mem_max   INTEGER,
    util_avg  REAL,
    cpu_avg   REAL,
    container TEXT,
    PRIMARY KEY (device_id, bucket_ms, pid)
);
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms      INTEGER NOT NULL,
    device_id  TEXT NOT NULL,
    kind       TEXT NOT NULL,
    severity   TEXT NOT NULL,
    confidence TEXT NOT NULL,
    title      TEXT NOT NULL,
    evidence   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_ts ON events (ts_ms);
";
