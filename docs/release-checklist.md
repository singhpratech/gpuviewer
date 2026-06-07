# Release checklist — manual real-hardware smoke tests

CI has no GPUs by design; these items can only be validated on real hardware. Run before
each release on whatever hardware is available; check off with hardware + kernel + driver
versions noted. Items here are the **[HW-VERIFY]** set from
`docs/research/07-non-nvidia-coverage.md` (§6 and inline marks) — no fixture can prove
them. For several items, **graceful absence ("unavailable"/None with a stated reason) IS
the pass criterion** — a fabricated value or a crash is the failure.

Tip: smoke tests on real GPUs record to the default `history.db`. Use
`--db /tmp/smoke.db` (or `--no-persist`) so test runs don't pollute real history.

## Kernel-version gate quick reference

| Gate | Driver | Min kernel |
|---|---|---|
| AMD fdinfo node exists | amdgpu | 5.14 |
| AMD fdinfo parseable (standardized keys; 5.14–5.18 dialect → empty list, no crash) | amdgpu | 5.19 |
| i915 per-client engine busy-ns | i915 | 5.19 |
| i915 per-client memory | i915 | 6.8 |
| xe fdinfo engine cycles | xe | 6.11 |
| xe PMU | xe | 6.15 |
| — additional gates (07-non-nvidia-coverage.md) — | | |
| AMD hwmon `power1_input` (input/average split) | amdgpu | 6.6 |
| i915 dGPU temp/fan hwmon | i915 | 6.12 |
| xe default for Lunar Lake + Battlemage | xe | 6.12 |
| xe hwmon temp2 (pkg) / temp3 (VRAM) | xe | 6.15 |
| xe PMU gt-actual/requested-frequency | xe | 6.16 |
| Panther Lake without force_probe | xe | 6.17 |

## NVIDIA

- [ ] **Driver-only install (`.so.1`-only)** — Precondition: NVIDIA dGPU, proprietary
  driver, **no CUDA toolkit** (only `libnvidia-ml.so.1` exists; the `.so` symlink does not).
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: NVML backend initializes via `lib_path("libnvidia-ml.so.1")`; real device frame
  emitted; no fallback to mock, no panic.
- [ ] **MIG-enabled GPU** — Precondition: MIG-enabled A100/H100.
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: device-level utilization queries returning `NVML_ERROR_NOT_SUPPORTED` render as
  unavailable/`null` — never an error, never a fabricated 0. TUI shows "unavailable".
