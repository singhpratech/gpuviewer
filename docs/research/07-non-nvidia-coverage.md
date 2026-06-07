# gpuviewer — Non-NVIDIA coverage decision record

MLX-on-Apple-Silicon honesty story · AMD all-families decoder/fixture plan · Intel
all-families plan · CI testability · phased worklist.

Status: synthesized 2026-06-07 from four research investigations (MLX/macOS, AMD kernel
audit, Intel kernel audit, CI testability audit). Companion to
`docs/research/04-synthesis.md` (v1 decision record) and
`docs/design/cross-platform.md` (in-flight Windows/macOS design — this document extends
it and amends it in exactly one place, §1.3; it does not re-litigate it). Repo claims
were re-verified against the working tree on 2026-06-07 ~13:30 (the tree was being
modified concurrently; every load-bearing claim below was spot-checked at that time).

Honesty contract (binding, restated): a metric is either **Some(value from a named real
source)** or **None with the reason stated**. No fabricated numbers, no silent zeroes,
inferences always labeled "likely". Claims below that only real hardware can validate are
marked **[HW-VERIFY]** and collected in §6.

---

## 0. Conflicts between reports and prior assumptions — resolved explicitly

1. **"Intel code never looks at hwmon" — FALSE.** The Intel backend reads
   `power1_max`, `energy1_input`, and `temp1_input` via `first_hwmon`
   (crates/core/src/intel.rs:98-123, :336-341) and both dGPU fixture trees model it. The
   real gaps are narrower: xe hwmon has **no temp1** (pkg temp is `temp2_input`, VRAM is
   `temp3_input`, kernel 6.15+ per `Documentation/ABI/testing/sysfs-driver-intel-xe-hwmon`),
   and only the card channel (index 1) is read where xe also exposes pkg (index 2). §3.
2. **"ANE Energy" channel name — WRONG.** The IOReport group is `Energy Model`; channel
   names are `ANE` (base chips), `ANE0` (Max), `ANE0_<n>` (Ultra) — verified in macmon
   `src/metrics.rs` (`starts_with("ANE")`) and mactop `ioreport.m`
   (`cfStringStartsWith(channelRef, "ANE")`). Match by contains/prefix after stripping any
   `DIE_N_` prefix, never exact names; per-chip unit labels (mJ/µJ/nJ) read from
   `IOReportChannelGetUnitLabel`. Same policy the design already sets for `GPU Energy`
   (cross-platform.md:284-290). §1.5.
3. **"MTL/ARL iGPUs run on xe" — WRONG.** Meteor Lake and Arrow Lake default to **i915**;
   xe is default only from Lunar Lake + Battlemage (kernel 6.12+), Panther Lake is xe-only
   (force_probe until 6.17). DG2/Alchemist remains i915-default with xe force-probe as a
   real user configuration. The backend's uevent `DRIVER=` dispatch (intel.rs:508-512)
   already handles every combination — keep it. §3.1.
4. **AMD `power1_input` era.** It landed in kernel **6.6** (not ~5.19–6.1): navi1x is
   average-only, Renoir-family and MI300 are input-only, RDNA3/Van Gogh/Strix have both.
   The repo's probe-both (amd.rs:108-112) is correct; CLAUDE.md needs the doc fix (§2.6).
5. **AMD fdinfo gate.** CLAUDE.md's "AMD fdinfo 5.14+" is really "node exists 5.14+,
   parseable standardized keys 5.19+" — the 5.14–5.18 dialect (`vram mem:` kB, percent
   engine lines, no `drm-pdev`) correctly yields an empty process list today. §2.6.
6. **CI matrix status.** The CI report read `.github/workflows/ci.yml` as single-job
   ubuntu; the **3-OS matrix has since landed** (verified in-tree 2026-06-07: ubuntu-latest
   lint leg, windows-latest, macos-15 pinned, clippy on every leg, macos-probe
   workflow_dispatch job — matching cross-platform.md §5.5). The report's downstream P3
   items were re-checked against the current tree and **still stand**: the event-sink test
   budget is still 2 s (collector.rs:1938, `for _ in 0..200` × 10 ms), the NVML stub `.so`
   promised by CLAUDE.md:119 still does not exist anywhere, and there is no
   fixture-count lying-green guard. §4.
7. **macOS budget mutability vs the design's Tier A placement.** cross-platform.md §4.1
   maps `recommendedMaxWorkingSetSize` to `mem_total_bytes` (StaticInfo, read once). The
   MLX investigation shows it is **runtime-mutable** via `sudo sysctl iogpu.wired_limit_mb=N`
   (takes effect immediately). This is the one genuine design amendment in this document:
   the budget must be re-read per frame (§1.3). Everything else in §1 slots into the
   design's existing tier structure without contradiction.
8. **"ANE utilization" stays in the won't-build column** (docs/research/04-synthesis.md:130).
   The new proposal is ANE **power** (observed watts), which is a different, honest claim;
   `ane_util_pct` is permanently None ("Apple publishes no ANE capacity reference") because
   the only known denominator is asitop's hardcoded 8 W — a fabricated capacity. §1.5.
9. **Concurrent-workflow note.** IOReport per-chip fixtures
   (`crates/core/tests/fixtures/ioreport/`, M2/M2-Ultra/M4 channel lists + voltage-states9
   captures) landed while this synthesis was being written — the macos-apple workstream
   from cross-platform.md §9 is in flight. `DynamicSample.throttle` is still
   non-Option (model.rs:109), i.e. the §5.4 model change has not landed yet. Worklist items
   in §5 that touch macOS or `events.rs` must rebase on that workstream, not race it.

---

## 1. The MLX story on Apple Silicon (v2 backend, device-level only)

