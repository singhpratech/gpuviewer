//! Mock backend: deterministic-ish simulation used for CI and demos (no GPU required).
//!
//! Device 0 simulates a training run: high util with periodic idle gaps (dataloader /
//! checkpoint pattern), VRAM climbing toward OOM, temperature chasing util until a thermal
//! throttle cycle kicks in. Device 1 simulates a desktop/inference box: bursty util and an
//! `ollama` process that periodically attaches with a large allocation, then exits.

use crate::backend::{BackendError, GpuBackend};
use crate::model::{
    now_ms, DeviceId, DynamicSample, ProcessKind, ProcessSample, StaticInfo, ThrottleReasons,
    Vendor,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// Tiny xorshift PRNG — keeps the core crate dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform float in [0, 1).
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform float in [lo, hi).
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + self.f() * (hi - lo)
    }
}

struct TrainSim {
    rng: Rng,
    tick: u64,
    temp_c: f64,
    vram_python: f64,
    throttling: bool,
    fan_pct: f64,
    /// tick at which the current idle gap ends (0 = not idle).
    idle_until: u64,
}

struct DesktopSim {
    rng: Rng,
    tick: u64,
    ollama_present: bool,
    ollama_toggle_at: u64,
    util_level: f64,
}

pub struct MockBackend {
    ids: [DeviceId; 2],
    train: TrainSim,
    desktop: DesktopSim,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            ids: [
                DeviceId("mock:0000:01:00.0".into()),
                DeviceId("mock:0000:03:00.0".into()),
            ],
            train: TrainSim {
                rng: Rng(0x9E37_79B9_7F4A_7C15),
                tick: 0,
                temp_c: 52.0,
                vram_python: 19.2 * GIB as f64,
                throttling: false,
                fan_pct: 35.0,
                idle_until: 0,
            },
            desktop: DesktopSim {
                rng: Rng(0xD1B5_4A32_D192_ED03),
                tick: 0,
                ollama_present: false,
                ollama_toggle_at: 45,
                util_level: 12.0,
            },
        }
    }
}

