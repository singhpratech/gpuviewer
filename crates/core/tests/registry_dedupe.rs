//! Cross-backend PCI dedupe — the registry contract documented on `all_backends`
//! (`crates/core/src/backend.rs`): the collector dedupes devices across backends by
//! normalized PCI address, FIRST backend in registry order wins, and ids that
//! `normalize_pci_id` refuses to key (synthetic `wddm:…`/`apple:…`/`mock:…` shapes)
//! are never merged — a double listing is visible and honest, a wrong merge is not.
//!
//! `normalize_pci_id` itself is pure-tested in `model.rs`; what was untested is the
//! collection-level behavior: which DEVICE survives, in which spelling, and what falls
//! through. The production loop lives in `Engine::with_backends`
//! (`crates/tui/src/collector.rs`, `pub(crate)` — not reachable from this crate), so
//! `discover` below is a literal transcription of that loop's registry semantics, keyed
//! on the REAL `normalize_pci_id` — the decision kernel is the production code path, the
//! loop is the documented contract. If the discovery loop ever moves into core (e.g. a
//! `pub fn` on `backend.rs`), switch this file to drive it directly and delete the
//! transcription.

use std::collections::HashMap;

use gpuviewer_core::{
    normalize_pci_id, BackendError, DeviceId, DynamicSample, GpuBackend, ProcessSample, StaticInfo,
    Vendor,
};

/// A scripted backend reporting a fixed device list. `broken` ids fail `static_info`
/// (the driverless/broken-library shape) — per the registry contract such a backend
/// must NOT claim the PCI key, so a later backend's sighting of the same board survives.
struct ScriptedBackend {
    name: &'static str,
    devices: Vec<DeviceId>,
    broken: Vec<DeviceId>,
}

impl ScriptedBackend {
    fn boxed(name: &'static str, ids: &[&str]) -> Box<dyn GpuBackend> {
        Box::new(ScriptedBackend {
            name,
            devices: ids.iter().map(|s| DeviceId((*s).into())).collect(),
            broken: Vec::new(),
        })
    }

    fn boxed_broken(name: &'static str, ids: &[&str], broken: &[&str]) -> Box<dyn GpuBackend> {
        Box::new(ScriptedBackend {
            name,
            devices: ids.iter().map(|s| DeviceId((*s).into())).collect(),
            broken: broken.iter().map(|s| DeviceId((*s).into())).collect(),
        })
    }
}

impl GpuBackend for ScriptedBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn devices(&mut self) -> Vec<DeviceId> {
        self.devices.clone()
    }

    fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
        if self.broken.contains(dev) {
            return Err(BackendError::Unavailable("scripted init failure".into()));
        }
        Ok(StaticInfo {
            id: dev.clone(),
            vendor: Vendor::Unknown,
            name: format!("{} {}", self.name, dev),
            backend: self.name.to_string(),
            mem_total_bytes: None,
            power_limit_mw: None,
            max_sm_clock_mhz: None,
            temp_slowdown_c: None,
            driver_version: None,
            process_hint: None,
            source_caveat: None,
        })
    }

    fn refresh_dynamic(&mut self, _dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        // Never reached by these tests; all-None is the honest shape for a stub that
        // observes nothing (throttle None = unobservable, NOT Some(all-false)).
        Ok(DynamicSample {
            ts_ms: 0,
            util_pct: None,
            util_engine: None,
            mem_used_bytes: None,
            power_mw: None,
            temp_c: None,
            fan_pct: None,
            sm_clock_mhz: None,
            mem_clock_mhz: None,
            encoder_pct: None,
            decoder_pct: None,
            throttle: None,
        })
    }

    fn refresh_processes(&mut self, _dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError> {
        Ok(Vec::new())
    }
}

/// First-wins device discovery over an ordered backend set — the registry semantics from
/// `Engine::with_backends` (collector.rs), keyed on the production `normalize_pci_id`:
/// walk backends in registry order; skip a device whose normalized PCI key an earlier
/// backend already CLAIMED; a key is claimed only once `static_info` succeeds (a backend
/// that cannot describe the board must not block a later backend from registering it);
/// ids `normalize_pci_id` refuses (`None`) are always kept — never merged.
fn discover(mut backends: Vec<Box<dyn GpuBackend>>) -> Vec<StaticInfo> {
    let mut devices = Vec::new();
    let mut seen_pci: HashMap<String, &'static str> = HashMap::new();
    for b in backends.iter_mut() {
        for id in b.devices() {
            let pci_key = normalize_pci_id(&id.0);
            if let Some(key) = &pci_key {
                if seen_pci.contains_key(key.as_str()) {
                    continue; // an earlier backend already claimed this physical device
                }
            }
            match b.static_info(&id) {
                Ok(info) => {
                    if let Some(key) = pci_key {
                        seen_pci.insert(key, b.name());
                    }
                    devices.push(info);
                }
                Err(_) => {
                    // Skipped device — like the registry, never fatal, never a claim.
                }
            }
        }
    }
    devices
}

