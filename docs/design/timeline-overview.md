# Timeline Overview — Design Note

**Date: 2026-06-07. Status: design, not yet built.** This is the zoomed-out timeline view
over the 1m rollup tier that already exists in the store. Companion to
`docs/research/06-production-platform-deepdive.md`; consistent with the CLAUDE.md punt
list (no daemon, no GUI, no exporter — this is a TUI view over data we already record).

## 1. Why

The replay view answers "show me 02:14" — a ±5 min window of 10s rollups around a cursor
(`REPLAY_HALF_WINDOW_MS = 300_000`, `app.rs:20`), reached by `r` or by Enter on an event.
What it cannot answer is **"show me the night"**: there is no way to *see* eight hours at
once and spot the shape — the throttle plateau, the VRAM staircase, the 3am gap — before
diving in. Today the user scrubs blind in 5-minute pages (`PgUp`/`PgDn`) or jumps
event-to-event, which only works if an event fired.

Meanwhile the data for the wide view is already on disk and already maintained: the
`samples_1m` tier is written by the `Recorder` on every flush, retained for 30 days
(`RETAIN_1M_MS`, `store.rs:30`), and read today by exactly one consumer — the `report`
digest (`main.rs:530,569`). The TUI never touches `Tier::OneMin`. The overview is the
first interactive consumer of a tier we have been paying for since persistence landed.

This is a navigation layer, not a new recording capability: nothing new is collected,
nothing new is stored.

## 2. UX

A third presentation mode alongside Live and Replay: **Overview** — one device (the
selected tab, same as everywhere else), full-width, three stacked bands:

```
┌ util avg/max 61% TIMELINE ──────────────────────────────────────────────┐
│  ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▔▔▔▔▔▔▔▔▔        ▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄     │   square-wave, 1m buckets
└ 22:00 ───────────────────── 02:14 cursor ─────────────────────── 06:00 ┘
┌ vram 18.2 GiB / 24.0 GiB TIMELINE ──────────────────────────────────────┐
│  ▁▁▁▂▂▃▃▄▄▅▅▆▆▇▇▇▇▇▇▇▇▇▇▇▇            ▇▇▇▇▇▇▇▇▇▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁     │
└──────────────────────────────────────────────────────────────────────── ┘
   ·      W           W  ·church                · ·         W              ← event markers
 REPLAY-style footer: TIMELINE 02:14:31 · window 8h (1m rollups) · …
```

- The **cursor is centered**, exactly like replay's x-axis (`-5m / cursor / +5m` becomes
  `-4h / cursor / +4h`): the same mental model, wider lens.
- The **event marker row** places one glyph per event at its timestamp, colored by
  severity like the story feed; `↑`/`↓` walk the markers (selection shown in the story
  pane below, which stays — it is the same `events_between` slice the markers come from).
- **Enter is the bridge**: Enter on a selected event, or with no selection on the cursor
  position, drops into the existing ±5 min replay at that moment — the overview finds the
  moment, replay explains it. Esc from that replay returns to the overview (not to live),
  so the find→inspect→back loop is one keystroke each way.
- **Zoom**: a fixed ladder of window widths — 1h · 3h · 8h · 24h · 72h (default 8h, the
  "last night" width). Windows ≤ 2h render from the 10s tier; wider windows from the 1m
  tier. The active tier and width are always printed in the footer, never inferred.
- The process table is **not** shown in overview (per-process detail is replay's job;
  pretending a 1m process rollup is "what was running" across an 8h window would be
  misleading). The pane is given to the taller charts.
- Entry points: `t` from Live (anchored at the newest recorded bucket) or from Replay
  (anchored at the replay cursor — "zoom out from here"). `gpuviewer view file.gpvr`
  sessions get the overview too, with the same pinning rule as replay: never a fake
  "live" behind a file (`view_file` check, as in `app.rs:215-221`).

## 3. Keybinds

No collisions with the existing maps (`t`, `+`/`-`, `End` are unbound today in both
modes; README keybind tables gain one section).

| Overview | |
|---|---|
| `t` (from Live or Replay) | enter overview, anchored at context |
| `t` / `Esc` | back to the mode you came from (file viewer: back to replay, never live) |
| `q` | quit (as everywhere) |
| `←` `→` | move cursor one bucket of the active tier (1m or 10s) |
| `PgUp` / `PgDn` | move cursor half a window |
| `Home` / `End` | oldest / newest recorded bucket |
| `+` / `-` | zoom in / out through the width ladder (cursor stays put) |
| `↑` `↓` | walk event markers |
| `Enter` | drop into ±5 min replay at the selected event / cursor |
| `Tab` / `Shift-Tab` | switch device |

