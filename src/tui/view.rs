//! Rendering. Everything here is a pure function of [`App`].

use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, Padding, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;

use crate::compat::Level;
use crate::deck::{RecipeView, Snapshot};
use crate::runner::Status;
use crate::telemetry::Metric;
use crate::tui::app::{App, Mode, Tab};
use crate::tui::theme::Theme;
use crate::updates::{short, State as UpdState};
use crate::util::{human_bytes, human_duration, human_gb, now_secs};
use unicode_width::UnicodeWidthStr;

pub fn draw(f: &mut Frame, app: &App) {
    let t = app.theme();
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), area);
    let [header, body, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    draw_header(f, app, header);
    match &app.snap {
        None => {
            let p = Paragraph::new(Line::from(vec![
                Span::styled("◐ ", Style::new().fg(t.accent)),
                Span::styled("probing hardware and engines…", Style::new().fg(t.muted)),
            ]))
            .alignment(Alignment::Center);
            let [mid] = Layout::vertical([Constraint::Length(1)]).flex(Flex::Center).areas(body);
            f.render_widget(p, mid);
        }
        Some(s) => match app.tab {
            Tab::Deck => draw_deck(f, app, s, body),
            Tab::Hosts => crate::tui::hostsview::draw_hosts(f, app, body),
            Tab::Topology => crate::tui::topoview::draw_topology(f, app, body),
            Tab::Machine => draw_machine(f, app, s, body),
            Tab::Sources => draw_sources(f, app, s, body),
            Tab::Browse => crate::tui::browseview::draw_browse(f, app, body),
        },
    }
    draw_status(f, app, status);
    if app.mode == Mode::Help {
        draw_help(f, app, area);
    }
    if app.mode == Mode::Themes {
        draw_theme_picker(f, app, area);
    }
    if app.mode == Mode::RunOn {
        draw_run_on(f, app, area);
    }
}

// ───────────────────────── building blocks ─────────────────────────

/// A coloured pill: ` text ` on `bg`, with rounded caps when Nerd Font glyphs are on.
pub(super) fn pill<'a>(text: impl Into<String>, bg: Color, t: &Theme, nerd: bool) -> Vec<Span<'a>> {
    let text = text.into();
    if nerd {
        vec![
            Span::styled("\u{e0b6}", Style::new().fg(bg)),
            Span::styled(text, Style::new().bg(bg).fg(t.on_pill).bold()),
            Span::styled("\u{e0b4}", Style::new().fg(bg)),
        ]
    } else {
        vec![Span::styled(format!(" {text} "), Style::new().bg(bg).fg(t.on_pill).bold())]
    }
}

/// Powerline segments: `[(text, bg)]` joined by arrows (Nerd Font) or flush blocks.
fn segments<'a>(segs: &[(String, Color)], end_bg: Color, t: &Theme, nerd: bool) -> Vec<Span<'a>> {
    let mut out = vec![];
    for (i, (text, bg)) in segs.iter().enumerate() {
        out.push(Span::styled(format!(" {text} "), Style::new().bg(*bg).fg(t.on_pill).bold()));
        let next = segs.get(i + 1).map(|s| s.1).unwrap_or(end_bg);
        if nerd {
            out.push(Span::styled("\u{e0b0}", Style::new().fg(*bg).bg(next)));
        }
    }
    out
}

pub(super) fn panel<'a>(title: Line<'a>, t: &Theme, focused: bool) -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { t.border_focus } else { t.border }))
        .title(title)
        .padding(Padding::horizontal(1))
        .style(Style::new().bg(t.panel))
}

