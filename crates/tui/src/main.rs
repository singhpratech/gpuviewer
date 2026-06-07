//! gpuviewer — the GPU flight recorder.
//!
//! Modes:
//!   gpuviewer                 interactive TUI (always-on persistence + narrated events)
//!   gpuviewer --json          stream NDJSON v1 lines per tick to stdout
//!   gpuviewer --json --once   print one frame line plus its event lines, then exit
//!   gpuviewer report          print a plain-text digest of the recorded history
//!   gpuviewer demo            seed 8h of simulated history, open scrolled back to the story
//!   gpuviewer export OUT      copy a window of history into a standalone .gpvr file
//!   gpuviewer view FILE       replay a .gpvr file read-only (no GPU, no recording)
//!
//! Persistence is on by default in the live modes: 10s/1m rollups + an event log go to the
//! history database (separate `-mock` file under `--mock`). `report` reads it back.

mod app;
mod collector;
mod ui;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use collector::{Collector, Engine, EngineConfig, FrameDevice};
use gpuviewer_core::mock::MockBackend;
use gpuviewer_core::{
    now_ms, Confidence, DeviceId, Event, EventEngine, EventKind, GpuBackend, Severity, StaticInfo,
};
use gpuviewer_history::{
    DataSource, DeviceRow, Recorder, SqliteStore, StoreError, Tier, RETAIN_10S_MS, RETAIN_1M_MS,
    RETAIN_EVENTS_MS,
};
use serde::Serialize;

struct Args {
    json: bool,
    once: bool,
    mock: bool,
    interval: Duration,
    /// Adaptive idle backoff on (default); `--no-backoff` turns it off.
    backoff: bool,
    /// Persistence on (default); `--no-persist` turns it off (and with it, replay/report).
    persist: bool,
    /// Override history database path (else the XDG default).
    db: Option<PathBuf>,
    /// `--on-event 'CMD'`: shell command fired for every emitted event.
    on_event: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            json: false,
            once: false,
            mock: false,
            interval: Duration::from_millis(1000),
            backoff: true,
            persist: true,
            db: None,
            on_event: None,
        }
    }
}

/// Parsed `report` subcommand options.
struct ReportArgs {
    since: Option<String>,
    until: Option<String>,
    db: Option<PathBuf>,
    mock: bool,
}

const HELP: &str = "gpuviewer — the GPU flight recorder\n\n\
    USAGE:\n  \
      gpuviewer [--json [--once]] [--mock] [--interval <ms>] [--no-backoff]\n            \
                [--db <path>] [--on-event <cmd>]\n  \
      gpuviewer report [--since <spec>] [--until <spec>] [--db <path>] [--mock]\n  \
      gpuviewer demo [--seed-only]\n  \
      gpuviewer export [--since <spec>] [--db <path>] [--mock] <out.gpvr>\n  \
      gpuviewer view <file.gpvr>\n\n\
    LIVE OPTIONS:\n  \
      --json          stream one NDJSON frame per tick to stdout\n  \
      --once          with --json: print a single frame and exit\n  \
      --mock          use ONLY the simulated GPUs (deterministic; also the fallback when no\n                  \
                      GPU is found). Records to a SEPARATE history-mock.db, never real history\n  \
      --interval <ms> sampling interval, default 1000, minimum 100\n  \
      --no-backoff    disable the adaptive low-power cadence (idle GPUs are normally polled\n                  \
                      slower so polling does not keep them awake)\n  \
      --no-persist    do not record history (the replay view and `report` need the recording)\n  \
      --db <path>     history database path (default: $XDG_DATA_HOME/gpuviewer/history.db).\n                  \
                      A database is stamped real or mock on first recording; recording the\n                  \
                      other kind into it is refused so simulations never pollute real history.\n                  \
                      Only ONE instance records to a database at a time — a second instance\n                  \
                      runs live-only (read-only modes like report/view are unrestricted)\n  \
      --on-event <c>  run `sh -c <c>` for every emitted event, with GPV_EVENT_KIND,\n                  \
                      GPV_EVENT_SEVERITY, GPV_EVENT_CONFIDENCE, GPV_EVENT_TITLE,\n                  \
                      GPV_EVENT_EVIDENCE, GPV_EVENT_DEVICE, GPV_EVENT_TS_MS, GPV_EVENT_JSON\n                  \
                      in the environment (capped at 60 spawns/min). Example:\n                  \
                      --on-event 'curl -s -d \"$GPV_EVENT_TITLE\" ntfy.sh/mytopic'\n\n\
    report — print a plain-text digest of recorded history (no ANSI):\n  \
      --since <spec>  window start; default 24h\n  \
      --until <spec>  window end; default now\n  \
      --db <path>     history database to read (read-only)\n  \
      --mock          read the history-mock.db instead of real history\n\n  \
      <spec> is a relative duration (12h, 45m, 7d) or a clock time (HH:MM, today; yesterday\n      \
      if that time is still in the future)\n\n\
    demo — seed 8h of simulated history into its own history-demo.db (history.db is never\n    \
    touched), then open the TUI already scrolled back to the last throttle onset:\n  \
      --seed-only     build the database and print the summary, then exit (no tty needed)\n\n\
    export — copy a window of recorded history into a standalone, shareable .gpvr file\n    \
    (opens anywhere with `gpuviewer view` — no GPU required). Prints per-table row counts;\n    \
    refuses to overwrite an existing output file:\n  \
      --since <spec>  window start; default 24h (the window always ends now)\n  \
      --db <path>     source history database (default: the recorded default)\n  \
      --mock          read history-mock.db as the source\n\n\
    view — replay a .gpvr file read-only: no backends, no recording, no live mode.\n    \
    q quits; arrows/pgup/pgdn scrub; enter jumps to the selected event\n\n\
    GENERAL:\n  \
      --version, -V   print version and exit\n  \
      --help, -h      show this help";