impl MockBackend {
    /// One simulation step for ALL devices at a synthetic timestamp, in `devices()` order —
    /// the seeding entry point for `gpuviewer demo`, which replays hours of history through
    /// the sims in seconds. The live path (`refresh_dynamic` at `now_ms()`) and this one
    /// share the same `step()`, so seeded history and live mock data are the same
    /// simulation, just on different clocks.
    pub fn tick_at(&mut self, ts_ms: u64) -> Vec<(DeviceId, DynamicSample, Vec<ProcessSample>)> {
        vec![
            (
                self.ids[0].clone(),
                self.train.step(ts_ms),
                self.train.processes(),
            ),
            (
                self.ids[1].clone(),
                self.desktop.step(ts_ms),
                self.desktop.processes(),
            ),
        ]
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn devices(&mut self) -> Vec<DeviceId> {
        self.ids.to_vec()
    }

    fn static_info(&mut self, dev: &DeviceId) -> Result<StaticInfo, BackendError> {
        if *dev == self.ids[0] {
            Ok(StaticInfo {
                id: dev.clone(),
                vendor: Vendor::Nvidia,
                name: "GeForce RTX 4090 (mock)".into(),
                backend: "mock".into(),
                mem_total_bytes: Some(24 * GIB),
                power_limit_mw: Some(450_000),
                max_sm_clock_mhz: Some(2_520),
                temp_slowdown_c: Some(84.0),
                driver_version: Some("mock 999.99".into()),
                process_hint: None,
                source_caveat: None,
            })
        } else if *dev == self.ids[1] {
            Ok(StaticInfo {
                id: dev.clone(),
                vendor: Vendor::Amd,
                name: "Radeon RX 7900 XTX (mock)".into(),
                backend: "mock".into(),
                mem_total_bytes: Some(24 * GIB),
                power_limit_mw: Some(355_000),
                max_sm_clock_mhz: Some(2_500),
                temp_slowdown_c: Some(110.0),
                driver_version: Some("mock amdgpu".into()),
                process_hint: None,
                source_caveat: None,
            })
        } else {
            Err(BackendError::DeviceNotFound(dev.clone()))
        }
    }

    fn refresh_dynamic(&mut self, dev: &DeviceId) -> Result<DynamicSample, BackendError> {
        let ts = now_ms();
        if *dev == self.ids[0] {
            Ok(self.train.step(ts))
        } else if *dev == self.ids[1] {
            Ok(self.desktop.step(ts))
        } else {
            Err(BackendError::DeviceNotFound(dev.clone()))
        }
    }

    fn refresh_processes(&mut self, dev: &DeviceId) -> Result<Vec<ProcessSample>, BackendError> {
        if *dev == self.ids[0] {
            Ok(self.train.processes())
        } else if *dev == self.ids[1] {
            Ok(self.desktop.processes())
        } else {
            Err(BackendError::DeviceNotFound(dev.clone()))
        }
    }
}

impl TrainSim {
    fn step(&mut self, ts_ms: u64) -> DynamicSample {
        self.tick += 1;

        // Idle gaps: every ~90 ticks, go idle for 8-20 ticks (checkpoint/validation pattern).
        if self.idle_until == 0 && self.tick.is_multiple_of(90) {
            self.idle_until = self.tick + 8 + (self.rng.next() % 13);
        }
        let idle = self.idle_until > self.tick;
        if !idle {
            self.idle_until = 0;
        }

        let util = if idle {
            self.rng.range(0.0, 4.0)
        } else {
            self.rng.range(91.0, 99.5)
        };

        // VRAM: python allocation climbs ~4.5 MiB/tick (~270 MiB/min at 1s ticks) so the
        // pressure event fires within a few minutes of watching; resets when OOM-adjacent
        // to keep the demo looping.
        self.vram_python += self.rng.range(3.0, 6.0) * 1024.0 * 1024.0;
        if self.vram_python > 22.8 * GIB as f64 {
            self.vram_python = 16.5 * GIB as f64;
        }

        // Temperature chases util; fan chases temperature; throttle with hysteresis at the
        // slowdown threshold.
        let target = if idle { 48.0 } else { 87.0 };
        let cooling = (self.fan_pct - 30.0) * 0.06;
        self.temp_c += (target - self.temp_c) * 0.06 - cooling * 0.02 + self.rng.range(-0.3, 0.3);
        self.fan_pct += ((self.temp_c - 55.0).max(0.0) * 2.6 - self.fan_pct) * 0.08;
        self.fan_pct = self.fan_pct.clamp(28.0, 100.0);

        if !self.throttling && self.temp_c >= 84.0 {
            self.throttling = true;
        } else if self.throttling && self.temp_c <= 79.0 {
            self.throttling = false;
        }

        let max_clock = 2520.0;
        let clock = if self.throttling {
            self.rng.range(1750.0, 1860.0)
        } else if idle {
            self.rng.range(210.0, 420.0)
        } else {
            self.rng.range(max_clock - 90.0, max_clock)
        };

        let power = if idle {
            self.rng.range(28_000.0, 45_000.0)
        } else if self.throttling {
            self.rng.range(300_000.0, 330_000.0)
        } else {
            self.rng.range(390_000.0, 448_000.0)
        };

        DynamicSample {
            ts_ms,
            util_pct: Some(util as f32),
            util_engine: None,
            mem_used_bytes: Some(self.vram_python as u64 + 700 * 1024 * 1024),
            power_mw: Some(power as u32),
            temp_c: Some(self.temp_c as f32),
            fan_pct: Some(self.fan_pct as f32),
            sm_clock_mhz: Some(clock as u32),
            mem_clock_mhz: Some(10_500),
            encoder_pct: Some(0.0),
            decoder_pct: Some(0.0),
            // The mock OBSERVES throttling by design (it scripts it) — always `Some`,
            // never the unobservable `None` (design §5.4: mock stays Some).
            throttle: Some(ThrottleReasons {
                thermal: self.throttling,
                ..Default::default()
            }),
        }
    }

