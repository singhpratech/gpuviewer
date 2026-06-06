# gpuviewer — Adversarial Synthesis Report

---

## 1. GAP ANALYSIS

### 1.1 Separating real gaps from fragmentation noise

Be skeptical of the headline gap ("no tool covers 4 vendors × 3 OSes"). Most users run **one vendor on one OS**. The full matrix is a distribution/marketing asset, not a wedge — and chasing it first means re-implementing nvtop on Linux (where nvtop is already excellent, free, and in every distro repo) while shipping degraded experiences everywhere else. Sorting the claimed gaps:

**NOT real gaps (fragmentation, or already served):**
- *Per-process GPU attribution on Linux.* nvtop does this across 12 vendors via NVML + DRM fdinfo. amdgpu_top does it deeper for AMD. A new tool matching this is table stakes, not differentiation.
- *"Beautiful" monitoring on Linux.* Mission Center (GTK4, per-app GPU columns, active 1.x cadence) substantially fills the "Task-Manager-but-better" slot. You can do better, but it's not unserved.
- *macOS device-level monitoring.* macmon, mactop v2, Stats are active and sudoless. Crowded.

**REAL gaps (validated by demand signals + zero shipping competitors):**

1. **Persistent history + event narration between `watch nvidia-smi` and Prometheus/Grafana.** Nothing answers "what did my GPU do during last night's training run, and why did util drop at 02:14?" without standing up a 4-component observability stack. GPU Hot hit the HN front page (1.5k stars) on exactly this pitch and then shipped *no persistence, no alerting* — demand validated, solution absent. The only "explainer" attempts are Meta's Zoomer (internal, unreleased) and Ingero (0 stars, more blog posts than commits). **The niche is simultaneously validated and empty.**
2. **Health/failure signals as human-readable events.** Throttle reasons exist in NVML as a hex bitmask nobody surfaces; Xid/ECC/row-remap — the things that actually kill training runs — are reachable only via DCGM (datacenter GPUs + root daemon) or dmesg grep. No lightweight tool says "GPU3 began thermal throttling at 02:14; clocks 1980→1410 MHz."
3. **Honest per-process compute monitoring on native Windows.** Task Manager shows ~3% for a maxed CUDA workload (3D-engine default graph; HAGS removes the CUDA graph entirely); nvitop is NVIDIA-only Python TUI with WDDM VRAM holes; AMD/Intel have *nothing* in terminal form. Real gap — but the underlying Windows data sources are the weakest of any platform (see §2), so it's a v2 expansion, not the wedge.
4. **VRAM-pressure / model-fit storytelling.** "Will this model fit; am I trending toward OOM" — gpuer (42 stars, weeks old, "vibe coded") exists *because* nothing else shows wired-limit headroom on macOS; no tool anywhere trends VRAM toward OOM as a first-class signal.

**Gaps that look real but are tar pits:**
- *Per-process GPU on macOS.* Not a market failure — an **OS prohibition**. powermetrics' per-process GPU ms/s reads 0 on Apple Silicon; `task_for_pid` needs root + SIP exemption; XNU rusage has no GPU field (verified through RUSAGE_INFO_V6); Activity Monitor uses unreplicated private sysmond plumbing. mactop v2's AGX AppUsage scraping is approximate, rescaled, and experimental. Anyone promising this honestly cannot deliver it.
- *Consumer-tier true saturation (SM occupancy/SOL%).* DCGM_FI_PROF_* requires datacenter SKUs; NVML GPM is Hopper+. On GeForce you cannot escape duty-cycle util — you can only *label it honestly* and contextualize it.

### 1.2 The single sharpest wedge for v1

> **The GPU flight recorder: a single static Linux binary (NVIDIA/AMD/Intel) that persistently records GPU telemetry and renders it as an annotated, replayable timeline of events — throttle onset with reason, VRAM climb toward OOM, process start/stop/kill, training idle gaps — instead of live gauges that die with the terminal.**

