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
//! - A `meta.data_source` stamp ("real"/"mock") guards every RECORDING open
//!   ([`SqliteStore::open_recording`]): simulated sessions can never write into a real
//!   recording, nor real sessions into a mock one. Reads ignore the stamp — replaying
//!   mock history is fine (the UI labels it), only co-mingled writes are not.
//! - An exclusive **instance lock** (`<db>.lock` sidecar, kernel advisory lock) held by
//!   every write handle for its lifetime: two live gpuviewer instances folding frames into
//!   the same file would double-count every rollup bucket and insert every narrated event
//!   twice — the audit's duplicate-narration blocker. The lock loser stays usable
//!   (live-only); read-only opens never take the lock, so `report`/`view`/replay run
//!   concurrently with a recording instance.
//! - A UNIQUE index over `(ts_ms, device_id, kind, title)` on the event log with
//!   `INSERT OR IGNORE` — defense in depth behind the lock: even a pre-lock binary running
//!   alongside a new one cannot land the same narration twice.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpuviewer_core::{
    Confidence, DeviceId, Event, EventKind, ProcessKind, Severity, ThrottleReasons, Vendor,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

/// Schema version stamped into `PRAGMA user_version` and `meta.schema_version`. Bump when
/// the table shape changes so a future migration step can branch on it.
/// v2: the event-dedupe UNIQUE index (`idx_events_dedupe`), added by
/// [`SqliteStore::migrate_event_dedupe`] on open with pre-existing duplicates collapsed.
pub const SCHEMA_VERSION: u32 = 2;

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

/// Provenance of the rows a recording session writes: real hardware or the mock simulation.
/// Stamped into `meta.data_source` on a database's first recording open and verified on every
/// later one — the product's core asset is that the recording is trustworthy, so simulated
/// samples must never be able to land in a real history file (nor real samples in a mock
/// one), no matter where `--db` points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataSource {
    /// Collected from real hardware backends.
    Real,
    /// Produced by the mock simulation (`--mock`, the no-GPU fallback, and `demo`).
    Mock,
}

impl DataSource {
    /// The token stored in `meta.data_source` (and printed in refusal messages).
    pub const fn as_str(self) -> &'static str {
        match self {
            DataSource::Real => "real",
            DataSource::Mock => "mock",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "real" => Some(DataSource::Real),
            "mock" => Some(DataSource::Mock),
            _ => None,
        }
    }
}

impl std::fmt::Display for DataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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

/// Row counts copied by [`SqliteStore::export_to`], one per table — printed by the CLI so
/// the user can see at a glance whether the incident window actually held data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportCounts {
    pub devices: u64,
    pub samples_10s: u64,
    pub samples_1m: u64,
    pub processes_10s: u64,
    pub events: u64,
}

/// Column lists shared by the read, write, and export-copy statements for each table, so
/// the three can never drift apart.
const SAMPLE_COLS: &str = "device_id, bucket_ms, n, util_min, util_avg, util_max, mem_avg, \
     mem_max, power_avg_mw, power_max_mw, temp_avg_c, temp_max_c, fan_max_pct, sm_clock_min, \
     sm_clock_avg, sm_clock_max, throttle_n, throttle_thermal_n, throttle_power_n, \
     throttle_hw_n";
const PROC_COLS: &str =
    "device_id, bucket_ms, pid, name, kind, mem_max, util_avg, cpu_avg, container";
const EVENT_COLS: &str = "ts_ms, device_id, kind, severity, confidence, title, evidence";

