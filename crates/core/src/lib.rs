//! gpuviewer-core — telemetry collection, data model, and event derivation.
//!
//! Architecture (see CLAUDE.md and docs/research/04-synthesis.md):
//! - [`backend::GpuBackend`]: nvtop's vendor-vtable split (static / dynamic / processes),
//!   per-field `Option<T>`, runtime-loaded vendor libs, failed init = skipped backend.
//! - [`events::EventEngine`]: derives the narrated "story" events from raw samples.
//! - [`mock::MockBackend`]: scripted simulation for CI/demos; the contract for real backends.

pub mod backend;
pub mod events;
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
}