pub(super) fn title<'a>(icon: &str, text: impl Into<String>, extra: Option<String>, t: &Theme) -> Line<'a> {
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(icon.to_string(), Style::new().fg(t.accent)),
        Span::raw(" "),
        Span::styled(text.into(), Style::new().fg(t.fg).bold()),
    ];
    if let Some(e) = extra {
        spans.push(Span::styled(format!(" · {e}"), Style::new().fg(t.muted)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

pub(super) fn level_color(l: Level, t: &Theme) -> Color {
    match l {
        Level::Ok => t.ok,
        Level::Warn => t.warn,
        Level::Fail => t.err,
    }
}

fn status_pill(r: &RecipeView, busy: bool) -> (&'static str, fn(&Theme) -> Color) {
    if busy {
        return ("… busy", |t| t.accent);
    }
    match r.status {
        Status::Loaded => ("● running", |t| t.ok),
        Status::Loading => ("◐ loading", |t| t.warn),
        Status::External => ("● extern", |t| t.info),
        Status::PortBusy => ("! busy", |t| t.warn),
        Status::Stopped => ("○ stopped", |t| t.dim),
    }
}

fn size_text(r: &RecipeView) -> String {
    if r.downloaded && r.size_bytes > 0 {
        human_bytes(r.size_bytes)
    } else {
        r.size_gb.map(|g| format!("~{}", human_gb(g))).unwrap_or_else(|| "?".into())
    }
}

pub(super) fn kv<'a>(k: &str, v: impl Into<String>, t: &Theme) -> Line<'a> {
    Line::from(vec![Span::styled(format!("{k:<10}"), Style::new().fg(t.muted)), Span::styled(v.into(), t.fg)])
}

// ───────────────────────── header & status ─────────────────────────

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let mut spans = segments(&[("◆ lmdeck".into(), t.accent)], t.bar_bg, t, app.nerd);
    spans.push(Span::raw(" "));
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, tab.title());
        spans.push(if *tab == app.tab {
            Span::styled(label, Style::new().fg(t.accent).bg(t.selected_bg).bold())
        } else {
            Span::styled(label, Style::new().fg(t.muted))
        });
    }
    if app.demo() {
        spans.push(Span::raw(" "));
        spans.extend(pill("DEMO DATA", t.warn, t, app.nerd));
    }
    let left = Line::from(spans);
    let mut right = vec![];
    if let Some(s) = &app.snap {
        let hw = &s.hardware;
        let mem = match hw.available_memory_gb {
            Some(a) => {
                format!("mem {} / {}", human_gb((hw.total_memory_gb - a).max(0.0)), human_gb(hw.total_memory_gb))
            }
            None => format!("mem {}", human_gb(hw.total_memory_gb)),
        };
        // Live numbers win over the snapshot's once the sampler has run.
        let mem = match app.local() {
            Some(l) if l.mem_used_gb.is_some() => {
                format!("mem {} / {}", human_gb(l.mem_used_gb.unwrap_or(0.0)), human_gb(l.mem_total_gb))
            }
            _ => mem,
        };
        let (host, platform) = match app.local() {
            Some(l) => (l.hostname.clone(), l.platform.clone()),
            None => (hw.hostname.clone(), hw.platform.clone()),
        };
        right.push(Span::styled(format!("{host} "), Style::new().fg(t.muted)));
        let mut segs = vec![(platform, t.seg[1]), (mem, t.seg[2])];
        if let Some(l) = app.local() {
            let live: Vec<String> = [l.temp().map(|c| format!("{c:.0}°C")), l.power().map(|w| format!("{w:.0}W"))]
                .into_iter()
                .flatten()
                .collect();
            if !live.is_empty() {
                segs.push((live.join(" "), t.seg[0]));
            }
        }
        right.extend(segments(&segs, t.bar_bg, t, false));
    }
    f.render_widget(Block::new().style(Style::new().bg(t.bar_bg)), area);
    f.render_widget(Paragraph::new(left), area);
    f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let (mode, mode_bg) = match app.mode {
        Mode::Normal => ("NORMAL", t.accent),
        Mode::Search => ("SEARCH", t.info),
        Mode::Help => ("HELP", t.seg[0]),
        Mode::Themes => ("THEME", t.seg[2]),
        Mode::RunOn => ("RUN ON", t.seg[1]),
    };
    let mut segs = vec![(mode.to_string(), mode_bg)];
    if app.tab == Tab::Hosts {
        let up = app.hosts.iter().filter(|h| h.online).count();
        segs.push((format!("⌁ {up}/{} up", app.hosts.len()), t.seg[3]));
    } else if app.tab == Tab::Browse {
        segs.push((format!("index {}", app.browse_visible().len()), t.seg[0]));
        segs.push((format!("sort {}", app.browse_sort.as_str()), t.seg[3]));
    } else if let Some(s) = &app.snap {
        let shown = app.visible().len();
        segs.push((format!("{} {shown}", if app.show_all { "all" } else { "deck" }), t.seg[0]));
        segs.push((format!("▶ {} running", s.running()), t.seg[3]));
    }
    if (app.tab == Tab::Browse && app.browse_fit) || (app.tab != Tab::Browse && app.only_fit) {
        segs.push(("fits".into(), t.seg[1]));
    }
    let mut spans = segments(&segs, t.bar_bg, t, app.nerd);
    let query = if app.tab == Tab::Browse { &app.browse_query } else { &app.query };
    if app.mode == Mode::Search || !query.is_empty() {
        spans.push(Span::styled(" /", Style::new().fg(t.info).bold()));
        spans.push(Span::styled(query.clone(), Style::new().fg(t.fg)));
        if app.mode == Mode::Search {
            spans.push(Span::styled("▏", Style::new().fg(t.accent)));
        }
    }
    if let Some(toast) = &app.toast {
        if toast.at.elapsed().as_secs() < 6 {
            let icon = match toast.level {
                Level::Ok => "✓",
                Level::Warn => "!",
                Level::Fail => "✗",
            };
            spans.push(Span::styled(format!("  {icon} {}", toast.text), Style::new().fg(level_color(toast.level, t))));
        }
    }
    let hints: &[(&str, &str)] = match app.mode {
        Mode::Search => &[("↵", "keep"), ("esc", "clear"), ("↑↓", "move")],
        Mode::RunOn => &[("↑↓", "host"), ("↵", "start there"), ("s", "switch there"), ("esc", "cancel")],
        _ if app.tab == Tab::Hosts => {
            &[("↑↓", "host"), ("←→", "model"), ("↵", "run deck pick here"), ("x", "stop"), ("l", "logs"), ("?", "help")]
        }
        _ if app.tab == Tab::Browse => {
            &[("/", "search"), ("↵", "install"), ("S", "sort"), ("f", "fits"), ("r", "refresh index"), ("?", "help")]
        }
        _ if app.tab == Tab::Topology => &[("tab", "view"), ("r", "refresh"), ("?", "help"), ("q", "quit")],
        _ => &[("↵", "start/stop"), ("s", "switch"), ("o", "run on…"), ("p", "pull"), ("l", "logs"), ("?", "help")],
    };
    let mut right = vec![];
    for (k, label) in hints {
        right.push(Span::styled(format!(" {k}"), Style::new().fg(t.accent).bold()));
        right.push(Span::styled(format!(" {label} "), Style::new().fg(t.muted)));
    }
    f.render_widget(Block::new().style(Style::new().bg(t.bar_bg)), area);
    let rw = right.iter().map(|s| s.width() as u16).sum::<u16>().min(area.width);
    let [l, r] = Layout::horizontal([Constraint::Min(0), Constraint::Length(rw)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), l);
    f.render_widget(Paragraph::new(Line::from(right)), r);
}

