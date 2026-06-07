# Production & Platform Deep-Dive — v0.1.0 Audited

**Date: 2026-06-07.** Builds on `01-market-landscape.md` (market matrix), `04-synthesis.md`
(decision record), and `05-competitive-deep-dive.md` (June-2026 competitive evidence). This
document is different in kind: it audits **our own tree**, not the market. Seven
investigations (production readiness, Linux coverage, the Windows v1.5 path, the macOS v2
path, the GPU support matrix, demand evidence, timeline design) were run and then
**adversarially re-verified** — every file:line citation re-read, both cross-compiles
re-run live, claims that failed verification corrected or dropped. Where a finding was
corrected during verification, this document carries the corrected form only.

The one-sentence verdict up front, because the rest of the document earns it:
**gpuviewer v0.1.0 is a ship-quality wedge demo and a pre-production always-on recorder,
and the gap is concentrated exactly on the word "always-on."**

---

## 1. Production readiness today

### 1.1 What genuinely holds

The storage/honesty layer is the production-grade part, and it is worth saying so before
the gaps, because it is the part the product thesis rests on:

- **Corruption quarantine** renames a damaged database aside (never deletes), reopens
  fresh, and narrates the reset as a `HistoryReset` event (`store.rs:186-206,271-287`;
  folded into the first tick at `collector.rs:200-224`; tested at `lib.rs:832-870`,
  asserting the `*.corrupt-*` file is preserved).
- **Absent metrics persist as NULL, never 0** (tested, `lib.rs:601-661`); event confidence
  round-trips storage and `.gpvr` export exactly (`lib.rs:770-809,1306-1313`); export
  refuses to overwrite and windows correctly, with decoy-window tests (`lib.rs:1225-1335`).
- **EPIPE is a clean Unix exit** with a tail flush (`main.rs:116-134,402-409`).
- **WSL2 and unprivileged-fdinfo limits** surface as in-UI hints instead of empty lists
  (`nvidia.rs:47-57`, `amd.rs:583-592`, `intel.rs:310-319`).
- **NVML throttle mapping is exhaustive** including unknown-future-bit tolerance and
  idle-is-not-throttle, all unit-tested (`nvidia.rs:164-193`, tests `:332-419`); NVML init
  uses the `.so.1` path first with fallback (`nvidia.rs:78-82`).
- **Resource bounds are sound for a single workstation**: RAM rings hard-capped (1800
  samples/device + 5000 events, `collector.rs:608`; drop-front ring `lib.rs:36-41`);
  SQLite bounded by retention (10s/48h, 1m/30d, events/30d — `store.rs:29-31`) with an
  ~hourly auto-prune keyed to data timestamps (`lib.rs:108,383-389`); raw 1 Hz samples
  never reach disk (`lib.rs:282-436`); event derivation is transition-based, not per-tick.
- **149 tests pass with no GPU present** (count verified by grep; all against the mock
  backend and committed fixture trees). Panic policy outside tests is clean apart from the
  `lock().unwrap()` sites flagged below.

### 1.2 The two blockers, stated plainly