/// (a) NVML's 8-hex-digit-domain spelling and sysfs's 4-digit spelling of one physical
/// GPU collapse to exactly one device; (b) the survivor is the FIRST backend's, and its
/// id keeps that backend's original spelling (dedupe selects, it never rewrites).
#[test]
fn nvml_and_sysfs_spellings_keep_exactly_one_device_first_backend_wins() {
    let devices = discover(vec![
        ScriptedBackend::boxed("rich", &["00000000:03:00.0"]),
        ScriptedBackend::boxed("poor", &["0000:03:00.0"]),
    ]);
    assert_eq!(
        devices.len(),
        1,
        "two spellings of one PCI address must register one device, got: {:?}",
        devices.iter().map(|d| &d.id).collect::<Vec<_>>()
    );
    assert_eq!(devices[0].backend, "rich", "first backend in order wins");
    assert_eq!(
        devices[0].id,
        DeviceId("00000000:03:00.0".into()),
        "the survivor keeps the winning backend's original spelling"
    );
}

/// Hex-case variants of one address are the same physical device.
#[test]
fn case_variants_of_one_address_dedupe() {
    let devices = discover(vec![
        ScriptedBackend::boxed("rich", &["0000:0A:00.0"]),
        ScriptedBackend::boxed("poor", &["0000:0a:00.0"]),
    ]);
    assert_eq!(devices.len(), 1, "case variants must dedupe");
    assert_eq!(devices[0].backend, "rich");
}

/// (b) First-wins is driven by REGISTRY ORDER alone, not by spelling, vendor, or name:
/// the same two backends in reversed order flip the survivor.
#[test]
fn survivor_follows_registry_order_not_spelling() {
    let devices = discover(vec![
        ScriptedBackend::boxed("poor", &["0000:03:00.0"]),
        ScriptedBackend::boxed("rich", &["00000000:03:00.0"]),
    ]);
    assert_eq!(devices.len(), 1);
    assert_eq!(
        devices[0].backend, "poor",
        "reversing registry order must flip the survivor"
    );
    assert_eq!(devices[0].id, DeviceId("0000:03:00.0".into()));
}

/// (c) Genuinely different PCI addresses are never merged — including the adjacent-
/// function decoy (`…:00.1` vs `…:00.0`) that a normalization bug truncating the
/// function digit would wrongly collapse.
#[test]
fn distinct_pci_addresses_are_never_merged() {
    let devices = discover(vec![
        ScriptedBackend::boxed("rich", &["0000:03:00.0"]),
        ScriptedBackend::boxed("poor", &["0000:04:00.0", "0000:03:00.1"]),
    ]);
    assert_eq!(
        devices.len(),
        3,
        "different bus and different function are different devices, got: {:?}",
        devices.iter().map(|d| &d.id).collect::<Vec<_>>()
    );
}

/// The real wddm/sysfs fall-through frame: dedupe and pass-through must coexist per
/// device, not per backend — the second backend loses the shared board but still
/// registers the board only it can see.
#[test]
fn dedupe_is_per_device_not_per_backend() {
    let devices = discover(vec![
        ScriptedBackend::boxed("rich", &["00000000:03:00.0"]),
        ScriptedBackend::boxed("poor", &["0000:03:00.0", "0000:05:00.0"]),
    ]);
    let labels: Vec<(&str, &str)> = devices
        .iter()
        .map(|d| (d.backend.as_str(), d.id.0.as_str()))
        .collect();
    assert_eq!(
        labels,
        vec![("rich", "00000000:03:00.0"), ("poor", "0000:05:00.0")],
        "shared board goes to the first backend; the unshared board falls through"
    );
}

/// (d) Ids `normalize_pci_id` refuses to key are NEVER merged — even when textually
/// identical across backends. Listing a device twice is visible and honest; merging two
/// devices that merely share a synthetic label is not (backend.rs registry contract).
#[test]
fn non_pci_ids_never_merge_even_when_textually_identical() {
    let devices = discover(vec![
        ScriptedBackend::boxed("a", &["wddm:10de:2684:0", "mock:0"]),
        ScriptedBackend::boxed("b", &["wddm:10de:2684:0", "mock:0"]),
    ]);
    assert_eq!(
        devices.len(),
        4,
        "synthetic ids must never dedupe, got: {:?}",
        devices
            .iter()
            .map(|d| (&d.backend, &d.id))
            .collect::<Vec<_>>()
    );
}

/// A first backend whose `static_info` fails must not claim the PCI key: the later
/// backend's sighting of the same board registers instead. This is the load-bearing
/// nuance behind backend.rs's "NVIDIA boards on driverless/broken-NVML machines fall
/// through to wddm" — a backend that cannot describe a device cannot own it.
#[test]
fn failed_static_info_does_not_claim_the_key() {
    let devices = discover(vec![
        ScriptedBackend::boxed_broken("rich", &["00000000:03:00.0"], &["00000000:03:00.0"]),
        ScriptedBackend::boxed("poor", &["0000:03:00.0"]),
    ]);
    assert_eq!(
        devices.len(),
        1,
        "the board must still register exactly once"
    );
    assert_eq!(
        devices[0].backend, "poor",
        "a backend whose static_info failed must not block the fall-through"
    );
    assert_eq!(devices[0].id, DeviceId("0000:03:00.0".into()));
}
