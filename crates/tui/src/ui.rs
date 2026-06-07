//! Dashboard rendering: device tabs · charts · gauges · process table · the story feed.
//! Two modes share the layout: Live draws the RAM ring + latest tick, Replay draws the
//! cached rollup window around the cursor (see `app.rs`) — same panes, different source,
//! and every replay surface says so ("REPLAY") so recorded data is never mistaken for live.

use std::sync::atomic::Ordering;

use chrono::{Local, TimeZone};
use gpuviewer_core::{fmt_bytes, Confidence, Event, Severity, StaticInfo, Vendor};
use gpuviewer_history::{SampleRollup, Tier};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, List, ListItem, Row, Table, Tabs,
};
use ratatui::Frame;

use crate::app::{App, Mode, ReplayWindow};
use crate::collector::Shared;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Seconds of history shown in live charts.
const CHART_WINDOW_S: f64 = 300.0;
/// Seconds either side of the cursor shown in replay charts (matches the queried window).
const REPLAY_WINDOW_S: f64 = 300.0;

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

    // `replay_window()` is `Some` only in replay mode, so a stale cache can never shadow
    // the live panes.
    let replay = app.replay_window();
    if let Some(info) = shared.infos.get(app.selected) {
        match replay {
            Some(w) => {
                render_charts_replay(f, charts_a, app, w, info);
                render_processes_replay(f, procs_a, app, w, info);
            }
            None => {
                render_charts(f, charts_a, app, shared, info);
                render_processes(f, procs_a, app, shared, info);
            }
        }
    }

    match replay {
        Some(w) => render_story_replay(f, story_a, app, w),
        None => render_story(f, story_a, app, shared),
    }
    render_footer(f, footer_a, app, shared);
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
    let util_segs = step_segments(&util_pts, 1.0);
    let ds: Vec<Dataset> = util_segs
        .iter()
        .map(|seg| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(seg)
        })
        .collect();
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
    render_vram_chart(
        f,
        vram_a,
        info,
        &vram_pts,
        &VramChartCtx {
            headline: &latest_mem,
            x_bounds: [-CHART_WINDOW_S, 0.0],
            title_tag: "",
            sample_w_s: 1.0,
        },
    );

    // Gauge row reads the latest tick's sample.
    let latest = shared.latest[app.selected].as_ref();
    render_gauges(
        f,
        gauges_a,
        &GaugeValues {
            power_mw: latest.and_then(|s| s.power_mw),
            power_limit_mw: info.power_limit_mw,
            temp_c: latest.and_then(|s| s.temp_c),
            temp_slowdown_c: info.temp_slowdown_c,
            throttling: latest.map(|s| s.throttle.any()).unwrap_or(false),
            fan_pct: latest.and_then(|s| s.fan_pct),
            sm_clock_mhz: latest.and_then(|s| s.sm_clock_mhz),
        },
    );
}

/// Replay charts: the 10s rollup window, x-axis in seconds relative to the cursor. The
/// chart titles carry the cursor bucket's value plus " REPLAY" so recorded data is never
/// mistaken for live.
fn render_charts_replay(f: &mut Frame, area: Rect, app: &App, w: &ReplayWindow, info: &StaticInfo) {
    let [util_a, vram_a, gauges_a] = Layout::vertical([
        Constraint::Percentage(45),
        Constraint::Percentage(40),
        Constraint::Length(3),
    ])
    .areas(area);

    let cursor = app.cursor_ms as f64;
    let samples: &[SampleRollup] = w
        .samples
        .get(app.selected)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let at = bucket_at(samples, app.cursor_ms);

    // Utilization (bucket averages) across the window.
    let util_pts: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|r| {
            r.util_avg
                .map(|u| ((r.bucket_ms as f64 - cursor) / 1000.0, u as f64))
        })
        .collect();
    let cursor_util = at
        .and_then(|r| r.util_avg)
        .map(|u| format!("{u:.0}%"))
        .unwrap_or_else(|| "—".into());
    let bucket_w_s = Tier::TenSec.width_ms() as f64 / 1000.0;
    let util_segs = step_segments(&util_pts, bucket_w_s);
    let ds: Vec<Dataset> = util_segs
        .iter()
        .map(|seg| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Cyan))
                .data(seg)
        })
        .collect();
    let chart = Chart::new(ds)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" util {cursor_util} REPLAY ")),
        )
        .x_axis(
            Axis::default()
                .bounds([-REPLAY_WINDOW_S, REPLAY_WINDOW_S])
                .labels(["-5m", "cursor", "+5m"]),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, 100.0])
                .labels(["0", "50", "100"]),
        );
    f.render_widget(chart, util_a);

    // VRAM (bucket averages).
    let vram_pts: Vec<(f64, f64)> = samples
        .iter()
        .filter_map(|r| {
            r.mem_avg
                .map(|m| ((r.bucket_ms as f64 - cursor) / 1000.0, m as f64 / GIB))
        })
        .collect();
    let cursor_mem = at
        .and_then(|r| r.mem_avg)
        .map(fmt_bytes)
        .unwrap_or_else(|| "—".into());
    render_vram_chart(
        f,
        vram_a,
        info,
        &vram_pts,
        &VramChartCtx {
            headline: &cursor_mem,
            x_bounds: [-REPLAY_WINDOW_S, REPLAY_WINDOW_S],
            title_tag: " REPLAY",
            sample_w_s: bucket_w_s,
        },
    );

    // Gauge row reads the bucket at the cursor. `throttle_n` counts frames in the bucket
    // with any throttle bit set — a recorded fact, so flagging on >0 is honest.
    render_gauges(
        f,
        gauges_a,
        &GaugeValues {
            power_mw: at.and_then(|r| r.power_avg_mw),
            power_limit_mw: info.power_limit_mw,
            temp_c: at.and_then(|r| r.temp_max_c),
            temp_slowdown_c: info.temp_slowdown_c,
            throttling: at.map(|r| r.throttle_n > 0).unwrap_or(false),
            fan_pct: at.and_then(|r| r.fan_max_pct),
            sm_clock_mhz: at.and_then(|r| r.sm_clock_avg),
        },
    );
}

