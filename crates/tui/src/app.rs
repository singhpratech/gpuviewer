//! TUI event loop and app state: the live dashboard plus the scroll-back replay mode —
//! the product's headline feature ("scroll back to 02:14 — it'll tell you why").

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

/// Which timeline the dashboard shows: the live tick stream, or a recorded window scrolled
/// back to [`App::cursor_ms`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Live,
    Replay,
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
    /// The recording file this session is pinned to (`gpuviewer view`); `Some` means there
    /// is no live source behind the UI, so the app never leaves replay.
    view_file: Option<String>,
    /// Cached replay window, re-queried on every seek.
    replay: Option<ReplayWindow>,
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
            view_file: None,
            replay: None,
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
            Mode::Live => None,
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
        res
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            {
                let shared = self.collector.shared.lock().unwrap();
                let n = shared.infos.len();
                if n > 0 && self.selected >= n {
                    self.selected = n - 1;
                }
                terminal.draw(|f| crate::ui::render(f, self, &shared))?;
            }

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
        if self.store.is_none() {
            let db_path = self.collector.shared.lock().unwrap().db_path.clone();
            match db_path.and_then(|p| SqliteStore::open_readonly(p).ok()) {
                Some(s) => self.store = Some(s),
                None => {
                    self.replay_hint = true;
                    return;
                }
            }
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

    /// Move the story selection by `delta` within the active feed (live ring or replay
    /// window). A first press selects the top row; the index clamps at both ends.
    fn move_story_selection(&mut self, delta: i64) {
        let count = match self.mode {
            Mode::Live => self.collector.shared.lock().unwrap().history.events().len(),
            Mode::Replay => self.replay.as_ref().map(|w| w.events.len()).unwrap_or(0),
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
    use gpuviewer_history::{Recorder, SqliteStore};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ratatui::Terminal;

    use super::{App, KeyOutcome, Mode};
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
            mem_used_bytes: Some(2 << 30),
            power_mw: Some(150_000),
            temp_c: Some(65.0),
            fan_pct: Some(45.0),
            sm_clock_mhz: Some(1600),
            mem_clock_mhz: Some(8000),
            encoder_pct: None,
            decoder_pct: None,
            throttle: ThrottleReasons::default(),
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
        let path = scratch_path();
        let dev = DeviceId("0000:09:00.0".into());
        seed_store(&path, &dev);

        let store = SqliteStore::open_readonly(&path).unwrap();
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
        }];
        let collector = crate::collector::Collector::stationary(infos, Some(path.clone()));
        let mut app = App::viewer(collector, store, "incident.gpvr".into());

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
}