/// Persistence failures. Per-metric absence is never one of these (it is `NULL` in the row);
/// an error here means the database itself is unusable.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying SQLite/IO failure.
    Sqlite(rusqlite::Error),
    /// Could not resolve a data directory for `open_default`.
    NoDataDir,
    /// `export_to` refused to clobber an existing output file — a .gpvr someone may
    /// already have shared must never be silently replaced.
    OutputExists(PathBuf),
    /// A recording open found the database stamped with the OTHER data source. Writing
    /// would silently corrupt the product's core asset (mock rows in a real recording, or
    /// real rows hiding inside a simulation file), so it is refused before any row lands.
    /// `db_source` keeps the raw stored token so an unrecognized future value still names
    /// itself in the message instead of being mistaken for one of ours.
    DataSourceMismatch {
        path: PathBuf,
        db_source: String,
        session_source: DataSource,
    },
    /// Another live instance already holds the database's exclusive instance lock. Letting
    /// a second writer in would double-count every rollup bucket and insert every narrated
    /// event twice (the audit's duplicate-narration blocker), so a write open is refused
    /// while the lock is held. The loser is expected to keep running live-only — this is
    /// "someone else is recording", not "the database is broken".
    Locked { path: PathBuf },
    /// The instance-lock sidecar could not be created or locked for a reason other than
    /// contention (permissions, an exotic filesystem without advisory locks). Refused
    /// rather than recorded unlocked: an unenforceable lock silently reopens the
    /// double-recording hole the lock exists to close.
    LockIo {
        lock_path: PathBuf,
        error: std::io::Error,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::NoDataDir => {
                // Per-OS advice naming the variables this OS's resolution actually reads
                // (see `default_data_dir`) — telling a Windows user to set $XDG_DATA_HOME
                // would be a dead end.
                #[cfg(target_os = "windows")]
                {
                    write!(f, "no data directory (set %LOCALAPPDATA% or %USERPROFILE%)")
                }
                #[cfg(target_os = "macos")]
                {
                    write!(f, "no data directory (set $HOME)")
                }
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                {
                    write!(f, "no data directory (set $XDG_DATA_HOME or $HOME)")
                }
            }
            StoreError::OutputExists(p) => {
                write!(f, "refusing to overwrite existing file {}", p.display())
            }
            StoreError::DataSourceMismatch {
                path,
                db_source,
                session_source,
            } => write!(
                f,
                "refusing to record {session_source} data into {} — it contains {db_source} \
                 history (use --db elsewhere or --no-persist)",
                path.display()
            ),
            StoreError::Locked { path } => write!(
                f,
                "another gpuviewer instance is already recording to {} (instance lock {} \
                 is held) — close it, or use --db elsewhere / --no-persist",
                path.display(),
                lock_path_for(path).display()
            ),
            StoreError::LockIo { lock_path, error } => write!(
                f,
                "cannot acquire the instance lock {}: {error}",
                lock_path.display()
            ),
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
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
    path: PathBuf,
    /// The exclusive instance lock on the `<db>.lock` sidecar, held for this write
    /// handle's whole lifetime so a second recording instance is refused for exactly as
    /// long as this one could write ([`StoreError::Locked`]). `None` on read-only
    /// connections and on export destinations (a fresh file no second instance can race
    /// for). Never touched after acquisition — closing the `File` (drop, or the process
    /// dying ANY way, including SIGKILL) is what releases the kernel lock.
    instance_lock: Option<File>,
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

        // Instance lock FIRST, before any look at the database itself (the audit's
        // duplicate-narration blocker: one writer per file, ever). The ordering is
        // load-bearing twice over: a lock refusal must return here, where it can never
        // reach the corruption machinery below — a database that is merely BUSY (another
        // instance recording) must not be quarantined as corrupt — and winning the lock
        // before `try_open_init` means two racing opens can never both initialize/write.
        let lock = Self::acquire_instance_lock(&path)?;

        let existed = path.exists();
        let (mut store, was_reset) = match Self::try_open_init(&path) {
            Ok(store) if !existed => (store, false),
            Ok(store) if store.quick_check_ok() => (store, false),
            // Open succeeded but the integrity check failed, or open/init itself failed:
            // either way the existing file is unusable. Rename it aside and start fresh.
            failed => {
                // Bind and drop the failed store BEFORE the rename: a `_` arm would keep
                // the scrutinee — an open SQLite connection — alive through the arm.
                // POSIX rename tolerates that, but SQLite's win32 VFS opens the file
                // without FILE_SHARE_DELETE, so on Windows the rename would hit a sharing
                // violation and corrupt-db RECOVERY would become a hard startup FAILURE.
                drop(failed);
                Self::quarantine(&path)?;
                (Self::try_open_init(&path)?, true)
            }
        };
        store.instance_lock = Some(lock);
        Ok((store, was_reset))
    }

    /// Open `path` for RECORDING as `source`, enforcing the data-source stamp: a fresh (or
    /// pre-marker) database is stamped with this session's source; one stamped with the
    /// OTHER source is refused ([`StoreError::DataSourceMismatch`]) before any row is
    /// written. Read paths ([`Self::open_readonly`]) never consult the stamp — replaying
    /// or reporting on mock history is fine (the UI labels it "(mock data)"), only
    /// co-mingling WRITES is forbidden.
    pub fn open_recording(
        path: impl AsRef<Path>,
        source: DataSource,
    ) -> Result<(Self, bool), StoreError> {
        let (store, was_reset) = Self::open(path)?;
        store.claim_data_source(source)?;
        Ok((store, was_reset))
    }

    /// Resolve the default history path and open it for recording. `mock=true` selects a
    /// SEPARATE file (`history-mock.db`) so simulated runs can NEVER contaminate the real
    /// recording — the demo and CI must not pollute a user's flight history. The same flag
    /// doubles as the data-source stamp: the default files are only ever opened with the
    /// mode their name encodes, so routing through [`Self::open_recording`] also
    /// retro-stamps pre-marker default databases correctly.
    pub fn open_default(mock: bool) -> Result<(Self, bool), StoreError> {
        let dir = default_data_dir()?;
        let (file, source) = if mock {
            ("history-mock.db", DataSource::Mock)
        } else {
            ("history.db", DataSource::Real)
        };
        Self::open_recording(dir.join(file), source)
    }

    /// A second, read-only connection to an existing database. WAL mode lets this reader run
    /// concurrently with the writer connection — the TUI replay view uses it while the
    /// collector keeps appending. Does not create or modify the file, and deliberately does
    /// NOT take the instance lock: only concurrent WRITERS double-record; `report`, `view`,
    /// and replay must keep working alongside a live recording instance.
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(Self {
            conn,
            path,
            instance_lock: None,
        })
    }

    /// The on-disk path of this store's database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The database's data-source stamp, if it has one. `None` for a pre-marker file (or an
    /// unrecognized token). Works on read-only connections — a viewer may want to label
    /// what a file holds without ever claiming it.
    pub fn data_source(&self) -> Result<Option<DataSource>, StoreError> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'data_source'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.as_deref().and_then(DataSource::parse))
    }

    // ---- open helpers ----

    fn try_open_init(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn,
            path: path.to_path_buf(),
            // No lock here: `open` slots the session lock in after the integrity check;
            // export destinations (`copy_window`) are fresh single-use files that need none.
            instance_lock: None,
        };
        store.init_pragmas()?;
        store.init_schema()?;
        Ok(store)
    }

    /// Acquire the exclusive per-database instance lock: a kernel advisory lock
    /// (`File::try_lock`, std-only, stable since Rust 1.89 — flock semantics on Linux) on
    /// the `<db>.lock` sidecar.
    ///
    /// WHY a kernel lock and not a pidfile: the lock dies with the process — a crash
    /// (panic, SIGKILL, OOM, power loss) releases it automatically, so a previous run can
    /// never wedge future ones. The sidecar file left on disk is inert; its existence means
    /// nothing (only the currently-held lock does), which is exactly the staleness bug
    /// pidfiles have and this design cannot.
    ///
    /// WHY a sidecar and not the database file itself: SQLite owns the db file's locking
    /// protocol (WAL readers and the writer coordinate through it); piling a foreign
    /// exclusive lock onto the same file could starve the concurrent read-only opens the
    /// product depends on.
    fn acquire_instance_lock(db_path: &Path) -> Result<File, StoreError> {
        let lock_path = lock_path_for(db_path);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            // Explicitly no truncate: the file carries no data (the kernel lock state is
            // the whole mechanism), and truncating a sidecar another instance holds locked
            // would be pointless churn.
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| StoreError::LockIo {
                lock_path: lock_path.clone(),
                error,
            })?;
        match file.try_lock() {
            // Hold the handle: the lock lives exactly as long as this `File` (the store
            // keeps it for the session; drop or process death releases it).
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(StoreError::Locked {
                path: db_path.to_path_buf(),
            }),
            Err(TryLockError::Error(error)) => Err(StoreError::LockIo { lock_path, error }),
        }
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
        // Migrations run after the base tables exist and before the version stamps, so a
        // database is only ever stamped with a shape it actually has.
        self.migrate_event_dedupe()?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let now = now_ms_wall();
        // `schema_version` is REPLACED (it must describe the shape the file has NOW, which
        // the migration above may just have changed); `created_ms` stays `INSERT OR
        // IGNORE` so it remains pinned to first creation across reopens.
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('created_ms', ?1)",
            params![now.to_string()],
        )?;
        Ok(())
    }

    /// Schema v1 → v2: collapse any duplicate narrations already in the event log, then
    /// add the UNIQUE index that makes new ones impossible.
    ///
    /// Defense in depth behind the instance lock (the audit's duplicate-narration
    /// blocker): the lock stops two NEW binaries from co-recording, but a database that
    /// was already double-recorded by pre-lock binaries — or that a pre-lock binary keeps
    /// writing to alongside a new one — must still converge to one row per narration.
    /// "GPU0 began throttling" twice at the same second kills the trust thesis.
    ///
    /// Gated on the index's existence rather than `user_version`: idempotent, self-healing
    /// if a version stamp ever exists without the index, and a cheap no-op probe on every
    /// later open (no scan of `events`).
    ///
    /// The DELETE and CREATE INDEX share one transaction deliberately: a process dying
    /// between them would otherwise leave rows deleted WITHOUT the constraint gained —
    /// data loss with nothing to show for it. All-or-nothing means a crashed migration
    /// simply reruns whole on the next open.
    fn migrate_event_dedupe(&self) -> Result<(), StoreError> {
        let have_index: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'idx_events_dedupe'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if have_index.is_some() {
            return Ok(());
        }
        // Keep MIN(id) per (ts, device, kind, title) group: ids are append-ordered, so the
        // survivor is the original narration and the deleted rows are its echoes. Severity/
        // confidence/evidence are not in the key — a duplicated narration differing only
        // there is still the same line on screen twice, which is exactly the bug.
        self.conn.execute_batch(
            "BEGIN;
             DELETE FROM events WHERE id NOT IN (
                 SELECT MIN(id) FROM events GROUP BY ts_ms, device_id, kind, title
             );
             CREATE UNIQUE INDEX idx_events_dedupe ON events (ts_ms, device_id, kind, title);
             COMMIT;",
        )?;
        Ok(())
    }

    /// Verify (or establish) the `meta.data_source` stamp for a recording session.
    ///
    /// An absent stamp is claimed with the session's own source. One rule covers two cases:
    /// - A fresh database: trivially correct — this session is its first writer.
    /// - A pre-marker database (created before the stamp existed): we cannot know what it
    ///   holds. The default-path files are unambiguous by construction (`history.db` only
    ///   ever recorded real sessions; `history-mock.db`/`history-demo.db` only mock ones)
    ///   and are always opened with the matching mode, so the self-stamp is exact there.
    ///   An arbitrary `--db` file is unknowable — refusing it outright would brick every
    ///   database existing users already have, so the conservative rule is: behave exactly
    ///   as before this change for the adopting open, then enforce from the stamp onward.
    fn claim_data_source(&self, source: DataSource) -> Result<(), StoreError> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'data_source'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            None => {
                self.conn.execute(
                    "INSERT OR IGNORE INTO meta(key, value) VALUES ('data_source', ?1)",
                    params![source.as_str()],
                )?;
                Ok(())
            }
            Some(s) if s == source.as_str() => Ok(()),
            Some(s) => Err(StoreError::DataSourceMismatch {
                path: self.path.clone(),
                db_source: s,
                session_source: source,
            }),
        }
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
            "INSERT OR REPLACE INTO {} ({SAMPLE_COLS}) \
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
            let mut stmt = tx.prepare_cached(&format!(
                "INSERT OR REPLACE INTO processes_10s ({PROC_COLS}) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"
            ))?;
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
    ///
    /// `INSERT OR IGNORE` against `idx_events_dedupe`: the same narration (same instant,
    /// device, kind, title) lands at most once no matter who replays it — a second writer
    /// the instance lock could not see (a pre-lock binary), a future double-feed bug,
    /// anything. Duplicate narration is the trust-killer the audit calls the
    /// duplicate-narration blocker, so the log itself refuses it.
    pub fn insert_events(&mut self, events: &[Event]) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(&format!(
                "INSERT OR IGNORE INTO events ({EVENT_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7)"
            ))?;
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
            "SELECT {SAMPLE_COLS} FROM {} WHERE device_id = ?1 AND bucket_ms >= ?2 \
             AND bucket_ms <= ?3 ORDER BY bucket_ms ASC",
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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events \
             WHERE ts_ms >= ?1 AND ts_ms <= ?2 ORDER BY ts_ms ASC, id ASC"
        ))?;
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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PROC_COLS} FROM processes_10s \
             WHERE device_id = ?1 AND bucket_ms = ?2 ORDER BY pid ASC"
        ))?;
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

    /// Newest event timestamp in the log, optionally restricted to one kind — the seek
    /// anchor for `gpuviewer demo` (the most recent throttle onset) and for the file
    /// viewer (a recording opens at its last event). `None` when no such event exists.
    pub fn latest_event_ms(&self, kind: Option<EventKind>) -> Result<Option<u64>, StoreError> {
        let v: Option<i64> = match kind {
            Some(k) => self.conn.query_row(
                "SELECT MAX(ts_ms) FROM events WHERE kind = ?1",
                params![kind_to_str(k)],
                |r| r.get(0),
            )?,
            None => self
                .conn
                .query_row("SELECT MAX(ts_ms) FROM events", [], |r| r.get(0))?,
        };
        Ok(v.map(|v| v as u64))
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

    // ---- export (.gpvr incident files) ----

    /// Copy the `[from_ms, to_ms]` window into a fresh standalone database at `out_path` —
    /// the shareable incident file (`.gpvr`). `meta` and `devices` travel whole (identity
    /// and provenance ride with the data); the sample/process/event tables are restricted
    /// to the window. Implemented as ATTACH + `INSERT..SELECT` in one transaction on a NEW
    /// writer connection, with THIS store's file attached `mode=ro` — the export can never
    /// modify the recording it reads, and no row round-trips through Rust.
    ///
    /// Refuses to overwrite an existing `out_path` ([`StoreError::OutputExists`]); a failed
    /// export removes its half-written file so a retry is not blocked by that refusal.
    pub fn export_to(
        &self,
        out_path: impl AsRef<Path>,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<ExportCounts, StoreError> {
        let out_path = out_path.as_ref();
        if out_path.exists() {
            return Err(StoreError::OutputExists(out_path.to_path_buf()));
        }
        let result = self.copy_window(out_path, from_ms, to_ms);
        if result.is_err() {
            let _ = std::fs::remove_file(out_path);
            for ext in ["-wal", "-shm"] {
                let mut side = out_path.as_os_str().to_os_string();
                side.push(ext);
                let _ = std::fs::remove_file(side);
            }
        }
        result
    }

    fn copy_window(
        &self,
        out_path: &Path,
        from_ms: u64,
        to_ms: u64,
    ) -> Result<ExportCounts, StoreError> {
        let mut dest = Self::try_open_init(out_path)?;
        // ATTACH cannot run inside a transaction, so bind the source first. `mode=ro` is
        // load-bearing: the recording stays untouchable even though this connection writes.
        dest.conn.execute(
            "ATTACH DATABASE ?1 AS src",
            params![format!("file:{}?mode=ro", uri_path(&self.path))],
        )?;
        let (from, to) = (from_ms as i64, to_ms as i64);
        let tx = dest.conn.transaction()?;
        // Source meta wins over the fresh file's own stamps: created_ms etc. describe the
        // recording, not the export. The window is stamped alongside so the file says what
        // slice it claims to hold.
        tx.execute(
            "INSERT OR REPLACE INTO meta SELECT key, value FROM src.meta",
            [],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) \
             VALUES ('export_from_ms', ?1), ('export_to_ms', ?2)",
            params![from.to_string(), to.to_string()],
        )?;
        let devices = tx.execute(
            "INSERT INTO devices SELECT device_id, name, vendor, mem_total_bytes \
             FROM src.devices",
            [],
        )?;
        let samples_10s = tx.execute(
            &format!(
                "INSERT INTO samples_10s SELECT {SAMPLE_COLS} FROM src.samples_10s \
                 WHERE bucket_ms >= ?1 AND bucket_ms <= ?2"
            ),
            params![from, to],
        )?;
        let samples_1m = tx.execute(
            &format!(
                "INSERT INTO samples_1m SELECT {SAMPLE_COLS} FROM src.samples_1m \
                 WHERE bucket_ms >= ?1 AND bucket_ms <= ?2"
            ),
            params![from, to],
        )?;
        let processes_10s = tx.execute(
            &format!(
                "INSERT INTO processes_10s SELECT {PROC_COLS} FROM src.processes_10s \
                 WHERE bucket_ms >= ?1 AND bucket_ms <= ?2"
            ),
            params![from, to],
        )?;
        // Fresh autoincrement ids in the copy; ORDER BY preserves the source's intra-tick
        // event order so `events_between` reads the export in the original sequence.
        // OR IGNORE because the source is opened read-only and may predate the dedupe
        // migration (a pre-v2 file can still hold duplicate narrations): the export must
        // collapse them against the fresh file's unique index, not abort on them.
        let events = tx.execute(
            &format!(
                "INSERT OR IGNORE INTO events ({EVENT_COLS}) SELECT {EVENT_COLS} \
                 FROM src.events \
                 WHERE ts_ms >= ?1 AND ts_ms <= ?2 ORDER BY ts_ms ASC, id ASC"
            ),
            params![from, to],
        )?;
        tx.commit()?;
        Ok(ExportCounts {
            devices: devices as u64,
            samples_10s: samples_10s as u64,
            samples_1m: samples_1m as u64,
            processes_10s: processes_10s as u64,
            events: events as u64,
        })
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

/// The instance-lock sidecar path: `<db>.lock` next to the database, following the
/// `-wal`/`-shm` sidecar convention so it is obviously associated with its file.
fn lock_path_for(db_path: &Path) -> PathBuf {
    let mut p = db_path.as_os_str().to_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

/// Percent-encode a filesystem path for a `file:` SQLite URI: `%`, `?`, and `#` would
/// otherwise read as URI syntax. SQLite percent-decodes the path, so this is lossless.
fn uri_path(p: &Path) -> String {
    p.to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23")
}

fn now_ms_wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Default per-OS data dir, creating nothing here (the caller's `open` makes the dir):
/// Linux `$XDG_DATA_HOME/gpuviewer` (else `~/.local/share/gpuviewer`), Windows
/// `%LOCALAPPDATA%\gpuviewer`, macOS `~/Library/Application Support/gpuviewer`.
///
/// Resolved from ENVIRONMENT VARIABLES on every OS — deliberately not the Windows Known
/// Folder API: child processes must be able to redirect it via env, which the hermetic
/// test pattern (and `--db`-less CI runs) depends on. The trade-off (ignoring
/// registry-redirected/roaming AppData) is acceptable for a CLI tool and is the reason
/// the `dirs`/`directories` crates were rejected (SHGetKnownFolderPath cannot be
/// redirected per-process; their path shapes also differ from ours on Windows AND macOS).
/// Empty values count as unset throughout: an empty override would silently root the
/// history under the current directory.
pub fn default_data_dir() -> Result<PathBuf, StoreError> {
    #[cfg(target_os = "windows")]
    {
        // %LOCALAPPDATA% is set for every interactive logon (and on GitHub runners) but
        // can be absent under service accounts — hence the %USERPROFILE% fallback.
        if let Some(v) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(v).join("gpuviewer"));
        }
        if let Some(v) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(v)
                .join("AppData")
                .join("Local")
                .join("gpuviewer"));
        }
        Err(StoreError::NoDataDir)
    }
    #[cfg(target_os = "macos")]
    {
        // XDG is deliberately NOT consulted on macOS: native tools put per-user data in
        // Application Support, and a half-XDG layout would scatter the history.
        if let Some(v) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(v).join("Library/Application Support/gpuviewer"));
        }
        Err(StoreError::NoDataDir)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // BYTE-IDENTICAL to the shipped v1 chain — existing users' history.db must not move.
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(xdg).join("gpuviewer"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(home).join(".local/share/gpuviewer"));
        }
        Err(StoreError::NoDataDir)
    }
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
// NOTE: the UNIQUE dedupe index on events (idx_events_dedupe) is deliberately NOT in
// SCHEMA_SQL. Creating it here would fail on a pre-v2 database that already holds
// duplicate narrations — and a failed init reads as corruption to `open`, which would
// quarantine a perfectly healthy file. `migrate_event_dedupe` (which collapses the
// duplicates first, in the same transaction) is the only place that creates it.