/// Consecutive points farther apart than this are not bridged: the trace breaks and the
/// blank stays, because a hole in the recording must look like a hole. Covers both the
/// 10s rollup spacing and the live cadence at its maximum idle-backoff stretch (10s).
const GAP_BRIDGE_S: f64 = 15.0;

/// Square-wave outline from sampled points: each value holds flat until the next sample,
/// then steps vertically — no diagonal interpolation smearing a 0→95 transition. Points
/// farther apart than [`GAP_BRIDGE_S`] start a new segment (rendered as separate
/// datasets), and each segment's last value is held for `trail_s`, the sample's real
/// width, so the newest bucket is drawn as wide as it actually is.
fn step_segments(pts: &[(f64, f64)], trail_s: f64) -> Vec<Vec<(f64, f64)>> {
    let mut segs: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut cur: Vec<(f64, f64)> = Vec::new();
    for &(x, y) in pts {
        match cur.last().copied() {
            Some((px, py)) if x - px <= GAP_BRIDGE_S => {
                cur.push((x, py)); // hold the previous value to here…
                cur.push((x, y)); // …then step vertically
            }
            Some((px, py)) => {
                cur.push((px + trail_s, py));
                segs.push(std::mem::take(&mut cur));
                cur.push((x, y));
            }
            None => cur.push((x, y)),
        }
    }
    if let Some(&(px, py)) = cur.last() {
        cur.push((px + trail_s, py));
        segs.push(cur);
    }
    segs
}

/// The 10s rollup covering `cursor_ms`, if recorded. Exact-bucket only: a cursor sitting
/// in a gap shows "—" on every gauge rather than a neighboring bucket's values.
fn bucket_at(samples: &[SampleRollup], cursor_ms: u64) -> Option<&SampleRollup> {
    let key = cursor_ms - cursor_ms % Tier::TenSec.width_ms();
    samples.iter().find(|r| r.bucket_ms == key)
}

/// Per-mode inputs to the shared VRAM chart: live and replay differ only in these.
struct VramChartCtx<'a> {
    headline: &'a str,
    x_bounds: [f64; 2],
    title_tag: &'a str,
    sample_w_s: f64,
}

/// The VRAM chart, shared by live (raw ring samples) and replay (bucket averages): only
/// the points and the per-mode context differ.
fn render_vram_chart(
    f: &mut Frame,
    area: Rect,
    info: &StaticInfo,
    pts: &[(f64, f64)],
    ctx: &VramChartCtx,
) {
    let total_gib = info.mem_total_bytes.map(|b| b as f64 / GIB).unwrap_or(0.0);
    let total_label = info
        .mem_total_bytes
        .map(fmt_bytes)
        .unwrap_or_else(|| "?".into());
    let segs = step_segments(pts, ctx.sample_w_s);
    let ds: Vec<Dataset> = segs
        .iter()
        .map(|seg| {
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::Magenta))
                .data(seg)
        })
        .collect();
    let y_max = if total_gib > 0.0 { total_gib } else { 1.0 };
    let y_max_label = format!("{y_max:.0}G");
    let chart = Chart::new(ds)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " vram {} / {total_label}{} ",
            ctx.headline, ctx.title_tag
        )))
        .x_axis(Axis::default().bounds(ctx.x_bounds))
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(["0", y_max_label.as_str()]),
        );
    f.render_widget(chart, area);
}

