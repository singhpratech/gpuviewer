# Cross-platform design: Windows (v1.5) and macOS Apple Silicon (v2)

Status: design record, synthesized 2026-06-07 from six research investigations
(NVML-under-WDDM, PDH, DXGI/D3DKMT, Apple/IOReport, packaging, foundation audit).
Companion to `docs/research/04-synthesis.md` (the v1 decision record) — this document
extends it; it does not re-litigate anything settled there.

Honesty contract (binding for every section below): a metric is either
**Some(value from a named real source)** or **None** — and where the *reason* for None is
knowable (OS prohibition, driver model, missing hardware), the UI says so. No fabricated
numbers. No silent zeroes. PDH utilization is scheduler duty-cycle and is labeled exactly
as honestly as NVML duty-cycle is today.

---

## 1. Support matrix

Platform × vendor × metric-group. "Validation" column: everything new in this design is
**compiles+CI, hardware-unvalidated** until it passes the manual real-hardware pre-release
checklist (same policy as Linux v1 had before release).

Legend: ✔ = Some(real source) · ✖ = None (with in-UI explanation where the reason is
knowable) · (label) = Some but with a mandatory honesty label in the UI/spec.

| Metric group | Linux NVIDIA (nvml) | Linux AMD (sysfs) | Linux Intel (fdinfo/sysfs) | **Windows NVIDIA (nvml + pdh)** | **Windows AMD/Intel (wddm)** | **macOS Apple Silicon (apple)** | Mock (all OS) |
|---|---|---|---|---|---|---|---|
| Device utilization | ✔ NVML duty-cycle (labeled) | ✔ gpu_busy_percent | ✔ fdinfo engine deltas | ✔ NVML duty-cycle (labeled) | ✔ PDH GPU Engine, busiest-engine, scheduler duty-cycle (labeled; WDDM 2.0+ gate, else ✖ "no WDDM 2.0 GPU") | ✔ Tier B `Device Utilization %` (duty-cycle-like, labeled; private interface), Tier C GPUPH residency fallback | ✔ scripted |
| VRAM total | ✔ memory_info.total | ✔ mem_info_vram_total | ✔ (when exposed) | ✔ NVML memory_info_v2.total (R510+ floor; pre-R510 → ✖) | ✔ DXGI `DedicatedVideoMemory` (iGPU/UMA: labeled carve-out, SharedSystemMemory shown separately) | ✔ Metal `recommendedMaxWorkingSetSize` (labeled "unified-memory working-set budget", NOT discrete VRAM) | ✔ |
| VRAM used (device) | ✔ memory_info.used | ✔ mem_info_vram_used | ✔/✖ per kernel | ✔ PDH `GPU Adapter Memory\Dedicated Usage` primary (VidMm truth); NVML fallback labeled "driver view, virtualized" | ✔ PDH `GPU Adapter Memory\Dedicated Usage` per LUID (NEVER `QueryVideoMemoryInfo` — that is per-process by design) | ✔ Tier B `In use system memory` (labeled "GPU-mapped system RAM, unified memory — not VRAM") | ✔ |
| Power draw | ✔ power_usage | ✔ hwmon/gpu_metrics | ✖ (iGPU: no hwmon) | ✔ NVML power_usage | ✖ "no public API for AMD/Intel power on Windows" | ✔ Tier C IOReport `GPU Energy` delta/Δt (labeled "SoC rail approximation; private interface") | ✔ |
| Temperature | ✔ temperature | ✔ hwmon/gpu_metrics | ✖ (iGPU: no hwmon) | ✔ NVML temperature | ✖ "no public API" (future: runtime-probed D3DKMT ADAPTERPERFDATA, deci-°C — see §3.6) | ✖ initially ("Apple exposes no public GPU temp; SMC keys are private and chip-churn-prone") | ✔ |
| Fan | ✔ fan_speed (boards with fans) | ✔ hwmon | ✖ | ✔ NVML fan_speed (else ✖ per board) | ✖ "no public API" | ✖ (often fanless; SMC private) | ✔ |
| Clocks (sm/mem) | ✔ clock_info | ✔ pp_dpm_*/gpu_metrics | ✔/✖ per kernel | ✔ NVML clock_info | ✖ "no public API" | sm: ✔ Tier C GPUPH residency-weighted DVFS freq (private interface); mem: ✖ | ✔ |
| Encoder/decoder util | ✔ encoder/decoder_utilization | ✖/✔ per kernel | ✔ video engines | ✔ NVML | ✔ PDH engtype `VideoEncode`/`VideoDecode` sums (open engtype set; absent → ✖) | ✖ | ✔ |
| Throttle reasons | ✔ clocks-event bitmask | ✔ gpu_metrics throttle status | ✖ | ✔ NVML (empirically readable under WDDM) | ✖ → `throttle: null` (see §5.4 model change) | ✖ → `throttle: null` | ✔ |
| Process list (pid/name/kind) | ✔ NVML lists (WSL2: hint) | ✔ fdinfo | ✔ fdinfo | ✔ NVML compute/graphics lists (_v3, _v2 fallback) ∪ PDH pids (kind=Unknown for PDH-only) | ✔ PDH `GPU Engine`/`GPU Process Memory` instances per LUID | ✖ **OS-prohibited** — `process_hint`: "macOS prohibits per-process GPU attribution for third-party tools (powermetrics requires root); device-level only" | ✔ |
| Per-process GPU memory | ✔ usedGpuMemory | ✔ fdinfo (5.19+) | ✔ (6.8+) | ✔ PDH `GPU Process Memory\Dedicated Usage` (+Shared shown separately). NVML usedGpuMemory is **always** Unavailable under WDDM → never the source, never 0 | ✔ same PDH source | ✖ (OS-prohibited) | ✔ |
| Per-process GPU util | ✔ process_utilization_stats (weak under concurrency — column only) | ✔ fdinfo deltas | ✔ fdinfo deltas | ✔ PDH per-pid max-across-engines (Task-Manager-comparable, engine named; sums labeled "can exceed 100%") | ✔ same | ✖ (OS-prohibited) | ✔ |
| Per-process CPU% / container | ✔ /proc | ✔ /proc | ✔ /proc | ✖ both (no /proc; honest None) | ✖ both | ✖ both (no process rows at all) | ✔ |
| **Validation status** | shipped v1 (real-HW per release checklist) | shipped v1 | shipped v1 | **compiles+CI, hardware-unvalidated** | **compiles+CI, hardware-unvalidated** | **compiles+CI, hardware-unvalidated; design freeze gated on WWDC26 re-check (§4.6)** | CI-exercised everywhere |

WSL2 stays a Linux-side concern (existing detection + hint). MIG never arises on Windows
(NVIDIA: Linux-only).

---

## 2. Windows NVIDIA backend (v1.5) — `crates/core/src/nvidia.rs` + shared `win::pdh`

The dual-source split, confirmed by research: **NVML = device truth** (utilization, temp,
power, clocks, throttle reasons, fan, total VRAM, PID enumeration); **PDH = per-process
attribution** (the WDDM kernel data NVML architecturally cannot see). This is the "honest
per-process number where Task Manager misleads" wedge.

### 2.1 nvml.dll loading

- Modern drivers (≥461.55, ~2020+) install `nvml.dll` into `C:\Windows\System32`, which is
  on the default `LoadLibraryExW` search path — plain `Nvml::init()` works (nvml-wrapper
  0.12 defaults to `"nvml.dll"` on non-Linux).
