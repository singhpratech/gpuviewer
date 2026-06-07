//! NDJSON v1 conformance suite — this test IS the conformance suite that
//! docs/spec/ndjson-v1.md references. It runs the real built binary (not the library)
//! against the mock backend, so what it asserts is exactly what a `gpuviewer --json`
//! consumer sees on the wire. If the spec and this file ever disagree, one of them is
//! wrong and the release is blocked until they agree again.

use std::process::Command;

use serde_json::Value;

/// Every event `kind` documented in docs/spec/ndjson-v1.md — all emitted today.
/// A kind on the wire that is not in this list is a spec violation, not a new feature.
const DOCUMENTED_KINDS: &[&str] = &[
    "throttle_start",
    "throttle_end",
    "process_attached",
    "process_exited",
    "vram_pressure",
    "idle_gap",
    "collector_stall",
    "history_reset",
    "hang_suspected",
    "cpu_spillover",
    "device_lost",
    "device_returned",
    "recording_started",
    "recording_stopped",
];

const DOCUMENTED_SEVERITIES: &[&str] = &["info", "warning", "critical"];
const DOCUMENTED_PROCESS_KINDS: &[&str] = &["compute", "graphics", "both", "unknown"];

/// Run `gpuviewer --json --once --mock` and return stdout split into lines.
/// XDG_DATA_HOME/HOME point at a scratch dir on the child process only: persistence is on
/// by default, and a conformance test must never write into the user's real data dir.
fn run_once() -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("gpuviewer-ndjson-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch data dir");
    let out = Command::new(env!("CARGO_BIN_EXE_gpuviewer"))
        .args(["--json", "--once", "--mock"])
        .env("XDG_DATA_HOME", &dir)
        .env("HOME", &dir)
        .output()
        .expect("failed to spawn the gpuviewer binary");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "--json --once --mock must exit 0, got {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("stdout must be UTF-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn assert_frame_device(dev: &Value) {
    assert!(dev["id"].is_string(), "device id must be a string: {dev}");
    assert!(
        dev["name"].is_string(),
        "device name must be a string: {dev}"
    );
    // The mock always answers, so a null sample here means the envelope dropped it.
    let sample = dev["sample"]
        .as_object()
        .unwrap_or_else(|| panic!("mock device sample must be an object: {dev}"));
    assert!(
        sample["util_pct"].is_number(),
        "mock util_pct must be a number (the mock always provides it): {sample:?}"
    );
    let procs = dev["processes"]
        .as_array()
        .unwrap_or_else(|| panic!("processes must be an array: {dev}"));
    assert!(!procs.is_empty(), "mock devices always have processes");
    for p in procs {
        assert!(p["pid"].is_u64(), "pid must be an unsigned integer: {p}");
        assert!(p["name"].is_string(), "process name must be a string: {p}");
        let kind = p["kind"]
            .as_str()
            .unwrap_or_else(|| panic!("process kind must be a string: {p}"));
        assert!(
            DOCUMENTED_PROCESS_KINDS.contains(&kind),
            "undocumented process kind {kind:?}"
        );
        // Additive v1 fields: the keys must be present (null when unknown), or old
        // consumers checking `"cpu_pct" in proc` would see the field flicker.
        for key in ["mem_bytes", "util_pct", "cpu_pct", "container"] {
            assert!(
                p.get(key).is_some(),
                "process field {key:?} must be present (null is fine): {p}"
            );
        }
    }
}

fn assert_event_line(v: &Value) {
    assert_eq!(v["v"], 1, "event line must carry v:1: {v}");
    for key in [
        "ts_ms",
        "device",
        "kind",
        "severity",
        "confidence",
        "title",
        "evidence",
    ] {
        assert!(
            v.get(key).is_some() && !v[key].is_null(),
            "event field {key:?} is required and non-null: {v}"
        );
    }
    assert!(v["ts_ms"].as_u64().unwrap_or(0) > 0, "event ts_ms > 0: {v}");
    assert!(
        v["device"].is_string(),
        "event device must be a string: {v}"
    );
    assert!(v["title"].is_string() && v["evidence"].is_string());

    let kind = v["kind"].as_str().expect("event kind must be a string");
    assert!(
        DOCUMENTED_KINDS.contains(&kind),
        "event kind {kind:?} is not documented in docs/spec/ndjson-v1.md"
    );
    let severity = v["severity"].as_str().expect("severity must be a string");
    assert!(
        DOCUMENTED_SEVERITIES.contains(&severity),
        "undocumented severity {severity:?}"
    );
    let confidence = v["confidence"]
        .as_str()
        .expect("confidence must be a string");
    assert!(
        confidence == "fact" || confidence == "likely",
        "confidence must be \"fact\" or \"likely\", got {confidence:?}"
    );
    // The honesty contract on the wire: an inference must read as hedged.
    if confidence == "likely" {
        let title = v["title"].as_str().unwrap_or_default();
        assert!(
            title.contains("likely"),
            "a \"likely\" event must hedge in its title: {title:?}"
        );
    }
}

#[test]
fn json_once_emits_one_conformant_frame_then_only_events() {
    let lines = run_once();
    assert!(
        !lines.is_empty(),
        "--json --once must print at least 1 line"
    );

    let parsed: Vec<Value> = lines
        .iter()
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("every stdout line must be JSON ({e}): {l}"))
        })
        .collect();

    // Line 0 is the frame line for the tick.
    let frame = &parsed[0];
    assert_eq!(frame["v"], 1, "frame line must carry v:1");
    assert_eq!(frame["type"], "frame", "line 0 must be the frame line");
    assert!(
        frame["ts_ms"].as_u64().unwrap_or(0) > 0,
        "frame ts_ms must be a positive integer"
    );
    assert!(
        frame.get("events").is_none(),
        "events were removed from the frame line in v1 — they are separate lines"
    );
    let devices = frame["devices"]
        .as_array()
        .expect("frame devices must be an array");
    assert_eq!(
        devices.len(),
        2,
        "the mock backend registers exactly 2 GPUs"
    );
    for dev in devices {
        assert_frame_device(dev);
    }

    // Everything after the frame is this tick's events (often zero device-derived ones
    // on a single tick: the first observation suppresses the attach-flood by design).
    for v in &parsed[1..] {
        assert_eq!(
            v["type"], "event",
            "with --once, every line after the frame must be an event: {v}"
        );
        assert_event_line(v);
    }

    // Session-boundary marks: a --once run is a complete recording session (persistence
    // is on, pointed at the scratch dir), so its start mark rides the first tick's
    // events and its stop mark is the stream's FINAL line — the spec's documented
    // emission choice for `recording_stopped` while stdout is still open.
    let kinds: Vec<&str> = parsed[1..]
        .iter()
        .filter_map(|v| v["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"recording_started"),
        "the session's start mark must be on the stream: {kinds:?}"
    );
    assert_eq!(
        parsed.last().unwrap()["kind"],
        "recording_stopped",
        "the stop mark must be the stream's final line on a --once run: {kinds:?}"
    );
}
