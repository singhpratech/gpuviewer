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
        assert!(kinds.contains(&EventKind::ThrottleStart), "no throttle event in 400 ticks");
        assert!(kinds.contains(&EventKind::ProcessAttached), "no attach event in 400 ticks");
        assert!(kinds.contains(&EventKind::ProcessExited), "no exit event in 400 ticks");
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
        assert!(events.is_empty(), "first observation must not narrate preexisting processes");
    }
}