- Change in `NvidiaBackend::init()` (nvidia.rs:78-82): cfg-gate the
  `lib_path("libnvidia-ml.so.1")` builder attempt to `target_os = "linux"`; on Windows go
  straight to `Nvml::init()`. (Skips one doomed LoadLibrary; behavior otherwise identical —
  the existing or_else chain already functions on Windows.)
- Failure modes mapped to `BackendError::Unavailable` (normal, backend skipped):
  `LibloadingError` (no NVIDIA driver / pre-2020 NVSMI-only driver) and
  `NVML_ERROR_DRIVER_NOT_LOADED`. We do NOT probe the legacy
  `C:\Program Files\NVIDIA Corporation\NVSMI\` path — instead we declare a driver floor.

### 2.2 Driver floor: R510+ (early 2022) — documented, not enforced

nvml-wrapper 0.12 binds `nvmlDeviceGetMemoryInfo_v2` and the `_v3` process-list symbols,
both added ~R510. Policy:

- `memory_info()` failing with `FailedToLoadSymbol` (pre-R510) → `mem_total_bytes`/`mem_used`
  `None`; the README/Windows notes state the R510+ floor. No v1 fallback exists in 0.12 —
  a clear "unavailable" rendering is the pragmatic choice.
- `running_compute_processes()`/`running_graphics_processes()` on `FailedToLoadSymbol` →
  retry the `_v2` method variants once, then give up to an empty list (pre-2022 drivers).
- All other per-metric errors keep the existing `opt()` → None mapping.

### 2.3 Per-metric sources on Windows (DynamicSample)

| Field | Source | Notes |
|---|---|---|
| util_pct | NVML utilization_rates | duty-cycle label unchanged |
| mem_used_bytes | **PDH `GPU Adapter Memory\Dedicated Usage`** for this device's LUID, primary; fallback NVML memory_info.used | WDDM virtualizes VRAM (oversubscription pages to system RAM) — the NVML number is the driver's view and can diverge; PDH dedicated usage is the VidMm number the OOM story needs. Fallback is labeled "driver view". |
| power_mw / temp_c / fan_pct / sm_clock_mhz / mem_clock_mhz / encoder_pct / decoder_pct | NVML, as on Linux | per-board NOT_SUPPORTED → None as today |
| throttle | NVML current_throttle_reasons → `Some(map_throttle(...))` | empirically readable under WDDM; on error → `None` (§5.4) |

VRAM→OOM narration on Windows: the VramPressure trend keeps `Confidence::Likely` and the
evidence string names the source ("PDH dedicated usage" vs "NVML driver view") — WDDM can
page to system RAM instead of OOMing, so a fact-grade OOM ETA would be confidently wrong.

### 2.4 Per-process (ProcessSample) on Windows

- Spine: NVML compute+graphics PID lists (gives `ProcessKind`). `UsedGpuMemory::Unavailable`
  (always, under WDDM — Windows KMD owns memory) → `mem_bytes` stays None from NVML.
  **Never 0.** The existing code already does this; it becomes load-bearing on Windows.
- Fill from the shared PDH snapshot (§3.2), joined by pid:
  - `mem_bytes` = `GPU Process Memory\Dedicated Usage` for (pid, this device's LUID).
    Shared Usage is carried separately to the UI (dedicated vs shared is the honest split);
    it is NOT added into `mem_bytes`.
  - `util_pct` = max across that pid's engine instances on this LUID
    (Task-Manager-comparable; the busiest engine's name goes in the UI tooltip/evidence).
    Documented as scheduler duty-cycle. Any summed per-engtype figure shown in the UI is
    labeled "engine-sum, can exceed 100%".
- Pids present only in PDH (e.g. dwm.exe if NVML's graphics list misses it) are appended
  with `kind: ProcessKind::Unknown`.
- `process_utilization_stats` (single-process-only per NVIDIA's own forum guidance) is NOT
  used on Windows — PDH is strictly better here.
- `cpu_pct`, `container`: None (no /proc). Already cfg-gated.
- pid-reuse guard: PDH instances churn with process lifecycle; lifecycle events correlate
  pid + first-seen tick (existing event-engine behavior) — a recycled pid mis-attributes at
  most one frame and never narrates an exit/attach pair from it.

### 2.5 LUID ↔ NVML device matching

`win::adapters` (§3.3) builds the per-session map `LUID → normalized PCI BDF` via
`D3DKMTOpenAdapterFromLuid` → `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERADDRESS)` →
`"0000:BB:DD.F"` (struct has no PCI domain field; client Windows is effectively always
domain 0). The NVIDIA backend matches that against `Device::pci_info().bus_id` through the
same normalization rule the collector already uses (`normalize_pci_id`, collector.rs:43 —
NVML prints an 8-hex-digit domain, D3DKMT yields none; both normalize to a 4-digit domain).
The normalization helper moves to `gpuviewer-core` (§9) so backends and collector share one
implementation. **A failed match is an honest terminal state**: the device keeps NVML
metrics, per-process columns go None, and one collector self-honesty event explains
"could not attribute per-process GPU data (LUID↔PCI match failed)". Never force a match.

### 2.6 Driver model / MCDM trap

Any call to `Device::driver_model()` is wrapped: `Ok` → WDDM/TCC handled normally;
`Err(NvmlError::UnexpectedVariant(_))` (future MCDM=2 — missing from 0.12's enum) →
"unknown compute driver model", treated like TCC-class (non-display) for messaging.
Never crash. (TCC itself: per-process memory would work, but it is Quadro/Tesla-only,
deprecated, and irrelevant to GeForce users — no special path.)

### 2.7 `process_hint` on Windows

Set at init: "per-process VRAM comes from Windows (WDDM) accounting, not the NVIDIA
driver — NVML cannot see it under WDDM". If PDH is unavailable (§3.2), the hint becomes
"per-process GPU stats unavailable: no WDDM 2.0 GPU/driver".

---

## 3. Windows cross-vendor WDDM backend (v1.5+) — new `crates/core/src/wddm.rs`

Covers AMD and Intel on Windows (and NVIDIA when NVML is absent), entirely from OS
surfaces: DXGI + PDH + D3DKMT. `pdh.dll`/`gdi32.dll`/`dxgi.dll` are OS system libraries —
linking them via the `windows` crate does **not** violate the no-vendor-SDK rule.

### 3.1 Enumeration & identity

- `CreateDXGIFactory1` → `EnumAdapters1` loop until `DXGI_ERROR_NOT_FOUND` → `GetDesc1`.
- Skip `DXGI_ADAPTER_FLAG_SOFTWARE` adapters (WARP / Microsoft Basic Render, VendorId
  0x1414).
- In-session join key: `AdapterLuid` (matches PDH instance luid tokens, D3DKMT, and —
  optionally — `IDXGIFactory6::EnumAdapterByLuid` cross-checks). **LUID is session-scoped**
  (changes on reboot/driver update) — never persisted as identity.
- Persistent `DeviceId`: normalized PCI BDF from D3DKMT ADAPTERADDRESS (`"0000:bb:dd.f"`,
  lowercased) — the same key shape NVML/sysfs produce, so history identity and
  cross-backend dedupe both work. If the D3DKMT query fails, fall back to
  `DeviceId(format!("wddm:{:04x}:{:04x}:{}", vendor_id, device_id, ordinal))` — which
  `normalize_pci_id` correctly refuses to dedupe (listing a device twice beats wrongly
  merging two).

### 3.2 PDH source (shared module `win::pdh`, used by both Windows backends)

One **process-wide persistent query** (OnceLock + Mutex), opened once:

- Counters added with `PdhAddEnglishCounterW` (localization-safe), wildcard paths:
  - `\GPU Engine(*)\Utilization Percentage`
  - `\GPU Process Memory(*)\Dedicated Usage`, `\Shared Usage`
  - `\GPU Adapter Memory(*)\Dedicated Usage`, `\Shared Usage`
- Documented non-English-locale fallback chain implemented:
  `PdhGetCounterInfoW` → `PdhExpandWildCardPathW` → `PdhAddCounterW` per expanded path
  (re-expanded when instance churn is detected), because wildcard-add is only proven on
  English Windows.
- One `PdhCollectQueryData` per Engine tick, snapshot-cached: `pdh::shared().snapshot(now)`
  re-collects only if the cache is older than ~250 ms, so the nvidia and wddm backends in
  the same tick share one collection (rate counters need two collections — the first frame
  legitimately yields None everywhere).
- Read with `PdhGetFormattedCounterArrayW(PDH_FMT_DOUBLE | PDH_FMT_NOCAP100)` using the
  two-call buffer pattern (`PDH_MORE_DATA` → allocate → call again). **NOCAP100 is
  mandatory** — values are silently capped at 100 otherwise, which would make summed
  multi-engine numbers quietly wrong (trust-thesis violation, not a crash).
- Per-item `CStatus` checked before trusting any value.
- **Absence-is-normal table** (each maps to None + at most one collector self-honesty
  event, never an error): `PDH_CSTATUS_NO_OBJECT` (0xC0000BB8 — no WDDM 2.0 GPU; exactly
  what GPU-less CI runners hit), `NO_COUNTER` (0xC0000BB9), `NO_INSTANCE` (0x800007D1),
  `PDH_NO_DATA` (0x800007D5), `PDH_CSTATUS_INVALID_DATA` (0xC0000BBA),
  `PDH_INVALID_DATA` (0xC0000BC6, first-sample case), and
  `PDH_QUERY_PERF_DATA_TIMEOUT` (0xC0000BFE — transient miss, NOT device_lost).
- Instance-name parser is a **pure function** (fixture-tested on any OS): split on `_`,
  keyword tokens `pid` (1), `luid` (2 — HighPart then LowPart, both hex DWORDs; order is
  inferred from observation, so matching always verifies BOTH parts against enumerated
  AdapterLuids and treats no-match as "unattributed"), `phys` (1), `eng` (1),
  `engtype` (rest — **opaque string, open set**, never an exhaustive enum: drivers/HAGS
  rename these), optional `part` (1). Grammar mirrors windows_exporter's production parser.

### 3.3 LUID→PCI module (`win::adapters`)

Thin, isolated wrapper over the least-contractual API in the chain (WDK-documented
gdi32 thunks, Windows 8+): `D3DKMTOpenAdapterFromLuid` →
`D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERADDRESS)` → `D3DKMT_ADAPTERADDRESS
{BusNumber, DeviceNumber, FunctionNumber}` → `D3DKMTCloseAdapter`. DXGI
VendorId/DeviceId is the fallback identity when the thunk fails — breakage degrades to
"device-level only with synthetic id", never a crash. `D3DKMTQueryStatistics` is
explicitly "Reserved for system use" — **never used**.

### 3.4 Per-metric sources (DynamicSample, wddm backend)

| Field | Source |
|---|---|
| util_pct | PDH `GPU Engine`: filter instances to this LUID, sum per (engtype, eng index) across pids → per-engine busy %; **device headline = busiest single engine** (Task-Manager-Performance-tab-comparable), engine name surfaced in UI. Labeled scheduler (VidSch) duty-cycle, not capacity. |
| mem_used_bytes | PDH `GPU Adapter Memory\Dedicated Usage` for this LUID (adapter-level counter is the one Microsoft confirms stays correct — KB4490156). `QueryVideoMemoryInfo` is **never** used for device-used: it reports the calling process's own budget/usage by design (would show gpuviewer's own ~0). |
| encoder_pct / decoder_pct | sums of `VideoEncode` / `VideoDecode` engtypes (max across engines of that type); absent engtype → None |
| power_mw, temp_c, fan_pct, sm_clock_mhz, mem_clock_mhz | **None.** In-UI: "Windows exposes no public temperature/power/clock API for this GPU; install-free monitoring shows utilization and memory only." |
| throttle | `None` (§5.4) |

StaticInfo: `mem_total_bytes` = `DXGI_ADAPTER_DESC1.DedicatedVideoMemory` (iGPU/UMA:
~0 dedicated is normal — label the shared budget via `SharedSystemMemory` separately,
never sum them); `name` = adapter Description; `driver_version` = None initially
(DXCore DriverVersion is a follow-up); `source_caveat` (§5.4) = the duty-cycle wording.

### 3.5 ProcessSample (wddm backend)

Same PDH joins as §2.4: `mem_bytes` = per-pid Dedicated Usage on this LUID; `util_pct` =
per-pid max-across-engines; `name` resolved via OS process query
(`K32GetModuleBaseNameW`/`QueryFullProcessImageNameW`, basename only — same trim rule as
nvidia.rs); `kind` = Unknown (PDH does not distinguish compute/graphics; engtype `Compute`/
`Cuda` presence may upgrade to Compute, treated as a heuristic only); `cpu_pct`/`container`
= None.

### 3.6 Explicitly out of scope (recorded so nobody "fixes" it in)

- D3DKMT `KMTQAITYPE_ADAPTERPERFDATA` (Task Manager's GPU-temp source; deci-°C,
  FanRPM, Power in **0.1% units — never render as watts**): driver-optional kernel thunk.
  Future opportunistic runtime probe, OFF for v1.5; zeros/failures → None.
- DXCore adapter-wide QueryState telemetry (states 2–10 incl. AdapterTemperatureCelsius):
  prerelease-banner docs, absent from windows 0.62.2 metadata. Revisit when GA — it would
  be the first fully public cross-vendor temp/clock path on Windows.
- WMI (`MSAcpi_ThermalZoneTemperature` is motherboard zones; `Win32_VideoController.AdapterRAM`
  is u32-capped): never.

### 3.7 Dedupe when NVML also covers a device

Registry order in `all_backends()` becomes: **nvidia → amd → intel → wddm** (wddm last;
Linux backends cfg'd out on Windows, so effectively nvidia → wddm there). The existing
collector dedupe (first backend wins on `normalize_pci_id`) then does the right thing with
zero new mechanism: an NVIDIA GPU claimed by NVML is skipped by wddm (NVML is the richer
source and gets its per-process data from the same shared PDH module anyway); AMD/Intel
adapters — and NVIDIA adapters on driverless/broken-NVML machines — fall through to wddm.
Synthetic `wddm:` ids never dedupe (by design of `normalize_pci_id`): a double listing is
visible and honest, a wrong merge is not.

---

## 4. macOS Apple Silicon device-level backend (v2) — new `crates/core/src/apple.rs`

**Verdict: conditional GO** — see §4.6 for the gate. Per-process GPU attribution is
**OS-prohibited for third parties** (re-verified June 2026: `powermetrics
--show-process-gpu` requires root; Activity Monitor's "% GPU" uses private plumbing with
no public equivalent). The backend is device-level only, and the UI says exactly why.

### 4.1 Three-tier source stack

- **Tier A — public, always on**: Metal static info. `MTLCreateSystemDefaultDevice` →
  `name`, `hasUnifiedMemory` (10.15+), `recommendedMaxWorkingSetSize` (10.12+ — used as
  `mem_total_bytes`, **labeled** "unified-memory working-set budget", because Apple
  publishes no 'total VRAM' and unified memory has none). The only fully supported tier;
  the floor that always renders.
- **Tier B — IOKit, undocumented-but-stable, no root**: `IOServiceMatching("IOAccelerator")`
  matches the AGXAccelerator service; its `PerformanceStatistics` CFDictionary supplies
  `Device Utilization %` (→ util_pct, duty-cycle-like — same honesty label as NVML),
  `Renderer Utilization %`/`Tiler Utilization %` (UI detail rows), `In use system memory`
  (→ mem_used_bytes, labeled GPU-mapped system RAM). Every key treated as Option —
  inventories differ across chips/OS releases and Intel-era keys (`Temperature(C)` etc.)
  must not be assumed.
- **Tier C — IOReport, private dylib**: vendored ~15-function FFI modeled on macmon's
  `sources.rs` (MIT, attribution comment in-file), loaded via **dlopen2 with `Option<fn>`
  fields** from `/usr/lib/libIOReport.dylib` — honoring the no-hard-link policy; a missing
  symbol degrades that metric to None, never fails init. Channels discovered by
  **enumeration + name matching, never index assumptions**: group `Energy Model` channel
  `GPU Energy` (energy delta / Δt → power_mw; unit from `IOReportChannelGetUnitLabel` —
  mJ/µJ/nJ all observed across chips); group `GPU Stats` subgroup `GPU Performance States`
  channel `GPUPH` → state-residency math (active/total → residency; residency-weighted
  DVFS table from IOKit pmgr `voltage-states9` → sm_clock_mhz, max_sm_clock_mhz).
  Ultra-die (`DIE_N_` prefixes) and future-chip (`MCPU`) renames are handled by
  contains-matching and committed per-chip sample fixtures — the same per-version fixture
  policy as AMD `gpu_metrics`.

### 4.2 Per-metric summary (DynamicSample)

util_pct = Tier B (fallback Tier C residency); mem_used_bytes = Tier B (labeled);
power_mw = Tier C (labeled approximation); sm_clock_mhz = Tier C; temp_c, fan_pct,
mem_clock_mhz, encoder_pct, decoder_pct = **None**; throttle = **None** (§5.4).
`refresh_processes` returns an empty Vec — with `StaticInfo.process_hint` set to the
OS-prohibition explainer (load-bearing copy: macOS users must read "the OS forbids this",
not "this app is worse here"). `DeviceId` = `apple:` + slugged chip name from
`MTLDevice.name` (e.g. `apple:m2-max`) — stable across reboots (Metal `registryID` is
per-boot, unusable for history identity).

### 4.3 Private-interface caveat handling

Every Tier B/C metric is stamped: `StaticInfo.source_caveat` = "read via undocumented
macOS interfaces (IOKit PerformanceStatistics / IOReport); may break on macOS updates" —
surfaced in the TUI device header and in `report`. This is non-negotiable: the Electron
cornerMask/Tahoe incident shows a silent private-API break reads as the product lying.
Defensive ObjC: all Metal/IOKit calls go through objc2 with exception-catching enabled —
on GitHub's paravirt runner, missing selectors raise `NSInvalidArgumentException` rather
than returning errors (Godot #101773), and `MTLDevice` methods must not be assumed total.

### 4.4 What stays None forever (until Apple moves)

Per-process anything (memory, util, names) — OS-prohibited; temp/fan — private SMC/IOHID
keys with documented per-chip churn (M4-temps-wrong-class bugs upstream in macmon), parked
until there is a public API or a compelling fixture-backed case.

### 4.5 CI smoke realism

GitHub `macos-15` arm64 runners (Apple Virtualization.framework, 3 vCPU/7 GB) expose a
non-nil "Apple Paravirtual device" via Metal (family Apple5, no MPS), so CI can smoke:
build + all mock tests + Tier A static-info path (assert device present and name
non-empty; **never** assert real-hardware values) + graceful-None assertions for Tier B/C
(AGXAccelerator and SoC pmgr/Energy Model channels are almost certainly absent in the
guest — paravirt GPU class is AppleParavirtDevice). That absence claim is **inference, not
citation**: before baking assertions, run the one-off probe job (§5.5) that dumps IOReport
channel enumeration and `ioreg -c IOAccelerator`/`-c AGXAccelerator` from a runner to
establish ground truth. Real-hardware telemetry = manual pre-release checklist, same as
Linux. If the guest unexpectedly exposes channels, tighten the assertions to match reality
rather than asserting None.

### 4.6 WWDC26 post-keynote re-check (MANDATORY GATE)

Keynote is **June 8, 2026** (tomorrow at time of writing); sessions run June 8–12. Rumor
coverage shows zero GPU-observability signals, but two outcomes would invalidate Tier B/C:
a new public telemetry API (better path — adopt it), or a lockdown of
IOReport/IOAccelerator access in macOS 27 (worse — re-plan). **Action items, blocking
Tier B/C design freeze (not blocking foundation/CI work):**
1. Scan the WWDC26 session list (Metal, Instruments, Xcode performance/observability) by
   June 12.
2. Smoke-test IOReport + AGXAccelerator presence on macOS 27 beta 1 when it drops.
3. Record the outcome as an addendum to this document.
Since macOS is v2 (after Windows v1.5), this gate costs zero schedule: Tier A + the
None-everywhere skeleton + CI leg can land now; Tier B/C implementation starts after the
re-check.

---

## 5. Foundation (lands first; everything else builds on it)

### 5.1 Per-OS data directory

One public function in `gpuviewer-history` (extend `default_data_dir()` at
`crates/history/src/store.rs:1141-1153`, export it, **delete** the duplicate at
`crates/tui/src/main.rs:855-863` and call the export):

```rust
/// Default per-OS data dir. Resolved from ENVIRONMENT VARIABLES on every OS — deliberately
/// not the Windows Known Folder API: child processes must be able to redirect it via env,
/// which the hermetic test pattern (and `--db`-less CI runs) depends on. The trade-off
/// (ignoring registry-redirected/roaming AppData) is acceptable for a CLI tool and is the
/// reason the `dirs`/`directories` crates were rejected (SHGetKnownFolderPath cannot be
/// redirected per-process; their path shapes also differ from ours on Windows AND macOS).
pub fn default_data_dir() -> Result<PathBuf, StoreError> {
    #[cfg(target_os = "windows")]
    {
        // %LOCALAPPDATA% is set for every interactive logon (and on GitHub runners) but
        // can be absent under service accounts — hence the %USERPROFILE% fallback.
        if let Some(v) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(v).join("gpuviewer"));
        }
        if let Some(v) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(v).join("AppData").join("Local").join("gpuviewer"));
        }
        Err(StoreError::NoDataDir)
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(v) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(v).join("Library/Application Support/gpuviewer"));
        }
        Err(StoreError::NoDataDir)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // BYTE-IDENTICAL to the shipped v1 chain — existing users' history.db must not move.
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(xdg).join("gpuviewer"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(home).join(".local/share/gpuviewer"));
        }
        Err(StoreError::NoDataDir)
    }
}
```

User-facing strings get per-OS wording: `crates/tui/src/main.rs:91` (`--db` help),
`crates/tui/src/main.rs:862` and `crates/history/src/store.rs:230` (NoDataDir error:
Windows "set %LOCALAPPDATA% or %USERPROFILE%", macOS "set $HOME", Linux unchanged).

### 5.2 Unix-assumption fixes (exact file:line list, from the verified audit)

| Site | Problem | Fix |
|---|---|---|
| `crates/tui/src/collector.rs:985-988` | `--on-event` spawns `sh -c` | `#[cfg(windows)]` branch: `Command::new("cmd").args(["/C", &self.cmd])`. **Land before the Windows CI leg** — Git-for-Windows' `sh.exe` on runner PATH would let the broken dispatch pass green while failing on real user machines. |
| `crates/tui/src/collector.rs:1902` | unit test uses `printenv` | per-OS command: Windows `cmd /C "echo %GPV_EVENT_KIND%> file"` |
| `crates/tui/src/main.rs:96-100` | `--on-event` help says `sh -c` + `$VAR` example | per-OS help wording (`cmd /C`, `%VAR%`) |
| `crates/history/src/store.rs:310-319` + `:470-484` | **latent Windows-only bug**: quarantine path renames the db while the failed `Connection` (match-scrutinee temporary) is still open; SQLite's win32 VFS opens without FILE_SHARE_DELETE → sharing violation → corrupt-db recovery becomes a hard startup failure | bind and drop the failed store before calling `quarantine()` |
| `crates/tui/tests/launch_artifacts.rs:37-38, 240-241, 291-292, 324-325, 377-378, 395-396`; `crates/tui/tests/ndjson_contract.rs:41-42` | scratch redirection sets only XDG_DATA_HOME/HOME | one shared spawn-env helper setting **XDG_DATA_HOME + HOME + LOCALAPPDATA + USERPROFILE** all to the scratch dir (extra vars harmless per-OS; works because resolution stays env-based) |
| `crates/tui/tests/launch_artifacts.rs:60` | asserts XDG layout `dir/gpuviewer/history-demo.db` (breaks on macOS) | parse the db path from the summary line already asserted on stdout at :55-58 |
| `crates/tui/tests/launch_artifacts.rs:431-437` | kill-then-immediately-relock: Microsoft documents post-TerminateProcess LockFileEx release as taking OS-dependent time → real windows-latest flake | bounded retry (~5 s) around the post-kill `open_recording`, WHY-comment citing the LockFileEx caveat |
| `crates/history/src/lib.rs:1076-1115` | `set_var("XDG_DATA_HOME")` test | same multi-var treatment |

