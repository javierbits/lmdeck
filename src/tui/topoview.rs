//! The Topology tab: hosts as boxes, CX7 cables as heavy lines, the
//! controller's management paths dashed, then every link in a table.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::fabric::{best_link, LinkKind, NicKind};
use crate::topology::{TopoHost, TopoLink, Topology};
use crate::tui::app::App;
use crate::tui::theme::Theme;
use crate::tui::view::{panel, title};

/// A character grid that turns into styled lines.
struct Grid {
    w: usize,
    cells: Vec<Vec<(char, Style)>>,
}

impl Grid {
    fn new(w: usize, h: usize) -> Grid {
        Grid { w, cells: vec![vec![(' ', Style::new()); w]; h] }
    }

    fn put(&mut self, x: usize, y: usize, s: &str, st: Style) {
        let Some(row) = self.cells.get_mut(y) else { return };
        for (i, c) in s.chars().enumerate() {
            if x + i < self.w {
                row[x + i] = (c, st);
            }
        }
    }

    fn hline(&mut self, x0: usize, x1: usize, y: usize, c: char, st: Style) {
        for x in x0..x1 {
            self.put(x, y, &c.to_string(), st);
        }
    }

    fn lines<'a>(self) -> Vec<Line<'a>> {
        self.cells
            .into_iter()
            .map(|row| {
                let mut spans: Vec<Span> = vec![];
                let mut run = String::new();
                let mut cur = Style::new();
                for (c, st) in row {
                    if st != cur && !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), cur));
                    }
                    cur = st;
                    run.push(c);
                }
                spans.push(Span::styled(run.trim_end().to_string(), cur));
                Line::from(spans)
            })
            .collect()
    }
}

fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(w.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

/// How a link is drawn: glyph and colour.
fn link_style(l: &TopoLink, t: &Theme) -> (char, Style) {
    let base = match l.link.kind {
        LinkKind::Cx7 => ('━', Style::new().fg(if l.active.is_empty() { t.accent } else { t.ok }).bold()),
        LinkKind::Lan => ('╌', Style::new().fg(t.info)),
        LinkKind::Tailscale => ('┄', Style::new().fg(t.muted)),
    };
    match l.link.verified {
        Some(false) => (base.0, Style::new().fg(t.err)),
        _ => base,
    }
}

fn speed(l: &TopoLink) -> String {
    l.link.gbps().map(|g| format!("{g:.0}G")).unwrap_or_default()
}

const BOX_H: usize = 8;

/// The host box contents (inside the border).
fn box_lines(h: &TopoHost, w: usize, t: &Theme) -> Vec<(String, Style)> {
    let mut v = vec![];
    let fg = if h.online { t.fg } else { t.dim };
    let gpu = h.gpu.clone().unwrap_or_else(|| if h.online { "no GPU".into() } else { "-".into() });
    let mem = match (h.mem_used_gb, h.mem_total_gb) {
        (Some(u), Some(tot)) => format!("{u:.0}/{tot:.0} GB"),
        (None, Some(tot)) => format!("{tot:.0} GB"),
        _ => "-".into(),
    };
    let temp = h.temp_c.map(|c| format!(" · {c:.0}°C")).unwrap_or_default();
    v.push((clip(&format!("{gpu} · {mem}{temp}"), w), Style::new().fg(t.muted)));
    let mgmt = match (&h.mgmt_kind[..], &h.mgmt_addr) {
        ("local", _) => "controller (this machine)".to_string(),
        (k, Some(a)) => format!("{k} {a}"),
        (k, None) => k.to_string(),
    };
    v.push((clip(&mgmt, w), Style::new().fg(if h.local { t.accent } else { t.info })));
    let cx7: Vec<String> = h
        .fabric
        .iter()
        .flat_map(|f| f.nics.iter())
        .filter(|n| matches!(n.kind, NicKind::Cx7))
        .filter_map(|n| n.ipv4.first().map(|a| a.ip.clone()))
        .collect();
    v.push(match (&h.fabric, cx7.is_empty()) {
        (Some(_), false) => (clip(&format!("cx7 {}", cx7.join(" ")), w), Style::new().fg(fg)),
        (Some(_), true) => ("no CX7 ports".into(), Style::new().fg(t.warn)),
        (None, _) => (clip(h.fabric_error.as_deref().unwrap_or("fabric unknown"), w), Style::new().fg(t.dim)),
    });
    let running = if h.running.is_empty() { "idle".to_string() } else { format!("▸ {}", h.running.join(", ")) };
    v.push((clip(&running, w), Style::new().fg(if h.running.is_empty() { t.dim } else { t.ok })));
    let weights = match h.weights.len() {
        0 => "no weights indexed".to_string(),
        1 => format!("weights: {}", h.weights[0]),
        n => format!("weights: {n} models"),
    };
    v.push((clip(&weights, w), Style::new().fg(t.muted)));
    v
}

/// The diagram, or `None` when the hosts don't fit side by side.
fn diagram<'a>(topo: &Topology, width: usize, t: &Theme) -> Option<Vec<Line<'a>>> {
    let n = topo.hosts.len();
    if n == 0 {
        return None;
    }
    let gap = 12usize;
    let bw = ((width.saturating_sub(gap * (n - 1) + 2)) / n).min(34);
    if bw < 20 {
        return None;
    }
    let total = bw * n + gap * (n - 1);
    let x0 = width.saturating_sub(total) / 2;
    let bx = |i: usize| x0 + i * (bw + gap);
    let cx = |i: usize| bx(i) + bw / 2;
    let raw = topo.raw_links();
    // The best link of each pair is drawn; adjacent pairs sit between boxes,
    // others arc underneath.
    let mut far = vec![];
    let mut near = vec![];
    for i in 0..n {
        for j in i + 1..n {
            let (a, b) = (&topo.hosts[i].key, &topo.hosts[j].key);
            let Some(best) = best_link(&raw, a, b) else { continue };
            let Some(l) = topo.links.iter().find(|l| l.link == *best) else { continue };
            if j == i + 1 {
                near.push((i, l));
            } else {
                far.push((i, j, l));
            }
        }
    }
    let top = 3usize;
    let h = top + BOX_H + far.len() * 2 + 1;
    let mut g = Grid::new(width, h);
    // Controller hub and its management paths.
    let ctl = topo.hosts.iter().position(|h| h.key == topo.controller).unwrap_or(0);
    let label = format!(" ◆ controller · {} ", topo.hosts[ctl].name);
    let lx = (x0 + total / 2).saturating_sub(label.chars().count() / 2);
    g.put(lx, 0, &label, Style::new().fg(t.accent).bold());
    let (first, last) = (cx(0), cx(n - 1));
    let bus = Style::new().fg(t.dim);
    g.hline(first, last + 1, 1, '┄', bus);
    let mid = x0 + total / 2;
    g.put(mid, 1, "┴", bus);
    for (i, hst) in topo.hosts.iter().enumerate() {
        let (c, st) = match (&hst.mgmt_kind[..], hst.online) {
            ("local", _) => ('│', Style::new().fg(t.accent)),
            (_, false) => ('┆', Style::new().fg(t.err)),
            ("tailscale", _) => ('┆', Style::new().fg(t.muted)),
            _ => ('┆', Style::new().fg(t.info)),
        };
        let corner = if i == 0 {
            "┌"
        } else if i == n - 1 {
            "┐"
        } else {
            "┬"
        };
        g.put(cx(i), 1, corner, bus);
        g.put(cx(i), 2, &c.to_string(), st);
    }
    // Boxes.
    for (i, hst) in topo.hosts.iter().enumerate() {
        let x = bx(i);
        let border = Style::new().fg(if hst.local {
            t.accent
        } else if hst.online {
            t.border
        } else {
            t.err
        });
        g.put(x, top, "╭", border);
        g.hline(x + 1, x + bw - 1, top, '─', border);
        g.put(x + bw - 1, top, "╮", border);
        for y in top + 1..top + BOX_H - 1 {
            g.put(x, y, "│", border);
            g.put(x + bw - 1, y, "│", border);
        }
        g.put(x, top + BOX_H - 1, "╰", border);
        g.hline(x + 1, x + bw - 1, top + BOX_H - 1, '─', border);
        g.put(x + bw - 1, top + BOX_H - 1, "╯", border);
        let (dot, dc) = if hst.online { ("●", t.ok) } else { ("○", t.err) };
        g.put(x + 2, top + 1, dot, Style::new().fg(dc));
        let name = format!("{}{}", hst.name, if hst.local { " ⌂" } else { "" });
        g.put(x + 4, top + 1, &clip(&name, bw - 6), Style::new().fg(t.fg).bold());
        for (k, (text, st)) in box_lines(hst, bw - 4, t).into_iter().enumerate() {
            g.put(x + 2, top + 2 + k, &text, st);
        }
    }
    // Adjacent links: through the gap, label above and below.
    for (i, l) in near {
        let (c, st) = link_style(l, t);
        let (a, b) = (bx(i) + bw, bx(i + 1));
        let y = top + 3;
        g.put(a - 1, y, "┤", st);
        g.hline(a, b, y, c, st);
        g.put(b, y, "├", st);
        let up = clip(&format!("{} {}", l.link.kind.as_str(), speed(l)), gap);
        g.put(a + (gap.saturating_sub(up.chars().count())) / 2, y - 1, &up, st);
        let down = match (l.link.verified, l.active.is_empty()) {
            (Some(false), _) => "✗ down".to_string(),
            (_, false) => "● busy".to_string(),
            (Some(true), true) => l.link.rtt_ms.map(|r| format!("{r:.2}ms")).unwrap_or_else(|| "ok".into()),
            _ => String::new(),
        };
        g.put(a + (gap.saturating_sub(down.chars().count())) / 2, y + 1, &clip(&down, gap), st);
    }
    // Far links: arcs under the boxes.
    for (k, (i, j, l)) in far.iter().enumerate() {
        let (c, st) = link_style(l, t);
        let y = top + BOX_H + k * 2;
        let (a, b) = (cx(*i), cx(*j));
        for yy in top + BOX_H - 1..y {
            if yy == top + BOX_H - 1 {
                g.put(a, yy, "┬", st);
                g.put(b, yy, "┬", st);
            } else {
                g.put(a, yy, "│", st);
                g.put(b, yy, "│", st);
            }
        }
        let (lc, rc) = if c == '━' { ("┗", "┛") } else { ("└", "┘") };
        g.put(a, y, lc, st);
        g.hline(a + 1, b, y, c, st);
        g.put(b, y, rc, st);
        let busy = if l.active.is_empty() { "" } else { " · ● busy" };
        let label = format!(
            " {} {} · {}⇄{}{busy} ",
            l.link.kind.as_str(),
            speed(l),
            topo.display(&l.link.a.host),
            topo.display(&l.link.b.host)
        );
        let lx = (a + b) / 2 - label.chars().count().min((a + b) / 2) / 2;
        g.put(lx, y, &label, st);
    }
    Some(g.lines())
}

fn link_rows<'a>(topo: &Topology, t: &Theme) -> Vec<Line<'a>> {
    let mut v = vec![Line::from(Span::styled(
        format!("{:<10}{:<24}{:<24}{:<14}{:<18}{:<7}{}", "link", "from", "to", "iface", "subnet", "speed", "check"),
        Style::new().fg(t.dim),
    ))];
    if topo.links.is_empty() {
        v.push(Line::from(Span::styled(
            "no links matched: no shared subnets between hosts' fabrics (add [[link]] to hosts.toml)",
            Style::new().fg(t.warn),
        )));
    }
    for l in &topo.links {
        let k = &l.link;
        let (_, st) = link_style(l, t);
        let check = match k.verified {
            Some(true) => format!("ok{}", k.rtt_ms.map(|r| format!(" {r:.2} ms")).unwrap_or_default()),
            Some(false) => "UNREACHABLE".into(),
            None => "unverified".into(),
        };
        let mut spans = vec![
            Span::styled(format!("{:<10}", k.kind.as_str()), st),
            Span::styled(format!("{:<24}", format!("{} {}", topo.display(&k.a.host), k.a.ip)), Style::new().fg(t.fg)),
            Span::styled(format!("{:<24}", format!("{} {}", topo.display(&k.b.host), k.b.ip)), Style::new().fg(t.fg)),
            Span::styled(format!("{:<14}", k.a.ifname.as_deref().unwrap_or("-")), Style::new().fg(t.muted)),
            Span::styled(format!("{:<18}", k.subnet.as_deref().unwrap_or("-")), Style::new().fg(t.muted)),
            Span::styled(format!("{:<7}", k.gbps().map(|g| format!("{g:.0}G")).unwrap_or("-".into())), st),
            Span::styled(
                check,
                Style::new().fg(match k.verified {
                    Some(true) => t.ok,
                    Some(false) => t.err,
                    None => t.dim,
                }),
            ),
        ];
        if k.manual {
            spans.push(Span::styled(" (hosts.toml)", Style::new().fg(t.dim)));
        }
        if !l.active.is_empty() {
            spans.push(Span::styled(format!("  ● {}", l.active.join(", ")), Style::new().fg(t.ok)));
        }
        v.push(Line::from(spans));
    }
    for n in &topo.notes {
        v.push(Line::from(Span::styled(format!("note: {n}"), Style::new().fg(t.warn))));
    }
    v
}

pub fn draw_topology(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme();
    let Some(topo) = &app.topology else {
        let p = Paragraph::new(Span::styled("◐ detecting fabric (CX7 ports, links)…", Style::new().fg(t.muted)))
            .block(panel(title("⌘", "Topology", None, t), t, true));
        f.render_widget(p, area);
        return;
    };
    let cx7 = topo.links.iter().filter(|l| l.link.kind == LinkKind::Cx7).count();
    let mut extra = format!("{} hosts · {} links · {cx7} cx7", topo.hosts.len(), topo.links.len());
    if topo.demo {
        extra.push_str(" · DEMO DATA");
    }
    let block = panel(title("⌘", "Topology", Some(extra), t), t, true);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = link_rows(topo, t);
    let pic = diagram(topo, inner.width as usize, t);
    let pic_h = pic.as_ref().map_or(0, |p| p.len() as u16 + 1);
    let [top, legend, bottom] =
        Layout::vertical([Constraint::Length(pic_h), Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    if let Some(p) = pic {
        f.render_widget(Paragraph::new(p), top);
    }
    let leg = Line::from(vec![
        Span::styled(" ━ ", Style::new().fg(t.accent).bold()),
        Span::styled("cx7 cable  ", Style::new().fg(t.muted)),
        Span::styled("━ ", Style::new().fg(t.ok).bold()),
        Span::styled("busy (nfs weights / cluster)  ", Style::new().fg(t.muted)),
        Span::styled("╌ ", Style::new().fg(t.info)),
        Span::styled("lan  ", Style::new().fg(t.muted)),
        Span::styled("┄ ", Style::new().fg(t.muted)),
        Span::styled("tailscale / management  ", Style::new().fg(t.muted)),
        Span::styled("━ ", Style::new().fg(t.err)),
        Span::styled("unreachable", Style::new().fg(t.muted)),
    ]);
    f.render_widget(Paragraph::new(leg), legend);
    let mut lines = rows;
    if topo.demo {
        lines.insert(
            0,
            Line::from(Span::styled("DEMO DATA — simulated 3-Spark triangle", Style::new().fg(t.warn).bold())),
        );
    }
    f.render_widget(Paragraph::new(lines), bottom);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_clips_and_groups_runs() {
        let mut g = Grid::new(6, 1);
        g.put(4, 0, "abcd", Style::new());
        assert_eq!(g.cells[0].iter().map(|c| c.0).collect::<String>(), "    ab");
        assert_eq!(clip("spark-long-name", 6), "spark…");
    }
}
