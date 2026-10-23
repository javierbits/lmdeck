//! The Hosts tab: every machine's live telemetry, with rolling history.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, SparklineBar, Table, TableState, Wrap};
use ratatui::Frame;

use crate::hosts::HostView;
use crate::telemetry::{now_ms, Metric, Sample, Telemetry};
use crate::tui::app::App;
use crate::tui::theme::Theme;
use crate::tui::view::{gauge, panel, title};
use crate::util::{human_duration, human_gb};

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// How far back the ▲/▼ change indicators look.
pub const CHANGE_SECS: u64 = 60;

/// Squeezes a series into `width` cells (max of each bucket), right-aligned:
/// short histories are padded on the left with `None`.
pub fn bucket(series: &[Option<f64>], width: usize) -> Vec<Option<f64>> {
    let n = series.len();
    if width == 0 {
        return vec![];
    }
    if n <= width {
        let mut v = vec![None; width - n];
        v.extend_from_slice(series);
        return v;
    }
    (0..width)
        .map(|i| {
            let (a, b) = (i * n / width, ((i + 1) * n / width).max(i * n / width + 1));
            series[a..b.min(n)].iter().flatten().copied().reduce(f64::max)
        })
        .collect()
}

/// The value range a metric is drawn against.
pub fn scale(m: Metric, series: &[Option<f64>]) -> (f64, f64) {
    let max = series.iter().flatten().copied().fold(0.0f64, f64::max);
    match m {
        Metric::Mem | Metric::GpuUtil | Metric::CpuUtil => (0.0, 100.0),
        Metric::Temp => (25.0, 95.0f64.max(max)),
        Metric::Power => (0.0, (max * 1.15).max(20.0)),
        Metric::Tokens => (0.0, (max * 1.15).max(5.0)),
    }
}

fn norm(v: f64, (lo, hi): (f64, f64)) -> f64 {
    if hi <= lo {
        return 0.0;
    }
    ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
}

fn lerp(a: Color, b: Color, x: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let m = |p: u8, q: u8| (p as f64 + (q as f64 - p as f64) * x.clamp(0.0, 1.0)).round() as u8;
            Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
        }
        _ if x < 0.5 => a,
        _ => b,
    }
}

/// Cool → hot colour for a 0‥1 level (info → ok → warn → err).
pub fn heat(x: f64, t: &Theme) -> Color {
    match x {
        x if x < 0.35 => lerp(t.info, t.ok, x / 0.35),
        x if x < 0.7 => lerp(t.ok, t.warn, (x - 0.35) / 0.35),
        x => lerp(t.warn, t.err, (x - 0.7) / 0.3),
    }
}

/// A one-line sparkline, each cell coloured by its level.
pub fn spark_spans<'a>(series: &[Option<f64>], m: Metric, width: usize, t: &Theme, dim: bool) -> Vec<Span<'a>> {
    let range = scale(m, series);
    bucket(series, width)
        .into_iter()
        .map(|v| match v {
            Some(v) => {
                let x = norm(v, range);
                let c = BARS[((x * 7.0).round() as usize).min(7)];
                Span::styled(c.to_string(), Style::new().fg(if dim { t.dim } else { heat(x, t) }))
            }
            None => Span::styled(" ", Style::new()),
        })
        .collect()
}

/// `▲3` / `▼2` against about a minute ago. `up_is_bad` picks the colours.
pub fn change_span<'a>(d: Option<f64>, up_is_bad: bool, t: &Theme) -> Span<'a> {
    match d {
        Some(d) if d.abs() >= 0.5 => {
            let up = d > 0.0;
            let color = if up == up_is_bad { t.warn } else { t.ok };
            Span::styled(format!("{}{:.0}", if up { "▲" } else { "▼" }, d.abs()), Style::new().fg(color))
        }
        Some(_) => Span::styled("·", Style::new().fg(t.dim)),
        None => Span::raw(""),
    }
}

/// Change of a metric over [`CHANGE_SECS`] in a host's history.
pub fn change(samples: &[Sample], m: Metric) -> Option<f64> {
    let last = samples.last()?;
    let now = m.get(last)?;
    let target = last.at.saturating_sub(CHANGE_SECS * 1000);
    let then = samples.iter().rev().find(|s| s.at <= target).and_then(|s| m.get(s))?;
    Some(now - then)
}