Verified non-issues (do not "fix"): zero `std::os::unix` anywhere in `crates/`;
`temp_dir()`/exit-code asserts/stderr strings/EPIPE handling are already portable (emit()
treats any stdout write error as consumer-hangup, covering ERROR_BROKEN_PIPE); the
instance lock (`File::try_lock`, stable 1.89: flock/LockFileEx) ports by design; the
threaded collector tests are iteration-bounded, so Windows' 15.6 ms sleep quantum slows
but does not flake them.

### 5.3 `.gitattributes` (commit at repo root, then `git add --renormalize .` — expected no-op)

```gitattributes
# One byte-identical checkout on every OS (overrides the Windows runner image's
# core.autocrlf=true). rustfmt's default newline_style=Auto then keeps LF everywhere.
* text=auto eol=lf

# Fixture trees replicate kernel sysfs/ioctl output byte-for-byte — never normalize.
crates/core/tests/fixtures/** -text

# Binary artifacts.
*.gpvr binary
*.db binary
*.png binary
*.ico binary
*.icns binary
```

### 5.4 Model honesty changes (integrator-owned; NDJSON trio updated together)

1. `DynamicSample.throttle: ThrottleReasons` → `Option<ThrottleReasons>`
   (`#[serde(default)]`). WHY: wddm/apple cannot observe throttle; the current all-false
   default would assert "not throttling" as fact — a fabricated negative. NDJSON v1 spec
   already states "every metric is nullable" as philosophy; the spec table, schema
   (`throttle` nullable), and conformance suite are updated in the same commit. Ripple:
   `events.rs` (None → no throttle narration, episode reset on blind spot — same rule as
   util), `ui.rs` (render "n/a — not exposed by this source"), `history` rollup counters
   (None increments nothing — counts remain counts of *observed-active* frames),
   `mock.rs` (stays `Some`).