/// Write `text` to stdout, treating a broken pipe as the consumer hanging up — these
/// streams are built to be piped (`gpuviewer report | head`, `--json | jq`), and a Unix
/// tool ends quietly when its reader goes away, it does not panic (Rust ignores SIGPIPE,
/// so without this every `println!` aborts mid-stream). Returns `false` once stdout is
/// gone so streaming callers can stop ticking and flush their recording tail.
fn emit(text: &str) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .is_ok()
}

/// [`emit`] with a trailing newline — one NDJSON line, one summary line.
fn emit_line(text: &str) -> bool {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}").and_then(|()| out.flush()).is_ok()
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => args.json = true,
            "--once" => args.once = true,
            "--mock" => args.mock = true,
            "--no-backoff" => args.backoff = false,
            "--no-persist" => args.persist = false,
            "--interval" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--interval needs a value (ms)"))?;
                let ms: u64 = v
                    .parse()
                    .map_err(|e| anyhow::anyhow!("--interval: invalid value {v:?}: {e}"))?;
                if ms < 100 {
                    eprintln!("gpuviewer: --interval {ms} clamped to 100ms");
                }
                args.interval = Duration::from_millis(ms.max(100));
            }
            "--db" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--db needs a path"))?;
                args.db = Some(PathBuf::from(v));
            }
            "--on-event" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--on-event needs a command"))?;
                args.on_event = Some(v);
            }
            "--version" | "-V" => {
                emit_line(concat!("gpuviewer ", env!("CARGO_PKG_VERSION")));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                emit_line(HELP);
                std::process::exit(0);
            }
            other => bail!("unknown flag: {other} (see --help)"),
        }
    }
    Ok(args)
}

/// Parse the `report` subcommand's args (already past the `report` token).
fn parse_report_args(mut it: impl Iterator<Item = String>) -> Result<ReportArgs> {
    let mut r = ReportArgs {
        since: None,
        until: None,
        db: None,
        mock: false,
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since" => {
                r.since = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--since needs a spec"))?,
                )
            }
            "--until" => {
                r.until = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--until needs a spec"))?,
                )
            }
            "--db" => {
                r.db = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--db needs a path"))?,
                ))
            }
            "--mock" => r.mock = true,
            "--help" | "-h" => {
                emit_line(HELP);
                std::process::exit(0);
            }
            other => bail!("unknown report flag: {other} (see --help)"),
        }
    }
    Ok(r)
}

/// Parsed `demo` subcommand options.
struct DemoArgs {
    /// Build the database and print the summary, then exit — CI and smoke scripts need the
    /// seeding without the TUI (which wants a tty).
    seed_only: bool,
}

fn parse_demo_args(it: impl Iterator<Item = String>) -> Result<DemoArgs> {
    let mut d = DemoArgs { seed_only: false };
    for a in it {
        match a.as_str() {
            "--seed-only" => d.seed_only = true,
            "--help" | "-h" => {
                emit_line(HELP);
                std::process::exit(0);
            }
            other => bail!("unknown demo flag: {other} (see --help)"),
        }
    }
    Ok(d)
}

/// Parsed `export` subcommand options.
struct ExportArgs {
    since: Option<String>,
    db: Option<PathBuf>,
    mock: bool,
    /// The output `.gpvr` path (positional).
    out: PathBuf,
}

fn parse_export_args(mut it: impl Iterator<Item = String>) -> Result<ExportArgs> {
    let mut since = None;
    let mut db = None;
    let mut mock = false;
    let mut out: Option<PathBuf> = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since" => {
                since = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--since needs a spec"))?,
                )
            }
            "--db" => {
                db = Some(PathBuf::from(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--db needs a path"))?,
                ))
            }
            "--mock" => mock = true,
            "--help" | "-h" => {
                emit_line(HELP);
                std::process::exit(0);
            }
            other if other.starts_with('-') => bail!("unknown export flag: {other} (see --help)"),
            _ => {
                if out.is_some() {
                    bail!("export takes exactly one output path (got {a:?} too)");
                }
                out = Some(PathBuf::from(a));
            }
        }
    }
    Ok(ExportArgs {
        since,
        db,
        mock,
        out: out
            .ok_or_else(|| anyhow::anyhow!("export needs an output path, e.g. incident.gpvr"))?,
    })
}

/// Parse the `view` subcommand's single positional file argument.
fn parse_view_args(it: impl Iterator<Item = String>) -> Result<PathBuf> {
    let mut file: Option<PathBuf> = None;
    for a in it {
        match a.as_str() {
            "--help" | "-h" => {
                emit_line(HELP);
                std::process::exit(0);
            }
            other if other.starts_with('-') => bail!("unknown view flag: {other} (see --help)"),
            _ => {
                if file.is_some() {
                    bail!("view takes exactly one file (got {a:?} too)");
                }
                file = Some(PathBuf::from(a));
            }
        }
    }
    file.ok_or_else(|| anyhow::anyhow!("view needs a file, e.g. gpuviewer view incident.gpvr"))
}

