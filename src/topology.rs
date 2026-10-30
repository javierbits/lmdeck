//! The controller's picture of the cluster: every host (with its telemetry,
//! GPUs, running models and weights), the L2 segments between them from
//! [`crate::fabric::build_graph`] (cables and synthetic switches), and the
//! management links from this machine to each `hosts.toml` URL. Served as
//! `GET /api/topology`, drawn on the web Hosts tab and the TUI Topology tab,
//! and printed by `lmdeck hosts topology [--json]`.
//!
//! The graph is fully dynamic: 1 host, 2 hosts on a cable, N hosts on a
//! switch, mixed triangles + switches, dual rails. Nothing is hard-coded to a
//! host count or shape.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::Config;
use crate::fabric::{self, Fabric, Graph, Link, LinkKind, Switch};
use crate::hosts::{HostEntry, HostView, HostsFile, LOCAL};
use crate::paths::Paths;
use crate::telemetry::GpuTel;
use crate::weights::Index;

/// One GPU (or Apple chip) as shown on a topology node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TopoGpu {
    pub index: u32,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_total_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_used_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub util_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_w: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TopoHost {
    /// `local` or the hosts.toml name (what links refer to).
    pub key: String,
    /// Primary label: the machine's real hostname when known.
    pub name: String,
    /// Alias from hosts.toml / `[node] name` when it differs from `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Detectable model: "DGX Spark", "Mac14,13", …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Icon kind: `spark`, `mac`, `macstudio`, `pc`, `unknown`.
    #[serde(default)]
    pub device_kind: String,
    pub url: Option<String>,
    pub local: bool,
    pub online: bool,
    pub error: Option<String>,
    /// All GPUs / chips on the host (not just the first).
    #[serde(default)]
    pub gpus: Vec<TopoGpu>,
    pub mem_total_gb: Option<f64>,
    pub mem_used_gb: Option<f64>,
    pub temp_c: Option<f64>,
    /// Servers answering on that host (telemetry).
    pub running: Vec<String>,
    /// HF repos / model paths on its disk.
    pub weights: Vec<String>,
    pub fabric: Option<Fabric>,
    pub fabric_error: Option<String>,
    /// How the controller reaches it (`local`, `lan`, `tailscale`) and the address.
    pub mgmt_kind: String,
    pub mgmt_addr: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    pub at: u64,
    pub demo: bool,
    /// Which demo shape was used (`tri`, `duo`, `switch8`, `single`), when demo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demo_topo: Option<String>,
    /// The controller's key (`local`).
    pub controller: String,
    pub hosts: Vec<TopoHost>,
    pub switches: Vec<Switch>,
    pub links: Vec<TopoLink>,
    /// L2 segments (cables + switches) for clients that want the classification.
    #[serde(default)]
    pub segments: Vec<fabric::Segment>,
    /// The CX7 fabric's shape and rank order across the hosts with CX7 ports
    /// (this machine first, then hosts.toml order). On a ring, `order` is the cabled order.
    #[serde(default)]
    pub fabric: fabric::Shape,
    pub notes: Vec<String>,
}

/// A link plus what is using it right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopoLink {
    #[serde(flatten)]
    pub link: Link,
    /// Why it is busy: `nfs weights: <recipe>`, `cluster: <recipe>`.
    pub active: Vec<String>,
}

impl Topology {
    pub fn host(&self, key: &str) -> Option<&TopoHost> {
        self.hosts.iter().find(|h| h.key == key)
    }

    pub fn raw_links(&self) -> Vec<Link> {
        self.links.iter().map(|l| l.link.clone()).collect()
    }

    pub fn display(&self, key: &str) -> String {
        if let Some(h) = self.host(key) {
            return h.name.clone();
        }
        self.switches.iter().find(|s| s.id == key).map(|s| s.name.clone()).unwrap_or_else(|| key.to_string())
    }

    /// Graph view for path lookups (placement / multi-node).
    pub fn graph(&self) -> Graph {
        Graph { segments: self.segments.clone(), switches: self.switches.clone(), links: self.raw_links() }
    }
}

