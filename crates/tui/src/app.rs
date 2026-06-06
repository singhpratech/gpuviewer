//! TUI event loop and app state.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind};

use crate::collector::Collector;

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
        let mut terminal = ratatui::init();
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