fn main() -> Result<()> {
    // Subcommands dispatch before the flag parser so their own options (which overlap names
    // like --db/--mock) are parsed in the right context.
    let mut raw = std::env::args().skip(1).peekable();
    match raw.peek().map(String::as_str) {
        Some("report") => {
            let _ = raw.next();
            return run_report(parse_report_args(raw)?);
        }
        Some("demo") => {
            let _ = raw.next();
            return run_demo(parse_demo_args(raw)?);
        }
        Some("export") => {
            let _ = raw.next();
            return run_export(parse_export_args(raw)?);
        }
        Some("view") => {
            let _ = raw.next();
            return run_view(parse_view_args(raw)?);
        }
        _ => {}
    }

    let args = parse_args()?;

    // Contamination guard (the mock/--db hole): recording is always-on, and --db can point
    // a --mock session at ANY database — including a user's real history.db. Every recording
    // database carries a real/mock stamp; preflight the explicit --db here so a cross-mode
    // open is refused with one clean error BEFORE the engine writes a single row. Both the
    // TUI and --json paths flow through this. The default paths need no preflight: the
    // engine resolves them per mode through `open_default`, which applies the same stamp
    // rule itself (and mock already gets its own file there).
    let session_source = if args.mock {
        DataSource::Mock
    } else {
        DataSource::Real
    };
    let mut persist = args.persist;
    if persist {
        if let Some(db) = &args.db {
            match SqliteStore::open_recording(db, session_source) {
                // Stamped/verified; drop the handle — the engine reopens it for the session.
                Ok(_) => {}
                // The mismatch is the one error that MUST stop the run: recording would
                // corrupt the file. Anything else (permissions, disk full) keeps the
                // engine's existing best-effort behavior — it retries the open itself and
                // degrades to live-only with a stderr note rather than killing the view.
                Err(e @ StoreError::DataSourceMismatch { .. }) => bail!("{e}"),
                // The duplicate-narration blocker: a second instance recording into the
                // SAME database would double-count every rollup bucket and narrate every
                // event twice. The lock loser keeps WORKING — a monitor that refuses to
                // monitor is worse than one that does not record — so it runs live-only
                // with the reason on stderr (and the footer's "replay needs persistence"
                // hint, the same plumbing --no-persist uses). Deciding here, rather than
                // letting the engine race the holder for its own reopen, keeps the
                // session deterministic even if the other instance exits a moment later.
                // (The default, no---db paths get the same outcome inside the engine: its
                // open fails with the Locked error and it degrades to live-only with the
                // error printed.)
                Err(e @ StoreError::Locked { .. }) => {
                    eprintln!("gpuviewer: {e}");
                    eprintln!(
                        "gpuviewer: continuing live-only — history persistence disabled \
                         for this session"
                    );
                    persist = false;
                }
                Err(_) => {}
            }
        }
    }

    let config = EngineConfig {
        force_mock: args.mock,
        // Persistence is on by default in the live modes (the wedge feature; --no-persist
        // opts out, and losing the instance-lock race above turns it off too). --mock
        // records to a separate file so the demo/CI never pollute real flight history.
        persist,
        db_path: args.db.clone(),
        interval: args.interval,
        on_event: args.on_event.clone(),
    };
    let engine = Engine::new(config);

    // The preflight assumed --mock tells the whole story, but the mock backend is also the
    // automatic fallback when no real GPU is found — that session records simulated data
    // too. With an explicit --db just verified/stamped "real", ticking would contaminate
    // it, so refuse before the first tick (no samples or events have been recorded yet; a
    // default-path session already switched to history-mock.db inside the engine).
    // `persist`, not `args.persist`: a lock loser records nothing, so there is nothing to
    // contaminate — bailing would wrongly kill its live-only session.
    if persist && !args.mock && engine.mock_in_use() {
        if let Some(db) = &args.db {
            bail!(
                "no real GPU found — this session would record mock (simulated) data into {} \
                 (pass --mock with a separate --db, or --no-persist)",
                db.display()
            );
        }
    }

    if args.json {
        run_json(engine, args.interval, args.once)
    } else {
        let collector = Collector::start(engine, args.interval, args.backoff);
        app::App::new(collector).run()
    }
}

/// NDJSON v1 envelopes (docs/spec/ndjson-v1.md; conformance: tests/ndjson_contract.rs).
/// Each stdout line carries `"v":1` and a `"type"` discriminator so consumers can route
/// lines without guessing from field shapes. `"v"` bumps only on breaking change.
#[derive(Serialize)]
struct FrameLine<'a> {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    ts_ms: u64,
    devices: &'a [FrameDevice],
}

/// One line per event, emitted immediately after the frame line of the tick that produced
/// it — events are not embedded in the frame, so `tail`-style consumers can filter by
/// `"type"` alone.
#[derive(Serialize)]
struct EventLine<'a> {
    v: u8,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    event: &'a Event,
}

