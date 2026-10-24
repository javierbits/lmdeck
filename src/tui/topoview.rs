//! Topology tab: hosts as boxes with braille device icons, switches as hubs,
//! cables as edges. Layout is computed from the graph (seeded, overlap-free).
//! When the terminal is too narrow, falls back to a segment-grouped list.

use ratatui::layout::{Constraint, Layout};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::icons::{self, IconKind};
use crate::topology::{TopoHost, Topology};
use crate::tui::app::App;
use crate::tui::theme::Theme;
use crate::tui::view::{panel, title};

const HOST_W: u16 = 20;
const HOST_H: u16 = 10;
const SWITCH_W: u16 = 16;
const SWITCH_H: u16 = 5;

#[derive(Clone)]
pub(crate) struct Pos {
    id: String,
    x: i32,
    y: i32,
    w: u16,
    h: u16,
    kind: NodeKind,
}

#[derive(Clone, Copy, PartialEq)]
enum NodeKind {
    Host,
    Switch,
}

/// Draw the topology diagram into `area`. Returns false when a list fallback was used.
pub fn draw(frame: &mut Frame, area: Rect, topo: &Topology, theme: &Theme, focus: usize) -> bool {
    if area.width < 20 || area.height < 8 {
        draw_list(frame, area, topo, theme);
        return false;
    }
    let positions = layout(topo, area.width, area.height);
    if positions.is_empty() {
        draw_list(frame, area, topo, theme);
        return false;
    }
    // Edges first (under nodes).
    for l in &topo.links {
        let a = positions.iter().find(|p| p.id == l.link.a.host);
        let b = positions.iter().find(|p| p.id == l.link.b.host);
        if let (Some(a), Some(b)) = (a, b) {
            let color = edge_color(theme, l.link.kind, !l.active.is_empty());
            draw_edge(frame, area, a, b, color, !l.active.is_empty());
        }
    }
    let mut host_i = 0usize;
    for p in &positions {
        match p.kind {
            NodeKind::Host => {
                if let Some(h) = topo.host(&p.id) {
                    draw_host(frame, area, p, h, theme, host_i == focus);
                    host_i += 1;
                }
            }
            NodeKind::Switch => {
                if let Some(sw) = topo.switches.iter().find(|s| s.id == p.id) {
                    draw_switch(frame, area, p, &sw.name, sw.ports, theme);
                }
            }
        }
    }
    if topo.demo {
        let note = format!(" DEMO DATA — {} ", topo.demo_topo.as_deref().unwrap_or("tri"));
        let w = note.width().min(area.width as usize) as u16;
        let r = Rect { x: area.x, y: area.y, width: w, height: 1 };
        frame.render_widget(Paragraph::new(Span::styled(note, Style::new().fg(theme.warn).bold())), r);
    }
    true
}

fn edge_color(t: &Theme, kind: crate::fabric::LinkKind, active: bool) -> Color {
    use crate::fabric::LinkKind::*;
    if active {
        return t.accent;
    }
    match kind {
        Cx7 => t.ok,
        Lan => t.muted,
        Tailscale => t.warn,
    }
}

