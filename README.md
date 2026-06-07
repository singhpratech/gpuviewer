# gpuviewer — the GPU flight recorder

> **It was already recording. Scroll back to 02:14 — it'll tell you why.**

gpuviewer is a Linux GPU monitor (NVIDIA · AMD · Intel) that records persistent, per-process
history and a narrated event log **just by being open** — no daemon to install, no recording
you had to remember to start. When last night's training run stalled, you scroll back through
the timeline to the moment it happened and read the story: the throttle onset with clock
deltas, the VRAM climb with an ETA, the process that exited and what it freed. Facts are
asserted plainly; inferences are always labeled **"likely"** and expand to the raw evidence
behind them. One unprivileged binary.

## Quick start

```sh
cargo run --release -- --mock        # the TUI on simulated GPUs — no hardware needed
```

(`--mock` is also the automatic fallback when no GPU is found; mock data is always labeled
"(mock data)" and records to a separate database, never your real history.)

**The demo.** Seeds 8 hours of simulated history — throttle episodes, training idle gaps, a
VRAM climb toward the cap, an `ollama` attach/exit cycle — into its own database, then opens
the TUI **already scrolled back to the last throttle onset**. The first thing you see is the
answer to "why did it slow down", not a live gauge:

```sh
cargo run --release -- demo
```

**The morning-after digest.** Plain text, no ANSI, paste-able into Slack or a bug report:

```sh
$ gpuviewer report --since 22:00
gpuviewer report — 2026-06-06 22:00 .. 2026-06-07 08:41 (23 events: 17 facts, 6 inferences)

GPU0 (GeForce RTX 4090): util avg 81% / max 99%, temp max 88°C, mem max 23.3 GiB, throttle buckets 14
GPU1 (Radeon RX 7900 XTX): util avg 12% / max 87%, temp max 71°C, mem max 12.4 GiB, throttle buckets 0

23:41:07  INFO  [fact]    ollama (pid 7777) attached to GPU1, using 11.3 GiB
02:14:31  WARN  [fact]    GPU0 began throttling (thermal) — clocks 2520→1815 MHz
02:14:48  INFO  [likely]  GPU0 sat idle 17s while python (pid 4521) stayed attached — likely a dataloader or checkpoint stall
02:16:02  INFO  [fact]    GPU0 stopped throttling after 1m 31s
03:02:11  WARN  [likely]  GPU0 VRAM 92% and climbing ~270 MiB/min — likely full in ~8 min (largest holder: python pid 4521)
06:58:40  INFO  [fact]    python (pid 4521) left GPU0, freeing 21.3 GiB
```

**Scripting and agents.** One NDJSON frame per tick plus that tick's narrated events, every
metric nullable, versioned and conformance-tested:

```sh
gpuviewer --json --once --mock       # one frame + its events to stdout, then exit 0
```

The stream contract — frames *and* events in one timestamped stream, JSON Schema, written
compatibility promise — is [`docs/spec/ndjson-v1.md`](docs/spec/ndjson-v1.md).

## How it compares

The honest version: several good tools own one half of the flight-recorder sentence. The
combination — always-on by virtue of normal use, per-process, scroll-back replay, narrated
causes under a facts-vs-"likely" contract, in one unprivileged binary — is the part that's
ours.

| | gpuviewer | nvtop | all-smi | qmassa | LACT | gpud | netdata |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| Always-on recording, no setup/daemon | ✓ | — | — ¹ | — ¹ | — | ◐ ⁴ | ◐ ³ |
| Scroll-back replay in-tool | ✓ | — | ✓ ¹ | ✓ ¹ | — | — | ✓ ³ |
| Narrated causes, facts vs "likely" | ✓ | — | — | — | ◐ ² | ◐ ⁴ | — |
| Per-process attribution (NVIDIA + AMD + Intel incl. xe) | ✓ | ✓ | ◐ ⁵ | ◐ ⁵ | — | — | — |
| Single unprivileged binary | ✓ | ✓ | ✓ | — ⁶ | — | ◐ ⁴ | — |

¹ all-smi (`record` → `view --replay`) and qmassa (`-t` → `replay`) both ship real TUI
replay — **of recordings you explicitly started beforehand**. Great when you knew the
incident was coming; no help for the one you didn't predict.

² LACT decodes throttle causes on all three vendors and shades them on its charts — in a
live GUI with an in-memory window (ephemeral; no persisted event log, no replay).

³ netdata's history is the real thing (per-second, ~14-day default retention, scrub-back
dashboards) — device-level, as a daemon with per-collector GPU configuration, no per-process
GPU, and threshold alerts rather than causes.

