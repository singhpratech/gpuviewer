# gpuviewer

**The GPU flight recorder.** A single static binary that doesn't just show you gauges —
it records your GPU's history and tells you the story: *which* process is using it, *what*
changed, and *why* it slowed down.

> `nvidia-smi` shows you a snapshot. Grafana needs four components and a weekend.
> Nothing in between can answer: **"What did my GPU do during last night's training run,
> and why did utilization drop at 02:14?"** — gpuviewer exists to answer exactly that.

## Status

🔬 **Research & design phase.** The market, vendor-API, and stack research (multi-agent web
research sweep, June 2026) lives in [`docs/research/`](docs/research/):

1. [Market landscape](docs/research/01-market-landscape.md) — 23-tool competitive matrix, 8 validated gaps
2. [Vendor APIs](docs/research/02-vendor-apis.md) — what telemetry is *actually* obtainable per vendor/OS, incl. the per-process feasibility matrix
3. [Stack evaluation](docs/research/03-stack-evaluation.md) — framework verdicts + architecture patterns
4. [Adversarial synthesis](docs/research/04-synthesis.md) — gap analysis, feasibility verdicts, committed stack, story-engine design, ranked risks

## Why another GPU monitor?

Validated gaps no shipping tool fills:

- **No persistent history without Grafana.** nvtop/btop graphs die with the terminal; GPU Hot
  (1.5k★ overnight) proved the demand then shipped no persistence.
- **Nobody explains state changes.** NVML exposes throttle *reasons* as a bitmask no tool
  decodes into "thermal throttling began 02:14, clocks 1980→1410 MHz". VRAM-pressure trends,
  process lifecycle, idle-gap detection — the data exists, the narration doesn't.
- **No tool covers NVIDIA + AMD + Intel + Apple across Linux + Windows + macOS.** nvtop has no
  Windows (PR rejected), btop has no per-process GPU, bottom has no macOS GPU.
- **Windows terminal GPU monitoring is NVIDIA-only**, while Task Manager shows ~3% for a maxed
  CUDA workload.

## The plan

**v1 — Linux flight recorder** (NVIDIA · AMD · Intel, one static binary):
ratatui TUI + `--json` streaming; ring-buffer + SQLite history; replayable timeline with
narrated events — throttle onset with cause, VRAM climb toward OOM with ETA, process
attach/exit, training idle gaps. Facts asserted plainly; inferences labeled "likely" and
expandable to raw evidence.

**v1.5 — Windows NVIDIA** (NVML + PDH dual-source — honest per-process where Task Manager misleads)
**v2 — macOS Apple Silicon** (sudoless IOReport/AGX device telemetry + the unified-memory
"will this model fit" story for MLX/local-LLM users) **+ iced GUI** from the same core
**v2+ — Windows AMD/Intel** (ADLX/IGCL + PDH), Prometheus exporter, multi-host view

## Architecture (decided)

```
gpuviewer-core      trait GpuBackend (nvtop's vtable, translated to Rust)
                    ├─ nvidia: nvml-wrapper (runtime dlopen, never hard-linked)
                    ├─ amd:    sysfs/hwmon/gpu_metrics/fdinfo (zero library deps)
                    ├─ intel:  fdinfo (i915 + xe dialects) + sysfs
                    └─ mock:   scripted backend for CI (no GPU required)
gpuviewer-history   RAM rings (live window) → 10s/1m SQLite rollups + event log
gpuviewer-tui       ratatui 0.30 — timeline, replay, event feed   (+ --json mode)
```

Built in Rust. No vendor SDK is ever hard-linked — missing drivers degrade gracefully,
`NOT_SUPPORTED` is a normal per-metric outcome, and the tool never out-consumes what it
monitors (adaptive polling; "low-impact mode").
