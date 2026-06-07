# IOReport / Apple-telemetry fixtures

**These fixtures are SYNTHETIC** — hand-written for deterministic tests of the pure
parsing/maths layer in `crates/core/src/apple.rs` (`apple::parse`), not captured from real
hardware. They deliberately include decoy entries that a wrong code path would match
(`CPU Energy`, `GPU SRAM Energy`, a `GPUPH` under the wrong subgroup, `Device Utilization
at cur p-state`). Per the repo's fixture policy, replace/augment them with real captures
per chip and macOS release once hardware passes through the manual pre-release checklist.

Channel inventories, unit labels, and the residency/DVFS pairing model are based on
macmon's `sources.rs` (MIT) — the prior art named in `docs/design/cross-platform.md` §4.1.

## How to capture real fixtures

Run the one-off CI probe (manual `macos-probe` job, or locally on a Mac):

```sh
cargo run -p gpuviewer-core --example macos_probe
ioreg -r -c IOAccelerator -d 2     # Tier B PerformanceStatistics ground truth
```

The probe prints `channel|…` lines in exactly the format below — commit its output
verbatim as `channels-<chip>-<macos>.txt`. Residency (`state|…`) and `voltage-states9`
captures are part of the Tier B/C unfreeze (design §4.6), after the WWDC26 re-check.

## File formats (must stay line-compatible with `apple::parse` and the probe)

- `channels-*.txt` — one IOReport channel per line:
  `channel|<group>|<subgroup>|<name>|<unit>` (empty fields allowed; `|` has never been
  observed inside a name — if Apple ever ships one, the parser drops that line and the
  fixture must record the fact here).
- `gpuph-*.txt` — one GPUPH performance-state residency per line, as an **interval
  delta** (i.e. `IOReportCreateSamplesDelta` output, not cumulative counters):
  `state|<name>|<delta_ticks>`.
- `voltage-states9-*.hex` — the raw IOKit pmgr `voltage-states9` property blob as hex
  bytes (whitespace/`#`-comments ignored): little-endian `(u32 freq, u32 voltage)` rows.
  Frequency units have churned per chip family (Hz observed on M1-era; kHz elsewhere) —
  the decoder normalizes by magnitude, and each chip's fixture pins its unit.

## Current fixtures

- `channels-m2.txt` — single-die inventory (M1/M2-shaped): `GPU Energy` in **mJ** under
  `Energy Model`, `GPUPH` under `GPU Stats`/`GPU Performance States`, plus decoys.
- `channels-m2-ultra.txt` — Ultra two-die inventory: `DIE_0_`/`DIE_1_` prefixes on both
  the energy and GPUPH channels (contains-matching coverage).
- `channels-m4-nj.txt` — future-chip shape reporting **nJ** (unit must be read from the
  channel's own label, never assumed).
- `gpuph-m2.txt` — interval residencies: 1,000,000 total ticks, 600,000 on `OFF`
  → 40% active; P-state ticks pair with `voltage-states9-m2.hex` → 798 MHz weighted.
- `voltage-states9-m2.hex` — 5 DVFS rows in **Hz** plus the leading all-zero "off" row
  every observed chip ships (must be dropped, not read as 0 MHz).
- `voltage-states9-khz-synthetic.hex` — synthetic kHz + already-MHz rows exercising the
  magnitude-based unit normalization branches.
