# Competitive Deep-Dive — Decision Document

**Date: 2026-06-06.** Builds on `01-market-landscape.md` (24-tool matrix, June 2026) and
`04-synthesis.md` (wedge + architecture decision record). This document goes deeper and more
current: every claim below was re-verified against live GitHub/web/binary evidence on
2026-06-06 across six research clusters (NVIDIA TUIs, multi-vendor monitors, history/web/fleet
tools, ML-training observability, Windows/macOS expansion turf, and a fresh-entrant sweep of
~1,100 repos created since 2026-01-01). Where a recommendation touches a settled decision in
`CLAUDE.md`, it is flagged explicitly.

---

## 1. Executive answer

**gpuviewer stands out because nobody else combines the four halves of the flight-recorder
sentence, and everyone who has one half lacks the others.** As of 2026-06-06:

- **History-without-story** exists (netdata: 14-day per-second retention, scrub-back, May-2026
  DCGM alerts; Beszel: minute-granularity device-level fleet history) — but device-level,
  daemon-heavy, and it never says *who* or *why*.
- **Replay-without-narration** exists (all-smi v0.21.0, 2026-05-26: record→NDJSON.zst +
  TUI replay with seek; qmassa: record/replay/SVG-plot on AMD/Intel since Jan 2025) — but
  opt-in: useless for the incident you didn't predict, and it explains nothing.
- **Events-without-timeline** exists (leptonai/gpud: single-binary Xid/kmsg/hw-slowdown events,
  weekly releases; NVSentinel; Meta GCM) — but fleet-health JSON verdicts for operators,
  NVIDIA-only depth, no scroll-back, no human narration.
- **Narration-without-recording** exists only above us (CoreWeave×W&B Mission Control —
  cloud-locked; Meta Zoomer — internal; Chamber — enterprise SaaS) or as unshipped marketing
  (Ingero: GitHub org 404s).

The intersection — **always-on by virtue of normal use, per-process, sub-minute, replayable,
and narrated in human language with the facts/"likely" honesty contract, in one unprivileged
static binary** — has zero shipping competitors. The strongest single proof: nvitop #217
(closed-wontfix 2026-05-19) is a user asking the cluster's best tool, verbatim, "did util drop
off 5 minutes ago when the dataloader switched shards?" and being told to go run Prometheus.

**What to do next, in one line:** land SQLite persistence + the event-anchored scroll-back UI
within weeks (all-smi is one release away from contesting the headline), while claiming the
"GPU flight recorder" category name and publishing the frames+events NDJSON contract now —
then launch with a replay demo, never a gauge screenshot.

---

## 2. Current landscape (June-2026-verified): tool × history × events × threat

All versions/dates verified 2026-06-06. "Threat" = threat to the wedge (persistent history +
narrated replayable events), not general competitiveness.

### Terminal GPU monitors (NVIDIA-centric)