fn run_json(mut engine: Engine, interval: Duration, once: bool) -> Result<()> {
    loop {
        let frame = engine.tick();
        let mut lines = vec![serde_json::to_string(&FrameLine {
            v: 1,
            kind: "frame",
            ts_ms: frame.ts_ms,
            devices: &frame.devices,
        })?];
        for event in &frame.events {
            lines.push(serde_json::to_string(&EventLine {
                v: 1,
                kind: "event",
                event,
            })?);
        }
        for line in &lines {
            if !emit_line(line) {
                // The consumer hung up (`--json | head` is a normal way to grab a frame).
                // End the run cleanly, persisting the partial tail like any other exit.
                engine.flush();
                return Ok(());
            }
        }
        if once {
            // Persist the partial tail before exiting so a one-shot run still records.
            engine.flush();
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

// ===========================================================================================
// report subcommand — a plain-text (no ANSI) digest of recorded history.
// ===========================================================================================

/// Resolve a `--since`/`--until` SPEC against `now_ms`:
/// - `12h` / `45m` / `7d` → that long before `now`.
/// - `HH:MM` → that clock time *today*; if today's instant is still in the future relative to
///   `now`, it means yesterday (you cannot have recorded a future event).
///
/// `now_ms` is passed in (not read from the clock) so the parser is deterministic in tests.
/// Returns `None` for an unparseable spec so the caller can report a clear error.
fn parse_spec(spec: &str, now_ms: u64) -> Option<u64> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }

    // Relative duration: <digits><unit> where unit ∈ {s, m, h, d}.
    if let Some(unit) = spec.chars().last() {
        if matches!(unit, 's' | 'm' | 'h' | 'd') {
            let num: u64 = spec[..spec.len() - 1].parse().ok()?;
            let secs = match unit {
                's' => num,
                'm' => num.checked_mul(60)?,
                'h' => num.checked_mul(3600)?,
                'd' => num.checked_mul(86_400)?,
                _ => unreachable!(),
            };
            let ms = secs.checked_mul(1000)?;
            return Some(now_ms.saturating_sub(ms));
        }
    }

    // Clock time HH:MM (today, or yesterday if that instant is still in the future).
    let (h, m) = spec.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    use chrono::{Local, TimeZone, Timelike};
    let now = Local.timestamp_millis_opt(now_ms as i64).single()?;
    let today = now
        .with_hour(h)?
        .with_minute(m)?
        .with_second(0)?
        .with_nanosecond(0)?;
    let chosen = if today.timestamp_millis() as u64 > now_ms {
        today - chrono::Duration::days(1)
    } else {
        today
    };
    Some(chosen.timestamp_millis().max(0) as u64)
}

fn run_report(args: ReportArgs) -> Result<()> {
    let now = now_ms();
    let from = match &args.since {
        Some(s) => parse_spec(s, now).ok_or_else(|| {
            anyhow::anyhow!("--since: cannot parse {s:?} (try 12h, 45m, 7d, HH:MM)")
        })?,
        None => now.saturating_sub(24 * 3600 * 1000),
    };
    let to = match &args.until {
        Some(s) => parse_spec(s, now).ok_or_else(|| {
            anyhow::anyhow!("--until: cannot parse {s:?} (try 12h, 45m, 7d, HH:MM)")
        })?,
        None => now,
    };

    // Resolve the database path. --db wins; else the XDG default, with --mock selecting the
    // separate mock file (same rule the writer uses, so report reads what the run recorded).
    let path = match &args.db {
        Some(p) => p.clone(),
        None => default_history_path(args.mock)?,
    };
    if !path.exists() {
        // A friendly, actionable error — not a panic. Exit 1 distinguishes "no file" from
        // "empty store" (exit 0), which a script may want to branch on.
        bail!(
            "no history database at {} — run gpuviewer first to record, or pass --db <path>{}",
            path.display(),
            if args.mock {
                ""
            } else {
                " (use --mock for the simulated run's history)"
            }
        );
    }

    let store = SqliteStore::open_readonly(&path)
        .map_err(|e| anyhow::anyhow!("cannot open {} read-only: {e}", path.display()))?;

    let report = build_report(&store, from, to)?;
    emit(&report);
    Ok(())
}

/// Build the digest text from an already-open store. Split from `run_report` so it can be
/// tested against a store seeded inline (no process spawn, no real history file).
fn build_report(store: &SqliteStore, from: u64, to: u64) -> Result<String> {
    let devices = store
        .devices()
        .map_err(|e| anyhow::anyhow!("reading devices: {e}"))?;
    let events = store
        .events_between(from, to)
        .map_err(|e| anyhow::anyhow!("reading events: {e}"))?;

    // Empty store: say exactly that and let the caller exit 0.
    let no_samples = devices.iter().all(|d| {
        store
            .samples_between(&d.device_id, from, to, Tier::OneMin)
            .map(|v| v.is_empty())
            .unwrap_or(true)
            && store
                .samples_between(&d.device_id, from, to, Tier::TenSec)
                .map(|v| v.is_empty())
                .unwrap_or(true)
    });
    if (devices.is_empty() || no_samples) && events.is_empty() {
        return Ok(format!(
            "gpuviewer report — {} .. {}: no recorded history in this window\n",
            fmt_clock(from),
            fmt_clock(to),
        ));
    }

    let (facts, inferences) = events
        .iter()
        .fold((0u32, 0u32), |(f, l), e| match e.confidence {
            Confidence::Fact => (f + 1, l),
            Confidence::Likely => (f, l + 1),
        });

    let mut out = String::new();
    out.push_str(&format!(
        "gpuviewer report — {} .. {} ({} events: {} facts, {} inferences)\n",
        fmt_clock(from),
        fmt_clock(to),
        events.len(),
        facts,
        inferences,
    ));

    // Per-device summary line. Prefer the 1m tier (the long tail) for the window summary;
    // a wholly-within-48h window may only have 10s rows, so fall back to those.
    out.push('\n');
    let label = device_labels(&devices);
    for d in &devices {
        let mut rows = store
            .samples_between(&d.device_id, from, to, Tier::OneMin)
            .unwrap_or_default();
        if rows.is_empty() {
            rows = store
                .samples_between(&d.device_id, from, to, Tier::TenSec)
                .unwrap_or_default();
        }
        let short = label
            .get(&d.device_id)
            .cloned()
            .unwrap_or_else(|| d.name.clone());
        if rows.is_empty() {
            out.push_str(&format!("{short} ({}): no samples in window\n", d.name));
            continue;
        }
        let util_avg = mean(rows.iter().filter_map(|r| r.util_avg));
        let util_max = rows.iter().filter_map(|r| r.util_max).fold(None, fmax);
        let temp_max = rows.iter().filter_map(|r| r.temp_max_c).fold(None, fmax);
        let mem_max = rows.iter().filter_map(|r| r.mem_avg).max();
        // `throttle_n` counts throttled raw samples WITHIN one bucket; summing it across
        // rows would report sample counts under a "buckets" label (5x off at 1Hz/10s).
        // Count buckets that saw any throttling, which is what the label promises.
        let throttle_buckets = rows.iter().filter(|r| r.throttle_n > 0).count();

        out.push_str(&format!(
            "{short} ({}): util avg {} / max {}, temp max {}, mem max {}, throttle buckets {}\n",
            d.name,
            fmt_pct(util_avg),
            fmt_pct(util_max),
            fmt_temp(temp_max),
            fmt_mem(mem_max),
            throttle_buckets,
        ));
    }

    // Chronological event list.
    out.push('\n');
    if events.is_empty() {
        out.push_str("(no events in window)\n");
    } else {
        for e in &events {
            let sev = match e.severity {
                Severity::Critical => "CRIT",
                Severity::Warning => "WARN",
                Severity::Info => "INFO",
            };
            let conf = match e.confidence {
                Confidence::Fact => "[fact]  ",
                Confidence::Likely => "[likely]",
            };
            // The stored title already begins with the device's short name (the event engine
            // narrates "GPU0 began throttling …"), so we do NOT prefix the label again — that
            // would read "GPU0 GPU0 …". A title that does not name a device (e.g. a collector
            // stall) carries its own context.
            out.push_str(&format!(
                "{}  {sev}  {conf}  {}\n",
                fmt_clock_full(e.ts_ms),
                e.title,
            ));
        }
    }

    // Footer: the retention windows so the reader knows how far back the data can reach.
    out.push('\n');
    out.push_str(&format!(
        "retention: 10s detail kept {}, 1m detail kept {}, events kept {}\n",
        fmt_retain(RETAIN_10S_MS),
        fmt_retain(RETAIN_1M_MS),
        fmt_retain(RETAIN_EVENTS_MS),
    ));
    Ok(out)
}

/// Stable GPU0/GPU1/... short labels in device-id order, matching the live UI's naming.
fn device_labels(
    devices: &[gpuviewer_history::DeviceRow],
) -> std::collections::HashMap<DeviceId, String> {
    devices
        .iter()
        .enumerate()
        .map(|(i, d)| (d.device_id.clone(), format!("GPU{i}")))
        .collect()
}

fn mean(it: impl Iterator<Item = f32>) -> Option<f32> {
    let mut sum = 0.0f64;
    let mut n = 0u32;
    for v in it {
        sum += v as f64;
        n += 1;
    }
    (n > 0).then(|| (sum / n as f64) as f32)
}

/// `Option`-aware float max for `fold`.
fn fmax(acc: Option<f32>, v: f32) -> Option<f32> {
    Some(acc.map_or(v, |a| a.max(v)))
}

fn fmt_pct(v: Option<f32>) -> String {
    v.map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "n/a".into())
}