⁴ gpud ships genuine fact-tier events (Xid, kmsg, hw-slowdown) from a single binary — built
as a fleet-health daemon, NVIDIA-focused, no scroll-back timeline, no narration.

⁵ all-smi: multi-vendor including Intel i915 + xe, but no AMD. qmassa: AMD + Intel, no
NVIDIA.

⁶ qmassa's deeper telemetry is sudo-gated.

If one of those fits your problem better, use it — LACT for fan/OC control, netdata for
fleet dashboards, gpud for cluster health verdicts. (Cells reflect our reading of each tool
as of June 2026; corrections welcome.)

## What it tells you

Example narrations, in the exact shape the event engine emits them. The tag is the
confidence tier: facts are observed state transitions; inferences always say "likely" in
the sentence itself and carry the raw numbers in an auditable evidence field.

- `[fact]` GPU0 began throttling (thermal) — clocks 2520→1815 MHz
- `[fact]` GPU0 stopped throttling after 1m 31s
- `[likely]` GPU0 VRAM 92% and climbing ~270 MiB/min — likely full in ~8 min (largest holder: python pid 4521)
- `[likely]` GPU0 sat idle 47s while python (pid 4521) stayed attached — likely a dataloader or checkpoint stall
- `[likely]` GPU0: python (pid 4521) likely hung — held 20.1 GiB for 10m 12s with zero GPU activity, process still alive
- `[likely]` ollama (pid 7777) loaded 11.3 GiB but GPU1 is ~idle while its CPU runs hot — likely partial CPU offload (model may not fit in VRAM)
- `[fact]` collection stalled 4.2s — a backend probe blocked; the data gap is recorded, last good frame at 02:14:31
- `[fact]` python (pid 4521) left GPU0, freeing 21.3 GiB

Inference thresholds are deliberately conservative — a hang is only narrated after ten
unbroken minutes of held VRAM with zero engine activity, and any break in the premise drops
the claim silently. A confidently-wrong narration is worse than no narration.

Events can also drive your own plumbing: `--on-event 'CMD'` runs a command for every emitted
event with `GPV_EVENT_*` variables in the environment (rate-capped), e.g.
`--on-event 'curl -s -d "$GPV_EVENT_TITLE" ntfy.sh/mytopic'`.

## The honesty contract

- **Facts vs inferences.** Every event carries `confidence: fact | likely`. Facts (a
  throttle bit set, a process gone from the list) are asserted plainly. Inferences (stall,
  hang, spillover, OOM ETA) always read as hedged and carry an `evidence` field with the raw
  numbers — visible in the TUI, the digest, and the JSON stream.
- **"Utilization" is duty-cycle, not saturation.** It measures the fraction of time at least
  one kernel was resident — a GPU at "100%" may be nowhere near compute- or bandwidth-bound.
  It is never presented as capacity used.
- **Absence is a normal outcome, not an error.** Every metric is nullable. Driver
  `NOT_SUPPORTED`, missing sysfs/hwmon files, MIG mode, and privilege walls render as
  "unavailable" — never zero, never a crash. Where the reason is knowable it is explained
  in-UI: on WSL2, per-process GPU attribution is unavailable *at the driver level* and the
  process pane says so; without root/`CAP_SYS_PTRACE`, fdinfo can only attribute your own
  processes and the pane shows a "your processes only" hint rather than pretending the list
  is complete.
- **The recorder reports on itself.** A blocked driver probe or missed tick becomes a
  recorded `[fact]` event ("collection stalled … the data gap is recorded") — a hole in the
  recording never masquerades as the GPU having gone quiet. A quarantined-and-recreated
  history file is narrated as a history reset, for the same reason.
- **Polling must not perturb what it measures.** Idle GPUs are polled on a stretched cadence
  (up to 5× the interval) so monitoring doesn't keep them awake or break GFXOFF; the
  effective cadence is shown in the footer rather than hidden; `--no-backoff` opts out.
- **Mock data is always labeled.** The footer says "(mock data)" exactly when the data is
  mock — including replays of mock recordings — and mock/demo runs record to separate
  database files, never your real history.
- **No vendor SDK is hard-linked, no vendor CLI is exec'd.** NVML is loaded at runtime; AMD
  and Intel are read straight from the kernel's sysfs/fdinfo interfaces.

## Status

**Working pre-release (v0.1.0) — Linux.** Build from source: `cargo build --release`
(binary at `target/release/gpuviewer`); no packaged binaries yet.

Shipped — in the binary today:

- **Backends:** NVIDIA (NVML, runtime-loaded — never hard-linked), AMD (sysfs/hwmon +
  versioned `gpu_metrics` throttle decoders v1.1–v3.0 + fdinfo), Intel (fdinfo in both the
  i915 and xe dialects + sysfs), deterministic mock fallback. Devices deduped across
  backends by PCI address.
