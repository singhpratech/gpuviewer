//! TUI event loop and app state.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::{TerminalOptions, Viewport};

use crate::collector::Collector;

/// Viewport assumed when the terminal reports a 0×0 size (bare ptys: `script`, some CI).
const FALLBACK_SIZE: (u16, u16) = (80, 24);

pub struct App {
    pub selected: usize,
    collector: Collector,
}

impl App {
    pub fn new(collector: Collector) -> Self {
        Self {
            selected: 0,
            collector,
        }
    }

    pub fn paused(&self) -> bool {
        self.collector.paused.load(Ordering::Relaxed)
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
                    let n = self.collector.shared.lock().unwrap().infos.len().max(1);
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Tab | KeyCode::Right => self.selected = (self.selected + 1) % n,
                        KeyCode::Left | KeyCode::BackTab => {
                            self.selected = (self.selected + n - 1) % n
                        }
                        KeyCode::Char('p') => {
                            let p = &self.collector.paused;
                            p.store(!p.load(Ordering::Relaxed), Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