2. `StaticInfo.source_caveat: Option<String>` (`#[serde(default)]`, TUI/report-side only —
   not in the NDJSON device object for now): carries the macOS private-interface caveat and
   the Windows duty-cycle/VidMm wording. Backends without a caveat set None.
3. `normalize_pci_id` moves from `crates/tui/src/collector.rs:43` to
   `gpuviewer-core` (e.g. `model.rs`), re-exported; collector and Windows backends share it.

### 5.5 CI matrix (`.github/workflows/ci.yml`)

```yaml
name: CI
on:
  push: { branches: [main] }
  pull_request:
permissions: { contents: read }
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        include:
          - { os: ubuntu-latest,  lint: true  }   # fmt + clippy + test
          - { os: windows-latest, lint: false }   # clippy + test (sees cfg(windows) code)
          - { os: macos-15,       lint: false }   # clippy + test (sees cfg(macos) code);
            # pinned: macos-latest re-points to macos-26 starting 2026-06-15 — do not let
            # the image change under a brand-new leg mid-rollout.
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10 # v6.0.3
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8 # stable
        with: { toolchain: stable, components: "rustfmt, clippy" }
      - uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4 # v2.9.1
        # keys per-OS automatically (rustc host triple + job id); 3 caches share the
        # repo's 10 GB quota — monitor hit rates after the matrix lands.
      - if: matrix.lint
        run: cargo fmt --check
      # clippy runs on EVERY leg: -D warnings on ubuntu alone would never compile the
      # cfg(windows)/cfg(macos) modules at all.
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
  # One-off ground-truth probe for §4.5 — manual trigger only.
  macos-probe:
    if: github.event_name == 'workflow_dispatch'
    runs-on: macos-15
    steps:
      - uses: actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10 # v6.0.3
      - run: ioreg -r -c IOAccelerator -d 2 || true
      - run: ioreg -r -c AGXAccelerator -d 2 || true
      - run: cargo run -p gpuviewer-core --example macos_probe || true
```
(`workflow_dispatch` added to `on:` for the probe job.) Budget: Windows leg 1.5–3×
ubuntu wall time (Defender + process-spawn-heavy integration suites). If it becomes the
long pole, drop clippy from the Windows leg before dropping tests. Any Windows-only
failure after §5.2 lands is a real porting bug, not runner noise — treat it as such.

