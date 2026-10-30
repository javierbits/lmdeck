//! The Topology tab draws what its legend says.
//!
//! On a real four-Spark ring the diagram drew one Spark as if selected (the
//! tab always focused the first host, and nothing could move it), drew CX7
//! green and Tailscale yellow while its legend says CX7 yellow and Tailscale
//! grey, and drew the Tailscale lines over the CX7 cables and through the
//! Wi-Fi switch box.

use lmdeck::fabric::{End, Link, LinkKind, Member, Segment, SegmentClass, Switch};
use lmdeck::topology::{TopoGpu, TopoHost, TopoLink, Topology};
use lmdeck::tui::theme::{omp, Theme};
use lmdeck::tui::topoview;
use proptest::prelude::*;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::Terminal;

/// A roomy terminal pane.
const W: u16 = 160;
const H: u16 = 60;

/// The glyphs an edge is drawn with: thin, and thick when in use.
const EDGE: [&str; 4] = ["─", "│", "━", "┃"];

/// lmdeck's theme, after checking its legend colors are told apart.
fn theme() -> &'static Theme {
    let t = omp();
    let c = [t.accent, t.ok, t.info, t.muted];
    for (i, a) in c.iter().enumerate() {
        assert!(!c[i + 1..].contains(a), "theme colors collide: {c:?}");
    }
    t
}

/// An online Spark.
fn host(key: &str) -> TopoHost {
    TopoHost {
        key: key.into(),
        name: key.into(),
        device_kind: "spark".into(),
        online: true,
        gpus: vec![TopoGpu { index: 0, name: "NVIDIA GB10".into(), ..Default::default() }],
        ..Default::default()
    }
}

/// A link of `kind` between `a` and `b`, in use when `busy`.
fn link(a: &str, b: &str, kind: LinkKind, busy: bool) -> TopoLink {
    let end = |h: &str| End { host: h.into(), ifname: None, ip: String::new(), hcas: vec![], gid_index: None };
    TopoLink {
        link: Link {
            a: end(a),
            b: end(b),
            kind,
            subnet: None,
            speed_mbps: None,
            manual: false,
            verified: None,
            rtt_ms: None,
        },
        active: if busy { vec!["nfs weights: demo".into()] } else { vec![] },
    }
}

/// The legend's color for a link of `kind`, in use when `busy`.
fn legend(t: &Theme, kind: LinkKind, busy: bool) -> Color {
    match (busy, kind) {
        (true, _) => t.ok,
        (false, LinkKind::Cx7) => t.accent,
        (false, LinkKind::Lan) => t.info,
        (false, LinkKind::Tailscale) => t.muted,
    }
}

/// Two Sparks side by side, joined by `links`.
fn pair(links: Vec<TopoLink>) -> Topology {
    Topology { hosts: vec![host("a"), host("b")], links, ..Default::default() }
}

/// The real cluster as the tab showed it: four Sparks cabled in a CX7 ring,
/// all on one Wi-Fi network (a 4-port switch they fan out around: ember on
/// top, gigabyte below), and three of them on Tailscale.
fn cluster() -> Topology {
    let sw = Switch {
        id: "switch:192.168.1.0/24".into(),
        name: "lan-switch 192.168.1.0/24".into(),
        subnet: "192.168.1.0/24".into(),
        kind: LinkKind::Lan,
        ports: 4,
    };
    let keys = ["ember", "faraday", "gigabyte", "spark"];
    let member = |h: &str| Member {
        host: h.into(),
        ifname: Some("wlP9s9".into()),
        ip: String::new(),
        hcas: vec![],
        gid_index: None,
        speed_mbps: None,
        rails: 1,
    };
    let mut hosts: Vec<TopoHost> = keys.iter().map(|&k| host(k)).collect();
    hosts[3].local = true;
    let mut links = vec![
        link("spark", "gigabyte", LinkKind::Cx7, false),
        link("gigabyte", "faraday", LinkKind::Cx7, false),
        link("faraday", "ember", LinkKind::Cx7, false),
        link("ember", "spark", LinkKind::Cx7, false),
    ];
    links.extend(keys.iter().map(|&k| link(k, &sw.id, LinkKind::Lan, false)));
    links.extend([
        link("gigabyte", "ember", LinkKind::Tailscale, false),
        link("spark", "ember", LinkKind::Tailscale, false),
        link("spark", "gigabyte", LinkKind::Tailscale, false),
    ]);
    Topology {
        hosts,
        segments: vec![Segment {
            subnet: sw.subnet.clone(),
            class: SegmentClass::Switch,
            kind: LinkKind::Lan,
            members: keys.iter().map(|&k| member(k)).collect(),
            switch: Some(sw.clone()),
        }],
        switches: vec![sw],
        links,
        ..Default::default()
    }
}