// ───────────────────────── deck tab ─────────────────────────

fn draw_deck(f: &mut Frame, app: &App, s: &Snapshot, area: Rect) {
    // 55% for the list, widened (up to 62%) only as far as a `⬆` badge needs
    // to be shown whole next to its recipe name.
    let base = area.width * 55 / 100;
    let want = app
        .visible()
        .iter()
        .filter_map(|r| update_badge(r, app.theme()).map(|b| r.name.width() + 1 + b.width()))
        .max()
        .map_or(0, |w| w as u16 + LIST_FIXED_COLS + if app.nerd { 13 } else { 11 });
    let lw = want.clamp(base, (area.width * 62 / 100).max(base));
    let [left, right] = Layout::horizontal([Constraint::Length(lw), Constraint::Min(0)]).areas(area);
    draw_list(f, app, s, left);
    if app.logs_max {
        draw_logs(f, app, right, true);
    } else {
        let [top, bottom] = Layout::vertical([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(right);
        draw_details(f, app, top);
        draw_logs(f, app, bottom, false);
    }
}

/// Deck list columns other than status and recipe: borders and padding (4),
/// the highlight symbol (1), engine/quant/size/dl/fit (38) and six gaps.
const LIST_FIXED_COLS: u16 = 4 + 1 + 10 + 11 + 7 + 2 + 8 + 6;

fn draw_list(f: &mut Frame, app: &App, s: &Snapshot, area: Rect) {
    let t = app.theme();
    let vis = app.visible();
    let total = s.recipes.iter().filter(|r| app.show_all || r.listed).count();
    let extra = if vis.len() == total { format!("{total}") } else { format!("{} of {total}", vis.len()) };
    let block = panel(title("▤", if app.show_all { "All recipes" } else { "Deck" }, Some(extra), t), t, !app.logs_max);
    let pill_w: u16 = if app.nerd { 13 } else { 11 };
    let name_w = (area.width.saturating_sub(LIST_FIXED_COLS + pill_w) as usize).max(14);
    let rows: Vec<Row> = vis
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let busy = app.busy.contains(&r.name);
            let (label, color) = status_pill(r, busy);
            let dl = if r.pulling {
                Span::styled("↓", Style::new().fg(t.accent).bold())
            } else if r.downloaded {
                Span::styled("✓", Style::new().fg(t.ok))
            } else if !r.weights_on.is_empty() {
                // On another host's disk: runnable here over NFS, never copied.
                Span::styled("⛁", Style::new().fg(t.info))
            } else {
                Span::styled("·", Style::new().fg(t.dim))
            };
            let fit = match r.fit {
                Level::Ok => Span::styled("✓ fits", Style::new().fg(t.ok)),
                Level::Warn => Span::styled("! tight", Style::new().fg(t.warn)),
                Level::Fail => Span::styled("✗ no fit", Style::new().fg(t.err)),
            };
            let name = name_cell(r, t, name_w);
            Row::new(vec![
                Cell::from(Line::from(pill(label, color(t), t, app.nerd))),
                Cell::from(name),
                Cell::from(Span::styled(r.engine.clone(), Style::new().fg(t.info))),
                Cell::from(Span::styled(r.quant.clone(), Style::new().fg(t.seg[0]))),
                Cell::from(
                    Line::from(Span::styled(size_text(r), Style::new().fg(t.muted))).alignment(Alignment::Right),
                ),
                Cell::from(dl),
                Cell::from(fit),
            ])
            // Row style sits under the cells, so pills keep their colours when selected.
            .style(if i == app.cursor { Style::new().bg(t.selected_bg) } else { Style::new() })
        })
        .collect();
    let header = Row::new(["status", "recipe", "engine", "quant", "size", "dl", "fit"])
        .style(Style::new().fg(t.dim))
        .bottom_margin(0);
    let table = Table::new(
        rows,
        [
            Constraint::Length(if app.nerd { 13 } else { 11 }),
            Constraint::Min(14),
            Constraint::Length(10),
            Constraint::Length(11),
            Constraint::Length(7),
            Constraint::Length(2),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(block)
    .column_spacing(1)
    .highlight_symbol(Span::styled("▎", Style::new().fg(t.accent)));
    let mut state = TableState::default().with_selected(if vis.is_empty() { None } else { Some(app.cursor) });
    f.render_stateful_widget(table, area, &mut state);
    if vis.is_empty() {
        let msg = if app.query.is_empty() { "no recipes match the filters" } else { "nothing matches that search" };
        let inner = area.inner(ratatui::layout::Margin::new(2, 3));
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(t.muted).italic())), inner);
    }
}