/// `██████░░░░` coloured by level.
fn bar<'a>(ratio: f64, width: usize, t: &Theme, dim: bool) -> Vec<Span<'a>> {
    let r = ratio.clamp(0.0, 1.0);
    let full = (r * width as f64).round() as usize;
    let color = if dim { t.dim } else { heat(r, t) };
    vec![
        Span::styled("█".repeat(full), Style::new().fg(color)),
        Span::styled("░".repeat(width - full.min(width)), Style::new().fg(t.gauge_bg)),
    ]
}

fn opt(v: Option<f64>, fmt: impl Fn(f64) -> String) -> String {
    v.map(fmt).unwrap_or_else(|| "–".into())
}

fn series(h: &HostView, m: Metric) -> Vec<Option<f64>> {
    h.history.iter().map(|s| m.get(s)).collect()
}

/// `3m ago`.
fn ago(ms: Option<u64>) -> String {
    match ms {
        Some(at) => format!("{} ago", human_duration(now_ms().saturating_sub(at) / 1000)),
        None => "never".into(),
    }
}

pub fn draw_hosts(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    if app.hosts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled("◐ sampling telemetry…", Style::new().fg(t.muted))))
            .block(panel(title("⌁", "Hosts", None, t), t, true));
        f.render_widget(p, area);
        return;
    }
    let table_h = (app.hosts.len() as u16 + 3).clamp(5, area.height / 2);
    let [top, mid, strip] =
        Layout::vertical([Constraint::Length(table_h), Constraint::Min(6), Constraint::Length(3)]).areas(area);
    draw_table(f, app, top);
    let [left, right] = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(mid);
    if let Some(h) = app.selected_host() {
        draw_gauges(f, app, h, left);
        draw_history(f, app, h, right);
        draw_strip(f, app, h, strip);
    }
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let up = app.hosts.iter().filter(|h| h.online).count();
    let extra = format!("{up}/{} online · ▲▼ vs {CHANGE_SECS}s ago · history {}", app.hosts.len(), history_span(app));
    let block = panel(title("⌁", "Hosts", Some(extra), t), t, true);
    let hist_w = (area.width.saturating_sub(86) / 4).clamp(6, 40) as usize;
    let rows: Vec<Row> = app
        .hosts
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let dim = !h.online;
            let fg = if dim { t.dim } else { t.fg };
            let tel = h.telemetry.as_ref();
            let dot = if h.online {
                Span::styled("●", Style::new().fg(t.ok))
            } else {
                Span::styled("○", Style::new().fg(t.err))
            };
            let name = Line::from(vec![
                dot,
                Span::raw(" "),
                Span::styled(h.name.clone(), Style::new().fg(fg).bold()),
                Span::styled(if h.local { " ⌂" } else { "" }, Style::new().fg(t.accent)),
            ]);
            let mut mem = vec![];
            match tel.and_then(|x| x.mem_pct().map(|p| (p, x))) {
                Some((p, x)) => {
                    mem.extend(bar(p / 100.0, 10, t, dim));
                    mem.push(Span::styled(
                        format!(" {:>3.0}% {:>4.0}/{:.0}G", p, x.mem_used_gb.unwrap_or(0.0), x.mem_total_gb),
                        Style::new().fg(fg),
                    ));
                }
                None => mem.push(Span::styled("–", Style::new().fg(t.dim))),
            }
            let num = |v: Option<f64>, m: Metric, up_is_bad: bool, fmt: &dyn Fn(f64) -> String| {
                let mut s = vec![Span::styled(opt(v, fmt), Style::new().fg(fg))];
                if h.online {
                    s.push(change_span(change(&h.history, m), up_is_bad, t));
                }
                Line::from(s)
            };
            let hist = |m: Metric| Line::from(spark_spans(&series(h, m), m, hist_w, t, dim));
            Row::new(vec![
                Cell::from(name),
                Cell::from(Line::from(mem)),
                Cell::from(num(tel.and_then(Telemetry::temp), Metric::Temp, true, &|c| format!("{c:.0}°"))),
                Cell::from(num(tel.and_then(Telemetry::power), Metric::Power, true, &|w| format!("{w:.0}W"))),
                Cell::from(num(tel.and_then(Telemetry::gpu_util), Metric::GpuUtil, true, &|u| format!("{u:.0}%"))),
                Cell::from(Span::styled(
                    opt(tel.and_then(|x| x.cpu_util_pct), |u| format!("{u:.0}%")),
                    Style::new().fg(fg),
                )),
                Cell::from(num(tel.and_then(Telemetry::tokens_per_s), Metric::Tokens, false, &|v| format!("{v:.0}"))),
                Cell::from(hist(Metric::Mem)),
                Cell::from(hist(Metric::Temp)),
                Cell::from(hist(Metric::Power)),
                Cell::from(hist(Metric::GpuUtil)),
            ])
            .style(if i == app.host_cursor { Style::new().bg(t.selected_bg) } else { Style::new() })
        })
        .collect();
    let header = Row::new(["host", "memory", "temp", "power", "gpu", "cpu", "tok/s", "mem", "temp", "power", "gpu"])
        .style(Style::new().fg(t.dim));
    let w = hist_w as u16;
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(25),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(w),
            Constraint::Length(w),
            Constraint::Length(w),
            Constraint::Length(w),
        ],
    )
    .header(header)
    .block(block)
    .column_spacing(1)
    .highlight_symbol(Span::styled("▎", Style::new().fg(t.accent)));
    let mut state = TableState::default().with_selected(Some(app.host_cursor));
    f.render_stateful_widget(table, area, &mut state);
}