Why this wedge and not the others:
- It is the **direct embodiment of the product thesis** ("tell the story, not gauges") — and the thesis survives stress-testing precisely because story-telling = *history + events + attribution*, and history+events is the one axis where every incumbent (nvtop, btop, nvitop, GPU Hot, Task Manager) ships nothing.
- It is differentiating **even as a TUI** in a saturated TUI market. "Yet another gauges TUI" would be ignored; "scroll back to 02:14 and see why the run stalled" has no competitor at any price below Grafana.
- The required telemetry (throttle bitmask, per-process VRAM, process lists, fdinfo engine deltas) is **fully obtainable unprivileged on Linux** for NVIDIA, and mostly for AMD/Intel — no dependence on the broken corners of the matrix.
- Cross-platform/cross-vendor breadth then becomes the *expansion path* riding on a differentiated core, rather than a checkbox pursued with an undifferentiated one.

---

## 2. FEASIBILITY VERDICT — platform × vendor matrix

Legend: **GREEN** = ship with confidence; **YELLOW** = obtainable with caveats/dual sources; **RED** = not honestly obtainable.

| Combo | Device telemetry | Per-process util | Per-process memory | Throttle/health events | Privileges | Verdict |
|---|---|---|---|---|---|---|
| **Linux + NVIDIA** | NVML (util, VRAM, power, temps, clocks, fan, enc/dec, PCIe, NVLink fields, ECC on datacenter) | NVML `nvmlDeviceGetProcessUtilization` (Maxwell+; weak semantics, known 0% bugs — nvtop #177) | NVML compute/graphics process lists, all users, accurate | Throttle-reasons bitmask (`GetCurrentClocksEventReasons`) works on **consumer** Kepler+; Xid via dmesg/`-q` only | None for all reads | **GREEN** — the anchor platform |
| **Linux + AMD** | sysfs/hwmon + `gpu_metrics` binary table (SMU-era ASICs: Navi10+/Renoir+; per-version decoders required; absent on Polaris/Vega10) | DRM fdinfo engine-ns deltas (kernel ≥5.14, standardized ≥5.19); **ROCm/KFD compute shows 0%** — must add `/sys/class/kfd/kfd/proc` | fdinfo `drm-resident-*`/VRAM/GTT keys (kernel ≥6.4) | `gpu_metrics` throttle_status bitmask; throttle events otherwise dmesg-only | sysfs world-readable; **other users' fdinfo needs root/CAP_SYS_PTRACE**; GRBM polling breaks GFXOFF (sample sparingly) | **GREEN**, kernel-version-gated |
| **Linux + Intel** | sysfs freq unprivileged; hwmon power/temp/fan **dGPU-only** (and only on recent kernels: xe temps 6.15, fans 6.16); iGPU power = root-only RAPL | fdinfo: i915 `drm-engine-*` (≥5.19), xe `drm-cycles-*` (≥6.11) — two parsers required | fdinfo memory regions (≥6.8) | L0 Sysman freq throttle reasons (engine metrics need CAP_PERFMON; gappy) | Device-wide engine util via PMU = **root/CAP_PERFMON**; fdinfo aggregation is the unprivileged workaround | **YELLOW-GREEN** — per-process fine, device power/util need privilege or approximation |
| **Windows + NVIDIA** | NVML works (util, temps, power, clocks, **throttle reasons**) | NVML per-PID sm/enc/dec works under WDDM (pmon proves it) | **NVML VRAM = always N/A under WDDM** (GeForce can't enter TCC) → must dual-source PDH "GPU Process Memory"/D3DKMT | Throttle bitmask works | No admin for reads | **YELLOW-GREEN** — mandatory NVML+PDH dual-source |
| **Windows + AMD** | ADLX (ships with Adrenalin driver; usage, hotspot/VRAM temps, board power, fans; capability-check every metric; custom EULA) | PDH "GPU Engine" counters only (engine-type util, Task Manager's source) | PDH/D3DKMT per-process memory | ADLX has no event surface; clocks/temps deltas only | No admin for telemetry | **YELLOW** |
| **Windows + Intel** | IGCL (no admin; but Battlemage counters broken: VRAM/card energy=0 #138, mem bandwidth=0 #120, PCIe structs=0 #149); L0 Sysman needs **Administrator** for temp/power on Windows | PDH only | PDH/D3DKMT | IGCL throttle flags (power/temp/current-limited) | IGCL no admin; Sysman admin | **YELLOW** — prefer IGCL, expect per-SKU breakage |
| **macOS + Apple Silicon (device-level)** | Sudoless: IOReport private dylib (GPU/ANE/CPU power, freq-state residency), AGXAccelerator `PerformanceStatistics` (Device Utilization %, GPU-used unified memory), SMC temps/fans; Metal `recommendedMaxWorkingSetSize` for memory cap | — | — | Thermal pressure via NSProcessInfo; freq-residency drops as throttle proxy | None — but **all private API**, MAS-rejection guaranteed, per-chip-generation breakage (M5 MCPU channels, Ultra DIE_N prefixes) | **GREEN (device only)** |
| **macOS per-process GPU** | — | powermetrics GPU ms/s **reads 0 on Apple Silicon**; task_for_pid = root+SIP; no XNU rusage GPU field; mactop's AGX AppUsage scrape = experimental/approximate | **Does not exist** in any public API | — | Even root doesn't fix it | **RED — do not promise** |
| **WSL2 (any)** | Device-level NVML mostly works | Process lists empty/N/A at driver level (microsoft/WSL #9938/#11277) — not fixable in-tool | N/A | Partial | — | **RED for per-process** — detect WSL2, degrade with an honest in-UI explanation |

**Hardest combos, stated plainly:** (1) macOS per-process — impossible honestly; ship device-level + unified-memory/model-fit story instead. (2) Windows AMD/Intel — feasible but built on a brittle tripod (ADLX EULA + IGCL per-SKU bugs + PDH enumeration cost); sequence after Linux and Windows-NVIDIA. (3) WSL2 — unsolvable per-process at the driver level; the only "feature" available is being the first tool to *explain the absence* instead of crashing on it (nvtop #432).

---

## 3. STACK RECOMMENDATION

### The argument, critically

**GUI-first** says: "beautiful UX" is the stated goal; the TUI niche is saturated (nvtop/btop/nvitop/gpustat/amdgpu_top/nviwatch); terminals cap beauty at braille resolution in the user's font; every month in the terminal delays the differentiator. This argument would win **if the differentiator were visual polish**. It is not — §1 establishes the differentiator is *history + narration*, which renders fine in cells. And GUI-first triples the day-one surface: packaging (dmg/msi/deb/rpm/AppImage), signing/notarization ($100–400/yr + CI runners per OS), and a rendering-stack risk that lands directly on this audience — **Tauri/WebKitGTK's DMABUF renderer produces blank windows on Linux + NVIDIA proprietary drivers** (tauri#9304/#9394, webkit 228268). A GPU monitor that must disable GPU acceleration to render on NVIDIA is a brand-destroying first impression. **Tauri is rejected outright; Electron is rejected** for the footprint irony (a 200–500MB-RAM monitor topping its own process list, à la NZXT CAM's reputation).

**TUI-first** says: the audience for "beat nvtop" is ML engineers SSH'd into Linux boxes, where a TUI is the product, not a compromise; a single static binary ships from one CI runner in weeks with zero signing ceremony; and it forces building the *actual hard problem* — the per-vendor collection layer and history engine — before any pixel debate. The saturation objection applies to gauges-TUIs, not to a flight recorder; bottom (13.4k stars) and nvtop (10.7k) prove the distribution channel works.

**Both-from-shared-core** is not a hedge — it's the proven architecture: amdgpu_top ships ratatui TUI + egui GUI + `--json` from one workspace; Mission Center independently converged on the same collector/frontend split.

### Commitment

**TUI-first from a shared headless core, GUI as the explicit, scheduled second act (not "someday").**

Concrete structure — three crates from day one (per the nvtop/Magpie convergent pattern):

1. **`gpuviewer-core`** — `trait GpuBackend` copying nvtop's vtable cut exactly (init / devices / populate_static / refresh_dynamic / refresh_processes / last_error; static-vs-dynamic split independently validated by Mission Center's D-Bus API). Explicit `all_backends() -> Vec<Box<dyn GpuBackend>>` — **no** ctor/inventory constructor magic. Dedup multi-backend device enumeration by PCI address (nvtop's `pdev`). Backends:
   - NVIDIA: **`nvml-wrapper` 0.12** (libloading inside; must use `Nvml::builder().lib_path("libnvidia-ml.so.1")` — the exact bottom workaround for driver-only installs).
   - AMD Linux: hand-rolled `std::fs` readers for sysfs/hwmon/`gpu_metrics`/fdinfo (skip rocm_smi entirely — SONAME churn, btop #774; optionally **`libdrm_amdgpu_sys`** for ioctl extras; **`libamdgpu_top`** as reference).
   - Intel Linux: one fdinfo parser handling both i915 `drm-engine-*` and xe `drm-cycles-*` dialects + sysfs freq (qmassa/qmlib is the reference implementation).
   - Later vendors loaded via **`dlopen2`** `#[derive(WrapperApi)]` with `Option<fn>` fields — nvtop's NULL-checked-dlsym pattern with zero boilerplate; signatures bindgen'd from official headers in no-link `-sys` crates (hand-written dlsym signatures are instant UB).
   - CPU/disk/net context: **`sysinfo`** (GPU explicitly out of its scope — parallel subsystem, never an extension).
2. **`gpuviewer-history`** — RAM ring buffer for the live window (bottom's retention model) + downsampled 10s/1m aggregates batch-INSERTed into **SQLite via `rusqlite` (WAL mode)** + an append-only event log (throttle/process/OOM events). Never write raw 1Hz samples to SQLite — that's precisely why netdata built dbengine. Skip DuckDB.
3. **Frontends** — v1: **ratatui 0.30 + crossterm** (Chart/Sparkline/Braille, layout-cache; bottom and amdgpu_top prove the niche) plus a `--json` streaming mode (amdgpu_top precedent — the cheap scripting/remote escape hatch). v2: **iced** GUI with plotters-iced, wgpu + tiny-skia CPU fallback, copying **Sniffnet's CI matrix** verbatim (MSI x64/arm64, DMG Intel/AS, AppImage/deb/rpm — the entire packaging problem already solved in public at 38k stars). Swap to egui only if accessibility becomes a hard requirement (iced #552: zero a11y since 2020) — accept egui's more utilitarian ceiling in exchange.

Testing (the ecosystem's actual weak point — nvtop tests only layout math): `FakeBackend` with scripted metric streams (DCGM's fake-GPU injection validates the pattern); recorded sysfs/fdinfo fixture trees with root-path-parameterized collectors; `cargo test --no-run` compile-gating on all three OSes; a CI-built stub `.so` exporting NVML symbols to exercise loader probe/fallback paths; and a written real-hardware pre-release checklist, because no one has solved hardware-free driver truth.

---

## 4. "STORY-TELLING" FEATURE DESIGN — what is actually derivable

Ranked by value ÷ effort. Every event must carry its raw evidence (the narration layer's credibility depends on being auditable — see Risk #2).

| # | Feature | Derivation | Value/Effort | Verdict |
|---|---|---|---|---|
| 1 | **Throttle events with cause** — "GPU0 thermal throttling began 02:14:31, SM clock 1980→1410 MHz, 84°C vs 83°C slowdown threshold" | NVIDIA: throttle-reasons bitmask (consumer Kepler+, unprivileged) + clock/temp/threshold deltas. AMD: `gpu_metrics` throttle_status. Intel: IGCL flags / Sysman freq throttle reasons | **Very high / Low** | **v1 flagship.** Data exists in every incumbent; none decodes the bitmask into an event. Use tolerant decoding (new driver bits appear) |
| 2 | **Process lifecycle on the timeline** — "python (PID 4521) attached to GPU0, +8.2 GB; exited 03:40, freed 8.2 GB" | Deltas of NVML process lists (all users, unprivileged) / fdinfo client sets | **High / Low** | v1. The narrative spine everything else hangs on |
| 3 | **VRAM pressure & OOM-risk** — "GPU0 VRAM 91% and climbing ~120 MB/min (python:4521); full in ~9 min" | Per-process VRAM trend + linear slope; flag the climbing PID. macOS variant: GPU-wired memory vs `recommendedMaxWorkingSetSize` / `iogpu.wired_limit_mb` headroom + "fits a ~13B model" framing (gpuer-validated demand) | **High / Low-Med** | v1. The single most-wanted mid-training signal per the MLOps synthesis |
| 4 | **"What changed" state diffing** — power-limit changed, P-state shift, fan ramp, clocks pinned, ECC count incremented | Threshold/edge detection on already-sampled state | High / Low | v1. Cheap once the event pipeline exists |
| 5 | **Training idle-gap detection** — "GPU0 idle 41s every ~10 min while python's CPU spiked → likely dataloader/validation; concurrent disk-write burst → likely checkpoint" | Util-gap detection + correlation with owning process CPU% (sysinfo) and disk I/O in the same window | **High / Medium** | v1.5. Heuristic — ship with explicit confidence labels ("likely"), never as fact. At 1 Hz you catch multi-second gaps (checkpoints, validation, epoch boundaries), not microbursts — say so in the UI |
| 6 | **Honest-utilization framing** | Label NVML util as duty-cycle ("time ≥1 kernel resident — not capacity") in-UI; on Hopper+ add NVML GPM SM-activity/occupancy | Med / Low (label) — Med (GPM) | Label in v1; GPM v2. Counters the category's most-repeated criticism at near-zero cost. True SOL% on GeForce: **not derivable — don't fake it** |
| 7 | **Xid / ECC failure events** — "GPU3 Xid 63 at 02:14 — row remap pending" | NVML ECC/retired-page counters (datacenter SKUs); Xid via dmesg/journal (often gated by `dmesg_restrict`) | High (trainers) / Medium | v2, best-effort with privilege detection. The earliest job-killer warning per cluster playbooks |
| 8 | **Per-process attribution of util drops** | NVML per-PID smUtil (unreliable under concurrency, pmon-mismatch bugs); AMD/Intel fdinfo engine deltas (good) | Med / Med | Supporting evidence under events, never headline numbers on NVIDIA |
| 9 | **Live causal "why" (Zoomer-class)** — eBPF/trace-correlated root-causing | — | High / **Very High** | **Punt.** Validated-empty for a reason; items 1–5 deliver 80% of the perceived value |

The replayable timeline (scrub back through last night with events overlaid) is the container making 1–7 coherent — it is the product, enabled by the history crate.

---

## 5. TOP RISKS, RANKED

1. **Data-source fragility across drivers/kernels/SKUs.** NVML struct-version garbage (v1/v2/v3 PID corruption), per-process 0% bugs, AMD `gpu_metrics` per-version layouts, i915-vs-xe dialects, IGCL Battlemage zeros, field-ID drift (the nvml-wrapper 0.12.1 silent-corruption bug). *Mitigation:* treat `NOT_SUPPORTED` as a normal per-metric outcome never fatal; tolerant bitmask decoding; per-version fixture tests; FakeBackend for everything above the collectors; real-hardware pre-release checklist; per-metric capability flags in the UI so absence reads as "unavailable on this stack," not breakage.
2. **Narration that's wrong kills the product.** One confidently-wrong "dataloader stall" and the story layer becomes "the tool that lies" — fatal for a thesis built on trust. *Mitigation:* two-tier events — facts (throttle bit set, VRAM slope, process exit) asserted plainly; inferences always hedged ("likely"), expandable to raw evidence; conservative thresholds; user-tunable sensitivity.
3. **Polling side effects — the monitor changing what it measures.** bottom #1291 (NVML temp polling blocks GPU sleep, raises idle power), AMD GRBM reads break GFXOFF, NVML PCIe-throughput calls block ~20ms each. *Mitigation:* adaptive cadence (slow to 10s+ when idle), batched `field_values` reads, perf-counter polling opt-in, an explicit "low-impact mode," and document measured self-impact — turn the risk into a credibility feature.
4. **Wedge mis-aim: shipping gauges into a saturated market.** If v1 ships before the history+events pipeline is the headline, it's nvtop-but-newer and dies. *Mitigation:* timeline + event log are v1 acceptance criteria, not stretch goals; the demo artifact is "replay last night," never a live dashboard screenshot.
5. **Privilege walls silently truncating the story.** Other-users' fdinfo needs root/CAP_SYS_PTRACE (AMD/Intel per-process shows only your processes); Intel device-util needs CAP_PERFMON; RAPL is root-only. *Mitigation:* detect and state it in-UI ("showing your processes only — run with sudo or `setcap` for all"); optional minimal setuid/setcap helper later (Mission Center's gatherer model), never required.
6. **History engine self-inflicted wounds.** Raw 1Hz inserts → fsync storms (the exact reason netdata wrote dbengine); unbounded growth → disk complaints. *Mitigation:* RAM hot window; downsample-then-batch into SQLite WAL; fixed retention caps with visible config; corruption-tolerant open (drop history, never fail to start).
7. **Apple private-API treadmill.** IOReport channel renames per chip generation (M5 MCPU, Ultra DIE_N), MAS distribution impossible. *Mitigation:* device-level-only scope; lean on the maintained `macmon` crate rather than re-deriving FFI; per-chip-family smoke tests each hardware cycle; distribute via brew.
8. **Windows expansion underestimation.** Mandatory NVML+PDH dual-sourcing, ADLX custom EULA review, IGCL per-SKU breakage, PDH wildcard enumeration cost. *Mitigation:* sequence Windows-NVIDIA as a discrete milestone after Linux traction; ground-truth Intel counters against the compute-runtime #932 matrices before claiming support.
9. **iced churn / zero accessibility** (breaking releases 0.12→0.14; #552 open since 2020). *Mitigation:* GUI deferred to v2 anyway; pin versions, budget upgrade time; pre-committed egui fallback criterion.
10. **CI cannot see real hardware** — the ecosystem norm is shipping driver bugs to users (nvtop, btop, Mission Center all did). *Mitigation:* fixtures + stub-`.so` loader tests catch the catchable; accept user reports as the hardware matrix and optimize triage (built-in `--report` diagnostic dump command).

---

## 6. WHAT TO PUNT IN v1

| Punted | Why | Revisit |
|---|---|---|
| **All control features** (fan curves, OC, power limits) | LACT owns it on Linux; requires root writes; hardware-damage liability; orthogonal to the wedge | v3, if ever |
| **Native Windows** (entirely) | Dual-source NVML+PDH plumbing + ADLX/IGCL brittleness would consume the v1 schedule; wedge audience is Linux-first | v1.5: Windows NVIDIA; v2: AMD/Intel via PDH+ADLX/IGCL |
| **macOS** (entirely) | Device-level is a crowded space (macmon/Stats); per-process is RED; private-API maintenance tax | v2: device-level + the model-fit/wired-limit memory story (the one unserved Mac angle) |
| **Per-process GPU on macOS, ANE utilization, WSL2 per-process workarounds** | OS-prohibited / driver-prohibited. Honesty here is the feature | Only if Apple/Microsoft ship APIs |
| **GUI** | §3 — TUI-first; the core and history engine are the hard, differentiating work | v2 via iced + Sniffnet CI, committed not "someday" |
| **Multi-node/cluster aggregation, Prometheus exporter, alerting/notifications** | Each drags toward the DCGM/Grafana fight on incumbents' turf; `--json` streaming over ssh is the v1 escape hatch | v2+: exporter is cheap adoption fuel once core is stable |
| **True saturation metrics (SOL%/occupancy), DCGM integration, eBPF causal tracing** | Locked to datacenter SKUs / Hopper+ / heavyweight; no mature Rust DCGM bindings | GPM on Hopper+ in v2; DCGM never, unless fleet features emerge |
| **Exotic vendors** (Jetson, NPUs, Qualcomm, Tenstorrent) | nvtop's breadth moat; near-zero wedge-audience overlap | Opportunistic, post-v2 |
| **Daemon/client split** | Mission Center's tracker is the catalog of its failure modes (socket dial failures, gatherer hangs, activation breakage); single-process with clean crate boundaries defers the cost | When GUI+TUI concurrency or root-helper demands it — the three-crate structure keeps the door open |
| **Mac App Store / signed-installer ceremony** | TUI binaries via GitHub releases/brew/cargo/AUR need none of it | With the v2 GUI |

**v1, in one sentence:** a single static Rust binary for Linux NVIDIA/AMD/Intel — ratatui TUI + `--json` — that records GPU history to a local ring-buffer/SQLite store and narrates throttling, VRAM pressure, process lifecycle, and idle gaps as an auditable, replayable timeline; everything else is sequenced behind proving that story.