- [ ] **WSL2 process-list absence** — Precondition: WSL2 with NVIDIA driver.
  Run: `cargo run --release -- --db /tmp/smoke.db` (TUI) and `--json --once`.
  Pass: WSL2 detected; process list empty **with the in-UI hint** explaining per-process
  GPU info is N/A at the driver level; no crash (nvtop #432 class).
- [ ] **Per-metric NOT_SUPPORTED spread** — Precondition: any consumer GeForce (many
  fields unsupported vs datacenter parts).
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: unsupported per-metric fields are `null`, supported ones real; no all-or-nothing
  failure.

## AMD

- [ ] **Real `gpu_metrics` blob captures** — Precondition: each reachable ASIC family
  (Strix-class APU 6.7+, Steam Deck, Cyan Skillfish/BC-250, MI300 6.7+, Vega20, Phoenix).
  Run: `cat /sys/class/drm/card*/device/gpu_metrics > capture-<asic>-<kernel>-<fw>.bin`.
  Pass: capture committed as fixture replacing/augmenting the hand-built synthetic blob;
  record exact PCI ids for the Strix Point (0x150e assumed) and Cyan Skillfish trees.
- [ ] **Steam Deck v2_2/v2_3/v2_4 decode** — Precondition: Steam Deck (Van Gogh); current
  firmware = program-6 → v2_4 size-168 on kernel 6.6+; older fw → v2_2/v2_3.
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: throttle is Some (observed), blob accepted at its real size; GTT (not the ~1 GiB
  UMA carve-out) reflects real memory use once GTT support lands.
- [ ] **Strix-class residency-delta semantics** — Precondition: Strix Point/Halo/Krackan,
  kernel 6.7+ (v3_0), idle desktop.
  Run: `cargo run --release -- --db /tmp/smoke.db`, watch ~5 min idle, then
  `cargo run --release -- report --since 10m --db /tmp/smoke.db`.
  Pass: **no** throttle events at idle (static residency counters → observed quiet); the
  0xFF padding never yields `hw_slowdown`. Under sustained load, only window-active
  reasons fire.
- [ ] **Cyan Skillfish all-FF sentinel** — Precondition: BC-250-class board (v2_2,
  `indep_throttle_status` never written → 0xFF…FF).
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: no permanent all-reasons-throttling; sentinel treated as not-available, falls
  through indep → legacy → none.
- [ ] **MI300: junction temp + XCP partitions** — Precondition: MI300/MI325, kernel 6.7+.
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: temp_c = Some(junction via temp2, sensor surfaced) — not None-despite-data (no
  temp1/edge exists); partitioned XCP topology does not collapse wrongly under
  dedupe-by-PCI-address. Do not claim MI300 support before this passes.
- [ ] **v3_0 PM_TIMER cycle length** — Precondition: Strix hardware + sourced SMU docs.
  Pass: residency-delta → percentage conversion stays unshipped until quantified;
  delta>0 = "engaged during window" is the only claim made meanwhile.
- [ ] **v1_4/v1_6 backports; v1_9 serialization** — Precondition: distro stable kernels;
  any kernel ≥ 6.17 (v1_9 attr-vector).
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: unknown/unhandled versions rejected cleanly — throttle None with reason, no crash,
  other metrics unaffected.
- [ ] **v2_x `average_gfx_activity` units per APU generation** — Precondition: each APU
  generation. Capture blob + a known-load reference before the field is ever surfaced
  (centi-percent vs percent is unconfirmed; currently unread — keep it unread until then).
- [ ] **GFXOFF polling side effect** — Precondition: AMD APU/dGPU that enters GFXOFF.
  Run: `cargo run --release -- --db /tmp/smoke.db` at idle; compare package power /
  GFXOFF residency with and without gpuviewer running (and with `--no-backoff`).
  Pass: adaptive cadence does not hold the GPU out of GFXOFF at idle.

## Intel

- [ ] **RC6/gtidle wakeref measurement (blocks GT-awake%)** — Precondition: i915 dGPU or
  iGPU and an xe GPU.
  Run: poll `gt_act_freq_mhz` / `rc6_residency_ms` / `gtidle/idle_residency_ms` at 1 Hz
  for 5 min at idle; compare C6/RC6 residency slope and package power vs an unpolled run.
  Pass: reading takes no runtime-PM wakeref (residency keeps climbing). If it does, the
  GT-awake% feature stays gated off.
- [ ] **xe act_freq during C6** — Precondition: idle xe GPU.
  Run: `cat /sys/class/drm/card*/device/tile0/gt0/freq0/act_freq` while idle.
  Pass: confirm whether it reads 0 during C6 like i915 (the code assumes same semantics);
  record the answer either way.
- [ ] **xe hwmon channel visibility** — Precondition: B580 on 6.15/6.16 AND an A770
  forced onto xe (DG2-on-xe is a real user config).
  Run: `ls -l /sys/class/drm/card*/device/hwmon/hwmon*/` then
  `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: temp_c = Some(pkg °C via temp2_input) — never None-with-data, never temp3 (VRAM)
  as device temp; power prefers card (index 1), falls back to pkg (index 2), None only
  when both absent; record whether `power1_max` exists per SKU.
- [ ] **Real sysfs/fdinfo captures** — Precondition: A770/i915 6.12+, B580/xe 6.15+, MTL
  laptop (i915 iGPU).
  Run: tar the relevant `/sys/class/drm/cardN` + `/proc/<pid>/fdinfo` subtrees under load.
  Pass: captures committed to replace/augment the synthetic fixture trees.
- [ ] **iGPU graceful absence** — Precondition: MTL (i915) or LNL (xe) laptop.
  Run: `cargo run --release -- --json --once --db /tmp/smoke.db`.
  Pass: no hwmon (iGPU) → temp/power unavailable, not error; no vram region → mem None;
  per-process util still works from fdinfo; util_pct stays None with the PMU-privilege
  rationale.
- [ ] **DG1 force_probe drop release** — Doc-only: find the exact kernel release that
  dropped DG1's force_probe requirement; record in the driver matrix.

## Apple Silicon (v2 — macOS; all under the WWDC26 §4.6 re-check gate)

- [ ] **`In use` vs `Alloc system memory` vs wired budget** — Precondition: Apple Silicon
  Mac + mlx_lm.
  Run: serve a model with `mlx_lm`, log `mlx.core.get_active_memory()`/`get_peak_memory()`
  alongside both IOAccelerator PerformanceStatistics keys; vary
  `sudo sysctl iogpu.wired_limit_mb=N`.
  Pass: identified which key tracks the OOM-relevant wired budget; capture committed as
  fixture. **Blocks the §1.3 budget-pressure narration's final copy.**
- [ ] **Pressure-API blindness to wired growth** — Precondition: one real machine,
  `mx.set_wired_limit()` workload (mlx-lm#883 is a single report).
  Pass: confirmed whether memory-pressure APIs stay false during unbounded wired growth
  before the "pressure APIs are blind to wired memory" copy ships.
- [ ] **Per-chip IOReport channel inventories** — Precondition: M1→M5 spread (M5 renamed
  ECPU→MCPU; M5 ANE channel names unpublished) on macOS 26/27.
  Pass: `GPU Energy`/`ANE*` channel names + unit labels captured per chip/OS into
  `crates/core/tests/fixtures/ioreport/`.
- [ ] **`Device Utilization %` presence matrix** — Precondition: per chip/macOS release.
  Run: `ioreg -r -c AGXAccelerator -d 2 | grep -A3 PerformanceStatistics`.
  Pass: presence/absence recorded; absent → util falls to Tier C GPUPH with the
  DVFS-residency label, never a fabricated number.
- [ ] **Thermal pressure level mapping** — Precondition: a Mac driven to throttle.
  Pass: reproduce that `com.apple.system.thermalpressurelevel` *Heavy* = actually
  throttling while both *Moderate* variants collapse to thermalState `fair`; confirm
  across releases. **`fair` must never be narrated as throttling.**
- [ ] **Fanless-Mac throttle timing** — Precondition: fanless Mac (Air), sustained MLX
  inference. Pass: reproduce (or correct) the third-party ~8–15 min onset / 30–50 %
  tokens/s degradation figures before they appear in any narration copy.

## Cross-cutting

- [ ] **Polling side effects (NVIDIA)** — Precondition: NVIDIA dGPU that idles to low
  power. Run: idle with gpuviewer running vs not; also time per-tick cost (PCIe-throughput
  NVML calls block ~20 ms each).
  Pass: adaptive backoff keeps idle power flat; `--no-backoff` documents the difference.
- [ ] **eGPU hot-unplug → device_lost** — Precondition: Thunderbolt eGPU.
  Run: `cargo run --release -- --db /tmp/smoke.db`, unplug under load, then
  `cargo run --release -- report --since 10m --db /tmp/smoke.db`.
  Pass: device-lost narrated as a Fact event; app keeps running; recording continues for
  surviving devices.
- [ ] **Shutdown grace on a wedged probe** — Precondition: a genuinely hung driver/GPU
  (e.g. wedged after reset failure).
  Run: start the TUI, hit `q`/Ctrl-C while the probe is blocked.
  Pass: exits within the grace window instead of hanging on the stuck probe thread.
- [ ] **Inter-tick CollectorStall on real stalls** — No clock seam exists in tests (G8);
  this is the explicit manual home for it. Precondition: any machine.
  Run: `cargo run --release -- --db /tmp/smoke.db`, then suspend/resume the laptop (or
  `kill -STOP <pid>`, wait 30 s, `kill -CONT <pid>`), then
  `cargo run --release -- report --since 10m --db /tmp/smoke.db`.
  Pass: a CollectorStall self-honesty event records the gap with its duration; charts show
  the gap rather than interpolating through it.
- [ ] **fdinfo privilege hint** — Precondition: Linux box with another user's GPU process
  running. Run: `cargo run --release -- --db /tmp/smoke.db` unprivileged.
  Pass: process table shows the "your processes only" hint; the list is not presented as
  complete.