fn history_span(app: &App) -> String {
    let spans: Vec<u64> =
        app.hosts.iter().filter_map(|h| Some(h.history.last()?.at.saturating_sub(h.history.first()?.at))).collect();
    let max = spans.into_iter().max().unwrap_or(0) / 1000;
    if max == 0 {
        "warming up".into()
    } else {
        human_duration(max)
    }
}

fn draw_gauges(f: &mut Frame, app: &App, h: &HostView, area: Rect) {
    let t = app.theme();
    let tel = h.telemetry.as_ref();
    let platform = tel.map(|x| x.platform.clone()).filter(|p| !p.is_empty());
    let block = panel(title("◈", h.name.clone(), platform, t), t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(x) = tel else {
        let msg = format!("no telemetry yet: {}", h.error.as_deref().unwrap_or("waiting"));
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(t.muted))).wrap(Wrap { trim: true }), inner);
        return;
    };
    let dim = !h.online;
    let color = |r: f64| if dim { t.dim } else { heat(r, t) };
    let mut gauges: Vec<(String, f64, String)> = vec![];
    if let Some(p) = x.mem_pct() {
        let what = if x.unified_memory { "memory (unified: CPU + GPU)" } else { "system memory" };
        let pressure = x.mem_pressure_pct.map(|p| format!(" · pressure {p:.0}%")).unwrap_or_default();
        gauges.push((
            what.into(),
            p / 100.0,
            format!("{} / {}{pressure}", human_gb(x.mem_used_gb.unwrap_or(0.0)), human_gb(x.mem_total_gb)),
        ));
    }
    for g in &x.gpus {
        if let (Some(u), Some(tot)) = (g.mem_used_gb, g.mem_total_gb) {
            gauges.push((
                format!("gpu {} vram", g.index),
                u / tot.max(0.001),
                format!("{} / {}", human_gb(u), human_gb(tot)),
            ));
        }
        if let Some(u) = g.util_pct {
            let clock = g.sm_clock_mhz.map(|c| format!(" · {c:.0} MHz")).unwrap_or_default();
            gauges.push((format!("gpu {} util", g.index), u / 100.0, format!("{u:.0}%{clock}")));
        }
    }
    if let Some(c) = x.temp() {
        let src = if x.gpu_temp().is_some() { "gpu temp" } else { "cpu temp" };
        let cpu =
            x.cpu_temp_c.filter(|_| x.gpu_temp().is_some()).map(|c| format!(" · cpu {c:.0}°C")).unwrap_or_default();
        gauges.push((src.into(), norm(c, (25.0, 95.0)), format!("{c:.0}°C{cpu}")));
    }
    if let Some(w) = x.power() {
        let peak = h.history.iter().filter_map(|s| s.power_w).fold(w, f64::max);
        let limit = x.gpus.iter().filter_map(|g| g.power_limit_w).reduce(|a, b| a + b);
        let (what, max) = match (x.system_power_w, limit) {
            (Some(_), _) => ("system power".to_string(), peak.max(1.0)),
            (None, Some(l)) => ("gpu power (vs limit)".to_string(), l),
            (None, None) => ("gpu power (vs peak)".to_string(), peak.max(1.0)),
        };
        gauges.push((what, w / max, format!("{w:.0} W · peak {peak:.0} W")));
    }
    if let Some(u) = x.cpu_util_pct {
        let load = x.load.map(|l| format!(" · load {:.1}", l[0])).unwrap_or_default();
        gauges.push(("cpu".into(), u / 100.0, format!("{u:.0}% of {} cores{load}", x.cpu_cores)));
    } else if let Some(l) = x.load {
        let r = l[0] / x.cpu_cores.max(1) as f64;
        gauges.push(("cpu load".into(), r, format!("{:.2} {:.2} {:.2} · {} cores", l[0], l[1], l[2], x.cpu_cores)));
    }
    let models_h = (x.models.len() as u16 + 1).max(2);
    let fit = (inner.height.saturating_sub(models_h) / 2) as usize;
    gauges.truncate(fit);
    let mut cons: Vec<Constraint> = gauges.iter().map(|_| Constraint::Length(2)).collect();
    cons.push(Constraint::Min(0));
    let rows = Layout::vertical(cons).split(inner);
    for (i, (label, ratio, text)) in gauges.iter().enumerate() {
        f.render_widget(gauge(label, *ratio, text.clone(), color(*ratio), t), rows[i]);
    }
    let mut lines = vec![Line::from(Span::styled("models", Style::new().fg(t.accent).bold()))];
    if x.models.is_empty() {
        lines.push(Line::from(Span::styled("nothing running", Style::new().fg(t.dim).italic())));
    }
    for m in &x.models {
        lines.push(model_line(m, t, dim));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), rows[gauges.len()]);
}

