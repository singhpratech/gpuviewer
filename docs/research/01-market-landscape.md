# Market Landscape — GPU Monitoring Tools (researched 2026-06)

> Multi-agent web research sweep across TUI/CLI, desktop GUI, Apple Silicon, and ML/datacenter
> categories. Star counts and versions verified against GitHub as of June 2026.

## The competitive matrix

| Tool | UI | OSes | NVIDIA | AMD | Intel | Apple | Per-process GPU | History | Notes |
|---|---|---|---|---|---|---|---|---|---|
| nvidia-smi | CLI | Linux, Win | ✅ | — | — | — | VRAM only (N/A on WDDM/WSL2) | none | feature-frozen; exists to be wrapped |
| nvtop (10.7k★) | TUI | Linux (+exp. macOS) | ✅ | ✅ | ✅ | partial | ✅ | session only | Windows PR #419 **rejected** 2026-04; macOS build = Apple backend only |
| nvitop (6.9k★) | TUI | Linux, Win | ✅ | — | — | — | best-in-class (SM% per PID) | session only | Python env required; NVIDIA only |
| btop (32.7k★) | TUI | Linux, macOS, BSD | Linux | Linux | Linux (root) | macOS | **none** (top complaint: #968 #855 #988) | session only | all 4 vendors but never on one OS |
| bottom/btm (13.4k★) | TUI | Linux, Win, macOS | Lin+Win | Linux | — | — | ✅ (NVIDIA/AMD) | session only | Rust; **no GPU at all on macOS**; uses nvml-wrapper |
| gpustat (4.4k★) | CLI | Linux, Win | ✅ | — | — | — | VRAM only | none | no release since 2023-08 |
| amdgpu_top (1.6k★) | TUI+egui | Linux | — | ✅ deep | — | — | ✅ per-engine | session | Rust; `libamdgpu_top` reusable crate |
| intel_gpu_top | console | Linux | — | — | ✅ | — | ✅ fdinfo | none | global PMU stats need root/CAP_PERFMON |
| qmassa (106★) | TUI | Linux | — | ✅ | ✅ | — | ✅ fdinfo | session | Rust; vendor-agnostic DRM fdinfo approach |
| macmon (1.6k★) | TUI | macOS AS | — | — | — | ✅ | **none** | session | Rust; sudoless via private libIOReport.dylib |
| mactop v2 (1.4k★) | TUI | macOS AS | — | — | — | ✅ | experimental GPU% | session | Go; AGXDeviceUserClient AppUsage scraping |
| asitop (4.6k★) | TUI | macOS AS | — | — | — | ✅ | none | none | **abandoned** 2024-04; needs sudo |
| Stats (39.4k★) | menu bar | macOS | — | eGPU | older | ✅ | none for GPU | none | dominant Mac monitor; ANE FR closed "not planned" |
| GPU-Z | GUI | Windows | ✅ | ✅ | ✅ | — | none | CSV log | 2008-era dialog UI; great sensors incl. PerfCap reason |
| HWiNFO64 | GUI | Windows | ✅ | ✅ | ✅ | — | none | CSV log | free tier kills shared-memory API after 12h |
| MSI Afterburner | GUI | Windows | ✅ | ✅ | partial | — | none | logging | dated skinned UI; single-dev bus factor |
| Task Manager GPU tab | GUI | Windows | ✅ | ✅ | ✅ | — | ✅ (WDDM counters) | none | **misleading for CUDA** (3D engine default; HAGS hides Compute graph) |
| Mission Center (GTK4) | GUI | Linux | ✅ | ✅ | ✅ | — | ✅ (nvtop-derived) | short | closest to "Task Manager but better"; Linux only |
| LACT (4.9k★) | GUI | Linux | ✅ | ✅ | ✅ | — | none | short | the "Afterburner for Linux" (control); Rust+GTK4 |
| GPU Hot (1.5k★) | web | Linux | ✅ | — | — | — | partial | **in-memory only** | HN-validated demand for "no Grafana" middle ground |
| DCGM+Prometheus+Grafana | web | Linux | ✅ | — | — | — | none (no PIDs: #347 #521) | ✅ real | only source of true saturation metrics; heavyweight; PROF fields need datacenter GPUs |
| W&B / MLflow system metrics | web | all | ✅ | ✅ | ✅(W&B) | — | own-PID only | per-run | only place GPU metrics share a timeline with loss/step |
| all-smi (170★) | TUI | Linux, macOS | ✅ | ✅ | ✅ | ✅ | ✅ | none | Rust; closest existing spirit; small; has mock-server CI pattern |

## Validated gaps (each cross-checked against issues/forums)

1. **No tool covers NVIDIA + AMD + Intel + Apple across Linux + Windows + macOS.**
   Every contender fails one axis: nvtop (no Windows — PR rejected; macOS = Apple-only build),
   btop (no per-process GPU, no Windows), bottom (no macOS GPU, no Intel, no Apple).

2. **Windows terminal GPU monitoring is NVIDIA-only.** No terminal tool exposes AMD (ADLX) or
   Intel GPU stats on Windows. Meanwhile Task Manager misleads CUDA users (shows ~0-3% under
   full ML load; HAGS removes the CUDA graph entirely). Vendor-agnostic WDDM PDH counters
   (`GPU Engine`/`GPU Process Memory`) exist and nobody in the terminal space uses them.

3. **No "just-works" persistent history.** Nothing between `watch nvidia-smi` (ephemeral) and
   DCGM+Prometheus+Grafana (heavyweight, K8s-flavored, PROF metrics locked to datacenter GPUs).
   "What did my GPU do during last night's training run?" is unanswerable with a single binary.
   GPU Hot's 1.5k stars in months proved demand; it ships no persistence/auth.

4. **Nobody explains state changes.** NVML literally exposes throttle *reasons* (thermal/power
   cap/sync-boost) as a bitmask — no tool surfaces them as events. Xid errors, ECC, VRAM-pressure
   trends → only reachable via DCGM or dmesg-grepping. The "why did util drop at 14:32" niche is
   simultaneously empty and validated (Meta's Zoomer = internal-only; Ingero = 0-star eBPF agent;
   W&B owns the timeline but can't explain dips in it).

5. **The misleading-util problem.** NVML "GPU-Util" = % of time ≥1 kernel was resident — 100% can
   mean 1-6% of peak FLOPs (trainy.ai, arthurchiao.art, utilyze). Every mainstream tool inherits
   it uncriticized. True saturation (SM_ACTIVE/occupancy) needs DCGM PROF (datacenter-only) or
   NVML GPM (Hopper+). An honest tool should at minimum label the metric correctly.

6. **macOS per-process GPU + local-LLM headroom.** No CLI tool shows per-process GPU on macOS
   (powermetrics' per-process GPU ms/s is broken on Apple Silicon). Local-LLM users want
   wired-limit headroom / "will this model fit" (`recommendedMaxWorkingSetSize`,
   `iogpu.wired_limit_mb`) — only the embryonic `gpuer` (42★) touches this.

7. **Per-process GPU on mixed-vendor machines** (AMD iGPU + NVIDIA dGPU laptops) requires 2-3
   single-vendor tools anywhere outside Linux/nvtop.

8. **2-20 machine fleet tier unaddressed** — gpustat-web (semi-dormant SSH fan-out), GPU Hot hub
   (no persistence/auth), or full Prometheus. Nothing lightweight with retained history.

## Recurring user complaints worth designing against

- sudo fatigue (asitop/pumas; intel_gpu_top CAP_PERFMON; btop Intel)
- breakage on every macOS release (powermetrics format churn; IOReport channel renames per chip)
- NVML polling keeping GPUs awake / raising idle power (bottom #1291; amdgpu GFXOFF break via GRBM polling)
- driver/library friction: NVML missing in containers, ROCm SMI soname churn broke btop twice (#774, #1540), nvidia-ml-py pinning
- WSL2: per-process info is N/A at the NVML level; tools crash on it instead of degrading (nvtop #432)
- distro packaging lag pushing users to curl|sh or stale features
- "ghost" 100% util with no processes; nvidia-smi hangs wedging monitoring scripts (uint32 timer overflow at ~66 days uptime, HN 46750425)
