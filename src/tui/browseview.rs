//! The Browse tab: the recipe index, what fits, and one-key install.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::browse::Row as BrowseRow;
use crate::compat::Level;
use crate::tui::app::{split_tags, App};
use crate::tui::theme::Theme;
use crate::tui::view::{fit_to, kv, level_color, panel, title};
use unicode_width::UnicodeWidthStr;

fn state_span<'a>(r: &BrowseRow, busy: bool, t: &Theme) -> Span<'a> {
    if busy {
        return Span::styled("…", Style::new().fg(t.accent).bold());
    }
    match r.installed {
        "deck" => Span::styled("●", Style::new().fg(t.ok)),
        "builtin" => Span::styled("◆", Style::new().fg(t.info)),
        _ => Span::styled("○", Style::new().fg(t.dim)),
    }
}

fn fit_span<'a>(l: Level, t: &Theme) -> Span<'a> {
    match l {
        Level::Ok => Span::styled("✓ fits", Style::new().fg(t.ok)),
        Level::Warn => Span::styled("! tight", Style::new().fg(t.warn)),
        Level::Fail => Span::styled("✗ no fit", Style::new().fg(t.err)),
    }
}

pub fn draw_browse(f: &mut Frame, app: &App, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Min(0)]).areas(area);
    draw_list(f, app, left);
    let [top, bottom] = Layout::vertical([Constraint::Min(8), Constraint::Length(9)]).areas(right);
    draw_details(f, app, top);
    draw_index(f, app, bottom);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let vis = app.browse_visible();
    let total = app.index.as_ref().map_or(0, |l| l.index.entries.len());
    let shown = if vis.len() == total { format!("{total}") } else { format!("{} of {total}", vis.len()) };
    let mut extra = format!("{shown} · sort {}", app.browse_sort.as_str());
    if app.browse_fit {
        extra.push_str(" · fits");
    }
    let block = panel(title("⌕", "Recipe index", Some(extra), t), t, true);
    // borders+padding 4, highlight 1, state 1, engine 11, sparks 3, speed 10, fit 8, gaps 6
    let name_w = (area.width.saturating_sub(4 + 1 + 1 + 11 + 3 + 10 + 8 + 6) as usize).max(12);
    let rows: Vec<Row> = vis
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let busy = app.busy.contains(&r.entry.name);
            let badge = r.update.as_ref().map(|_| Span::styled(" ⬆", Style::new().fg(t.warn).bold()));
            let room = name_w.saturating_sub(if badge.is_some() { 2 } else { 0 });
            let mut name = vec![Span::styled(fit_to(&r.entry.name, room), Style::new().fg(t.fg).bold())];
            name.extend(badge);
            let speed = r.entry.speed.as_ref().map(|s| format!("{:.1} t/s", s.tok_s)).unwrap_or_else(|| "–".into());
            Row::new(vec![
                Cell::from(state_span(r, busy, t)),
                Cell::from(Line::from(name)),
                Cell::from(Span::styled(r.entry.tags.engine.clone().unwrap_or_default(), Style::new().fg(t.info))),
                Cell::from(
                    Line::from(Span::styled(format!("×{}", r.entry.sparks()), Style::new().fg(t.seg[0])))
                        .alignment(Alignment::Right),
                ),
                Cell::from(Line::from(Span::styled(speed, Style::new().fg(t.muted))).alignment(Alignment::Right)),
                Cell::from(fit_span(r.fit, t)),
            ])
            .style(if i == app.browse_cursor { Style::new().bg(t.selected_bg) } else { Style::new() })
        })
        .collect();
    let header = Row::new(["", "entry", "engine", "spk", "speed", "fit"]).style(Style::new().fg(t.dim));
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Min(12),
            Constraint::Length(11),
            Constraint::Length(3),
            Constraint::Length(10),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(block)
    .column_spacing(1)
    .highlight_symbol(Span::styled("▎", Style::new().fg(t.accent)));
    let mut state = TableState::default().with_selected(if vis.is_empty() { None } else { Some(app.browse_cursor) });
    f.render_stateful_widget(table, area, &mut state);
    if vis.is_empty() {
        let msg = match &app.index {
            Some(l) if l.index.entries.is_empty() => "the index is empty or not fetched yet · r fetches it",
            None => "the index is empty or not fetched yet · r fetches it",
            _ => "nothing matches that search",
        };
        let inner = area.inner(ratatui::layout::Margin::new(2, 3));
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(t.muted).italic())), inner);
    }
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let Some(r) = app.browse_selected() else {
        f.render_widget(panel(title("◇", "Details", None, t), t, false), area);
        return;
    };
    let e = &r.entry;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(e.name.clone(), Style::new().fg(t.accent).bold()))];
    if !e.description.is_empty() {
        lines.push(Line::from(Span::styled(e.description.clone(), Style::new().fg(t.fg))));
    }
    lines.push(Line::raw(""));
    let state = match r.installed {
        "deck" => format!("● in your deck as {}", r.installed_as.clone().unwrap_or_default()),
        "builtin" => {
            format!("◆ ships with lmdeck as {} (↵ copies it to your deck)", r.installed_as.clone().unwrap_or_default())
        }
        _ => "○ not installed".into(),
    };
    lines.push(kv("state", state, t));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<10}", "fit"), Style::new().fg(t.muted)),
        fit_span(r.fit, t),
        Span::styled(format!("  {}", r.fit_reason), Style::new().fg(level_color(r.fit, t))),
    ]));
    if let Some(u) = &r.update {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", "update"), Style::new().fg(t.muted)),
            Span::styled(u.clone(), Style::new().fg(t.warn)),
        ]));
    }
    lines.push(kv("repo", e.repo.clone(), t));
    if let Some(g) = &e.git_ref {
        lines.push(kv("ref", crate::updates::short(g), t));
    }
    lines.push(kv("readme", r.readme.clone(), t));
    let tags: Vec<String> = e.tags.pairs().into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    lines.push(kv("tags", tags.join("  "), t));
    if let Some(g) = e.size_gb {
        lines.push(kv("weights", format!("~{g:.0} GB"), t));
    }
    if let Some(s) = &e.speed {
        let note = if s.note.is_empty() { String::new() } else { format!("  ({})", s.note) };
        lines.push(kv("speed", format!("{:.1} tok/s{note}", s.tok_s), t));
    }
    lines.push(kv(
        "recipe",
        if e.recipe.is_some() { "native recipe in the index" } else { "wraps the repo's scripts" },
        t,
    ));
    lines.push(Line::raw(""));
    let (key, text) = match r.installed {
        "deck" => ("", "already in your deck · Deck tab to start it".to_string()),
        _ => ("↵ ", r.install.clone()),
    };
    lines.push(Line::from(vec![
        Span::styled(key, Style::new().fg(t.accent).bold()),
        Span::styled(text, Style::new().fg(t.ok)),
    ]));
    let block = panel(title("◇", "Details", None, t), t, false);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

