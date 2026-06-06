# Vendor Telemetry APIs — what is actually obtainable (researched 2026-06)

> Per-vendor deep dive: every metric source, per-process attribution reality, privilege
> requirements, Rust support, and the caveats that broke other tools.

## NVIDIA — NVML (Linux + Windows)

**Access**: dlopen `libnvidia-ml.so.1` (never the `.so` symlink — driver-only installs lack it;
bottom hit this) / `nvml.dll` (System32 on R445+ drivers). WSL2: `/usr/lib/wsl/lib/`.
Backward-compatible versioned entry points (`_v2`/`_v3`).

**Device metrics (unprivileged)**: utilization (gpu%/mem-controller%), memory (`_v2` adds
reserved), power (Hopper+ distinguishes AVERAGE vs INSTANT field values), temp + thresholds,
clocks, **throttle/clocks-event reasons bitmask** (GpuIdle, SwPowerCap, HwThermalSlowdown,
HwPowerBrakeSlowdown, SyncBoost… — works on consumer GPUs, Kepler+), fan duty% + RPM (recent
drivers), PCIe throughput (**each call blocks ~20ms — sample sparingly**), NVLink via field
values (old per-link counter API deprecated on Ampere+), encoder/decoder util, ECC counters
(datacenter parts), `nvmlDeviceGetSamples` (driver-buffered recent samples — recovers short
history between polls), GPM (Hopper+: DCGM-style SM activity/occupancy via plain NVML!).

