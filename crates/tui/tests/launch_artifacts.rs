//! Launch-artifact CLI suite: `demo --seed-only`, `export`, and `view` run as the real
//! built binary — exactly what a user (or CI smoke job) sees at the shell. The TUI itself
//! needs a tty, so these exercise precisely the paths that must work without one. Every
//! test points the process at a scratch data dir, so the user's real history is never read
//! or written.

use std::path::{Path, PathBuf};
use std::process::Command;

use gpuviewer_core::{now_ms, Confidence, DeviceId, Event, EventKind, Severity, Vendor};
use gpuviewer_history::{DataSource, SqliteStore, StoreError, Tier};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gpuviewer"))
}

/// The binary with EVERY per-OS data-dir root redirected to `dir` on the child process
/// only: XDG_DATA_HOME (Linux), HOME (macOS — and the Linux fallback), LOCALAPPDATA +
/// USERPROFILE (Windows). Setting all four on every OS is deliberate: resolution is
/// env-based per OS, so the variables the local OS ignores are inert, and one helper
/// keeps every spawn hermetic across the whole CI matrix.
fn bin_hermetic(dir: &Path) -> Command {
    let mut cmd = bin();
    cmd.env("XDG_DATA_HOME", dir)
        .env("HOME", dir)
        .env("LOCALAPPDATA", dir)
        .env("USERPROFILE", dir);
    cmd
}