| Tool (state) | History | Events/narration | Threat |
|---|---|---|---|
| **nvtop** 3.3.2 (2026-02-08; 10.7k★, pushed 2026-05-06) | None. On-screen ring dies with terminal. New `-s`/`-l` JSON has **zero timestamp fields** (verified in `src/interface.c print_snapshot`) and broke schema twice (#438, #444) | None. Throttle/Xid/ECC never surfaced anywhere in codebase | **Low** — moving orthogonally (NPU backends); declined daemon/export for years (#40, #64, #65, #234) |
| **nvitop** v1.7.0 (2026-05-16; 6.9k★) | TUI graphs hard-fixed 1s/terminal-width; #217 closed-wontfix 2026-05-19 ("run Prometheus"). NEW examples/monitor-web (2026-05-25): 24h@1s **in-memory** Plotly buffer, processes disabled, no persistence | None (monitor-web status badges are about the collector, not the GPU) | **Medium** — fastest mover; watch monitor-web graduating to a supported `--web` with SQLite |
| **gpustat** v1.1.1 (2023-08; dormant) | None | None — #135 "Display thermal throttles" open since 2022-10, unanswered; broken process table on driver 535+ (#161) | **None** — its open-issue list is our demand archive |
| **nviwatch** v0.2.1 (dormant since 2025-08) | InfluxDB streaming only (self-hosted DB required) | None | **Low** |
| **nvidia-smi daemon/replay** (driver 580.159 verified locally) | NVIDIA's own abandoned flight recorder: root-only, device-only, compressed /var/log/nvstats, "experimental" since ~2015 | None — throttle reasons stay a `-q -d PERFORMANCE` bitmask; dmon `-s v` = cumulative counters | **None** — the category-validating fossil |

### Multi-vendor system monitors

| Tool | History | Events/narration | Threat |
|---|---|---|---|
| **btop** v1.4.7 (2026-05-01; 32.7k★) | None; prebuilt binaries ship **without GPU support at all** (#1551) | None | **Low** — watch PR #1552 (per-process NVML) for per-process pitch erosion only |
| **bottom** v0.12.3 (2026-01-01; 13.4k★) | In-memory 10-min retention only; #1389 (open) literally asks for record + "feed it back in and view it" | None (#1046 event-feed FR open 3 years) | **None** — #1291 (NVML polling keeps GPU awake, open since 2023) validates our adaptive cadence |
| **Mission Center** v1.1.0 (2025-11-11; quiet) | ~60s session graphs | None | **None** — its xe-era bugs (#462 B580 "Unknown", #474 Lunar Lake) prove the Intel hole |
| **qmassa** v2.1.0 + qmmd exporter (2026-05-16; Intel dev) | **Only shipping TUI replay in the TUI clusters**: opt-in `-t` record → `replay` → `plot` SVG. No always-on, no retention mgmt, sudo-gated | Embryonic: live PL1 throttle annotation (i915/xe only). No log, no narration | **Medium** — closest mechanical ancestor on AMD/Intel; no NVIDIA, no narration ambition in 2+ years |
| **amdgpu_top** v0.11.5 (2026-05-18) | None persistent (JSON stream, dumps) | Throttle **residency %** as live stat (v0.11.1) — not events | **Low** — also a resource (libamdgpu_top, gpu_metrics fixtures) |
| **intel_gpu_top** (igt 2.4) | None | None | **None** — it *is* the vacuum: segfaults/"Failed to detect engines!" on xe hardware (frigate #21794, Arch bbs 295732/304567) |
| **glances** v4.5.4 (2026-04-19; 32.8k★) | Session-only; persistence = 30 external exporters | Threshold WARNING/CRITICAL log, session-only, no GPU defaults, no causes | **Low** |
| **netdata** v2.10.3 (2026-04-27; 79k★) | **The real thing**: tiered dbengine, per-second ~14 days default, scrub-back UI. NEW DCGM collector 2026-05-04 (127 fields) | Alert-shaped: default XID/row-remap/power+thermal-throttle alerts on the DCGM path only; ML anomaly bits. No causes, no per-process GPU, no narrative | **High** — has both hard substrate pieces (retention + scrub UI), actively marketing GPU-for-AI |
| **LACT** v0.9.0 (2026-04-25) | GUI session charts, 10–3600s in-memory cap | **Decodes throttle causes on all three vendors** with timestamped chart shading (source-verified) — GUI-only, ephemeral, no events API, no narration | **Low-Medium** — the literal "nobody decodes bitmasks" refuter; control-tool DNA |

### History / web / fleet tools

| Tool | History | Events/narration | Threat |
|---|---|---|---|
| **all-smi** v0.22.0 (2026-05-27; Lablup-backed Rust; 170★) | **Opt-in** `record` → zstd-NDJSON → `view --replay` (seek, 0.25–8x) shipped v0.21.0 (2026-05-26). Schema-versioned frames (`schema: 1`), SSE, webhooks. No always-on, no SQLite, no rollups | None — threshold toasts + in-memory ring of 50 transitions + webhook. No throttle decode, no event log | **High** — closest trajectory match; one release from contesting the headline |
| **GPU Hot** v1.9.0 (2026-05-28; 1.5k★) | None server-side (code-verified: one previous sample per GPU; charts live in the browser tab). #32 persistence request closed unimplemented; v1.9.0 "history" = 60s→120s RAM | None; alerts PR #40 unmerged | **Medium** — owns the validated homelab/browser audience; 9 releases without persistence = wedge still open |
| **gpustat-web** v0.3.0 (Nov 2023; alpha) | None | None | **None** |
| **DCGM 4.5.3 + dcgm-exporter 4.8.2** (2026-05-07) + Grafana 12239 | Real, after assembling 4 components; no host PIDs (#521 closed unimplemented); PROF silently degrades on GeForce (#398) | Event-shaped metrics (XID counters, 4.8.2 "health incidents", bind/unbind beta) — series, never sentences | **Low** — the heavyweight ceiling we position under |
| **nvitop-exporter** v1.7.0 + dashboard 22589 | Prometheus-delegated; only exporter with honest host-PID series | None | **Low** — feature-parity benchmark, not wedge competitor |
| **beszel** (active; hub+agent) | Persistent SQLite-backed fleet history, GPU on by default once agent installed — minute-granularity, device-level | None (thresholds only) | **Low** — owns fleet-lite persistence at checkbox depth |
| **leptonai/gpud** v0.11.8 (2026-06-04; 482★, weekly releases, ex-hyperscaler team) | Internal state tracking + SQLite for health eval; not a user timeline | **Substantial fact-tier**: Xid, kmsg, hw-slowdown (HW bits only — ignores SW power-cap/SW thermal, the dominant 4090 causes), DCGM faults as machine-readable events. No narration, no replay, NVIDIA-only, cloud-tether optional | **High** — missing from our prior matrix; one timeline-UI away on NVIDIA |
| **NVIDIA NVSentinel** v1.8.0 (2026-06-01) | MongoDB remediation pipeline (K8s) | Taxonomic fault→action; #890 (P1, open) asks it for throttle-onset detection | **Low** — three weight classes up; normalizes the vocabulary |
| **Meta GCM** (open-sourced 2025-12-18; 225★) | OTLP→Prometheus; Slurm prolog/epilog | Facts-only, job-attributed (XID/NVLink/thermal); no prose, no single-node story | **Medium** — a Slurm-free repackaging would land on our event taxonomy |

### ML-training observability

| Tool | History | Events/narration | Threat |
|---|---|---|---|
| **W&B** ~0.27.2 + CoreWeave Mission Control | Per-run cloud series, 15s, own process tree only, dead between runs; silent-drop trail (#8137, #8498 open, #10581) | Native: none (docs confirm). **Mission Control: infra events + straggler detection ON training plots — our thesis, shipped, CoreWeave-cloud-locked** | **High** — kill shot would be an on-prem W&B infra agent |
| **MLflow** 3.13.0 (2026-05-29) | Per-run, opt-in, 10s | None; team pivoted to GenAI tracing | **Low** — integration target |
| **PyTorch Kineto + HTA** v0.6.1 (2026-04-21) | Trace windows only; nothing retroactive | **Best open "why" engine** (idle-time-by-cause) — inside deliberately captured windows only | **Low** — hand-off partner: we find the moments worth windowing |
| **dynolog** (Anyscale bundles in Ray ≥2.47) | No replayable history | None; forward-only on-demand tracing, pre-arranged env var | **Low** — forces our language to be "retroactive," not just "no code changes" |
| **Meta Zoomer** (internal; blog 2025-11-21) | Internal fleet flight-recorder | Most advanced anywhere (auto-classify, auto-fix diffs) — unreleased | **Low** product / real blueprint risk |
| **Ingero** (claims v0.19.0) | CLAIMED SQLite store — unverifiable; github.com/ingero-io 404s; only repo 0★, one day of commits | CLAIMED our exact pitch + eBPF causality | **Medium** as positioning squat, low as product |
| **Utilyze** (Systalyze, 2026-04-27; 128-pt HN) | None — explicitly live-only | Compute- vs memory-bound classification, live; explanation reserved for paid platform | **Low** — owns "util is a lie" on datacenter SKUs; we are the honest consumer-GPU complement |

### Fresh 2026 entrants (sweep of repos created since 2026-01-01)

| Tool | History | Events | Threat |
|---|---|---|---|
| **hw-smi** (ProjectPhysX; 276★, v1.5 2026-05-22) | None | None | Low — cross-vendor breadth + famous author; links vendor SDKs we avoid |
| **gpuwatch** (44★, quiet since March) | None | Crash/OOM/overheat push alerts to 20 channels — events die in the notification | Low — validates event-sink demand; a `--on-event` flag subsumes it |
| **GPUFlight/gpufl** (6★, pushed 2026-06-05) | Session NDJSON logs of *your instrumented app* (CUPTI) | None | **Medium — brand collision**: squats "GPU flight" name/domain/PyPI |
| **gpu-histop** (0★, one-day), **gptop** (evilsocket, dormant), **dofek** (Tauri — the stack we rejected, tauri#9394), **llmtop**, **TrainWatch**, **zml-smi**, **NVSonar**, **Chamber** (YC W26 enterprise SaaS) | Session-only or none (TrainWatch: persisted alert *list*) | None to partial | Low individually — collectively prove the "history" and "knows-the-workload" framings are spreading |
| **nv-monitor** v1.13.0 (2026-05-07; 286★) | None | None | Low — <80KB static NVML binary incl. aarch64/GB10; kills our "DGX Spark has no TUI" line |

### Windows + macOS (v1.5/v2 turf, abbreviated)

Task Manager: HAGS removes the CUDA graph entirely; Microsoft's answer is "wait" (Q&A 3903903;
unchanged through 25H2 — 2026 Task Manager work went to NPU columns). GPU-Z 2.69.0's PerfCap
Reason proves Windows throttle-cause decoding is feasible and familiar — logged to CSV, never
evented. HWiNFO 8.48 monetizes long-duration monitoring via the 12h free-tier shared-memory
cutoff. macOS: Stats v2.12.16 (39.4k★) locks GPU charts to 1 minute and closed the day-scale
history request (#2663) as duplicate; macmon v0.7.2 and mactop v2.1.3 both punted history to
new Prometheus endpoints; asitop dead since 2024-04; gpuer (simonw, 2026-03-27) validated the
memory-story demand and stalled. The live model-fit/wired-limit story remains 100% unshipped
while static calculators exploded (llmfit ~11.7k★ since Feb 2026, FitLLM.run, etc.).
**WWDC26 is June 8–12 — re-verify macOS per-process APIs after the keynote before any v2 scope
commitment.**

---

## 3. Verified gaps: real vs partially-served vs refuted

Adversarial verification was run against every gap we believed in. Verdict legend:
**REAL** = unserved residual confirmed; **PARTIAL** = the literal claim is dead but a sharper
residual survives; **KILLED** = stop saying it.

### 3.1 The kill list (claims we must stop making, with the evidence that killed them)

| Dead claim | Killed by |
|---|---|
| "Nobody decodes throttle bitmasks" | **LACT v0.9.0** decodes NVIDIA/AMD/Intel causes with timestamped GUI throttle-interval shading (source-verified: `nvidia.rs:825`, `amd.rs:906`, `intel.rs:439`); **gpud** ships timestamped `hw_slowdown` events; **amdgpu_top** v0.11.1+ decodes live; **GPU-Z PerfCap** has done it on Windows for a decade |
| "Every 2025–26 entrant launched NVIDIA-first/only" | **all-smi** launched multi-vendor (9 accelerator families incl. Intel i915+xe client GPUs); hw-smi is cross-vendor on two OSes |
| "DGX Spark/GB10 owners have zero modern TUI" | **nv-monitor** v1.13.0 (<80KB static, aarch64, GB10/GB200/Jetson), sparkview, Beszel agent — stale as of May 2026 |
| "No-instrumentation monitoring is our unique differentiator" | **zymtrace** profiles 24/7 zero-code (eBPF+CUPTI); **Ingero** attaches to running processes; **dynolog** is bundled in Ray ≥2.47. Durable language: **retroactive + always-on + narrated + local**, not "no code changes" |
| "Nothing maps GPU PIDs to container names" | **gtop** (1★, Python) maps NVIDIA PIDs→Docker names; dcgm-exporter does pod labels in K8s. Residual: nobody *narrates* container-attributed fallback |
| "Exec-free + unprivileged + static binary is unserved" | **nv-monitor** serves it NVIDIA-only; **all-smi** serves it cross-vendor via musl (minus AMD). Residual: the *combination* with adaptive cadence + timeout-as-event + published self-impact |
| "No honest per-process CUDA number on Windows" | **bottom**'s default NVML `process_utilization_stats` column + `nvidia-smi pmon` per-PID sm% are HAGS-immune. Residual: nobody pairs it with built-in scroll-back history |
| "Nobody ships replay" | **qmassa** (since v0.5.0, 2025-01) and **all-smi** v0.21.0 (2026-05-26) both ship record→TUI-replay. Residual: both opt-in, both narration-free |

### 3.2 Real gaps (the residuals that survived adversarial search)

1. **Always-on-by-virtue-of-normal-use recording + scroll-back** (high confidence). No tool an
   engineer would already be running records persistent, replayable, per-process, sub-minute
   telemetry by default in an unprivileged single binary. Beszel = minute-granularity
   device-level behind hub+agent; netdata GPU collectors = opt-in config edits ("doesn't
   support auto-detection"); atopgpud = root daemon, 10-min atop cadence; all-smi/qmassa/
   nvidia-smi-daemon = record-before-the-incident. **This is the wedge, confirmed unowned.**
2. **Persistent, narrated, cross-vendor throttle/health events** (high). Nobody pairs
   onset/recovery, decodes the *full* cause set (gpud skips SW power-cap/SW thermal — the
   dominant causes on a power-limited 4090), attaches clock/temp deltas
   ("02:14:31 thermal throttle, SM 1980→1410 MHz, recovered 02:16:02"), persists it, and
   works headless. LACT = GUI shading ≤1h in-memory; amdgpu_top = 30-second AMD log.
3. **Derived process-lifecycle events** (high). "python PID 4242 exited 02:14, freeing
   21.3 GB" as a first-class persisted record exists nowhere; everything after-the-fact is
   raw-sample forensics (atop replay-diffing, PromQL over pid-labeled series), all NVIDIA-only.
   **Per-process GPU history for AMD/Intel is served by literally nothing.**
4. **External zero-integration stall/hang detection + narration at 1Hz** (high). The
   conjunction — detect without being asked, narrate in plain language, sub-minute, local
   persistent timeline — is unserved. In-process tools narrate (TraceML, NVRx, NCCL watchdog);
   external observers detect at minutes-scale inside managed platforms (Mission Control 2-min
   heartbeat, Datadog 10s+/zombie monitors); attach-on-demand diagnosers (Ingero, zymtrace)
   explain after a human noticed.
5. **Runtime LLM VRAM narration** (high). ETA-to-OOM from live slope: nowhere. Partial-offload/
   CPU-spillover as an event: nowhere — ollama #14258 (open, "single most common source of
   user confusion," 500+ related issues) proposes upgrading a debug log line. Live macOS
   wired-limit headroom: nowhere. Fit-*checking* is served only at load time (LM Studio
   guardrails, llama.cpp default `--fit`, GPUStack) and by static calculators.
6. **Windows: HAGS-immune per-process compute + built-in scroll-back history** (high; v1.5
   turf). The honest number exists live (pmon, bottom); every tool with retained history is
   device-level (HWiNFO 12h-capped, GPU-Z CSV, .hml) or uses the HAGS-broken WDDM counters
   (uberAgent/ControlUp). Bonus: honest NVML+D3DKMT dual-sourcing of per-process VRAM is
   unshipped by anyone.
7. **AMD/Intel (both i915 and xe dialects): always-on history + decoded narrated events**
   (high). all-smi and qmassa now pair per-process fdinfo with opt-in recording — and derive
   zero events (verified: no throttle decoding anywhere in all-smi source; qmassa = a PL1
   legend flag). The xe driver hole (intel_gpu_top broken through igt 2.4; btop #1407 open;
   netdata inherits via wrapper) makes our shipped dual-dialect support a displacement story.
8. **One stable NDJSON stream carrying frames AND narrated events with a compatibility
   promise** (high). all-smi has `schema: 1` frames + SSE + webhooks but alerts never enter
   the stream and no compat promise exists; nvtop's JSON has no timestamps and broke twice
   (#438/#444). **Strategic warning verified: all-smi is ~one release from the frames half.**
   Our defensible residual = event semantics (facts vs "likely" + evidence) unified in-stream
   + the stability promise.
9. **The recorder that doesn't perturb or lie about itself** (high). No Linux monitor does
   idle-aware backoff (bottom #1291 open since 2023; the only sleep-aware poller is Windows
   closed-source HWiNFO), surfaces collection timeouts as recorded events (btop #1612 NVML
   ioctl wedge; Zabbix's timeout yields a server-side item state), or publishes its own
   measured observer cost.
10. **In-UI absence explanation + cross-layer fallback narration** (medium). nvtop still
    crashes on WSL2 N/A (#432 open against latest release); nvitop renders bare "N/A";
    all-smi `doctor` is a disconnected one-shot audit. Nobody composes process-attach +
    engine-idle + container identity + device-node visibility into
    "docker:jellyfin ffmpeg attached but video engine idle — likely missing /dev/dri."

---

## 4. What users are begging for (top pains, with sources)

Ranked by frequency × fit. These are demands, in users' words, declined by incumbents.

1. **"Record it so I can look later" — declined everywhere.** nvtop #65/#64/#234; nvitop #20,
   #167 ("a bit heavy to have prometheus and grafana… is there a lightweight solution?"),
   #217 closed-wontfix 2026-05-19 with our pitch verbatim; bottom #1389 (open, asks for NDJSON
   *and* "feed it back in and view it" — both already shipped in gpuviewer); gpu-hot #32
   (declined); Stats #2663 (closed-duplicate). Canonical scale: unix.SE "GPU usage monitoring
   (CUDA)" = 1,143,150 views; SO 8223811 = 503k views — accepted answers are still CSV hacks.
2. **"Tell me what happened, not a bitmask."** gpustat #135 (throttle display, open since
   2022-10); bottom #1046 (events feed, open 3 years); LACT #307 (raw throttle bits confuse
   users at idle); NVSentinel #890 (P1 — NVIDIA's own fleet tool asked for throttle-onset
   detection); dcgm-exporter #348 (alerting FR, open 2 years).
3. **"Why was my GPU idle?" — the 8-year forum ritual.** PyTorch t/170801 mega-thread (util 0%
   with memory allocated), t/18818/t/21180/t/185946/t/187306; yolov7 #2064; r/MachineLearning
   gpu_sentinel thread (86 pts — "over a thousand dollars of cloud charges" from an
   uncatchable hang); Expanse YC Show HN (101 pts): 59% of a national cluster's compute
   wasted; evo-hq/evo #52 (2026-06-02): agents need machine-readable GPU/process liveness —
   our `--json` is the answer, undocumented as such.
4. **"VRAM crept up overnight and OOM'd; which process?"** ollama #10597 (unfreed VRAM, 17
   comments), #16336 (25GB KV cache held hostage), vllm #36973 (3.4GiB leak, 23 comments) —
   every diagnosis is serial nvidia-smi screenshots hours apart.
5. **"Ollama silently fell back to CPU."** ollama #14258 (open, "single most common source of
   user confusion," 500+ related issues, fix-in-flight is a log-level bump); r/ollama and
   r/LocalLLaMA threads where commenters do VRAM arithmetic by hand (`ollama ps` shows
   "25%/75% CPU/GPU" and nobody noticed for weeks).
6. **"Utilization is lying to me."** Utilyze Show HN (128 pts) and NVSonar launched on exactly
   this in 2026; trainy.ai's SM-efficiency post remains canonical. We already label duty-cycle
   honestly — on consumer cards where Utilyze cannot run.
7. **"The monitor is perturbing my GPU / wedging."** bottom #1291 (NVML temp polling keeps GPU
   awake — open since 2023-08); btop #1612 (NVML ioctl stall aborts the monitor, 2026-04);
   gpustat #99; netdata #10362; HN 46750425 (nvidia-smi hangs at ~66-day uptime; the NVLink
   precursor errors were visible and nothing surfaced them).
8. **"My 3090's VRAM cooked for 6 months and nothing told me."** r/LocalLLaMA 1h56yko; NVIDIA
   forum thread 168346 (85k+ views, 350+ comments, years open) — junction temps hidden on
   Linux; community resorts to raw PCIe register readers (gddr6 repos).
9. **WSL2 crash-and-confusion.** nvtop #432 (asserts on N/A), #459 (still core-dumping
   2026-04); WSL #7162/#9938/#11277 — absence-without-explanation is the cross-tool norm; we
   already ship the explanation.
10. **Per-process attribution broken where it matters.** btop #968 (+9 reactions, top ask,
    unshipped); nvtop #320 (fdinfo assertion crash, open); dcgm-exporter #521 (closed
    unimplemented — no host PIDs); container names show "[Not Found]" (GPU Hot HN thread).
11. **Cross-vendor rot / the xe hole.** Arc B580 owners have no working monitor (frigate
    #21794 closed-stale; Arch bbs 295732 and fresh 304567; Mission Center #462; btop #1407
    open) — our shipped dual i915/xe dialect support is a near-monopoly.
12. **sudo/installer fatigue.** Utilyze's capability-mutating curl|bash drew HN objections
    (SilentM68, smcleod); intel_gpu_top needs CAP_PERFMON; macmon's whole pitch is "sudoless."
    Static binary + "everything you see was read without root" is marketable.
13. **Fleet-lite (2–20 boxes) keeps bouncing off Prometheus.** gpustat #73, gpu-hot #26,
    r/selfhosted 1dwlfdo — hold the v1 line per roadmap; the NDJSON contract is the bridge.

---

## 5. Standout strategy

### 5.1 Resolving the three lenses

The defensibility, adoption, and product-depth lenses agree on the core (persistence +
scroll-back first; claim the category; publish the contract; honesty as moat). They conflict
in three places, resolved as follows:

- **Launch timing** (adoption: "launch fast, the naming window is closing" vs product-depth:
  "never demo the promise, demo the binary"). **Resolution: two tracks.** The days-effort
  positioning work (README category claim, NDJSON spec, `report` digest, mock demo) happens
  *now*, in parallel with the weeks-effort persistence + scroll-back. The public launch
  (Show HN) is gated on minimal scroll-back + SQLite landing — because the killing top comment
  ("all-smi already has replay") is otherwise unanswerable. Claiming the name in the repo is
  not a launch; do it today.
- **Open contract vs imitation risk** (defensibility worries all-smi/netdata could implement
  our published spec). **Resolution: publish anyway.** all-smi will ship a schema regardless;
  first-mover on the *spec* makes gpuviewer the reference implementation, and the moat was
  never the format — it is the event taxonomy, the per-version decoder fixture corpus, and
  the trust reputation. Format secrecy buys weeks; ecosystem lock-in compounds for years.
- **Event sinks vs the punt list** (adoption/product-depth want `--on-event`/webhook;
  `04-synthesis.md` §6 punts "alerting/notifications" to v2+). **Resolution: flagged roadmap
  amendment, narrowly scoped.** New evidence since that punt: gpuwatch earned 44★ in days for
  notifications with *zero recording*; bottom #1046 sat 3 years; r/ML gpu_sentinel validated
  notify-on-idle. A single `--on-event 'CMD'` / webhook flag is an **output sink on the
  already-shipped event stream** — no rules engine, no notification subsystem, no daemon.
  This re-litigates the punt with evidence, per the CLAUDE.md standard. The Prometheus
  exporter stays punted for v1 (unchanged).

Everything else from the lenses grafts cleanly: defensibility's fixture-corpus moat and trust
weaponization; adoption's demo/packaging/thread-seeding machinery; product-depth's
event-anchored seeking, digest-first UX, and the two new narrations (hang, spillover) gated
behind conservative thresholds.

### 5.2 Positioning + tagline

**Positioning:** gpuviewer is the GPU flight recorder — the only monitor where simply having
it open means last night's incident is already recorded, replayable, and *explained*: facts
asserted plainly, inferences labeled "likely" with auditable evidence. Against nvtop/btop it
is not a better gauge but the black box their maintainers formally declined to build; against
all-smi/qmassa it is always-on narrated history versus opt-in raw replay you had to start
before the incident; against netdata/Grafana it is per-process, 1Hz, zero-daemon, and it says
*why* — in a single unprivileged static binary you scp to the box.

**Tagline:** *"It was already recording. Scroll back to 02:14 — it'll tell you why."*
(Short form for badges/topics: **"The GPU flight recorder."**)

### 5.3 Prioritized moves

| # | Move | Gap it owns | Vs whom | Effort |
|---|---|---|---|---|
| 1 | **Land SQLite 10s/1m rollups + event-anchored scroll-back replay** (story feed as jump list: Enter on "02:14 thermal throttle" seeks there; free scrub second; visible retention caps; corruption-tolerant open) | Gap 1 (the wedge) + Gap 3 | all-smi (opt-in replay, no narration), qmassa, netdata/Beszel (device-level, no why), nvidia-smi daemon | **Weeks — schedule-critical.** all-smi ships monthly; this is a race, not a backlog item |
| 2 | **Claim the category name now**: README h1 = "The GPU flight recorder," repo topics, comparison table (vs all-smi record, gpud, netdata DCGM, qmassa, LACT, DCGM stack), kill the "research phase" framing | Category ownership | gpu-flight/gpufl (name squatter, pushed 2026-06-05), the 2026 flight-recorder zeitgeist (Dial9, pg_flight_recorder, 6+ agent recorders) | **Days** |
| 3 | **Publish the NDJSON contract v1**: frames AND narrated events in one timestamped stream, `schema_version`, written semver compat promise, JSON Schema + golden conformance fixtures, consumer recipes (scripts, agents per evo #52, Home Assistant, W&B/MLflow overlay) | Gap 8 | all-smi (`schema: 1`, no events-in-stream, no promise), nvtop JSON (no timestamps, broke twice #438/#444) | **Days** |
| 4 | **`gpuviewer report --since 22:00`** plain-text night digest ("3 events: 2 facts, 1 inference"), paste-able into Slack/issues, works over SSH/CI | Gaps 1+4 (digest-first; nobody summarizes) | all-smi (scrub 12h at ≤8x), netdata (charts, no why) | **Days** |
| 5 | **AMD `gpu_metrics` (per-version v1.0–v3.0) + Intel i915/xe throttle decoders** with committed real-hardware fixture trees; upgrade all throttle events to onset/recovery pairs with quantified deltas ("SM 1980→1410 MHz, recovered 02:16:02") incl. NVIDIA SW power-cap/thermal | Gaps 2+7 (only persistent cross-vendor throttle narrator) | LACT (GUI-ephemeral), gpud (NVIDIA HW-bits only), amdgpu_top (30s residency log), gpustat #135's dead repo | **Weeks.** Fixture discipline is non-negotiable (the honesty kill-condition) — and the fixture corpus is the asset a feature-PR cannot copy (btop's AMD path broke twice without it, #774/#1540) |
| 6 | **`gpuviewer demo` + launch artifact**: scripted overnight incident on the deterministic mock (throttle 02:14→02:16, ollama +15.6GB w/ ETA, idle-gap, exit freeing 21.3GB), VHS GIF of the *scroll-back* as README lead + Show HN — with the honesty receipt (RTX 4090 oracle, 56/57 frames) and a preemptive counter-table for "netdata/all-smi/gpud already do this" | Conversion of Gap 1 | Every competitor demo needs hardware (GPU Hot needs Docker+toolkit; Utilyze needs datacenter SKUs; all-smi needs a capture you made) | **Days; launch gated on move 1** |
| 7 | **Recorder self-honesty**: per-call collection timeouts surfaced as narrated timeline events ("NVML unresponsive 02:14–02:15; last good frame attached"), adaptive idle backoff that preserves GPU sleep/GFXOFF, published measured self-impact from the 4090 rig | Gap 9 | bottom #1291 (open 3y), btop #1612, HWiNFO (Windows-only precedent), HN 46750425's invisible precursors | **Weeks; ships with v1** — "always-on" becomes the attack vector without it |
| 8 | **`--on-event 'CMD'` / webhook (ntfy/Slack) sink + documented W&B/MLflow adapter** injecting narrated events into run timelines — Mission-Control-style overlays on *any* hardware | Pains 2–4; subsumes the notify-me category | gpuwatch (44★, no recording), bottom #1046, CoreWeave Mission Control (cloud-locked) | **Days–weeks.** ⚠️ Flagged amendment to the 04-synthesis punt list (see §5.1); Prometheus exporter remains punted |
| 9 | **Shareable incidents**: `gpuviewer export --since 22:00 incident.gpvr` (SQLite slice + event log) replayable on any machine without hardware; then seed the verified demand threads with tool-shaped answers (bottom #1389, nvitop #167/#217, gpustat #135, ollama spillover threads, evo #52) | Gaps 1+3 distribution; "attach the recording to the bug report" | qmassa (validated the artifact with 0 marketing), serial-screenshot forensics in ollama/vllm threads | **Weeks** |
| 10 | **Two new narrations**: (a) hang — "VRAM held, zero engine activity 12 min, process alive — job likely hung"; (b) spillover — "ollama attached +15.6GB but GPU engine ~idle while ollama CPU pegged — likely partial CPU offload." Both inference-tier, "likely"-labeled, evidence-expandable, conservative thresholds, user-tunable | Gaps 4+5 (nowhere-served; ollama #14258's 500-issue pain) | Nothing external ships either; in-process tools (TraceML/NVRx) require instrumentation | **Weeks; sequenced after 1/5/7** — highest confidently-wrong risk in the pipeline |

**Sequencing logic:** moves 2/3/4 are days and run in parallel with move 1 (weeks,
critical-path). Move 6 fires the launch once 1 lands. Moves 5/7 are the v1 hardening wave;
8/9 ride the launch; 10 is the first post-launch feature wave. Total critical path to launch:
the duration of move 1.

---

## 6. Threats to watch + tripwires

Ordered by (probability × wedge damage). Each tripwire is a concrete observable.

| Threat | Why it matters | Tripwires |
|---|---|---|
| **all-smi (Lablup)** — highest | Rust single binary with record/TUI-replay/schema-versioned NDJSON/SSE/webhooks shipped May 2026, corporate backing, monthly cadence. One release from "always-on" | Release notes mentioning background/always-on capture, persisted alert log, SQLite/rollups, per-process rows in record, or an event/annotation concept. **Check lablup/all-smi releases monthly** |
| **leptonai/gpud** — high | 482★, weekly releases, ex-hyperscaler team; single-binary Xid/kmsg/hw-slowdown *events* today | A local timeline/replay UI; SW power-cap/SW-thermal events; AMD/Intel support; consumer-GPU positioning; de-emphasis of the Lepton cloud tether |
| **netdata** — high | Already owns always-on retention + scrub UI; 2026-05-04 DCGM collector shipped default XID/throttle alerts + anomaly ML, marketed at AI infra | Throttle/XID alerts ported to the consumer nvidia_smi collector; any per-process GPU collection; "GPU events" product language; GPU collectors flipped to auto-detect |
| **W&B / CoreWeave Mission Control** — high (conditional) | Infra events on training plots is our thesis, shipped — cloud-locked today | Any W&B announcement of an **on-prem/local "infrastructure agent"**; wandb changelog entries about throttle/XID/system events |
| **nvitop** — medium | monitor-web (2026-05-25) is concrete motion toward "history without Grafana"; maintainer ships fast when motivated | monitor-web graduating from `examples/` to a supported `nvitop --web`; on-disk persistence; `root_pids={}` process-disable removed |
| **qmassa** — medium | Only other shipping TUI replay; the most capable author on Intel/AMD fdinfo | Always-on ring/daemon mode; an event log or any narration beyond the PL1 flag; NVIDIA depth |
| **GPU Hot** — medium | 1.5k★ validated browser audience; active maintainer | Alerts PR #40 merged; any SQLite/persistence commit; auth |
| **NVIDIA trickle-down** — medium-term structural | NVSentinel #890 (P1) shows NVIDIA being asked for throttle-onset detection; dcgm-exporter 4.8.x drifting event-ward (health incidents, bind/unbind) | An official nvidia-smi/NVML "events" mode; NVSentinel #890 closing as shipped; DCGM event features reaching GeForce |
| **eBPF wave** — medium-term | zymtrace (always-on, zero-code), eACGM (IWQoS'25), Ingero's correct-but-unshipped blueprint, eBPF Foundation funding GPU work. Kernel-level causality sees what NVML polling cannot | A polished OSS single-node eBPF agent with narration; Ingero's GitHub org materializing with real artifacts; zymtrace adding event/timeline semantics |
| **Meta GCM repackaging** — medium | Our event taxonomy with Meta's brand, currently Slurm/OTLP-coupled | A community fork stripping Slurm into a single-node binary; GCM roadmap items for workstation use |
| **Brand erosion** — medium, time-bound | gpu-flight org (gpufl, gpuflight.com, PyPI — pushed 2026-06-05) squats the name adjacency; GpuViewR/gpuview sit next to "gpuviewer" in search | gpufl pivoting from in-app library to system agent; any entrant using "flight recorder" for a GPU monitor. **Counter: move 2, now** |
| **btop PR #1552** — low, scoped | Per-process NVIDIA GPU in a 32.7k★ incumbent erodes the per-process pitch (never the history/narration wedge) | PR merged |
| **macOS v2 turf** (macmon/mactop) — v2-horizon | Both added Prometheus endpoints in 2026 = history demand acknowledged, delegated. A SQLite+events release from either occupies our v2 slot | Persistence commits in vladkens/macmon or metaspartan/mactop; **WWDC26 (June 8–12) keynote** — any per-process GPU API (erases a differentiator) or further private-API breakage |

---

## 7. What we explicitly do NOT chase, and why

Consistent with the `CLAUDE.md` punt list and `04-synthesis.md` §6 — re-affirmed against
June-2026 evidence; one amendment flagged.

1. **Control features (fan/OC/power limits).** LACT owns it (now v0.9.0 with cross-vendor
   throttle display — let it). Root writes, hardware liability, orthogonal to the wedge.
2. **Daemon/client split.** Unchanged. "Always-on" means *by virtue of normal use* (TUI or
   `--json` running) — not a background service. Users who want boot-time recording can run
   `gpuviewer --json` under their own systemd user unit; we document that, we don't ship a
   daemon. (Mission Center's Magpie tracker remains the catalog of daemon failure modes.)
3. **Prometheus exporter in v1.** Unchanged — the heavyweight-stack fight is the incumbents'
   turf (nvitop-exporter, dcgm-exporter, qmmd all exist). The NDJSON contract (move 3) is the
   v1-compatible bridge; revisit the exporter at v2 as adoption fuel per the roadmap.
   ⚠️ Note the *narrow* amendment in §5.1: a single `--on-event` output sink is pulled
   forward on new evidence; it is not an alerting subsystem.
4. **Cluster/fleet views.** beszel owns fleet-lite persistence at checkbox depth; netdata,
   GCM, l9gpu, NVSentinel own everything above. Our fleet story is `--json` over SSH + a
   stable contract others can aggregate.
5. **The accuracy/SOL% fight.** Utilyze owns "real perf counters on datacenter SKUs" — do not
   contest it. We are the *honest consumer-GPU complement*: duty-cycle labeling (shipped),
   plus opt-in NVML GPM SM-activity on Hopper+ at v2. True SOL% on GeForce is not derivable —
   never fake it.
6. **eBPF causal tracing (Zoomer-class "why").** Still the validated tar pit: highest effort,
   needs root+BTF kernels, and HTA/dynolog cover the deep-window need. Our relationship to
   profilers is a hand-off ("idle gap 02:14:31 — if recurring, capture a trace"), not
   competition.
7. **macOS per-process GPU / ANE / WSL2 per-process workarounds.** OS-prohibited at the
   driver/kernel level. Honesty about the absence *is* the feature (and our WSL2 explanation
   already ships). Re-check only if WWDC26 or Microsoft changes the APIs.
8. **Exotic accelerators (NPUs, TPUs, Tenstorrent, Rockchip…).** nvtop and all-smi own breadth;
   near-zero wedge-audience overlap. Our breadth axis is *depth per vendor* (decoders,
   fixtures), not vendor count.
9. **A web dashboard now.** GPU Hot's flank is real but the hard part (the persistent store)
   comes first; a later thin web/replay view over the SQLite store attacks it from strength.
   Tauri remains rejected (tauri#9394 — dofek is currently living that mistake); GUI is v2
   via iced, unchanged.
10. **Vendor SDK linking.** hw-smi links ADLX/AMDSMI/L0 and is already filing upstream bug
    reports for broken counters; btop's rocm_smi path broke twice. Pure sysfs/fdinfo + NVML
    via explicit `lib_path` stays settled.

---

## Appendix: corrections to prior research

- **leptonai/gpud was missing from the 01 matrix** — it is the most serious single-binary
  event-tier overlap and is now tracked (threat table, §6).
- 01's gap #4 phrasing ("nobody surfaces throttle reasons as events") is now only true with
  the §3.2-2 qualifications — LACT/gpud/amdgpu_top force the sharper claim.
- 01's complaint list item "nvidia-smi hangs wedging monitoring scripts" now has a fix PR
  upstream (open-gpu-kernel-modules #1014) but the precursor-invisibility lesson stands and
  feeds move 7.
- 04's §4 feature table remains valid; this document's moves 1–10 are its execution ordering
  under June-2026 competitive pressure, with one flagged punt-list amendment (§5.1).
