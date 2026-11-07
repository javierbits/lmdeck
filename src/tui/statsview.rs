//! Stats tab: local usage recorded from engine `/metrics` counters — a
//! GitHub-style activity calendar, per-model totals, and the selected day's
//! hourly profile. Reads only [`crate::usage::UsageView`]; drawing never
//! probes anything.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::App;
use crate::tui::view::{fit_to, panel, title};
use crate::usage::{self, Counts, DayView, UsageView};

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const LEVELS: [&str; 5] = ["·", "░", "▒", "▓", "█"];

pub fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let Some(snap) = app.snap.as_ref() else { return };
    let local_name = snap.hardware.hostname.clone();
    let (u, hosts) = fleet_usage(app, &snap.usage, &local_name);
    if area.width < 60 || area.height < 12 {
        let msg = if u.has_data() {
            "usage statistics need a wider terminal (the calendar needs room)"
        } else {
            "no usage recorded yet"
        };
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(t.muted))).alignment(Alignment::Center), area);
        return;
    }
    let [left, right] = Layout::horizontal([Constraint::Percentage(57), Constraint::Percentage(43)]).areas(area);
    let cal_h = 13.min(left.height.saturating_sub(6)).max(9);
    let [cal, day] = Layout::vertical([Constraint::Length(cal_h), Constraint::Min(5)]).areas(left);
    let [totals, hours] = Layout::vertical([Constraint::Min(8), Constraint::Length(12)]).areas(right);
    draw_calendar(f, app, &u, hosts.len(), cal);
    draw_day(f, app, &u, day);
    draw_totals(f, app, &u, &hosts, totals);
    draw_hours(f, app, &u, hours);
}

/// The controller's own usage plus every watched host that exposes
/// `/api/usage`. Returns the merged view and each host's contribution.
fn fleet_usage(app: &App, local: &UsageView, local_name: &str) -> (UsageView, Vec<(String, u64, bool)>) {
    let mut fleet = local.clone();
    let mut hosts = vec![(local_name.to_string(), local.total.tokens(), true)];
    for h in &app.hosts {
        if h.local {
            continue;
        }
        let Some(u) = &h.usage else { continue };
        if u.total.tokens() == 0 && !u.recording {
            continue;
        }
        fleet.merge(u);
        hosts.push((h.name.clone(), u.total.tokens(), h.online));
    }
    (fleet, hosts)
}

fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
}

fn level(tokens: u64, max: u64) -> usize {
    if tokens == 0 {
        return 0;
    }
    if max == 0 {
        return 1;
    }
    let frac = ((tokens as f64 + 1.0).ln() / (max as f64 + 1.0).ln()).clamp(0.0, 1.0);
    (1.0 + frac * 3.999) as usize
}

fn day_label(day: i64) -> String {
    format!("{} {}", WEEKDAYS[usage::weekday(day)], usage::date_key(day))
}

fn hour_label(hour: u8) -> String {
    format!("{hour:02}:00")
}

