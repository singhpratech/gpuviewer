//! Collection engine shared by the TUI thread and `--json` mode.

use std::collections::HashMap;
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
    /// Total VRAM, so JSON consumers can compute used/total without a second query.
    pub mem_total_bytes: Option<u64>,
    pub sample: Option<DynamicSample>,
    pub processes: Vec<ProcessSample>,
}

/// One collection tick across all devices. Internal shape only: `--json` serializes the
/// envelope structs in `main.rs` instead (events go out as separate lines, per
/// docs/spec/ndjson-v1.md), so this deliberately does not derive `Serialize`.
pub struct Frame {
    pub ts_ms: u64,
    pub devices: Vec<FrameDevice>,
    pub events: Vec<Event>,
}

/// Normalize a PCI address (`domain:bus:dev.func`) for cross-backend dedupe: NVML reports
/// `00000000:01:00.0` while sysfs reports `0000:01:00.0` — the same physical GPU. Lowercase
/// everything; trim/zero-pad the domain to 4 hex digits (a genuinely >16-bit domain keeps
/// its extra digits — both sources print those the same way). Returns `None` for anything
/// that isn't a PCI address (`mock:…`, `nvml:0` fallback ids) — those are never deduped:
/// wrongly merging two distinct devices is worse than listing one twice.
fn normalize_pci_id(id: &str) -> Option<String> {
    let id = id.to_ascii_lowercase();
    let (domain, rest) = id.split_once(':')?;
    let (bus, devfn) = rest.split_once(':')?;
    let (dev, func) = devfn.split_once('.')?;
    // Each segment must be pure hex of plausible width (catches embedded extra `:`/`.`
    // too, since those aren't hex digits).
    let hex = |s: &str, max: usize| {
        !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_hexdigit())
    };
    if !hex(domain, 8) || !hex(bus, 2) || !hex(dev, 2) || !hex(func, 1) {
        return None;
    }
    let domain = format!("{:0>4}", domain.trim_start_matches('0'));
    Some(format!("{domain}:{bus:0>2}:{dev:0>2}.{func}"))
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
        // Normalized PCI address → name of the backend that registered it first.
        let mut seen_pci: HashMap<String, &'static str> = HashMap::new();

        for (bi, b) in backends.iter_mut().enumerate() {
            for id in b.devices() {
                // Cross-backend dedupe by PCI address (settled CLAUDE.md decision). First
                // backend wins: registry order is nvidia → amd → intel, so the richest
                // source for a device is canonical.
                let pci_key = normalize_pci_id(&id.0);
                if let Some(key) = &pci_key {
                    if let Some(first) = seen_pci.get(key.as_str()) {
                        eprintln!(
                            "gpuviewer: {id} ({}) duplicates a device already registered by {first}; skipping",
                            b.name()
                        );
                        continue;
                    }
                }
                match b.static_info(&id) {
                    Ok(info) => {
                        if let Some(key) = pci_key {
                            seen_pci.insert(key, b.name());
                        }
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

    /// Whether the data on screen is simulated — true when the mock backend is active
    /// (forced via `--mock` or registered as the no-real-GPU fallback; it is exclusive
    /// either way). The UI labels mock data as mock, and must never label live data so.
    pub fn mock_in_use(&self) -> bool {
        self.backends.iter().any(|b| b.name() == "mock")
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
                mem_total_bytes: info.mem_total_bytes,
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
    /// True when the data is simulated (mock backend active) — drives the footer's
    /// "(mock data)" tag, which must track the actual data source.
    pub mock: bool,
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
        let mock = engine.mock_in_use();
        let shared = Arc::new(Mutex::new(Shared {
            infos,
            latest: vec![None; n],
            processes: vec![Vec::new(); n],
            // Live window: 30 min at 1s ticks; event log capped generously.
            history: HistoryStore::new(1800, 5000),
            mock,
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

#[cfg(test)]
mod tests {
    use super::normalize_pci_id;

    #[test]
    fn normalize_pci_id_unifies_nvml_and_sysfs_forms() {
        // NVML's 8-hex-digit domain and sysfs's 4-digit domain are the same device.
        assert_eq!(
            normalize_pci_id("00000000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        assert_eq!(
            normalize_pci_id("0000:01:00.0").as_deref(),
            Some("0000:01:00.0")
        );
        // NVML historically uppercases hex; normalization is case-insensitive.
        assert_eq!(
            normalize_pci_id("00000000:0A:00.0").as_deref(),
            Some("0000:0a:00.0")
        );
        // A non-zero domain survives the trim/pad in both spellings.
        assert_eq!(
            normalize_pci_id("00000001:03:00.0").as_deref(),
            Some("0001:03:00.0")
        );
        assert_eq!(
            normalize_pci_id("0001:03:00.0").as_deref(),
            Some("0001:03:00.0")
        );
    }

    #[test]
    fn normalize_pci_id_rejects_non_pci_ids() {
        // Mock and index-fallback ids must never dedupe against anything.
        assert_eq!(normalize_pci_id("mock:0000:01:00.0"), None);
        assert_eq!(normalize_pci_id("nvml:0"), None);
        assert_eq!(normalize_pci_id(""), None);
        assert_eq!(normalize_pci_id("0000:01:00"), None); // no function part
        assert_eq!(normalize_pci_id("0000:01:00.0.1"), None); // trailing junk
        assert_eq!(normalize_pci_id("0000:01:02:00.0"), None); // extra segment
    }
}
