//! TUI event loop and app state: the live dashboard, the scroll-back replay mode —
//! the product's headline feature ("scroll back to 02:14 — it'll tell you why") — and
//! the timeline overview, the zoomed-out altitude above replay (hours per screen,
//! Enter drills back down).

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use gpuviewer_core::{now_ms, Event};
use gpuviewer_history::{ProcessRollup, SampleRollup, SqliteStore, Tier};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::{TerminalOptions, Viewport};

use crate::collector::Collector;

/// Viewport assumed when the terminal reports a 0×0 size (bare ptys: `script`, some CI).
const FALLBACK_SIZE: (u16, u16) = (80, 24);

/// Half-width of the replay window queried around the cursor: ±5 min of 10s rollups.
const REPLAY_HALF_WINDOW_MS: u64 = 300_000;
/// Left/Right scrub step in replay.
const SCRUB_STEP_MS: u64 = 10_000;
/// PgUp/PgDn scrub step in replay.
const SCRUB_PAGE_MS: u64 = 300_000;
/// PgUp/PgDn jump for the timeline cursor, in rendered columns.
const TIMELINE_PAGE_COLS: u64 = 10;

const HOUR_MS: u64 = 60 * 60 * 1000;

/// One rung of the timeline zoom ladder: the window it spans and the rollup tier that
/// feeds it. The 1h rung reads the 10s tier; everything wider reads the 1m tier (a 3h
/// window already holds more 10s buckets than any terminal has columns).
pub struct TimelineZoom {
    pub label: &'static str,
    pub span_ms: u64,
    pub tier: Tier,
}

/// '-' walks down this ladder (wider window), '+' climbs back up (narrower).
pub const TIMELINE_ZOOMS: [TimelineZoom; 7] = [
    TimelineZoom {
        label: "1h",
        span_ms: HOUR_MS,
        tier: Tier::TenSec,
    },
    TimelineZoom {
        label: "3h",
        span_ms: 3 * HOUR_MS,
        tier: Tier::OneMin,
    },
    TimelineZoom {
        label: "6h",
        span_ms: 6 * HOUR_MS,
        tier: Tier::OneMin,
    },
    TimelineZoom {
        label: "12h",
        span_ms: 12 * HOUR_MS,
        tier: Tier::OneMin,
    },
    TimelineZoom {
        label: "24h",
        span_ms: 24 * HOUR_MS,
        tier: Tier::OneMin,
    },
    TimelineZoom {
        label: "48h",
        span_ms: 48 * HOUR_MS,
        tier: Tier::OneMin,
    },
    TimelineZoom {
        label: "7d",
        span_ms: 7 * 24 * HOUR_MS,
        tier: Tier::OneMin,
    },
];

/// The rung the timeline opens at: 6h — wide enough to frame "the run stalled hours
/// ago", narrow enough that one column is still a couple of minutes.
pub const TIMELINE_DEFAULT_ZOOM: usize = 2;

/// Which timeline the dashboard shows: the live tick stream, a recorded window scrolled
/// back to [`App::cursor_ms`], or the zoomed-out timeline overview above both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Live,
    Replay,
    /// Hours of recorded history per screen, with a column cursor that Enter drills
    /// back down into replay — the core loop is overview → drill → replay → live.
    Timeline,
}

/// How the live/replay charts paint the square wave: a braille step outline (the
/// shipped default) or the filled half-block silhouette. Both walk the same
/// hold-then-step points with the same gap-breaking — strictly a paint choice, cycled
/// with 's'. The timeline overview ignores it: its strips are always solid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartStyle {
    Braille,
    Solid,
}

impl ChartStyle {
    /// The other style — 's' cycles between the two.
    pub fn toggled(self) -> Self {
        match self {
            ChartStyle::Braille => ChartStyle::Solid,
            ChartStyle::Solid => ChartStyle::Braille,
        }
    }
}

/// One queried replay window, cached between seeks so rendering never touches SQLite.
/// `samples`/`processes` are indexed like `Shared::infos`, so the device tabs keep working
/// unchanged in replay.
pub struct ReplayWindow {
    pub from_ms: u64,
    pub to_ms: u64,
    /// 10s-tier rollups per device across `[from_ms, to_ms]`, oldest first.
    pub samples: Vec<Vec<SampleRollup>>,
    /// Per-device process rollups for the bucket at the cursor.
    pub processes: Vec<Vec<ProcessRollup>>,
    /// Events in the window, oldest first (the store's order).
    pub events: Vec<Event>,
}

/// One queried timeline-overview window, cached like [`ReplayWindow`] so rendering never
/// touches SQLite. Holds the raw rollup rows: bucket→column aggregation happens at render
/// time because it depends on the terminal width.
pub struct TimelineWindow {
    pub from_ms: u64,
    pub to_ms: u64,
    /// The rollup tier the window was queried at (10s for the 1h zoom, else 1m).
    pub tier: Tier,
    /// True when `from_ms` was pulled in to the start of recorded history — the requested
    /// zoom is wider than the recording, and the UI must show the actual span.
    pub clamped: bool,
    /// Rollups per device across `[from_ms, to_ms]`, oldest first; indexed like
    /// `Shared::infos`.
    pub samples: Vec<Vec<SampleRollup>>,
    /// Events in the window, oldest first — all devices, like the story feed (event
    /// titles name their GPU).
    pub events: Vec<Event>,
}

/// What a key press asks the event loop to do. Returned instead of acted on inline so the
/// whole key dispatch — including "q quits from both modes, Esc only from live" — is
/// unit-testable without a terminal.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    Quit,
    Continue,
}

pub struct App {
    pub selected: usize,
    pub mode: Mode,
    /// Replay cursor in unix millis; meaningful only in `Mode::Replay`.
    pub cursor_ms: u64,
    /// The selected story-feed row, shared by both modes: in Live it indexes the
    /// newest-first feed, in Replay the window's chronological event list.
    pub story_selected: Option<usize>,
    /// True after a replay key was pressed with no history store available — the footer
    /// explains why nothing happened instead of silently swallowing the key.
    pub replay_hint: bool,
    /// How the live/replay charts are painted; 's' cycles it.
    pub chart_style: ChartStyle,
    /// Timeline cursor in unix millis; meaningful only in `Mode::Timeline`.
    pub timeline_cursor_ms: u64,
    /// Current rung on [`TIMELINE_ZOOMS`].
    pub timeline_zoom: usize,
    /// Columns the timeline strips spanned at the last draw — written by the event loop
    /// before each draw so the cursor keys step exactly one rendered column.
    pub timeline_cols: usize,
    /// The recording file this session is pinned to (`gpuviewer view`); `Some` means there
    /// is no live source behind the UI, so the app never leaves replay.
    view_file: Option<String>,
    /// Cached replay window, re-queried on every seek.
    replay: Option<ReplayWindow>,
    /// Cached timeline window, re-queried on entry, zoom changes, and — in a live
    /// session — every collector tick (see [`Self::timeline_tick`]).
    timeline: Option<TimelineWindow>,
    /// `Shared::tick_seq` at the moment the cached timeline window was built. The event
    /// loop re-queries the window once the collector has folded a newer frame, so a live
    /// overview's right edge keeps tracking "now" instead of silently freezing at the
    /// instant 't' was pressed.
    timeline_built_seq: u64,
    /// Second, read-only store connection (WAL lets it read while the collector's writer
    /// keeps appending). Opened lazily on the first replay entry; stays `None` when
    /// persistence is off or the store cannot be opened.
    store: Option<SqliteStore>,
    collector: Collector,
}