/// The calendar: 7 rows of weekdays, one column per week, today in the last
/// column. Cells are shaded by that day's tokens; the selected day is boxed
/// by its background, today is underlined.
fn draw_calendar(f: &mut Frame, app: &App, u: &UsageView, hosts: usize, area: Rect) {
    let t = app.theme();
    let selected = app.stats_selected_day();
    let max = u.days.iter().map(|d| d.tokens()).max().unwrap_or(0);
    let fleet = if hosts > 1 { format!(" · {hosts} hosts") } else { String::new() };
    let days = if u.active_days == 1 { "day" } else { "days" };
    let block = panel(
        title(
            "▦",
            "Usage",
            Some(format!(
                "{} active {days} · recording {}{fleet}",
                u.active_days,
                if u.recording { "on" } else { "off" }
            )),
            t,
        ),
        t,
        true,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 9 || inner.width < 20 {
        return;
    }
    let weeks = ((inner.width as usize).saturating_sub(4)).clamp(9, 53);
    let today = u.today;
    let start = today - usage::weekday(today) as i64 - (weeks as i64 - 1) * 7;
    let mut lines: Vec<Line> = vec![];
    for (row, name) in WEEKDAYS.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("{name:<4}"), Style::new().fg(t.muted))];
        for col in 0..weeks {
            let day = start + (col as i64) * 7 + row as i64;
            if day > today || day < today - usage::KEEP_DAYS {
                spans.push(Span::raw(" "));
                continue;
            }
            let tokens = u.day(day).map(|d| d.tokens()).unwrap_or(0);
            let lv = level(tokens, max);
            let mut style = Style::new().fg(match lv {
                0 => t.dim,
                1 => t.info,
                _ => t.accent,
            });
            if day == selected {
                style = style.bg(t.selected_bg).fg(t.fg).add_modifier(Modifier::BOLD);
            } else if day == today {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            spans.push(Span::styled(LEVELS[lv], style));
        }
        lines.push(Line::from(spans));
    }
    let mut legend = vec![Span::styled("less ", Style::new().fg(t.dim))];
    for (i, l) in LEVELS.iter().enumerate() {
        legend.push(Span::styled(
            *l,
            Style::new().fg(match i {
                0 => t.dim,
                1 => t.info,
                _ => t.accent,
            }),
        ));
    }
    legend.push(Span::styled(" more", Style::new().fg(t.dim)));
    let recorded = if !u.recording {
        "   · recording is off"
    } else if hosts > 1 {
        "   · recorded on each host"
    } else {
        "   · recorded locally"
    };
    legend.push(Span::styled(recorded, Style::new().fg(if u.recording { t.ok } else { t.warn })));
    lines.push(Line::from(legend));
    f.render_widget(Paragraph::new(lines), inner);
}

