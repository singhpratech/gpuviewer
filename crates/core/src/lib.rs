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

pub use backend::{all_backends, BackendError, GpuBackend};
pub use events::{Confidence, Event, EventEngine, EventKind, Severity};
pub use model::{
    fmt_bytes, now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo,
    ThrottleReasons, Vendor,
};

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
}