impl App {
    pub fn new(collector: Collector) -> Self {
        Self {
            selected: 0,
            mode: Mode::Live,
            cursor_ms: 0,
            story_selected: None,
            replay_hint: false,
            chart_style: ChartStyle::Braille,
            timeline_cursor_ms: 0,
            timeline_zoom: TIMELINE_DEFAULT_ZOOM,
            timeline_cols: (FALLBACK_SIZE.0 - 2) as usize,
            view_file: None,
            replay: None,
            timeline: None,
            timeline_built_seq: 0,
            store: None,
            collector,
        }
    }

    /// An app pinned to a recording file (`gpuviewer view FILE.gpvr`): starts in replay at
    /// the file's newest event (or its newest bucket when it holds none) and never drops to
    /// Live — there is no collector ticking and no live source, only the file. Esc/'r' are
    /// inert; 'q' quits. Pair with [`Collector::stationary`].
    pub fn viewer(collector: Collector, store: SqliteStore, file: String) -> Self {
        let mut app = Self::new(collector);
        let anchor = store.latest_event_ms(None).ok().flatten();
        app.store = Some(store);
        app.view_file = Some(file);
        app.enter_replay(anchor);
        app
    }

    /// The pinned recording's display name (`gpuviewer view`), `None` in a live session.
    pub fn view_file(&self) -> Option<&str> {
        self.view_file.as_deref()
    }

    pub fn paused(&self) -> bool {
        self.collector.paused.load(Ordering::Relaxed)
    }

    /// The cached replay window when in replay mode. Rendering reads through here so a
    /// stale cache can never leak into the live view.
    pub fn replay_window(&self) -> Option<&ReplayWindow> {
        match self.mode {
            Mode::Replay => self.replay.as_ref(),
            Mode::Live | Mode::Timeline => None,
        }
    }

    /// The cached timeline window when in timeline mode — the same staleness guard as
    /// [`Self::replay_window`].
    pub fn timeline_window(&self) -> Option<&TimelineWindow> {
        match self.mode {
            Mode::Timeline => self.timeline.as_ref(),
            Mode::Live | Mode::Replay => None,
        }
    }