- **Always-on recording:** SQLite (WAL) 10s/1m rollups + an append-only event log, with
  per-process rollups (memory, util, CPU%, container identity when knowable), retention
  sweeps, and corrupt-database quarantine. Raw 1 Hz samples never touch disk — the live
  window rides in RAM rings.
- **Scroll-back replay:** `r` from the live view, or Enter on any event in the story feed to
  jump straight to it; scrub by 10s/5m; works on your real history, the demo, and exported
  recordings.
- **Narrated events:** throttle onset/recovery with clock deltas, process attach/exit with
  freed VRAM, VRAM-pressure ETA, idle gap, suspected hang, CPU spillover, collector
  stall/slow-probe self-reports, history reset.
- **Subcommands and sinks:** `report` (plain-text digest), `demo` (pre-seeded incident),
  `export`/`view` (shareable `.gpvr` incident files that replay anywhere, no GPU required),
  `--json` (NDJSON contract v1 with JSON Schema and a conformance test that runs the built
  binary), `--on-event` command sink, adaptive idle backoff.
- **Tests:** the full suite passes on machines with no GPU — everything runs against the
  mock backend and committed sysfs/fdinfo fixture trees.

In progress / not yet:

- Packaged releases (cargo-only for now).
- Real-hardware soak across the driver matrix — a manual pre-release checklist by design;
  CI stays GPU-free.
- Windows NVIDIA (v1.5) and macOS Apple Silicon + iced GUI (v2) — see roadmap.

## Architecture

```
gpuviewer-core      trait GpuBackend (nvtop's vtable, translated to Rust)
                    ├─ nvidia: nvml-wrapper (runtime dlopen, never hard-linked)
                    ├─ amd:    sysfs/hwmon/gpu_metrics(v1.1–v3.0)/fdinfo (zero library deps)
                    ├─ intel:  fdinfo (i915 + xe dialects) + sysfs
                    └─ mock:   deterministic simulation (CI + demo; no GPU required)
gpuviewer-history   RAM rings (live window) → 10s/1m SQLite rollups + event log  [shipped]
gpuviewer-tui       ratatui 0.30 — live view, scroll-back replay, story feed
                    + report · demo · export (.gpvr) · view   (+ --json mode)
```

Built in Rust. Missing drivers degrade gracefully, `NOT_SUPPORTED` is a normal per-metric
outcome, and the tool never out-consumes what it monitors (adaptive polling; low-power
cadence surfaced in the footer).

### Keybinds

| Live view | |
|---|---|
| `q` / `Esc` | quit |
| `←` `→` / `Tab` / `Shift-Tab` | switch device |
| `p` | pause/resume collection |
| `↑` `↓` | select an event in the story feed |
| `Enter` | scroll back to the selected event |
| `r` | enter replay at the newest recorded moment |

| Replay view | |
|---|---|
| `q` | quit |
| `Esc` / `r` | back to live (inert in `view` — a file has no live mode behind it) |
| `←` `→` | scrub 10s |
| `PgUp` / `PgDn` | scrub 5m |
| `Home` | jump to the oldest recorded moment |
| `↑` `↓` then `Enter` | jump to the selected event |

### Retention defaults

| | granularity | kept for |
|---|---|---|
| recent past | 10s rollups | 48 hours |
| long tail | 1m rollups | 30 days |
| event log | every event | 30 days |

History lives at `$XDG_DATA_HOME/gpuviewer/history.db`
(`~/.local/share/gpuviewer/history.db`); `--db` overrides it, `--no-persist` disables
recording (which the replay view and `report` need). `--mock` records to `history-mock.db`
and `demo` to `history-demo.db` — your real history is never polluted by simulations.

### Roadmap

- **v1.5 — Windows NVIDIA:** NVML + PDH dual-source — an honest per-process number where
  Task Manager misleads.
- **v2 — macOS Apple Silicon:** device-level telemetry only (per-process GPU is
  OS-prohibited, and we will say so in-UI rather than fake it) + an iced GUI from the same
  core.
- **v2+:** Windows AMD/Intel, Prometheus exporter, multi-host views.

Deliberately not chasing: fan/OC control (LACT owns it), a daemon/client split ("always-on"
means by virtue of normal use — for boot-time recording, run `gpuviewer --json` under your
own systemd user unit), cluster views, and eBPF causal tracing (we hand off to profilers:
"idle gap at 02:14:31 — if recurring, capture a trace").

## License

MIT.

---

The product/architecture decision record and the June-2026 market, vendor-API, and
competitive evidence live in [`docs/research/`](docs/research/) — start with
`04-synthesis.md`; the comparison table above is sourced from `05-competitive-deep-dive.md`.