### 5.6 Cross-target local check limits (Linux dev machine)

`cargo check -p gpuviewer-core --target x86_64-pc-windows-gnu` and
`--target aarch64-apple-darwin` both pass locally (verified on rustc 1.96) — use them for
fast feedback on the **core** crate only. `gpuviewer-history`/`-tui` cannot cross-check
here (libsqlite3-sys's bundled C build needs a target C toolchain); their Windows/macOS
verification happens exclusively on the Actions matrix. Note: local cross-checks use the
`-gnu` Windows target; CI/release use `-msvc` — symbol-level differences are possible, the
matrix is authoritative.

---

## 6. Icon brief — flight-recorder motif for a terminal app

**Concept**: real flight recorders are *orange* (the "black box" lie is the hook). The mark
is a dark rounded GPU-chip plate carrying a telemetry strip with a stepped utilization
waveform, crossed by an orange playhead — "scroll back to 02:14". Geometric, flat, no
gradients, no text. Must read at 16 px.

**Colors (exactly 3 + transparency)**:
- Plate: `#10151D` (near-black slate; also the canonical "on dark" background)
- Waveform + tick marks: `#7FD4FF` (light cyan — matches TUI chart accent)
- Playhead + plate outline: `#FF8A3D` (recorder orange)

**Master SVG spec** (`assets/icon/gpuviewer.svg`, `viewBox="0 0 512 512"`, design grid =
32 px units so every stroke lands on pixel boundaries at 16 px = 1 unit):

1. **Chip plate**: rounded rect x=48 y=48 w=416 h=416 rx=64, fill `#10151D`,
   stroke `#FF8A3D` width 24. (At 16 px this rasterizes to a 13×13 rounded square with a
   ~1 px orange rim.) No chip pins — they turn to noise below 32 px.
2. **Timeline strip**: the horizontal middle band of the plate, y=224…288 conceptually;
   indicated only by the waveform itself plus two baseline tick marks: rects
   (x=96 y=348 w=32 h=16) and (x=384 y=348 w=32 h=16), fill `#7FD4FF`, opacity 0.55.
3. **Utilization waveform**: stepped polyline (square steps, like the TUI sparkline),
   stroke `#7FD4FF` width 28, fill none, stroke-linecap square, points:
   `96,320 160,320 160,232 224,232 224,288 288,288 288,176 352,176 352,256 416,256`.
   (Reads as a 2 px stepped line at 16 px.)
4. **Playhead**: vertical line x=304 from y=104 to y=408, stroke `#FF8A3D` width 24,
   plus a downward-pointing triangle handle at the top: path
   `M 272 96 L 336 96 L 304 144 Z`, fill `#FF8A3D`. Positioned right-of-center,
   deliberately crossing the waveform's tallest step ("the moment it stalled").
5. **16 px discipline**: at 16 px only three things must survive — orange-rimmed dark
   rounded square, one cyan stepped line, one orange vertical line with a head. Verify by
   rasterizing and hand-checking; nudge coordinates to the 32 px grid, never add detail.

**Deliverables** (icon+packaging workstream):
- `assets/icon/gpuviewer.svg` — master (above).
- PNGs: 16/32/48/64/128/256/512 → `packaging/icons/<N>x<N>/gpuviewer.png`
  (rasterize with resvg/rsvg-convert; inspect 16 and 32 by eye).
- `assets/icon/gpuviewer.ico` — frames 16/24/32/48/256, the 256 frame PNG-compressed
  (Vista+; Microsoft guidance) — consumed by winresource in `crates/tui/build.rs`.
- `packaging/gpuviewer.desktop`:
  ```ini
  [Desktop Entry]
  Type=Application
  Name=gpuviewer
  Comment=GPU flight recorder
  Exec=gpuviewer
  Icon=gpuviewer
  Terminal=true
  Categories=System;Monitor;
  ```
  (`Terminal=true`: launchers spawn it inside the user's terminal emulator; `Icon` is a
  theme name resolved against hicolor.) Installed by deb/rpm to
  `usr/share/applications/`; icons to `usr/share/icons/hicolor/<N>x<N>/apps/gpuviewer.png`
  + `usr/share/icons/hicolor/scalable/apps/gpuviewer.svg`.
- **ICNS: explicitly skipped.** A bare Mach-O in a tar.gz has no icon surface — macOS
  icons live in an .app bundle's Info.plist/Resources, and Terminal apps surface no icon
  anyway. Revisit when the v2 iced GUI ships an .app (budget Developer ID signing +
  notarization, ~$99/yr, at the same time).

---

## 7. Release pipeline — hand-rolled `.github/workflows/release.yml`

**Decision: do not adopt cargo-dist.** It is alive again (0.32.0, May 2026) but its 2025
abandonment-and-rescue history, its ownership of the generated workflow file, and its
tag-referenced (non-SHA-pinned) generated steps all fight this repo's SHA-pinned minimal-CI
culture — and it doesn't subsume the cargo-deb/cargo-generate-rpm/winresource wiring we
need anyway. Revisit only past ~5 target triples or if installer scripts become a priority.

**Trigger**: push of tags `v*`. **Permissions**: `contents: write`, `id-token: write`,
`attestations: write`.

**Target matrix**:

| Job | Runner | Target | Why |
|---|---|---|---|
| linux | **ubuntu-22.04** | `x86_64-unknown-linux-gnu` | **gnu, never musl**: statically-linked musl cannot dlopen at all (musl refuses to implement it — no dynamic loader, incompatible TLS), and even dynamic musl cannot load the glibc-linked `libnvidia-ml.so.1` — the NVML path would be dead. ubuntu-22.04 sets a glibc 2.35 floor (ubuntu-latest=24.04 would jump it to 2.39; note RHEL 9 = 2.34 is already below the floor — adopt cargo-zigbuild later if those users matter; 22.04 runners retire ~early 2027). |
| windows (v1.5) | windows-latest | `x86_64-pc-windows-msvc` | NVML on Windows is 64-bit only. **zip only, no MSI**: WiX v6 gates binaries behind the Open Source Maintenance Fee EULA, and an unsigned MSI trips SmartScreen exactly like an unsigned exe — toolchain burden, zero trust benefit. |
| macos (v2) | macos-15 | `aarch64-apple-darwin` | plain tar.gz; no ICNS (§6); unsigned (see notes). |

**Archive layouts**:
- `gpuviewer-<ver>-x86_64-unknown-linux-gnu.tar.gz` → top-level dir of the same name:
  `gpuviewer`, `README.md`, `LICENSE-MIT`, `LICENSE-APACHE`.
- `gpuviewer_<ver>-1_amd64.deb` (cargo-deb 3.7.0) and `gpuviewer-<ver>-1.x86_64.rpm`
  (cargo-generate-rpm 0.21.0): binary → `usr/bin/`, desktop file →
  `usr/share/applications/`, icons → hicolor paths (§6). Metadata in
  `crates/tui/Cargo.toml`: `[package.metadata.deb] name = "gpuviewer"` (so the package
  isn't `gpuviewer-tui`), assets array as in §6;
  `[package.metadata.generate-rpm]` assets `{source, dest, mode}` (dest dirs end with
  `/`, mode octal strings). Build with `cargo deb -p gpuviewer-tui` /
  `cargo generate-rpm -p crates/tui`. **Verify asset source paths from a workspace
  locally first** (manifest-relative vs workspace-root resolution is the known trap).
  Important property: both tools' auto-dependency scanners read DT_NEEDED only — the
  dlopen'd `libnvidia-ml.so.1` is invisible, so the packages correctly take **no** NVIDIA
  driver dependency. Do not add one manually; degrading to other backends/mock is the
  designed behavior.
- `gpuviewer-<ver>-x86_64-pc-windows-msvc.zip` → `gpuviewer.exe` (icon embedded via
  winresource), `README.md`, licenses.
- `gpuviewer-<ver>-aarch64-apple-darwin.tar.gz` → `gpuviewer`, `README.md`, licenses,
  `INSTALL-macos.txt` (quarantine note below).

**Checksums + attestation**: each build job writes `SHA256SUMS-<target>` (`sha256sum` /
`shasum -a 256` / `Get-FileHash`) and runs
`actions/attest-build-provenance@a2bbfa25375fe432b6a289bc6b6cd05ecd0c4c32 # v4.1.0` with
`subject-path` over its artifacts (free for public repos; users verify with
`gh attestation verify <file> -R <owner>/<repo>`; gate the step on repo visibility if it
ever goes private).

**Release creation — gh CLI, no third-party release action** (preinstalled on all hosted
runners, fits the SHA-pin policy):
1. job `draft`: `gh release create "$TAG" --verify-tag --draft --title "$TAG"`.
2. build jobs (`needs: draft`): build, package, checksum, attest, then
   `gh release upload "$TAG" <files>` (`GH_TOKEN: ${{ github.token }}`).
3. job `publish` (`needs: [linux, windows, macos]`):
   `gh release edit "$TAG" --draft=false`.

Action SHA pins (captured via `git ls-remote`, 2026-06-07): checkout v6.0.3 =
`df4cb1c069e1874edd31b4311f1884172cec0e10`; attest-build-provenance v4.1.0 =
`a2bbfa25375fe432b6a289bc6b6cd05ecd0c4c32`; upload-artifact v7.0.1 =
`043fb46d1a93c77aae656e7c1c64a875d1fc6a0a` (only if artifact handoff is ever needed);
rust-cache v2.9.1 = `c19371144df3bb44fab255c43d04cbc2ab54d1c4`; dtolnay/rust-toolchain =
`29eef336d9b2848a0b548edc03f92a220660cdb8` (pin + explicit `toolchain:` input).

**Unsigned-binary user notes** (release notes + README):
- macOS: browser downloads get `com.apple.quarantine` (Safari/Chrome set it; Archive
  Utility propagates it). Since Sequoia 15.1 the ctrl-click bypass is gone — users either
  approve in System Settings → Privacy & Security, or run
  `xattr -d com.apple.quarantine ./gpuviewer`, or download with curl/wget (which set no
  quarantine attribute — document `curl -LO <url>` as the friction-free path).
- Windows: SmartScreen flags the unsigned exe — "More info → Run anyway". Stated plainly;
  no MSI would have avoided it.

---

## 8. Cargo layout: dependencies, features, MSRV

**MSRV statement**: stays **1.95** (toolchain 1.96). All new deps clear it: windows
0.62.2 (rust_version 1.82), winresource 0.1.31, core-foundation 0.10, dlopen2, objc2
family. `File::try_lock` needs 1.89 ✓. No edition change.

**`crates/core/Cargo.toml`**:
```toml
[features]
default = ["nvidia", "wddm", "apple"]
nvidia  = ["dep:nvml-wrapper"]                       # linux + windows targets (existing)
wddm    = ["dep:windows"]                            # effective on windows targets only
apple   = ["dep:core-foundation", "dep:dlopen2",
           "dep:objc2", "dep:objc2-metal", "dep:objc2-foundation"]  # macos only

[target.'cfg(any(target_os = "linux", target_os = "windows"))'.dependencies]
nvml-wrapper = { version = "0.12", optional = true }  # existing

[target.'cfg(target_os = "windows")'.dependencies]
# OS system libraries (pdh/gdi32/dxgi), not vendor SDKs — direct linking is allowed.
# Pin the minor: the Wdk_* feature namespace has moved between windows-rs releases.
windows = { version = "0.62.2", optional = true, features = [
  "Win32_Foundation",
  "Win32_System_Performance",   # Pdh*
  "Win32_Graphics_Dxgi",        # CreateDXGIFactory1 / DXGI_ADAPTER_DESC1
  "Wdk_Graphics_Direct3D",      # D3DKMTOpenAdapterFromLuid / QueryAdapterInfo
] }
# Win32_Graphics_DXCore deliberately NOT enabled for v1.5 (prerelease telemetry states).

[target.'cfg(target_os = "macos")'.dependencies]
core-foundation = { version = "0.10", optional = true }
dlopen2        = { version = "0.8", optional = true }   # private libIOReport.dylib, Option<fn>
# Tier A Metal static info; enable objc2 exception catching (paravirt runners raise
# NSInvalidArgumentException on missing selectors). Re-verify exact latest minors when
# pinning — versions below are current as of research date.
objc2            = { version = "0.6", optional = true, features = ["exception"] }
objc2-metal      = { version = "0.3", optional = true }
objc2-foundation = { version = "0.3", optional = true }
```
(IOKit itself is a public OS framework: hand-rolled `#[link(name = "IOKit", kind =
"framework")]` externs in `apple/iokit.rs` are fine under the no-vendor-SDK rule — the rule
targets vendor SDKs with soname churn, not OS-shipped frameworks. Only the *private*
libIOReport goes through dlopen2.)

**`crates/tui/Cargo.toml`**:
```toml
[target.'cfg(target_os = "windows")'.build-dependencies]
winresource = "0.1.31"
```
`crates/tui/build.rs` gates on `std::env::var("CARGO_CFG_TARGET_OS") == Ok("windows")`
(build scripts run on the HOST — `#[cfg]` would be wrong) and embeds
`assets/icon/gpuviewer.ico`; MSVC's rc.exe is present on windows-latest.

Plus `[package.metadata.deb]` / `[package.metadata.generate-rpm]` blocks per §7.

**CI-only tools (not workspace deps)**: cargo-deb 3.7.0, cargo-generate-rpm 0.21.0 —
`cargo install --locked` in release.yml (host tools; their MSRVs are irrelevant to the
project's 1.95).

**`gpuviewer-history`**: no new dependencies (the data-dir port is std-only — that was the
point of rejecting `dirs`/`directories`).

---

## 9. Module/file plan and ownership map

Registry order after integration (`all_backends()`):
**nvidia → amd → intel → wddm → apple** (Linux trio cfg'd to linux; wddm cfg'd to
windows+feature; apple cfg'd to macos+feature; mock fallback unchanged; first-wins PCI
dedupe unchanged).

New `crates/core/src/lib.rs` entries (integrator):
```rust
#[cfg(all(feature = "wddm", target_os = "windows"))] pub mod wddm;
#[cfg(all(feature = "wddm", target_os = "windows"))] pub(crate) mod win; // pdh, adapters
#[cfg(all(feature = "apple", target_os = "macos"))]  pub mod apple;      // + submodules
```

**Ownership map** (each workstream owns its files EXCLUSIVELY; merge order: foundation →
integrator model change → backends/icon/release in parallel → integrator wiring):

| Workstream | Exclusively owned files |
|---|---|
| **foundation** | `crates/history/src/store.rs` (data-dir fn, quarantine fix, error strings), `crates/history/src/lib.rs` (env test), `crates/tui/src/main.rs` (delete dup data-dir fn, help/error strings), `crates/tui/src/collector.rs` (cmd /C dispatch + its test), `crates/tui/tests/launch_artifacts.rs`, `crates/tui/tests/ndjson_contract.rs` (env-helper edits only), `.gitattributes`, `.github/workflows/ci.yml` |
| **windows-nvidia** | `crates/core/src/nvidia.rs` (cfg'd init chain, R510 policy, PDH fusion via the §3.2 interface, driver-model wrap, hints) |
| **windows-wddm** | `crates/core/src/wddm.rs`, `crates/core/src/win/mod.rs`, `crates/core/src/win/pdh.rs`, `crates/core/src/win/adapters.rs`, `crates/core/tests/fixtures/pdh/` (instance-name fixtures + README) |
| **macos-apple** | `crates/core/src/apple.rs` (or `apple/mod.rs`), `crates/core/src/apple/metal.rs`, `crates/core/src/apple/iokit.rs`, `crates/core/src/apple/ioreport.rs`, `crates/core/examples/macos_probe.rs`, `crates/core/tests/fixtures/ioreport/` |
| **icon-packaging** | `assets/icon/gpuviewer.svg`, `assets/icon/gpuviewer.ico`, `packaging/icons/**/gpuviewer.png`, `packaging/gpuviewer.desktop`, `crates/tui/build.rs` |
| **release** | `.github/workflows/release.yml` |
| **integrator** | `crates/core/src/backend.rs` (registry), `crates/core/src/lib.rs` (module fences/re-exports), `crates/core/src/model.rs` (throttle Option, source_caveat, normalize_pci_id move), `crates/core/src/events.rs`, `crates/core/src/mock.rs`, `crates/tui/src/ui.rs` (n/a labels, caveat surfacing), `crates/core/Cargo.toml`, `crates/tui/Cargo.toml`, `crates/history/Cargo.toml`, `Cargo.toml` (workspace), `README.md`, `docs/spec/ndjson-v1.md`, `docs/spec/ndjson-v1.schema.json` |

Cross-stream interface freeze (so backends compile in parallel): the windows-wddm stream
publishes, as the first commit, the signatures of
`win::pdh::{shared() -> &'static SharedPdh, SharedPdh::snapshot(&self, now_ms: u64) ->
Option<PdhSnapshot>, parse_instance(&str) -> Option<ParsedInstance>}` and
`win::adapters::{enumerate() -> Vec<AdapterInfo>}` exactly as specified in §3.2/§3.3;
windows-nvidia codes against them. Sequencing exception (recorded): ndjson_contract.rs is
foundation-owned during the portability pass, then ownership transfers to integrator for
the throttle-nullable trio commit.

---

## 10. Test plan per module (CI has no GPUs — that is a feature here)

**Scripted tests that run on ANY OS** (the bulk):
- `win::pdh::parse_instance`: pure parser vs committed fixture strings
  (`pid_1234_luid_0x00000000_0x00005678_phys_0_eng_0_engtype_3D`, adapter forms, `part_N`
  forms, decoy malformed strings a wrong split would mis-parse) — runs on Linux.
- Engine-aggregation math (busiest-engine headline, per-engtype sums, NOCAP100 >100%
  handling, LUID grouping, unmatched-LUID → unattributed): pure functions over scripted
  `PdhSnapshot` values — any OS.
- `normalize_pci_id` round-trips for NVML 8-digit-domain, D3DKMT no-domain BDF, and the
  refuse-to-dedupe cases (`wddm:*`, `apple:*`, `mock:*`) — extends the existing tests when
  the helper moves to core.
- IOReport residency/energy math: per-chip committed sample fixtures
  (`crates/core/tests/fixtures/ioreport/`) decoded by unit-label (mJ/µJ/nJ) — any OS;
  channel-name matching (DIE_N_ prefixes, MCPU) against fixture channel lists.
- Throttle-nullable ripple: event-engine tests asserting None-throttle never narrates and
  resets episodes (mirrors the existing util-None blind-spot tests); history rollup
  counting with None samples; NDJSON conformance suite asserting `"throttle": null` is
  emitted and schema-valid (trio updated together).
- Data-dir resolution: per-OS unit tests via env override (the reason resolution stays
  env-based); Linux chain byte-identity covered by the existing lib.rs test.
- Mock-backend coverage of every new UI state: "n/a — not exposed by this source",
  source_caveat rendering, process_hint texts (mock grows scripted variants).

**Absence-is-normal tests** (the contract that future refactors must not "fix" into
errors):
- PDH: every code in the §3.2 table → None + one self-honesty event; first-sample
  INVALID_DATA → None frame; per-item CStatus failure → that item None. On the GPU-less
  windows-latest runner the real `PdhAddEnglishCounterW` returns `PDH_CSTATUS_NO_OBJECT` —
  a cfg(windows) integration test asserts init degrades gracefully and `--json --once`
  still emits a valid frame. **The Windows CI leg exercises the absence path for free.**
- NVML on CI runners: init fails (no nvml.dll) → backend skipped → mock fallback; the
  existing launch tests already assert the binary always renders.
- macOS: cfg(macos) smoke asserts `MTLCreateSystemDefaultDevice` non-nil with non-empty
  name (paravirt "Apple Paravirtual device" — never assert real-HW values), and Tier B/C
  probes returning None produce a fully-rendered device with caveats — pending §4.5
  ground-truth probe before the None assertions are baked.
- Quarantine-on-Windows regression test (corrupt db file → recovery, not startup failure)
  — only meaningful on the Windows leg, runs everywhere harmlessly.

**What only the CI matrix proves** (not provable locally on Linux):
- history/tui compile+test on windows-msvc and macos arm64 (libsqlite3-sys C build cannot
  cross-compile locally); LockFileEx lock semantics + the bounded-retry fix; cmd /C
  dispatch with a real cmd.exe; CRLF-free checkout under the image's autocrlf=true;
  PDH/Metal real-API behavior on GPU-less VMs; the winresource icon embed (rc.exe).

**What only real hardware proves** (manual pre-release checklist, per §1 validation
column): every "Some" cell of the two Windows rows and the macOS row; PdhCollectQueryData
latency measurement (no authoritative cost figure exists — measure before fixing the
Windows tick budget); non-English-locale Windows (wildcard fallback chain); HAGS-era
engtype inventories per driver release; per-chip IOReport channel inventories (M1→M5,
Ultra dies). Hardware-unvalidated status clears only via that checklist.

---

## Addendum (2026-06-07): objc2 `exception` feature reversed (§4.3/§8)

Verification of the integration tree found that `objc2 = { features = ["exception"] }`
pulls **objc2-exception-helper**, whose build script compiles an Objective-C shim
(`try_catch.m`) with the **host** C compiler via cc-rs. Two consequences, both
disqualifying:

1. **All-Rust dependency rule violation** — the only sanctioned compiled-C exception in
   this workspace is rusqlite's bundled SQLite; a vendored ObjC shim was never granted an
   exemption.
2. **The §5.6 Linux-host cross-check gate breaks** — `cargo check -p gpuviewer-core
   --target aarch64-apple-darwin` fails with `cc: error: unrecognized command-line option
   '-arch'` (Linux gcc, not an Apple toolchain).

**Resolution**: the feature is dropped. The hazard §4.3 cited — paravirt CI runners
raising `NSInvalidArgumentException` on *missing selectors* (Godot #101773) — is guarded
in pure Rust instead: `apple.rs` checks `respondsToSelector:` before every Metal call, so
an unanswered selector degrades to a `None` field without any unwind machinery. Residual
risk (a selector that exists but throws) is accepted and documented in `metal::probe`;
if Tier B/C work later demonstrates a real throw-path, the mitigation must be a pure-Rust
one or carry an explicit decision-record exemption here. §4.3's "exception-catching
enabled" wording and §8's dependency sketch are superseded by this addendum.

## Addendum (2026-06-07): §5.4 model change landed

`DynamicSample.throttle` is now `Option<ThrottleReasons>` (`None` = unobservable — wddm
and apple emit it; NVML query failure maps to it; AMD without a `gpu_metrics` node and
Intel without a readable `throttle_reason_status` map to it), `StaticInfo.source_caveat`
carries the macOS working-set-budget label and the WDDM duty-cycle/VidMm wording, and
`normalize_pci_id` moved to `gpuviewer-core::model`. The NDJSON spec/schema/conformance
trio was updated in the same change, plus an additive nullable `util_engine` sample field
so the WDDM busiest-engine name reaches consumers and the UI (§3.4).

Deferred from that change (recorded so it is not mistaken for done): `source_caveat` is
not yet persisted in the history store, so `report` and `.gpvr` replays of *recordings*
cannot reprint it — the live TUI (all three views) renders it from the live backends.
Persisting it is a small schema addition that belongs with the Tier B/C unfreeze.