    pub fn run(mut self) -> Result<()> {
        // A bare pty (e.g. `script` captures) reports a 0×0 size; the fullscreen viewport
        // would autoresize to an empty buffer and draw nothing. Pin a fixed 80×24 viewport
        // instead — `Viewport::Fixed` is never autoresized, and a pty that can't report a
        // size won't deliver meaningful resize events either. Real terminals report a
        // nonzero size and keep the normal resize-tracking fullscreen path.
        let zero_sized = ratatui::crossterm::terminal::size()
            .map(|(w, h)| w == 0 || h == 0)
            .unwrap_or(false);
        let mut terminal = if zero_sized {
            let (w, h) = FALLBACK_SIZE;
            ratatui::init_with_options(TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, w, h)),
            })
        } else {
            ratatui::init()
        };
        let res = self.event_loop(&mut terminal);
        ratatui::restore();
        // A detached collector thread would be killed mid-recording when main returns —
        // losing the partial rollup tail AND the session's `recording_stopped` mark, so
        // every TUI quit would read as a crash to the next session. Stop it cooperatively
        // (bounded — see `Collector::shutdown`) after the terminal is restored, so the
        // screen is back even if the wait runs its full grace.
        self.collector.shutdown();
        res
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            // Re-query the timeline overview if the collector ticked since it was built
            // — a stale window presented as "ending now" would be a quiet lie.
            self.timeline_tick();
            let width = {
                let shared = self.collector.shared.lock().unwrap();
                let n = shared.infos.len();
                if n > 0 && self.selected >= n {
                    self.selected = n - 1;
                }
                terminal
                    .draw(|f| crate::ui::render(f, self, &shared))?
                    .area
                    .width
            };
            // The timeline cursor steps by one rendered column, so the key handler needs
            // the width the strips were actually drawn at.
            self.timeline_cols = crate::ui::timeline_strip_cols(width);

            if event::poll(Duration::from_millis(250))? {
                if let TermEvent::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if self.handle_key(key.code) == KeyOutcome::Quit {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Dispatch one key press according to the current mode.
    pub fn handle_key(&mut self, code: KeyCode) -> KeyOutcome {
        match self.mode {
            Mode::Live => self.handle_key_live(code),
            Mode::Replay => self.handle_key_replay(code),
            Mode::Timeline => self.handle_key_timeline(code),
        }
    }

    fn handle_key_live(&mut self, code: KeyCode) -> KeyOutcome {
        let n = self.collector.shared.lock().unwrap().infos.len().max(1);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return KeyOutcome::Quit,
            KeyCode::Tab | KeyCode::Right => self.selected = (self.selected + 1) % n,
            KeyCode::Left | KeyCode::BackTab => self.selected = (self.selected + n - 1) % n,
            KeyCode::Char('p') => {
                let p = &self.collector.paused;
                p.store(!p.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            KeyCode::Up => self.move_story_selection(-1),
            KeyCode::Down => self.move_story_selection(1),
            KeyCode::Enter => {
                // Enter on a selected event scrolls back to it. The feed displays
                // newest-first, so the visual index maps back from the end of the log.
                let ts = self.story_selected.and_then(|i| {
                    let sh = self.collector.shared.lock().unwrap();
                    let evs = sh.history.events();
                    evs.len().checked_sub(1 + i).map(|j| evs[j].ts_ms)
                });
                if let Some(ts) = ts {
                    self.enter_replay(Some(ts));
                }
            }
            KeyCode::Char('r') => self.enter_replay(None),
            KeyCode::Char('t') => self.enter_timeline(),
            KeyCode::Char('s') => self.chart_style = self.chart_style.toggled(),
            _ => {}
        }
        KeyOutcome::Continue
    }

    fn handle_key_replay(&mut self, code: KeyCode) -> KeyOutcome {
        let n = self.collector.shared.lock().unwrap().infos.len().max(1);
        match code {
            // q always quits; Esc must NOT — in replay it returns to the live view.
            KeyCode::Char('q') => return KeyOutcome::Quit,
            KeyCode::Esc | KeyCode::Char('r') => {
                // Inert in the file viewer: there is no live view behind a recording to
                // return to, and swapping in an empty "live" pane would be a lie.
                if self.view_file.is_none() {
                    self.mode = Mode::Live;
                    self.story_selected = None;
                }
            }
            KeyCode::Left => self.scrub(-(SCRUB_STEP_MS as i64)),
            KeyCode::Right => self.scrub(SCRUB_STEP_MS as i64),
            KeyCode::PageUp => self.scrub(-(SCRUB_PAGE_MS as i64)),
            KeyCode::PageDown => self.scrub(SCRUB_PAGE_MS as i64),
            KeyCode::Home => {
                if let Some((earliest, _)) = self.recorded_range() {
                    self.seek(earliest);
                }
            }
            KeyCode::Tab => self.selected = (self.selected + 1) % n,
            KeyCode::BackTab => self.selected = (self.selected + n - 1) % n,
            KeyCode::Up => self.move_story_selection(-1),
            KeyCode::Down => self.move_story_selection(1),
            KeyCode::Enter => {
                // Re-anchor the cursor to the selected event, then keep the selection
                // pointing at that same event inside the re-centered window.
                let ts = self
                    .story_selected
                    .zip(self.replay.as_ref())
                    .and_then(|(i, w)| w.events.get(i).map(|e| e.ts_ms));
                if let Some(ts) = ts {
                    self.seek(ts);
                    self.story_selected = self
                        .replay
                        .as_ref()
                        .and_then(|w| w.events.iter().position(|e| e.ts_ms == ts));
                }
            }
            KeyCode::Char('t') => self.enter_timeline(),
            KeyCode::Char('s') => self.chart_style = self.chart_style.toggled(),
            _ => {}
        }
        KeyOutcome::Continue
    }

    /// Timeline keys: the zoom ladder on '+'/'-', a column cursor on the arrows, Enter to
    /// drill down into replay at the cursor — overview → drill → replay → live.
    fn handle_key_timeline(&mut self, code: KeyCode) -> KeyOutcome {
        let n = self.collector.shared.lock().unwrap().infos.len().max(1);
        match code {
            // q always quits; Esc/'t' climb back down the altitude ladder — to the pinned
            // replay in the file viewer (there is no live view behind a recording), to
            // the live view otherwise.
            KeyCode::Char('q') => return KeyOutcome::Quit,
            KeyCode::Char('t') | KeyCode::Esc => {
                self.mode = if self.view_file.is_some() {
                    Mode::Replay
                } else {
                    Mode::Live
                };
            }
            // '=' is unshifted '+' on common layouts; accept both.
            KeyCode::Char('+') | KeyCode::Char('=') => self.timeline_rezoom(-1),
            KeyCode::Char('-') => self.timeline_rezoom(1),
            KeyCode::Left => self.timeline_step(-1),
            KeyCode::Right => self.timeline_step(1),
            KeyCode::PageUp => self.timeline_step(-(TIMELINE_PAGE_COLS as i64)),
            KeyCode::PageDown => self.timeline_step(TIMELINE_PAGE_COLS as i64),
            KeyCode::Home => {
                if let Some(w) = &self.timeline {
                    self.timeline_cursor_ms = w.from_ms;
                }
            }
            KeyCode::End => {
                if let Some(w) = &self.timeline {
                    self.timeline_cursor_ms = w.to_ms;
                }
            }
            KeyCode::Tab => self.selected = (self.selected + 1) % n,
            KeyCode::BackTab => self.selected = (self.selected + n - 1) % n,
            KeyCode::Enter => self.enter_replay(Some(self.timeline_cursor_ms)),
            _ => {}
        }
        KeyOutcome::Continue
    }

    /// Enter replay at `cursor` (an event's ts), or at the newest recorded bucket when
    /// `None` ('r'). Public because `gpuviewer demo` opens the session already scrolled
    /// back to the last throttle onset — the user's first sight is the answer to "why did
    /// it slow down", not a live gauge. With no store available (persistence disabled, or
    /// the database unreadable) this sets the footer hint and stays live — a replay view
    /// with no recording behind it would be an empty lie.
    pub fn enter_replay(&mut self, cursor: Option<u64>) {
        if !self.ensure_store() {
            return;
        }
        // An empty store still enters replay (anchored at "now"): blank charts plus a
        // cursor time read more honestly than a key that silently does nothing.
        let anchor = cursor
            .or_else(|| self.recorded_range().map(|(_, latest)| latest))
            .unwrap_or_else(now_ms);
        self.mode = Mode::Replay;
        self.replay_hint = false;
        self.story_selected = None;
        self.seek(anchor);
        // When entering on an event, keep the selection anchored to it in the new window.
        if let Some(ts) = cursor {
            self.story_selected = self
                .replay
                .as_ref()
                .and_then(|w| w.events.iter().position(|e| e.ts_ms == ts));
        }
    }

    /// Enter the timeline overview ('t') from live or replay. Coming from replay the
    /// cursor carries over, so the overview frames where you already were; from live it
    /// starts at the window's newest edge. Needs the same store replay does — without one
    /// the footer hint explains the dead key.
    pub fn enter_timeline(&mut self) {
        if !self.ensure_store() {
            return;
        }
        let anchor = (self.mode == Mode::Replay).then_some(self.cursor_ms);
        self.mode = Mode::Timeline;
        self.replay_hint = false;
        self.story_selected = None;
        self.timeline_refresh(anchor);
    }

    /// Open the lazy read-only store connection on first use. `false` (with the footer
    /// hint set) when persistence is off or the database is unreadable.
    fn ensure_store(&mut self) -> bool {
        if self.store.is_some() {
            return true;
        }
        let db_path = self.collector.shared.lock().unwrap().db_path.clone();
        match db_path.and_then(|p| SqliteStore::open_readonly(p).ok()) {
            Some(s) => {
                self.store = Some(s);
                true
            }
            None => {
                self.replay_hint = true;
                false
            }
        }
    }

    /// (Re)query the timeline window at the current zoom. The window ends at "now" in a
    /// live session but at the end of recorded coverage in the file viewer
    /// ([`Self::recorded_end`]) — a recording has no "now", and hours of empty future
    /// would squeeze the data into the left margin. The start clamps to the oldest
    /// recorded bucket so a recording younger than the zoom shows its actual span
    /// (flagged via `clamped`), then aligns down to the tier width so the inclusive
    /// bucket query and the column mapping agree on the window's first bucket. `cursor`
    /// keeps an existing cursor where the new window still contains it; `None` lands on
    /// the newest edge.
    fn timeline_refresh(&mut self, cursor: Option<u64>) {
        let zoom = &TIMELINE_ZOOMS[self.timeline_zoom];
        let to_ms = match (self.view_file.is_some(), self.recorded_end()) {
            (true, Some(end)) => end,
            _ => now_ms(),
        };
        let mut from_ms = to_ms.saturating_sub(zoom.span_ms);
        let mut clamped = false;
        if let Some((earliest, _)) = self.recorded_range() {
            if earliest > from_ms {
                from_ms = earliest;
                clamped = true;
            }
        }
        from_ms -= from_ms % zoom.tier.width_ms();

        let (ids, seq) = {
            let sh = self.collector.shared.lock().unwrap();
            let ids: Vec<_> = sh.infos.iter().map(|i| i.id.clone()).collect();
            (ids, sh.tick_seq)
        };
        let Some(store) = &self.store else { return };
        let samples = ids
            .iter()
            .map(|id| {
                store
                    .samples_between(id, from_ms, to_ms, zoom.tier)
                    .unwrap_or_default()
            })
            .collect();
        let events = store.events_between(from_ms, to_ms).unwrap_or_default();
        self.timeline_cursor_ms = cursor.unwrap_or(to_ms).clamp(from_ms, to_ms);
        self.timeline_built_seq = seq;
        self.timeline = Some(TimelineWindow {
            from_ms,
            to_ms,
            tier: zoom.tier,
            clamped,
            samples,
            events,
        });
    }

    /// Re-query the cached timeline window when the collector has folded a new frame
    /// since it was built, so the live overview's right edge keeps tracking "now" and
    /// rollups recorded after entry keep arriving on screen. Called once per event-loop
    /// pass — a u64 compare unless a tick actually landed, so rendering stays off
    /// SQLite. The file viewer never refreshes: a recording does not grow. A cursor
    /// sitting on the newest edge rides it; anywhere else it keeps its wall-clock
    /// position (clamped, like every refresh).
    fn timeline_tick(&mut self) {
        if self.mode != Mode::Timeline || self.view_file.is_some() {
            return;
        }
        let seq = self.collector.shared.lock().unwrap().tick_seq;
        if seq == self.timeline_built_seq {
            return;
        }
        let cursor = self
            .timeline
            .as_ref()
            .filter(|w| self.timeline_cursor_ms < w.to_ms)
            .map(|_| self.timeline_cursor_ms);
        self.timeline_refresh(cursor);
    }

    /// Step the zoom ladder by `delta` rungs ('+' narrower, '-' wider) and re-query; the
    /// cursor keeps its wall-clock position wherever the new window still contains it.
    fn timeline_rezoom(&mut self, delta: isize) {
        self.timeline_zoom = self
            .timeline_zoom
            .saturating_add_signed(delta)
            .min(TIMELINE_ZOOMS.len() - 1);
        self.timeline_refresh(Some(self.timeline_cursor_ms));
    }

    /// Move the timeline cursor by `cols` rendered columns (the event loop records the
    /// strip width before each draw), clamped to the window.
    fn timeline_step(&mut self, cols: i64) {
        let Some(w) = &self.timeline else { return };
        let span = w.to_ms.saturating_sub(w.from_ms).max(1);
        let col_ms = (span / self.timeline_cols.max(1) as u64).max(1);
        let delta = col_ms.saturating_mul(cols.unsigned_abs());
        let target = if cols < 0 {
            self.timeline_cursor_ms.saturating_sub(delta)
        } else {
            self.timeline_cursor_ms.saturating_add(delta)
        };
        self.timeline_cursor_ms = target.clamp(w.from_ms, w.to_ms);
    }

    /// Move the cursor and re-query the cached window (cursor ± 5 min). The queries are
    /// cheap indexed range reads, so a re-query per keypress is fine; a failed query leaves
    /// the affected slice empty rather than crashing the UI.
    fn seek(&mut self, cursor_ms: u64) {
        self.cursor_ms = cursor_ms;
        let from_ms = cursor_ms.saturating_sub(REPLAY_HALF_WINDOW_MS);
        let to_ms = cursor_ms.saturating_add(REPLAY_HALF_WINDOW_MS);
        let ids: Vec<_> = {
            let sh = self.collector.shared.lock().unwrap();
            sh.infos.iter().map(|i| i.id.clone()).collect()
        };
        let Some(store) = &self.store else { return };
        let samples = ids
            .iter()
            .map(|id| {
                store
                    .samples_between(id, from_ms, to_ms, Tier::TenSec)
                    .unwrap_or_default()
            })
            .collect();
        let processes = ids
            .iter()
            .map(|id| store.processes_at(id, cursor_ms).unwrap_or_default())
            .collect();
        let events = store.events_between(from_ms, to_ms).unwrap_or_default();
        self.replay = Some(ReplayWindow {
            from_ms,
            to_ms,
            samples,
            processes,
            events,
        });
    }

    /// Scrub by `delta_ms`, clamped to the recorded range so the cursor cannot wander into
    /// the void past either end of the recording. (Seeking to an event's exact ts is NOT
    /// clamped — events can sit just past the last flushed bucket.)
    fn scrub(&mut self, delta_ms: i64) {
        let target = if delta_ms < 0 {
            self.cursor_ms.saturating_sub(delta_ms.unsigned_abs())
        } else {
            self.cursor_ms.saturating_add(delta_ms as u64)
        };
        let clamped = match self.recorded_range() {
            Some((earliest, latest)) => target.clamp(earliest, latest),
            None => target,
        };
        self.seek(clamped);
    }

    /// `(earliest, latest)` recorded bucket across both tiers, when the store holds any.
    fn recorded_range(&self) -> Option<(u64, u64)> {
        let store = self.store.as_ref()?;
        let earliest = store.earliest_bucket_ms().ok().flatten()?;
        let latest = store.latest_bucket_ms().ok().flatten()?;
        Some((earliest, latest))
    }

    /// End of recorded coverage: the newest bucket's key PLUS its width.
    /// `latest_bucket_ms` is a bucket START, and a window ending exactly on it would
    /// clip the recording's final bucket to a zero-width slice that renders as a blank
    /// column — recorded data shown as a recording gap, the exact lie the strips
    /// promise never to tell. The recorder folds every frame into both tiers, so the
    /// newest key across tiers is always a 10s key and +10s is the coverage end.
    /// Events are inserted the moment they happen while their bucket may never be
    /// flushed (the recording can end first), so the newest event can sit past the
    /// last bucket and extends the end too.
    fn recorded_end(&self) -> Option<u64> {
        let store = self.store.as_ref()?;
        let bucket_end = store
            .latest_bucket_ms()
            .ok()
            .flatten()
            .map(|key| key + Tier::TenSec.width_ms());
        let event_ts = store.latest_event_ms(None).ok().flatten();
        match (bucket_end, event_ts) {
            (Some(b), Some(e)) => Some(b.max(e)),
            (b, e) => b.or(e),
        }
    }

    /// Move the story selection by `delta` within the active feed (live ring or replay
    /// window). A first press selects the top row; the index clamps at both ends.
    fn move_story_selection(&mut self, delta: i64) {
        let count = match self.mode {
            Mode::Live => self.collector.shared.lock().unwrap().history.events().len(),
            Mode::Replay => self.replay.as_ref().map(|w| w.events.len()).unwrap_or(0),
            // The timeline has no story pane; its event lane is cursor-driven.
            Mode::Timeline => 0,
        };
        if count == 0 {
            self.story_selected = None;
            return;
        }
        self.story_selected = Some(match self.story_selected {
            None => 0,
            Some(i) => i.saturating_add_signed(delta as isize).min(count - 1),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    use gpuviewer_core::{
        Confidence, DeviceId, DynamicSample, Event, EventKind, ProcessKind, ProcessSample,
        Severity, ThrottleReasons, Vendor,
    };
    use gpuviewer_history::{Recorder, SqliteStore, Tier};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ratatui::Terminal;

    use super::{App, ChartStyle, KeyOutcome, Mode, TIMELINE_ZOOMS};
    use crate::collector::test_collector;

    /// Base timestamp of the seeded recording: aligned to BOTH the 10s and 1m buckets so
    /// earliest/latest are exact, and fixed so the tests are deterministic.
    const BASE: u64 = 1_000_000_080_000;

    fn scratch_path() -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gpuviewer-replay-test-{}-{n}.db",
            std::process::id()
        ))
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        for ext in ["-wal", "-shm"] {
            let mut p = path.as_os_str().to_os_string();
            p.push(ext);
            let _ = std::fs::remove_file(p);
        }
    }

    fn full_sample(ts_ms: u64, util: f32) -> DynamicSample {
        DynamicSample {
            ts_ms,
            util_pct: Some(util),
            util_engine: None,
            mem_used_bytes: Some(2 << 30),
            power_mw: Some(150_000),
            temp_c: Some(65.0),
            fan_pct: Some(45.0),
            sm_clock_mhz: Some(1600),
            mem_clock_mhz: Some(8000),
            encoder_pct: None,
            decoder_pct: None,
            throttle: Some(ThrottleReasons::default()),
        }
    }

    /// A process that exists ONLY in the seeded history — the live mock never produces a
    /// "ghost-train", so seeing it on screen proves replay reads the store, not the live
    /// process list.
    fn ghost_proc() -> ProcessSample {
        ProcessSample {
            pid: 4242,
            name: "ghost-train".into(),
            kind: ProcessKind::Compute,
            mem_bytes: Some(2 << 30),
            util_pct: Some(35.0),
            cpu_pct: Some(150.0),
            container: Some("docker:3f2a9c1b".into()),
        }
    }

    fn throttle_event(ts_ms: u64, dev: &DeviceId) -> Event {
        Event {
            ts_ms,
            device: dev.clone(),
            kind: EventKind::ThrottleStart,
            severity: Severity::Warning,
            confidence: Confidence::Fact,
            title: "GPU0 began throttling (thermal)".into(),
            evidence: "throttle bits: [thermal]; 84C".into(),
        }
    }

    fn likely_event(ts_ms: u64, dev: &DeviceId) -> Event {
        Event {
            ts_ms,
            device: dev.clone(),
            kind: EventKind::IdleGap,
            severity: Severity::Info,
            confidence: Confidence::Likely,
            title: "GPU0 sat idle 14s — likely a dataloader stall".into(),
            evidence: "util 92% -> mean 2% over 14s".into(),
        }
    }

    /// Seed a recording for `dev`: three 10s buckets (BASE util_avg 50, BASE+10s util 20,
    /// BASE+20s util 80), the ghost-train process in every bucket, and two events at
    /// BASE+5s (fact) and BASE+15s (inference).
    fn seed_store(path: &PathBuf, dev: &DeviceId) {
        let (store, _) = SqliteStore::open(path).unwrap();
        let mut rec = Recorder::new(store);
        rec.store_mut()
            .register_device(dev, "Seeded GPU", Vendor::Unknown, Some(24 << 30))
            .unwrap();
        rec.observe(dev, &full_sample(BASE + 1_000, 40.0), &[ghost_proc()]);
        rec.observe(dev, &full_sample(BASE + 3_000, 60.0), &[ghost_proc()]);
        rec.observe(dev, &full_sample(BASE + 11_000, 20.0), &[ghost_proc()]);
        rec.observe(dev, &full_sample(BASE + 21_000, 80.0), &[ghost_proc()]);
        rec.record_events(&[
            throttle_event(BASE + 5_000, dev),
            likely_event(BASE + 15_000, dev),
        ]);
        rec.flush();
    }

    /// An app over a seeded store: returns it plus the scratch path for cleanup.
    fn seeded_app() -> (App, PathBuf) {
        let path = scratch_path();
        let collector = test_collector(Some(path.clone()));
        let dev = collector.shared.lock().unwrap().infos[0].id.clone();
        seed_store(&path, &dev);
        (App::new(collector), path)
    }

    /// An app pinned to the recording at `path` (`gpuviewer view incident.gpvr`),
    /// with `dev` as its one device. Split from [`seeded_viewer`] so tests can seed a
    /// custom store first.
    fn viewer_over(path: &PathBuf, dev: &DeviceId) -> App {
        let store = SqliteStore::open_readonly(path).unwrap();
        let infos = vec![gpuviewer_core::StaticInfo {
            id: dev.clone(),
            vendor: Vendor::Unknown,
            name: "Seeded GPU".into(),
            backend: "file".into(),
            mem_total_bytes: Some(24 << 30),
            power_limit_mw: None,
            max_sm_clock_mhz: None,
            temp_slowdown_c: None,
            driver_version: None,
            process_hint: None,
            source_caveat: None,
        }];
        let collector = crate::collector::Collector::stationary(infos, Some(path.clone()));
        App::viewer(collector, store, "incident.gpvr".into())
    }

    /// A viewer over the standard [`seed_store`] recording, plus the scratch path for
    /// cleanup and the device the file records.
    fn seeded_viewer() -> (App, PathBuf, DeviceId) {
        let path = scratch_path();
        let dev = DeviceId("0000:09:00.0".into());
        seed_store(&path, &dev);
        (viewer_over(&path, &dev), path, dev)
    }

    fn draw(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();
        {
            let sh = app.collector.shared.lock().unwrap();
            terminal.draw(|f| crate::ui::render(f, app, &sh)).unwrap();
        }
        terminal.backend().to_string()
    }

    /// 'r' enters replay anchored at the newest recorded bucket, and everything on screen
    /// comes from the store: chart title shows the cursor bucket's util_avg, the process
    /// table shows a process that exists only in history (with cpu column and container
    /// after the name), and the story feed lists the window's events.
    #[test]
    fn replay_enters_at_latest_and_renders_from_store() {
        let (mut app, path) = seeded_app();

        assert_eq!(app.handle_key(KeyCode::Char('r')), KeyOutcome::Continue);
        assert_eq!(app.mode, Mode::Replay);
        assert_eq!(
            app.cursor_ms,
            BASE + 20_000,
            "r anchors at the newest recorded bucket"
        );

        let screen = draw(&app);
        assert!(screen.contains("REPLAY"), "must say REPLAY:\n{screen}");
        assert!(
            screen.contains("util 80% REPLAY"),
            "chart must show the cursor bucket's util_avg from the store:\n{screen}"
        );
        assert!(
            screen.contains("ghost-train"),
            "process present only in history must render:\n{screen}"
        );
        assert!(
            screen.contains("[docker:3f2a9c1b]"),
            "container shown after the process name:\n{screen}"
        );
        assert!(
            screen.contains("150%"),
            "cpu column from cpu_avg:\n{screen}"
        );
        assert!(
            screen.contains("began throttling"),
            "window events must reach the story feed:\n{screen}"
        );
        cleanup(&path);
    }

    /// Left/Right scrub ±10s and PgUp/PgDn ±5min, all clamped to the recorded range;
    /// Home jumps to the earliest bucket; Tab still switches devices.
    #[test]
    fn replay_scrub_clamps_to_recorded_range() {
        let (mut app, path) = seeded_app();
        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.cursor_ms, BASE + 20_000);

        app.handle_key(KeyCode::Left);
        assert_eq!(app.cursor_ms, BASE + 10_000, "left scrubs back 10s");
        app.handle_key(KeyCode::Right);
        app.handle_key(KeyCode::Right);
        assert_eq!(
            app.cursor_ms,
            BASE + 20_000,
            "right scrub clamps at the newest bucket"
        );
        app.handle_key(KeyCode::PageUp);
        assert_eq!(app.cursor_ms, BASE, "PgUp clamps at the earliest bucket");
        app.handle_key(KeyCode::PageDown);
        assert_eq!(app.cursor_ms, BASE + 20_000, "PgDn clamps at the newest");
        app.handle_key(KeyCode::Home);
        assert_eq!(app.cursor_ms, BASE, "Home jumps to the earliest bucket");

        app.handle_key(KeyCode::Tab);
        assert_eq!(app.selected, 1, "tab still switches devices in replay");
        cleanup(&path);
    }

    /// Up/Down select events within the replay window and Enter re-anchors the cursor to
    /// the selected event's ts (the selection follows the event into the new window).
    #[test]
    fn replay_enter_anchors_cursor_to_selected_event() {
        let (mut app, path) = seeded_app();
        app.handle_key(KeyCode::Char('r'));

        app.handle_key(KeyCode::Down); // first press selects the oldest window event
        assert_eq!(app.story_selected, Some(0));
        app.handle_key(KeyCode::Enter);
        assert_eq!(
            app.cursor_ms,
            BASE + 5_000,
            "Enter re-anchors the cursor to the event ts"
        );
        assert_eq!(
            app.story_selected,
            Some(0),
            "selection follows the event into the re-centered window"
        );

        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.cursor_ms, BASE + 15_000);
        cleanup(&path);
    }

    /// From live: Up selects the newest story row and Enter enters replay anchored at
    /// that event's ts.
    #[test]
    fn live_enter_on_selected_event_enters_replay() {
        let (mut app, path) = seeded_app();
        {
            let mut sh = app.collector.shared.lock().unwrap();
            let dev = sh.infos[0].id.clone();
            sh.history.push_events([throttle_event(BASE + 5_000, &dev)]);
        }
        app.handle_key(KeyCode::Up);
        assert_eq!(
            app.story_selected,
            Some(0),
            "first press selects the top row"
        );
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.mode, Mode::Replay);
        assert_eq!(
            app.cursor_ms,
            BASE + 5_000,
            "cursor anchors at the selected event's ts"
        );
        cleanup(&path);
    }

    /// q quits from BOTH modes; Esc quits only from live — in replay it returns to live
    /// (and 'r' toggles back out too).
    #[test]
    fn quit_and_escape_semantics_per_mode() {
        let (mut app, path) = seeded_app();

        assert_eq!(app.handle_key(KeyCode::Char('q')), KeyOutcome::Quit);
        assert_eq!(
            app.handle_key(KeyCode::Esc),
            KeyOutcome::Quit,
            "Esc quits live"
        );

        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.mode, Mode::Replay);
        assert_eq!(
            app.handle_key(KeyCode::Esc),
            KeyOutcome::Continue,
            "Esc must NOT quit while in replay"
        );
        assert_eq!(app.mode, Mode::Live, "Esc returns to live");

        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.mode, Mode::Replay);
        assert_eq!(
            app.handle_key(KeyCode::Char('q')),
            KeyOutcome::Quit,
            "q still quits from replay"
        );
        assert_eq!(app.handle_key(KeyCode::Char('r')), KeyOutcome::Continue);
        assert_eq!(app.mode, Mode::Live, "'r' in replay returns to live");

        // Esc back in live quits again, and the footer no longer says REPLAY.
        assert_eq!(app.handle_key(KeyCode::Esc), KeyOutcome::Quit);
        let screen = draw(&app);
        assert!(
            !screen.contains("REPLAY"),
            "live view must not be labeled REPLAY:\n{screen}"
        );
        cleanup(&path);
    }

    /// `gpuviewer view`: the app opens pinned to the file — replay from the first frame,
    /// anchored at the newest event — and Esc/'r' must NOT drop to Live (there is no live
    /// behind a recording); 'q' still quits; the footer names the file as read-only.
    #[test]
    fn viewer_pins_replay_and_never_drops_to_live() {
        let (mut app, path, _dev) = seeded_viewer();

        assert_eq!(app.mode, Mode::Replay, "the viewer starts in replay");
        assert_eq!(
            app.cursor_ms,
            BASE + 15_000,
            "cursor anchors at the file's newest event"
        );

        assert_eq!(app.handle_key(KeyCode::Esc), KeyOutcome::Continue);
        assert_eq!(
            app.mode,
            Mode::Replay,
            "Esc must not drop to Live — there is no live behind the file"
        );
        assert_eq!(app.handle_key(KeyCode::Char('r')), KeyOutcome::Continue);
        assert_eq!(app.mode, Mode::Replay, "'r' is inert in the viewer too");

        let screen = draw(&app);
        assert!(
            screen.contains("viewing incident.gpvr (read-only)"),
            "footer must name the file and promise read-only:\n{screen}"
        );
        assert!(
            screen.contains("REPLAY"),
            "viewer is labeled REPLAY:\n{screen}"
        );
        assert!(
            screen.contains("ghost-train"),
            "the file's recorded processes must render:\n{screen}"
        );

        assert_eq!(
            app.handle_key(KeyCode::Char('q')),
            KeyOutcome::Quit,
            "q quits the viewer"
        );
        cleanup(&path);
    }

    /// Replay keys with no store (persistence disabled) stay live and surface the footer
    /// hint instead of doing nothing silently.
    #[test]
    fn replay_without_store_shows_footer_hint() {
        let mut app = App::new(test_collector(None));
        app.handle_key(KeyCode::Char('r'));
        assert_eq!(app.mode, Mode::Live, "no store: replay must not engage");
        assert!(app.replay_hint);
        let screen = draw(&app);
        assert!(
            screen.contains("replay needs persistence"),
            "footer must explain the dead key:\n{screen}"
        );
    }

    /// 't' opens the timeline overview at the 6h rung with the window ending at "now"
    /// and the cursor on the newest edge; '+'/'-' walk the zoom ladder and clamp at both
    /// ends; the cursor steps by rendered column, jumps by page, and pins to the window
    /// edges; Enter drills down into replay anchored exactly at the cursor; 't' from
    /// replay climbs back up with the cursor carried over; Esc drops to live.
    #[test]
    fn timeline_zooms_steps_and_drills_into_replay() {
        let (mut app, path) = seeded_app();

        app.handle_key(KeyCode::Char('t'));
        assert_eq!(app.mode, Mode::Timeline);
        let w = app.timeline_window().expect("entering builds a window");
        let (from, to) = (w.from_ms, w.to_ms);
        let span = TIMELINE_ZOOMS[2].span_ms;
        assert!(
            to - from >= span && to - from < span + 60_000,
            "default window spans the 6h rung (start aligned down to the 1m tier)"
        );
        assert_eq!(w.tier, Tier::OneMin, "6h reads the 1m tier");
        assert_eq!(
            app.timeline_cursor_ms, to,
            "cursor starts at the newest edge"
        );

        // Zoom ladder: '+' narrows and clamps at 1h, '-' widens and clamps at 7d.
        app.handle_key(KeyCode::Char('+'));
        app.handle_key(KeyCode::Char('+'));
        assert_eq!(app.timeline_zoom, 0);
        assert_eq!(
            app.timeline_window().unwrap().tier,
            Tier::TenSec,
            "the 1h rung reads the 10s tier"
        );
        app.handle_key(KeyCode::Char('+'));
        assert_eq!(app.timeline_zoom, 0, "'+' clamps at the narrowest rung");
        for _ in 0..10 {
            app.handle_key(KeyCode::Char('-'));
        }
        assert_eq!(
            app.timeline_zoom,
            TIMELINE_ZOOMS.len() - 1,
            "'-' clamps at 7d"
        );

        // Cursor: Home/End pin to the edges, arrows step one rendered column, PgDn
        // jumps ten, and steps clamp at the window edges.
        app.timeline_cols = 100;
        app.handle_key(KeyCode::Home);
        let w = app.timeline_window().unwrap();
        let (from, to) = (w.from_ms, w.to_ms);
        let col_ms = ((to - from).max(1) / 100).max(1);
        assert_eq!(
            app.timeline_cursor_ms, from,
            "Home pins to the window start"
        );
        app.handle_key(KeyCode::Left);
        assert_eq!(app.timeline_cursor_ms, from, "Left clamps at the start");
        app.handle_key(KeyCode::Right);
        assert_eq!(
            app.timeline_cursor_ms,
            from + col_ms,
            "Right steps exactly one rendered column"
        );
        app.handle_key(KeyCode::PageDown);
        assert_eq!(app.timeline_cursor_ms, from + 11 * col_ms, "PgDn jumps ten");
        app.handle_key(KeyCode::End);
        assert_eq!(app.timeline_cursor_ms, to, "End pins to the window end");
        app.handle_key(KeyCode::PageUp);
        assert_eq!(app.timeline_cursor_ms, to - 10 * col_ms);

        // Enter drills down; 't' climbs back up with the cursor carried over.
        let cursor = app.timeline_cursor_ms;
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.mode, Mode::Replay);
        assert_eq!(
            app.cursor_ms, cursor,
            "Enter drills into replay at the timeline cursor"
        );
        app.handle_key(KeyCode::Char('t'));
        assert_eq!(app.mode, Mode::Timeline);
        assert_eq!(
            app.timeline_cursor_ms, cursor,
            "re-entering from replay keeps the cursor"
        );
        assert_eq!(app.handle_key(KeyCode::Esc), KeyOutcome::Continue);
        assert_eq!(app.mode, Mode::Live, "Esc drops from the overview to live");

        // Tab still switches devices in the overview.
        app.handle_key(KeyCode::Char('t'));
        app.handle_key(KeyCode::Tab);
        assert_eq!(app.selected, 1);
        assert_eq!(
            app.handle_key(KeyCode::Char('q')),
            KeyOutcome::Quit,
            "q quits from the timeline too"
        );
        cleanup(&path);
    }

    /// 't' with no store behaves like 'r': stays live and surfaces the footer hint.
    #[test]
    fn timeline_without_store_shows_footer_hint() {
        let mut app = App::new(test_collector(None));
        app.handle_key(KeyCode::Char('t'));
        assert_eq!(
            app.mode,
            Mode::Live,
            "no store: the overview must not engage"
        );
        assert!(app.replay_hint);
    }

    /// In the file viewer the timeline window ends at the END of the file's newest
    /// bucket — its key plus the 10s width, never the key itself, which would clip the
    /// final recorded bucket to a zero-width slice rendered as a gap (a recording has
    /// no "now" either way). The start clamps to the recorded span, the replay cursor
    /// carries over, the screen labels itself TIMELINE with solid strips and lane
    /// markers, and Esc returns to the pinned replay — never to a live view that does
    /// not exist.
    #[test]
    fn viewer_timeline_clamps_to_file_span_and_returns_to_replay() {
        let (mut app, path, _dev) = seeded_viewer();

        app.handle_key(KeyCode::Char('t'));
        assert_eq!(app.mode, Mode::Timeline);
        let w = app.timeline_window().expect("viewer builds a window");
        assert_eq!(
            w.to_ms,
            BASE + 30_000,
            "the viewer window ends at the END of the file's newest bucket (key + 10s)"
        );
        assert_eq!(
            w.from_ms, BASE,
            "the start clamps to the file's oldest bucket"
        );
        assert!(w.clamped, "a 20s file inside a 6h zoom is clamped");
        assert_eq!(
            app.timeline_cursor_ms,
            BASE + 15_000,
            "the replay cursor (the file's newest event) carries over"
        );

        let screen = draw(&app);
        assert!(
            screen.contains("TIMELINE") && screen.contains("peak per col"),
            "the overview labels itself and its aggregation:\n{screen}"
        );
        assert!(
            screen.contains('█'),
            "strips paint solid filled columns:\n{screen}"
        );
        assert!(
            screen.contains('⚠'),
            "the lane marks the throttle fact with its severity icon:\n{screen}"
        );
        assert!(
            screen.contains('?'),
            "the lane marks the inference as '?' — not a fact icon:\n{screen}"
        );
        assert!(
            screen.contains("viewing incident.gpvr (read-only)"),
            "the viewer keeps naming the file:\n{screen}"
        );
        assert!(
            screen.contains("(30s recorded)"),
            "a clamped window must state the actual recorded span — three full 10s \
             buckets, not one bucket short:\n{screen}"
        );

        assert_eq!(app.handle_key(KeyCode::Esc), KeyOutcome::Continue);
        assert_eq!(
            app.mode,
            Mode::Replay,
            "the viewer's overview exits to the pinned replay, not to live"
        );
        cleanup(&path);
    }

    /// Window-end defect guard at the 1h/TenSec rung, where the newest 10s bucket's KEY
    /// always equals the old (buggy) window end: the window must extend to the bucket's
    /// END, so the file's final recorded bucket paints and the End-cursor readout shows
    /// its values — never "util — · vram —" over recorded data.
    #[test]
    fn viewer_timeline_tensec_rung_covers_the_final_bucket() {
        let (mut app, path, _dev) = seeded_viewer();

        app.handle_key(KeyCode::Char('t'));
        app.handle_key(KeyCode::Char('+'));
        app.handle_key(KeyCode::Char('+'));
        let w = app.timeline_window().expect("viewer builds a window");
        assert_eq!(w.tier, Tier::TenSec, "the 1h rung reads the 10s tier");
        assert_eq!(
            w.to_ms,
            BASE + 30_000,
            "the window ends at the newest 10s bucket's END — ending on its key clips \
             the final bucket to a zero-width slice that renders as a gap"
        );

        app.handle_key(KeyCode::End);
        assert_eq!(app.timeline_cursor_ms, BASE + 30_000);
        let screen = draw(&app);
        assert!(
            screen.contains("util 80%"),
            "the End cursor sits over the final recorded bucket, not a phantom gap:\n{screen}"
        );
        assert!(
            screen.contains("vram 2.0 GiB"),
            "vram at the End cursor comes from the final bucket too:\n{screen}"
        );
        cleanup(&path);
    }

    /// The OneMin-rung variant of the window-end defect: when a recording's final frame
    /// lands in the first 10s slot of a minute (the newest 10s key equals the 1m key —
    /// about one in six recordings), a window ending on the bucket KEY clips the entire
    /// final minute to nothing. The recorded minute must render.
    #[test]
    fn viewer_timeline_keeps_final_minute_when_file_ends_at_minute_start() {
        let path = scratch_path();
        let dev = DeviceId("0000:09:00.0".into());
        {
            let (store, _) = SqliteStore::open(&path).unwrap();
            let mut rec = Recorder::new(store);
            rec.store_mut()
                .register_device(&dev, "Seeded GPU", Vendor::Unknown, Some(24 << 30))
                .unwrap();
            rec.observe(&dev, &full_sample(BASE + 1_000, 40.0), &[]);
            // The final frame sits just past a minute boundary: 10s key == 1m key.
            rec.observe(&dev, &full_sample(BASE + 61_000, 70.0), &[]);
            rec.flush();
        }
        let mut app = viewer_over(&path, &dev);

        app.handle_key(KeyCode::Char('t'));
        let w = app.timeline_window().expect("viewer builds a window");
        assert_eq!(
            w.tier,
            Tier::OneMin,
            "the default 6h rung reads the 1m tier"
        );
        assert_eq!(
            w.to_ms,
            BASE + 70_000,
            "the window ends at the newest 10s bucket's END (BASE+60s key + 10s)"
        );

        app.handle_key(KeyCode::End);
        let screen = draw(&app);
        assert!(
            screen.contains("util 70%"),
            "the file's final minute must render under the End cursor, not vanish:\n{screen}"
        );
        assert!(
            screen.contains("(1m10s recorded)"),
            "the clamped span states the full recorded coverage:\n{screen}"
        );
        cleanup(&path);
    }

    /// Events land in the store the moment they happen, while their bucket may never be
    /// flushed (the recording can end first) — an event past the last flushed bucket
    /// must stretch the viewer's window so it still appears in the event lane.
    #[test]
    fn viewer_timeline_stretches_to_an_event_past_the_last_bucket() {
        let (mut app, path, dev) = seeded_viewer();
        {
            let (mut store, _) = SqliteStore::open(&path).unwrap();
            store
                .insert_events(&[throttle_event(BASE + 45_000, &dev)])
                .unwrap();
        }

        app.handle_key(KeyCode::Char('t'));
        let w = app.timeline_window().expect("viewer builds a window");
        assert_eq!(
            w.to_ms,
            BASE + 45_000,
            "the window end stretches to the newest event past the last bucket"
        );
        assert!(
            w.events.iter().any(|e| e.ts_ms == BASE + 45_000),
            "the late event must be inside the window, not silently dropped"
        );
        cleanup(&path);
    }

    /// Tick-invalidation defect guard: a live overview must not freeze at the instant
    /// 't' was pressed. Between ticks the cache holds (no SQLite on the render path);
    /// once the collector folds a new frame the window is re-queried with its right
    /// edge back at "now" — an edge-pinned cursor rides the edge, a scrolled-back
    /// cursor keeps its wall-clock position.
    #[test]
    fn timeline_refreshes_on_collector_tick() {
        let (mut app, path) = seeded_app();
        app.handle_key(KeyCode::Char('t'));
        assert_eq!(app.mode, Mode::Timeline);

        // No tick since the window was built: the cache holds (sentinel survives).
        app.timeline.as_mut().unwrap().to_ms = 12_345;
        app.timeline_tick();
        assert_eq!(
            app.timeline.as_ref().unwrap().to_ms,
            12_345,
            "no tick → no re-query"
        );

        // A tick landed: the window is rebuilt ending at "now", and the cursor —
        // sitting on the newest edge — rides it.
        let before = gpuviewer_core::now_ms();
        app.collector.shared.lock().unwrap().tick_seq += 1;
        app.timeline_tick();
        let to_ms = app.timeline.as_ref().unwrap().to_ms;
        assert!(
            to_ms >= before,
            "a tick must pull the stale right edge back to now"
        );
        assert_eq!(
            app.timeline_cursor_ms, to_ms,
            "a cursor pinned to the newest edge rides the refreshed edge"
        );

        // A scrolled-back cursor stays put across a refresh.
        app.timeline_cols = 100;
        app.handle_key(KeyCode::Home);
        app.handle_key(KeyCode::Right);
        app.handle_key(KeyCode::Right);
        let scrolled = app.timeline_cursor_ms;
        app.collector.shared.lock().unwrap().tick_seq += 1;
        app.timeline_tick();
        assert_eq!(
            app.timeline_cursor_ms, scrolled,
            "a scrolled-back cursor must keep its wall-clock position on refresh"
        );
        cleanup(&path);
    }

    /// The file viewer's overview never tick-refreshes — a recording does not grow (and
    /// its stationary collector never ticks; the guard holds even if the seq moved).
    #[test]
    fn viewer_timeline_never_tick_refreshes() {
        let (mut app, path, _dev) = seeded_viewer();
        app.handle_key(KeyCode::Char('t'));
        app.timeline.as_mut().unwrap().to_ms = 12_345;
        app.collector.shared.lock().unwrap().tick_seq += 1;
        app.timeline_tick();
        assert_eq!(
            app.timeline.as_ref().unwrap().to_ms,
            12_345,
            "a pinned recording must never be re-queried on ticks"
        );
        cleanup(&path);
    }

    /// 's' cycles braille↔solid in live and replay — same data, same gaps, different
    /// paint — and the timeline ignores it (its strips are always solid).
    #[test]
    fn chart_style_toggles_in_live_and_replay_but_not_timeline() {
        let (mut app, path) = seeded_app();
        assert_eq!(
            app.chart_style,
            ChartStyle::Braille,
            "braille is the default"
        );

        fn has_braille(s: &str) -> bool {
            s.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
        }

        app.handle_key(KeyCode::Char('s'));
        assert_eq!(app.chart_style, ChartStyle::Solid, "'s' toggles in live");
        app.handle_key(KeyCode::Char('r'));
        let screen = draw(&app);
        assert!(
            screen.contains("util 80% REPLAY"),
            "solid replay shows the same cursor value:\n{screen}"
        );
        assert!(
            !has_braille(&screen),
            "solid mode must not draw braille outlines:\n{screen}"
        );

        app.handle_key(KeyCode::Char('s'));
        assert_eq!(
            app.chart_style,
            ChartStyle::Braille,
            "'s' toggles in replay"
        );
        let screen = draw(&app);
        assert!(
            has_braille(&screen),
            "braille mode draws the step outline:\n{screen}"
        );
        assert!(
            screen.contains("s style"),
            "the footer advertises the toggle:\n{screen}"
        );

        app.handle_key(KeyCode::Char('t'));
        app.handle_key(KeyCode::Char('s'));
        assert_eq!(
            app.chart_style,
            ChartStyle::Braille,
            "the timeline ignores 's' — its strips are always solid"
        );
        cleanup(&path);
    }
}
