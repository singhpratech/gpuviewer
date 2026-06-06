//! gpuviewer-history — the flight recorder's storage.
//!
//! v0: RAM ring buffers for the live window + append-only event log.
//! Next milestone: fold rings into SQLite (rusqlite, WAL) 10s/1m rollup tiers with retention
//! pruning. Never write raw 1Hz samples to SQLite (netdata's dbengine lesson).

use std::collections::{HashMap, VecDeque};

use gpuviewer_core::{DeviceId, DynamicSample, Event};

/// Fixed-capacity ring of samples for one device's live window.
pub struct DeviceHistory {
    samples: VecDeque<DynamicSample>,
    cap: usize,
}

impl DeviceHistory {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, s: DynamicSample) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    pub fn latest(&self) -> Option<&DynamicSample> {
        self.samples.back()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DynamicSample> {
        self.samples.iter()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// All devices' rings + the event log.
pub struct HistoryStore {
    per_device: HashMap<DeviceId, DeviceHistory>,
    events: Vec<Event>,
    sample_cap: usize,
    event_cap: usize,
}

impl HistoryStore {
    pub fn new(sample_cap: usize, event_cap: usize) -> Self {
        Self {
            per_device: HashMap::new(),
            events: Vec::new(),
            sample_cap,
            event_cap,
        }
    }

    pub fn push_sample(&mut self, dev: &DeviceId, s: DynamicSample) {
        self.per_device
            .entry(dev.clone())
            .or_insert_with(|| DeviceHistory::new(self.sample_cap))
            .push(s);
    }

    pub fn push_events(&mut self, evs: impl IntoIterator<Item = Event>) {
        self.events.extend(evs);
        if self.events.len() > self.event_cap {
            let drop = self.events.len() - self.event_cap;
            self.events.drain(..drop);
        }
    }

    pub fn device(&self, dev: &DeviceId) -> Option<&DeviceHistory> {
        self.per_device.get(dev)
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpuviewer_core::ThrottleReasons;

    fn sample(ts: u64) -> DynamicSample {
        DynamicSample {
            ts_ms: ts,
            util_pct: Some(1.0),
            mem_used_bytes: None,
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

    #[test]
    fn ring_caps_at_capacity() {
        let mut h = DeviceHistory::new(3);
        for i in 0..10 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.latest().unwrap().ts_ms, 9);
        assert_eq!(h.iter().next().unwrap().ts_ms, 7);
    }
}