/// The recipe name, its origin when it isn't in the deck, and the update badge,
/// fitted to `w` columns: the badge shrinks to `⬆` and then the name gets an
/// ellipsis, so the badge is never the part that is cut off.
fn name_cell<'a>(r: &RecipeView, t: &Theme, w: usize) -> Line<'a> {
    let origin = if r.listed { String::new() } else { format!(" {}", r.origin) };
    let badge = update_badge(r, t);
    let full = badge.as_ref().map_or(0, |b| 1 + b.width());
    let badge = match badge {
        Some(b) if r.name.width() + origin.width() + full > w => Some(Span::styled("⬆", b.style)),
        b => b,
    };
    let room = w.saturating_sub(badge.as_ref().map_or(0, |b| 1 + b.width()));
    // Where it runs and where its weights live, when that isn't just here.
    let mut tags = vec![];
    if !r.nodes.is_empty() {
        tags.push(format!("×{}", r.nodes.len()));
    }
    if let Some(h) = &r.placed_on {
        tags.push(format!("→{h}"));
    }
    if !r.weights_on.is_empty() && (r.weights_on.len() > 1 || !r.downloaded) {
        tags.push(format!("⛁{}", r.weights_on.join(",")));
    }
    let mut spans = vec![Span::styled(fit_to(&r.name, room), Style::new().fg(t.fg).bold())];
    // The origin label gives way to placement tags, which say more.
    if !origin.is_empty() && tags.is_empty() && r.name.width() + origin.width() <= room {
        spans.push(Span::styled(origin, Style::new().fg(t.dim)));
    }
    if let Some(b) = badge {
        spans.push(Span::raw(" "));
        spans.push(b);
    }
    let mut used: usize = spans.iter().map(|s| s.width()).sum();
    for tag in tags {
        let tag = format!(" {tag}");
        if used + tag.width() > w {
            break;
        }
        used += tag.width();
        spans.push(Span::styled(tag, Style::new().fg(t.info)));
    }
    Line::from(spans)
}

