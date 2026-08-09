# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commit authorship (hard rule — no exceptions)

Every commit is authored and committed solely as
`singhpratech <42719720+singhpratech@users.noreply.github.com>`. Never add
`Co-Authored-By` trailers, "Generated with" lines, or any AI attribution; never use a
personal email in authorship. This overrides any default trailer behavior. Any subagent
or workflow that commits must be given this rule verbatim in its prompt.

## What this project is

**gpuviewer** — "the GPU flight recorder": a cross-vendor GPU monitor whose differentiator is
**persistent history + narrated events** (throttle onset with cause, VRAM→OOM trend with ETA,
process lifecycle, training idle gaps), not live gauges. The wedge is the replayable timeline:
"scroll back to 02:14 and see why the run stalled." Read `docs/research/04-synthesis.md` first —
it is the product/architecture decision record; the other files in `docs/research/` are the
supporting market/API/stack evidence (researched June 2026, with issue numbers).
`docs/design/cross-platform.md` is the settled decision record for the Windows (v1.5) and
macOS (v2) backends — `wddm.rs`/`apple.rs`/`nvidia.rs` cite it by section number throughout;
it extends 04-synthesis with the same re-litigate-only-with-new-evidence status.
`docs/design/timeline-overview.md` is the design note for the shipped timeline view.

## Commands

```sh
cargo build                 # build all crates
cargo test                  # all tests run against the mock backend — no GPU needed
cargo test -p gpuviewer-core --lib          # single-crate tests
cargo run --release -- --mock               # TUI with simulated GPUs
cargo run --release -- --json --once --mock # one NDJSON frame + events to stdout (CI/scripting)
cargo run --release -- --json --mock --interval 100   # fast-forward sim, NDJSON stream
cargo run --release -- demo                 # seed 8h of simulated history, open scrolled back
cargo run --release -- demo --seed-only     # just build history-demo.db (no tty needed)
cargo run --release -- report --since 12h   # plain-text digest of recorded history
cargo run --release -- export --since 12h out.gpvr    # shareable incident slice
cargo run --release -- view out.gpvr        # replay a .gpvr read-only, no GPU required
```

Flags: `--json`, `--once`, `--mock`, `--interval <ms>` (default 1000, clamped to min 100 with
a stderr warning), `--db <path>`, `--no-persist` (disable recording — replay and `report` need
it), `--no-backoff` (disable the adaptive idle cadence), `--on-event 'CMD'` (per event, with
`GPV_EVENT_*` env vars, capped 60 spawns/min; `sh -c` on Unix, `cmd /C` on Windows), `-V`/`-h`.
`report` takes `--since`/`--until`/`--db`/`--mock`; `export` takes `--since`/`--db`/`--mock`
plus one output path — its window **always ends now**, there is no `--until`. Time specs:
`30s`/`45m`/`12h`/`7d` or `HH:MM` (today, else yesterday); default window 24h. **Subcommands
must be the first argument** (`gpuviewer --mock report` fails with "unknown flag: report").