/// The selected day: totals and the models that produced them.
fn draw_day(f: &mut Frame, app: &App, u: &UsageView, area: Rect) {
    let t = app.theme();
    let day = app.stats_selected_day();
    let block = panel(title("◇", "Day", Some(day_label(day)), t), t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = inner.height as usize;
    if rows == 0 {
        return;
    }
    let Some(d) = u.day(day) else {
        let why = if u.recording {
            "no tokens recorded that day"
        } else {
            "recording is off (turn on [stats] record in config.toml)"
        };
        f.render_widget(Paragraph::new(Span::styled(why, Style::new().fg(t.muted).italic())), inner);
        return;
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("generated ", Style::new().fg(t.muted)),
            Span::styled(compact(d.gen), Style::new().fg(t.accent).bold()),
            Span::styled("  ·  prompt ", Style::new().fg(t.muted)),
            Span::styled(compact(d.prompt), Style::new().fg(t.fg)),
            Span::styled(
                if d.requests > 0 { format!("  ·  {} requests", d.requests) } else { String::new() },
                Style::new().fg(t.muted),
            ),
        ]),
        Line::from(Span::styled(
            format!("{} models · {} tokens", d.models.len(), compact(d.tokens())),
            Style::new().fg(t.dim),
        )),
        Line::raw(""),
    ];
    let mut models: Vec<(&String, &Counts)> = d.models.iter().collect();
    models.sort_by_key(|(_, c)| std::cmp::Reverse(c.tokens()));
    let room = rows.saturating_sub(lines.len());
    for (name, c) in models.into_iter().take(room) {
        let share = if d.tokens() > 0 { 100.0 * c.tokens() as f64 / d.tokens() as f64 } else { 0.0 };
        let name_w = inner.width as usize;
        let value = format!("{:>8}", compact(c.tokens()));
        let label = fit_to(name, name_w.saturating_sub(value.chars().count() + 8));
        lines.push(Line::from(vec![
            Span::styled(label, Style::new().fg(t.fg)),
            Span::styled(format!("{value} "), Style::new().fg(t.accent)),
            Span::styled(format!("{share:>4.0}%"), Style::new().fg(t.muted)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// All-time totals and the most-used models.
fn draw_totals(f: &mut Frame, app: &App, u: &UsageView, hosts: &[(String, u64, bool)], area: Rect) {
    let t = app.theme();
    let block = panel(title("Σ", "All time", None, t), t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines: Vec<Line> = vec![];
    let kv = |k: &str, v: String, c: ratatui::style::Color| {
        Line::from(vec![
            Span::styled(format!("{k:<12}"), Style::new().fg(t.muted)),
            Span::styled(v, Style::new().fg(c)),
        ])
    };
    lines.push(kv("tokens", compact(u.total.tokens()), if u.total.tokens() > 0 { t.accent } else { t.dim }));
    lines.push(kv("generated", compact(u.total.gen), t.fg));
    lines.push(kv("prompt", compact(u.total.prompt), t.fg));
    if u.total.requests > 0 {
        lines.push(kv("requests", u.total.requests.to_string(), t.fg));
    }
    lines.push(kv("today", compact(u.today_counts.tokens()), t.fg));
    lines.push(kv("last 7 days", compact(u.last7.tokens()), t.fg));
    lines.push(kv("active", format!("{} days · streak {}", u.active_days, u.streak_days), t.muted));
    if let Some(b) = u.best_day {
        lines.push(kv("best day", format!("{} · {}", usage::date_key(b.day), compact(b.tokens)), t.muted));
    }
    if let Some(h) = u.peak_hour {
        lines.push(kv("peak hour", format!("{} · {}", hour_label(h.hour), compact(h.tokens)), t.muted));
    }
    lines.push(kv("models", u.models.len().to_string(), t.muted));
    if hosts.len() > 1 {
        let text: Vec<String> = hosts
            .iter()
            .map(|(name, tokens, online)| format!("{name}{} {}", if *online { "" } else { "?" }, compact(*tokens)))
            .collect();
        let value = fit_to(&text.join(" · "), (inner.width as usize).saturating_sub(13));
        lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "hosts"), Style::new().fg(t.muted)),
            Span::styled(value, Style::new().fg(t.muted)),
        ]));
    }
    lines.push(Line::raw(""));
    let room = (inner.height as usize).saturating_sub(lines.len());
    let top = u.models.len().min(room);
    for m in u.models.iter().take(top) {
        let share = if u.total.tokens() > 0 { 100.0 * m.tokens() as f64 / u.total.tokens() as f64 } else { 0.0 };
        let value = format!("{:>7} {:>4.0}%", compact(m.tokens()), share);
        let label = fit_to(&m.name, (inner.width as usize).saturating_sub(value.chars().count() + 1));
        lines.push(Line::from(vec![
            Span::styled(label, Style::new().fg(t.fg)),
            Span::styled(format!(" {value}"), Style::new().fg(t.accent)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// Hour-of-day profile for the selected day.
fn draw_hours(f: &mut Frame, app: &App, u: &UsageView, area: Rect) {
    let t = app.theme();
    let day = app.stats_selected_day();
    let block = panel(title("◷", "Hours", Some(day_label(day)), t), t, false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 || inner.width < 26 {
        return;
    }
    let hours = u.day(day).map(|d: &DayView| d.hours).unwrap_or([0; 24]);
    let max = hours.iter().copied().max().unwrap_or(0);
    if max == 0 {
        let why = if u.recording { "no tokens that day" } else { "recording is off" };
        f.render_widget(Paragraph::new(Span::styled(why, Style::new().fg(t.muted).italic())), inner);
        return;
    }
    let rows = (inner.height as usize).saturating_sub(1).max(1);
    let mut lines: Vec<Line> = vec![];
    for row in (1..=rows).rev() {
        let mut spans = vec![Span::raw(" ")];
        for tokens in hours.iter() {
            let need = (*tokens as f64 / max as f64 * rows as f64).round() as usize;
            let filled = need >= row;
            spans.push(Span::styled(if filled { "█" } else { " " }, Style::new().fg(t.accent)));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    let mut axis = vec![' '; 49];
    for h in [0usize, 6, 12, 18, 23] {
        let label = h.to_string();
        for (i, c) in label.chars().enumerate() {
            let at = 1 + 2 * h + i;
            if at < axis.len() {
                axis[at] = c;
            }
        }
    }
    lines.push(Line::from(Span::styled(axis.into_iter().collect::<String>(), Style::new().fg(t.dim))));
    // The selected day's own peak, on the right of the calendar date line.
    if let Some((h, v)) = hours.iter().enumerate().max_by_key(|(_, v)| **v) {
        if lines.len() < inner.height as usize {
            lines.push(Line::from(Span::styled(
                format!("peak {} · {}", hour_label(h as u8), compact(*v)),
                Style::new().fg(t.muted),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}