/// `s` cut to `w` columns with a trailing `…` when it doesn't fit.
pub(super) fn fit_to(s: &str, w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

/// `⬆` after a wrapped recipe whose `[source]` repo has newer commits:
/// the warn colour when a pull takes it, muted for a pin (`u` moves it).
fn update_badge<'a>(r: &RecipeView, t: &Theme) -> Option<Span<'a>> {
    let u = r.update.as_ref()?;
    match u.state {
        UpdState::Behind => Some(Span::styled("⬆ update", Style::new().fg(t.warn).bold())),
        UpdState::PinnedNewer => Some(Span::styled("⬆ newer", Style::new().fg(t.info))),
        _ => None,
    }
}

fn update_line<'a>(r: &RecipeView, t: &Theme) -> Option<Line<'a>> {
    let u = r.update.as_ref()?;
    let follows = u.follows.as_deref().map(|b| format!(" ({b})")).unwrap_or_default();
    let (text, color) = match u.state {
        UpdState::Behind => (format!("update available{follows}: {} · press u or p", u.delta()), t.warn),
        UpdState::PinnedNewer => {
            (format!("pinned; upstream{follows} is newer: {} · press u to move the pin", u.delta()), t.info)
        }
        UpdState::Current => {
            (format!("up to date{follows} at {}", u.local.as_deref().map(short).unwrap_or_default()), t.ok)
        }
        UpdState::NotCloned => ("cloned on first start".into(), t.muted),
        UpdState::Unknown => (
            match &u.error {
                Some(e) => format!("unknown ({e})"),
                None => "not checked yet".into(),
            },
            t.muted,
        ),
        UpdState::Disabled => ("update checks are off".into(), t.muted),
    };
    Some(Line::from(vec![
        Span::styled(format!("{:<10}", "upstream"), Style::new().fg(t.muted)),
        Span::styled(text, Style::new().fg(color)),
    ]))
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let Some(r) = app.selected() else {
        f.render_widget(panel(title("◇", "Details", None, t), t, false), area);
        return;
    };
    let mut lines: Vec<Line> = vec![];
    let (label, color) = status_pill(r, app.busy.contains(&r.name));
    let mut head = pill(label, color(t), t, app.nerd);
    head.push(Span::raw(" "));
    let (fit_label, fit_bg) = match r.fit {
        Level::Ok => ("fits", t.ok),
        Level::Warn => ("tight fit", t.warn),
        Level::Fail => ("doesn't fit", t.err),
    };
    head.extend(pill(fit_label, fit_bg, t, app.nerd));
    head.push(Span::raw(" "));
    head.extend(pill(r.engine.clone(), t.seg[1], t, app.nerd));
    head.push(Span::raw(" "));
    head.extend(pill(r.quant.clone(), t.seg[0], t, app.nerd));
    lines.push(Line::from(head));
    if let Some(d) = &r.description {
        lines.push(Line::from(Span::styled(d.clone(), Style::new().fg(t.muted).italic())));
    }
    lines.push(Line::raw(""));
    lines.push(kv("recipe", r.reference.clone(), t));
    if let Some(m) = &r.model {
        let rev = r.revision.as_deref().map(|v| format!(" @{}", &v[..v.len().min(8)])).unwrap_or_default();
        lines.push(kv("model", format!("{m}{rev}"), t));
    }
    let ctx = r.ctx.map(|c| format!(" · ctx {c}")).unwrap_or_default();
    lines.push(kv("serve", format!("{} · :{}{ctx}", r.engine, r.port), t));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<10}", "endpoint"), Style::new().fg(t.muted)),
        Span::styled(r.endpoint.clone(), Style::new().fg(t.info).underlined()),
    ]));
    if let (None, Some(u)) = (&r.model, &r.update) {
        let at = u.git_ref.as_deref().map(|g| format!(" @ {}", if g.len() == 40 { short(g) } else { g.to_string() }));
        lines.push(kv("source", format!("{}{}", u.repo, at.unwrap_or_default()), t));
    }
    let weights = if r.pulling {
        "downloading…".to_string()
    } else if r.model.is_none() && r.update.is_some() {
        "fetched by the wrapped repo's scripts".to_string()
    } else if r.downloaded {
        format!("on disk, {}{}", size_text(r), if r.cached { " (HF cache)" } else { "" })
    } else {
        format!("not downloaded ({}) · press p", size_text(r))
    };
    lines.push(kv("weights", weights, t));
    if !r.weights_on.is_empty() {
        let home = r.weights_home.as_deref().map(|h| format!(" · home {h}")).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{:<10}", "on"), Style::new().fg(t.muted)),
            Span::styled(format!("⛁ {}", r.weights_on.join(", ")), Style::new().fg(t.info)),
            Span::styled(home, Style::new().fg(t.dim)),
        ]));
    }
    if let Some(h) = &r.placed_on {
        lines.push(kv("running", format!("on {h} (weights read over NFS from their home)"), t));
    }
    if !r.nodes.is_empty() {
        let phase = r.cluster_phase.as_deref().map(|p| format!(" · {p}")).unwrap_or_default();
        lines.push(kv("nodes", format!("{} nodes, started in order{phase}", r.nodes.len()), t));
        for (i, n) in r.nodes.iter().enumerate() {
            let c = match n.status.as_str() {
                "loaded" | "running" => t.ok,
                "loading" => t.warn,
                "offline" => t.err,
                _ => t.dim,
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<10}", ""), Style::new()),
                Span::styled(format!("{}. ", i + 1), Style::new().fg(t.dim)),
                Span::styled(format!("{:<8}", n.name), Style::new().fg(t.fg).bold()),
                Span::styled(format!("{:<7}", n.role), Style::new().fg(t.muted)),
                Span::styled(format!("on {:<10}", n.host), Style::new().fg(t.info)),
                Span::styled(format!("● {}", n.status), Style::new().fg(c)),
            ]));
        }
        if let Some(e) = &r.cluster_error {
            lines.push(Line::from(Span::styled(format!("✗ {e}"), Style::new().fg(t.err))));
        }
    }
    if let Some(l) = update_line(r, t) {
        lines.push(l);
    }
    if let Some(need) = r.need_gb {
        lines.push(kv("memory", format!("needs ~{}", human_gb(need)), t));
    }
    if let (Some(pid), Some(at)) = (r.pid, r.started_at) {
        lines.push(kv("process", format!("pid {pid} · up {}", human_duration(now_secs().saturating_sub(at))), t));
    }
    if let Some(m) = app.local().and_then(|l| l.model(&r.name)) {
        let mut parts = vec![];
        if let Some(g) = m.rss_gb {
            let share = m.mem_share_pct.map(|p| format!(" ({p:.0}% of RAM)")).unwrap_or_default();
            parts.push(format!("rss {}{share}", human_gb(g)));
        }
        if let Some(g) = m.gpu_mem_gb {
            parts.push(format!("vram {}", human_gb(g)));
        }
        parts.push(match (m.tokens_per_s, &m.metrics) {
            (Some(v), Some(src)) => format!("{v:.1} tok/s ({src})"),
            (None, Some(_)) => "idle".into(),
            _ => "tok/s n/a (no engine /metrics)".into(),
        });
        if let Some(n) = m.requests_running.filter(|n| *n > 0.0) {
            parts.push(format!("{n:.0} req"));
        }
        lines.push(kv("live", parts.join(" · "), t));
    }
    if r.is_up() {
        if let Some(h) = app.hosts.iter().find(|h| h.local) {
            // One rolling line per metric: the machine-wide picture while this runs.
            for m in [Metric::GpuUtil, Metric::Power, Metric::Temp] {
                let series: Vec<Option<f64>> = h.history.iter().map(|s| m.get(s)).collect();
                let Some(now) = series.last().copied().flatten() else { continue };
                let mut spans = vec![
                    Span::styled(format!("{:<10}", m.label()), Style::new().fg(t.muted)),
                    Span::styled(format!("{:<7}", format!("{now:.0}{}", m.unit())), Style::new().fg(t.fg)),
                ];
                spans.extend(crate::tui::hostsview::spark_spans(&series, m, 30, t, false));
                lines.push(Line::from(spans));
            }
        }
    }
    lines.push(Line::raw(""));
    for c in &r.checks {
        let mark = match c.level {
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Fail => "✗",
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::new().fg(level_color(c.level, t))),
            Span::styled(format!("{:<10}", c.what), Style::new().fg(t.muted)),
            Span::styled(c.detail.clone(), t.fg),
        ]));
    }
    lines.push(Line::raw(""));
    match (&r.command, &r.plan_error) {
        (Some(cmd), _) => {
            lines.push(Line::from(vec![
                Span::styled("$ ", Style::new().fg(t.accent)),
                Span::styled(cmd.clone(), Style::new().fg(t.dim)),
            ]));
        }
        (None, Some(e)) => lines.push(Line::from(Span::styled(format!("✗ {e}"), Style::new().fg(t.err)))),
        _ => {}
    }
    let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(panel(
        title("◇", r.display_name.clone(), None, t),
        t,
        false,
    ));
    f.render_widget(p, area);
}