fn draw_index(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let mut lines = vec![];
    match &app.index {
        Some(l) => {
            let age = if app.index_busy { "fetching…".to_string() } else { format!("fetched {}", l.age_text()) };
            lines.push(kv("source", l.url.clone(), t));
            lines.push(kv("cache", format!("{age} · schema {}", l.index.schema), t));
            if let Some(h) = &l.hint {
                lines.push(Line::from(Span::styled(format!("! {h}"), Style::new().fg(t.warn))));
            }
            let mut by_key: Vec<(String, Vec<String>)> = vec![];
            for (k, v, n) in crate::browse::tag_values(&l.index) {
                let label = format!("{v}({n})");
                match by_key.iter_mut().find(|x| x.0 == k) {
                    Some(x) => x.1.push(label),
                    None => by_key.push((k, vec![label])),
                }
            }
            let (_, active) = split_tags(&app.browse_query);
            for (k, vs) in by_key.iter().filter(|(k, _)| k == "engine" || k == "quant" || k == "sparks") {
                let w = area.width.saturating_sub(16) as usize;
                let mut s = vs.join(" ");
                if s.width() > w {
                    s = fit_to(&s, w);
                }
                lines.push(kv(k, s, t));
            }
            if !active.is_empty() {
                lines.push(kv("filter", active.join(" + "), t));
            }
        }
        None => lines.push(Line::from(Span::styled("no index loaded", Style::new().fg(t.muted)))),
    }
    lines.push(Line::from(Span::styled("/ search · tag:exl3 · #sparks=2", Style::new().fg(t.dim))));
    let block = panel(title("⌁", "Index", None, t), t, false);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}