fn fmt_temp(v: Option<f32>) -> String {
    v.map(|t| format!("{t:.0}\u{00b0}C"))
        .unwrap_or_else(|| "n/a".into())
}

fn fmt_mem(v: Option<u64>) -> String {
    v.map(gpuviewer_core::fmt_bytes)
        .unwrap_or_else(|| "n/a".into())
}

/// Retention window as a coarse human string (only ever 48h / 30d here).
fn fmt_retain(ms: u64) -> String {
    let hours = ms / (3600 * 1000);
    if hours >= 24 && hours.is_multiple_of(24) {
        format!("{}d", hours / 24)
    } else {
        format!("{hours}h")
    }
}

/// Date + time of day for the window endpoints in the header (a multi-day report needs the
/// date, not just HH:MM).
fn fmt_clock(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "????-??-?? ??:??".into())
}

/// HH:MM:SS for an event row.
fn fmt_clock_full(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}

/// The default history path for `report`/`export`, mirroring `SqliteStore::open_default`'s
/// naming so they read the same file the collector wrote. Resolved without opening anything.
fn default_history_path(mock: bool) -> Result<PathBuf> {
    let file = if mock {
        "history-mock.db"
    } else {
        "history.db"
    };
    Ok(default_data_dir()?.join(file))
}