fn mgmt_of(url: &str) -> (String, Option<String>) {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
    let addr = match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.trim_matches(['[', ']']).to_string(),
        _ => host.to_string(),
    };
    let kind = if fabric::is_tailscale(&addr) || addr.ends_with(".ts.net") { "tailscale" } else { "lan" };
    (kind.into(), Some(addr))
}

fn gpus_of(t: Option<&crate::telemetry::Telemetry>, fabric: Option<&Fabric>) -> Vec<TopoGpu> {
    if let Some(t) = t {
        if !t.gpus.is_empty() {
            return t.gpus.iter().map(topo_gpu_from_tel).collect();
        }
    }
    fabric
        .map(|f| {
            f.gpus
                .iter()
                .enumerate()
                .map(|(i, g)| TopoGpu {
                    index: i as u32,
                    name: g.name.clone(),
                    mem_total_gb: g.memory_gb,
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn topo_gpu_from_tel(g: &GpuTel) -> TopoGpu {
    TopoGpu {
        index: g.index,
        name: g.name.clone(),
        mem_total_gb: g.mem_total_gb,
        mem_used_gb: g.mem_used_gb,
        temp_c: g.temp_c,
        util_pct: g.util_pct,
        power_w: g.power_w,
    }
}

/// `LMDECK_DEMO_TOPO`: `tri` (default), `duo`, `switch8`, `single`, `mix`,
/// `ring4` (four Sparks cabled 1-2-3-4-1) or `fold4` (the folded ring, 1-2-4-3-1).
pub fn demo_topo_name() -> String {
    std::env::var("LMDECK_DEMO_TOPO")
        .ok()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| matches!(s.as_str(), "tri" | "duo" | "switch8" | "single" | "mix" | "ring4" | "fold4"))
        .unwrap_or_else(|| "tri".into())
}

/// One host's fabric: this machine detects, others are asked; demo mode
/// reads built-in fixtures (shape from [`demo_topo_name`]).
pub fn fetch_fabric(k: &str, entry: Option<&HostEntry>, online: bool, demo: bool) -> Result<Fabric, String> {
    if demo {
        let shape = demo_topo_name();
        if shape == "single" {
            if k != LOCAL {
                return Err("offline (demo)".into());
            }
            let mut f = fabric::fixtures::load(fabric::fixtures::TRI[0].1);
            f.demo = true;
            return Ok(f);
        }
        let items: &[(&str, &str)] = match shape.as_str() {
            "duo" => &fabric::fixtures::DUO,
            "switch8" => &fabric::fixtures::SWITCH8,
            "ring4" => &fabric::fixtures::RING4,
            "fold4" => &fabric::fixtures::FOLD4,
            "mix" => &[
                ("spark1", include_str!("../tests/fixtures/fabric/mix-spark1.json")),
                ("spark2", fabric::fixtures::TRI[1].1),
                ("spark3", fabric::fixtures::TRI[2].1),
                ("spark4", include_str!("../tests/fixtures/fabric/mix-spark4.json")),
            ],
            _ => &fabric::fixtures::TRI,
        };
        let want = if k == LOCAL { items[0].0 } else { k };
        return items
            .iter()
            .find(|(n, _)| *n == want)
            .map(|(_, t)| {
                let mut f = fabric::fixtures::load(t);
                f.demo = true;
                f
            })
            .ok_or_else(|| "offline (demo)".to_string());
    }
    if k == LOCAL {
        return Ok(fabric::detect());
    }
    if !online {
        return Err("offline".into());
    }
    let e = entry.ok_or("not in hosts.toml")?;
    crate::remote::Client::new(e).get::<Fabric>("fabric").map_err(|e| format!("{e:#}"))
}

/// Matches links between fabrics, then applies hosts.toml `[[link]]`
/// overrides and `link_addr`.
fn graph_from(
    file: &HostsFile,
    entries: &BTreeMap<String, HostEntry>,
    node: &str,
    with_fabric: &[(String, &Fabric)],
) -> Graph {
    let overrides: Vec<crate::hosts::LinkOverride> = file
        .links
        .iter()
        .map(|o| {
            let fix = |h: &str| if crate::remote::is_local(h, node) { LOCAL.to_string() } else { h.to_string() };
            crate::hosts::LinkOverride { a: fix(&o.a), b: fix(&o.b), ..o.clone() }
        })
        .collect();
    let mut g = fabric::build_graph(with_fabric, &overrides);
    for l in g.links.iter_mut() {
        for end in [&mut l.a, &mut l.b] {
            if end.host.starts_with("switch:") {
                continue;
            }
            if let Some(addr) = entries.get(&end.host).and_then(|e| e.link_addr.clone()) {
                if l.kind.is_direct() {
                    end.ip = addr;
                }
            }
        }
    }
    g
}

/// Links among just `keys` (for placement and multi-node planning), without
/// telemetry or reachability checks.
pub fn links_for(paths: &Paths, cfg: &Config, keys: &[String]) -> Vec<Link> {
    graph_for(paths, cfg, keys).links
}

/// Full graph among `keys` (placement / multi-node use [`fabric::path_between`]).
pub fn graph_for(paths: &Paths, cfg: &Config, keys: &[String]) -> Graph {
    let demo = crate::telemetry::demo_mode();
    let file = HostsFile::load(paths).unwrap_or_default();
    let entries: BTreeMap<String, HostEntry> = file.hosts.iter().map(|e| (e.name.clone(), e.clone())).collect();
    let mut keys: Vec<String> = keys.to_vec();
    keys.sort();
    keys.dedup();
    let fabrics: Vec<(String, Fabric)> = std::thread::scope(|s| {
        let jobs: Vec<_> = keys
            .iter()
            .map(|k| {
                let e = entries.get(k).cloned();
                s.spawn(move || fetch_fabric(k, e.as_ref(), true, demo).ok().map(|f| (k.clone(), f)))
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok().flatten()).collect()
    });
    let refs: Vec<(String, &Fabric)> = fabrics.iter().map(|(k, f)| (k.clone(), f)).collect();
    graph_from(&file, &entries, &cfg.node_name(), &refs)
}

/// Peers the demo hosts list for each `LMDECK_DEMO_TOPO` (after `local`).
fn demo_peers() -> Vec<String> {
    let n = match demo_topo_name().as_str() {
        "single" => 1,
        "duo" => 2,
        "switch8" => 8,
        "mix" | "ring4" | "fold4" => 4,
        _ => 3,
    };
    (2..=n).map(|i| format!("spark{i}")).collect()
}

/// The CX7 fabric shape across this machine and every hosts.toml host, in
/// rack order (this machine first, then hosts.toml order). Hosts without a
/// CX7 link drop out; one Spark left reads `single`.
pub fn shape_for(paths: &Paths, cfg: &Config) -> fabric::Shape {
    let peers: Vec<String> = if crate::telemetry::demo_mode() {
        demo_peers()
    } else {
        HostsFile::load(paths).map(|f| f.hosts.into_iter().map(|e| e.name).collect()).unwrap_or_default()
    };
    let mut keys = vec![LOCAL.to_string()];
    keys.extend(peers);
    let graph = graph_for(paths, cfg, &keys);
    let on_cx7 = |h: &str| {
        graph.segments.iter().any(|s| s.kind == LinkKind::Cx7 && s.members.iter().any(|m| m.host == h))
            || graph.links.iter().any(|l| l.kind == LinkKind::Cx7 && l.involves(h))
    };
    let rack: Vec<String> = keys.into_iter().filter(|k| on_cx7(k)).collect();
    if rack.len() <= 1 {
        return fabric::shape(&graph, &[LOCAL.to_string()]);
    }
    fabric::shape(&graph, &rack)
}

/// The fabric a `command_by_fabric` recipe starts on: `LMDECK_FABRIC` when
/// set, else [`shape_for`]. `None` for recipes without per-fabric commands.
pub fn fabric_for(paths: &Paths, cfg: &Config, r: &crate::recipe::Recipe) -> Option<fabric::Shape> {
    if r.engine.command_by_fabric.is_empty() {
        return None;
    }
    if let Some(kind) = std::env::var("LMDECK_FABRIC").ok().and_then(|v| fabric::ShapeKind::parse(&v)) {
        return Some(fabric::Shape { kind, order: vec![] });
    }
    Some(shape_for(paths, cfg))
}

/// What is using each link: NFS placements and multi-node runs that are up.
pub fn activity(paths: &Paths, hosts: &[TopoHost]) -> Vec<(String, String, String)> {
    let running = |key: &str, name: &str| hosts.iter().any(|h| h.key == key && h.running.iter().any(|r| r == name));
    let mut out = vec![];
    for p in crate::placement::Record::all(paths) {
        if running(&p.run_host, &p.served_as) {
            out.push((p.home.clone(), p.run_host.clone(), format!("nfs weights: {}", p.recipe)));
        }
    }
    for c in crate::cluster::Record::all(paths) {
        let up: Vec<&crate::cluster::NodeRec> = c.nodes.iter().filter(|n| running(&n.host, &n.served_as)).collect();
        let head = c.nodes.iter().find(|n| n.role == crate::cluster::Role::Head);
        if let Some(h) = head {
            for n in up.iter().filter(|n| n.host != h.host) {
                out.push((h.host.clone(), n.host.clone(), format!("cluster: {}", c.recipe)));
            }
        }
    }
    out
}

fn label_host(
    v: &HostView,
    node: &str,
    fabric: Option<&Fabric>,
    demo: bool,
) -> (String, Option<String>, Option<String>, String) {
    let hostname = fabric
        .map(|f| f.hostname.clone())
        .filter(|h| !h.is_empty())
        .or_else(|| v.telemetry.as_ref().map(|t| t.hostname.clone()).filter(|h| !h.is_empty()));
    let alias = if v.local {
        let n = if demo { None } else { Some(node.to_string()) };
        n.filter(|n| hostname.as_ref() != Some(n) && n != LOCAL)
    } else {
        Some(v.name.clone()).filter(|n| hostname.as_ref() != Some(n))
    };
    let name = hostname.clone().unwrap_or_else(|| if v.local { node.to_string() } else { v.name.clone() });
    let model = fabric.and_then(|f| f.product.clone()).or_else(|| {
        v.telemetry.as_ref().and_then(|t| {
            if t.platform.contains("DGX Spark") {
                Some("DGX Spark".into())
            } else if t.platform.contains("Apple") {
                Some(t.platform.clone())
            } else {
                None
            }
        })
    });
    let kind = fabric
        .map(|f| f.device_kind.clone())
        .filter(|k| !k.is_empty())
        .unwrap_or_else(|| fabric::device_kind(model.as_deref(), &[], hostname.as_deref().unwrap_or(&v.name)));
    (name, alias, model, kind)
}

/// Builds the topology. `views` come from the [`crate::hosts::Sampler`];
/// `verify` pings each host↔host link from its `a` side.
pub fn build(paths: &Paths, cfg: &Config, views: &[HostView], verify: bool) -> Topology {
    let demo = crate::telemetry::demo_mode();
    let demo_topo = demo.then(demo_topo_name);
    let file = HostsFile::load(paths).unwrap_or_default();
    let entries: BTreeMap<String, HostEntry> = file.hosts.iter().map(|e| (e.name.clone(), e.clone())).collect();
    let node = cfg.node_name();
    let index = Index::fresh(paths, cfg, crate::weights::INDEX_TTL);
    let keys: Vec<String> = views.iter().map(|v| v.name.clone()).collect();
    let fabrics: Vec<Result<Fabric, String>> = std::thread::scope(|s| {
        let jobs: Vec<_> = keys
            .iter()
            .map(|k| {
                let entry = entries.get(k).cloned();
                let online = views.iter().any(|v| &v.name == k && v.online);
                s.spawn(move || fetch_fabric(k, entry.as_ref(), online, demo))
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err("panicked".into()))).collect()
    });
    let mut hosts = vec![];
    for (v, f) in views.iter().zip(fabrics) {
        let t = v.telemetry.as_ref();
        let (mgmt_kind, mgmt_addr) =
            if v.local { ("local".to_string(), None) } else { mgmt_of(v.url.as_deref().unwrap_or("")) };
        let weights =
            index.inventory(&v.name).map(|i| i.items.iter().map(|it| it.id.clone()).collect()).unwrap_or_default();
        let fabric_err = f.as_ref().err().cloned();
        let fabric = f.ok();
        let (name, alias, model, device_kind) = label_host(v, &node, fabric.as_ref(), demo);
        let gpus = gpus_of(t, fabric.as_ref());
        hosts.push(TopoHost {
            key: v.name.clone(),
            name,
            alias,
            model,
            device_kind,
            url: v.url.clone(),
            local: v.local,
            online: v.online,
            error: v.error.clone(),
            gpus,
            mem_total_gb: t.map(|t| t.mem_total_gb).filter(|m| *m > 0.0),
            mem_used_gb: t.and_then(|t| t.mem_used_gb),
            temp_c: t.and_then(|t| t.gpus.iter().find_map(|g| g.temp_c).or(t.cpu_temp_c)),
            running: t.map(|t| t.models.iter().map(|m| m.name.clone()).collect()).unwrap_or_default(),
            weights,
            fabric_error: fabric_err,
            fabric,
            mgmt_kind,
            mgmt_addr,
        });
    }
    let with_fabric: Vec<(String, &Fabric)> =
        hosts.iter().filter_map(|h| h.fabric.as_ref().map(|f| (h.key.clone(), f))).collect();
    let mut graph = graph_from(&file, &entries, &node, &with_fabric);
    if verify && !demo {
        let checks: Vec<(bool, Option<f64>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = graph
                .links
                .iter()
                .filter(|l| !l.a.host.starts_with("switch:") && !l.b.host.starts_with("switch:"))
                .map(|l| {
                    let (from, to) = (l.a.host.clone(), l.b.ip.clone());
                    let entry = entries.get(&from).cloned();
                    s.spawn(move || {
                        if from == LOCAL {
                            return fabric::reach(&to);
                        }
                        let Some(e) = entry else { return (false, None) };
                        match crate::remote::Client::new(&e).get::<Value>(&format!("fabric/reach?addr={to}")) {
                            Ok(v) => (
                                v.get("ok").and_then(Value::as_bool).unwrap_or(false),
                                v.get("rtt_ms").and_then(Value::as_f64),
                            ),
                            Err(_) => (false, None),
                        }
                    })
                })
                .collect();
            jobs.into_iter().map(|j| j.join().unwrap_or((false, None))).collect()
        });
        let mut i = 0;
        for l in graph.links.iter_mut() {
            if l.a.host.starts_with("switch:") || l.b.host.starts_with("switch:") {
                continue;
            }
            if let Some((ok, rtt)) = checks.get(i) {
                l.verified = Some(*ok);
                l.rtt_ms = *rtt;
            }
            i += 1;
        }
    }
    let busy = if demo { demo_activity(&graph.links) } else { activity(paths, &hosts) };
    let links: Vec<TopoLink> = graph
        .links
        .iter()
        .cloned()
        .map(|l| {
            let active = busy
                .iter()
                .filter(|(a, b, _)| {
                    // Activity between hosts: mark the cable, or every star edge of a shared switch.
                    if l.involves(a) && l.involves(b) && a != b {
                        return true;
                    }
                    if let Some(sw) =
                        l.a.host
                            .starts_with("switch:")
                            .then_some(&l.a.host)
                            .or_else(|| l.b.host.starts_with("switch:").then_some(&l.b.host))
                    {
                        // Host A and B both on this switch segment?
                        return graph.segments.iter().any(|s| {
                            s.switch.as_ref().map(|x| &x.id) == Some(sw)
                                && s.members.iter().any(|m| &m.host == a)
                                && s.members.iter().any(|m| &m.host == b)
                        }) && (l.involves(a) || l.involves(b));
                    }
                    false
                })
                .map(|(_, _, why)| why.clone())
                .collect::<Vec<_>>();
            let mut active = active;
            active.sort();
            active.dedup();
            TopoLink { link: l, active }
        })
        .collect();
    let rack: Vec<String> =
        hosts.iter().filter(|h| h.fabric.as_ref().is_some_and(|f| f.has_cx7())).map(|h| h.key.clone()).collect();
    let shape = fabric::shape(&graph, &rack);
    let mut notes = vec![];
    if shape.kind == fabric::ShapeKind::Ring {
        let show = |k: &str| hosts.iter().find(|h| h.key == k).map(|h| h.name.clone()).unwrap_or_else(|| k.to_string());
        notes.push(format!(
            "switchless ring of {}: cabled order {}. Multi-node ranks follow this order; \
             hosts across the ring reach each other through a neighbour",
            shape.order.len(),
            shape.describe(show)
        ));
    }
    if demo {
        notes.push(format!(
            "demo data (LMDECK_DEMO=1, LMDECK_DEMO_TOPO={}): simulated fabric from built-in fixtures",
            demo_topo.as_deref().unwrap_or("tri")
        ));
    }
    if hosts.len() < 2 {
        notes.push(
            "only this machine is in the topology — add peers with `lmdeck host add <name> <url>` \
             to see cables, switches and run-on / multi-node options"
                .into(),
        );
    }
    for h in &hosts {
        if let Some(f) = &h.fabric {
            if !f.has_cx7() && !f.nics.is_empty() && hosts.len() > 1 {
                notes.push(format!("{}: no ConnectX-7 ports detected", h.name));
            }
        }
    }
    Topology {
        at: crate::telemetry::now_ms(),
        demo,
        demo_topo,
        controller: LOCAL.into(),
        hosts,
        switches: graph.switches,
        links,
        segments: graph.segments,
        fabric: shape,
        notes,
    }
}

fn demo_activity(links: &[Link]) -> Vec<(String, String, String)> {
    // Prefer a CX7 cable involving local; else the first CX7 star.
    if let Some(l) = links.iter().find(|l| {
        l.kind == LinkKind::Cx7
            && l.involves(LOCAL)
            && !l.a.host.starts_with("switch:")
            && !l.b.host.starts_with("switch:")
    }) {
        let other = if l.a.host == LOCAL { l.b.host.clone() } else { l.a.host.clone() };
        return vec![(LOCAL.to_string(), other, "cluster: demo multi-node run".into())];
    }
    if let Some(l) = links.iter().find(|l| l.kind == LinkKind::Cx7 && l.involves(LOCAL)) {
        let other = if l.a.host == LOCAL { l.b.host.clone() } else { l.a.host.clone() };
        return vec![(LOCAL.to_string(), other, "cluster: demo multi-node run".into())];
    }
    vec![]
}

/// `lmdeck hosts topology`: one block per host, then switches, then the links.
/// A host's weights on one line: every name when there are a few, otherwise
/// the count, the first three and a pointer to `lmdeck weights`.
fn weights_summary(ids: &[String]) -> String {
    const SHOWN: usize = 3;
    if ids.len() <= SHOWN + 1 {
        return ids.join(", ");
    }
    format!(
        "{} models: {} and {} more (`lmdeck weights` lists them)",
        ids.len(),
        ids[..SHOWN].join(", "),
        ids.len() - SHOWN
    )
}

pub fn render_text(t: &Topology) -> String {
    let mut s = String::new();
    if t.demo {
        s.push_str(&format!(
            "DEMO DATA — simulated topology ({}) (LMDECK_DEMO=1)\n\n",
            t.demo_topo.as_deref().unwrap_or("tri")
        ));
    }
    s.push_str("hosts\n");
    for h in &t.hosts {
        let state = if h.online { "online" } else { "offline" };
        let mem = match (h.mem_used_gb, h.mem_total_gb) {
            (Some(u), Some(tot)) => format!("{u:.0}/{tot:.0} GB"),
            (None, Some(tot)) => format!("{tot:.0} GB"),
            _ => "-".into(),
        };
        let mgmt = match (&h.mgmt_kind[..], &h.mgmt_addr) {
            ("local", _) => "controller".to_string(),
            (k, Some(a)) => format!("{k} {a}"),
            (k, None) => k.to_string(),
        };
        let label = match &h.alias {
            Some(a) => format!("{} · {a}", h.name),
            None => h.name.clone(),
        };
        let gpu = if h.gpus.is_empty() {
            "-".into()
        } else if h.gpus.len() == 1 {
            h.gpus[0].name.clone()
        } else {
            format!("{}× {}", h.gpus.len(), h.gpus[0].name)
        };
        s.push_str(&format!(
            "  {:<22} {:<8} {:<22} {:<12} {}{}\n",
            label,
            state,
            gpu,
            mem,
            mgmt,
            h.model.as_deref().map(|m| format!("  [{m}]")).unwrap_or_default()
        ));
        if h.gpus.len() > 1 {
            for g in &h.gpus {
                s.push_str(&format!(
                    "             gpu{} {}{}{}{}\n",
                    g.index,
                    g.name,
                    g.mem_total_gb.map(|m| format!(" {m:.0}GB")).unwrap_or_default(),
                    g.temp_c.map(|c| format!(" {c:.0}°C")).unwrap_or_default(),
                    g.util_pct.map(|u| format!(" {u:.0}%")).unwrap_or_default()
                ));
            }
        }
        if !h.running.is_empty() {
            s.push_str(&format!("             running: {}\n", h.running.join(", ")));
        }
        if !h.weights.is_empty() {
            s.push_str(&format!("             weights: {}\n", weights_summary(&h.weights)));
        }
        if let Some(f) = &h.fabric {
            for n in f.nics.iter().filter(|n| matches!(n.kind, fabric::NicKind::Cx7) && !n.ipv4.is_empty()) {
                let ips: Vec<String> = n.ipv4.iter().map(|a| format!("{}/{}", a.ip, a.prefix)).collect();
                s.push_str(&format!(
                    "             cx7 {} {} {}{}\n",
                    n.name,
                    ips.join(","),
                    n.speed_mbps.map(|m| format!("{}G", m / 1000)).unwrap_or_else(|| "-".into()),
                    n.rdma.as_deref().map(|r| format!(" rdma {r}")).unwrap_or_default()
                ));
            }
        }
        if let Some(e) = &h.fabric_error {
            s.push_str(&format!("             fabric: {e}\n"));
        }
    }
    if !t.switches.is_empty() {
        s.push_str("\nswitches\n");
        for sw in &t.switches {
            s.push_str(&format!("  {:<22} {:<8} {:>2} ports  {}\n", sw.name, sw.kind.as_str(), sw.ports, sw.subnet));
        }
    }
    match t.fabric.kind {
        fabric::ShapeKind::None | fabric::ShapeKind::Single => {}
        fabric::ShapeKind::Ring => {
            s.push_str(&format!("\nfabric: ring (switchless), cabled order {}\n", t.fabric.describe(|h| t.display(h))))
        }
        k => s.push_str(&format!("\nfabric: {} ({})\n", k.as_str(), t.fabric.describe(|h| t.display(h)))),
    }
    s.push_str("\nlinks\n");
    if t.links.is_empty() {
        s.push_str("  (none matched: no shared subnets between hosts' fabrics)\n");
    }
    for l in &t.links {
        let k = &l.link;
        let check = match k.verified {
            Some(true) => format!("ok{}", k.rtt_ms.map(|r| format!(" {r:.2} ms")).unwrap_or_default()),
            Some(false) => "UNREACHABLE".into(),
            None => "unverified".into(),
        };
        s.push_str(&format!(
            "  {:<10} {:<22} ⇄ {:<22} {:<10} {:<18} {:<6} {}{}{}\n",
            k.kind.as_str(),
            format!("{} {}", t.display(&k.a.host), k.a.ip),
            format!("{} {}", t.display(&k.b.host), k.b.ip),
            k.a.ifname.as_deref().unwrap_or("-"),
            k.subnet.as_deref().unwrap_or("-"),
            k.gbps().map(|g| format!("{g:.0}G")).unwrap_or_else(|| "-".into()),
            check,
            if k.manual { " (hosts.toml)" } else { "" },
            if l.active.is_empty() { String::new() } else { format!("  ● {}", l.active.join(", ")) }
        ));
    }
    for n in &t.notes {
        s.push_str(&format!("\nnote: {n}"));
    }
    if !t.notes.is_empty() {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::weights_summary;

    #[test]
    fn long_weight_lists_are_summarised() {
        let ids: Vec<String> = (1..=4).map(|i| format!("m{i}")).collect();
        assert_eq!(weights_summary(&ids), "m1, m2, m3, m4");
        let ids: Vec<String> = (1..=30).map(|i| format!("org/model-{i}")).collect();
        let s = weights_summary(&ids);
        assert_eq!(s, "30 models: org/model-1, org/model-2, org/model-3 and 27 more (`lmdeck weights` lists them)");
    }
}
