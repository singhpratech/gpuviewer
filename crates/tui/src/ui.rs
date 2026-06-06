//! Dashboard rendering: device tabs · charts · gauges · process table · the story feed.

use chrono::{Local, TimeZone};
use gpuviewer_core::{fmt_bytes, Confidence, Event, Severity, StaticInfo, Vendor};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, List, ListItem, Row, Table, Tabs,
};
use ratatui::Frame;

use crate::app::App;
use crate::collector::Shared;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Seconds of history shown in charts.
const CHART_WINDOW_S: f64 = 300.0;

fn vendor_color(v: Vendor) -> Color {
    match v {
        Vendor::Nvidia => Color::Green,
        Vendor::Amd => Color::Red,
        Vendor::Intel => Color::Blue,
        Vendor::Apple => Color::White,
        Vendor::Unknown => Color::Gray,
    }
}

pub fn render(f: &mut Frame, app: &App, shared: &Shared) {
    let [tabs_a, main_a, story_a, footer_a] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(12),
        Constraint::Length(9),
        Constraint::Length(1),
    ])
    .areas(f.area());

    render_tabs(f, tabs_a, app, shared);

    let [charts_a, procs_a] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(main_a);

    if let Some(info) = shared.infos.get(app.selected) {
        render_charts(f, charts_a, app, shared, info);
        render_processes(f, procs_a, app, shared, info);
    }

    render_story(f, story_a, shared);
    render_footer(f, footer_a, app, shared.mock);
}

