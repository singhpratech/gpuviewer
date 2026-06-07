# gpuviewer NDJSON contract, version 1

## Scope

This document specifies what `gpuviewer --json` writes to stdout. The stream is
[NDJSON](https://github.com/ndjson/ndjson-spec): one JSON object per line, `\n`-terminated,
no enclosing array. It is the machine-readable face of the flight recorder — everything the
TUI shows comes from the same collection tick that produces these lines.

Each tick emits exactly one **frame** line, followed immediately by zero or more **event**
lines for the events that tick produced. With `--once`, gpuviewer prints one frame line plus
that tick's event lines, then exits 0. Diagnostics go to stderr only; stdout carries nothing
but contract lines.

Every line carries `"v": 1` (the contract version this document describes) and a `"type"`
discriminator (`"frame"` or `"event"`), so consumers route lines without guessing from field
shapes.

The machine-checkable companion to this document is
[`ndjson-v1.schema.json`](./ndjson-v1.schema.json) (JSON Schema draft 2020-12). The
conformance suite is `crates/tui/tests/ndjson_contract.rs`, which runs the built binary and
asserts the output against this contract.

## Honesty notes (read before charting anything)

- **Every metric is nullable.** `null` means "this source does not expose it" (driver
  `NOT_SUPPORTED`, missing sysfs file, privilege wall, MIG mode) — a normal outcome, not an
  error. Consumers must treat `null` as "unavailable", never as zero.
- **`util_pct` is duty-cycle, not saturation.** It is the fraction of time at least one
  kernel was resident on the device — a GPU at "100%" may be far from compute- or
  bandwidth-bound. Do not present it as capacity used.
- **The process list may be incomplete.** Listing other users' processes requires
  privileges on most platforms; on WSL2 per-process GPU attribution is unavailable at the
  driver level. The device's `process_hint` (TUI-side) explains such gaps; absence of a
  process from the array is not proof it isn't using the GPU.
- **Events are two-tier.** `"confidence": "fact"` events assert observed state transitions
  plainly. `"confidence": "likely"` events are inferences, always hedged with "likely" in
  the title, and always carry the raw numbers behind the claim in `evidence`.

## Frame line

One per collection tick. Example (wrapped for readability — the stream never wraps):

```json
{"v":1,"type":"frame","ts_ms":1780000000000,"devices":[
  {"id":"0000:01:00.0","name":"GeForce RTX 4090","mem_total_bytes":25769803776,
   "sample":{"ts_ms":1780000000000,"util_pct":97.2,"mem_used_bytes":21354232313,
             "power_mw":400176,"temp_c":74.1,"fan_pct":62.0,"sm_clock_mhz":2447,
             "mem_clock_mhz":10500,"encoder_pct":0.0,"decoder_pct":null,
             "throttle":{"thermal":false,"power_cap":false,"hw_slowdown":false,
                         "sync_boost":false,"other":false}},
   "processes":[{"pid":4521,"name":"python","kind":"compute","mem_bytes":20620229113,
                 "util_pct":96.0,"cpu_pct":null,"container":null}]}
]}
```

### Top-level fields

| field     | JSON type | nullable | semantics |
|-----------|-----------|----------|-----------|
| `v`       | integer   | no       | Contract version. `1` for everything in this document. |
| `type`    | string    | no       | `"frame"`. |
| `ts_ms`   | integer   | no       | Unix epoch milliseconds. One timestamp per collection frame — all devices in this line were sampled in the same tick. |
| `devices` | array of device objects | no | One entry per registered device, in stable order. May be empty if no device answered. |

### Device object

| field             | JSON type | nullable | semantics |
|-------------------|-----------|----------|-----------|
| `id`              | string    | no       | Stable device identity: PCI address (`0000:01:00.0`) for PCI devices; other devices use a prefixed key (e.g. `mock:0000:01:00.0`). Stable across ticks within a run. |
| `name`            | string    | no       | Marketing/product name as reported by the driver. |
| `mem_total_bytes` | integer   | yes      | Total VRAM in bytes, so consumers can compute used/total without a second query. `null` when the source does not expose it. |
| `sample`          | object    | yes      | This tick's metrics (below). `null` when the device failed to answer this tick — the device is still listed so consumers see the gap. |
| `processes`       | array of process objects | no | Processes attached to this device this tick. May be empty; may be incomplete (see honesty notes). |

### Sample object

| field            | JSON type | nullable | semantics |
|------------------|-----------|----------|-----------|
| `ts_ms`          | integer   | no       | When this device was sampled (epoch ms). May trail the frame's `ts_ms` by collection latency. |
| `util_pct`       | number    | yes      | Device utilization, 0–100. **Duty-cycle, not saturation** (see honesty notes). |
| `mem_used_bytes` | integer   | yes      | VRAM in use, bytes. |
| `power_mw`       | integer   | yes      | Board power draw, milliwatts. |
| `temp_c`         | number    | yes      | Primary (edge/hotspot per source) temperature, °C. |
| `fan_pct`        | number    | yes      | Fan speed, 0–100. `null` on fanless parts (iGPUs) and where unexposed. |
| `sm_clock_mhz`   | integer   | yes      | Shader/SM clock, MHz. |
| `mem_clock_mhz`  | integer   | yes      | Memory clock, MHz. |
| `encoder_pct`    | number    | yes      | Video encoder utilization, 0–100. |
| `decoder_pct`    | number    | yes      | Video decoder utilization, 0–100. |
| `throttle`       | object    | no       | Decoded throttle reasons; five booleans, all always present: `thermal`, `power_cap`, `hw_slowdown`, `sync_boost`, `other`. Unknown future driver bits land in `other`, never dropped. |

### Process object

| field       | JSON type | nullable | semantics |
|-------------|-----------|----------|-----------|
| `pid`       | integer   | no       | OS process id. |
| `name`      | string    | no       | Process name (comm); best-effort, may be a fallback like the pid when unreadable. |
| `kind`      | string    | no       | One of `"compute"`, `"graphics"`, `"both"`, `"unknown"`. |
| `mem_bytes` | integer   | yes      | Device memory attributed to this process, bytes. `null` where the driver cannot attribute it (e.g. WDDM, WSL2). |
| `util_pct`  | number    | yes      | Per-process GPU utilization, 0–100. Weak semantics under concurrency on some drivers — treat as indicative. |
| `cpu_pct`   | number    | yes      | Process CPU usage as % of one core (`100.0` = one full core; can exceed 100 on multithreaded processes). `null` when unknown. |
| `container` | string    | yes      | Container identity when the process runs in one, e.g. `"docker:1a2b3c4d5e6f"`. `null` for host processes or when unknown. |

## Event line

Zero or more per tick, each emitted immediately **after** the frame line of the tick that
produced it. Example:

```json
{"v":1,"type":"event","ts_ms":1780000000000,"device":"0000:01:00.0",
 "kind":"throttle_start","severity":"warning","confidence":"fact",
 "title":"GPU0 began throttling (thermal) — clocks 2520→1815 MHz",
 "evidence":"throttle bits: [thermal]; 84°C vs 84°C slowdown threshold"}
```

| field        | JSON type | nullable | semantics |
|--------------|-----------|----------|-----------|
| `v`          | integer   | no       | Contract version, `1`. |
| `type`       | string    | no       | `"event"`. |
| `ts_ms`      | integer   | no       | When the event was derived (epoch ms; the sample timestamp that triggered it). |
| `device`     | string    | no       | The `id` of the device this event belongs to (matches a frame device `id`). |
| `kind`       | string    | no       | Event kind, snake_case — full list below. |
| `severity`   | string    | no       | `"info"`, `"warning"`, or `"critical"`. |
| `confidence` | string    | no       | `"fact"` (observed state transition) or `"likely"` (inference, hedged in `title`). |
| `title`      | string    | no       | One-line human narration. Inference titles always contain "likely". |
| `evidence`   | string    | no       | The raw numbers behind the narration, always auditable. |

### `kind` values

Emitted today:

| kind               | confidence | meaning |
|--------------------|------------|---------|
| `throttle_start`   | fact       | A performance-limiting throttle reason became active (idle clock-down is not throttling). |
| `throttle_end`     | fact       | All throttle reasons cleared. "Recovered" is only claimed when clocks actually returned near pre-throttle levels. |
| `process_attached` | fact       | A new process appeared in a device's process list. Suppressed on the very first observation (those processes were already there). |
| `process_exited`   | fact       | A process left a device's process list. |
| `vram_pressure`    | likely     | VRAM usage is high and climbing; the title extrapolates a time-to-full. A linear extrapolation, hence an inference. |
| `idle_gap`         | likely     | The device sat idle after sustained activity while a large allocation stayed attached — likely a dataloader or checkpoint stall. |
| `collector_stall`  | fact       | gpuviewer's own collection loop fell behind its tick cadence; the recording has a hole that must not masquerade as device idleness. |
| `history_reset`    | fact       | Recorded history was truncated or restarted; the discontinuity is the recorder's, not the device's. |
| `hang_suspected`   | likely     | A device stopped answering queries while a workload was attached — possibly a hung kernel or driver. |
| `cpu_spillover`    | likely     | A GPU-attached process is burning CPU while the GPU sits idle — the classic CPU-bound dataloader. |
| `device_lost`      | fact       | A registered device stopped answering its dynamic probe for 5 consecutive ticks; the device stays in `devices` with `sample: null`. Only the silence is asserted — the cause (driver reset, unplug, library death) is not. Added additively under rules (b)/(c). |
| `device_returned`  | fact       | A device previously declared lost answered again. The samples between loss and return were never collected; that gap stays blank in history, never zero-filled. Added additively under rules (b)/(c). |
| `recording_started` | fact      | A recording session began folding history into the database — recorder lifecycle, not device behavior; only emitted when persistence is on. `evidence` carries the binary version, tick interval, backend names/device count, and database name. If the previous session never wrote its stop mark, the title says so: it ended uncleanly (crash, kill, or power loss) and the gap size is unknowable. Rides the first frame after startup. Added additively under rules (b)/(c). |
| `recording_stopped` | fact      | The recording session ended cleanly; time after this mark is gpuviewer not running, never the GPU sitting idle. Emission choice: this line is the stream's final event when stdout is still open at shutdown (`--once`, a fatal collector stop); after a consumer hangup the stream is already gone, so the mark is recording-only. A killed process writes no mark at all — the next `recording_started` narrates that. Added additively under rules (b)/(c). |

## Compatibility promise

These rules are the contract; the conformance suite enforces the shape, this section
defines what may change:

- (a) `"v"` bumps **only** on breaking change.
- (b) Additive fields may appear in any release without a bump.
- (c) Consumers **must** ignore unknown fields, unknown `"type"` values, and unknown
  `"kind"` values.
- (d) Field removals/renames/type changes never happen within a major `"v"`.

Corollary: a consumer written against this document keeps working, unmodified, for the
lifetime of `"v": 1`. New event kinds and new fields arrive silently; anything that would
break you arrives as `"v": 2` lines, which rule (c) tells you to skip until you upgrade.

## Consumer recipes

**Alert on events with jq** — frames are noise here; route by `type`:

```sh
gpuviewer --json | jq -r 'select(.type=="event") | "[\(.severity)] \(.title)"'
```

**A python loop** — note the contract-mandated tolerance of unknown shapes:

```python
import json, subprocess

proc = subprocess.Popen(["gpuviewer", "--json"], stdout=subprocess.PIPE, text=True)
for line in proc.stdout:
    msg = json.loads(line)
    if msg.get("v") != 1:
        continue  # rule (c): skip versions you don't speak
    if msg.get("type") == "frame":
        for dev in msg["devices"]:
            util = (dev.get("sample") or {}).get("util_pct")  # nullable, like everything
            print(f'{dev["id"]}: util={util}')
    elif msg.get("type") == "event":
        print(f'{msg["severity"]}: {msg["title"]}')
    # unknown "type": ignore, per the compatibility promise
```

**Poor man's flight recorder** — NDJSON is append-friendly, so a file is a replayable log:

```sh
gpuviewer --json >> gpu-$(date +%F).ndjson
# later: what throttled overnight?
jq -r 'select(.kind=="throttle_start") | .title' gpu-2026-06-06.ndjson
```