History-db default resolves per-OS through ONE resolver
(`gpuviewer_history::store::default_data_dir`, env-var-only by design — the `dirs`/`directories`
crates were rejected so tests and CI can redirect; an empty env var counts as unset):
Linux `$XDG_DATA_HOME/gpuviewer` else `~/.local/share/gpuviewer` (this chain is frozen
byte-identical to shipped v1 — users' `history.db` must not move); macOS
`~/Library/Application Support/gpuviewer` (XDG deliberately ignored); Windows
`%LOCALAPPDATA%\gpuviewer` else `%USERPROFILE%\AppData\Local\gpuviewer`.

Binary is `gpuviewer` (crate `gpuviewer-tui`). With no real backend available the mock is
the automatic fallback, so the TUI always renders. Recording is always-on by default;
`--mock` records to a separate `history-mock.db` and `demo` to `history-demo.db`, so
simulations never pollute real history.

Recording DBs are **stamped** real or mock on first write; recording the other flavor into one
is a fatal `DataSourceMismatch` before any row lands. One recorder per DB file (kernel lock on
`<db>.lock`) — the lock loser keeps running live-only with a stderr note. Persist + no real GPU
+ an explicit `--db` is a fatal startup error: a mock session must never record into a real DB.
A `.gpvr` **is** a standalone SQLite db with the identical schema (meta/devices whole; sample,
process, and event rows window-filtered; `export_from_ms`/`export_to_ms` stamped in meta), so
`view` just `open_readonly`s it — there is no separate import format. Retention (public
constants): 10s detail and per-process rows 48h; 1m rollups and events 30d; the Recorder
auto-prunes roughly hourly. `--json` treats a closed stdout (`| head`) as a clean shutdown with
the recording tail persisted, and a `--once` run ends with the `recording_stopped` stop-mark as
its final NDJSON line.

MSRV is **1.95**, pinned once at the workspace root (`[workspace.package] rust-version`) and
inherited by every crate — forced by libsqlite3-sys 0.38 (rusqlite 0.40, bundled) using
`cfg_select!`, stable since 1.95. CI builds on stable only, so verify MSRV by hand
(`cargo +1.95.0 check`) before dep bumps or new syntax.

## Workspace layout

- `crates/core` — model (`model.rs`), `GpuBackend` trait + registry (`backend.rs`), event
  derivation (`events.rs`), mock simulation (`mock.rs`), shared Linux `/proc` metadata
  (`proc_meta.rs` — `CpuTracker` cumulative-ticks→rate and `parse_cgroup`/`container_of`
  docker/k8s/podman/lxc labels, never a fabricated id; used by all three Linux backends), and
  the vendor backends: `nvidia.rs` (NVML, Linux+Windows), `amd.rs`/`intel.rs` (Linux
  sysfs/fdinfo), `wddm.rs` (Windows cross-vendor DXGI/PDH/D3DKMT), `apple.rs` (macOS Metal +
  gated IOReport tiers). serde is the only unconditional dep; features `nvidia`/`wddm`/`apple`
  (all default-on) pull target-gated deps — `nvml-wrapper` 0.12 (linux/windows,
  `legacy-functions`), `windows` 0.62 minor-pinned (windows), the objc2 family + `dlopen2` +
  `core-foundation` (macos; objc2's `exception` feature is **banned** — it compiles a C shim,
  see the Cargo.toml decision comment).
- `crates/history` — `DeviceHistory` ring + `HistoryStore` (events log), plus the shipped
  SQLite tier: `SqliteStore` (`store.rs` — 10s/1m rollups, event log, WAL, retention, `.gpvr`
  export) and `Recorder` (folds frames into buckets; deliberately swallows store errors so a
  persistence failure never kills collection). Write opens run in **load-bearing order**:
  exclusive `<db>.lock` kernel lock (one recorder per file) → refuse newer-schema files
  byte-untouched (`SchemaTooNew`) → only then `quick_check`, quarantining corruption to
  `*.corrupt-<secs>`. A busy or merely-newer db must never reach quarantine. Events dedupe via
  `UNIQUE(ts_ms, device_id, kind, title)` created **only** by `migrate_event_dedupe`, never in
  `SCHEMA_SQL`: a CREATE UNIQUE there would fail on pre-v2 duplicates, read as corruption, and
  quarantine a healthy file. Schema changes = bump `SCHEMA_VERSION` + add a migration; never
  add constraints to `SCHEMA_SQL` that can fail on existing data.
- `crates/tui` — `collector.rs` (Engine = tick loop shared by TUI thread and `--json` mode,
  adaptive backoff, collector self-honesty events incl. device-lost/returned and recording
  start/stop session marks), `app.rs` (**three** modes: live, scroll-back replay with
  event-anchored seek, and the `t` timeline overview — 7-rung zoom ladder 1h–7d, 10s tier at
  1h else 1m tier, `Enter` drills into replay; `gpuviewer view` pins the app to replay over a
  threadless `Collector::stationary`, where `Esc`/`r` stay inert; all rendering must read
  through the mode-gated `replay_window()`/`timeline_window()` accessors or a stale cache leaks
  into live), `ui.rs` (tabs/charts/gauges/process table/story feed + timeline strips and event
  lane; STALE/LOST staleness affordances), `main.rs` (CLI: `report`/`demo`/`export`/`view`
  subcommands, NDJSON emission).

## Roadmap sequencing (do not reorder casually)

v1 = Linux only (NVIDIA/AMD/Intel), TUI + `--json`, history + events. v1.5 = Windows (NVML+PDH
dual-source in `nvidia.rs` plus cross-vendor OS-surface `wddm.rs` for AMD/Intel and NVML-less
NVIDIA). v2 = macOS Apple Silicon (device-level only) + iced GUI. **The Windows and macOS
backends are already in-tree and CI-compiled** — `apple.rs` ships Tier A (public Metal) now,
Tiers B/C are a None-everywhere stub behind the WWDC26 re-check gate. The v-numbers gate
*release and hardware validation* (`docs/release-checklist.md`), not code presence. Punted
entirely from v1: control features (fan/OC — LACT owns it), daemon/client split, Prometheus
exporter, cluster views, macOS per-process GPU (OS-prohibited — see below).

## Architecture decisions (settled — re-litigate only with new evidence)

- **Workspace of three crates**: `gpuviewer-core` (collection), `gpuviewer-history`
  (rings + SQLite), frontend (`gpuviewer-tui`, ratatui 0.30). GUI later is **iced**
  (Sniffnet's playbook); **Tauri was rejected** (WebKitGTK blank-window bug on proprietary
  NVIDIA — tauri#9394 — hits exactly our users), Slint rejected (no chart widget),
  Electron rejected.
- **`trait GpuBackend`** mirrors nvtop's vtable split: `static_info` (once) /
  `refresh_dynamic` (per tick) / `refresh_processes` (per tick); per-field `Option<T>`,
  never validity bitmasks. Explicit registry (`all_backends(force_mock)`), no inventory/ctor
  magic; **registration order nvidia → amd → intel → wddm → apple is load-bearing** — the tui
  collector dedupes devices first-backend-wins on `normalize_pci_id`, so NVML claims NVIDIA
  boards ahead of wddm. Synthetic ids (`mock:`/`wddm:`/`apple:`) never dedupe by design
  (double listing beats a wrong merge). Mock registers only as a fallback when zero real
  backends init, or exclusively under `force_mock`. Contract tested in
  `crates/core/tests/registry_dedupe.rs` — currently a *transcription* of the collector loop,
  not the production fn (move the loop into core to fix).
- **Never hard-link a vendor SDK.** NVIDIA via `nvml-wrapper` (must init with
  `Nvml::builder().lib_path("libnvidia-ml.so.1")` — the `.so` symlink only exists with CUDA
  toolkit installed). AMD Linux via direct sysfs/hwmon/`gpu_metrics`/fdinfo parsing — do NOT
  link librocm_smi64 (soname churn broke btop twice). Intel Linux via fdinfo + sysfs with
  **both i915 and xe dialects** (different fdinfo keys: `drm-engine-*` ns vs `drm-cycles-*`).
  Future dynamic loads via `dlopen2` with `Option<fn>` fields, signatures bindgen'd from
  official headers. **OS system libraries are not vendor SDKs** — linking the `windows` crate
  (Pdh/DXGI/D3DKMT) and Metal/CoreFoundation is allowed; the ban targets vendor SDKs.
- **History**: RAM ring buffers for the live window; downsampled 10s/1m aggregates
  batch-inserted into SQLite (`rusqlite`, bundled, WAL) + append-only event log. Never write
  raw 1Hz samples to SQLite. One timestamp per collection frame, not per metric.
- **Events are two-tier**: facts (throttle bit set, process exited) asserted plainly;
  inferences (dataloader stall) always labeled "likely" and expandable to raw evidence.
  A confidently-wrong narration kills the product's trust thesis. Concretely: **14 kinds**.
  `EventEngine` derives throttle start/end and process attach/exit as Facts, plus four hedged
  Likely inferences with hard-coded thresholds (consts in `events.rs`): VramPressure (≥85% of
  total climbing ≥16 MiB/min over a 180s window), IdleGap (util <10% for ≥10s after ≥30s of
  ≥50%, with a ≥256 MiB holder attached throughout; narrated at recovery), HangSuspected
  (device ≤2% + an idle ≥1 GiB holder unbroken for 600s), CpuSpillover (new ≥2 GiB holder, 90s
  window, mean util <15% + mean CPU ≥150% of a core). The other six kinds (`collector_stall`,
  `history_reset`, `device_lost`/`_returned`, `recording_started`/`_stopped`) are
  collector-emitted facts about the recorder itself, never `EventEngine` output. Derivation
  order inside `observe()` is load-bearing: throttle → spillover → process → hang → idle_gap →
  vram. `throttle: None` means **unobservable**, never "not throttling" — an open throttle
  episode is dropped silently on None, with no fabricated `ThrottleEnd`.
- **Backoff policy** (tested constants — change together with `ui.rs`): idle = <5% util, with a
  util-less fallback for Intel (encoder/decoder ≥5%, an asserted throttle bit, ≥4 MiB VRAM
  delta, or >25 MHz clock jitter); 60 consecutive all-idle ticks stretch cadence to 5×
  interval capped at 10s, and any activity snaps back instantly; **a failed probe is not idle**
  — faults are watched at full rate. Stall threshold = `max(3×interval, 5s)`; 3 consecutive
  tick panics = Critical event + collection stops. Charts never bridge gaps >15s
  (`ui.rs GAP_BRIDGE_S`, derived from the 10s backoff cap — unrecorded time must render as a
  hole, never as zero).

## Domain rules that look like bugs but aren't

- `NVML_ERROR_NOT_SUPPORTED` is a **normal per-metric outcome** — render "unavailable",
  never fail. Same for absent sysfs files, missing hwmon (Intel iGPU has none), MIG-enabled
  GPUs (device-level utilization queries legitimately return NOT_SUPPORTED).
- WSL2: per-process GPU info is N/A **at the driver level** — detect WSL2 and explain the
  absence in-UI; never crash on it. Two distinct nvtop cautionary tales, both real — #432
  ("shows incorrect GPU memory and may abort when per-process memory is N/A", cited here and
  in `docs/release-checklist.md`) and #459 ("core dump on wsl", cited in README's "The
  journey"). They are different issues; neither citation is a typo for the other.
- Polling has side effects: NVML PCIe-throughput calls block ~20ms each; AMD GRBM register
  polling breaks GFXOFF; NVIDIA temp polling keeps GPUs awake (bottom #1291). Adaptive
  cadence and opt-in perf-counter polling are requirements.
- NVML "utilization" is duty-cycle (time ≥1 kernel resident), not capacity — label it
  honestly in the UI; never present it as saturation.
- fdinfo per-process for **other users'** processes needs root/CAP_SYS_PTRACE — show
  "your processes only" hint when unprivileged, don't pretend the list is complete.
- AMD `gpu_metrics` is a versioned packed binary struct (v1.0–v3.0) with per-version field
  offsets AND units (C vs centi-C, W vs mW); decoders are per-version, fixtures required.
- Kernel-version gates (degrade gracefully): AMD fdinfo 5.14+/5.19+; Intel i915 engine 5.19+,
  per-process memory 6.8+, xe engine cycles 6.11+, xe PMU 6.15+.
- **Windows**: under WDDM, NVML per-process `usedGpuMemory` is architecturally *always*
  Unavailable → `None`, never 0 (PDH fills the column). PDH reads **require**
  `PDH_FMT_NOCAP100`, restated locally because windows-rs 0.62.2 lacks the const — without it
  PDH silently caps at 100 and summed multi-engine numbers go quietly wrong. Never use DXGI
  `QueryVideoMemoryInfo` for device memory (it reports the *calling process's* own budget).
  wddm temp/power/fan/clocks/throttle are `None` by design (§3.6 — "do not fix these in").
  LUIDs are session-scoped and never persisted: persistent identity is the D3DKMT PCI BDF, and
  the non-PCI fallback id deliberately refuses dedupe.
- **macOS**: guard every Metal selector with `respondsToSelector:` (objc2's `exception` feature
  is banned — compiled-C shim). `refresh_processes` returns empty **by OS prohibition**, with a
  hint that blames the OS, not the app. `mem_total` is Metal's `recommendedMaxWorkingSetSize` —
  a unified-memory working-set budget, **not** VRAM.

## Testing strategy (CI has no GPUs)

- `MockBackend` implements `GpuBackend` with scripted streams — all history/event/UI tests
  run against it.
- sysfs/fdinfo collectors take a **root-dir parameter**; tests run against committed fixture
  trees (`crates/core/tests/fixtures/`). Current trees are **synthetic** (hand-written,
  with decoy values a wrong code path would read — see the fixtures README); replace/augment
  with captures from real hardware per kernel/driver release. AMD `gpu_metrics` tests build
  decoy-laden binary blobs per struct revision — a new kernel header revision needs a new
  builder *and* layout entry together.
- The NDJSON contract has a conformance suite (`crates/tui/tests/ndjson_contract.rs`) that runs
  the built binary and **hand-asserts** the wire format documented in `docs/spec/ndjson-v1.md`
  (hardcoded 14-kind event list; `recording_stopped` must be the final line of a `--once` run).
  `ndjson-v1.schema.json` has **no automated consumer** — stream changes must update spec,
  schema, and suite together by hand; the suite only enforces the first and third.
- CLI behavior is pinned by `crates/tui/tests/launch_artifacts.rs` — 7 tests running the real
  built binary hermetically (`XDG_DATA_HOME`/`HOME`/`LOCALAPPDATA`/`USERPROFILE` all redirected
  to a scratch dir): `demo --seed-only` without a tty, export window math + overwrite refusal,
  `view` junk-file rejection, EPIPE clean exit, the mock/real DB contamination guard in both
  directions, the single-recorder instance lock (incl. a bounded post-kill retry for Windows
  `LockFileEx` release delay), and `--help` coverage. Keep new CLI surfaces covered there.
- NVML loader plumbing tested against a stub `.so` exporting a subset of NVML symbols:
  `crates/core/tests/nvml_stub_loader.rs` (Linux-only **and** gated on the default `nvidia`
  feature — a `--no-default-features` run silently drops it to zero tests) compiles
  `tests/nvml_stub/stub.rs` with bare `rustc` at test time and asserts `lib_path` init plus
  both degradation modes (`NOT_SUPPORTED` return and missing symbol → `None`, never failure).
- Apple IOReport parsing runs cross-OS against `crates/core/tests/fixtures/ioreport/` in the
  `channel|...` line format shared with `examples/macos_probe.rs` (the manual `macos-probe` CI
  job; probe output commits verbatim as fixtures).
- Real-hardware smoke tests are a manual pre-release checklist (`docs/release-checklist.md`),
  not CI.

## CI

`.github/workflows/ci.yml` runs one test job over a 3-OS matrix — ubuntu-latest,
windows-latest, macos-15 (pinned: `macos-latest` re-points to macos-26 from 2026-06-15).
`cargo fmt --check` runs on ubuntu only; `cargo clippy --all-targets -- -D warnings` runs on
**every** leg — ubuntu alone would never compile the cfg(windows)/cfg(macos) paths; `cargo test`
on every leg (mock backend, no GPU). The Linux leg enforces a lying-green guard: fixture-suite
test counts must stay ≥ committed floors (amd_fixtures 19, intel_fixtures 23, nvml_stub_loader
2, enumerated via `cargo test --test <t> -- --list`) — **raise** the floor when adding fixture
tests; never lower it without a recorded reason. A manual-only `macos-probe` job
(workflow_dispatch) dumps `ioreg` and runs `examples/macos_probe.rs` for ground-truth fixture
capture.

Local cross-target checks (fast feedback, core crate only — `-history`/`-tui` cannot cross-check
here because libsqlite3-sys's bundled C build needs a target C toolchain):
`cargo check -p gpuviewer-core --target aarch64-apple-darwin` and
`--target x86_64-pc-windows-gnu`. Note CI/release use `-msvc`, so the matrix is authoritative.

## Packaging & release

Release = push tag `v<ver>` (must equal the workspace version — the draft job fails fast on
mismatch) → `.github/workflows/release.yml` builds a **draft** release (tar.gz/deb/rpm/AppImage/
zip + per-target `SHA256SUMS` + build-provenance attestations); `verify-assets` checks the 9
asset names and the owner publishes manually. cargo-dist was rejected.

Local repro: `cargo build --release --locked`; `cargo deb -p gpuviewer-tui --no-build`
(cargo-deb 3.7.0 — `-p` takes the package **name**); `cargo generate-rpm -p crates/tui` **from
the workspace root** (cargo-generate-rpm 0.21.0 — `-p` takes the package **directory** and asset
paths resolve against CWD); `tools/make-appimage.sh` (appimagetool SHA-pinned at 1.9.1). The
AppImage bundles neither glibc (Linux builds on ubuntu-22.04 to hold the glibc 2.35 floor) nor
`libnvidia-ml.so.1`; the target is **gnu, never musl** — musl cannot dlopen libnvidia-ml.
Icons: `python3 tools/make-icons.py` (stdlib-only, but requires a locally installed headless
Chrome/Chromium).

deb/rpm asset lists live in `crates/tui/Cargo.toml` metadata and deliberately declare **no**
NVIDIA package dependency — the dlopen'd `libnvidia-ml.so.1` is invisible to DT_NEEDED scanners
and degrading is the designed behavior; never add one. `crates/tui/build.rs` embeds the Windows
`.exe` icon via winresource, gated on env `CARGO_CFG_TARGET_OS` (build scripts compile for the
HOST); Linux→Windows cross-builds skip the embed by design. User-facing install docs:
`docs/packaging/installing.md`.
