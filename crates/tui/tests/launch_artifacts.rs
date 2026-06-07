//! Launch-artifact CLI suite: `demo --seed-only`, `export`, and `view` run as the real
//! built binary — exactly what a user (or CI smoke job) sees at the shell. The TUI itself
//! needs a tty, so these exercise precisely the paths that must work without one. Every
//! test points the process at a scratch data dir, so the user's real history is never read
//! or written.

use std::path::PathBuf;
use std::process::Command;

use gpuviewer_core::{now_ms, Confidence, DeviceId, Event, EventKind, Severity, Vendor};
use gpuviewer_history::{SqliteStore, Tier};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gpuviewer"))
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
/// events and rollups spanning ~8h. XDG_DATA_HOME/HOME are overridden on the child process
/// only, so the test is hermetic.
#[test]
fn demo_seed_only_builds_an_eight_hour_story() {
    let dir = scratch_dir("demo");
    let out = bin()
        .args(["demo", "--seed-only"])
        .env("XDG_DATA_HOME", &dir)
        .env("HOME", &dir)
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

    let db = dir.join("gpuviewer").join("history-demo.db");
    assert!(db.exists(), "demo db must exist at {}", db.display());
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

/// --help documents the launch artifacts — demo/export/view must be discoverable.
#[test]
fn help_documents_the_launch_artifacts() {
    let out = bin().arg("--help").output().expect("failed to spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in ["demo", "export", "view", ".gpvr", "--seed-only", "--since"] {
        assert!(stdout.contains(needle), "--help must mention {needle:?}");
    }
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
        throttle_thermal_n: 0,
        throttle_power_n: 0,
        throttle_hw_n: 0,
    }
}