fn render_tabs(f: &mut Frame, area: Rect, app: &App, shared: &Shared) {
    let titles: Vec<Line> = shared
        .infos
        .iter()
        .enumerate()
        .map(|(i, info)| {
            Line::from(vec![
                Span::styled(
                    format!(" {} ", i),
                    Style::default()
                        .fg(Color::Black)
                        .bg(vendor_color(info.vendor)),
                ),
                Span::raw(format!(" {} ", info.name)),
            ])
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.selected)
        .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    f.render_widget(tabs, area);
}

fn render_charts(f: &mut Frame, area: Rect, app: &App, shared: &Shared, info: &StaticInfo) {
    let [util_a, vram_a, gauges_a] = Layout::vertical([
        Constraint::Percentage(45),
        Constraint::Percentage(40),
        Constraint::Length(3),
    ])
    .areas(area);

    let now_ms = gpuviewer_core::now_ms() as f64;
    let hist = shared.history.device(&info.id);

    // Utilization chart.
    let util_pts: Vec<(f64, f64)> = hist
        .map(|h| {
            h.iter()
                .filter_map(|s| {
                    s.util_pct
                        .map(|u| ((s.ts_ms as f64 - now_ms) / 1000.0, u as f64))
                })
                .filter(|(x, _)| *x >= -CHART_WINDOW_S)
                .collect()
        })
        .unwrap_or_default();
    let latest_util = shared.latest[app.selected]
        .as_ref()
        .and_then(|s| s.util_pct)
        .map(|u| format!("{u:.0}%"))
        .unwrap_or_else(|| "—".into());
    let ds = vec![Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(&util_pts)];
    let chart = Chart::new(ds)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" util {} ", latest_util)),
        )
        .x_axis(Axis::default().bounds([-CHART_WINDOW_S, 0.0]))
        .y_axis(
            Axis::default()
                .bounds([0.0, 100.0])
                .labels(["0", "50", "100"]),
        );
    f.render_widget(chart, util_a);

    // VRAM chart.
    let total_gib = info.mem_total_bytes.map(|b| b as f64 / GIB).unwrap_or(0.0);
    let vram_pts: Vec<(f64, f64)> = hist
        .map(|h| {
            h.iter()
                .filter_map(|s| {
                    s.mem_used_bytes
                        .map(|m| ((s.ts_ms as f64 - now_ms) / 1000.0, m as f64 / GIB))
                })
                .filter(|(x, _)| *x >= -CHART_WINDOW_S)
                .collect()
        })
        .unwrap_or_default();
    let latest_mem = shared.latest[app.selected]
        .as_ref()
        .and_then(|s| s.mem_used_bytes)
        .map(fmt_bytes)
        .unwrap_or_else(|| "—".into());
    let total_label = info
        .mem_total_bytes
        .map(fmt_bytes)
        .unwrap_or_else(|| "?".into());
    let ds = vec![Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Magenta))
        .data(&vram_pts)];
    let y_max = if total_gib > 0.0 { total_gib } else { 1.0 };
    let y_max_label = format!("{y_max:.0}G");
    let chart = Chart::new(ds)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" vram {latest_mem} / {total_label} ")),
        )
        .x_axis(Axis::default().bounds([-CHART_WINDOW_S, 0.0]))
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(["0", y_max_label.as_str()]),
        );
    f.render_widget(chart, vram_a);

    // Gauge row: power · temp · fan/clock.
    let [pw_a, tp_a, fc_a] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(gauges_a);

    let latest = shared.latest[app.selected].as_ref();

    let (p_ratio, p_label) = match (latest.and_then(|s| s.power_mw), info.power_limit_mw) {
        (Some(p), Some(lim)) if lim > 0 => (
            (p as f64 / lim as f64).clamp(0.0, 1.0),
            format!("{:.0}W / {:.0}W", p as f64 / 1000.0, lim as f64 / 1000.0),
        ),
        (Some(p), _) => (0.0, format!("{:.0}W", p as f64 / 1000.0)),
        _ => (0.0, "—".into()),
    };
    f.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" power "))
            .ratio(p_ratio)
            .gauge_style(Style::default().fg(Color::Yellow))
            .label(p_label),
        pw_a,
    );

    let (t_ratio, t_label, t_color) = match latest.and_then(|s| s.temp_c) {
        Some(t) => {
            let max = info.temp_slowdown_c.unwrap_or(95.0) as f64;
            let throttling = latest.map(|s| s.throttle.any()).unwrap_or(false);
            (
                (t as f64 / max).clamp(0.0, 1.0),
                if throttling {
                    format!("{t:.0}°C ⚠ THROTTLING")
                } else {
                    format!("{t:.0}°C")
                },
                if throttling { Color::Red } else { Color::Green },
            )
        }
        None => (0.0, "—".into(), Color::Green),
    };
    f.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(" temp "))
            .ratio(t_ratio)
            .gauge_style(Style::default().fg(t_color))
            .label(t_label),
        tp_a,
    );

    let fan = latest
        .and_then(|s| s.fan_pct)
        .map(|v| format!("fan {v:.0}%"))
        .unwrap_or_else(|| "fan —".into());
    let clk = latest
        .and_then(|s| s.sm_clock_mhz)
        .map(|c| format!("{c} MHz"))
        .unwrap_or_else(|| "— MHz".into());
    let fan_ratio = latest
        .and_then(|s| s.fan_pct)
        .map(|v| (v as f64 / 100.0).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    f.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" fan · clock "),
            )
            .ratio(fan_ratio)
            .gauge_style(Style::default().fg(Color::Blue))
            .label(format!("{fan} · {clk}")),
        fc_a,
    );
}

fn render_processes(f: &mut Frame, area: Rect, app: &App, shared: &Shared, info: &StaticInfo) {
    let mut procs = shared.processes[app.selected].clone();
    procs.sort_by_key(|p| std::cmp::Reverse(p.mem_bytes.unwrap_or(0)));

    let rows: Vec<Row> = procs
        .iter()
        .map(|p| {
            Row::new(vec![
                p.pid.to_string(),
                p.name.clone(),
                p.kind.label().to_string(),
                p.mem_bytes.map(fmt_bytes).unwrap_or_else(|| "—".into()),
                p.util_pct
                    .map(|u| format!("{u:.0}%"))
                    .unwrap_or_else(|| "—".into()),
            ])
        })
        .collect();

    let mut block = Block::default().borders(Borders::ALL).title(" processes ");
    if let Some(hint) = &info.process_hint {
        // Explain a known-incomplete process list (WSL2 driver limitation, privilege
        // wall) right where the user is looking for the missing rows.
        block = block.title_bottom(Line::styled(
            format!(" {hint} "),
            Style::default().fg(Color::DarkGray).italic(),
        ));
    }
    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Min(10),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(vec!["pid", "name", "type", "mem", "util"]).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::DarkGray),
        ),
    )
    .block(block);
    f.render_widget(table, area);
}