/// The gauge row's inputs, shared by live (latest sample) and replay (bucket at cursor).
/// Every field is `Option`: an absent metric renders "—", never a fake zero.
struct GaugeValues {
    power_mw: Option<u32>,
    power_limit_mw: Option<u32>,
    temp_c: Option<f32>,
    temp_slowdown_c: Option<f32>,
    /// Whether the temp gauge flags throttling (live: current bits; replay: any throttled
    /// frame in the cursor's bucket).
    throttling: bool,
    fan_pct: Option<f32>,
    sm_clock_mhz: Option<u32>,
}

/// Gauge row: power · temp · fan/clock.
fn render_gauges(f: &mut Frame, area: Rect, v: &GaugeValues) {
    let [pw_a, tp_a, fc_a] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(area);

    let (p_ratio, p_label) = match (v.power_mw, v.power_limit_mw) {
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

    let (t_ratio, t_label, t_color) = match v.temp_c {
        Some(t) => {
            let max = v.temp_slowdown_c.unwrap_or(95.0) as f64;
            (
                (t as f64 / max).clamp(0.0, 1.0),
                if v.throttling {
                    format!("{t:.0}°C ⚠ THROTTLING")
                } else {
                    format!("{t:.0}°C")
                },
                if v.throttling {
                    Color::Red
                } else {
                    Color::Green
                },
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

    let fan = v
        .fan_pct
        .map(|p| format!("fan {p:.0}%"))
        .unwrap_or_else(|| "fan —".into());
    let clk = v
        .sm_clock_mhz
        .map(|c| format!("{c} MHz"))
        .unwrap_or_else(|| "— MHz".into());
    let fan_ratio = v
        .fan_pct
        .map(|p| (p as f64 / 100.0).clamp(0.0, 1.0))
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

/// Process table column widths (shared by live and replay).
const PROC_WIDTHS: [Constraint; 6] = [
    Constraint::Length(7),
    Constraint::Min(10),
    Constraint::Length(4),
    Constraint::Length(9),
    Constraint::Length(6),
    Constraint::Length(6),
];

fn proc_header() -> Row<'static> {
    Row::new(vec!["pid", "name", "type", "mem", "util", "cpu"]).style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::DarkGray),
    )
}

/// "ollama [docker:3f2a9c1b]" — the container identity rides with the name column so a
/// containerized workload is recognizable at a glance.
fn proc_display_name(name: &str, container: Option<&str>) -> String {
    match container {
        Some(c) => format!("{name} [{c}]"),
        None => name.to_string(),
    }
}

fn fmt_opt_pct(v: Option<f32>) -> String {
    v.map(|p| format!("{p:.0}%")).unwrap_or_else(|| "—".into())
}

/// The process pane's block, with the `process_hint` (WSL2 driver limitation, privilege
/// wall) surfaced right where the user is looking for the missing rows. The hint applies
/// in replay too: the recording only ever held what the collector could see.
fn proc_block(info: &StaticInfo) -> Block<'static> {
    let mut block = Block::default().borders(Borders::ALL).title(" processes ");
    if let Some(hint) = &info.process_hint {
        block = block.title_bottom(Line::styled(
            format!(" {hint} "),
            Style::default().fg(Color::DarkGray).italic(),
        ));
    }
    block
}

fn render_processes(f: &mut Frame, area: Rect, app: &App, shared: &Shared, info: &StaticInfo) {
    let mut procs = shared.processes[app.selected].clone();
    procs.sort_by_key(|p| std::cmp::Reverse(p.mem_bytes.unwrap_or(0)));

    let rows: Vec<Row> = procs
        .iter()
        .map(|p| {
            Row::new(vec![
                p.pid.to_string(),
                proc_display_name(&p.name, p.container.as_deref()),
                p.kind.label().to_string(),
                p.mem_bytes.map(fmt_bytes).unwrap_or_else(|| "—".into()),
                fmt_opt_pct(p.util_pct),
                fmt_opt_pct(p.cpu_pct),
            ])
        })
        .collect();

    let table = Table::new(rows, PROC_WIDTHS)
        .header(proc_header())
        .block(proc_block(info));
    f.render_widget(table, area);
}

/// Replay process table: the 10s rollup bucket at the cursor (`processes_at`), with the
/// bucket's peak memory and mean util/cpu per process.
fn render_processes_replay(
    f: &mut Frame,
    area: Rect,
    app: &App,
    w: &ReplayWindow,
    info: &StaticInfo,
) {
    let mut procs = w.processes.get(app.selected).cloned().unwrap_or_default();
    procs.sort_by_key(|p| std::cmp::Reverse(p.mem_max.unwrap_or(0)));

    let rows: Vec<Row> = procs
        .iter()
        .map(|p| {
            Row::new(vec![
                p.pid.to_string(),
                proc_display_name(&p.name, p.container.as_deref()),
                p.kind.label().to_string(),
                p.mem_max.map(fmt_bytes).unwrap_or_else(|| "—".into()),
                fmt_opt_pct(p.util_avg),
                fmt_opt_pct(p.cpu_avg),
            ])
        })
        .collect();

    let table = Table::new(rows, PROC_WIDTHS)
        .header(proc_header())
        .block(proc_block(info));
    f.render_widget(table, area);
}

fn render_story(f: &mut Frame, area: Rect, app: &App, shared: &Shared) {
    let cap = area.height.saturating_sub(2) as usize;
    let sel = app.story_selected;
    // Newest first; skip just enough of the newest rows to keep the selection visible.
    let skip = sel.map_or(0, |s| s.saturating_sub(cap.saturating_sub(1)));
    let items: Vec<ListItem> = shared
        .history
        .events()
        .iter()
        .rev()
        .enumerate()
        .skip(skip)
        .take(cap)
        .map(|(i, e)| {
            let mut line = event_line(e);
            if Some(i) == sel {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" story — what changed and why ")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );
    f.render_widget(list, area);
}

/// Replay story feed: the window's events in chronological order, with a marker row
/// anchoring the cursor's own place in time among them.
fn render_story_replay(f: &mut Frame, area: Rect, app: &App, w: &ReplayWindow) {
    let cap = area.height.saturating_sub(2) as usize;
    let marker_at = w
        .events
        .iter()
        .take_while(|e| e.ts_ms <= app.cursor_ms)
        .count();
    let mut rows: Vec<(Option<usize>, Line)> = Vec::with_capacity(w.events.len() + 1);
    for (i, e) in w.events.iter().enumerate() {
        if i == marker_at {
            rows.push((None, cursor_line(app.cursor_ms)));
        }
        rows.push((Some(i), event_line(e)));
    }
    if marker_at == w.events.len() {
        rows.push((None, cursor_line(app.cursor_ms)));
    }

    // Keep the selected row (or, with nothing selected, the cursor marker) visible.
    let focus = app
        .story_selected
        .map(|s| s + usize::from(s >= marker_at))
        .unwrap_or(marker_at);
    let skip = focus.saturating_sub(cap.saturating_sub(1));
    let items: Vec<ListItem> = rows
        .into_iter()
        .skip(skip)
        .take(cap)
        .map(|(idx, line)| {
            let line = if idx.is_some() && idx == app.story_selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            };
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " story — events {}–{} ",
                fmt_clock(w.from_ms),
                fmt_clock(w.to_ms)
            ))
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );
    f.render_widget(list, area);
}