Consistency fix to take while here: replay has `Home` but not `End` (`app.rs:227-231`);
add `End` to replay for symmetry.

## 4. Data path

The store already has nearly everything; this section is exact about names.

**Exists — reused unchanged:**

| Need | Query | Where |
|---|---|---|
| rollups across the window | `SqliteStore::samples_between(id, from, to, Tier::OneMin)` — and `Tier::TenSec` for ≤2h windows | `store.rs:438`; 1m tier read today only by `report` (`main.rs:530,569`) |
| events for markers + story pane | `SqliteStore::events_between(from, to)` | `store.rs:464` |
| clamp range / Home / End | `SqliteStore::earliest_bucket_ms()` / `latest_bucket_ms()` (already span both tiers) | `store.rs:575,580` |
| anchor for `t` from Live | `latest_bucket_ms()`; the demo could anchor on `latest_event_ms(Some(ThrottleStart))` as it does for replay | `store.rs:559` |
| read connection | the lazily-opened second read-only WAL connection replay already uses (`App::store`) | `app.rs:73-76,262-271` |

The query pattern is the proven replay one: an `OverviewWindow` cache struct (mirroring
`ReplayWindow`, `app.rs:37-46`) re-queried per seek/zoom. Cost check: the worst ladder
step is 72h of 1m buckets = 4,320 rows/device — two orders of magnitude under what
`report` already reads happily; indexed range reads per keypress are fine, same as
replay's per-keypress re-query (`app.rs:291-293`).

**Needs adding:**

- **Nothing in the store for the feature as scoped.** The one candidate —
  `samples_decimated(id, from, to, tier, max_points)` (GROUP BY `bucket_ms / k` in SQL) —
  is only warranted if a future ladder step goes to weeks (30d of 1m = 43,200 rows) or
  per-keypress profiling on real data says so. Decimating in the app needs no schema or
  API change and is the default plan. Do not build it speculatively.
- In the TUI: `Mode::Overview` (the `Mode` enum and every `match` on it), the
  `OverviewWindow` cache, the came-from memory (`Live` vs `Replay`) for `Esc`, and the
  zoom ladder state.

## 5. Rendering — on the step_segments square wave