**Blocker 1 — "always-on" is a process, not a property.** Recording happens only inside a
live `gpuviewer` process: persistence is wired per-process in `Engine::new`
(`collector.rs:183-231`). There is no daemon (punted by decision — `04-synthesis.md` §6,
`05-competitive-deep-dive.md` §7.2), no shipped systemd unit or service file anywhere in
the repo, and the only operational guidance is a one-line README aside ("run
`gpuviewer --json` under your own systemd user unit", README:247-249). The README headline
"It was already recording" (README:3) and the comparison row "Always-on recording, no
setup/daemon ✓" (README:67) are true only while a session happens to be open. Worse, there
is **no session-boundary event**: `Engine` starts with `last_tick_end = None`
(`collector.rs:124,242`), so a cross-restart gap can never trip the `CollectorStall`
narration (`collector.rs:270-301`) — a recorder-not-running hole replays as silently absent
buckets, while the honesty contract promises "a hole in the recording never masquerades as
the GPU having gone quiet" (README:135-138). That promise is currently implemented only for
in-process stalls.

**Blocker 2 — two instances double-write every narration.** There is no instance lock
(grep for flock/pidfile/lockfile: zero hits), each instance runs its own `EventEngine`
deriving identical events from the same hardware (`collector.rs:139-247`), and the events
table is a plain `INSERT` into an `AUTOINCREMENT` table with no UNIQUE constraint
(`store.rs:410-433`, schema `:924-933`). Both instances resolve the same default
`history.db` (`store.rs:211-219`); WAL + `busy_timeout` serializes the writers rather than
excluding them. `events_between` has no dedupe (`store.rs:464-501`), so every
throttle/attach/exit surfaces doubled in `report` and replay. **Observed live 2026-06-06:
two witnesses each recording the same throttle events.** The README's own recommended
boot-time setup (a systemd `--json` unit, README:247-249) plus normal TUI use is exactly
this two-writer scenario, and `--on-event` hooks also fire once per instance. Sample
rollups dedupe only by accident (`INSERT OR REPLACE` on the `(device_id, bucket_ms)` PK,
`store.rs:341-345`) — last-writer-wins on phase-shifted buckets.

### 1.3 Major gaps

1. **Driver death / hot-unplug mid-run — the canonical flight-recorder incident — is
   silent in the recording, the events, and the TUI.** A failing `refresh_dynamic` becomes
   `sample = None` via `.ok()` (`collector.rs:312`); with no sample, the event engine and
   recorder are both skipped (`collector.rs:345-358`), so nothing is written and no event
   can fire; the TUI keeps rendering the last good sample forever (`collector.rs:627-630`
   update `latest[i]` only on `Some`; no staleness indicator exists in `ui.rs`). No
   `DeviceLost`-style `EventKind` exists (`events.rs:31-52`); `CollectorStall` is purely
   time-gap-based (`collector.rs:273-276`), so a fast-failing probe never trips it; the
   `HangSuspected` doc comment promises "device stopped answering queries"
   (`events.rs:46-48`) but the derivation bails when util is absent (`events.rs:466-470`).
   One precision: in `--json` mode the per-tick frame **does** carry `sample: null` for the
   dead device (`docs/spec/ndjson-v1.md:73` documents it deliberately) — so an NDJSON
   consumer can see the gap; the TUI user and the replay timeline cannot.
2. **The collector thread has no watchdog, and the failure shape is worse than a crash.**
   The thread is spawned detached with no join handle or health flag
   (`collector.rs:618-657`). `engine.tick()` runs at `collector.rs:624`, *before* the
   shared mutex is acquired at `:625` — so a panic inside a backend (the code path least
   exercised by the mock-based suite) does **not** poison the mutex and does **not** crash
   the UI: the thread dies, recording stops, and the TUI draws stale, plausible-looking
   data forever with zero signal of any kind. (A panic inside the locked region
   `:625-636` would instead crash the UI later through the `lock().unwrap()` chain at
   `app.rs:151` / `collector.rs:625`.) For a tool whose value is "it was recording when it
   mattered," silent recorder death behind a live-looking UI is the worst failure shape.
3. **Mock fallback can silently substitute simulated telemetry in unattended use.**
   `all_backends` falls back to `MockBackend` whenever no real backend initializes
   (`backend.rs:77-79`) — e.g. a driver upgrade breaking NVML init at boot. There is a
   stderr breadcrumb ("gpuviewer: nvidia backend skipped: …", `backend.rs:62`), but the
   `--json` envelope carries no machine-readable mock flag (`main.rs:365-372`,
   `collector.rs:18-26`); the only in-stream marker is the `mock:` device-id prefix
   (`ndjson-v1.md:70`). A continuous `--json` unit would stream simulated data
   indefinitely. The mitigations (separate `history-mock.db` by default, TUI "(mock data)"
   footer label, both tested) are **bypassed by `--db`**: an explicit path wins regardless
   of `mock_in_use` (`collector.rs:183-187`), so a mock-fallback run with `--db` writes
   simulated rollups into the user-chosen real database — contradicting README:19-20.
4. **The AMD backend has never executed on real AMD silicon.** Validation is fixtures
   only, and the fixture trees are explicitly synthetic (hand-written;
   `crates/core/tests/fixtures/README` says so in its first line — and note current
   CLAUDE.md discloses this correctly; the stale "captured from real hardware" wording
   survives only in `03-stack-evaluation.md:86`, a plan document, and must not be quoted as
   current state). Real-hardware coverage is one NVIDIA RTX 4090 Laptop plus Intel iGPU
   enumeration. Meanwhile README:70 publicly checks "Per-process attribution
   (NVIDIA + AMD + Intel incl. xe) ✓" and README:160-163 lists AMD under "Shipped". The
   honest wording is **"implemented, synthetic-fixture-tested, not hardware-validated."**
5. **Zero release engineering.** No CI of any kind (`.github/` absent — confirmed),
   despite a test suite designed to run GPU-free in CI and despite `04-synthesis.md` §3
   promising `cargo test --no-run` compile-gating on all three OSes. No packaged binaries
   (README:155-156 admits it), no man page, no MSRV in any `Cargo.toml` — while the
   workspace comment admits toolchain sensitivity (rusqlite pinned to 0.39 because rustc
   1.94.0's `cfg_select` breaks libsqlite3-sys 0.38, `Cargo.toml:19-24`). Bonus finding:
   CLAUDE.md's claimed "CI-built stub `.so`" NVML loader test does not exist anywhere in
   the tree either — it is a plan, not an artifact.

### 1.4 Minor but real

- **Shutdown flush is partly fictional.** `Engine::Drop` claims to flush on "Ctrl-C / q"
  (`collector.rs:396-401`), but in TUI mode the Engine lives in the detached collector
  thread, which is killed without unwinding when `main` returns — Drop never runs; only
  the `--json` paths flush explicitly (`main.rs:404-414`). No SIGTERM/SIGINT handler
  exists (signal-hook in Cargo.lock is transitive via crossterm), so systemd stopping the
  recommended `--json` unit also skips the flush. Loss is bounded (events persist per
  tick; only partial 10s/1m buckets are at risk), but the in-code claim is wrong and a
  service-managed recorder loses its tail on every restart.
- **Logging is stderr-only and partially self-defeating.** Nine `eprintln` sites, no log
  framework/levels/file. The `Engine::new` warnings fire on the main thread before raw
  mode begins and remain readable; the two `EventSink` warnings — rate-cap
  (`collector.rs:443-447`) and `--on-event` spawn failure (`:471`) — are emitted from the
  collector thread mid-TUI and are invisible or screen-corrupting in the raw-mode
  alt-screen. An always-on recorder has no diagnostics trail of its own.
- **Suspend/resume — the everyday laptop case — produces an un-narrated hole.** The stall
  gap is measured with `std::time::Instant` (`collector.rs:273-276`), which on Linux is
  `CLOCK_MONOTONIC` and excludes suspend: a multi-hour sleep produces **no event** while
  the wall-clock-keyed timeline shows a multi-hour hole. If the gap does fire, the
  narration hardcodes the wrong cause ("a backend probe blocked", `collector.rs:289-293`).
  Event timestamps are wall-clock `now_ms` with no monotonic anchoring (`model.rs:161-166`),
  so NTP steps can also disorder the timeline.
- **`--on-event` design is sound** (event data passed via `GPV_EVENT_*` env vars, never
  interpolated into the command string, `collector.rs:454-465`; 60/min rate cap) with two
  residual gaps: `GPV_EVENT_TITLE`/`EVIDENCE` embed untrusted local process names (NVML
  `sys_process_name` / `/proc/<pid>/comm`) and no documentation warns hook authors to
  quote; and hung hook children are only reaped via non-blocking `try_wait`
  (`collector.rs:477-480`), so a never-exiting hook accumulates live children without
  bound (the cap limits spawns/min, not total alive — up to ~86k/day).

### 1.5 Verdict

Eleven of thirteen production findings survived adversarial verification as stated,
including both blockers; the two corrections (the panic-before-lock mechanism, the
main-thread eprintlns) made the picture worse and better respectively, not different.
The blockers are tractable: an instance lock + an event dedupe key, a shipped systemd user
unit + session-boundary events (the daemon punt itself stands — a unit file is not a
daemon/client split), a device-lost event + staleness marking, and CI. **Until they land,
the public claim should be "records while open," not "it was already recording."**

---

## 2. Platform support

### 2.1 Linux today

Linux coverage is real but sharply tiered, and any public statement must use the tiers.

**NVIDIA (proprietary driver) — works, hardware-validated. The only such cell.** Near-
complete device telemetry (util, VRAM, power + enforced limit, temp + slowdown threshold,
fan, SM/mem clocks, enc/dec util, driver version — `nvidia.rs:204-251`); exhaustive
throttle-cause decoding including SW_POWER_CAP and SW_THERMAL, the dominant consumer causes
that rivals skip (gpud ships HW bits only — `05-competitive-deep-dive.md` §2/§3.1);
per-process PIDs + VRAM with C/G merge, per-PID sm-util via watermark (explicitly demoted
to "populate a column, never headline numbers", `nvidia.rs:296-309`), CPU% and container
identity from `/proc`. Validated on the RTX 4090 Laptop dev machine. NVML-only: nouveau/NVK
cards are simply never listed — there is no DRM fallback.

**AMD (amdgpu) — implemented, synthetic-fixture-tested, never run on real silicon.** Broad
on paper: sysfs/hwmon device coverage, versioned `gpu_metrics` throttle decoders for
v1.1–1.3, v2.0–2.4, and v3.0 with honest-absence gates on truncated/lying blobs
(`amd.rs:290-487`), fdinfo per-process. Honest NOs by design: enc/dec util `None` (no VCN
decoder yet), `temp_slowdown_c` `None` (temp1_crit is not the throttle knee),
`driver_version` `None` (in-tree driver); throttle exists only where `gpu_metrics` exists
(SMU-era ASICs, Navi10+/Renoir+ — Polaris/Vega10 degrade to no throttle story). Known
wedge-relevant defects:

- **ROCm/KFD compute reads ~0% and is misclassified as Graphics** — the AMD ML-training
  user, the product's target, renders as an idle graphics process. The `/sys/class/kfd`
  cover is a "comes later" code comment (`amd.rs:875-881`) and a must-cover in
  `02-vendor-apis.md:56-57`; it is unimplemented.
- **fdinfo memory rides only the deprecated key**: the parser reads `drm-memory-vram`
  alone (`amd.rs:527`), not `drm-resident-*`/`drm-total-*`, though the repo's own research
  says prefer `drm-resident-*` (deprecated since kernel 6.13, `02-vendor-apis.md:55`).
  Blast radius, stated precisely: the device-level VRAM→OOM **ETA is unaffected** (it
  reads sysfs `mem_info_vram_used`, `events.rs:654-708`); what silently blanks is
  **per-process attribution** — the "(largest holder: …)" suffix, attach/exit freed-VRAM
  amounts, and the process-table MEM column. Media engines (dec/enc/jpeg/vpe) are also
  unparsed, so AMD video workloads read as idle Graphics.
- **APUs (Steam Deck, laptops): the memory story tracks the wrong quantity.** The backend
  reads `mem_info_vram_total/used` unconditionally (`amd.rs:778,805`) — on an APU that is
  the fixed carve-out, which `02-vendor-apis.md:70-71` calls "meaningless (GTT is what
  matters)". GTT is never read; the VRAM-pressure/ETA narration on an APU is computed over
  the carve-out while real allocations land in GTT. The all-None worst case is tolerated
  and tested (amd-igpu-minimal fixture); v2.x/v3.0 APU throttle decoders exist but have
  never seen a real APU blob.
- **Old `radeon`-driver cards appear listed-with-nulls, unexplained**: `discover()`
  filters on PCI vendor 0x1002 only with no `DRIVER=` check (`amd.rs:679-709` — unlike
  Intel, which gates on i915/xe), so a radeon-era card enumerates and renders "—" on
  nearly every gauge with no in-UI explanation that the driver itself is unsupported.

**Intel (i915 + xe) — honest-by-design but thin.** Both fdinfo dialects are genuinely
implemented and dispatched per-device from `uevent DRIVER=`: i915 busy-ns over wall time
vs xe cycles over GT ticks (the classic porting bug is explicitly tested against,
`intel_fixtures.rs:278-316`); act-freq with RC6-zero→None honesty; RP0 as max (not the
user cap); dGPU hwmon temp + energy-derived power; status-gated throttle decode on both
dialects including the xe `freq0/throttle/` spelling — **a metric intel_gpu_top still does
not surface on xe** (the repo's own test comments; broader "first" claims are not
verifiable). The cost of honesty: device-level util, mem-used, fan, mem-clock, and enc/dec
are hardcoded `None` (`intel.rs:641-668`) — the PMU path needs root/CAP_PERFMON and is not
implemented even for privileged runs, and there is no RAPL fallback — and iGPUs have no
hwmon, so on the ubiquitous Intel iGPU the device pane is **permanently dashes with the
current code**; no privilege or kernel upgrade changes that. The why-it's-absent
explanation exists only as process-pane hints; the gauges just show "—".

**A cross-cutting defect this combination creates: adaptive backoff is unreachable on any
machine containing an Intel device.** `device_is_idle()` returns false whenever `util_pct`
is `None` (`collector.rs:530-539` — "we cannot prove it is asleep"), Intel util is *always*
`None`, and backoff requires **all** devices idle (`collector.rs:631-633`) — so on
NVIDIA+Intel hybrids (including the dev laptop) the cadence never stretches and NVML is
polled at full rate forever. The bottom #1291 mitigation — a CLAUDE.md requirement and a
README honesty-contract bullet (README:139-141) — is dead code on exactly the hardware
that needs it.

**Kernel gates** match CLAUDE.md and degrade by absence (missing file/key → `None`), never
by version sniffing: AMD fdinfo 5.14+/5.19+, RDNA3 `power1_input`-only 6.7+; i915 engine
5.19+, per-process memory 6.8+, hwmon temp/fan 6.12+; xe cycles 6.11+, PMU and temps
6.15+, fans 6.16+. Practical consequence: an Arc B580 on Ubuntu 24.04's stock 6.8 kernel
shows no per-process util, no temps, no fans, no VRAM total, no device util — mostly a
frequency + power-delta + throttle display. The Intel Arc path, like AMD, has never run on
real Arc silicon.

**Edge hardware, case by case:**

- **WSL2 — the best-executed cell.** Device-level NVML works; per-process is N/A at the
  driver level forever (microsoft/WSL #9938/#11277; nvtop #432 is the cautionary crash).
  The in-UI explanation is implemented end-to-end — kernel-release detection, hint carried
  in `StaticInfo::process_hint`, rendered as the process pane's bottom title, with a
  render test (`nvidia.rs:38-57`, `ui.rs:517-529,787-818`). Caveat: never executed on an
  actual WSL2 system.
- **MIG — the promise not kept.** No `mig_mode()` check exists anywhere (grep: zero hits),
  despite `02-vendor-apis.md:30-31` ("must check mig_mode() first"). The generic
  `opt()`→None mapping prevents crashes — README's "unavailable, never crash" promise for
  MIG is kept — but unlike WSL2 there is **no in-UI explanation of why** util reads "—" on
  a MIG box. Per-MIG-instance util needs DCGM, which is punted ("no usable DCGM Rust
  bindings exist"). No MIG fixture, no test, never run on MIG hardware.
- **Jetson/Tegra — unsupported and untested**, and the failure mode depends on JetPack:
  where NVML is genuinely absent (older JetPacks), the registry falls through to the
  labeled mock — the user sees simulated desktop GPUs; on Orin-era JetPack 5+,
  `libnvidia-ml.so.1` ships, so the integrated GPU would likely enumerate as a real but
  mostly-unavailable NVML device. Neither path is tested on Tegra. The only documentation
  is the §6 punt ("Exotic vendors … opportunistic, post-v2"); README never says Jetson is
  out of scope. nv-monitor owns this segment (`05-competitive-deep-dive.md` §2).
- **Virtualized GPUs — unhandled and untested everywhere.** virtio-gpu (vendor 0x1af4)
  matches no backend → mock fallback. Full passthrough should look native but is
  unverified. SR-IOV VFs (NVIDIA vGPU guest NVML, amdgpu/i915 VFs) would enumerate through
  the existing vendor filters with many metrics legitimately `None` — plausible, unproven;
  no fixture models any of it.
- **Unknown DRM devices — silently absent.** Enumeration filters by PCI vendor (AMD) or
  vendor+driver (Intel), and NVIDIA exists only through NVML — so nouveau, virtio, msm,
  panfrost, v3d devices are never listed, with no "we see cardN but don't support it" row
  anywhere. For a tool whose thesis is explaining absence, **unsupported-hardware absence
  is the one absence it never explains.** (A generic DRM fallback is architecturally cheap
  — the trait, model, and registry need ~nothing; the duplicated fdinfo scan machinery in
  amd.rs/intel.rs wants factoring first — but it is a punted post-v2 item per the decision
  record, and the cross-backend PCI dedupe that would then become load-bearing is currently
  unreachable code: no two shipping backends can report the same device.)

### 2.2 Windows v1.5 (NVIDIA only)

**The port is architecturally cheap and the blockers are precisely known.**
`gpuviewer-core` already cross-compiles for `x86_64-pc-windows-gnu` (verified live during
the audit); `nvidia.rs` is correctly cfg-gated with cpu/container honestly `None` off-Linux
and `process_name` already splitting on `\` paths; the entire history/events/TUI stack is
platform-neutral by inspection (zero unix-only imports anywhere; the tui/history
cross-check fails only at the missing mingw C compiler for bundled SQLite — an environment
limitation). But no CI exists to prove any of it, so Windows compile status is unproven
for two of three crates.

**What NVML gives a Windows user, before any new collector work:** all device telemetry
(util, VRAM used/total, power, temps, clocks, fan, enc/dec), the flagship throttle
onset/recovery narration with the full cause bitmask (consumer Kepler+ under Windows;
`map_throttle` is pure and portable), device-level VRAM→OOM events, process attach/exit
lifecycle (PIDs + names), **per-PID sm util** — the HAGS-immune number Task Manager cannot
show (Task Manager reads ~3% for a maxed CUDA workload: the 3D engine is the default graph
and HAGS removes the CUDA graph entirely; Microsoft's answer is "wait" — Q&A 3903903,
unchanged through 25H2) — and the entire history/replay/report/export/view + NDJSON
surface. Precision from verification: NVML's per-process call returns sm/enc/dec samples
but the code maps **only `sm_util`** into the model's single `util_pct` field
(`nvidia.rs:305`); per-PID enc/dec is discarded today.

**The WDDM VRAM dual-source requirement.** Per-process VRAM via NVML is **always N/A under
Windows WDDM** — the Windows kernel owns VRAM and GeForce cannot switch to TCC — so
`GetComputeRunningProcesses` returns PIDs with `usedGpuMemory = Unavailable`
(`02-vendor-apis.md:22-25`; matrix row "❌ WDDM (use PDH)"). The researched mandatory fix
is dual-sourcing PDH **"GPU Process Memory"** counters (Task Manager's own source) and/or
`D3DKMTQueryStatistics` via the official `windows` crate. Zero PDH/D3DKMT code exists in
the repo today. `05-competitive-deep-dive.md` §3.2-6 notes honest NVML+D3DKMT
dual-sourcing of per-process VRAM is "unshipped by anyone" — it is both the blocker and
the differentiator. Without it, three of the six narration families are structurally dead
on Windows (IdleGap, HangSuspected, CpuSpillover — their honesty gates require per-PID
memory/CPU that WDDM-NVML cannot supply: `events.rs:434-437,478,571,612`); throttle,
attach/exit, and device-level VRAM pressure still work.

**Porting fixes a naive build would miss, all verified at line level:** persistence
silently defaults OFF on native Windows (data-dir resolution is `$XDG_DATA_HOME`/`$HOME`
only, duplicated at `store.rs:841` and `main.rs:727` — needs `%LOCALAPPDATA%`);
`--on-event` spawns `sh -c` (`collector.rs:454-456` — needs `cmd /C`); and the
`VramPressure` "largest holder" suffix uses `max_by_key(mem_bytes.unwrap_or(0))`
(`events.rs:709-714`) — when **all** processes have `mem_bytes = None`, exactly the
WDDM-without-PDH state, it names an arbitrary process with zero evidence. That latent
confidently-wrong narration is reachable **today** on WSL2 and on Linux kernels with
fdinfo engines but no fdinfo memory — fix it before, not at, the port.

**Why Windows AMD/Intel wait for v2:** ADLX carries a custom EULA needing legal review,
requires capability-checking every metric, has **no per-process API** (per-process on
Windows is PDH/D3DKMT regardless of vendor) and no event surface; the only Rust binding
(adlx-rs) is 0.0.0-alpha and stale, so the plan is hand-rolled COM vtable FFI. IGCL is
broken per-SKU on Battlemage as of 2025-26 — VRAM/card energy counters = 0 (#138), memory
bandwidth = 0 (#120), PCIe structs = 0 (#149) — demanding per-SKU ground-truthing against
the compute-runtime #932 matrices; L0 Sysman needs Administrator for temps/power on
Windows (#932); zero Rust bindings exist for IGCL. Both are YELLOW in the synthesis
matrix ("brittle tripod… sequence after Linux and Windows-NVIDIA"), and we have zero real
AMD/Intel-dGPU hardware validation even on Linux.

### 2.3 macOS v2 (Apple Silicon, device-level only)

**Everything in this subsection is dated pre-WWDC26 (June 8–12 — the keynote is
tomorrow).** The repo's own gate requires re-verifying macOS per-process APIs after the
keynote before any v2 scope commitment (`05-competitive-deep-dive.md:120-121,342`); a
per-process GPU API announcement would erase a differentiator, further private-API
breakage would add cost.

**Device-level is GREEN — with a permanent treadmill.** The full sudoless recipe is
mapped (`02-vendor-apis.md:124-145`): IOReport private dylib (Energy Model power, GPU
performance-state residency → util + residency-weighted freq from IORegistry pmgr
`voltage-states9`), AGXAccelerator `PerformanceStatistics` (Device/Renderer/Tiler
utilization, "In use system memory", core count, recoveryCount), SMC `Tp*`/`Tg*` + IOHID
temps/fans, Metal `recommendedMaxWorkingSetSize` as the memory budget. The treadmill is
concrete, with named incidents: per-chip unit-label churn (mJ on M1 vs µJ/nJ on M3/M4;
freq Hz on M1–M3 vs kHz on M4+ — always parse `IOReportChannelGetUnitLabel`), channel
renames (ANE0/ANE/DIE_N_ on Ultra; M5's MCPU* channels → macmon panic #47; the macOS 26
format change forced dual parsers), SMC key names shifting every SoC generation, ≥100 ms
minimum sample spacing, and guaranteed Mac App Store rejection (private APIs throughout;
distribute via brew/Developer-ID). Two recorded doc defects to resolve at v2 scoping:
`04-synthesis.md` risk 7 says "lean on the maintained macmon crate" while
`02-vendor-apis.md:159-161` says "no general IOReport crate exists — every tool vendors
its FFI" and prescribes porting ~300–500 lines of macmon's `sources.rs` (MIT) — the
realistic plan is the vendored port; and the documented direct `#[link]` against
`/usr/lib/libIOReport.dylib` conflicts with CLAUDE.md's never-hard-link rule — the macOS
carve-out (dyld shared cache, not Linux soname churn) should be an explicit recorded
decision, not an accident.

**Per-process GPU is RED — do not promise.** Multiply evidenced: XNU `rusage_info` has no
GPU field (verified through RUSAGE_INFO_V6; only `ri_neural_footprint` for ANE memory
works sudoless); powermetrics' per-process "GPU ms/s" requires root **and** reads 0 on
Apple Silicon; `task_for_pid` needs root + SIP exemption; Activity Monitor uses
unreplicated private sysmond accounting. **Even root doesn't fix it.** Per-PID VRAM does
not exist in any public API. Competitors' "per-process GPU" on macOS is mactop v2's
experimental AGXDeviceUserClient AppUsage scrape — approximate, undocumented, rescaled.
The only honest sudoless signal is context **presence**: IORegistry `IOUserClientCreator`
("pid N, ProcessName"). The shipped README already carries the honest framing
(README:242-244).

**The v2 engineering delta is larger than "add a backend":**

- Three shipped narrations (IdleGap, HangSuspected, CpuSpillover) are structurally
  impossible — their gates need per-PID memory ≥256 MiB/1 GiB/2 GiB and cpu_pct from
  Linux `/proc` (`events.rs:81,89,97`); the macOS story feed reduces to throttle-proxy
  events + attach/exit presence + device-level memory pressure + collector self-reports.
- Throttle facts must be redesigned: throttle events are `Confidence::Fact` off a hardware
  bitmask (`events.rs:252-257`), but macOS has only proxies (NSProcessInfo thermal
  pressure; freq-residency drops). Mapping a proxy into `ThrottleReasons.thermal` as Fact
  would break the two-tier contract — v2 needs OS-asserted-thermal-pressure with distinct
  wording, or a Likely-tier proxy event. Undesigned today.
- The unified-memory story mostly ports (`vram_pressure_events` is source-agnostic math),
  with four real deltas: the budget must become runtime-mutable (`iogpu.wired_limit_mb` is
  a live sysctl; `StaticInfo.mem_total_bytes` is queried once); "VRAM" is hard-coded in
  the narration title, the UI chart label, and the NDJSON spec semantics — relabeling must
  be additive under the spec's compatibility promise; per-PID climb attribution is
  impossible (copy must stop promising it on macOS); and the gpuer-validated "fits a ~13B
  model" headroom framing is net-new narration — "live macOS wired-limit headroom:
  nowhere" remains the one unserved Mac angle.
- The `events.rs:709-714` largest-holder bug bites here too — and verification showed it
  does **not** "silently drop" with a presence-only process list: `max_by_key` over
  all-zero keys still returns a process, so the title would misattribute an arbitrary
  process as "largest holder". Gate on `mem_bytes.is_some()`.

Carry-over status, honestly graded: `gpuviewer-core` type-checks clean for
`aarch64-apple-darwin` (verified live); `Vendor::Apple` and a platform-key `DeviceId` are
pre-plumbed; history/tui contain zero `cfg(target_os)`. But no macOS build has ever been
*executed* anywhere (no CI, no Mac hardware) — v2 starts from zero validated runtime. And
the market context stands: device-level macOS monitoring is explicitly "NOT a real gap"
(macmon/mactop v2/Stats are active and sudoless); the defensible v2 residual is the
platform-neutral spine — history + narrated events + replay + the wired-limit/model-fit
story — and it is a race (a SQLite+events release from macmon or mactop occupies the slot;
tripwires in `05-competitive-deep-dive.md` §6).

---

## 3. The GPU support matrix

One table, honestly worded. "Validated" means executed on that hardware by us; v0.1.0 has
**exactly one validated cell**.

| Segment | Status | The honest one-liner |
|---|---|---|
| NVIDIA consumer dGPU · Linux · proprietary driver | **Works today — validated** | Full device telemetry, throttle causes incl. SW power-cap/thermal, per-process VRAM+sm-util, history/replay/events. The anchor. |
| NVIDIA Optimus hybrid laptops | **Works — with a defect** | Telemetry fine; adaptive backoff never engages (Intel util=None pins the idle detector), so polling keeps the dGPU awake — the bottom #1291 mitigation is dead on this segment. |
| NVIDIA datacenter (non-MIG) | **Should work — never validated** | Same NVML paths; ECC/Xid events are v2 roadmap, not shipped. |
| NVIDIA + MIG enabled | **Partial — unexplained** | Device util legitimately NOT_SUPPORTED → renders "—" with no in-UI why (no `mig_mode()` check exists); per-MIG-instance util: **never** (needs DCGM, punted). |
| NVIDIA on nouveau/NVK | **Never listed** | NVML-only enumeration; no DRM fallback (post-v2 punt). |
| NVIDIA Jetson/Tegra | **Unsupported, untested** | Labeled mock (NVML absent) or sparse NVML device (Orin-era JetPack 5+), depending on stack. Punted post-v2; nv-monitor owns it. |
| AMD dGPU (amdgpu, SMU-era: Navi10+/Vega12/20) | **Implemented — fixture-tested only, never on real silicon** | Device + throttle decoders v1.1–v3.0 + fdinfo per-process; ROCm/KFD compute reads ~0% as Graphics; per-PID memory rides a deprecated fdinfo key. |
| AMD dGPU pre-SMU (Polaris/Vega10, amdgpu) | **Partial** | Device metrics minus any throttle story (no `gpu_metrics` node) — degrades honestly. |
| AMD on old `radeon` driver | **Listed with unexplained nulls** | Vendor-only filter enumerates it; nearly every gauge "—" with no "unsupported driver" explanation. |
| AMD APU / Steam Deck | **Partial — memory story misleading** | VRAM narration tracks the carve-out; GTT (what matters) is never read. Never run on a real Deck. |
| Intel iGPU (i915) | **Partial by design — validated (enumeration)** | GT freq, throttle reasons, per-process on 5.19+/6.8+; device util/mem/temp/power/fan permanently "—" with current code (PMU/RAPL unimplemented; no hwmon on iGPU). |
| Intel Arc DG2 (i915) / Battlemage (xe) | **Implemented — never on real Arc** | Both dialects incl. xe throttle decode (which intel_gpu_top still doesn't surface); on stock 6.8 LTS kernels most metrics are gated away (cycles 6.11+, temps 6.15+, fans 6.16+). |
| WSL2 (NVIDIA) | **Device-level works; per-process never** | Driver-level N/A (WSL #9938/#11277) — and the absence is explained in-UI, end-to-end tested. The best-executed degradation. Never run on actual WSL2. |
| Virtualized: virtio-gpu / SR-IOV VFs / vGPU guests | **Unhandled / unproven** | virtio → mock fallback; passthrough should look native (unverified); VFs plausible with many Nones. No fixtures. |
| Qualcomm Adreno, Mali, VideoCore, other platform DRM | **Not planned (post-v2 opportunistic at best)** | Invisible today; if the only GPU, the user sees the labeled mock. No "we see it but don't support it" message exists. |
| Windows NVIDIA | **Planned — v1.5** | Device + throttle + per-PID sm-util work via NVML; per-process VRAM requires the PDH/D3DKMT dual-source (unbuilt); 3 of 6 narrations dark until then. |
| Windows AMD / Intel | **Planned — v2** | ADLX (EULA, no per-process, no events) + IGCL (per-SKU zeros #138/#120/#149) + PDH; brittle tripod, sequenced last. |
| Apple Silicon (device-level) | **Planned — v2** | Sudoless private-API recipe mapped; per-chip-generation breakage expected; today macOS shows only the mock. |
| Apple Silicon (per-process) | **Never** | OS-prohibited; even root doesn't fix it. We say so rather than scrape. |

---

## 4. Demand evidence

### 4.1 The strongest signals

All re-verified June 2026 (`05-competitive-deep-dive.md` §§1,3,4; `01-market-landscape.md`):

1. **The wedge request, verbatim, declined by the best tool in the space:** nvitop #217
   (closed-wontfix 2026-05-19) — a user asks "did util drop off 5 minutes ago when the
   dataloader switched shards?" and is told to run Prometheus.
2. **"Record it so I can look later" declined across every incumbent:** nvtop #65/#64/#234;
   nvitop #20/#167 ("a bit heavy to have prometheus and grafana… is there a lightweight
   solution?"); bottom #1389 (open — asks for NDJSON *and* "feed it back in and view it",
   both already shipped here); gpu-hot #32 (declined); Stats #2663 (closed-duplicate).
   Scale of the underlying question: unix.SE "GPU usage monitoring (CUDA)" at 1,143,150
   views; SO 8223811 at 503k — accepted answers are still CSV hacks.
3. **"Tell me what happened, not a bitmask":** gpustat #135 (throttle display, open since
   2022-10, unanswered); bottom #1046 (events feed, open 3 years); LACT #307 (raw bits
   confuse users); NVSentinel #890 (P1 — NVIDIA's own fleet tool asked for throttle-onset
   detection); dcgm-exporter #348.
4. **"Why was my GPU idle" is an 8-year forum ritual:** PyTorch t/170801 mega-thread and
   siblings; r/MachineLearning gpu_sentinel (86 pts — "over a thousand dollars of cloud
   charges" from an uncatchable hang); Expanse Show HN (101 pts — 59% of a national
   cluster's compute wasted).
5. **VRAM creep and silent CPU fallback:** ollama #10597, #16336; vllm #36973 (every
   diagnosis is serial nvidia-smi screenshots hours apart); ollama #14258 — open, "single
   most common source of user confusion," 500+ related issues, fix-in-flight is a
   log-level bump.
6. **Category validation without execution:** GPU Hot hit the HN front page (1.5k★) on
   exactly this pitch and shipped no persistence through 9 releases; gpuer (simonw)
   validated the macOS memory-story demand and stalled. The niche is simultaneously
   validated and empty.

### 4.2 The honest counter-case

The same evidence read skeptically:

- **Every signal above is demand for the category, not for gpuviewer.** v0.1.0 has shipped
  no binaries, no releases, no packaging — our own demand is unmeasured. Issue-thread
  reactions and SE view counts measure curiosity and pain, not willingness to install and
  keep a recorder running.
- **"Validated and empty" has a second reading.** nvtop, bottom, btop — maintainers with
  millions of installs and a decade of distribution — declined recording and events for
  *years* (nvtop #40/#64/#65/#234; bottom #1046). Their revealed assessment may be that
  the maintenance and scope cost exceeds the demand. We are betting they are wrong; it is
  a bet.
- **The kill list shows how fast "nobody does X" dies** (`05` §3.1): "nobody decodes
  throttle bitmasks" was killed by LACT/gpud/amdgpu_top/GPU-Z; "nobody ships replay" by
  all-smi and qmassa. The wedge survives only as a conjunction (always-on + per-process +
  narrated + persistent + unprivileged), and conjunctions erode one release at a time —
  all-smi is explicitly assessed as one release from contesting the headline, with
  corporate backing and monthly cadence; netdata already owns retention + scrub-back.
- **The loudest, most quantified pain is in the punted segment.** Xid/ECC, fleet waste
  (Expanse's 59%), cluster hangs — datacenter and fleet demand, which the roadmap
  deliberately does not chase. The consumer/workstation signals (ollama confusion,
  dataloader stalls) are real but softer and harder to monetize into adoption.
- **And the production audit's own conclusion compounds it:** the headline claim currently
  outruns the binary. Launching on these demand signals before the §1.2 blockers land —
  with a recorder that double-narrates under its own recommended setup and goes silent on
  driver death — would spend the trust thesis exactly where the demand is most skeptical.

Net: the demand evidence is strong enough to justify the wedge and the sequencing, and not
strong enough to justify shipping before the recorder defends its own headline.

---

## 5. What this means for sequencing

The roadmap does not change: **v1 = Linux (NVIDIA/AMD/Intel), v1.5 = Windows NVIDIA,
v2 = macOS Apple Silicon device-level + iced GUI.** Control features, daemon/client split,
Prometheus exporter, cluster views, and macOS per-process stay punted. What the audit
adds is *ordering inside* those milestones, with evidence:

**v1 hardening, before any louder public claim** (these are the audit's blockers and the
trust-critical majors, not new features):

1. Instance lock + an event dedupe key — the observed two-writer duplicate-narration
   defect corrupts the artifact the product is named after.
2. A shipped systemd user unit + session-boundary events (recording started/stopped), so
   "always-on" is achievable with supported parts and a recorder-not-running hole is
   narrated. The daemon punt itself stands; a unit file is not a daemon/client split.
3. A device-lost event + staleness marking in the TUI (and a collector liveness flag) —
   driver death must not render as frozen plausible gauges.
4. CI (the GPU-free suite + NDJSON conformance + the three-OS `cargo test --no-run` gate
   `04-synthesis.md` §3 already promised) and a packaging baseline. An MSRV while at it —
   the rusqlite pin already proves toolchain sensitivity.
5. Fix `events.rs:709-714` (largest-holder requires `Some(mem)`) — reachable today on
   WSL2 and old-kernel AMD/Intel, prerequisite for both ports.
6. Fix backoff-vs-Intel (`device_is_idle` on util-less devices) — the polling-side-effects
   requirement is currently dead on every hybrid, including the dev machine.
7. Soften the README AMD/Intel-Arc cells to "implemented, awaiting hardware reports" until
   real-silicon runs exist — or obtain the runs. The flagship cross-vendor ✓ currently
   rests on code never run in anger.

**v1.5 (Windows NVIDIA)** is correctly scoped and cheap-but-not-free: the PDH "GPU Process
Memory" dual-source is the one substantive collector (it lights up per-process VRAM and
the three dark narrations); the rest is `%LOCALAPPDATA%` resolution, `cmd /C` dispatch, a
Windows CI job with a stub `nvml.dll` analog, and a release binary. Windows AMD/Intel stay
v2 per the ADLX/IGCL evidence in §2.2.

**v2 (macOS)** is gated on the post-WWDC26 re-verification (June 8–12 — re-check next
week) and needs design work the port itself doesn't cover: the throttle-proxy event tier,
the runtime-mutable memory budget, the VRAM→wired-limit relabeling under the NDJSON
compatibility promise, and the model-fit framing — the one unserved Mac angle, and a race
against macmon/mactop shipping persistence first.

The through-line: every platform expansion inherits the v1 hardening for free, and none of
it is wasted if a platform slips — the instance lock, session events, device-lost
narration, and holder fix are all platform-neutral. **The cheapest credibility move is
also the most honest one: make the headline true before making it louder.**
