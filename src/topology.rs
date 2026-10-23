//! The controller's picture of the cluster: every host (with its telemetry,
//! running models and weights), the links between them from
//! [`crate::fabric::match_links`], and the management links from this
//! machine to each `hosts.toml` URL. Served as `GET /api/topology`, drawn as
//! an SVG on the web Hosts tab, a box diagram in the TUI, and printed by
//! `lmdeck hosts topology [--json]`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::Config;
use crate::fabric::{self, Fabric, Link, LinkKind};
use crate::hosts::{HostEntry, HostView, HostsFile, LOCAL};
use crate::paths::Paths;
use crate::weights::Index;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TopoHost {
    /// `local` or the hosts.toml name (what links refer to).
    pub key: String,
    /// What to show: this machine's `[node] name`, else the key.
    pub name: String,
    pub url: Option<String>,
    pub local: bool,
    pub online: bool,
    pub error: Option<String>,
    pub gpu: Option<String>,
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
    /// The controller's key (`local`).
    pub controller: String,
    pub hosts: Vec<TopoHost>,
    pub links: Vec<TopoLink>,
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
        self.host(key).map(|h| h.name.clone()).unwrap_or_else(|| key.to_string())
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

/// One host's fabric: this machine detects, others are asked; demo mode
/// reads the built-in 3-Spark fixtures (this machine plays spark1).
pub fn fetch_fabric(k: &str, entry: Option<&HostEntry>, online: bool, demo: bool) -> Result<Fabric, String> {
    if demo {
        let want = if k == LOCAL { "spark1" } else { k };
        return fabric::fixtures::TRI
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
fn links_from(
    file: &HostsFile,
    entries: &BTreeMap<String, HostEntry>,
    node: &str,
    with_fabric: &[(String, &Fabric)],
) -> Vec<Link> {
    // Overrides name hosts by hosts.toml name or by this machine's node name.
    let overrides: Vec<crate::hosts::LinkOverride> = file
        .links
        .iter()
        .map(|o| {
            let fix = |h: &str| if crate::remote::is_local(h, node) { LOCAL.to_string() } else { h.to_string() };
            crate::hosts::LinkOverride { a: fix(&o.a), b: fix(&o.b), ..o.clone() }
        })
        .collect();
    let mut links = fabric::match_links(with_fabric, &overrides);
    for l in links.iter_mut() {
        for end in [&mut l.a, &mut l.b] {
            if let Some(addr) = entries.get(&end.host).and_then(|e| e.link_addr.clone()) {
                if l.kind.is_direct() {
                    end.ip = addr;
                }
            }
        }
    }
    links
}

/// Links among just `keys` (for placement and multi-node planning), without
/// telemetry or reachability checks.
pub fn links_for(paths: &Paths, cfg: &Config, keys: &[String]) -> Vec<Link> {
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
    links_from(&file, &entries, &cfg.node_name(), &refs)
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

/// Builds the topology. `views` come from the [`crate::hosts::Sampler`];
/// `verify` pings each link from its `a` side (through that host's serve).
pub fn build(paths: &Paths, cfg: &Config, views: &[HostView], verify: bool) -> Topology {
    let demo = crate::telemetry::demo_mode();
    let file = HostsFile::load(paths).unwrap_or_default();
    let entries: BTreeMap<String, HostEntry> = file.hosts.iter().map(|e| (e.name.clone(), e.clone())).collect();
    let node = cfg.node_name();
    let index = Index::fresh(paths, cfg, crate::weights::INDEX_TTL);
    // Fabric per host, in parallel.
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
        let name = if v.local {
            if demo {
                "spark1".to_string()
            } else {
                node.clone()
            }
        } else {
            v.name.clone()
        };
        hosts.push(TopoHost {
            key: v.name.clone(),
            name,
            url: v.url.clone(),
            local: v.local,
            online: v.online,
            error: v.error.clone(),
            gpu: t.and_then(|t| t.gpus.first().map(|g| g.name.clone())),
            mem_total_gb: t.map(|t| t.mem_total_gb).filter(|m| *m > 0.0),
            mem_used_gb: t.and_then(|t| t.mem_used_gb),
            temp_c: t.and_then(|t| t.gpus.first().and_then(|g| g.temp_c).or(t.cpu_temp_c)),
            running: t.map(|t| t.models.iter().map(|m| m.name.clone()).collect()).unwrap_or_default(),
            weights,
            fabric_error: f.as_ref().err().cloned(),
            fabric: f.ok(),
            mgmt_kind,
            mgmt_addr,
        });
    }
    let with_fabric: Vec<(String, &Fabric)> =
        hosts.iter().filter_map(|h| h.fabric.as_ref().map(|f| (h.key.clone(), f))).collect();
    let mut links = links_from(&file, &entries, &node, &with_fabric);
    if verify && !demo {
        let checks: Vec<(bool, Option<f64>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = links
                .iter()
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
        for (l, (ok, rtt)) in links.iter_mut().zip(checks) {
            l.verified = Some(ok);
            l.rtt_ms = rtt;
        }
    }
    let busy = if demo { demo_activity(&links) } else { activity(paths, &hosts) };
    let links: Vec<TopoLink> = links
        .into_iter()
        .map(|l| {
            let active = busy
                .iter()
                .filter(|(a, b, _)| l.involves(a) && l.involves(b) && a != b)
                .filter(|(a, b, _)| fabric::best_link(std::slice::from_ref(&l), a, b).is_some())
                .map(|(_, _, why)| why.clone())
                .collect();
            TopoLink { link: l, active }
        })
        .collect();
    // Only the best link of a pair carries the activity.
    let mut links = links;
    let raw: Vec<Link> = links.iter().map(|l| l.link.clone()).collect();
    for l in links.iter_mut() {
        let best = fabric::best_link(&raw, &l.link.a.host, &l.link.b.host);
        if best != Some(&l.link) {
            l.active.clear();
        }
    }
    let mut notes = vec![];
    if demo {
        notes.push("demo data (LMDECK_DEMO=1): a simulated 3-Spark triangle from built-in fixtures".into());
    }
    for h in &hosts {
        if let Some(f) = &h.fabric {
            if !f.has_cx7() && !f.nics.is_empty() {
                notes.push(format!("{}: no ConnectX-7 ports detected", h.name));
            }
        }
    }
    Topology { at: crate::telemetry::now_ms(), demo, controller: LOCAL.into(), hosts, links, notes }
}

fn demo_activity(links: &[Link]) -> Vec<(String, String, String)> {
    // Demo only: pretend a 2-node run spans local↔spark2 over their cable.
    links
        .iter()
        .filter(|l| l.kind == LinkKind::Cx7 && l.involves(LOCAL) && l.involves("spark2"))
        .map(|_| (LOCAL.to_string(), "spark2".to_string(), "cluster: demo multi-node run".to_string()))
        .collect()
}

/// `lmdeck hosts topology`: one block per host, then the links.
pub fn render_text(t: &Topology) -> String {
    let mut s = String::new();
    if t.demo {
        s.push_str("DEMO DATA — simulated 3-Spark triangle (LMDECK_DEMO=1)\n\n");
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
        s.push_str(&format!(
            "  {:<10} {:<8} {:<14} {:<12} {}\n",
            h.name,
            state,
            h.gpu.as_deref().unwrap_or("-"),
            mem,
            mgmt
        ));
        if !h.running.is_empty() {
            s.push_str(&format!("             running: {}\n", h.running.join(", ")));
        }
        if !h.weights.is_empty() {
            s.push_str(&format!("             weights: {}\n", h.weights.join(", ")));
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
            "  {:<10} {:<16} ⇄ {:<16} {:<10} {:<9} {:<6} {}{}{}\n",
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