/// `▸ name  vllm :8000  pid 4100  64.0G 53%  52 tok/s`
pub fn model_line<'a>(m: &crate::telemetry::ModelTel, t: &Theme, dim: bool) -> Line<'a> {
    let fg = if dim { t.dim } else { t.fg };
    let mut s = vec![
        Span::styled("▸ ", Style::new().fg(t.accent)),
        Span::styled(m.name.clone(), Style::new().fg(fg).bold()),
        Span::styled(format!(" {} :{}", m.engine, m.port), Style::new().fg(t.info)),
    ];
    if let Some(p) = m.pid {
        s.push(Span::styled(format!(" pid {p}"), Style::new().fg(t.muted)));
    }
    if let Some(g) = m.rss_gb {
        let share = m.mem_share_pct.map(|p| format!(" {p:.0}%")).unwrap_or_default();
        s.push(Span::styled(format!(" rss {}{share}", human_gb(g)), Style::new().fg(fg)));
    }
    if let Some(g) = m.gpu_mem_gb {
        s.push(Span::styled(format!(" vram {}", human_gb(g)), Style::new().fg(fg)));
    }
    match m.tokens_per_s {
        Some(v) => {
            s.push(Span::styled(format!(" {v:.1} tok/s"), Style::new().fg(if dim { t.dim } else { t.ok }).bold()))
        }
        None if m.metrics.is_some() => s.push(Span::styled(" idle", Style::new().fg(t.dim))),
        None => {}
    }
    Line::from(s)
}