fn draw_edge(frame: &mut Frame, area: Rect, a: &Pos, b: &Pos, color: Color, thick: bool) {
    let (ax, ay) = (a.x + a.w as i32 / 2, a.y + a.h as i32 / 2);
    let (bx, by) = (b.x + b.w as i32 / 2, b.y + b.h as i32 / 2);
    // Bresenham in screen space.
    let (mut x0, mut y0) = (ax, ay);
    let (x1, y1) = (bx, by);
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let ch = if thick { "━" } else { "─" };
    let vch = if thick { "┃" } else { "│" };
    loop {
        if (x0 != ax || y0 != ay) && x0 >= 0 && y0 >= 0 {
            let (sx0, sy0) = (area.x as i32 + x0, area.y as i32 + y0);
            if sx0 >= area.x as i32
                && sy0 >= area.y as i32
                && sx0 < (area.x + area.width) as i32
                && sy0 < (area.y + area.height) as i32
            {
                let on_node =
                    [a, b].iter().any(|p| x0 >= p.x && x0 < p.x + p.w as i32 && y0 >= p.y && y0 < p.y + p.h as i32);
                if !on_node {
                    let glyph = if dx >= dy.abs() { ch } else { vch };
                    frame.render_widget(
                        Paragraph::new(Span::styled(glyph, Style::new().fg(color))),
                        Rect { x: sx0 as u16, y: sy0 as u16, width: 1, height: 1 },
                    );
                }
            }
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn draw_host(frame: &mut Frame, area: Rect, p: &Pos, h: &TopoHost, theme: &Theme, focused: bool) {
    let r = Rect {
        x: (area.x as i32 + p.x).max(area.x as i32) as u16,
        y: (area.y as i32 + p.y).max(area.y as i32) as u16,
        width: p.w.min(area.width.saturating_sub(p.x.max(0) as u16)),
        height: p.h.min(area.height.saturating_sub(p.y.max(0) as u16)),
    };
    if r.width < 8 || r.height < 4 {
        return;
    }
    let border = if focused {
        theme.accent
    } else if h.online {
        theme.ok
    } else {
        theme.muted
    };
    let title = match &h.alias {
        Some(a) if a != &h.name => format!(" {} · {a} ", truncate(&h.name, 12)),
        _ => format!(" {} ", truncate(&h.name, 16)),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border))
        .title(Span::styled(title, Style::new().fg(border).bold()));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    if inner.width < 4 || inner.height < 2 {
        return;
    }
    let kind = IconKind::parse(&h.device_kind);
    let braille = icons::bitmap(kind).braille();
    let mut lines: Vec<Line> = vec![];
    if inner.width >= 6 && inner.height >= 4 {
        for row in &braille {
            lines.push(Line::from(Span::styled(
                format!("{:^width$}", row, width = inner.width as usize),
                Style::new().fg(if h.online { theme.accent } else { theme.muted }),
            )));
        }
    }
    let gpu = if h.gpus.is_empty() {
        if h.online {
            "no GPU".into()
        } else {
            "-".into()
        }
    } else if h.gpus.len() == 1 {
        truncate(&h.gpus[0].name, inner.width as usize)
    } else {
        format!("{}× {}", h.gpus.len(), truncate(&h.gpus[0].name, (inner.width as usize).saturating_sub(4)))
    };
    lines.push(Line::from(Span::styled(gpu, Style::new().fg(theme.fg))));
    if h.local {
        lines.push(Line::from(Span::styled("controller", Style::new().fg(theme.accent))));
    } else if !h.online {
        lines.push(Line::from(Span::styled("offline", Style::new().fg(theme.err))));
    }
    if let Some(m) = &h.model {
        lines.push(Line::from(Span::styled(truncate(m, inner.width as usize), Style::new().fg(theme.muted))));
    }
    if !h.running.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate(&format!("● {}", h.running[0]), inner.width as usize),
            Style::new().fg(theme.warn),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_switch(frame: &mut Frame, area: Rect, p: &Pos, name: &str, ports: usize, theme: &Theme) {
    let r = Rect {
        x: (area.x as i32 + p.x).max(area.x as i32) as u16,
        y: (area.y as i32 + p.y).max(area.y as i32) as u16,
        width: p.w.min(area.width.saturating_sub(p.x.max(0) as u16)),
        height: p.h.min(area.height.saturating_sub(p.y.max(0) as u16)),
    };
    if r.width < 6 || r.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(theme.muted))
        .title(Span::styled(format!(" {} ", truncate(name, r.width as usize - 4)), Style::new().fg(theme.muted)));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let braille = icons::bitmap(IconKind::Switch).braille();
    let mut lines = vec![];
    if inner.width >= 6 {
        lines.push(Line::from(Span::styled(
            format!("{:^width$}", braille.first().cloned().unwrap_or_default(), width = inner.width as usize),
            Style::new().fg(theme.muted),
        )));
    }
    lines.push(Line::from(Span::styled(format!("{ports} ports"), Style::new().fg(theme.muted))));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_list(frame: &mut Frame, area: Rect, topo: &Topology, theme: &Theme) {
    let mut lines: Vec<Line> = vec![];
    if topo.demo {
        lines.push(Line::from(Span::styled(
            format!("DEMO DATA — {}", topo.demo_topo.as_deref().unwrap_or("tri")),
            Style::new().fg(theme.warn).bold(),
        )));
    }
    if topo.hosts.len() < 2 {
        lines.push(Line::from(Span::styled(
            "Single host — add peers with `lmdeck host add` to see fabric links.",
            Style::new().fg(theme.muted),
        )));
    }
    for seg in &topo.segments {
        let tag = match seg.class {
            crate::fabric::SegmentClass::Cable => "cable",
            crate::fabric::SegmentClass::Switch => "switch",
        };
        let title = seg.switch.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| seg.subnet.clone());
        lines.push(Line::from(Span::styled(
            format!("▸ {tag} {title} ({})", seg.kind.as_str()),
            Style::new().fg(theme.accent).bold(),
        )));
        for m in &seg.members {
            let h = topo.host(&m.host);
            let label = h
                .map(|h| match &h.alias {
                    Some(a) => format!("{} · {a}", h.name),
                    None => h.name.clone(),
                })
                .unwrap_or_else(|| m.host.clone());
            let gpu = h
                .map(|h| {
                    if h.gpus.is_empty() {
                        "-".into()
                    } else if h.gpus.len() == 1 {
                        h.gpus[0].name.clone()
                    } else {
                        format!("{}× {}", h.gpus.len(), h.gpus[0].name)
                    }
                })
                .unwrap_or_else(|| "-".into());
            lines.push(Line::from(format!(
                "   {}  {}  {}{}",
                label,
                m.ip,
                gpu,
                m.ifname.as_deref().map(|i| format!("  {i}")).unwrap_or_default()
            )));
        }
    }
    if topo.segments.is_empty() {
        for h in &topo.hosts {
            let label = match &h.alias {
                Some(a) => format!("{} · {a}", h.name),
                None => h.name.clone(),
            };
            lines.push(Line::from(format!(
                "  {}  {}  {}",
                label,
                if h.online { "online" } else { "offline" },
                h.gpus.first().map(|g| g.name.as_str()).unwrap_or("-")
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Topology "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Seeded layout: switches near the centre, hosts around their switches or
/// in a ring / line for cables. Deterministic for a given graph + size.
pub(crate) fn layout(topo: &Topology, width: u16, height: u16) -> Vec<Pos> {
    let n_hosts = topo.hosts.len();
    let n_sw = topo.switches.len();
    if n_hosts == 0 {
        return vec![];
    }
    let pad = 1i32;
    let aw = width as i32;
    let ah = height as i32;
    let mut positions: Vec<Pos> = vec![];

    if n_hosts == 1 && n_sw == 0 {
        positions.push(Pos {
            id: topo.hosts[0].key.clone(),
            x: (aw - HOST_W as i32).max(0) / 2,
            y: (ah - HOST_H as i32).max(0) / 2,
            w: HOST_W.min(width),
            h: HOST_H.min(height),
            kind: NodeKind::Host,
        });
        return positions;
    }

    // Place switches first, spaced horizontally in the middle band.
    let sw_y = (ah / 2 - SWITCH_H as i32 / 2).max(pad);
    let sw_span = (aw - pad * 2).max(1);
    for (i, sw) in topo.switches.iter().enumerate() {
        let x = if n_sw == 1 { (aw - SWITCH_W as i32) / 2 } else { pad + (sw_span * i as i32) / (n_sw as i32) };
        positions.push(Pos {
            id: sw.id.clone(),
            x: x.max(pad),
            y: sw_y,
            w: SWITCH_W,
            h: SWITCH_H,
            kind: NodeKind::Switch,
        });
    }

    // Hosts attached to a switch fan out in a circle around that switch.
    let mut placed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for sw in &topo.switches {
        let members: Vec<&str> = topo
            .segments
            .iter()
            .filter(|s| s.switch.as_ref().map(|x| x.id.as_str()) == Some(sw.id.as_str()))
            .flat_map(|s| s.members.iter().map(|m| m.host.as_str()))
            .collect();
        let sp = positions.iter().find(|p| p.id == sw.id).cloned();
        let Some(sp) = sp else { continue };
        let n = members.len().max(1) as f64;
        // Clearance so HOST_W boxes on a ring do not overlap.
        let need = (HOST_W as f64 + 4.0) / (2.0 * (std::f64::consts::PI / n).sin());
        let radius = need.max((HOST_W.max(HOST_H) as f64) * 1.5 + 2.0);
        for (i, key) in members.iter().enumerate() {
            if !placed.insert((*key).to_string()) {
                continue;
            }
            let angle = std::f64::consts::TAU * (i as f64) / n - std::f64::consts::FRAC_PI_2;
            let cx = sp.x as f64 + sp.w as f64 / 2.0 + angle.cos() * radius;
            let cy = sp.y as f64 + sp.h as f64 / 2.0 + angle.sin() * radius;
            positions.push(Pos {
                id: (*key).to_string(),
                x: (cx - HOST_W as f64 / 2.0).round() as i32,
                y: (cy - HOST_H as f64 / 2.0).round() as i32,
                w: HOST_W,
                h: HOST_H,
                kind: NodeKind::Host,
            });
        }
    }

    // Remaining hosts (cables / no segment): ring or line.
    let rest: Vec<&TopoHost> = topo.hosts.iter().filter(|h| !placed.contains(&h.key)).collect();
    if !rest.is_empty() {
        let n = rest.len() as f64;
        if rest.len() == 2 && n_sw == 0 {
            let y = (ah - HOST_H as i32) / 2;
            let gap = 8;
            let total = HOST_W as i32 * 2 + gap;
            let x0 = (aw - total) / 2;
            for (i, h) in rest.iter().enumerate() {
                positions.push(Pos {
                    id: h.key.clone(),
                    x: x0 + i as i32 * (HOST_W as i32 + gap),
                    y,
                    w: HOST_W,
                    h: HOST_H,
                    kind: NodeKind::Host,
                });
            }
        } else {
            let cx = aw as f64 / 2.0;
            let cy = ah as f64 / 2.0;
            let radius = (aw.min(ah) as f64 * 0.32).max(12.0);
            for (i, h) in rest.iter().enumerate() {
                let angle = std::f64::consts::TAU * (i as f64) / n - std::f64::consts::FRAC_PI_2;
                positions.push(Pos {
                    id: h.key.clone(),
                    x: (cx + angle.cos() * radius - HOST_W as f64 / 2.0).round() as i32,
                    y: (cy + angle.sin() * radius - HOST_H as f64 / 2.0).round() as i32,
                    w: HOST_W,
                    h: HOST_H,
                    kind: NodeKind::Host,
                });
            }
        }
    }

    // Clamp into area and resolve overlaps with a few push iterations.
    for p in &mut positions {
        p.x = p.x.clamp(pad, (aw - p.w as i32 - pad).max(pad));
        p.y = p.y.clamp(pad, (ah - p.h as i32 - pad).max(pad));
    }
    for _ in 0..40 {
        let mut moved = false;
        for i in 0..positions.len() {
            for j in i + 1..positions.len() {
                let (ai, aj) = (&positions[i], &positions[j]);
                let ox = (ai.x + ai.w as i32).min(aj.x + aj.w as i32) - ai.x.max(aj.x);
                let oy = (ai.y + ai.h as i32).min(aj.y + aj.h as i32) - ai.y.max(aj.y);
                if ox > 0 && oy > 0 {
                    if ox < oy {
                        let d = (ox + 1) / 2;
                        positions[i].x -= d;
                        positions[j].x += d;
                    } else {
                        let d = (oy + 1) / 2;
                        positions[i].y -= d;
                        positions[j].y += d;
                    }
                    moved = true;
                }
            }
        }
        for p in &mut positions {
            p.x = p.x.clamp(pad, (aw - p.w as i32 - pad).max(pad));
            p.y = p.y.clamp(pad, (ah - p.h as i32 - pad).max(pad));
        }
        if !moved {
            break;
        }
    }
    positions
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + ch.to_string().width() > max - 1 {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

/// Public entry used by the TUI view: panel + diagram (or list) + legend + link table.
pub fn draw_topology(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let Some(topo) = &app.topology else {
        let p = Paragraph::new(Span::styled("◐ detecting fabric (CX7 ports, links)…", Style::new().fg(t.muted)))
            .block(panel(title("⌘", "Topology", None, t), t, true));
        f.render_widget(p, area);
        return;
    };
    let cx7 = topo.links.iter().filter(|l| l.link.kind == crate::fabric::LinkKind::Cx7).count();
    let mut extra =
        format!("{} hosts · {} sw · {} links · {cx7} cx7", topo.hosts.len(), topo.switches.len(), topo.links.len());
    if topo.demo {
        extra.push_str(" · DEMO DATA");
    }
    let block = panel(title("⌘", "Topology", Some(extra), t), t, true);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = link_rows(topo, t);
    let pic_h = if inner.width >= 40 && inner.height >= 14 {
        (inner.height * 2 / 3).max(HOST_H + 2).min(inner.height.saturating_sub(4))
    } else {
        0
    };
    let [top, legend, bottom] =
        Layout::vertical([Constraint::Length(pic_h), Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    if pic_h > 0 {
        draw(f, top, topo, t, 0);
    } else {
        draw_list(f, top, topo, t);
    }
    let leg = Line::from(vec![
        Span::styled(" ━ ", Style::new().fg(t.accent).bold()),
        Span::styled("cx7  ", Style::new().fg(t.muted)),
        Span::styled("━ ", Style::new().fg(t.ok).bold()),
        Span::styled("busy  ", Style::new().fg(t.muted)),
        Span::styled("╌ ", Style::new().fg(t.info)),
        Span::styled("lan  ", Style::new().fg(t.muted)),
        Span::styled("┄ ", Style::new().fg(t.muted)),
        Span::styled("tailscale  ", Style::new().fg(t.muted)),
        Span::styled("▢ ", Style::new().fg(t.muted)),
        Span::styled("switch", Style::new().fg(t.muted)),
    ]);
    f.render_widget(Paragraph::new(leg), legend);
    let mut lines = rows;
    if topo.demo {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("DEMO DATA — simulated topology ({})", topo.demo_topo.as_deref().unwrap_or("tri")),
                Style::new().fg(t.warn).bold(),
            )),
        );
    }
    f.render_widget(Paragraph::new(lines), bottom);
}

fn link_rows<'a>(topo: &'a Topology, t: &Theme) -> Vec<Line<'a>> {
    use crate::fabric::LinkKind;
    let mut lines = vec![];
    for l in &topo.links {
        let (glyph, st) = match l.link.kind {
            LinkKind::Cx7 if !l.active.is_empty() => ("━", Style::new().fg(t.ok).bold()),
            LinkKind::Cx7 => ("━", Style::new().fg(t.accent)),
            LinkKind::Lan => ("╌", Style::new().fg(t.info)),
            LinkKind::Tailscale => ("┄", Style::new().fg(t.muted)),
        };
        let a = topo.display(&l.link.a.host);
        let b = topo.display(&l.link.b.host);
        let gbps = l.link.gbps().map(|g| format!("{g:.0}G")).unwrap_or_else(|| "-".into());
        let busy = if l.active.is_empty() { String::new() } else { format!("  ● {}", l.active.join(", ")) };
        lines.push(Line::from(vec![
            Span::styled(format!(" {glyph} "), st),
            Span::raw(format!("{a} ⇄ {b}  {gbps}{busy}")),
        ]));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  (no links matched)", Style::new().fg(t.muted))));
    }
    lines
}

/// Clear helper kept for callers that used to wipe the topo pane.
#[allow(dead_code)]
pub fn clear(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::{End, Link, LinkKind, Segment, SegmentClass, Switch};
    use crate::topology::{TopoGpu, TopoHost, TopoLink, Topology};

    fn host(key: &str, name: &str) -> TopoHost {
        TopoHost {
            key: key.into(),
            name: name.into(),
            device_kind: "spark".into(),
            online: true,
            gpus: vec![TopoGpu { index: 0, name: "NVIDIA GB10".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn single_host_centred() {
        let t = Topology { hosts: vec![host("local", "spark-01.lab")], ..Default::default() };
        let p = layout(&t, 80, 24);
        assert_eq!(p.len(), 1);
        assert!(p[0].x > 10 && p[0].x < 60, "{:?}", p[0].x);
    }

    #[test]
    fn eight_on_switch_no_overlap() {
        let sw = Switch {
            id: "switch:10.20.0.0/24".into(),
            name: "SN2100-TOR".into(),
            subnet: "10.20.0.0/24".into(),
            kind: LinkKind::Cx7,
            ports: 8,
        };
        let hosts: Vec<TopoHost> = (1..=8).map(|i| host(&format!("spark{i}"), &format!("spark-{i:02}.lab"))).collect();
        let members: Vec<_> = hosts
            .iter()
            .enumerate()
            .map(|(i, h)| crate::fabric::Member {
                host: h.key.clone(),
                ifname: Some("enp1s0f0np0".into()),
                ip: format!("10.20.0.{}", 11 + i),
                hcas: vec!["rocep1s0f0".into()],
                gid_index: Some(3),
                speed_mbps: Some(200_000),
                rails: 1,
            })
            .collect();
        let links: Vec<TopoLink> = members
            .iter()
            .map(|m| TopoLink {
                link: Link {
                    a: End {
                        host: m.host.clone(),
                        ifname: m.ifname.clone(),
                        ip: m.ip.clone(),
                        hcas: m.hcas.clone(),
                        gid_index: m.gid_index,
                    },
                    b: End { host: sw.id.clone(), ifname: None, ip: "10.20.0.0".into(), hcas: vec![], gid_index: None },
                    kind: LinkKind::Cx7,
                    subnet: Some(sw.subnet.clone()),
                    speed_mbps: Some(200_000),
                    manual: false,
                    verified: None,
                    rtt_ms: None,
                },
                active: vec![],
            })
            .collect();
        let t = Topology {
            hosts,
            switches: vec![sw.clone()],
            segments: vec![Segment {
                subnet: sw.subnet.clone(),
                class: SegmentClass::Switch,
                kind: LinkKind::Cx7,
                members,
                switch: Some(sw),
            }],
            links,
            ..Default::default()
        };
        let p = layout(&t, 120, 40);
        assert_eq!(p.len(), 9);
        for i in 0..p.len() {
            for j in i + 1..p.len() {
                let (a, b) = (&p[i], &p[j]);
                let ox = (a.x + a.w as i32).min(b.x + b.w as i32) - a.x.max(b.x);
                let oy = (a.y + a.h as i32).min(b.y + b.h as i32) - a.y.max(b.y);
                assert!(!(ox > 0 && oy > 0), "overlap {} × {}", a.id, b.id);
            }
        }
    }
}