fn draw_logs(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let t = app.theme();
    let name = app.selected().map(|r| r.name.clone()).unwrap_or_default();
    let file = app
        .log_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|f| f.to_string_lossy().into_owned())
        .filter(|_| !app.logs.is_empty());
    let extra = match (file, app.log_scroll) {
        (Some(f), 0) => Some(format!("{f} · live")),
        (Some(f), n) => Some(format!("{f} · ↑{n}")),
        (None, _) => None,
    };
    let block = panel(title("≡", format!("Logs {name}"), extra, t), t, focused);
    let inner_h = block.inner(area).height as usize;
    let lines: Vec<Line> = if app.logs.is_empty() {
        vec![Line::from(Span::styled("no log yet; start the server with ↵", Style::new().fg(t.dim).italic()))]
    } else {
        let end = app.logs.len().saturating_sub(app.log_scroll);
        let start = end.saturating_sub(inner_h);
        app.logs[start..end]
            .iter()
            .map(|l| {
                let low = l.to_ascii_lowercase();
                let color = if low.contains("error") || low.contains("traceback") || low.contains("fatal") {
                    t.err
                } else if low.contains("warn") {
                    t.warn
                } else if low.contains("ready") || low.contains("listening") || low.contains("started") {
                    t.ok
                } else {
                    t.muted
                };
                Line::from(Span::styled(l.clone(), Style::new().fg(color)))
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ───────────────────────── machine tab ─────────────────────────

pub(super) fn gauge<'a>(label: &str, ratio: f64, text: String, color: Color, t: &Theme) -> Gauge<'a> {
    Gauge::default()
        .block(Block::new().title(Span::styled(label.to_string(), Style::new().fg(t.muted))))
        .gauge_style(Style::new().fg(color).bg(t.gauge_bg))
        .ratio(ratio.clamp(0.0, 1.0))
        .label(Span::styled(text, Style::new().fg(t.fg).bold()))
        .use_unicode(true)
}

fn draw_machine(f: &mut Frame, app: &App, s: &Snapshot, area: Rect) {
    let t = app.theme();
    let hw = &s.hardware;
    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let block = panel(title("◈", "Machine", Some(hw.hostname.clone()), t), t, true);
    let inner = block.inner(left);
    f.render_widget(block, left);
    let gpus = hw.gpus.len() as u16;
    let rows = Layout::vertical(
        [Constraint::Length(7), Constraint::Length(2), Constraint::Length(2)]
            .into_iter()
            .chain((0..gpus).map(|_| Constraint::Length(2)))
            .chain([Constraint::Min(0)]),
    )
    .split(inner);
    let mut info = vec![
        kv("platform", hw.platform.clone(), t),
        kv("cpu", format!("{} ({} cores)", hw.cpu, hw.cores), t),
        kv("system", format!("{} / {}", hw.os, hw.arch), t),
        kv("memory", format!("{}{}", human_gb(hw.total_memory_gb), if hw.unified_memory { " unified" } else { "" }), t),
    ];
    info.push(kv("lmdeck", format!("v{}", s.version), t));
    f.render_widget(Paragraph::new(info).wrap(Wrap { trim: true }), rows[0]);

    let total = hw.total_memory_gb.max(0.001);
    let used = hw.available_memory_gb.map(|a| (total - a).max(0.0));
    let (ratio, text) = match used {
        Some(u) => (u / total, format!("{} used of {}", human_gb(u), human_gb(total))),
        None => (0.0, format!("{} (free unknown)", human_gb(total))),
    };
    let mem_color = if ratio > 0.9 {
        t.err
    } else if ratio > 0.7 {
        t.warn
    } else {
        t.ok
    };
    f.render_widget(gauge("system memory", ratio, text, mem_color, t), rows[1]);
    let committed: f64 = s.recipes.iter().filter(|r| r.is_up()).filter_map(|r| r.need_gb).sum();
    f.render_widget(
        gauge(
            "held by running recipes",
            committed / total,
            format!("~{} across {} server(s)", human_gb(committed), s.running()),
            t.seg[0],
            t,
        ),
        rows[2],
    );
    for (i, g) in hw.gpus.iter().enumerate() {
        let mem = g.memory_gb.map(human_gb).unwrap_or_else(|| "shared".into());
        let cc = g.compute_capability.as_deref().map(|c| format!(" · sm {c}")).unwrap_or_default();
        let ratio = match (g.memory_gb, hw.unified_memory) {
            (Some(m), false) => (m / total).min(1.0),
            _ => 1.0,
        };
        f.render_widget(
            gauge(&format!("gpu {i}: {} {}", g.vendor, g.name), ratio, format!("{mem}{cc}"), t.info, t),
            rows[3 + i],
        );
    }

    let mut lines = vec![Line::from(Span::styled("engines", Style::new().fg(t.accent).bold()))];
    let tool_line = |name: &str, ok: bool, detail: &str| {
        Line::from(vec![
            Span::styled(if ok { "✓ " } else { "✗ " }, Style::new().fg(if ok { t.ok } else { t.dim })),
            Span::styled(format!("{name:<12}"), Style::new().fg(if ok { t.fg } else { t.muted }).bold()),
            Span::styled(detail.to_string(), Style::new().fg(t.dim)),
        ])
    };
    for (n, st) in &s.inventory.engines {
        lines.push(tool_line(n, st.available, &st.detail));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("tools", Style::new().fg(t.accent).bold())));
    for (n, st) in &s.inventory.tools {
        lines.push(tool_line(n, st.available, &st.detail));
    }
    let fit = s.recipes.iter().filter(|r| r.fits()).count();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(format!("{fit}"), Style::new().fg(t.ok).bold()),
        Span::styled(format!(" of {} known recipes fit this machine", s.recipes.len()), Style::new().fg(t.muted)),
    ]));
    let p = Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel(
        title("⚙", "Doctor", Some("r re-probes".into()), t),
        t,
        false,
    ));
    f.render_widget(p, right);
}

