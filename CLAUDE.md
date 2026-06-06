# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

**gpuviewer** — "the GPU flight recorder": a cross-vendor GPU monitor whose differentiator is
**persistent history + narrated events** (throttle onset with cause, VRAM→OOM trend with ETA,
process lifecycle, training idle gaps), not live gauges. The wedge is the replayable timeline:
"scroll back to 02:14 and see why the run stalled." Read `docs/research/04-synthesis.md` first —
it is the product/architecture decision record; the other files in `docs/research/` are the
supporting market/API/stack evidence (researched June 2026, with issue numbers).

## Commands

```sh
cargo build                 # build all crates
cargo test                  # all tests run against the mock backend — no GPU needed
cargo test -p gpuviewer-core --lib          # single-crate tests
cargo run --release -- --mock               # TUI with simulated GPUs (the demo)
cargo run --release -- --json --once --mock # one NDJSON frame to stdout (CI/scripting)
cargo run --release -- --json --mock --interval 100   # fast-forward sim, NDJSON stream
```

Binary is `gpuviewer` (crate `gpuviewer-tui`). With no real backend available the mock is
the automatic fallback, so the TUI always renders.

## Workspace layout

- `crates/core` — model (`model.rs`), `GpuBackend` trait + registry (`backend.rs`),
  event derivation (`events.rs`), mock simulation (`mock.rs`). Zero deps except serde.
- `crates/history` — `DeviceHistory` ring + `HistoryStore` (events log). SQLite rollups next.
- `crates/tui` — `collector.rs` (Engine = tick loop shared by TUI thread and `--json` mode),
  `app.rs` (event loop), `ui.rs` (tabs/charts/gauges/process table/story feed).

## Roadmap sequencing (do not reorder casually)

v1 = Linux only (NVIDIA/AMD/Intel), TUI + `--json`, history + events. v1.5 = Windows NVIDIA.
v2 = macOS Apple Silicon (device-level only) + iced GUI. Punted entirely from v1: control
features (fan/OC — LACT owns it), daemon/client split, Prometheus exporter, cluster views,
macOS per-process GPU (OS-prohibited — see below).

## Architecture decisions (settled — re-litigate only with new evidence)

- **Workspace of three crates**: `gpuviewer-core` (collection), `gpuviewer-history`
  (rings + SQLite), frontend (`gpuviewer-tui`, ratatui 0.30). GUI later is **iced**
  (Sniffnet's playbook); **Tauri was rejected** (WebKitGTK blank-window bug on proprietary
  NVIDIA — tauri#9394 — hits exactly our users), Slint rejected (no chart widget),
  Electron rejected.
- **`trait GpuBackend`** mirrors nvtop's vtable split: `static_info` (once) /
  `refresh_dynamic` (per tick) / `refresh_processes` (per tick); per-field `Option<T>`,
  never validity bitmasks. Explicit backend registry (`all_backends()`), no
  inventory/ctor magic. Dedupe devices across backends by PCI address.
- **Never hard-link a vendor SDK.** NVIDIA via `nvml-wrapper` (must init with
  `Nvml::builder().lib_path("libnvidia-ml.so.1")` — the `.so` symlink only exists with CUDA
  toolkit installed). AMD Linux via direct sysfs/hwmon/`gpu_metrics`/fdinfo parsing — do NOT
  link librocm_smi64 (soname churn broke btop twice). Intel Linux via fdinfo + sysfs with
  **both i915 and xe dialects** (different fdinfo keys: `drm-engine-*` ns vs `drm-cycles-*`).
  Future dynamic loads via `dlopen2` with `Option<fn>` fields, signatures bindgen'd from
  official headers.
- **History**: RAM ring buffers for the live window; downsampled 10s/1m aggregates
  batch-inserted into SQLite (`rusqlite`, bundled, WAL) + append-only event log. Never write
  raw 1Hz samples to SQLite. One timestamp per collection frame, not per metric.
- **Events are two-tier**: facts (throttle bit set, process exited) asserted plainly;
  inferences (dataloader stall) always labeled "likely" and expandable to raw evidence.
  A confidently-wrong narration kills the product's trust thesis.

## Domain rules that look like bugs but aren't

- `NVML_ERROR_NOT_SUPPORTED` is a **normal per-metric outcome** — render "unavailable",
  never fail. Same for absent sysfs files, missing hwmon (Intel iGPU has none), MIG-enabled
  GPUs (device-level utilization queries legitimately return NOT_SUPPORTED).
- WSL2: per-process GPU info is N/A **at the driver level** — detect WSL2 and explain the
  absence in-UI; never crash on it (nvtop #432 is the cautionary tale).
- Polling has side effects: NVML PCIe-throughput calls block ~20ms each; AMD GRBM register
  polling breaks GFXOFF; NVIDIA temp polling keeps GPUs awake (bottom #1291). Adaptive
  cadence and opt-in perf-counter polling are requirements.
- NVML "utilization" is duty-cycle (time ≥1 kernel resident), not capacity — label it
  honestly in the UI; never present it as saturation.
- fdinfo per-process for **other users'** processes needs root/CAP_SYS_PTRACE — show
  "your processes only" hint when unprivileged, don't pretend the list is complete.
- AMD `gpu_metrics` is a versioned packed binary struct (v1.0–v3.0) with per-version field
  offsets AND units (C vs centi-C, W vs mW); decoders are per-version, fixtures required.
- Kernel-version gates (degrade gracefully): AMD fdinfo 5.14+/5.19+; Intel i915 engine 5.19+,
  per-process memory 6.8+, xe engine cycles 6.11+, xe PMU 6.15+.

## Testing strategy (CI has no GPUs)

- `MockBackend` implements `GpuBackend` with scripted streams — all history/event/UI tests
  run against it.
- sysfs/fdinfo collectors take a **root-dir parameter**; tests run against committed fixture
  trees captured from real hardware (`tests/fixtures/`). Capture new fixtures per
  kernel/driver release.
- NVML loader plumbing tested against a CI-built stub `.so` exporting NVML symbols.
- Real-hardware smoke tests are a manual pre-release checklist, not CI.