/// The cursor's own row in the replay story feed — "you are here" among the events.
fn cursor_line(cursor_ms: u64) -> Line<'static> {
    Line::styled(
        format!("── cursor {} ──", fmt_clock(cursor_ms)),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn event_line(e: &Event) -> Line<'static> {
    let ts = fmt_clock(e.ts_ms);

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

/// Wall HH:MM:SS for event rows and the replay cursor.
fn fmt_clock(ms: u64) -> String {
    Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}

fn render_footer(f: &mut Frame, area: Rect, app: &App, shared: &Shared) {
    // "(mock data)" must appear exactly when the data IS mock: a footer that calls
    // live NVML data mock (or vice versa) is the confidently-wrong labeling this
    // product exists to avoid. The rule holds in replay too — a mock run records to
    // the mock database, so its replay is also mock data.
    let mock_tag = if shared.mock { " (mock data)" } else { "" };
    let text = match app.mode {
        // The file viewer has no live mode behind the recording, so the "esc live" hint
        // would be a dead key dressed as a feature — the file's name and the read-only
        // promise take its place. Provenance is unstated, so no mock/live claim either.
        Mode::Replay => match app.view_file() {
            Some(file) => format!(
                " REPLAY {}  viewing {file} (read-only) · q quit · enter jump to event · \
                 arrows scrub 10s · pgup/pgdn 5m",
                fmt_clock(app.cursor_ms)
            ),
            None => format!(
                " REPLAY {}  esc live · enter jump to event · arrows scrub 10s · pgup/pgdn 5m  \
                 (10s rollups, 48h retention){mock_tag}",
                fmt_clock(app.cursor_ms)
            ),
        },
        Mode::Live => {
            let paused = if app.paused() { " · PAUSED" } else { "" };
            // Surface the stretched cadence whenever low-power backoff is active, so a
            // sparser chart is explained rather than mistaken for missing data.
            let eff = shared.effective_interval_ms.load(Ordering::Relaxed);
            let cadence = if eff > shared.interval_ms {
                if eff.is_multiple_of(1000) {
                    format!(" · low-power cadence {}s", eff / 1000)
                } else {
                    format!(" · low-power cadence {:.1}s", eff as f64 / 1000.0)
                }
            } else {
                String::new()
            };
            let hint = if app.replay_hint {
                " · replay needs persistence (--no-persist is set)"
            } else {
                ""
            };
            format!(
                " q quit · ←/→/tab device · p pause{paused} · ↑/↓ story · enter/r \
                 replay{cadence}{hint}  —  gpuviewer{mock_tag}"
            )
        }
    };
    let line = Line::from(text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Left);
    f.render_widget(line, area);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use gpuviewer_core::{ProcessKind, ProcessSample};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::app::App;
    use crate::collector::{test_collector, Collector, Engine, EngineConfig};

    /// A mock engine that does not persist (tests never touch the on-disk store here).
    fn mock_engine() -> Engine {
        Engine::new(EngineConfig {
            force_mock: true,
            ..Default::default()
        })
    }

    /// The processes pane must surface `StaticInfo::process_hint` (the WSL2 / privilege
    /// wall explanation) when present, and render unchanged when it is `None` — the mock
    /// sets `None`, so only this test exercises the `Some` branch.
    #[test]
    fn process_hint_renders_in_processes_pane() {
        // Long interval: the collector thread stays quiet while we drive draws by hand.
        let collector = Collector::start(mock_engine(), Duration::from_secs(3600), false);
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
        let collector = Collector::start(mock_engine(), Duration::from_secs(3600), false);
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

    fn draw(app: &App, shared: &Arc<std::sync::Mutex<crate::collector::Shared>>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();
        {
            let sh = shared.lock().unwrap();
            terminal.draw(|f| super::render(f, app, &sh)).unwrap();
        }
        terminal.backend().to_string()
    }

    /// The LIVE process table carries the cpu column and shows the container after the
    /// name ("ollama [docker:3f2a9c1b]") — the same shape replay renders from rollups.
    #[test]
    fn live_process_table_shows_cpu_and_container() {
        let collector = test_collector(None);
        let shared = Arc::clone(&collector.shared);
        let app = App::new(collector);

        shared.lock().unwrap().processes[0] = vec![ProcessSample {
            pid: 7,
            name: "ollama".into(),
            kind: ProcessKind::Compute,
            mem_bytes: Some(3 << 30),
            util_pct: Some(80.0),
            cpu_pct: Some(42.0),
            container: Some("docker:3f2a9c1b".into()),
        }];
        let screen = draw(&app, &shared);
        assert!(
            screen.contains("cpu"),
            "cpu column header missing:\n{screen}"
        );
        assert!(
            screen.contains("ollama [docker:3f2a9c1b]"),
            "container must follow the process name:\n{screen}"
        );
        assert!(screen.contains("42%"), "cpu_pct value missing:\n{screen}");
    }

    /// The footer announces the stretched cadence while low-power backoff is active, and
    /// stays quiet at the configured interval — a sparser chart must be explained, never
    /// left to read as missing data.
    #[test]
    fn footer_shows_low_power_cadence_when_backed_off() {
        let collector = test_collector(None);
        let shared = Arc::clone(&collector.shared);
        let app = App::new(collector);

        // At the configured cadence: no label.
        let screen = draw(&app, &shared);
        assert!(!screen.contains("low-power cadence"));

        // Backed off to 5s: the footer says so.
        shared
            .lock()
            .unwrap()
            .effective_interval_ms
            .store(5000, Ordering::Relaxed);
        let screen = draw(&app, &shared);
        assert!(
            screen.contains("low-power cadence 5s"),
            "active backoff must be announced:\n{screen}"
        );
    }
}