fn render_story(f: &mut Frame, area: Rect, shared: &Shared) {
    let items: Vec<ListItem> = shared
        .history
        .events()
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .map(event_line)
        .map(ListItem::new)
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" story — what changed and why ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );
    f.render_widget(list, area);
}

fn event_line(e: &Event) -> Line<'static> {
    let ts = Local
        .timestamp_millis_opt(e.ts_ms as i64)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into());

    let (icon, color) = match e.severity {
        Severity::Critical => ("✖", Color::Red),
        Severity::Warning => ("⚠", Color::Yellow),
        Severity::Info => ("·", Color::DarkGray),
    };

    let mut spans = vec![
        Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{icon} "), Style::default().fg(color)),
    ];
    if e.confidence == Confidence::Likely {
        spans.push(Span::styled(
            "likely ".to_string(),
            Style::default().fg(Color::DarkGray).italic(),
        ));
    }
    spans.push(Span::styled(e.title.clone(), Style::default().fg(color)));
    Line::from(spans)
}

fn render_footer(f: &mut Frame, area: Rect, app: &App, mock: bool) {
    let paused = if app.paused() { " · PAUSED" } else { "" };
    // "(mock data)" must appear exactly when the data IS mock: a footer that calls
    // live NVML data mock (or vice versa) is the confidently-wrong labeling this
    // product exists to avoid.
    let mock_tag = if mock { " (mock data)" } else { "" };
    let line = Line::from(format!(
        " q quit · ←/→/tab device · p pause{paused}  —  gpuviewer{mock_tag}"
    ))
    .style(Style::default().fg(Color::DarkGray))
    .alignment(Alignment::Left);
    f.render_widget(line, area);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::app::App;
    use crate::collector::{Collector, Engine};

    /// The processes pane must surface `StaticInfo::process_hint` (the WSL2 / privilege
    /// wall explanation) when present, and render unchanged when it is `None` — the mock
    /// sets `None`, so only this test exercises the `Some` branch.
    #[test]
    fn process_hint_renders_in_processes_pane() {
        // Long interval: the collector thread stays quiet while we drive draws by hand.
        let collector = Collector::start(Engine::new(true), Duration::from_secs(3600));
        let shared = Arc::clone(&collector.shared);
        let app = App::new(collector);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();

        // Baseline: no hint (mock default) renders without it.
        {
            let sh = shared.lock().unwrap();
            terminal.draw(|f| super::render(f, &app, &sh)).unwrap();
        }
        let screen = terminal.backend().to_string();
        assert!(screen.contains("processes"));
        assert!(!screen.contains("your processes only"));

        // With a hint on the selected device, the pane shows it.
        shared.lock().unwrap().infos[0].process_hint = Some("your processes only".into());
        {
            let sh = shared.lock().unwrap();
            terminal.draw(|f| super::render(f, &app, &sh)).unwrap();
        }
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("your processes only"),
            "process_hint missing from processes pane:\n{screen}"
        );
    }

    /// The footer's "(mock data)" tag must track the actual data source — it was once
    /// hardcoded, calling live NVML data mock. The mock engine asserts the tag; then
    /// flipping `Shared::mock` (what a real backend produces) asserts its absence.
    #[test]
    fn footer_mock_tag_tracks_data_source() {
        let collector = Collector::start(Engine::new(true), Duration::from_secs(3600));
        let shared = Arc::clone(&collector.shared);
        let app = App::new(collector);
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();

        // Mock engine: the footer says so.
        {
            let sh = shared.lock().unwrap();
            terminal.draw(|f| super::render(f, &app, &sh)).unwrap();
        }
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("(mock data)"),
            "mock data must be labeled mock:\n{screen}"
        );

        // Live data must never carry the mock tag.
        shared.lock().unwrap().mock = false;
        {
            let sh = shared.lock().unwrap();
            terminal.draw(|f| super::render(f, &app, &sh)).unwrap();
        }
        let screen = terminal.backend().to_string();
        assert!(
            !screen.contains("(mock data)"),
            "live data must not be labeled mock:\n{screen}"
        );
        assert!(screen.contains("gpuviewer"), "footer brand text missing");
    }
}