// ───────────────────────── sources tab ─────────────────────────

fn draw_sources(f: &mut Frame, app: &App, s: &Snapshot, area: Rect) {
    let t = app.theme();
    let [top, bottom] = Layout::vertical([Constraint::Min(6), Constraint::Length(9)]).areas(area);
    let mut rows: Vec<Row> = vec![];
    let count = |origin: &str| s.recipes.iter().filter(|r| r.origin == origin).count();
    for (name, spec, n, age) in [
        ("deck", "your recipes (~/.config/lmdeck/recipes)".to_string(), count("deck"), None),
        ("builtin", "shipped with lmdeck".to_string(), count("builtin"), None),
    ]
    .into_iter()
    .chain(s.sources.iter().map(|src| {
        (
            src.name.as_str(),
            src.spec.clone(),
            src.recipes.unwrap_or(0),
            Some(
                src.fetched_at
                    .map(|at| format!("{} ago", human_duration(now_secs().saturating_sub(at))))
                    .unwrap_or_else(|| "never fetched".into()),
            ),
        )
    })) {
        rows.push(Row::new(vec![
            Cell::from(Span::styled(name.to_string(), Style::new().fg(t.fg).bold())),
            Cell::from(Span::styled(spec, Style::new().fg(t.info))),
            Cell::from(Line::from(Span::styled(n.to_string(), Style::new().fg(t.accent))).alignment(Alignment::Right)),
            Cell::from(Span::styled(age.unwrap_or_else(|| "local".into()), Style::new().fg(t.muted))),
        ]));
    }
    let table =
        Table::new(rows, [Constraint::Length(16), Constraint::Min(20), Constraint::Length(8), Constraint::Length(16)])
            .header(Row::new(["origin", "spec", "recipes", "fetched"]).style(Style::new().fg(t.dim)))
            .column_spacing(2)
            .block(panel(title("⇄", "Sources", Some(format!("{} configured", s.sources.len())), t), t, true));
    f.render_widget(table, top);
    let mut lines = vec![];
    if s.recipe_errors.is_empty() {
        lines.push(Line::from(Span::styled("✓ every recipe file parsed", Style::new().fg(t.ok))));
    }
    for (file, err) in s.recipe_errors.iter().take(4) {
        lines.push(Line::from(vec![
            Span::styled("✗ ", Style::new().fg(t.err)),
            Span::styled(format!("{file}: "), Style::new().fg(t.fg)),
            Span::styled(err.clone(), Style::new().fg(t.muted)),
        ]));
    }
    lines.push(Line::raw(""));
    for (cmd, what) in [
        ("lmdeck source add github:owner/repo", "add a recipe repo"),
        ("lmdeck source sync", "re-fetch every source"),
        ("lmdeck recipe add origin/name", "copy a recipe into the deck"),
    ] {
        lines.push(Line::from(vec![
            Span::styled("$ ", Style::new().fg(t.accent)),
            Span::styled(format!("{cmd:<40}"), Style::new().fg(t.fg)),
            Span::styled(what, Style::new().fg(t.dim)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(panel(title("✎", "Notes", None, t), t, false)),
        bottom,
    );
}

// ───────────────────────── help ─────────────────────────

pub const KEYS: &[(&str, &str)] = &[
    ("↑ ↓  j k", "move (recipes, or hosts on the hosts tab)"),
    ("↵ / space", "start or stop the selected server"),
    ("s", "switch: stop the others, start this one"),
    ("x", "stop"),
    ("p", "pull: download weights / update a checkout"),
    ("u", "take a ⬆ update (moves a pinned ref)"),
    ("l", "expand / collapse the live log"),
    ("o", "run on…: pick the host (weights stay home)"),
    ("hosts ↵ s", "run the deck's pick on the selected host"),
    ("hosts ← →", "pick one of that host's servers"),
    ("hosts x l", "stop it / tail its log"),
    ("/", "fuzzy search (esc clears)"),
    ("f", "only recipes that fit this machine"),
    ("a", "deck ↔ every known recipe"),
    ("1-6  tab", "deck · hosts · topology · machine · sources · browse"),
    ("browse ↵ i", "add the entry to your deck (lmdeck add)"),
    ("browse S r", "sort name/sparks/speed · fetch the index"),
    ("browse /", "search; tag:exl3 or #sparks=2 filter by tag"),
    ("t", "next theme (saved)"),
    ("T", "theme picker: live preview, ↵ saves"),
    ("r", "refresh now (re-probe engines)"),
    ("?", "this help"),
    ("q  esc", "quit (servers keep running)"),
];

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let w = 58.min(area.width);
    let h = (KEYS.len() as u16 + 5).min(area.height);
    let [v] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let mut lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!("{k:>12}  "), Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::styled(*what, Style::new().fg(t.fg)),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(
        Line::from(Span::styled(format!("theme: {} · any key closes", t.name), Style::new().fg(t.dim)))
            .alignment(Alignment::Center),
    );
    let block = panel(title("?", "Keys", None, t), t, true).padding(Padding::uniform(1));
    f.render_widget(Paragraph::new(lines).block(block), r);
}

// ───────────────────────── run-on picker ─────────────────────────

fn draw_run_on(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let Some(p) = &app.run_on else { return };
    let rows_h: u16 =
        p.rows.iter().map(|r| 2 + u16::from(!r.summary.is_empty()) + r.notes.len().min(2) as u16).sum::<u16>().max(1);
    let w = 92.min(area.width);
    let h = (rows_h + 6).min(area.height.saturating_sub(2)).max(7);
    let [v] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let block = panel(title("⇢", format!("Run {} on…", p.name), None, t), t, true).padding(Padding::horizontal(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [list, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    let mut lines: Vec<Line> = vec![];
    if p.rows.is_empty() {
        lines.push(Line::from(Span::styled("◐ planning on every host…", Style::new().fg(t.muted))));
    }
    for (i, row) in p.rows.iter().enumerate() {
        let on = i == p.cursor;
        let base = if on { Style::new().bg(t.selected_bg) } else { Style::new() };
        let (badge, bc) = match (row.ok, row.mode.as_str()) {
            (false, _) => ("✗ can't", t.err),
            (true, "local") => ("● local", t.ok),
            (true, "nfs") => ("⇄ nfs ro", t.info),
            (true, _) => ("○ no weights", t.warn),
        };
        let pad = (w as usize).saturating_sub(4);
        lines.push(Line::from(vec![
            Span::styled(if on { "▸ " } else { "  " }, base.fg(t.accent).bold()),
            Span::styled(format!("{:<28}", fit_to(&row.label, 27)), base.fg(if row.ok { t.fg } else { t.dim }).bold()),
            Span::styled(format!("{badge:<13}"), base.fg(bc).bold()),
        ]));
        if !row.summary.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(fit_to(&row.summary, pad.saturating_sub(4)), Style::new().fg(t.muted)),
            ]));
        }
        for n in row.notes.iter().take(2) {
            let c = if row.ok { t.warn } else { t.err };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(fit_to(&format!("! {n}"), pad.saturating_sub(4)), Style::new().fg(c)),
            ]));
        }
        lines.push(Line::raw(""));
    }
    f.render_widget(Paragraph::new(lines), list);
    let help = Line::from(vec![
        Span::styled("↑↓", Style::new().fg(t.accent).bold()),
        Span::styled(" host  ", Style::new().fg(t.dim)),
        Span::styled("↵", Style::new().fg(t.accent).bold()),
        Span::styled(" start there  ", Style::new().fg(t.dim)),
        Span::styled("s", Style::new().fg(t.accent).bold()),
        Span::styled(" switch there  ", Style::new().fg(t.dim)),
        Span::styled("esc", Style::new().fg(t.accent).bold()),
        Span::styled(" cancel", Style::new().fg(t.dim)),
    ]);
    let note = Line::from(Span::styled(
        "weights never move: other hosts read the home's copy read-only over CX7 NFS",
        Style::new().fg(t.dim),
    ));
    f.render_widget(Paragraph::new(vec![note, help]), foot);
}