/// `$XDG_DATA_HOME/gpuviewer` or `~/.local/share/gpuviewer`, mirroring the store's own
/// resolution so every subcommand reads/writes where the collector records.
fn default_data_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("gpuviewer"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".local/share/gpuviewer"));
    }
    bail!("no data directory (set $XDG_DATA_HOME or $HOME)")
}

// ===========================================================================================
// demo subcommand — seed 8h of simulated history, then open the TUI already scrolled back.
// ===========================================================================================

/// How much simulated history `demo` seeds, at [`DEMO_STEP_MS`] per simulated tick.
const DEMO_SPAN_MS: u64 = 8 * 3600 * 1000;
const DEMO_STEP_MS: u64 = 1000;

fn run_demo(args: DemoArgs) -> Result<()> {
    // The demo gets its OWN file next to the default path, deleted and recreated on every
    // run: the demo must be reproducible, and it must never touch history.db (nor even the
    // mock file a real `--mock` session may be recording to).
    let path = default_data_dir()?.join("history-demo.db");
    // Probe the instance lock BEFORE deleting: another live session may be recording to
    // this very file (a second `demo`, or `--mock --db .../history-demo.db`), and deleting
    // a recording out from under its writer is the destructive cousin of the audit's
    // duplicate-narration blocker. The probe handle drops immediately and seed_demo
    // re-locks; the instant between is a demo-only race we accept over threading a lock
    // handle through the seeder. Any non-lock error (no file yet, corrupt leftovers) is
    // exactly what the recreate below resolves.
    if let Err(e @ StoreError::Locked { .. }) = SqliteStore::open_recording(&path, DataSource::Mock)
    {
        bail!("{e}");
    }
    let _ = std::fs::remove_file(&path);
    for ext in ["-wal", "-shm"] {
        let mut side = path.as_os_str().to_os_string();
        side.push(ext);
        let _ = std::fs::remove_file(side);
    }

    let now = now_ms();
    let (events, throttles) = seed_demo(&path, now.saturating_sub(DEMO_SPAN_MS), now)?;
    emit_line(&format!(
        "seeded {}h of simulated history: {events} events, {throttles} throttle episodes \
         (db: {})",
        DEMO_SPAN_MS / 3_600_000,
        path.display()
    ));
    if args.seed_only {
        return Ok(());
    }

    // The TUI's first sight is the scroll-back answering "why did it slow down": open in
    // replay at the most recent throttle onset (or, should the seed somehow hold none, at
    // the newest recorded moment — `enter_replay(None)`'s fallback). The live session keeps
    // running underneath with `--mock` semantics, recording onto the same demo file.
    let anchor = SqliteStore::open_readonly(&path)
        .ok()
        .and_then(|s| s.latest_event_ms(Some(EventKind::ThrottleStart)).ok())
        .flatten();
    let interval = Duration::from_millis(1000);
    let engine = Engine::new(EngineConfig {
        force_mock: true,
        persist: true,
        db_path: Some(path),
        interval,
        on_event: None,
    });
    let collector = Collector::start(engine, interval, true);
    let mut app = app::App::new(collector);
    app.enter_replay(anchor);
    app.run()
}

/// Drive the mock simulation from `from_ms` to `to_ms` at 1s steps through the real event
/// engine and recorder — the exact pipeline a live session runs, so the seeded history is
/// indistinguishable from a recording that ran all morning. Returns `(events recorded,
/// throttle episodes among them)`.
fn seed_demo(path: &Path, from_ms: u64, to_ms: u64) -> Result<(usize, usize)> {
    // The demo is simulation — stamp the (freshly recreated) file `mock` so a later real
    // session pointed at it with --db is refused instead of mixing hardware history in.
    let (store, _) = SqliteStore::open_recording(path, DataSource::Mock)
        .map_err(|e| anyhow::anyhow!("cannot create demo database at {}: {e}", path.display()))?;
    let mut rec = Recorder::new(store);
    let mut mock = MockBackend::new();
    let mut engine = EventEngine::new();

    // Register devices first (store rows + GPU0/GPU1 short names), mirroring Engine::new.
    let mut infos = Vec::new();
    for (i, id) in mock.devices().into_iter().enumerate() {
        let info = mock
            .static_info(&id)
            .map_err(|e| anyhow::anyhow!("mock static_info for {id}: {e}"))?;
        rec.store_mut()
            .register_device(&id, &info.name, info.vendor, info.mem_total_bytes)
            .map_err(|e| anyhow::anyhow!("registering {id}: {e}"))?;
        engine.register_device(id, format!("GPU{i}"));
        infos.push(info);
    }

    let mut events = 0usize;
    let mut throttles = 0usize;
    for ts in (from_ms..=to_ms).step_by(DEMO_STEP_MS as usize) {
        let mut tick_events = Vec::new();
        for (id, sample, procs) in &mock.tick_at(ts) {
            // Match by id, not position — a reordered `tick_at` must not cross-wire the
            // per-device thresholds (mem totals, slowdown temps) into the wrong sim.
            let info = infos.iter().find(|i| i.id == *id);
            tick_events.extend(engine.observe(
                id,
                sample,
                procs,
                info.and_then(|i| i.mem_total_bytes),
                info.and_then(|i| i.temp_slowdown_c),
            ));
            rec.observe(id, sample, procs);
        }
        events += tick_events.len();
        throttles += tick_events
            .iter()
            .filter(|e| e.kind == EventKind::ThrottleStart)
            .count();
        rec.record_events(&tick_events);
    }
    // Persist the partial tail buckets so the recording reaches all the way to `to_ms`.
    rec.flush();
    Ok((events, throttles))
}

