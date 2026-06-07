//! gpuviewer-core — telemetry collection, data model, and event derivation.
//!
//! Architecture (see CLAUDE.md and docs/research/04-synthesis.md):
//! - [`backend::GpuBackend`]: nvtop's vendor-vtable split (static / dynamic / processes),
//!   per-field `Option<T>`, runtime-loaded vendor libs, failed init = skipped backend.
//! - [`events::EventEngine`]: derives the narrated "story" events from raw samples.
//! - [`mock::MockBackend`]: scripted simulation for CI/demos; the contract for real backends.

#[cfg(target_os = "linux")]
pub mod amd;
pub mod backend;
pub mod events;
#[cfg(target_os = "linux")]
pub mod intel;
pub mod mock;
pub mod model;
#[cfg(all(feature = "nvidia", any(target_os = "linux", target_os = "windows")))]
pub mod nvidia;
#[cfg(target_os = "linux")]
pub mod proc_meta;

pub use backend::{all_backends, BackendError, GpuBackend};
pub use events::{Confidence, Event, EventEngine, EventKind, Severity};
pub use model::{
    fmt_bytes, now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo,
    ThrottleReasons, Vendor,
};
#[cfg(target_os = "linux")]
pub use proc_meta::{container_of, parse_cgroup, CpuTracker};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_backend_produces_samples_and_processes() {
        let mut b = mock::MockBackend::new();
        let devs = b.devices();
        assert_eq!(devs.len(), 2);
        for d in &devs {
            let info = b.static_info(d).unwrap();
            assert!(info.mem_total_bytes.is_some());
            let s = b.refresh_dynamic(d).unwrap();
            assert!(s.util_pct.is_some());
            let procs = b.refresh_processes(d).unwrap();
            assert!(!procs.is_empty());
        }
    }

    #[test]
    fn event_engine_emits_throttle_and_process_events() {
        let mut b = mock::MockBackend::new();
        let devs = b.devices();
        let mut engine = EventEngine::new();
        for (i, d) in devs.iter().enumerate() {
            engine.register_device(d.clone(), format!("GPU{i}"));
        }
        let infos: Vec<StaticInfo> = devs.iter().map(|d| b.static_info(d).unwrap()).collect();

        let mut kinds = std::collections::HashSet::new();
        // Run enough simulated ticks to cross a throttle cycle and an ollama attach/exit.
        for _ in 0..400 {
            for (d, info) in devs.iter().zip(&infos) {
                let s = b.refresh_dynamic(d).unwrap();
                let p = b.refresh_processes(d).unwrap();
                for e in engine.observe(d, &s, &p, info.mem_total_bytes, info.temp_slowdown_c) {
                    kinds.insert(e.kind);
                }
            }
        }
        assert!(
            kinds.contains(&EventKind::ThrottleStart),
            "no throttle event in 400 ticks"
        );
        assert!(
            kinds.contains(&EventKind::ProcessAttached),
            "no attach event in 400 ticks"
        );
        assert!(
            kinds.contains(&EventKind::ProcessExited),
            "no exit event in 400 ticks"
        );
    }

    #[test]
    fn first_process_observation_does_not_flood_events() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![ProcessSample {
            pid: 1,
            name: "preexisting".into(),
            kind: ProcessKind::Compute,
            mem_bytes: Some(1024),
            util_pct: None,
            cpu_pct: None,
            container: None,
        }];
        let sample = DynamicSample {
            ts_ms: 1000,
            util_pct: Some(50.0),
            mem_used_bytes: Some(1024),
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            throttle: ThrottleReasons::default(),
        };
        let events = engine.observe(&dev, &sample, &procs, Some(1 << 30), None);
        assert!(
            events.is_empty(),
            "first observation must not narrate preexisting processes"
        );
    }

    /// "recovered" is only honest when clocks are actually back near pre-throttle levels;
    /// a throttle that ends because the GPU went idle must not claim recovery.
    #[test]
    fn throttle_end_narrates_recovery_honestly() {
        fn sample(ts_ms: u64, sm_clock_mhz: u32, thermal: bool) -> DynamicSample {
            DynamicSample {
                ts_ms,
                util_pct: Some(90.0),
                mem_used_bytes: Some(1 << 30),
                power_mw: None,
                temp_c: Some(80.0),
                fan_pct: None,
                sm_clock_mhz: Some(sm_clock_mhz),
                mem_clock_mhz: None,
                encoder_pct: None,
                decoder_pct: None,
                throttle: ThrottleReasons {
                    thermal,
                    ..Default::default()
                },
            }
        }
        let run = |end_clock: u32| -> Event {
            let mut engine = EventEngine::new();
            let dev = DeviceId("test".into());
            engine.observe(&dev, &sample(1_000, 2400, false), &[], Some(1 << 34), None);
            engine.observe(&dev, &sample(2_000, 1800, true), &[], Some(1 << 34), None);
            let events = engine.observe(
                &dev,
                &sample(3_000, end_clock, false),
                &[],
                Some(1 << 34),
                None,
            );
            assert_eq!(events.len(), 1, "throttle end must emit exactly one event");
            assert_eq!(events[0].kind, EventKind::ThrottleEnd);
            events[0].clone()
        };

        // Clocks back at ≥90% of pre-throttle (2400): honest to say "recovered".
        let recovered = run(2350);
        assert!(
            recovered.evidence.contains("recovered to 2350 MHz"),
            "expected recovery narration, got: {}",
            recovered.evidence
        );

        // Throttle ended at idle clocks: claiming recovery would be a lie.
        let idle_end = run(300);
        assert!(
            !idle_end.evidence.contains("recovered"),
            "must not claim recovery at idle clocks, got: {}",
            idle_end.evidence
        );
        assert!(
            idle_end.evidence.contains("300 MHz") && idle_end.evidence.contains("2400 MHz"),
            "must state current and pre-throttle clocks, got: {}",
            idle_end.evidence
        );
    }

    /// A sharp VRAM drop (allocator reset / process exit) must restart the trend window —
    /// an endpoint slope over a sawtooth understates the current climb rate.
    #[test]
    fn vram_window_resets_on_sharp_drop() {
        let total: u64 = 16 << 30;
        let mk = |ts_ms: u64, used: u64| DynamicSample {
            ts_ms,
            util_pct: Some(90.0),
            mem_used_bytes: Some(used),
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            throttle: ThrottleReasons::default(),
        };
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        // Climb to just under the pressure threshold, then drop sharply (>5% of total).
        let mut ts = 0u64;
        let mut used = 8 << 30;
        for _ in 0..70 {
            engine.observe(&dev, &mk(ts, used), &[], Some(total), None);
            ts += 1000;
            used += 64 << 20; // +64 MiB/s
        }
        engine.observe(&dev, &mk(ts, 4 << 30), &[], Some(total), None);
        ts += 1000;

        // Immediately at high usage again: window restarted, so the minimum span is not
        // yet met and no event may fire off the stale pre-drop trend.
        let events = engine.observe(
            &dev,
            &mk(ts, (total as f64 * 0.9) as u64),
            &[],
            Some(total),
            None,
        );
        assert!(
            events.is_empty(),
            "no pressure event may fire from a stale window after a sharp drop: {:?}",
            events.iter().map(|e| &e.title).collect::<Vec<_>>()
        );
    }

    /// With every process size unknown (WSL2, unprivileged fdinfo) the pressure narration
    /// must not crown an arbitrary process "largest holder" on zero evidence — and must
    /// still name one as soon as a real size is known.
    #[test]
    fn vram_pressure_holder_requires_known_size() {
        let total: u64 = 16 << 30;
        let mk = |ts_ms: u64, used: u64| DynamicSample {
            ts_ms,
            util_pct: Some(90.0),
            mem_used_bytes: Some(used),
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            throttle: ThrottleReasons::default(),
        };
        let proc_named = |pid: u32, name: &str, mem: Option<u64>| ProcessSample {
            pid,
            name: name.into(),
            kind: ProcessKind::Compute,
            mem_bytes: mem,
            util_pct: None,
            cpu_pct: None,
            container: None,
        };

        // Climb from 86% toward 95% at ~600 MiB/min; the pressure event fires once the
        // 60 s minimum trend span is met. Returns the first pressure narration.
        let run = |mem_a: Option<u64>, mem_b: Option<u64>| -> Event {
            let mut engine = EventEngine::new();
            let dev = DeviceId("test".into());
            let procs = vec![proc_named(1, "alpha", mem_a), proc_named(2, "beta", mem_b)];
            let mut ts = 0u64;
            let mut used = (total as f64 * 0.86) as u64;
            let mut hits = Vec::new();
            for _ in 0..=13 {
                let events = engine.observe(&dev, &mk(ts, used), &procs, Some(total), None);
                hits.extend(
                    events
                        .into_iter()
                        .filter(|e| e.kind == EventKind::VramPressure),
                );
                ts += 10_000;
                used += 100 << 20;
            }
            assert!(!hits.is_empty(), "pressure event never fired");
            hits.remove(0)
        };

        let unknown = run(None, None);
        assert!(
            !unknown.title.contains("largest holder"),
            "must not name a holder when every process size is unknown: {}",
            unknown.title
        );

        let known = run(None, Some(2 << 30));
        assert!(
            known.title.contains("largest holder: beta pid 2"),
            "must name the process whose size is known: {}",
            known.title
        );
    }

    // ---- idle-gap (training stall) tests: synthetic 1 Hz traces, controlled ts_ms ----

    fn idle_sample(ts_ms: u64, util_pct: f32) -> DynamicSample {
        DynamicSample {
            ts_ms,
            util_pct: Some(util_pct),
            mem_used_bytes: Some(8 << 30),
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

    fn python_proc() -> ProcessSample {
        ProcessSample {
            pid: 4521,
            name: "python".into(),
            kind: ProcessKind::Compute,
            mem_bytes: Some(6 << 30),
            util_pct: None,
            cpu_pct: None,
            container: None,
        }
    }

    /// Drive a 1 Hz constant-util trace through the engine; returns every event emitted.
    fn drive(
        engine: &mut EventEngine,
        dev: &DeviceId,
        ts_range: std::ops::RangeInclusive<u64>,
        util_pct: f32,
        procs: &[ProcessSample],
    ) -> Vec<Event> {
        let mut out = Vec::new();
        for ts in ts_range.step_by(1000) {
            out.extend(engine.observe(
                dev,
                &idle_sample(ts, util_pct),
                procs,
                Some(16 << 30),
                None,
            ));
        }
        out
    }

    /// A 14s trough after 30s of sustained activity, with python attached throughout,
    /// narrates exactly one IdleGap when util recovers — hedged, with raw evidence.
    #[test]
    fn idle_gap_fires_once_on_recovery_and_is_hedged() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        engine.register_device(dev.clone(), "GPU0".into());
        let procs = vec![python_proc()];

        let active = drive(&mut engine, &dev, 0..=30_000, 92.0, &procs);
        let during = drive(&mut engine, &dev, 31_000..=44_000, 2.0, &procs);
        assert!(
            active
                .iter()
                .chain(&during)
                .all(|e| e.kind != EventKind::IdleGap),
            "the gap must not narrate before it ends (its duration is unknown)"
        );

        let recovery = drive(&mut engine, &dev, 45_000..=45_000, 93.0, &procs);
        let gaps: Vec<&Event> = recovery
            .iter()
            .filter(|e| e.kind == EventKind::IdleGap)
            .collect();
        assert_eq!(gaps.len(), 1, "exactly one IdleGap on recovery");
        let e = gaps[0];
        assert_eq!(e.confidence, Confidence::Likely, "a stall is an inference");
        assert_eq!(e.severity, Severity::Info);
        assert_eq!(
            e.ts_ms, 45_000,
            "event is stamped at gap end, from sample.ts_ms"
        );
        assert!(
            e.title.contains("GPU0 sat idle 14s") && e.title.contains("python (pid 4521)"),
            "title must name the span and the holder, got: {}",
            e.title
        );
        assert!(
            e.title.contains("likely"),
            "inference must hedge: {}",
            e.title
        );
        assert!(
            e.evidence.contains("92%")
                && e.evidence.contains("31000..45000")
                && e.evidence.contains("pid 4521")
                && e.evidence.contains("6.0 GiB"),
            "evidence must carry the raw numbers, got: {}",
            e.evidence
        );

        // Continued activity must not replay the gap.
        let after = drive(&mut engine, &dev, 46_000..=60_000, 93.0, &procs);
        assert!(after.iter().all(|e| e.kind != EventKind::IdleGap));
    }

    /// A 5s trough is a normal scheduling hiccup, not a stall — below the floor, silent.
    #[test]
    fn idle_gap_below_minimum_duration_is_silent() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![python_proc()];

        let mut all = drive(&mut engine, &dev, 0..=30_000, 92.0, &procs);
        all.extend(drive(&mut engine, &dev, 31_000..=35_000, 2.0, &procs));
        all.extend(drive(&mut engine, &dev, 36_000..=40_000, 93.0, &procs));
        assert!(
            all.iter().all(|e| e.kind != EventKind::IdleGap),
            "a 5s gap is below IDLE_GAP_MIN_MS and must not narrate"
        );
    }

    /// A trough with no process attached is just an idle GPU — narrating a "stall"
    /// with nobody there to stall would be confidently wrong.
    #[test]
    fn idle_gap_without_attached_process_is_silent() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        let mut all = drive(&mut engine, &dev, 0..=30_000, 92.0, &[]);
        all.extend(drive(&mut engine, &dev, 31_000..=44_000, 2.0, &[]));
        all.extend(drive(&mut engine, &dev, 45_000..=50_000, 93.0, &[]));
        assert!(
            all.iter().all(|e| e.kind != EventKind::IdleGap),
            "no holder process means no stall narration"
        );
    }

    /// If the big-memory holder exits mid-gap, the process_exited fact already tells
    /// the story; an IdleGap on top of it would double-count the same incident.
    #[test]
    fn idle_gap_suppressed_when_holder_exits_mid_gap() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![python_proc()];

        let mut all = drive(&mut engine, &dev, 0..=30_000, 92.0, &procs);
        all.extend(drive(&mut engine, &dev, 31_000..=36_000, 2.0, &procs));
        // python exits with the gap still open; the rest of the gap runs holder-less.
        all.extend(drive(&mut engine, &dev, 37_000..=44_000, 2.0, &[]));
        all.extend(drive(&mut engine, &dev, 45_000..=50_000, 93.0, &[]));

        assert!(
            all.iter().any(|e| e.kind == EventKind::ProcessExited),
            "the exit itself must still be narrated as a fact"
        );
        assert!(
            all.iter().all(|e| e.kind != EventKind::IdleGap),
            "holder exited mid-gap: the exit event covers it, IdleGap must stay silent"
        );
    }

    /// Without ≥30s of sustained activity beforehand, a trough is not a training stall
    /// (a GPU that was barely working cannot "stall").
    #[test]
    fn idle_gap_requires_prior_sustained_activity() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![python_proc()];

        // Only 10s of activity: never qualifies as "active".
        let mut all = drive(&mut engine, &dev, 0..=10_000, 92.0, &procs);
        all.extend(drive(&mut engine, &dev, 11_000..=24_000, 2.0, &procs));
        all.extend(drive(&mut engine, &dev, 25_000..=30_000, 93.0, &procs));
        assert!(
            all.iter().all(|e| e.kind != EventKind::IdleGap),
            "no sustained activity before the trough — no stall to narrate"
        );
    }

    // ---- hang-suspicion and CPU-spillover tests: synthetic 1 Hz traces, controlled ts_ms ----

    /// A sample whose util may be absent — used to exercise the "util goes None" reset paths.
    fn opt_sample(ts_ms: u64, util_pct: Option<f32>) -> DynamicSample {
        DynamicSample {
            ts_ms,
            util_pct,
            mem_used_bytes: Some(8 << 30),
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

    /// Build a process holding `mem` bytes, with optional self-util and CPU%.
    fn proc_with(
        pid: u32,
        name: &str,
        mem: u64,
        util_pct: Option<f32>,
        cpu_pct: Option<f32>,
    ) -> ProcessSample {
        ProcessSample {
            pid,
            name: name.into(),
            kind: ProcessKind::Compute,
            mem_bytes: Some(mem),
            util_pct,
            cpu_pct,
            container: None,
        }
    }

    /// Drive a 1 Hz trace with an optional util value, holding `procs` constant across it.
    fn drive_opt(
        engine: &mut EventEngine,
        dev: &DeviceId,
        ts_range: std::ops::RangeInclusive<u64>,
        util_pct: Option<f32>,
        procs: &[ProcessSample],
    ) -> Vec<Event> {
        let mut out = Vec::new();
        for ts in ts_range.step_by(1000) {
            out.extend(engine.observe(dev, &opt_sample(ts, util_pct), procs, Some(16 << 30), None));
        }
        out
    }

    /// A 6 GiB holder whose own util is unreported (the normal hung-kernel case): VRAM held,
    /// device dead, holder alive for ten unbroken minutes narrates exactly one HangSuspected,
    /// stamped at the 10-minute mark — not a second early — and hedged with raw evidence.
    #[test]
    fn hang_fires_once_exactly_at_ten_minutes() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        engine.register_device(dev.clone(), "GPU0".into());
        let procs = vec![proc_with(4521, "python", 6 << 30, None, None)];

        // Episode opens at ts=0. At 9m59s (599_000) it has not yet reached HANG_MIN_MS.
        let before = drive_opt(&mut engine, &dev, 0..=599_000, Some(1.0), &procs);
        assert!(
            before.iter().all(|e| e.kind != EventKind::HangSuspected),
            "must not fire at 9m59s — that is below HANG_MIN_MS"
        );

        // One more tick lands exactly on 10m: fires once.
        let at_ten = drive_opt(&mut engine, &dev, 600_000..=600_000, Some(1.0), &procs);
        let hangs: Vec<&Event> = at_ten
            .iter()
            .filter(|e| e.kind == EventKind::HangSuspected)
            .collect();
        assert_eq!(hangs.len(), 1, "exactly one HangSuspected at the 10m mark");
        let e = hangs[0];
        assert_eq!(e.confidence, Confidence::Likely, "a hang is an inference");
        assert_eq!(e.severity, Severity::Warning);
        assert_eq!(e.ts_ms, 600_000, "stamped at the 10-minute mark");
        assert!(
            e.title.contains("likely hung")
                && e.title.contains("python (pid 4521)")
                && e.title.contains("6.0 GiB"),
            "title must hedge and name the holder/mem, got: {}",
            e.title
        );
        assert!(
            e.evidence.contains("0..600000")
                && e.evidence.contains("pid 4521")
                && e.evidence.contains("6.0 GiB"),
            "evidence must carry the raw window and holder, got: {}",
            e.evidence
        );

        // A sustained hang narrates once, never again.
        let after = drive_opt(&mut engine, &dev, 601_000..=900_000, Some(1.0), &procs);
        assert!(
            after.iter().all(|e| e.kind != EventKind::HangSuspected),
            "a single episode must narrate exactly once"
        );
    }

    /// Util returning to life before the 10-minute mark resets the episode: no hang.
    #[test]
    fn hang_does_not_fire_when_util_recovers_early() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![proc_with(4521, "python", 6 << 30, None, None)];

        // Nine minutes of apparent death, then the GPU wakes up — episode dropped.
        let mut all = drive_opt(&mut engine, &dev, 0..=539_000, Some(1.0), &procs);
        all.extend(drive_opt(
            &mut engine,
            &dev,
            540_000..=545_000,
            Some(93.0),
            &procs,
        ));
        // Back to idle, but only briefly — nowhere near a fresh 10 minutes.
        all.extend(drive_opt(
            &mut engine,
            &dev,
            546_000..=560_000,
            Some(1.0),
            &procs,
        ));
        assert!(
            all.iter().all(|e| e.kind != EventKind::HangSuspected),
            "recovery before 10m must reset the episode — no hang"
        );
    }

    /// The anchored holder exiting at 5 minutes ends the episode silently; the exit itself
    /// is still narrated as a plain fact (the two narrations must not collide).
    #[test]
    fn hang_does_not_fire_when_holder_exits_and_exit_still_narrates() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![proc_with(4521, "python", 6 << 30, None, None)];

        let mut all = drive_opt(&mut engine, &dev, 0..=300_000, Some(1.0), &procs);
        // python exits at 5m; the device stays dead for another 10 minutes holder-less.
        all.extend(drive_opt(
            &mut engine,
            &dev,
            301_000..=900_000,
            Some(1.0),
            &[],
        ));

        assert!(
            all.iter().any(|e| e.kind == EventKind::ProcessExited),
            "the holder's exit must still be narrated as a fact"
        );
        assert!(
            all.iter().all(|e| e.kind != EventKind::HangSuspected),
            "anchored holder exited — no hang to claim"
        );
    }

    /// Util going unobservable mid-window drops the episode: we cannot claim "zero engine
    /// activity" through a blind spot. A short re-idle afterwards must not reach 10m.
    #[test]
    fn hang_does_not_fire_when_util_goes_none_midwindow() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        let procs = vec![proc_with(4521, "python", 6 << 30, None, None)];

        let mut all = drive_opt(&mut engine, &dev, 0..=300_000, Some(1.0), &procs);
        // Util unobservable for a stretch — no claim possible.
        all.extend(drive_opt(
            &mut engine,
            &dev,
            301_000..=320_000,
            None,
            &procs,
        ));
        // Idle resumes, but the window restarted and the test span is far short of 10m.
        all.extend(drive_opt(
            &mut engine,
            &dev,
            321_000..=360_000,
            Some(1.0),
            &procs,
        ));
        assert!(
            all.iter().all(|e| e.kind != EventKind::HangSuspected),
            "a blind spot mid-window must reset the episode — no hang"
        );
    }

    /// A hang is an idle gap that never recovered. When activity finally returns after a
    /// 12-minute trough that already narrated a hang, the IdleGap must stay silent — one
    /// incident, one narration.
    #[test]
    fn hang_suppresses_the_idle_gap_it_lived_in() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        engine.register_device(dev.clone(), "GPU0".into());
        let procs = vec![proc_with(4521, "python", 6 << 30, None, None)];

        // 30s+ of real work makes the device idle-gap-eligible, then it drops dead.
        let mut all = drive(&mut engine, &dev, 0..=40_000, 92.0, &procs);
        // A 12-minute trough at ~1% — long enough to both open an idle gap and trip a hang.
        all.extend(drive_opt(
            &mut engine,
            &dev,
            41_000..=761_000,
            Some(1.0),
            &procs,
        ));
        // Activity returns: the idle gap closes here, and would normally narrate.
        all.extend(drive(&mut engine, &dev, 762_000..=765_000, 93.0, &procs));

        let hangs = all
            .iter()
            .filter(|e| e.kind == EventKind::HangSuspected)
            .count();
        let gaps = all.iter().filter(|e| e.kind == EventKind::IdleGap).count();
        assert_eq!(hangs, 1, "the trough must narrate exactly one hang");
        assert_eq!(
            gaps, 0,
            "the hang already covered this trough — the IdleGap must be suppressed"
        );
    }

    /// The textbook spillover: an 11.3 GiB model attaches, the GPU stays near-idle for the
    /// whole 90s window while the process pegs ~3 cores. Fires once, hedged, with means.
    #[test]
    fn spillover_fires_on_textbook_case() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());
        engine.register_device(dev.clone(), "GPU0".into());

        let mem = 12_133_000_000u64; // ~11.3 GiB
                                     // First observation establishes the baseline (no procs) so the model reads as NEW.
        drive_opt(&mut engine, &dev, 0..=0, Some(3.0), &[]);
        let ollama = vec![proc_with(7777, "ollama", mem, Some(0.0), Some(310.0))];
        // The 90s window: ts 1000..=91000 is the close (91000 - 1000 = 90000 = window).
        let out = drive_opt(&mut engine, &dev, 1_000..=91_000, Some(5.0), &ollama);

        let evts: Vec<&Event> = out
            .iter()
            .filter(|e| e.kind == EventKind::CpuSpillover)
            .collect();
        assert_eq!(evts.len(), 1, "exactly one CpuSpillover at window close");
        let e = evts[0];
        assert_eq!(
            e.confidence,
            Confidence::Likely,
            "spillover is an inference"
        );
        assert_eq!(e.severity, Severity::Warning);
        assert!(
            e.title.contains("likely partial CPU offload")
                && e.title.contains("ollama (pid 7777)")
                && e.title.contains("11.3 GiB"),
            "title must hedge and name the model/mem, got: {}",
            e.title
        );
        assert!(
            e.evidence.contains("1000..91000")
                && e.evidence.contains("CPU mean")
                && e.evidence.contains("util mean"),
            "evidence must carry the window and both means, got: {}",
            e.evidence
        );
    }

    /// If the GPU shows real use at any point in the window, the model IS using it — the
    /// spillover premise is refuted and nothing narrates.
    #[test]
    fn spillover_cancels_when_gpu_gets_busy() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        let mem = 12 << 30;
        drive_opt(&mut engine, &dev, 0..=0, Some(3.0), &[]);
        let ollama = vec![proc_with(7777, "ollama", mem, Some(0.0), Some(310.0))];
        // Idle most of the window, then a clear burst of GPU use before it closes.
        let mut out = drive_opt(&mut engine, &dev, 1_000..=60_000, Some(5.0), &ollama);
        out.extend(drive_opt(
            &mut engine,
            &dev,
            61_000..=91_000,
            Some(80.0),
            &ollama,
        ));
        assert!(
            out.iter().all(|e| e.kind != EventKind::CpuSpillover),
            "a busy GPU refutes the offload claim — no spillover"
        );
    }

    /// A process that exits mid-window is cancelled silently; only its exit fact narrates.
    #[test]
    fn spillover_cancels_when_process_exits_midwindow() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        let mem = 12 << 30;
        drive_opt(&mut engine, &dev, 0..=0, Some(3.0), &[]);
        let ollama = vec![proc_with(7777, "ollama", mem, Some(0.0), Some(310.0))];
        let mut out = drive_opt(&mut engine, &dev, 1_000..=40_000, Some(5.0), &ollama);
        // ollama exits well before the 90s window would close.
        out.extend(drive_opt(
            &mut engine,
            &dev,
            41_000..=91_000,
            Some(5.0),
            &[],
        ));
        assert!(
            out.iter().all(|e| e.kind != EventKind::CpuSpillover),
            "process exited mid-window — spillover must be cancelled silently"
        );
        assert!(
            out.iter().any(|e| e.kind == EventKind::ProcessExited),
            "the exit itself must still narrate"
        );
    }

    /// With no CPU visibility for the whole window we cannot say the process "burns CPU";
    /// the honesty rule forbids the claim, so it stays silent even with a near-idle GPU.
    #[test]
    fn spillover_silent_without_cpu_visibility() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        let mem = 12 << 30;
        drive_opt(&mut engine, &dev, 0..=0, Some(3.0), &[]);
        // cpu_pct None throughout — the GPU is idle, but we never saw the CPU.
        let ollama = vec![proc_with(7777, "ollama", mem, Some(0.0), None)];
        let out = drive_opt(&mut engine, &dev, 1_000..=91_000, Some(5.0), &ollama);
        assert!(
            out.iter().all(|e| e.kind != EventKind::CpuSpillover),
            "no CPU visibility means no claim — must stay silent"
        );
    }

    /// Mostly-blind CPU visibility (only two readings, both hot) clears the CPU *mean* but
    /// not the sample-count floor: too few samples to mean it, so the claim is withheld.
    /// This isolates SPILLOVER_MIN_CPU_SAMPLES from the mean-CPU gate.
    #[test]
    fn spillover_silent_with_too_few_cpu_samples() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        let mem = 12 << 30;
        drive_opt(&mut engine, &dev, 0..=0, Some(3.0), &[]);
        // CPU unseen for almost the whole window, then exactly two hot readings: the mean
        // of those two (310%) would pass, but two samples is below the floor of three.
        let blind = vec![proc_with(7777, "ollama", mem, Some(0.0), None)];
        let hot = vec![proc_with(7777, "ollama", mem, Some(0.0), Some(310.0))];
        let mut out = drive_opt(&mut engine, &dev, 1_000..=88_000, Some(5.0), &blind);
        out.extend(drive_opt(
            &mut engine,
            &dev,
            89_000..=90_000,
            Some(5.0),
            &hot,
        ));
        out.extend(drive_opt(
            &mut engine,
            &dev,
            91_000..=91_000,
            Some(5.0),
            &blind,
        ));
        assert!(
            out.iter().all(|e| e.kind != EventKind::CpuSpillover),
            "two CPU samples is below SPILLOVER_MIN_CPU_SAMPLES — must stay silent"
        );
    }

    /// A small (1 GiB) attachment is below SPILLOVER_HOLDER_MIN_BYTES: no window opens, so
    /// even an idle GPU and a hot CPU never narrate a spillover.
    #[test]
    fn spillover_silent_for_small_attachment() {
        let mut engine = EventEngine::new();
        let dev = DeviceId("test".into());

        drive_opt(&mut engine, &dev, 0..=0, Some(3.0), &[]);
        let small = vec![proc_with(7777, "ollama", 1 << 30, Some(0.0), Some(310.0))];
        let out = drive_opt(&mut engine, &dev, 1_000..=91_000, Some(5.0), &small);
        assert!(
            out.iter().all(|e| e.kind != EventKind::CpuSpillover),
            "a 1 GiB holder is below the spillover threshold — no assessment opens"
        );
    }

    /// Every new inference narration must read as hedged and carry Confidence::Likely —
    /// a grep-style guard against a fact-tier slip on the riskiest events.
    #[test]
    fn hang_and_spillover_titles_are_hedged_inferences() {
        // Drive a hang to completion.
        let mut engine = EventEngine::new();
        let dev = DeviceId("hang".into());
        engine.register_device(dev.clone(), "GPU0".into());
        let procs = vec![proc_with(4521, "python", 6 << 30, None, None)];
        let mut all = drive_opt(&mut engine, &dev, 0..=600_000, Some(1.0), &procs);

        // Drive a spillover to completion on a separate device.
        let dev2 = DeviceId("spill".into());
        engine.register_device(dev2.clone(), "GPU1".into());
        drive_opt(&mut engine, &dev2, 0..=0, Some(3.0), &[]);
        let ollama = vec![proc_with(7777, "ollama", 12 << 30, Some(0.0), Some(310.0))];
        all.extend(drive_opt(
            &mut engine,
            &dev2,
            1_000..=91_000,
            Some(5.0),
            &ollama,
        ));

        for e in all
            .iter()
            .filter(|e| matches!(e.kind, EventKind::HangSuspected | EventKind::CpuSpillover))
        {
            assert_eq!(
                e.confidence,
                Confidence::Likely,
                "{:?} must be an inference, not a fact",
                e.kind
            );
            assert!(
                e.title.to_lowercase().contains("likely"),
                "{:?} title must hedge with \"likely\": {}",
                e.kind,
                e.title
            );
        }
        assert!(
            all.iter().any(|e| e.kind == EventKind::HangSuspected),
            "the hang fixture must actually fire a hang"
        );
        assert!(
            all.iter().any(|e| e.kind == EventKind::CpuSpillover),
            "the spillover fixture must actually fire a spillover"
        );
    }
}