MLX/mlx-lm local inference and LoRA training is the single biggest reason people watch
GPU metrics on Apple Silicon. The finding that makes the whole story coherent: **MLX
computes on the GPU (Metal) and CPU only, never the ANE** — maintainer awni, verbatim in
ml-explore/mlx#18 (wontfix): "at the moment we don't have plans to support ANE in MLX
given it is a closed source API" (2023-12, reaffirmed 2025-05). llama.cpp likewise skips
the ANE (ggml-org/llama.cpp discussion #336). Consequence: the device-level backend
designed in cross-platform.md §4 observes the **complete** hardware surface MLX uses.
Device-level-only is not a degraded MLX story; it is the whole MLX story.

### 1.1 Exact signals (source / key / channel / label)

| Signal | Source (tier per cross-platform.md §4.1) | Key/channel | Mandatory label |
|---|---|---|---|
| util_pct | Tier B IOKit `IOServiceMatching("IOAccelerator")` → AGXAccelerator `PerformanceStatistics` | `Device Utilization %` | duty-cycle-like; `source_caveat` private-interface stamp (design already covers this, cross-platform.md:272-274, :303-311) |
| util fallback | Tier C IOReport `GPU Stats`/`GPU Performance States` | `GPUPH` residency | DVFS-residency util ≠ Device Utilization % — pick one per frame and label which (docs/research/02-vendor-apis.md:135-138) |
| mem_used | Tier B PerformanceStatistics | `In use system memory` (sibling `Alloc system memory` = driver-reserved/mapped superset) | "GPU-mapped system RAM, unified memory — not VRAM" (design covers, :275-276). Which key tracks the wired budget that precedes MLX OOM is **[HW-VERIFY]** — see §1.3 open question |
| mem budget | Tier A Metal `recommendedMaxWorkingSetSize` | — | "unified-memory working-set budget, NOT total VRAM"; **re-read per frame** (§1.3 amendment) |
| power_mw | Tier C IOReport `Energy Model` → `GPU Energy` | unit from `IOReportChannelGetUnitLabel` | "SoC rail approximation; private interface" (design covers, :284-290) |
| sm_clock | Tier C GPUPH residency-weighted DVFS | `voltage-states9` table | private interface (design covers) |
| ane_power_mw (NEW) | Tier C IOReport `Energy Model` | `ANE` / `ANE0` / `ANE0_<n>` (see §0.2) | §1.5 |
| thermal pressure (NEW) | **Tier A public** `ProcessInfo.thermalState` + `NSProcessInfoThermalStateDidChangeNotification` | nominal/fair/serious/critical | "system-wide signal, not GPU-specific" — §1.6 |

Field evidence the Tier B keys move under MLX-class workloads: simonw/gpuer reads exactly
`ioreg -r -c AGXAccelerator -d 2` → `PerformanceStatistics` sudoless and shows GPU usage
while LM Studio serves a local LLM; the same dict
(`Device Utilization %`, `Alloc system memory`, `In use system memory`) is documented at
eclecticlight.co (M1 memory article, developer comment); macmon was written specifically
to watch local-LLM runs. MLX users already file issues from this exact signal
(mlx-examples#669 util drop at eval→generation; discussion #860 "CPUs at 100%, GPU not
utilized"). Per-chip/per-OS presence of `Device Utilization %` is **[HW-VERIFY]**
(key inventories differ across chips/OS — cross-platform.md:277-278).

### 1.2 The None ledger (each with its stated reason)

- Per-process util/memory/names → "macOS prohibits per-process GPU attribution for
  third-party tools (powermetrics requires root); device-level only." Already designed
  (cross-platform.md:295-299, :36). **Extension (new, S-sized):** the macOS
  `process_hint` should also say "inside MLX use
  `mlx.core.get_active_memory()`/`get_peak_memory()` — the OS forbids external tools from
  seeing it." Converts a dead-end into a workflow.
- `temp_c`, `fan_pct` → "private SMC/IOHID keys churn per chip generation; parked"
  (design covers, :312-316).
- `throttle` → None: "macOS exposes no GPU clock-limit reasons; see system thermal
  pressure instead." Already mandated by the §5.4 model change (not yet landed —
  model.rs:109 still non-Option). thermalState is a different claim with a different scope
  and must never be laundered into `ThrottleReasons`.
- `mem_clock_mhz`, `encoder_pct`, `decoder_pct` → not exposed (design covers, :293-295).
- `ane_util_pct` → "Apple publishes no ANE capacity reference; power only" (§1.5).
- True "VRAM total" → does not exist on unified memory; the budget is shown *as* a budget.

### 1.3 Memory narration: budget pressure, never OOM (design amendment)

The Linux `vram_pressure` text ("VRAM 92% and climbing ~X/min — likely full in ~N min",
events.rs ~:675-760, `Confidence::Likely`) would be **confidently wrong on macOS** in
both directions:

- "Full" is not a failure boundary: MLX's default memory limit is 1.5×
  `recommendedMaxWorkingSetSize` and macOS pages/compresses past the budget — crossing it
  usually means slowdown, not failure (mlx.core.set_memory_limit docs).
- The real catastrophic paths are externally invisible: Metal command-buffer OOM crash
  (`kIOGPUCommandBufferCallbackErrorOutOfMemory`, mlx-lm#854) and — worst — unbounded
  wired growth via `mx.set_wired_limit()` ending in a **kernel panic while the system
  reported `memoryPressure: false`** throughout (mlx-lm#883: 80.14 GB wired of 96 GB,
  free 0.01 GB). Wired memory bypasses normal pressure accounting. **Do not narrate
  macOS memory-pressure state as an OOM proxy** — until verified on hardware it is a
  known liar for exactly the MLX-server scenario **[HW-VERIFY: single report; confirm
  pressure-API behavior under a large set_wired_limit workload on one real machine]**.

**Decision — keep the trend math, change the claim.** On macOS the event becomes
*working-set budget pressure*, `Confidence::Likely`, ETA-to-budget, consequence
explicitly uncertain:

- Title: `"<name> GPU-mapped memory 92% of working-set budget and climbing ~X/min —
  likely reaches the budget in ~N min"`.
- Evidence: `"used <In use system memory>/<recommendedMaxWorkingSetSize> (budget, not
  total RAM; raisable via iogpu.wired_limit_mb); slope +X/min over Y min (linear
  extrapolation). Past the budget macOS may page/compress (slowdown) or Metal allocations
  may fail — outcome depends on how much memory the process has wired, which is not
  observable here."`
- The "largest holder" suffix already degrades to empty with no process map
  (events.rs:733-740) — no per-process fabrication occurs.

**Design amendment (the only one):** `recommendedMaxWorkingSetSize` is runtime-mutable —
`sudo sysctl iogpu.wired_limit_mb=N` takes effect immediately and the value jumps to
match. cross-platform.md §4.1 places it in StaticInfo (read once); it must instead be
**re-read every frame** (or on change), otherwise the percent denominator silently lies
mid-recording — a silent-zero-class violation. A budget change mid-run is itself a
narratable Fact event ("working-set budget changed 48 → 64 GiB
(iogpu.wired_limit_mb)") — exactly what a flight recorder exists for.

**Open question blocking the narration's final copy [HW-VERIFY]:** does
`In use system memory` or `Alloc system memory` track consumption against the iogpu wired
budget (the OOM-relevant quantity)? gpuer's reserved-vs-active distinction suggests
*Alloc* may be the budget-relevant numerator; the design currently maps *In use* to
mem_used. Capture both on real hardware while running mlx_lm with known
`mlx.core.get_active_memory()` values; commit the capture as a fixture; fold into the
pre-release checklist and the WWDC26 §4.6 gate.

### 1.4 Which narrated events survive device-level-only data

| Event | macOS fate | Why |
|---|---|---|
| `idle_gap` | **Survives unchanged** (Likely) | needs only util_pct (Tier B); directly matches observed MLX pain (mlx-examples#669, #860) |
| `vram_pressure` | **Survives, relabeled** (Likely) | budget-pressure reframing per §1.3; budget re-read per frame |
| `throttle_onset/recovered` | **Does not fire** | throttle = None (§5.4); no fabricated negative. Replaced by `thermal_pressure` (Fact) + `gpu_throttle_likely` (Likely) — §1.6 |
| `hang_suspected` | **Does not fire** | requires "holder alive" per-process evidence — OS-prohibited; util-flat alone is indistinguishable from idle. Honest silence |
| process lifecycle | **Does not fire** | `refresh_processes` empty + process_hint explains why (design covers) |
| collector self-honesty / blind-spot | Survive | source-agnostic |

The product fit is exact: MLX users' actual monitoring asks are (1) memory growth until
crash (mlx-lm#883 explicitly requests "memory monitoring with graceful degradation before
crashes"; mlx-examples#1262 "active memory continues to rise until the run crashes" — the
budget-pressure storyline, slope and all), (2) is-the-GPU-actually-used (idle_gap), and
(3) why did tokens/s degrade over time (thermal — §1.6: fanless Macs throttle in ~8–15 min
of sustained inference with 30–50 % tokens/s degradation **[HW-VERIFY: third-party
analyses, not reproduced]**). All three are flight-recorder questions, not live-gauge
questions.

### 1.5 New signal: `ane_power_mw` — proposed, honesty-labeled

**Decision: surface it**, as a Tier C sibling of `power_mw`.

- Source: IOReport group `Energy Model`, channel matched by `contains("ANE")` after
  stripping any `DIE_N_` prefix (names per §0.2); unit per channel from
  `IOReportChannelGetUnitLabel`; per-chip fixtures (the `fixtures/ioreport/` directory the
  in-flight workstream just created is exactly where they go).
- Label: "ANE power, W — SoC energy-counter approximation via private IOReport; private
  interface" (standard Tier C `source_caveat` stamping).
- **Honesty rule: watts only, never a percent.** `ane_util_pct` = None, reason "Apple
  publishes no ANE capacity reference" (asitop's 8 W denominator is fabricated). Keeps
  the 04-synthesis won't-build commitment intact (§0.8).
- Why it earns its place: it is the only external signal distinguishing CoreML/ANE
  workloads (whisper.cpp CoreML encoders, Apple-Intelligence daemons, Anemll-style ANE
  LLMs) from MLX/GPU workloads. Narration payoff (Likely-tier): "model running, GPU util
  ~0, ANE power 4 W → likely running on the Neural Engine via CoreML, not the GPU."
  Under MLX it reads ~0 W — itself diagnostic confirmation of GPU-path execution.
- **WWDC26 re-check gate: YES** (Tier C private interface; cross-platform.md:332-348
  applies). M5 ANE channel inventory unknown — **[HW-VERIFY]**.

### 1.6 New signals: thermal pressure (Fact) and corroborated throttle (Likely)

- **`thermal_pressure` event — Tier A, public, Confidence::Fact.**
  `ProcessInfo.thermalState` (Foundation: nominal/fair/serious/critical) +
  change notification, Apple-documented for macOS. Fires on state transitions. Copy:
  "System thermal pressure entered 'serious' (ProcessInfo.thermalState) — system-wide
  signal, not GPU-specific; macOS may reduce CPU/GPU clocks." Severity Info at fair,
  Warning at serious/critical. It is a fact *about the OS-reported state*, asserted as
  exactly that. **No WWDC26 gate needed** (public API), though the free macOS-27-beta
  smoke applies like everything else.
  - Granularity caveat baked into copy: the finer Darwin notification
    `com.apple.system.thermalpressurelevel` (public notify API, **undocumented name** —
    Tier B-grade) shows *Heavy = actually throttling* while both *Moderate* variants
    collapse into thermalState `fair` (stanislas.blog MacThrottle empirical mapping). So
    **`fair` must never be narrated as throttling.** Whether to read the finer channel is
    optional follow-up; its cross-version behavior is **[HW-VERIFY]**, and whether the
    undocumented name passes the project's private-interface policy is an open product
    question (same trust tier as the IOAccelerator keys, so precedent says yes with a
    caveat stamp).
- **`gpu_throttle_likely` event — Confidence::Likely, only when Tier C is alive.**
  Conjunction-gated: thermalState ≥ serious (or pressure level Heavy) **and**
  GPUPH residency-weighted frequency materially below the session's sustained baseline
  **while** util is high. Copy: "GPU clocks down ~X% under sustained load with system
  thermal pressure 'serious' — likely thermal throttling"; the expandable evidence row
  carries before/after MHz and the thermal state. Without frequency corroboration, never
  narrate GPU throttling from thermalState alone. (04-synthesis already anticipated
  "freq-residency drops as throttle proxy", :51.) **WWDC26 gate: YES** (depends on
  Tier C). `pmset -g therm CPU_Speed_Limit` is dead on Apple Silicon — rejected as a
  source (exelban/stats#749).

---

## 2. AMD all-families fixture + decoder plan

The AMD audit compiled the verbatim kernel structs from `kgd_pp_interface.h` and printed
`sizeof`/`offsetof` — the offsets below are compiler-verified, not hand-walked. It found
**four honesty-contract violations in shipped code**, all confirmed still present in the
working tree at synthesis time.

### 2.1 Shipped bugs (fix before any new families)

1. **v3_0 residency offsets are −2** (amd.rs:396-404 uses 226/230/…/250; compiled truth is
   228/232/236/240/244/248/252 — a 2-byte pad after `current_gfx_maxfreq` u16 @224 was
   missed). The kernel memsets the table to 0xFF before writing
   (`smu_cmn.h:51-62`), so the pad at 226-227 reads 0xFFFF → the decoder reports
   `hw_slowdown=true` on **every sample** from every Strix Point / Strix Halo / Krackan
   APU (SMU 14.0.x, kernel 6.7+). The in-file tests pass because the test builder shares
   the same wrong offsets — the self-confirming-builder trap.
2. **v3_0 (and v1_6+) residency fields are monotonic counters**, not status bits
   ("incremented on every metrics table update when X was engaged",
   `smu14_driver_if_v14_0_0.h:213-219`). Even at correct offsets, nonzero==throttling
   turns one historic PROCHOT into a permanent throttle claim. Correct decode =
   **watermark delta between reads** (same pattern as the fdinfo engine-ns logic).
3. **v2_4 size gate is 164; real `sizeof` is 168** (164 data bytes + 4 tail-pad from u64
   alignment; `structure_size = sizeof` per smu_cmn.h:60). Confirmed in-tree:
   amd.rs:355-359 still gates on 164 → every real v2_4 blob (Van Gogh, program-6
   firmware, kernel 6.6+ — i.e. **current-firmware Steam Decks**) is silently rejected.
4. **No all-FF sentinel handling anywhere.** Cyan Skillfish (BC-250-class) emits v2_2 but
   never writes `indep_throttle_status` → the field is 0xFF…FF from the memset → the
   decoder asserts thermal+power+hw_slowdown+other **all true, permanently**. Guard:
   `u64::MAX`/`u32::MAX` = "not available", fall through indep → legacy → none.

### 2.2 Version → ASIC map (kernel-source verified; what the decoder must speak)

| Version | Hardware | Status in repo |
|---|---|---|
| v1_0 (legacy@68, size **80**) | Vega12, Vega20 = Radeon VII / MI50 / MI60 | rejected → **add** (coarse `other`) |
| v1_1 / v1_2 (legacy@68, 96/104) | Navi1x on 5.12–5.13 / header-only | handled, **untested** |
| v1_3 (legacy@68, indep@112, 120) | Navi1x 5.14+, RDNA2, MI100, MI200, RDNA3 dGPU, **RDNA4 (RX 9060/9070, 6.11+)**, MI300 @6.5–6.6 | handled + the one committed binary fixture |
| v1_4 / v1_6 | header-defined; **no tagged kernel observed emitting them** | keep rejecting; offsets compiled if ever needed (v1_4 legacy@40 size 288) — [HW-VERIFY: distro stable backports] |
| v1_5 (legacy@**104**, size **360**) | MI300 series, kernels 6.7–6.12 | rejected → **add** |
| v1_7 / v1_8 (residency-**acc** counters @44-60, accumulation_counter@40) | MI300/MI325/MI350, 6.13–6.16 | rejected → **add** with delta decode |
| v1_9 (attribute-vector format) | master/6.17-dev | correctly rejected; **watch item** — sysfs serialization unknown [HW-VERIFY] |
| v2_0 (legacy@112, 120) | Renoir 5.12 only | handled, untested |
| v2_1 (legacy@108, 120) | Renoir 5.13; Rembrandt/Mendocino; Phoenix/Hawk Point; Raphael/Granite Ridge iGPU | handled, untested via committed blob |
| v2_2 (legacy@108, indep@120, 128) | Renoir-family 5.14+; Van Gogh old-fw; **Cyan Skillfish (legacy-only — the sentinel case)** | handled but sentinel-broken (§2.1.4) |
| v2_3 (legacy@108, indep@120, 152) | Van Gogh 6.3+ (fw ≥ 0x043F3E00) | handled, untested via committed blob |
| v2_4 (legacy@108, indep@120, **168**) | Van Gogh 6.6+ (program-6 fw — likely Steam Deck OLED) | **size gate wrong** (§2.1.3) |
| v3_0 (residency@228-252, 264) | Strix Point / Strix Halo / Krackan / Gorgon Point, 6.7+ | **offsets + semantics wrong** (§2.1.1-2) |

Units: v1_x temps °C / power W; v2_x and v3_0 temps **centi-°C** / power **mW**
(kgd_pp_interface.h comments; renoir_ppt.c `/100`; smu_v14_0_0_ppt.c `/100`).
A Steam Deck monitor must decode v2_2 AND v2_3 AND v2_4 (fw- and kernel-dependent).

### 2.3 Collector gaps beyond the decoder

- **MI300 temperature**: no temp1/edge channel exists (junction temp2 + mem temp3 only,
  amdgpu_pm.c) → today `temp_c = None` despite a real junction reading. Add the fallback
  chain edge → junction → temp1, surfacing which sensor was used (a Some reported as None
  is the honesty contract violated in the other direction).
- **APU/GTT memory**: `mem_info_vram_total` on APUs is the BIOS UMA carve-out (Steam Deck
  default ~1 GiB); real allocations live in GTT (`mem_info_gtt_total/used`, fdinfo
  `drm-memory-gtt`) which the repo does not read → APU memory pressure (the Deck story)
  is dramatically understated at device and process level. Read GTT, **label it
  separately from VRAM** — never sum them silently.
- **fdinfo forward-compat**: `drm-memory-<region>` is documented deprecated (amdgpu-only
  alias of `drm-resident-`); standardized 6.4+ keys scale units across bytes/KiB/MiB
  (`drm_file.c`), which the current strict-KiB parser cannot read. Media engines
  (`drm-engine-dec/enc/jpeg/vpe/dma`) unread → a VAAPI/AMF-only process shows util None.
- Real trees never contain `…: 0 ns` engine lines (the kernel omits zero counters) and
  never have edge on temp2 (channel meanings are fixed temp1=edge/temp2=junction/
  temp3=mem) — both fine as committed *decoys*, but realistic captures must not replicate
  them.

### 2.4 Fixture trees to commit (synthetic per fixtures/README.md policy; decoys adjacent
to every word read; **plus the kernel-true 0xFF decoys** — padding/unwritten fields are
0xFF on real silicon, exactly what catches offset slips)

1. **`amd-strixpoint-kernel6.10/`** — v3_0 blob, size 264; `current_gfx_maxfreq`@224 =
   2900 (decoy); **padding @226-227 = 0xFF (the killer decoy — any −2 decoder reads
   0xFFFF and false-positives)**; residency at 228-252 (e.g. prochot=37, thm_gfx=12);
   APU sysfs (no fan, temp1 edge only, power1_average+input, no caps, vram carve-out +
   GTT files). Tests: (i) static counters across two reads → **no** throttle (delta
   semantics); (ii) thm_gfx delta > 0 → thermal=true, hw_slowdown=false despite nonzero
   prochot baseline; (iii) 0xFF padding never produces a reason. PCI id placeholder
   (0x150e assumed) — confirm at capture time [HW-VERIFY].
2. **`amd-vangogh-steamdeck-kernel6.8/`** — v2_4 blob with `structure_size`=**168**,
   indep@120 = SPPT_APU, legacy=0x40@108 decoy, fan_pwm/padding @112-118 = 0xFF, **tail
   pad @164-167 = 0xFF** (a 164-gating decoder fails loudly); hwmon `power1_label`=slowPPT
   + `power2_label`=fastPPT (unique to GC 10.3.1), no fan; 1 GiB vram carve-out + 8 GiB
   GTT; one game pid (gfx ns, small vram, large gtt) + one media-only pid (dec/enc only —
   drives the engine-coverage work; until then asserts graceful ignore).
3. **`amd-cyanskillfish-bc250-kernel6.8/`** — v2_2 blob, legacy@108 = 0,
   **indep@120 = FF FF FF FF FF FF FF FF** (exactly what the driver produces). Test:
   decodes to `ThrottleReasons::default()`. Fails loudly today.
4. **`amd-mi300-kernel6.12/`** — v1_5 blob (size 360, legacy@104 nonzero, unwritten
   fields 0xFF); hwmon temp2=junction + temp3=mem, **no temp1**, power1_input only, no
   fan. Optional sibling blob `gpu_metrics.v1_7` (residency-acc@44-60) for the 6.13+
   delta decoder. MI300 XCP partition topology vs PCI dedupe is **[HW-VERIFY]** before
   claiming MI300 support.
5. **`amd-vega20-radeonvii-kernel6.8/`** — v1_0 blob (size **80**, throttle_status@68
   nonzero, fan/pcie decoys @72-75); full SOC15 dGPU hwmon in the **real** channel order.
6. **`amd-polaris-rx580-kernel6.8/`** — **no `gpu_metrics` file at all** (GC < 9.1.0 hides
   the node); full hwmon + 8-level `pp_dpm_sclk` with `Mhz` casing. The honest pre-SMU
   degradation story.
7. **`amd-phoenix-apu-kernel6.8/`** — realistic mainstream APU (v2_1 blob, temp1-only,
   power1_input µW, no fan/cap, carve-out + GTT, gtt-dominant fdinfo pid). Keep
   `amd-igpu-minimal` as the degenerate case.
8. **fdinfo dialect/edge pids** (inside trees 1/7 or a tiny `amd-fdinfo-kernel5.15/`):
   a 5.14-dialect fdinfo (yields empty list, no crash); a standardized-keys-only pid
   (`drm-resident-vram: 512 MiB`, no legacy key — multi-unit parser driver; until
   implemented asserts mem_bytes None rather than a wrong number); a pid with engine
   lines entirely absent (real idle client → util None, not 0).

**Builder policy (kills the self-confirming-builder failure mode):** in-test blob
builders must hardcode offsets as literals cross-checked against the audit's compiled
`offsetof` output (citing kgd_pp_interface.h), never derived from `throttle_layout()`;
add a cross-check test asserting the layout table against those literals.

### 2.5 Decoder work order (by severity)

(1) v3_0 offsets +2, delta decode, 0xFF guard → (2) v2_4 size 168 → (3) all-FF sentinel
guards everywhere → (4) add v1_0 → (5) add v1_5, v1_7/v1_8 (prefix-gating since tail
arrays are version-elastic) → (6) junction-temp fallback → (7) GTT device+process memory
→ (8) media-engine keys → (9) standardized memory keys multi-unit → (10) v1_9 watch item.

### 2.6 Doc corrections

CLAUDE.md "gpu_metrics … (v1.0–v3.0)" → v1.0–v1.9 + v2.x + v3.0 with MI300 churn per
release; hwmon `power1_average`/`power1_input` split is kernel **6.6**; AMD fdinfo is
"node 5.14+, parseable 5.19+".

---

## 3. Intel all-families plan (i915 + xe dialects)

### 3.1 What is true and already right (do not re-litigate)

- hwmon **is** read (§0.1) — the hypothesized "never looks at hwmon" gap is false.
- Driver matrix: i915 default through Gen9→Arrow Lake **and** DG2/Alchemist; xe default
  from Lunar Lake + Battlemage (6.12+); Panther Lake xe-only (force_probe until 6.17);
  DG2-forced-on-xe is a real user configuration. The uevent `DRIVER=` dispatch handles
  all of it.
- All four CLAUDE.md kernel gates verified by bracketing tagged kernels: i915 engine
  busy-ns 5.19, i915 per-client memory 6.8, xe fdinfo cycles 6.11, xe PMU 6.15
  (gt-actual/requested-frequency added 6.16).
- Device-utilization honesty verified: util_pct is hard None with the PMU-privilege
  rationale; fdinfo is never aggregated into a device number, so the your-processes-only
  bias touches only the labeled process table. i915 ns/wall vs xe cycles/total-cycles
  math is correct (xe `drm-total-cycles` is a RING_TIMESTAMP GT-clock value — wall time
  would be wrong, and the fixture tests are built to catch exactly that porting bug).

### 3.2 Two real hwmon gaps (bug-shaped — a Some reported as None)

1. **xe temperature**: the backend reads `temp1_input` for both dialects
   (intel.rs:98-104 confirmed in-tree); xe hwmon has **no temp1** — pkg temp is
   `temp2_input`, VRAM is `temp3_input` (6.15+, ABI-documented). A Battlemage owner on
   6.15+ gets `temp_c = None` forever while Some(pkg °C) sits in sysfs. Fix: dialect-aware
   temp (i915 → temp1, xe → temp2); **never** surface temp3 (VRAM) as device temp — a
   different physical claim.
2. **xe energy/power channel**: only card-channel (`power1_max`/`energy1_input`) is read;
   xe's scheme is index1=card (energy1 BMG/PMT-gated), index2=pkg (BMG or DG2-on-xe).
   Pkg-only platforms get power None where Some exists. Fix: prefer card, fall back to
   pkg, None only when both absent; carry a pkg-vs-card label if the model grows one.
   Channel visibility per tag/SKU read from master — **[HW-VERIFY on v6.15/v6.16 tags +
   a real DG2-on-xe box]**; whether power1_max exists on all xe dGPUs (determines whether
   the pkg fallback is ever exercised) is also **[HW-VERIFY]**.

Minor hardening: numeric-aware sort in `first_hwmon` (lexicographic `hwmon10` < `hwmon5`);
parse `drm-engine-capacity-<class>` so multi-instance classes divide by
capacity×Δtotal instead of clamping; extend the known Arc id table (A580 56a2, A310 56a6).

### 3.3 New-signal opportunity: GT-awake% from RC6/gtidle (unprivileged)

Nothing reads RC6/C6 residency despite unprivileged sysfs on both drivers: i915
`cardN/gt/gtN/rc6_residency_ms` (5.19+) + legacy `cardN/power/rc6_residency_ms`; xe
`device/tileN/gtN/gtidle/idle_residency_ms`. Δresidency/Δwall → an honest device-level
**"GT awake %"** — labeled as awake-time, **explicitly not utilization** — a real
measured Some where the device row today shows "—", and a clean idle-gap event source.
**Gate: measure first whether reading these files takes runtime-PM wakerefs that keep the
GT out of C6** (the "polling has side effects" domain rule) — **[HW-VERIFY]**. Whether
xe's act_freq reads 0 during C6 like i915's is also **[HW-VERIFY]** (the code assumes
same semantics).

Future privileged tier (behind CAP_PERFMON detection, label the source): i915 per-engine
busy-ns PMU events; xe `engine-active-ticks`/`engine-total-ticks` (6.15+) +
`gt-actual-frequency` (6.16+) — true device utilization where fdinfo cannot honestly
provide it.

### 3.4 Fixture trees to commit

1. **`intel-xe-kernel6.15-bmg/`** (highest value — **fails today**, pins the temp2 fix):
   B580 clone; hwmon gains `temp2_input`=58000, `temp3_input`=64000 (**decoy** — VRAM
   must not become device temp), deliberately **no temp1_input**; `energy2_input` (pkg,
   ≠ energy1 — channel-choice decoy), `power2_max` (decoy), `fan1_input`=1450 (asserts
   `fan_pct` stays None — no fan *max* reference), `freq0/rpa_freq` decoy; optional
   `gtidle/idle_residency_ms`. Test: `temp_c == Some(58.0)`.
2. **`intel-i915-kernel6.12-arc/`**: A770 + hwmon `temp1_input`=61000, `fan1_input`=2100
   (i915 dGPU temps/fans landed 6.12); decoy `gt/gt0/rps_act_freq_mhz` differing from
   card-level `gt_act_freq_mhz` (proves which file is read); `rc6_residency_ms` decoy
   pair (per-GT vs legacy, different values) for when RC6 reading lands. Tests:
   `temp_c == Some(61.0)`, `fan_pct == None`.
3. **`intel-i915-igpu-mtl/`**: Meteor Lake (8086:7d55, DRIVER=i915), no hwmon, no
   `lmem_total_bytes`, **both gt0 and gt1 throttle dirs with a gt1 reason set as decoy**
   (only gt0 may be consulted — MTL gt1 is the media GT), fdinfo with `system0`-only
   regions (mem must stay None) + busy-ns (per-process util must work on an iGPU).
4. **`intel-xe-igpu-lnl/`**: Lunar Lake (DRIVER=xe), no hwmon (IS_DGFX gate), freq0
   present, fdinfo `system`/`gtt` regions only and **no vram0** (mem None), rcs cycles
   (util works).
5. Optional: **`intel-xe-dg2-forced/`** (A770 with DRIVER=xe, pkg-only hwmon — pins the
   channel fallback); **`intel-i915-kernel5.19/`** (engine ns present, memory keys
   absent); plus the pre-gate fdinfo pid (memory keys but no engine keys — old-kernel
   shape, listed with util None) inside the existing 6.8 tree.

---

## 4. CI testability

### 4.1 Code-path-to-test gap table (paths with NO covering test, as of re-verification)

| # | Untested path | Location | Risk |
|---|---|---|---|
| G1 | gpu_metrics layouts v1_1, v1_2, v2_0, v2_2, v2_4 | amd.rs:296-360 | 5 of 9 offset tables unverified; v2_4 is the Steam Deck path and is actively wrong |
| G2 | NVML loader plumbing (init chain, lib_path fallback, static/dynamic opt-mapping, compute+graphics merge, process-util watermark, name basename) | nvidia.rs:75-325 | **CLAUDE.md:119 claims a CI-built stub `.so` exists; it does not** — docs-vs-reality defect |
| G3 | Engine-level cross-backend PCI dedupe (first-wins, skip+stderr) | collector.rs:387-403 | `normalize_pci_id` is pure-tested; the loop never is. Becomes load-bearing the moment wddm lands (registry order nvidia→…→wddm relies on it) |
| G4 | CpuSpillover with util None all window: `mean_util = 0/max(1) = 0` passes the `<15%` gate | events.rs:644-648 (confirmed in-tree) | **Honesty bug**: can narrate "GPU is ~idle" with zero GPU observations. CPU sample floor exists; util floor does not |
| G5 | IdleGap blind-spot reset (util→None mid-gap) | events.rs:378-385 | hang twin is tested (lib.rs:639-666); this one is not |
| G6 | VramPressure 90 s cooldown; hw_slowdown→Critical severity | events.rs:721-726, :764-770 | both branches never exercised |
| G7 | HistoryReset pending event riding the first tick | collector.rs:459-482 | store-level flag tested; engine plumbing not |
| G8 | Inter-tick CollectorStall emission; slow-probe Info note + cooldown | collector.rs:589-614, :671-698 | slow-probe is testable (~750 ms scripted backend); stall-gap needs a clock seam or goes to the manual checklist explicitly |
| G9 | Forward schema compat: `init_schema` unconditionally restamps `user_version` down to 2 | store.rs:506-525 (confirmed) | future-schema db silently downgrade-stamped; needs a product decision (refuse-to-write is the honest option) then a guard+test |
| G10 | Recorder auto-prune trigger (`PRUNE_EVERY_FLUSHES`) | history/src/lib.rs:383-389 | only direct `prune()` tested |
| G11 | Intel pre-engine-key fdinfo pid end-to-end | fixtures | parse-level absence covered; no fixture pid models the old-kernel shape |
| G12 | xe temp2 / pkg-channel paths | intel.rs:98-123 | the §3.2 fixes land untestable without trees §3.4.1 |

### 4.2 Robustness/lying-green findings (matrix is now live — these are due)

- **Event-sink test budget 2 s** (collector.rs:1938, 200×10 ms) — the one realistic
  windows-latest flake (Defender scanning cmd.exe children). Raise to ~10 s.
- **Six bare `bin()` spawns** in launch_artifacts.rs (export/view/report calls) skip the
  4-var hermetic redirection — safe today (explicit `--db`), fragile against any future
  default-dir touch. Switch to `bin_hermetic`.
- **No fixture-count guard**: amd_fixtures.rs/intel_fixtures.rs are `cfg(linux)`-gated
  (legitimately), but a cfg regression silencing them **on Linux** would pass green. Add
  a Linux-leg CI step asserting `cargo test --test amd_fixtures -- --list` enumerates >0
  tests (same for intel_fixtures). Same guard protects the default `nvidia` feature.
- **No committed manual pre-release checklist artifact** — the policy is documented in
  four places, but there is no executable document. §6 is the seed content.

### 4.3 Prioritized test additions

P1 (shipped-code de-risk): G1 blob builders from compiled offsetof literals; G2 stub
`.so` + `crates/core/tests/nvml_stub.rs` (or amend CLAUDE.md:119 — leaving the false
claim is itself a defect); G3 dedupe test via the existing `with_backends` seam; G4 fix
(+util_n floor) and test; G5 test.
P2 (engine/store): G6, G7, G8 (slow-probe half), G9 decision+guard+test, G10, G11.
P3 (matrix hardening): sink budget, bin_hermetic, --list guards, checklist artifact.

---

## 5. Phased implementation worklist (for the follow-up workflow)

Ordering principle: fixture+test work that de-risks **already-shipped** code first
(Phase 1), then extension of shipped decoders to unhandled families (Phase 2), then
new-signal work behind the honesty contract (Phase 3). Hardware-only validation is §6,
not in this list. Items touching `events.rs`, `model.rs`, `ui.rs`, or macOS must rebase
on the in-flight cross-platform workstreams (§0.9) — in particular the §5.4
`Option<ThrottleReasons>` change and the apple backend are integrator-/workstream-owned
there; nothing below duplicates them.

### Phase 1 — fix and pin shipped code

| id | title | files | rationale | effort |
|---|---|---|---|---|
| P1-1 | Fix AMD v3_0 throttle decode: offsets +2, delta-watermark semantics, 0xFF-sentinel guard | crates/core/src/amd.rs | False `hw_slowdown` on every sample from every Strix-class APU (§2.1.1-2) — an always-on wrong narration, the product-killing class of bug | M |
| P1-2 | Fix v2_4 size gate 164→168; add all-FF sentinel guards (u64::MAX indep → legacy; u32::MAX → absent) across every version | crates/core/src/amd.rs | Silent throttle loss on current-firmware Steam Decks; permanent all-reasons-throttling on Cyan Skillfish (§2.1.3-4) | S |
| P1-3 | Commit `amd-strixpoint-kernel6.10/` fixture (v3_0 blob, 0xFF pad decoy @226-227, delta tests) | crates/core/tests/fixtures/amd-strixpoint-kernel6.10/**, crates/core/tests/amd_fixtures.rs | Pins P1-1 against regression with the kernel-true decoy a −2 decoder cannot survive | M |
| P1-4 | Commit `amd-vangogh-steamdeck-kernel6.8/` (v2_4 size-168 blob, 0xFF tail pad, slowPPT/fastPPT, GTT files, media-only pid) | crates/core/tests/fixtures/amd-vangogh-steamdeck-kernel6.8/**, crates/core/tests/amd_fixtures.rs | Pins P1-2; first realistic APU-memory and Deck tree; seeds Phase 2 GTT/media work | M |
| P1-5 | Commit `amd-cyanskillfish-bc250-kernel6.8/` (indep = all-FF sentinel) | crates/core/tests/fixtures/amd-cyanskillfish-bc250-kernel6.8/**, crates/core/tests/amd_fixtures.rs | The sentinel regression test — fails loudly today, proves the guard | S |
| P1-6 | Blob-builder tests for v1_1/v1_2/v2_0/v2_2/v2_4 from compiled-offsetof literals + layout-table cross-check test | crates/core/src/amd.rs (tests module) | 5 of 9 offset tables have zero verification (G1); literals-not-layout() kills the self-confirming-builder trap | M |
| P1-7 | Intel dialect-aware temp: i915→temp1, xe→temp2; never temp3-as-device-temp | crates/core/src/intel.rs | Battlemage 6.15+ owners get None where Some(pkg °C) exists — honesty violated in the Some-reported-as-None direction (§3.2.1) | S |
| P1-8 | Commit `intel-xe-kernel6.15-bmg/` fixture (temp2=58000, temp3 decoy, energy2/power2 decoys, fan1 present, no temp1) | crates/core/tests/fixtures/intel-xe-kernel6.15-bmg/**, crates/core/tests/intel_fixtures.rs | Fails today; pins P1-7 and seeds P2 channel-fallback work | M |
| P1-9 | Commit `intel-i915-kernel6.12-arc/` fixture (temp1 Some, fan1, rps_ decoy, rc6 decoy pair) | crates/core/tests/fixtures/intel-i915-kernel6.12-arc/**, crates/core/tests/intel_fixtures.rs | No tree exercises Some(temp) on either dialect today; rc6 pair pre-stages P3-5 | S |
| P1-10 | events.rs honesty: util_n floor in CpuSpillover + tests for spillover-all-None, IdleGap blind-spot reset, Vram cooldown, hw_slowdown→Critical | crates/core/src/events.rs, crates/core/src/lib.rs (tests) | G4 is a live honesty bug ("GPU ~idle" with zero GPU observations); G5/G6 are untested narration edges. Coordinate with in-flight events.rs ownership (§0.9) | M |
| P1-11 | NVML stub `.so` + loader test, or amend CLAUDE.md:119 | new stub crate or CI step, crates/core/tests/nvml_stub.rs, .github/workflows/ci.yml, CLAUDE.md | The testing-strategy doc claims an artifact that does not exist (G2) | M |
| P1-12 | Engine-level PCI dedupe test (two scripted backends, NVML vs sysfs spellings of one address) | crates/tui/src/collector.rs (tests) | G3 — becomes load-bearing when the wddm backend lands behind the same first-wins loop | S |
| P1-13 | Matrix hardening: sink budget 2 s→10 s; bin_hermetic for the six bare spawns; `--list` fixture-count CI guard | crates/tui/src/collector.rs:1938, crates/tui/tests/launch_artifacts.rs, .github/workflows/ci.yml | The realistic Windows flake + the only lying-green hole now that the 3-OS matrix is live (§4.2) | S |
| P1-14 | History guards: forward-schema refuse-to-write (user_version > SCHEMA_VERSION) + test; Recorder auto-prune trigger test | crates/history/src/store.rs, crates/history/src/lib.rs | G9 (needs the product decision recorded in-commit: refuse is the honest option) + G10 | S |
| P1-15 | Engine HistoryReset-on-first-tick test + slow-probe note test (750 ms scripted backend); move inter-tick stall-gap to the manual checklist explicitly | crates/tui/src/collector.rs (tests), docs/release-checklist.md (stub ref) | G7/G8 — the collector self-honesty events are themselves untested claims | M |

### Phase 2 — extend shipped decoders to unhandled families

| id | title | files | rationale | effort |
|---|---|---|---|---|
| P2-1 | Add gpu_metrics v1_0 (legacy@68, size 80) + `amd-vega20-radeonvii-kernel6.8/` fixture | crates/core/src/amd.rs, crates/core/tests/fixtures/amd-vega20-radeonvii-kernel6.8/**, crates/core/tests/amd_fixtures.rs | Radeon VII / MI50 / MI60 provide a throttle story the repo currently discards | M |
| P2-2 | Add v1_5 (legacy@104, size 360) and v1_7/v1_8 (residency-acc deltas @44-60, prefix-gated sizes) + `amd-mi300-kernel6.12/` fixture | crates/core/src/amd.rs, crates/core/tests/fixtures/amd-mi300-kernel6.12/**, crates/core/tests/amd_fixtures.rs | MI300 on every kernel since 6.7 currently has no throttle decode; keep rejecting v1_9 as a watch item | L |
| P2-3 | AMD temp fallback edge→junction→temp1 with chosen-sensor surfaced | crates/core/src/amd.rs | MI300 has no edge channel — temp_c None despite real junction data (§2.3) | S |
| P2-4 | AMD GTT memory: device `mem_info_gtt_total/used` + fdinfo `drm-memory-gtt`, labeled separately from VRAM | crates/core/src/amd.rs, crates/core/src/model.rs (if a field is added), docs/spec/ndjson-v1.md + schema + conformance suite together | APU/Deck memory pressure is understated today; never silently sum carve-out and GTT. NDJSON trio rule applies | M |
| P2-5 | AMD fdinfo forward-compat: standardized `drm-resident-*` multi-unit (bytes/KiB/MiB) parser + media-engine keys (dec/enc/jpeg/vpe/dma) | crates/core/src/amd.rs, fixture pids in P1-4 tree + `amd-fdinfo-kernel5.15/` | Deprecated `drm-memory-*` will eventually vanish; media-only processes currently show util None/kind Graphics | M |
| P2-6 | Commit `amd-polaris-rx580-kernel6.8/` + `amd-phoenix-apu-kernel6.8/` trees | crates/core/tests/fixtures/**, crates/core/tests/amd_fixtures.rs | Pre-SMU hwmon-only degradation and the realistic mainstream-APU case (v2_1, GTT-dominant pid) | S |
| P2-7 | Intel xe energy/power channel fallback card→pkg + optional `intel-xe-dg2-forced/` tree | crates/core/src/intel.rs, crates/core/tests/fixtures/intel-xe-dg2-forced/** | §3.2.2 — pkg-only platforms get None where Some exists; A-series users demonstrably force xe | M |
| P2-8 | Intel iGPU trees: `intel-i915-igpu-mtl/` (gt1 throttle decoy, system0-only regions) + `intel-xe-igpu-lnl/` (no hwmon, no vram0) + pre-gate fdinfo pid | crates/core/tests/fixtures/**, crates/core/tests/intel_fixtures.rs | The iGPU half of the install base has no fdinfo-activity coverage; G11 | M |
| P2-9 | Intel hardening: numeric hwmon sort, `drm-engine-capacity-*` division, Arc id table (56a2, 56a6) | crates/core/src/intel.rs | Small correctness holes found in the audit (§3.2 minor) | S |
| P2-10 | Doc corrections: CLAUDE.md gpu_metrics version range, power1_input=6.6, fdinfo node-5.14/parse-5.19; fixtures README xe temp2/temp3 note | CLAUDE.md, crates/core/tests/fixtures/README.md | Wrong doc claims propagate into future work (§2.6) | S |

### Phase 3 — new signals behind the honesty contract

| id | title | files | rationale | effort |
|---|---|---|---|---|
| P3-1 | macOS budget-pressure narration: relabeled vram_pressure (ETA-to-budget, uncertain consequence), per-frame re-read of recommendedMaxWorkingSetSize, budget-change Fact event | crates/core/src/apple/* (in-flight workstream's files), crates/core/src/events.rs | §1.3 — the one design amendment; an ETA-to-OOM would be confidently wrong on unified memory. Lands only after the apple backend + §5.4 throttle-Option are merged | M |
| P3-2 | `ane_power_mw` Tier C metric (Energy Model, contains("ANE") after DIE_N strip, unit-label parse, per-chip fixtures) + "likely on Neural Engine via CoreML" narration; `ane_util_pct` permanently None with reason | crates/core/src/apple/ioreport.rs, crates/core/tests/fixtures/ioreport/**, crates/core/src/model.rs + NDJSON trio, crates/core/src/events.rs | §1.5 — the only external CoreML-vs-MLX discriminator; watts only, fabricated-denominator ban. **WWDC26 gate applies** | M |
| P3-3 | `thermal_pressure` Fact event from ProcessInfo.thermalState (Tier A public), system-scope copy; optional Tier B `com.apple.system.thermalpressurelevel` behind a recorded policy decision | crates/core/src/apple/*, crates/core/src/events.rs, docs/spec trio | §1.6 — the only fully-public dynamic macOS signal; `fair` never narrated as throttling | M |
| P3-4 | `gpu_throttle_likely` conjunction event (thermalState ≥ serious AND GPUPH freq drop AND high util) | crates/core/src/events.rs, crates/core/src/apple/* | §1.6 — Likely-tier only, evidence row carries MHz before/after. Gated on Tier C alive + hardware verification | M |
| P3-5 | Intel GT-awake% from RC6/gtidle deltas, labeled "awake-time — not utilization"; idle-gap event source | crates/core/src/intel.rs, crates/core/src/model.rs + NDJSON trio, crates/core/tests/fixtures (rc6 pairs from P1-9) | §3.3 — honest measured Some where the device row shows "—". **Blocked on the wakeref side-effect measurement (§6)** | M |
| P3-6 | macOS process_hint MLX extension ("inside MLX use mlx.core.get_active_memory()…") | crates/core/src/apple.rs (hint string), crates/tui/src/ui.rs if copy length needs handling | §1.2 — converts the OS-prohibition dead-end into a workflow | S |
| P3-7 | Privileged PMU tier (CAP_PERFMON detect): i915 per-engine busy-ns, xe engine-active/total-ticks (6.15+), source-labeled true device util | crates/core/src/intel.rs | §3.3 future tier — only after the unprivileged story is complete | L |
| P3-8 | Commit `docs/release-checklist.md` consolidating §6 + the existing scattered hardware items | docs/release-checklist.md, CLAUDE.md (reference) | Policy is documented in four places with no executable artifact (§4.2) | S |

---

## 6. Hardware-only validation (manual pre-release checklist seed — no fixture can prove these)

macOS/MLX:
- Which PerformanceStatistics key (`In use` vs `Alloc system memory`) tracks the iogpu
  wired budget; correlate with `mlx.core.get_active_memory()` on real hardware; commit
  the capture (blocks §1.3 final copy).
- macOS memory-pressure API behavior under a large `mx.set_wired_limit` workload
  (mlx-lm#883 is a single report; verify before the "pressure APIs are blind to wired
  growth" copy ships).
- Per-chip IOReport channel inventories M1→M5 (M5 renamed ECPU→MCPU; M5 ANE channel names
  unpublished) + macOS 26/27 — under the WWDC26 §4.6 gate.
- `Device Utilization %` presence matrix per chip/macOS release.
- `com.apple.system.thermalpressurelevel` level behavior across releases; thermalState
  `fair`-vs-actual-throttle mapping reproduction.
- Fanless-Mac throttle timing/tokens-s degradation figures (third-party, unreproduced).

AMD:
- Real gpu_metrics blob captures per ASIC/firmware (all committed blobs are hand-built);
  exact PCI ids for the Strix Point and Cyan Skillfish/BC-250 trees.
- v3_0 PM_TIMER cycle length (residency-delta → percentage conversion stays unquantified
  until sourced; delta>0 = "engaged during window" is safe).
- Whether any stable/distro kernel shipped v1_4/v1_6; v1_9 attr-vector sysfs
  serialization on ≥6.17.
- MI300 XCP partition topology vs dedupe-by-PCI-address.
- v2_x `average_gfx_activity` centi-percent vs percent per APU generation (unread today —
  must be captured before ever surfaced).

Intel:
- Whether reading `gt_act_freq_mhz`/`rc6_residency_ms`/`gtidle` takes runtime-PM wakerefs
  (blocks P3-5); whether xe act_freq reads 0 during C6 like i915.
- xe hwmon per-channel visibility on v6.15/v6.16 tags and a real DG2-forced-on-xe box;
  `power1_max` presence across xe dGPUs.
- A770/i915 6.12+, B580/xe 6.15+, MTL laptop captures to replace synthetic trees.
- Exact kernel release that dropped DG1's force_probe (doc-only).

General (from the CI audit, for the same checklist artifact):
- NVML against a real driver incl. driver-only `.so.1`-only installs; MIG devices
  returning NOT_SUPPORTED; WSL2 process-list absence hint; polling side effects (GFXOFF,
  GPU-wake, PCIe-call latency); real eGPU unplug → device_lost; shutdown-grace on a
  genuinely wedged probe; inter-tick CollectorStall on real stalls (no clock seam).