fn draw_history(f: &mut Frame, app: &App, h: &HostView, area: Rect) {
    let t = app.theme();
    let block = panel(title("≋", "History", Some(format!("{} samples", h.history.len())), t), t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let metrics: Vec<Metric> =
        Metric::ALL.into_iter().filter(|m| h.history.iter().any(|s| m.get(s).is_some())).take(5).collect();
    if metrics.is_empty() {
        f.render_widget(Paragraph::new(Span::styled("no samples yet", Style::new().fg(t.dim))), inner);
        return;
    }
    let rows = Layout::vertical(metrics.iter().map(|_| Constraint::Ratio(1, metrics.len() as u32))).split(inner);
    for (m, r) in metrics.iter().zip(rows.iter()) {
        let s = series(h, *m);
        let vals: Vec<f64> = s.iter().flatten().copied().collect();
        let (lo, hi) = (vals.iter().copied().fold(f64::MAX, f64::min), vals.iter().copied().fold(0.0, f64::max));
        let now = s.last().copied().flatten();
        let mut head = vec![
            Span::styled(format!("{:<6}", m.label()), Style::new().fg(t.muted)),
            Span::styled(opt(now, |v| format!("{v:.0}{}", m.unit())), Style::new().fg(t.fg).bold()),
            Span::raw(" "),
            change_span(change(&h.history, *m), !matches!(m, Metric::Tokens), t),
        ];
        if !vals.is_empty() {
            head.push(Span::styled(format!("  min {lo:.0} · max {hi:.0}"), Style::new().fg(t.dim)));
        }
        let [label, chart] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(*r);
        f.render_widget(Paragraph::new(Line::from(head)), label);
        if chart.height == 0 {
            continue;
        }
        let range = scale(*m, &s);
        let w = chart.width as usize;
        let bars: Vec<SparklineBar> = bucket(&s, w)
            .into_iter()
            .map(|v| {
                let x = v.map(|v| norm(v, range));
                let bar = SparklineBar::from(x.map(|x| (x * 1000.0).round() as u64));
                bar.style(x.map(|x| Style::new().fg(if h.online { heat(x, t) } else { t.dim })))
            })
            .collect();
        f.render_widget(Sparkline::default().data(bars).max(1000).absent_value_symbol(" "), chart);
    }
}

fn draw_strip(f: &mut Frame, app: &App, h: &HostView, area: Rect) {
    let t = app.theme();
    let sep = || Span::styled("  │  ", Style::new().fg(t.dim));
    let mut l1 = vec![
        Span::styled(h.name.clone(), Style::new().fg(t.accent).bold()),
        Span::raw(" "),
        Span::styled(h.url.clone().unwrap_or_else(|| "this machine".into()), Style::new().fg(t.info)),
        sep(),
    ];
    if h.online {
        l1.push(Span::styled(format!("● online {}ms", h.latency_ms.unwrap_or(0)), Style::new().fg(t.ok)));
    } else {
        l1.push(Span::styled(
            format!(
                "○ offline: {} · last seen {} · values are the last known",
                h.error.as_deref().unwrap_or("?"),
                ago(h.last_seen)
            ),
            Style::new().fg(t.err),
        ));
    }
    let mut l2 = vec![];
    if let Some(x) = &h.telemetry {
        l1.push(sep());
        l1.push(Span::styled(
            format!("{} {}/{} · {} cores", x.hostname, x.os, x.arch, x.cpu_cores),
            Style::new().fg(t.muted),
        ));
        if let Some(l) = x.load {
            l1.push(Span::styled(format!(" · load {:.1} {:.1} {:.1}", l[0], l[1], l[2]), Style::new().fg(t.muted)));
        }
        if let (Some(w), Some(src)) = (x.system_power_w, &x.system_power_source) {
            l2.push(Span::styled(format!("power {w:.0} W ({src})"), Style::new().fg(t.muted)));
            l2.push(sep());
        }
        if let (Some(c), Some(src)) = (x.cpu_temp_c, &x.cpu_temp_source) {
            l2.push(Span::styled(format!("cpu {c:.0}°C ({src})"), Style::new().fg(t.muted)));
            l2.push(sep());
        }
        for r in x.rails.iter().filter(|r| r.label != "sys_total").take(6) {
            l2.push(Span::styled(format!("{} {:.1}W ", r.label, r.watts), Style::new().fg(t.dim)));
        }
        l2.push(Span::styled(format!("sources: {}", x.sources.join(", ")), Style::new().fg(t.dim)));
        if x.demo {
            l2.push(sep());
            l2.push(Span::styled("DEMO DATA", Style::new().fg(t.warn).bold()));
        }
    }
    let block = Block::new().borders(Borders::TOP).border_style(Style::new().fg(t.border)).style(Style::new().bg(t.bg));
    f.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2)]).block(block).alignment(Alignment::Left), area);
}