    fn processes(&mut self) -> Vec<ProcessSample> {
        let idle = self.idle_until > self.tick;
        vec![
            ProcessSample {
                pid: 4521,
                name: "python".into(),
                kind: ProcessKind::Compute,
                mem_bytes: Some(self.vram_python as u64),
                util_pct: Some(if idle { 1.0 } else { 96.0 }),
                // A dataloader pegging a few cores while the GPU works (more during an idle
                // gap, the CPU-bound stall fingerprint) — gives the CPU% column live coverage.
                cpu_pct: Some(if idle { 320.0 } else { 180.0 }),
                container: None,
            },
            ProcessSample {
                pid: 1203,
                name: "Xorg".into(),
                kind: ProcessKind::Graphics,
                mem_bytes: Some(420 * 1024 * 1024),
                util_pct: Some(2.0),
                cpu_pct: Some(6.0),
                container: None,
            },
        ]
    }
}

impl DesktopSim {
    fn step(&mut self, ts_ms: u64) -> DynamicSample {
        self.tick += 1;

        // ollama attaches/leaves on a cycle to exercise process lifecycle events.
        if self.tick >= self.ollama_toggle_at {
            self.ollama_present = !self.ollama_present;
            let hold = if self.ollama_present { 70 } else { 50 };
            self.ollama_toggle_at = self.tick + hold + (self.rng.next() % 30);
        }

        let target = if self.ollama_present {
            self.rng.range(55.0, 92.0)
        } else {
            self.rng.range(3.0, 28.0)
        };
        self.util_level += (target - self.util_level) * 0.3;

        let used = if self.ollama_present {
            (12.4 * GIB as f64) + self.rng.range(-0.2, 0.2) * GIB as f64
        } else {
            1.1 * GIB as f64
        };

        DynamicSample {
            ts_ms,
            util_pct: Some(self.util_level as f32),
            util_engine: None,
            mem_used_bytes: Some(used as u64),
            power_mw: Some(if self.ollama_present { 248_000 } else { 41_000 }),
            temp_c: Some(if self.ollama_present { 71.0 } else { 44.0 }),
            fan_pct: Some(if self.ollama_present { 58.0 } else { 0.0 }),
            sm_clock_mhz: Some(if self.ollama_present { 2_390 } else { 350 }),
            mem_clock_mhz: None, // exercise the Option path: not every metric exists
            encoder_pct: None,
            decoder_pct: None,
            throttle: Some(ThrottleReasons::default()),
        }
    }

    fn processes(&mut self) -> Vec<ProcessSample> {
        let mut v = vec![ProcessSample {
            pid: 980,
            name: "gnome-shell".into(),
            kind: ProcessKind::Graphics,
            mem_bytes: Some(610 * 1024 * 1024),
            util_pct: Some(3.0),
            cpu_pct: Some(12.0),
            container: None,
        }];
        if self.ollama_present {
            v.push(ProcessSample {
                pid: 7777,
                name: "ollama".into(),
                kind: ProcessKind::Compute,
                mem_bytes: Some(11 * GIB + 350 * 1024 * 1024),
                util_pct: Some(74.0),
                // Runs in a container (the cluster-operator's "which pod" column) and burns a
                // core or so serving the model — gives both new columns mock coverage.
                cpu_pct: Some(140.0),
                container: Some("docker:3f2a9c1b4d5e".into()),
            });
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two fresh backends stepped with the same timestamp sequence must produce identical
    /// frames — `gpuviewer demo` relies on this: the seeded story is reproducible, and any
    /// hidden wall-clock dependence in the sims (which would make the demo unrepeatable)
    /// fails here.
    #[test]
    fn tick_at_is_deterministic_across_instances() {
        let mut a = MockBackend::new();
        let mut b = MockBackend::new();
        for i in 0..500u64 {
            let ts = 1_700_000_000_000 + i * 1_000;
            let fa = a.tick_at(ts);
            let fb = b.tick_at(ts);
            assert_eq!(fa, fb, "frames diverged at tick {i}");
            assert_eq!(fa.len(), 2, "tick_at must cover BOTH mock devices");
            for (_, sample, procs) in &fa {
                assert_eq!(sample.ts_ms, ts, "samples carry the synthetic timestamp");
                assert!(!procs.is_empty(), "mock devices always have processes");
            }
        }
    }

    /// `tick_at` and the live `refresh_dynamic` route through the same simulation step, so
    /// driving one backend via `tick_at` and another via the trait methods yields the same
    /// per-tick *state evolution* (only the timestamps differ — the live path stamps
    /// `now_ms()`). Throttling within 500 ticks proves the seeded story actually contains
    /// the throttle onset the demo scrolls back to.
    #[test]
    fn tick_at_drives_the_same_simulation_as_refresh_dynamic() {
        use crate::backend::GpuBackend;
        let mut seeded = MockBackend::new();
        let mut live = MockBackend::new();
        let train = live.devices()[0].clone();
        let mut seeded_throttled = false;
        for i in 0..500u64 {
            let frame = seeded.tick_at(i * 1_000);
            let (_, s, _) = &frame[0];
            let mut l = live.refresh_dynamic(&train).unwrap();
            // Same evolution apart from the clock: align it and compare everything else.
            l.ts_ms = s.ts_ms;
            assert_eq!(*s, l, "sim state diverged at tick {i}");
            let _ = live.refresh_processes(&train).unwrap();
            seeded_throttled |= s.throttle.is_some_and(|t| t.any());
        }
        assert!(
            seeded_throttled,
            "500 ticks of the training sim must include a throttle episode"
        );
    }
}