// ───────────────────────── theme picker ─────────────────────────

fn draw_theme_picker(f: &mut Frame, app: &App, area: Rect) {
    use crate::tui::theme::{rgb, themes};
    let t = app.theme();
    let all = crate::theme::all();
    let n = themes().len();
    let w = 62.min(area.width);
    let h = (n as u16 + 5).min(area.height.saturating_sub(2)).max(7);
    let [v] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(v);
    f.render_widget(Clear, r);
    let block = panel(title("◐", "Theme", Some(format!("{}/{n}", app.theme_cursor + 1)), t), t, true)
        .padding(Padding::horizontal(1));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [list, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    let rows = list.height as usize;
    let top = app.theme_cursor.saturating_sub(rows.saturating_sub(1)).min(n.saturating_sub(rows));
    let lines: Vec<Line> = (top..n.min(top + rows))
        .map(|i| {
            let (th, p) = (&themes()[i], &all[i]);
            let on = i == app.theme_cursor;
            let base = if on { Style::new().bg(t.selected_bg) } else { Style::new() };
            let mut spans = vec![
                Span::styled(if on { " ▸ " } else { "   " }, base.fg(t.accent).bold()),
                Span::styled(format!("{:<18}", th.title), if on { base.fg(t.fg).bold() } else { base.fg(t.fg) }),
                Span::styled(format!("{:<7}", if th.light { "light" } else { "dark" }), base.fg(t.dim)),
            ];
            // A tile of the theme's own background with its colours on it.
            spans.push(Span::styled(" ", Style::new().bg(rgb(p.bg))));
            for c in p.swatch()[1..].iter() {
                spans.push(Span::styled("■ ", Style::new().fg(rgb(*c)).bg(rgb(p.bg))));
            }
            spans.push(Span::styled(if i == app.theme_before { " ●" } else { "  " }, base.fg(t.ok)));
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), list);
    let help = Line::from(vec![
        Span::styled("↑↓", Style::new().fg(t.accent).bold()),
        Span::styled(" preview  ", Style::new().fg(t.dim)),
        Span::styled("↵", Style::new().fg(t.accent).bold()),
        Span::styled(" save  ", Style::new().fg(t.dim)),
        Span::styled("esc", Style::new().fg(t.accent).bold()),
        Span::styled(" revert  ", Style::new().fg(t.dim)),
        Span::styled("● current", Style::new().fg(t.ok)),
    ]);
    let src = Line::from(Span::styled(
        if all[app.theme_cursor].slug == crate::theme::DEFAULT {
            "lmdeck's own palette".to_string()
        } else {
            "palette from Omarchy (MIT) · omarchy.org".to_string()
        },
        Style::new().fg(t.dim),
    ));
    f.render_widget(Paragraph::new(vec![src, help]), foot);
}