**Per-process**:
- PIDs + VRAM: `GetComputeRunningProcesses_v3` / `GetGraphicsRunningProcesses_v3`. Works
  unprivileged for all users' processes on Linux. **On Windows WDDM `usedGpuMemory` is ALWAYS
  N/A** (Windows kernel owns VRAM; GeForce can't switch to TCC) → fall back to PDH
  `GPU Process Memory` counters (Task Manager's source).
- Per-PID utilization: `GetProcessUtilization` (Maxwell+) → smUtil/memUtil/encUtil/decUtil
  samples since `lastSeenTimeStamp`. Works under WDDM too (it's what `nvidia-smi pmon` shows).
  Semantics weak: values unreliable with concurrent processes; NOT_FOUND when idle is normal.
- Lifetime stats: accounting API — but enabling it needs root and doesn't survive driver reload.
- MIG: device-level utilization queries return NOT_SUPPORTED when MIG enabled — must check
  `mig_mode()` first or the tool shows errors on MIG boxes. Per-instance util needs DCGM.

**Rust**: `nvml-wrapper` 0.12.1 (2026-03) — broad coverage incl. throttle reasons, field values,
process utilization, GPM, samples. Use `Nvml::builder().lib_path("libnvidia-ml.so.1")`. Active
but slow-cadence maintenance; 0.12.1 fixed silent data corruption from CUDA 12/13 field-ID
drift (live hazard, pin carefully). No usable DCGM Rust bindings exist — NVML-only is correct
for a desktop tool.

**Treat `NVML_ERROR_NOT_SUPPORTED` as a normal per-metric outcome, never fatal.**

## AMD — sysfs/fdinfo first (Linux), ADLX + PDH (Windows)

**Linux, no library needed (all world-readable)**:
- `gpu_busy_percent`, `mem_busy_percent`, `mem_info_vram_*/gtt_*`, hwmon (edge/junction/mem
  temps, power µW + cap, sclk/mclk, fan RPM/PWM), `pp_dpm_*` DPM tables, PCIe link.
- `gpu_metrics` binary table: one atomic read = all sensors incl. **throttle status bitmask**,
  per-engine activity, socket power, energy accumulator. Versioned packed struct
  (v1.0–v3.0) with per-version offsets AND unit changes — need per-version decoders
  (amdgpu_top's are the reference). Only SMU-era ASICs (Navi10+/Renoir+/Vega12/20).
- libdrm `AMDGPU_INFO` ioctls on the render node (sensors, VRAM usage, GRBM pipe-busy
  counters — **GRBM polling keeps GPU out of GFXOFF; make it opt-in** like amdgpu_top's
  `--no-pc`).
- **Per-process: DRM fdinfo** (kernel 5.14+, standardized 5.19+): per-engine cumulative busy ns
  (gfx/compute/dma/dec/enc/jpeg/vpe) → delta = per-PID util%; per-process VRAM/GTT
  (prefer `drm-resident-*`, `drm-memory-*` deprecated since 6.13); multi-GPU via `drm-pdev`;
  dedupe via `drm-client-id`. **Gap: ROCm/HIP compute via KFD shows ~0% in fdinfo engine
  stats** — cover via `/sys/class/kfd/kfd/proc/<pid>/vram_*`.
- Reading other users' fdinfo needs root/CAP_SYS_PTRACE (same model as nvtop: per-user view
  unprivileged, system-wide view needs elevation).

**Windows**: ADLX (`amdadlx64.dll` ships with Adrenalin driver; COM-style; capability-check
every metric via `IADLXGPUMetricsSupport`; custom EULA — review before shipping; has built-in
metrics history buffer). **ADLX has NO per-process API** → PDH `GPU Engine`/`GPU Process
Memory` counters + `D3DKMTQueryStatistics` (vendor-agnostic WDDM; official `windows` crate).

**Rust**: `libdrm_amdgpu_sys` (active, has gpu_metrics decoders for all versions, dynamic
loading feature) or depend on/copy `libamdgpu_top` (MIT). Pure-sysfs path = zero deps.
`adlx-rs` is 0.0.0-alpha and stale — plan hand-rolled COM vtable FFI or bindgen on the C API.

**Quirks**: APUs — VRAM carve-out meaningless (GTT is what matters), no mem_busy_percent,
package-level power only (CPU+GPU split needs gpu_metrics v2/v3). RDNA3 may expose only
instantaneous `power1_input` (kernel 6.7+), not average — probe both. hwmon/card indices not
stable across boots — resolve via PCI bus id.

## Intel — two-driver world on Linux; IGCL + PDH on Windows

**The defining caveat: i915 vs xe are two different worlds.** i915 (Gen9→Meteor Lake,
DG2/Alchemist default) vs xe (Lunar Lake, Battlemage, Panther Lake+) differ in sysfs layouts,
fdinfo keys, PMU names, and hwmon ABIs — implement both, detect via `drm-driver`/uevent.
`intel_gpu_top` is i915-only and IGT maintainers **will not port it to xe** — the xe-era tools
are IGT's `gputop` and qmassa (whose author is an Intel engineer).

**Linux per-process (the strong path, no library needed)**:
- i915: `drm-engine-<render|copy|video|video-enhance|compute>` busy-ns (kernel 5.19+);
  per-process memory `drm-total-local0` (VRAM)/`drm-total-system` (6.8+).
- xe: memory regions system/gtt/vram0/stolen (6.8+); engine util via
  `drm-cycles-*`/`drm-total-cycles-*` (6.11+).
- Unprivileged = own processes only; root/CAP_SYS_PTRACE for all (same model as AMD).

**Linux device-level**:
- sysfs freq unprivileged both drivers (`gt_act_freq_mhz` / `tile0/gt0/freq0/act_freq`).
- hwmon power/energy/fan/temp is **effectively dGPU-only** and recent-kernel-gated (i915
  fan/temp 6.12+; xe pkg/VRAM temps 6.15, fans 6.16 — Ubuntu 24.04's 6.8 kernel has none).
- **iGPU power exists only via root-only RAPL** uncore (CVE-2020-8694) or perf with CAP_PERFMON.
- Device-wide engine util: i915/xe perf PMU = root or CAP_PERFMON (intel_gpu_top's infamous
  "Failed to initialize PMU"); xe PMU only exists since kernel 6.15. **Unprivileged
  workaround: aggregate fdinfo deltas across visible clients.**
- No device-wide VRAM-used sysfs on xe — aggregate from fdinfo or Level Zero `zesMemoryGetState`.

**Level Zero Sysman** (the only true cross-platform Intel API — with asterisks): use modern
`zesInit()` (legacy `ZES_ENABLE_SYSMAN` is deprecated/i915-only/spec-frozen); one init mode per
process. Real-world coverage is gappy: engine metrics on Linux sit on perf_event_open
(CAP_PERFMON, regression #707 briefly required CAP_SYS_ADMIN); iGPU power domains absent on
Linux by design (#751/#925); temps need root on Linux and **Administrator on Windows** (#932);
fan handles broken on Linux/Battlemage; `zesDeviceProcessesGetState` frequently
UNSUPPORTED_FEATURE — do not depend on it. ProjectPhysX's compatibility matrices
(compute-runtime #932, hw-smi README) are the best ground truth for what actually works.

**Windows**: prefer **IGCL** (ctlApi; DLL ships with the driver, iGPU+Arc, no admin) —
`ctlPowerTelemetryGet` gives one snapshot with util counters, temps, VRAM bandwidth, fans,
**throttle flags** (power/temp/current-limited). Known broken on Battlemage as of 2025-26:
vram/card energy counters = 0 (#138), memory bandwidth = 0 (#120), PCIe structs = 0 (#149) —
capability-check and ground-truth per SKU. Per-process on Windows: PDH/D3DKMT only (Intel
APIs have nothing adequate). 64-bit only; telemetry routes through Level Zero internally.

**Rust**: no official Intel bindings for anything. **qmassa/qmlib** (Apache-2.0, active, Intel
engineer) is the best Linux reference/dependency — pure fdinfo+sysfs+hwmon for i915/xe/amdgpu.
all-smi proves L0 Sysman FFI via libloading is practical (~25 functions needed). IGCL: zero
Rust bindings exist — bindgen on `igcl_api.h` + port the cApiWrapper DLL-loading logic.

**Counters are monotonic snapshots everywhere** (IGCL, Sysman activeTime/energy, fdinfo
busy-ns/cycles): utilization = delta between samples; handle wrap at bit width.

## Apple Silicon — private APIs, sudoless (macmon's proven recipe)

**IOReport** (private `/usr/lib/libIOReport.dylib`, direct `#[link]` works, NO sudo):
- "Energy Model" group: GPU/CPU/ANE/DRAM energy counters → power by delta/dt. **Unit labels
  vary per chip (mJ on M1, µJ/nJ on M3/M4) — always parse `IOReportChannelGetUnitLabel`.**
  ANE channel name varies (ANE0/ANE/DIE_N_ prefixes on Ultra).
- "GPU Stats"/"GPU Performance States": per-DVFS-state residency → utilization (1 − idle
  residency) + freq (residency-weighted; state table from IORegistry `pmgr` `voltage-states9`;
  **Hz on M1-M3, kHz on M4+**).
- ≥100ms between samples; breakage history is real (M5 added MCPU* channels → macmon panic
  #47; macOS 26 format change → dual parsers).

**IOKit AGXAccelerator `PerformanceStatistics`** (public symbols on undocumented data, no sudo):
"Device Utilization %", "Renderer/Tiler Utilization %", "In use system memory", GPU core count,
recoveryCount (GPU restarts!). Note: DVFS-derived util ≠ Device Utilization % — pick one,
label it.

**Metal (public, sandbox-safe)**: `recommendedMaxWorkingSetSize` (the de-facto GPU memory
budget; raisable via `iogpu.wired_limit_mb` sysctl), `currentAllocatedSize` (own process only).

**SMC/IOHID**: GPU/CPU temps (SMC `Tp*/Tg*` keys macOS 14+; IOHID sensors on M1/macOS 12-13),
fan RPM. Key names shift every SoC generation.

**Per-process reality**: essentially NONE sudoless. `rusage_info` has NO GPU field (verified
current xnu headers; ri_neural_footprint for ANE memory DOES work sudoless). powermetrics'
per-process "GPU ms/s" requires root and is reportedly broken on Apple Silicon. Activity
Monitor uses private sysmond accounting. **Only sudoless signal: enumerate AGXDeviceUserClient
IORegistry entries → `IOUserClientCreator` = "pid 1234, ProcessName"** — i.e., *which*
processes hold GPU contexts (presence, not %). mactop v2 experimentally scrapes the `AppUsage`
accumulated-GPU-time arrays from those entries and rescales — approximate, undocumented, but
the only game in town.

**Distribution**: private APIs ⇒ no Mac App Store. Developer ID + notarization (what
macmon/Stats/iStat all do).

**Rust**: port macmon's `sources.rs` FFI (~300-500 lines, MIT, M1–M5 support, the canonical
reference); `objc2-metal` for Metal; `core-foundation` + `libc` otherwise. No general IOReport
crate exists — every tool vendors its FFI.

## Per-process attribution feasibility matrix

| Platform | PIDs + names | Per-PID VRAM | Per-PID util | Privilege |
|---|---|---|---|---|
| Linux + NVIDIA | ✅ NVML | ✅ NVML | ✅ NVML (weak semantics) | none |
| Linux + AMD | ✅ fdinfo | ✅ fdinfo (KFD gap → kfd proc) | ✅ fdinfo per-engine | none (own user) / root (all users) |
| Linux + Intel | ✅ fdinfo | partial fdinfo | ✅ fdinfo per-engine | none (own) / root (all); device-level PMU needs CAP_PERFMON |
| Windows (all vendors) | ✅ PDH | ✅ PDH GPU Process Memory | ✅ PDH per-engine | none |
| Windows + NVIDIA extra | ✅ NVML | ❌ WDDM (use PDH) | ✅ NVML pmon-style | none |
| macOS Apple Silicon | ✅ AGXDeviceUserClient (presence) | ❌ none exists | ⚠️ experimental AppUsage scraping | none |
| WSL2 | ⚠️ often empty | ❌ N/A at NVML level | ❌ | — |

**Conclusion**: full cross-vendor per-process is achievable on Linux (fdinfo + NVML) and
Windows (PDH — *the* underexploited API; nobody in the terminal space uses it). macOS gets
device-level depth + process *presence* + experimental GPU-time scraping. WSL2: degrade
gracefully, never crash (nvtop #432's lesson).
