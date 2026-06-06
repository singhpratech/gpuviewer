//! Collection engine shared by the TUI thread and `--json` mode.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpuviewer_core::{
    all_backends, DeviceId, DynamicSample, Event, EventEngine, GpuBackend, ProcessSample,
    StaticInfo,
};
use gpuviewer_history::HistoryStore;
use serde::Serialize;

/// One collection tick's output for one device.
#[derive(Serialize)]
pub struct FrameDevice {
    pub id: DeviceId,
    pub name: String,
    pub sample: Option<DynamicSample>,
    pub processes: Vec<ProcessSample>,
}

/// One collection tick across all devices.
#[derive(Serialize)]
pub struct Frame {
    pub ts_ms: u64,
    pub devices: Vec<FrameDevice>,
    pub events: Vec<Event>,
}

pub struct Engine {
    backends: Vec<Box<dyn GpuBackend>>,
    /// (backend index, device id, static info)
    devices: Vec<(usize, DeviceId, StaticInfo)>,
    event_engine: EventEngine,
}

impl Engine {
    pub fn new(force_mock: bool) -> Self {
        let mut backends = all_backends(force_mock);
        let mut devices = Vec::new();
        let mut event_engine = EventEngine::new();

        for (bi, b) in backends.iter_mut().enumerate() {
            for id in b.devices() {
                match b.static_info(&id) {
                    Ok(info) => {
                        event_engine.register_device(id.clone(), format!("GPU{}", devices.len()));
                        devices.push((bi, id, info));
                    }
                    Err(e) => {
                        eprintln!("gpuviewer: skipping {id} ({}): {e}", b.name());
                    }
                }
            }
        }

        Self {
            backends,
            devices,
            event_engine,
        }
    }

    pub fn static_infos(&self) -> Vec<StaticInfo> {
        self.devices.iter().map(|(_, _, i)| i.clone()).collect()
    }

    pub fn tick(&mut self) -> Frame {
        let mut frame_devices = Vec::with_capacity(self.devices.len());
        let mut events = Vec::new();

        for (bi, id, info) in &self.devices {
            let backend = &mut self.backends[*bi];
            let sample = backend.refresh_dynamic(id).ok();
            let processes = backend.refresh_processes(id).unwrap_or_default();
            if let Some(s) = &sample {
                events.extend(self.event_engine.observe(
                    id,
                    s,
                    &processes,
                    info.mem_total_bytes,
                    info.temp_slowdown_c,
                ));
            }
            frame_devices.push(FrameDevice {
                id: id.clone(),
                name: info.name.clone(),
                sample,
                processes,
            });
        }

        Frame {
            ts_ms: gpuviewer_core::now_ms(),
            devices: frame_devices,
            events,
        }
    }
}

/// State shared between the collector thread and the UI.
pub struct Shared {
    pub infos: Vec<StaticInfo>,
    pub latest: Vec<Option<DynamicSample>>,
    pub processes: Vec<Vec<ProcessSample>>,
    pub history: HistoryStore,
}

pub struct Collector {
    pub shared: Arc<Mutex<Shared>>,
    pub paused: Arc<AtomicBool>,
}

impl Collector {
    /// Spawn the background collection thread.
    pub fn start(mut engine: Engine, interval: Duration) -> Self {
        let infos = engine.static_infos();
        let n = infos.len();
        let shared = Arc::new(Mutex::new(Shared {
            infos,
            latest: vec![None; n],
            processes: vec![Vec::new(); n],
            // Live window: 30 min at 1s ticks; event log capped generously.
            history: HistoryStore::new(1800, 5000),
        }));
        let paused = Arc::new(AtomicBool::new(false));

        let s = Arc::clone(&shared);
        let p = Arc::clone(&paused);
        std::thread::spawn(move || loop {
            if !p.load(Ordering::Relaxed) {
                let frame = engine.tick();
                let mut sh = s.lock().unwrap();
                for (i, fd) in frame.devices.iter().enumerate() {
                    if let Some(sample) = &fd.sample {
                        sh.history.push_sample(&fd.id, sample.clone());
                        sh.latest[i] = Some(sample.clone());
                    }
                    sh.processes[i] = fd.processes.clone();
                }
                sh.history.push_events(frame.events);
            }
            std::thread::sleep(interval);
        });

        Self { shared, paused }
    }
}