The square-wave outline chart just landed (`64cab97` — "braille step traces, not solid
fill") and is exactly the right primitive, because it was built tier-aware:
`step_segments(pts, trail_s)` (`ui.rs:312`) holds each value flat until the next sample
and steps vertically, and the caller passes the sample's real width as `trail_s` — live
passes `1.0`, replay passes `Tier::TenSec.width_ms()/1000` (`ui.rs:230-231`). The overview
passes `60.0` for the 1m tier and gets honest bucket-wide treads for free.

One generalization is required: **`GAP_BRIDGE_S = 15.0` (`ui.rs:305`) must become a
parameter.** It is tuned for 10s buckets and the max idle-backoff live cadence; at the 1m
tier every adjacent pair of buckets is 60s apart and the whole trace would degenerate into
disconnected dots. Rule: `gap_bridge_s = 1.5 × bucket width` (15s for 10s buckets — the
current behavior, unchanged; 90s for 1m buckets), so a single missing bucket breaks the
trace at every tier, identically. This touches `step_segments`'s signature and its two
existing callers plus tests — small, mechanical.

Per band:

- **util**: two datasets per the rollup's stored aggregates — `util_max` in a dim style
  underneath, `util_avg` in the bright style on top (both square-wave). A 1m average
  flattens spikes (a 30s 100% burst in an idle minute reads ~50%); drawing max behind avg
  keeps bursts visible without lying about the mean. Title says `util avg/max` explicitly.
- **vram**: `mem_avg` via the existing shared `render_vram_chart` (`ui.rs:353`), which
  already takes per-mode context (`VramChartCtx`) — add the overview's bounds/tag/width;
  y-axis 0..`mem_total` as today.
- **markers**: a 1-row `Line` of glyphs positioned by `(ts - from) / window × width`,
  severity-colored, selected marker bold/reversed. Collisions (two events in one cell)
  render the higher severity — the story pane below carries the full list.
- **headline values**: from the exact bucket at the cursor via the `bucket_at` pattern
  (`ui.rs:338`) generalized to take a tier — a cursor in a gap shows "—" on every
  headline, never a neighboring bucket's values.

## 6. Honesty rules

Same contract as replay, restated for the wider lens because wide views are where lying
is easiest:

- **A hole in the recording renders as a hole.** No bridging across missing buckets at any
  zoom (the per-tier gap rule above). A night where the recorder wasn't running is *blank*,
  not flat — and the overview will make the always-on gap visible at a glance, which is an
  argument for, not against, shipping it before the session-boundary events land
  (deep-dive §1.2 blocker 1: those holes are currently un-narrated; the overview at least
  stops them being invisible).
- **Recorded data is always tagged.** Every chart title carries ` TIMELINE` exactly as
  replay titles carry ` REPLAY` (`ui.rs:246,280`), and the footer leads with
  `TIMELINE <cursor clock>` plus the window width and active tier — mirroring replay's
  `(10s rollups, 48h retention)` disclosure (`ui.rs:730-733`) with
  `(1m rollups, 30d retention)` / `(10s rollups, 48h retention)` per zoom step. Recorded
  data must never be mistakable for live at any zoom.
- **Mock stays labeled.** The footer's `(mock data)` rule is keyed off `shared.mock` and
  already covers replays of mock recordings (`ui.rs:714-718`, with the regression test at
  `:824`); the overview footer uses the same tag through the same path. File-viewer
  sessions keep the existing rule: provenance unstated, so no mock/live claim either way.
- **Averages are labeled as averages** (`util avg/max` in the title), and the 10s↔1m tier
  switch is printed, never silent — a user comparing a 1h view to an 8h view of the same
  hour will see different shapes, and the footer says why.
- **Absent metrics render "—"** on the cursor headline, per the exact-bucket rule — the
  `GaugeValues`/`Option` discipline (`ui.rs:393-404`) carries over unchanged.

## 7. Deliberately out of scope

- **No new collection, storage, or retention changes.** The overview renders the existing
  tiers; it does not justify new ones. (No raw-1Hz zoom: 10s is the recorded floor by
  design — never write raw samples to SQLite, CLAUDE.md.)
- **No cross-device overlay.** One device per tab, like every other pane. A multi-GPU
  combined lane is a different feature with real design questions (alignment, scale).
- **No event filtering/search**, no marker tooltips, no annotation/editing of history.
  The story pane is the event surface; filtering is a story-feed feature if it's anything.
- **No NDJSON or store schema changes.** This is purely a TUI presentation; `--json`
  consumers already get frames + events and can build their own wide views.
- **No GUI work.** The v2 iced GUI reuses `samples_between`/`events_between` — landing the
  query-and-cache pattern now is the prep work, the widget is not.
- **Not a fix for the always-on blockers.** The overview *exposes* recording holes; the
  instance lock, session-boundary events, and unit file (deep-dive §5) are separate,
  prior work.

## 8. Effort by file

Small feature, one new mode, no new dependencies. In review order:

| File | Work | Size |
|---|---|---|
| `crates/tui/src/ui.rs` | `gap_bridge` parameter on `step_segments` (+ its two callers, + tests); `render_overview` (two chart bands via existing helpers, marker row, footer arm); tier-aware `bucket_at` | ~150–200 lines, the marker row is the only genuinely new widget code |
| `crates/tui/src/app.rs` | `Mode::Overview` + came-from memory; `OverviewWindow` cache + seek/zoom/clamp (mirrors `seek`/`scrub`/`recorded_range`); key dispatch (`handle_key_overview`); `t` entries from both modes; `End` in replay; tests mirroring the existing seeded-store replay tests (`seed_store` pattern, `app.rs:465-489`, seeds the 1m tier already — the Recorder folds both tiers) | ~200–250 lines incl. tests |
| `crates/history/src/store.rs` | **nothing** (the audit point: every needed query exists) | 0 |
| `crates/tui/src/main.rs` | none required; optional later: `demo` opening in overview instead of replay — defer, the demo's current "answer first" framing is the point | 0 |
| `README.md` | overview keybind table section + one line under Scroll-back replay | ~15 lines |

Riskiest part: none of it is algorithmically hard; the care points are the honesty rules
(gap parameter, tier labeling, exact-bucket headlines) and the mode-transition matrix
(Live/Replay/Overview × viewer-pinned), which is why the came-from memory and the viewer
pinning rule get unit tests like `quit_and_escape_semantics_per_mode` and
`viewer_pins_replay_and_never_drops_to_live` (`app.rs:626,669`) before any rendering work.