// ===========================================================================================
// export subcommand — copy a window of history into a standalone, shareable .gpvr file.
// ===========================================================================================

fn run_export(args: ExportArgs) -> Result<()> {
    let now = now_ms();
    let from = match &args.since {
        Some(s) => parse_spec(s, now).ok_or_else(|| {
            anyhow::anyhow!("--since: cannot parse {s:?} (try 12h, 45m, 7d, HH:MM)")
        })?,
        None => now.saturating_sub(24 * 3600 * 1000),
    };

    let src = match &args.db {
        Some(p) => p.clone(),
        None => default_history_path(args.mock)?,
    };
    if !src.exists() {
        bail!(
            "no history database at {} — run gpuviewer first to record, or pass --db <path>",
            src.display()
        );
    }
    if args.out.exists() {
        // The store enforces this too; checking here keeps the message at CLI altitude.
        bail!(
            "refusing to overwrite {} — pick a new output path",
            args.out.display()
        );
    }

    let store = SqliteStore::open_readonly(&src)
        .map_err(|e| anyhow::anyhow!("cannot open {} read-only: {e}", src.display()))?;
    let counts = store
        .export_to(&args.out, from, now)
        .map_err(|e| anyhow::anyhow!("export failed: {e}"))?;

    emit(&format!(
        "exported {} .. {} into {}\n  devices        {}\n  samples_10s    {}\n  \
         samples_1m     {}\n  processes_10s  {}\n  events         {}\n",
        fmt_clock(from),
        fmt_clock(now),
        args.out.display(),
        counts.devices,
        counts.samples_10s,
        counts.samples_1m,
        counts.processes_10s,
        counts.events,
    ));
    Ok(())
}

// ===========================================================================================
// view subcommand — replay a .gpvr file read-only: no backends, no collector, no recorder.
// ===========================================================================================

fn run_view(path: PathBuf) -> Result<()> {
    if !path.exists() {
        bail!(
            "no such file: {} — export one with `gpuviewer export incident.gpvr`",
            path.display()
        );
    }
    let store = SqliteStore::open_readonly(&path)
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
    // Probe the schema up front: junk bytes or someone else's SQLite file should fail with
    // one friendly line here, not deep inside the first replay query.
    let devices = store
        .devices()
        .map_err(|_| anyhow::anyhow!("{} is not a gpuviewer recording (.gpvr)", path.display()))?;
    let infos: Vec<StaticInfo> = devices.into_iter().map(static_info_from_row).collect();
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let collector = Collector::stationary(infos, Some(path.clone()));
    app::App::viewer(collector, store, label).run()
}