/// A unique scratch directory per test; removed at the end (a failed test leaves it for
/// inspection under the OS temp dir).
fn scratch_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("gpuviewer-cli-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `demo --seed-only` builds the demo database under the (overridden) data dir, prints the
/// one-line summary with the db path, and exits 0 without a tty. The store must hold >0
/// events and rollups spanning ~8h. The data-dir env is overridden on the child process
/// only, so the test is hermetic.
#[test]
fn demo_seed_only_builds_an_eight_hour_story() {
    let dir = scratch_dir("demo");
    let out = bin_hermetic(&dir)
        .args(["demo", "--seed-only"])
        .output()
        .expect("failed to spawn gpuviewer");
    assert!(
        out.status.success(),
        "demo --seed-only must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("seeded 8h of simulated history:"),
        "summary line missing: {stdout}"
    );
    assert!(
        stdout.contains("events") && stdout.contains("throttle episodes"),
        "summary must count events and throttle episodes: {stdout}"
    );
    assert!(
        stdout.contains("history-demo.db"),
        "summary must include the db path: {stdout}"
    );

    // The db path is parsed from the summary line rather than assuming the XDG layout:
    // the per-OS default dir shapes differ (macOS appends Library/Application Support),
    // and the summary line is already part of the asserted contract above.
    let db_line = stdout
        .lines()
        .find(|l| l.contains("(db: "))
        .expect("summary line must name the db path");
    let after = &db_line[db_line.find("(db: ").unwrap() + "(db: ".len()..];
    let db = PathBuf::from(after.trim_end().trim_end_matches(')'));
    assert!(db.exists(), "demo db must exist at {}", db.display());
    assert!(
        db.starts_with(&dir),
        "the demo db must land under the redirected scratch dir, got {}",
        db.display()
    );
    let store = SqliteStore::open_readonly(&db).unwrap();

    // Rollups span ~8h (bucket flooring trims up to 60s off either edge, never more).
    let earliest = store
        .earliest_bucket_ms()
        .unwrap()
        .expect("seeded rollups must exist");
    let latest = store
        .latest_bucket_ms()
        .unwrap()
        .expect("seeded rollups must exist");
    let span = latest.saturating_sub(earliest);
    let eight_hours: u64 = 8 * 3600 * 1000;
    assert!(
        span >= eight_hours - 120_000 && span <= eight_hours + 120_000,
        "rollups must span ~8h, got {span} ms"
    );

    let events = store.events_between(earliest, latest + 60_000).unwrap();
    assert!(!events.is_empty(), "the seeded story must contain events");
    assert!(
        store
            .latest_event_ms(Some(EventKind::ThrottleStart))
            .unwrap()
            .is_some(),
        "the demo's whole point: a throttle onset to scroll back to"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `export --since 30m` copies only the last half hour. The source is seeded in-process
/// with 2h of 10s rollups relative to the real clock (the CLI resolves --since against
/// now), with a decoy event well outside the window that must NOT cross into the export.
/// A second run against the same OUT must refuse and leave the file byte-identical.
#[test]
fn export_honors_the_window_and_refuses_overwrite() {
    let dir = scratch_dir("export");
    let src = dir.join("history.db");
    let out_path = dir.join("incident.gpvr");
    let now = now_ms();
    let dev = DeviceId("0000:05:00.0".into());

    // 2h of 10s buckets ending at the current bucket, plus one event 90m back (decoy) and
    // one 10m back (must export). Rollups are inserted directly: this test is about the
    // CLI's window plumbing, not the Recorder.
    {
        let (mut store, _) = SqliteStore::open(&src).unwrap();
        store
            .register_device(&dev, "CLI GPU", Vendor::Nvidia, Some(8 << 30))
            .unwrap();
        let b_now = now - now % 10_000;
        let rows: Vec<_> = (0..=720u64)
            .map(|i| sample_rollup(&dev, b_now - (720 - i) * 10_000))
            .collect();
        store.insert_sample_rollups(Tier::TenSec, &rows).unwrap();
        store
            .insert_events(&[
                throttle_event(&dev, now - 90 * 60_000, "decoy ninety minutes ago"),
                throttle_event(&dev, now - 10 * 60_000, "recent throttle onset"),
            ])
            .unwrap();
    }

    let out = bin()
        .args([
            "export",
            "--db",
            src.to_str().unwrap(),
            "--since",
            "30m",
            out_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to spawn gpuviewer");
    assert!(
        out.status.success(),
        "export must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("samples_10s") && stdout.contains("events"),
        "export must print per-table row counts: {stdout}"
    );

    // The export holds ~180 buckets (the binary's `now` is a touch later than ours, so
    // allow a little drift) and ONLY in-window rows.
    let exported = SqliteStore::open_readonly(&out_path).unwrap();
    let rows = exported
        .samples_between(&dev, 0, now + 3_600_000, Tier::TenSec)
        .unwrap();
    assert!(
        (174..=187).contains(&rows.len()),
        "a 30m window of 10s buckets is ~180 rows, got {}",
        rows.len()
    );
    // Regression (0.2.2): the copy must keep every column in its own column. The
    // positional INSERT shipped before named the observed count `throttle_hw_n`.
    let r0 = rows.first().expect("window must not be empty");
    assert_eq!(
        (
            r0.throttle_n,
            r0.throttle_observed_n,
            r0.throttle_thermal_n,
            r0.throttle_power_n,
            r0.throttle_hw_n
        ),
        (0, 10, 0, 0, 0),
        "exported throttle counters must survive the copy column-for-column"
    );
    assert_eq!(r0.util_max, Some(90.0));
    assert_eq!(r0.sm_clock_max, Some(1800));
    let oldest = rows.first().expect("window must not be empty").bucket_ms;
    assert!(
        oldest >= now - 31 * 60_000,
        "no bucket older than the window may cross: oldest {oldest}, now {now}"
    );
    let events = exported.events_between(0, now + 3_600_000).unwrap();
    assert_eq!(events.len(), 1, "only the recent event is in-window");
    assert!(events[0].title.contains("recent throttle onset"));

    // Second run: refused, file untouched.
    let before = std::fs::read(&out_path).unwrap();
    let out = bin()
        .args([
            "export",
            "--db",
            src.to_str().unwrap(),
            out_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to spawn gpuviewer");
    assert!(
        !out.status.success(),
        "overwriting an existing export must fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to overwrite"),
        "the refusal must be explicit: {stderr}"
    );
    assert_eq!(
        std::fs::read(&out_path).unwrap(),
        before,
        "the refused run must leave the file byte-identical"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `view` on a missing path prints one friendly error and exits 1 — and a non-recording
/// file (junk bytes) is rejected the same way instead of opening a broken UI.
#[test]
fn view_rejects_missing_and_invalid_files() {
    let dir = scratch_dir("view");

    let missing = dir.join("nope.gpvr");
    let out = bin()
        .args(["view", missing.to_str().unwrap()])
        .output()
        .expect("failed to spawn gpuviewer");
    assert_eq!(out.status.code(), Some(1), "missing file must exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no such file"),
        "the error must say the file is missing: {stderr}"
    );

    let junk = dir.join("junk.gpvr");
    std::fs::write(&junk, b"definitely not a sqlite database").unwrap();
    let out = bin()
        .args(["view", junk.to_str().unwrap()])
        .output()
        .expect("failed to spawn gpuviewer");
    assert_eq!(out.status.code(), Some(1), "junk file must exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a gpuviewer recording"),
        "the error must say the file is not a recording: {stderr}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The NDJSON stream is built to be piped, and `--json | head -1` is the documented way to
/// grab a frame — so a consumer hanging up must end the run cleanly (exit 0), not abort
/// with the default Rust broken-pipe panic. The stream would otherwise run forever, so a
/// watchdog kills it (and fails) if neither happens.
#[test]
fn json_stream_ends_cleanly_when_the_consumer_hangs_up() {
    use std::io::Read;
    use std::process::Stdio;

    let dir = scratch_dir("epipe");
    let mut child = bin_hermetic(&dir)
        .args(["--json", "--mock", "--interval", "100"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn gpuviewer");

    // Prove the stream started, then hang up: the next write hits EPIPE.
    let mut stdout = child.stdout.take().expect("stdout must be piped");
    let mut first = [0u8; 1];
    stdout.read_exact(&mut first).expect("first stream byte");
    drop(stdout);

    let mut status = None;
    for _ in 0..100 {
        if let Some(s) = child.try_wait().expect("try_wait") {
            status = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let Some(status) = status else {
        let _ = child.kill();
        panic!("the stream must end on its own once the consumer is gone");
    };
    assert!(
        status.success(),
        "hangup must end the stream cleanly (exit 0), got {status:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The mock/--db contamination guard at the CLI: pointing a --mock session at a database
/// stamped `real` must be a clean startup refusal — exit nonzero, one actionable line on
/// stderr naming the file — not a panic, and never a write. The --json path shares the
/// guard, so that is the mode exercised (no tty needed); both directions are hardware-
/// independent because the preflight runs before any backend probing.
#[test]
fn recording_refuses_cross_mode_db() {
    let dir = scratch_dir("guard");

    // Mock data into a real-stamped db: refused.
    let real_db = dir.join("real-history.db");
    drop(SqliteStore::open_recording(&real_db, DataSource::Real).unwrap());
    let out = bin_hermetic(&dir)
        .args([
            "--json",
            "--once",
            "--mock",
            "--db",
            real_db.to_str().unwrap(),
        ])
        .output()
        .expect("failed to spawn gpuviewer");
    assert!(
        !out.status.success(),
        "recording mock data into a real db must exit nonzero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to record mock data into")
            && stderr.contains("real-history.db")
            && stderr.contains("it contains real history"),
        "the refusal must name the file and the mismatch: {stderr}"
    );
    // The refused run wrote nothing: still stamped real, still zero recorded rows.
    let store = SqliteStore::open_readonly(&real_db).unwrap();
    assert_eq!(store.data_source().unwrap(), Some(DataSource::Real));
    assert!(
        store.devices().unwrap().is_empty(),
        "no mock device may have been registered into the real db"
    );
    assert!(store
        .events_between(0, now_ms() + 3_600_000)
        .unwrap()
        .is_empty());
    drop(store);

    // The reverse: a real session pointed at a mock-stamped db is refused the same way.
    let mock_db = dir.join("mock-history.db");
    drop(SqliteStore::open_recording(&mock_db, DataSource::Mock).unwrap());
    let out = bin_hermetic(&dir)
        .args(["--json", "--once", "--db", mock_db.to_str().unwrap()])
        .output()
        .expect("failed to spawn gpuviewer");
    assert!(
        !out.status.success(),
        "recording real data into a mock db must exit nonzero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to record real data into")
            && stderr.contains("it contains mock history"),
        "the reverse refusal must be just as explicit: {stderr}"
    );

    // Read-only paths ignore the stamp: `report` on the mock-stamped db still works.
    let out = bin()
        .args(["report", "--db", mock_db.to_str().unwrap()])
        .output()
        .expect("failed to spawn gpuviewer");
    assert!(
        out.status.success(),
        "report must read a mock-stamped db regardless; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The instance lock at the CLI (the audit's duplicate-narration blocker): while one
/// instance records to a --db, a second instance pointed at the same file must LOSE the
/// lock but keep working — live-only, exit 0, the reason on stderr — and read-only modes
/// (`report`) must run concurrently with the holder. Once the holder exits, the lock is
/// free and the next recording open succeeds (flock semantics: the lock dies with the
/// process, so nothing can wedge).
#[test]
fn second_instance_loses_the_lock_and_runs_live_only() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let dir = scratch_dir("lock");
    let db = dir.join("shared-history.db");

    // Instance 1: a long-running --json stream — the systemd-unit shape from the audit.
    let mut holder = bin_hermetic(&dir)
        .args([
            "--json",
            "--mock",
            "--interval",
            "100",
            "--db",
            db.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn the holding instance");
    // Its first frame line proves Engine::new completed — the store is open, the lock held.
    // Keep the reader alive: dropping it would EPIPE the stream and end the holder early.
    let mut holder_out = BufReader::new(holder.stdout.take().expect("stdout must be piped"));
    let mut first_frame = String::new();
    holder_out
        .read_line(&mut first_frame)
        .expect("the holder must emit a first frame");
    assert!(first_frame.contains("\"frame\""), "got: {first_frame}");

    // Instance 2, same --db: must still do its job (a frame on stdout, exit 0) while
    // recording nothing, and must say WHY on stderr.
    let out = bin_hermetic(&dir)
        .args(["--json", "--once", "--mock", "--db", db.to_str().unwrap()])
        .output()
        .expect("failed to spawn the losing instance");
    assert!(
        out.status.success(),
        "the lock loser must keep working (live-only), stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"frame\""),
        "the loser must still emit its frame"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another gpuviewer instance is already recording")
            && stderr.contains("shared-history.db"),
        "the loser must name the conflict and the file: {stderr}"
    );
    assert!(
        stderr.contains("live-only"),
        "the loser must say it is running live-only: {stderr}"
    );

    // Read-only modes are unrestricted while the lock is held.
    let report = bin()
        .args(["report", "--db", db.to_str().unwrap()])
        .output()
        .expect("failed to spawn report");
    assert!(
        report.status.success(),
        "report must read alongside a live recording; stderr: {}",
        String::from_utf8_lossy(&report.stderr)
    );

    // Holder exits -> kernel releases the lock -> the next recording open succeeds.
    // Bounded retry instead of a one-shot open: on Linux flock release is immediate at
    // process death, but Microsoft documents that LockFileEx locks may take OS-dependent
    // time to be released after TerminateProcess — an immediate single attempt is a real
    // windows-latest flake, not a hardening nicety.
    holder.kill().expect("kill holder");
    holder.wait().expect("wait holder");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match SqliteStore::open_recording(&db, DataSource::Mock) {
            Ok(store) => {
                drop(store);
                break;
            }
            Err(StoreError::Locked { .. }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("the lock must be free once the holder is gone: {e}"),
        }
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// --help documents the launch artifacts — demo/export/view must be discoverable.
#[test]
fn help_documents_the_launch_artifacts() {
    let out = bin().arg("--help").output().expect("failed to spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "demo",
        "export",
        "view",
        ".gpvr",
        "--seed-only",
        "--since",
        "--until",
        "--keep-cmdline",
        "doctor",
        "report",
        "--json",
    ] {
        assert!(stdout.contains(needle), "--help must mention {needle:?}");
    }
}

/// `export --until` bounds the window's right edge, an inverted window is refused, and
/// process command lines are omitted from the shareable file unless `--keep-cmdline`.
#[test]
fn export_until_bounds_the_window_and_cmdline_is_opt_in() {
    let dir = scratch_dir("export-until");
    let src = dir.join("src.db");
    let dev = DeviceId("0000:01:00.0".into());
    let now = now_ms();
    let b_now = now - now % 10_000;
    {
        let (mut store, _) = SqliteStore::open(&src).unwrap();
        store
            .register_device(&dev, "CLI GPU", Vendor::Nvidia, Some(8 << 30))
            .unwrap();
        let rows: Vec<_> = (0..=720u64)
            .map(|i| sample_rollup(&dev, b_now - (720 - i) * 10_000))
            .collect();
        store.insert_sample_rollups(Tier::TenSec, &rows).unwrap();
        store
            .insert_process_rollups(&[gpuviewer_history::ProcessRollup {
                device_id: dev.clone(),
                bucket_ms: b_now - 90 * 60_000,
                pid: 4521,
                name: "python".into(),
                kind: gpuviewer_core::ProcessKind::Compute,
                mem_max: Some(4 << 30),
                util_avg: Some(90.0),
                cpu_avg: Some(120.0),
                container: None,
                cmdline: Some("python train.py --config private/client.yaml".into()),
            }])
            .unwrap();
        store
            .insert_events(&[
                throttle_event(&dev, now - 90 * 60_000, "the overnight incident"),
                throttle_event(&dev, now - 10 * 60_000, "something later"),
            ])
            .unwrap();
    }
    let export = |out: &Path, extra: &[&str]| {
        let mut args = vec![
            "export",
            "--db",
            src.to_str().unwrap(),
            "--since",
            "2h",
            "--until",
            "1h",
        ];
        args.extend_from_slice(extra);
        args.push(out.to_str().unwrap());
        bin().args(&args).output().expect("spawn")
    };

    let plain = dir.join("plain.gpvr");
    let out = export(&plain, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("command lines omitted"));
    let ex = SqliteStore::open_readonly(&plain).unwrap();
    let events = ex.events_between(0, now + 3_600_000).unwrap();
    assert_eq!(
        events.len(),
        1,
        "--until excludes what came after the window"
    );
    assert!(events[0].title.contains("overnight"));
    let procs = ex.processes_at(&dev, b_now - 90 * 60_000).unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!(
        procs[0].cmdline, None,
        "command lines are opt-in for a shared file"
    );

    let kept = dir.join("kept.gpvr");
    let out = export(&kept, &["--keep-cmdline"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ex = SqliteStore::open_readonly(&kept).unwrap();
    let procs = ex.processes_at(&dev, b_now - 90 * 60_000).unwrap();
    assert_eq!(
        procs[0].cmdline.as_deref(),
        Some("python train.py --config private/client.yaml")
    );

    let out = bin()
        .args([
            "export",
            "--db",
            src.to_str().unwrap(),
            "--since",
            "1h",
            "--until",
            "2h",
            dir.join("inverted.gpvr").to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "an inverted window must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("empty window"));
    std::fs::remove_dir_all(&dir).ok();
}

/// `report --json` is ONE parseable object with the documented envelope.
#[test]
fn report_json_is_one_object_with_devices_and_events() {
    let dir = scratch_dir("report-json");
    let src = dir.join("src.db");
    let dev = DeviceId("0000:01:00.0".into());
    let now = now_ms();
    {
        let (mut store, _) = SqliteStore::open(&src).unwrap();
        store
            .register_device(&dev, "CLI GPU", Vendor::Nvidia, Some(8 << 30))
            .unwrap();
        let b_now = now - now % 10_000;
        let rows: Vec<_> = (0..=60u64)
            .map(|i| sample_rollup(&dev, b_now - (60 - i) * 10_000))
            .collect();
        store.insert_sample_rollups(Tier::TenSec, &rows).unwrap();
        store
            .insert_events(&[throttle_event(&dev, now - 60_000, "a throttle")])
            .unwrap();
    }
    let out = bin()
        .args([
            "report",
            "--db",
            src.to_str().unwrap(),
            "--since",
            "1h",
            "--json",
        ])
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "one line, one object");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(v["v"], 1);
    assert_eq!(v["type"], "report");
    let d = &v["devices"][0];
    assert_eq!(d["id"], "0000:01:00.0");
    assert_eq!(d["has_samples"], true);
    assert_eq!(d["util_max_pct"], 90.0);
    assert_eq!(v["events"][0]["kind"], "throttle_start");
    std::fs::remove_dir_all(&dir).ok();
}

/// `doctor --mock` runs without a GPU or a tty, names the backend, every device with its
/// metric split, and the (not yet created) history database — and records nothing.
#[test]
fn doctor_reports_backends_devices_and_history_without_recording() {
    let dir = scratch_dir("doctor");
    let out = bin_hermetic(&dir)
        .args(["doctor", "--mock"])
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "doctor",
        "mock    in use (--mock)",
        "GPU0",
        "GPU1",
        "available",
        "unavailable",
        "xid errors",
        "history-mock.db",
        "not created yet",
    ] {
        assert!(
            stdout.contains(needle),
            "doctor output must mention {needle:?}:\n{stdout}"
        );
    }
    // Nothing recorded: the data dir holds no database afterwards.
    let any_db = walk(&dir)
        .iter()
        .any(|p| p.extension().is_some_and(|e| e == "db"));
    assert!(
        !any_db,
        "doctor must never create or write a history database"
    );
    std::fs::remove_dir_all(&dir).ok();
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// SIGTERM (`systemctl stop`) ends a `--json` session the clean way: exit 0, and the
/// stream's last line is the `recording_stopped` mark — the stop the next session would
/// otherwise misreport as a crash.
#[cfg(unix)]
#[test]
fn sigterm_ends_a_json_session_with_the_stop_mark() {
    use std::io::{BufRead, BufReader};
    let dir = scratch_dir("sigterm");
    let mut child = bin_hermetic(&dir)
        .args(["--json", "--mock", "--interval", "200"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    assert!(
        first.contains("\"type\":\"frame\""),
        "stream started: {first}"
    );
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut reader, &mut rest).unwrap();
    let code = child.wait().expect("wait");
    assert!(
        code.success(),
        "a SIGTERM'd session exits cleanly, got {code:?}"
    );
    let last = rest.lines().last().unwrap_or(&first);
    let v: serde_json::Value = serde_json::from_str(last).expect("last line is JSON");
    assert_eq!(v["kind"], "recording_stopped", "final line: {last}");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- fixtures ----

fn throttle_event(dev: &DeviceId, ts_ms: u64, title: &str) -> Event {
    Event {
        ts_ms,
        device: dev.clone(),
        kind: EventKind::ThrottleStart,
        severity: Severity::Warning,
        confidence: Confidence::Fact,
        title: title.into(),
        evidence: "throttle bits: [thermal]".into(),
    }
}

fn sample_rollup(dev: &DeviceId, bucket_ms: u64) -> gpuviewer_history::SampleRollup {
    gpuviewer_history::SampleRollup {
        device_id: dev.clone(),
        bucket_ms,
        n: 10,
        util_min: Some(10.0),
        util_avg: Some(50.0),
        util_max: Some(90.0),
        mem_avg: Some(4 << 30),
        mem_max: Some(5 << 30),
        power_avg_mw: Some(150_000),
        power_max_mw: Some(200_000),
        temp_avg_c: Some(60.0),
        temp_max_c: Some(70.0),
        fan_max_pct: Some(55.0),
        sm_clock_min: Some(1200),
        sm_clock_avg: Some(1500),
        sm_clock_max: Some(1800),
        throttle_n: 0,
        // n=10 frames, all of which could read the bitmask: an observed all-clear, which
        // must survive export/view as distinct from "never observable".
        throttle_observed_n: 10,
        throttle_thermal_n: 0,
        throttle_power_n: 0,
        throttle_hw_n: 0,
    }
}