/// Draws `topo` the way the Topology tab does.
fn render(topo: &Topology) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| {
        let area = f.area();
        assert!(topoview::draw(f, area, topo, theme()), "drew the diagram, not the list");
    })
    .unwrap();
    term.backend().buffer().clone()
}

/// Every box on screen (host cards and switches), found by its corners.
fn boxes(buf: &Buffer) -> Vec<Rect> {
    let a = buf.area;
    let mut out = vec![];
    for y in 0..a.height {
        for x in 0..a.width {
            if buf[(x, y)].symbol() != "┌" {
                continue;
            }
            let right = (x + 1..a.width).find(|&r| buf[(r, y)].symbol() == "┐");
            let bottom = (y + 1..a.height).find(|&b| buf[(x, b)].symbol() == "└");
            if let (Some(r), Some(b)) = (right, bottom) {
                out.push(Rect::new(x, y, r - x + 1, b - y + 1));
            }
        }
    }
    out
}

/// The text along a box's top border: its title.
fn top_row(buf: &Buffer, r: Rect) -> String {
    (r.x..r.x + r.width).map(|x| buf[(x, r.y)].symbol()).collect()
}

/// The colors of the edge cells: line glyphs outside every box.
fn edge_colors(buf: &Buffer) -> Vec<Color> {
    let boxes = boxes(buf);
    let a = buf.area;
    let mut out = vec![];
    for y in 0..a.height {
        for x in 0..a.width {
            let cell = &buf[(x, y)];
            if EDGE.contains(&cell.symbol()) && !boxes.iter().any(|r| r.contains((x, y).into())) {
                out.push(cell.fg);
            }
        }
    }
    out
}

#[test]
fn no_spark_looks_selected() {
    let t = theme();
    let buf = render(&cluster());
    let cards: Vec<Rect> = boxes(&buf).into_iter().filter(|r| !top_row(&buf, *r).contains("lan-switch")).collect();
    assert_eq!(cards.len(), 4, "four host cards");
    for r in cards {
        assert_eq!(buf[(r.x, r.y)].fg, t.ok, "{} is online: green like the others", top_row(&buf, r).trim());
    }
}

#[test]
fn an_offline_spark_stays_grey() {
    let t = theme();
    let mut topo = pair(vec![link("a", "b", LinkKind::Cx7, false)]);
    topo.hosts[1].online = false;
    let buf = render(&topo);
    let borders: Vec<Color> = boxes(&buf).iter().map(|r| buf[(r.x, r.y)].fg).collect();
    assert_eq!(borders, vec![t.ok, t.muted], "a online, b offline");
}

#[test]
fn edges_use_the_legend_colors() {
    let t = theme();
    for (kind, busy) in
        [(LinkKind::Cx7, false), (LinkKind::Cx7, true), (LinkKind::Lan, false), (LinkKind::Tailscale, false)]
    {
        let got = edge_colors(&render(&pair(vec![link("a", "b", kind, busy)])));
        assert!(!got.is_empty(), "{kind:?} drew no edge");
        assert!(got.iter().all(|c| *c == legend(t, kind, busy)), "{kind:?} busy={busy}: {got:?}");
    }
}

#[test]
fn tailscale_runs_under_the_cx7_cable() {
    let t = theme();
    // As topology lists them: CX7 first, Tailscale last.
    let links = vec![link("a", "b", LinkKind::Cx7, false), link("a", "b", LinkKind::Tailscale, false)];
    let got = edge_colors(&render(&pair(links)));
    assert!(!got.is_empty() && got.iter().all(|c| *c == t.accent), "{got:?}");
}

#[test]
fn no_line_crosses_the_wifi_switch() {
    let buf = render(&cluster());
    let sw = boxes(&buf).into_iter().find(|r| top_row(&buf, *r).contains("lan-switch")).expect("the switch box");
    for y in sw.y + 1..sw.y + sw.height - 1 {
        for x in sw.x + 1..sw.x + sw.width - 1 {
            assert!(!EDGE.contains(&buf[(x, y)].symbol()), "a line crosses the switch at ({x}, {y})");
        }
    }
}

proptest! {
    /// Where links share a path the most important one shows (in use, then
    /// CX7, then LAN, then Tailscale), whatever order topology lists them in.
    #[test]
    fn the_most_important_link_is_drawn_on_top(links in prop::collection::vec((0usize..3, any::<bool>()), 1..6)) {
        let t = theme();
        let kinds = [LinkKind::Tailscale, LinkKind::Lan, LinkKind::Cx7];
        let (k, busy) = *links.iter().max_by_key(|(k, busy)| (*busy, *k)).unwrap();
        let want = legend(t, kinds[k], busy);
        let got = edge_colors(&render(&pair(links.iter().map(|&(k, b)| link("a", "b", kinds[k], b)).collect())));
        prop_assert!(!got.is_empty());
        prop_assert!(got.iter().all(|c| *c == want), "want {:?}, got {:?}", want, got);
    }
}