/// Rebuild a `StaticInfo` from a recording's device row. Only what the file holds: limits,
/// max clocks, and driver are unknown, so they stay `None` — the gauges render absolute
/// values rather than inventing bounds the recording never carried.
fn static_info_from_row(d: DeviceRow) -> StaticInfo {
    StaticInfo {
        id: d.device_id,
        vendor: d.vendor,
        name: d.name,
        backend: "file".into(),
        mem_total_bytes: d.mem_total_bytes,
        power_limit_mw: None,
        max_sm_clock_mhz: None,
        temp_slowdown_c: None,
        driver_version: None,
        process_hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpuviewer_core::{EventKind, ThrottleReasons, Vendor};
    use gpuviewer_history::{Recorder, SampleRollup, SqliteStore};

    // ---- since/until spec parser ----

    // A fixed "now": 2026-06-07 12:00:00 local. Computed from chrono so the test is
    // timezone-independent (the spec resolves in local time, like the user types it).
    fn fixed_now() -> u64 {
        use chrono::{Local, TimeZone};
        Local
            .with_ymd_and_hms(2026, 6, 7, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as u64
    }

    #[test]
    fn parse_spec_relative_durations() {
        let now = fixed_now();
        assert_eq!(parse_spec("12h", now), Some(now - 12 * 3600 * 1000));
        assert_eq!(parse_spec("45m", now), Some(now - 45 * 60 * 1000));
        assert_eq!(parse_spec("7d", now), Some(now - 7 * 86_400 * 1000));
        assert_eq!(parse_spec("30s", now), Some(now - 30 * 1000));
        // Whitespace is tolerated.
        assert_eq!(parse_spec("  2h ", now), Some(now - 2 * 3600 * 1000));
    }

    #[test]
    fn parse_spec_clock_today_vs_yesterday() {
        let now = fixed_now(); // 12:00 today
                               // An earlier time today resolves to today.
        let nine = parse_spec("09:30", now).unwrap();
        assert!(nine < now);
        assert_eq!(now - nine, (2 * 3600 + 30 * 60) * 1000);
        // A later time (still in the future today) must mean yesterday.
        let twenty = parse_spec("20:00", now).unwrap();
        assert!(twenty < now, "a future clock time means yesterday");
        // 20:00 yesterday is 16h before 12:00 today.
        assert_eq!(now - twenty, 16 * 3600 * 1000);
    }

    #[test]
    fn parse_spec_rejects_garbage() {
        let now = fixed_now();
        assert_eq!(parse_spec("", now), None);
        assert_eq!(parse_spec("12x", now), None);
        assert_eq!(parse_spec("abc", now), None);
        assert_eq!(parse_spec("99:99", now), None); // out-of-range clock
        assert_eq!(parse_spec("25:00", now), None);
        assert_eq!(parse_spec("h", now), None); // no number
    }

    // ---- report formatting against an inline-seeded store ----

    fn scratch_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gpuviewer-report-test-{}-{n}.db",
            std::process::id()
        ))
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        for ext in ["-wal", "-shm"] {
            let mut p = path.as_os_str().to_os_string();
            p.push(ext);
            let _ = std::fs::remove_file(p);
        }
    }

    fn full_sample(
        ts_ms: u64,
        util: f32,
        throttle: ThrottleReasons,
    ) -> gpuviewer_core::DynamicSample {
        gpuviewer_core::DynamicSample {
            ts_ms,
            util_pct: Some(util),
            mem_used_bytes: Some(8 * 1024 * 1024 * 1024),
            power_mw: Some(200_000),
            temp_c: Some(70.0),
            fan_pct: Some(50.0),
            sm_clock_mhz: Some(1800),
            mem_clock_mhz: Some(9000),
            encoder_pct: None,
            decoder_pct: None,
            throttle,
        }
    }

    #[test]
    fn report_digest_has_header_summary_and_events() {
        let path = scratch_path();
        let (store, _) = SqliteStore::open(&path).unwrap();
        let dev = DeviceId("0000:01:00.0".into());
        // Seed via the real Recorder so the rollup path is exercised end to end.
        let mut rec = Recorder::new(store);
        rec.store_mut()
            .register_device(&dev, "Test GPU 9000", Vendor::Nvidia, Some(24 << 30))
            .unwrap();

        let thermal = ThrottleReasons {
            thermal: true,
            ..Default::default()
        };
        // Four frames in bucket [0,10s); TWO throttled — one bucket with throttling, so a
        // summary that sums per-bucket sample counts instead of counting buckets reads 2,
        // not 1. Then a frame in the next bucket to force the completed bucket to flush.
        rec.observe(&dev, &full_sample(1_000, 40.0, Default::default()), &[]);
        rec.observe(&dev, &full_sample(2_000, 80.0, thermal), &[]);
        rec.observe(&dev, &full_sample(3_000, 45.0, thermal), &[]);
        rec.observe(&dev, &full_sample(9_000, 60.0, Default::default()), &[]);
        rec.observe(&dev, &full_sample(11_000, 5.0, Default::default()), &[]);
        rec.flush();

        rec.record_events(&[
            Event {
                ts_ms: 5_000,
                device: dev.clone(),
                kind: EventKind::ThrottleStart,
                severity: Severity::Warning,
                confidence: Confidence::Fact,
                title: "GPU0 began throttling (thermal) — clocks 2520->1815 MHz".into(),
                evidence: "throttle bits: [thermal]; 84C".into(),
            },
            Event {
                ts_ms: 6_000,
                device: dev.clone(),
                kind: EventKind::VramPressure,
                severity: Severity::Warning,
                confidence: Confidence::Likely,
                title: "GPU0 VRAM 90% and climbing — likely full in ~5 min".into(),
                evidence: "slope".into(),
            },
        ]);

        let text = build_report(rec.store(), 0, 20_000).unwrap();

        // Header counts both events and splits fact vs inference.
        assert!(
            text.contains("1 facts, 1 inferences"),
            "header miscounts confidence:\n{text}"
        );
        assert!(
            text.starts_with("gpuviewer report —"),
            "missing header:\n{text}"
        );
        assert!(
            text.contains("GPU0 (Test GPU 9000)"),
            "device summary missing:\n{text}"
        );
        // 1m tier folds all five frames in the minute: (40+80+45+60+5)/5 = 46 → 46%. A broken
        // average (e.g. over bucket count vs present-count, or wrong tier) would not read 46.
        assert!(text.contains("util avg 46%"), "util average wrong:\n{text}");
        assert!(
            text.contains("util avg 46% / max 80%"),
            "util max wrong:\n{text}"
        );
        // One bucket had throttling (two throttled frames within it). Summing the per-bucket
        // sample counts would read 2 — the decoy that catches a count/sum mixup.
        assert!(
            text.contains("throttle buckets 1"),
            "throttle bucket count wrong:\n{text}"
        );
        // Event rows: a fact plainly, an inference flagged [likely].
        assert!(
            text.contains("WARN  [fact]") && text.contains("began throttling (thermal)"),
            "fact event row missing/mislabeled:\n{text}"
        );
        assert!(
            text.contains("[likely]") && text.contains("likely full in"),
            "inference row must be marked [likely]:\n{text}"
        );
        // Footer mentions retention.
        assert!(
            text.contains("retention:"),
            "missing retention footer:\n{text}"
        );

        drop(rec);
        cleanup(&path);
    }

    #[test]
    fn report_empty_store_says_so() {
        let path = scratch_path();
        let (store, _) = SqliteStore::open(&path).unwrap();
        let text = build_report(&store, 0, 1_000_000).unwrap();
        assert!(
            text.contains("no recorded history in this window"),
            "empty store must say so exactly:\n{text}"
        );
        drop(store);
        cleanup(&path);
    }

    /// Touch the SampleRollup import so the seeded-store helpers compile against the public
    /// type even if a future refactor stops using it directly above.
    #[allow(dead_code)]
    fn _assert_rollup_type(_: SampleRollup) {}
}
