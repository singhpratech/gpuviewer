# Tech Stack Evaluation (researched 2026-06)

> Critical evaluation of UI frameworks + architecture patterns for a cross-platform,
> cross-vendor GPU monitor in Rust. Versions verified June 2026.

## UI framework verdicts

### ratatui 0.30 (TUI) — **chosen for v1**
- 20.9k★, extremely active; Chart/Sparkline/Gauge widgets + Braille canvas; v0.30 renders
  Braille over Blocks (stacked charts) — directly useful for a GPU dashboard.
- Proven beautiful in this exact category: bottom (ratatui), btop (the aesthetic benchmark),
  qmassa (ratatui + GPU stats); OpenAI Codex CLI's TUI is ratatui.
- **Works over SSH** — table stakes for ML rigs/headless boxes; no GUI can do this.
- Best distribution story of any option: single static binary, real cross-compilation,
  cargo-dist → brew/scoop/AUR/deb/rpm in days. No signing required.
- Cons: cell-grid beauty ceiling; mouse capture breaks native text selection; pre-1.0 churn.

### iced 0.14 (pure-Rust GUI) — **chosen for v2 GUI**
- The flagship comparable exists: **Sniffnet** (38k★ — more stars than iced itself) is a
  beautiful cross-platform realtime network monitor in iced + plotters-iced — a ready-made
  blueprint (its packaging CI for deb/rpm/dmg/msi is copyable prior art).
- System76 shipped all of COSMIC desktop on iced; wgpu-rendered antialiased Canvas = highest
  polish ceiling of pure-Rust options.
- Cons: zero accessibility (no AccessKit, ~5y open issue); 15-month gap between releases;
  DIY packaging/tray/updater.

### Tauri v2 — rejected (for this app)
- Highest absolute polish ceiling (web charts) + best bundler/tray/updater… **but**:
- **Disqualifying**: WebKitGTK + proprietary NVIDIA driver = blank/black windows on Linux
  (tauri #9394 #9304 #14924; `webkit2gtk-nvidia-quirk` crate exists solely for this).
  A GPU monitor's core Linux users ARE proprietary-NVIDIA users.
- Linux auto-updater covers AppImage only; 150-250MB RAM with webview processes — the
  monitoring-tool demographic notices.

### egui — rejected as primary GUI (fallback option)
- amdgpu_top's GUI is egui; Rerun proves it can carry serious data-viz. AccessKit built in.
- But the honest ceiling reads "developer tool / debug panel" — fighting the imgui aesthetic
  contradicts the "beautiful" goal. Keep as fallback if iced churn becomes untenable.

### Slint — rejected
- **No chart widget at all**; official answer is rasterize-plotters-to-bitmap-per-frame
  (discussions #381/#604/#9518). Worst realtime-chart story of all options.

### Electron — rejected
- ~120MB Chromium floor; the monitor would out-consume what it monitors; this niche's READMEs
  brag about beating Electron. Sensor access via node-gyp is strictly worse than Rust FFI.

## Architecture patterns (adopted)

1. **nvtop's vendor-plugin model, translated to Rust** — `trait GpuBackend` with the same
   clean split nvtop's `struct gpu_vendor` vtable proved: `static_info` (once) /
   `refresh_dynamic` (per tick) / `refresh_processes` (per tick). Per-field `Option<T>`
   replaces nvtop's C validity bitmasks. Registry tries every backend at startup, keeps the
   ones whose `init()` succeeds.

2. **Never hard-link vendor SDKs** — runtime loading only:
   - NVIDIA: `nvml-wrapper` (libloading underneath; loads `libnvidia-ml.so.1`/`nvml.dll` at
     `Nvml::init()`, clean Err when absent — the model bottom shipped default-on).
   - AMD Linux: **direct sysfs/hwmon/gpu_metrics/fdinfo parsing** — zero library deps.
     Avoid librocm_smi64 (soname churn broke btop twice: #774, #1540).
   - Intel Linux: sysfs/hwmon + fdinfo baseline; optional Level Zero Sysman via libloading
     (`libze_loader.so.1`/`ze_loader.dll`) — no mature L0 crate exists, own ~30 `zes_*`
     bindings via bindgen --dynamic-loading; per-field "Sysman overrides sysfs when fresh"
     merge (all-smi's pattern).
   - Windows per-process (all vendors): PDH `GPU Engine`/`GPU Process Memory` counters +
     `D3DKMTQueryStatistics` via official `windows` crate — vendor-agnostic WDDM data,
     same source as Task Manager.
   - Apple: hand-rolled FFI to private `libIOReport.dylib` + IOKit AGXAccelerator
     `PerformanceStatistics` + SMC — port macmon's `sources.rs` (~300-500 lines, MIT).

3. **Single process by default, daemon optional later** — start btop/bottom-style
   (collector thread → store → UI) but structure as `core` + frontend crates from day one
   (Mission Center/Magpie's lesson). A later `--serve` mode exposes the same core over
   local HTTP/Prometheus; buys GUI/web/remote/privilege-separation without forcing a daemon
   on casual TUI users. If networked mode ships: auth + loopback-bind from day one
   (glances/LACT both shipped unauthenticated TCP and it's their top security complaint).

4. **History: rings + SQLite rollups** — fixed-capacity `VecDeque` rings for the live window
   (full resolution); background fold into bundled-`rusqlite` rollup tables at 10s/1m/1h
   tiers with retention pruning (beszel's scheme; netdata tier ratios ~14d/3mo/1y as sizing
   reference). One timestamp per collection frame, not per metric (avoids chart jitter).
   No DuckDB on the write path; no custom TSDB.

5. **CI without GPUs** — `MockBackend` implementing the same trait (all-smi's mock-server
   pattern: scripted values + drift; env-var fake MIG/vGPU modes); sysfs/fdinfo parsers take
   a root-dir parameter and run against committed fixture trees captured from real hardware;
   NVML plumbing against NVIDIA's nvml-mock / fake-gpu-operator in a container job. This
   already exceeds nvtop/nvml-wrapper CI (which is compile+clippy only).

## Key risks carried forward

- NVML symbol/struct versioning (`_v2`/`_v3` ABI churn) — replicate nvtop's dlsym fallback
  chains; wrong signature through libloading is UB. nvml-wrapper 0.12.1's silent-corruption
  fix (CUDA 12 vs 13 field-ID drift) shows this is a live hazard.
- Apple private-API drift — every new chip/macOS renames IOReport channels (M5 MCPU*, Ultra
  DIE_N_ prefixes, macOS 26 format change). Budget per-release fixture updates.
- Polling cost: NVML PCIe throughput calls block ~20ms each; AMD GRBM register polling breaks
  GFXOFF; NVIDIA polling keeps GPUs awake (bottom #1291). Tiered sampling cadence + idle
  detection are design requirements, not nice-to-haves.
- GUI route later requires 3-OS CI matrix + macOS notarization + Windows code signing.
