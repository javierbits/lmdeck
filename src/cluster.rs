//! Multi-node recipes: `[[nodes]]` entries, each a command for one machine,
//! brought up in order with a readiness gate between nodes and torn down in
//! reverse. Engine-agnostic: lmdeck fills in the fast-link facts (head
//! address, interface, HCA, GID index) and the weights mount, and the
//! recipe's own commands decide what runs (Ray + vLLM, SGLang, …).
//!
//! The link facts mirror lmswitch's `vllm-dual.yaml`: the CX7 IPs become
//! `master_addr` / `worker_ip`, and NCCL is pinned to the CX7 interface and
//! HCA. Weights stay on one home and other nodes mount it over NFS, the way
//! Mia's `WORKER_WEIGHTS=nfs` does (see [`crate::placement`]).

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::deck::Deck;
use crate::fabric::{Graph, Link, LinkKind, Shape, ShapeKind};
use crate::hosts::LOCAL;
use crate::ops::{NodeState, Target};
use crate::paths::Paths;
use crate::placement::Nfs;
pub use crate::recipe::NodeRole as Role;
use crate::recipe::{Engine, NodeSpec, Recipe, StartOrder, AUTO};

#[derive(Debug, Clone, Default)]
pub struct Opts {
    pub only: bool,
    pub force: bool,
    pub allow_slow_link: bool,
}

/// One node as it was started: what topology and stop use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRec {
    pub name: String,
    pub role: Role,
    pub host: String,
    pub served_as: String,
    pub port: u16,
}

/// A multi-node run (`<state>/clusters/<recipe>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub recipe: String,
    /// In start order.
    pub nodes: Vec<NodeRec>,
    /// `starting`, `up`, `failed`.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub progress: Vec<String>,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("clusters")
}

impl Record {
    pub fn all(paths: &Paths) -> Vec<Record> {
        let Ok(rd) = fs::read_dir(dir(paths)) else { return vec![] };
        let mut v: Vec<Record> = rd
            .flatten()
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        v.sort_by(|a, b| a.recipe.cmp(&b.recipe));
        v
    }

    pub fn load(paths: &Paths, recipe: &str) -> Option<Record> {
        fs::read_to_string(dir(paths).join(format!("{recipe}.json"))).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    pub fn save(&self, paths: &Paths) {
        let _ = fs::create_dir_all(dir(paths));
        if let Ok(body) = serde_json::to_string_pretty(self) {
            let _ = fs::write(dir(paths).join(format!("{}.json", self.recipe)), body);
        }
    }

    pub fn remove(paths: &Paths, recipe: &str) {
        let _ = fs::remove_file(dir(paths).join(format!("{recipe}.json")));
    }

    /// The host node `name` was started on (where a `host = "auto"` node runs).
    pub fn host(&self, name: &str) -> Option<&str> {
        self.nodes.iter().find(|n| n.name == name).map(|n| n.host.as_str())
    }
}

/// One node's resolved launch.
#[derive(Debug, Clone, Serialize)]
pub struct NodePlan {
    pub name: String,
    pub role: Role,
    pub host: String,
    /// lmdeck picked the host (`host = "auto"`).
    pub auto: bool,
    pub rank: usize,
    pub served_as: String,
    pub port: u16,
    pub gate: String,
    pub link: Option<Link>,
    /// `home`, `nfs` or `none`.
    pub weights: String,
    pub nfs: Option<Nfs>,
    pub vars: BTreeMap<String, String>,
    pub recipe: Recipe,
    pub command_line: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub recipe: String,
    /// In start order.
    pub nodes: Vec<NodePlan>,
    /// The CX7 fabric across the nodes' hosts, in rank order.
    pub fabric: Shape,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn describe(&self, node_name: &str) -> Vec<String> {
        let show = |h: &str| if h == LOCAL { format!("{node_name} (local)") } else { h.to_string() };
        let mut out = vec![format!("{}: {} nodes, start order:", self.recipe, self.nodes.len())];
        match self.fabric.kind {
            ShapeKind::None | ShapeKind::Single => {}
            ShapeKind::Ring => {
                out.push(format!("fabric: ring (switchless), cabled order {}", self.fabric.describe(show)))
            }
            k => out.push(format!("fabric: {}", k.as_str())),
        }
        for (i, n) in self.nodes.iter().enumerate() {
            out.push(format!(
                "{}. {} [{}] on {}{} as {} (port {}), gate: {}",
                i + 1,
                n.name,
                if n.role == Role::Head { "head" } else { "worker" },
                show(&n.host),
                if n.auto { " (auto)" } else { "" },
                n.served_as,
                n.port,
                n.gate
            ));
            if let Some(l) = &n.link {
                out.push(format!("     link: {}", l.label()));
            }
            out.push(format!("     weights: {}", n.weights));
            if let Some(gb) = n.recipe.hardware.min_memory_gb {
                out.push(format!("     memory: needs ~{gb:.0} GB"));
            }
            if let Some(nfs) = &n.nfs {
                out.push(format!("     export on {}: {}", show(&nfs.home), nfs.export_line));
                out.push(format!("     volume: {}", nfs.volume_cmd));
                for c in &nfs.preflight_cmds {
                    out.push(format!("     preflight: {c}"));
                }
            }
            out.push(format!("     $ {}", n.command_line));
            for (k, v) in &n.recipe.engine.env {
                out.push(format!("       {k}={v}"));
            }
        }
        out.push("stop: reverse order".into());
        for w in &self.warnings {
            out.push(format!("warning: {w}"));
        }
        out
    }
}

fn subst(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// The best link from `a` to `b`. Two hosts that share no CX7 segment (across
/// a switchless ring, or two workers on either side of the head) get the CX7
/// path through the hosts cabled between them (and a warning), so the plan
/// stays on the fabric instead of falling back to the LAN.
pub(crate) fn link_for(
    graph: &Graph,
    a: &str,
    b: &str,
    show: &dyn Fn(&str) -> String,
    warnings: &mut Vec<String>,
) -> Option<Link> {
    let best = crate::fabric::path_as_link(graph, a, b);
    if best.as_ref().is_some_and(|l| l.kind.is_direct()) {
        return best;
    }
    let Some((ea, eb, via)) = crate::fabric::cable_path(graph, a, b) else { return best };
    let hops: Vec<String> = via.iter().map(|h| show(h)).collect();
    warnings.push(format!(
        "{} and {} share no cable: their CX7 traffic goes through {}, which routes it \
         (or the recipe relays it, as Mia's start-switchless.sh does)",
        show(a),
        show(b),
        hops.join(" → ")
    ));
    Some(Link {
        a: ea,
        b: eb,
        kind: LinkKind::Cx7,
        subnet: None,
        speed_mbps: None,
        manual: false,
        verified: None,
        rtt_ms: None,
    })
}

/// `{fabric}`, `{fabric_order}` and the ring neighbour placeholders for `host`.
/// The `ring_*` values stay empty off a ring.
fn fabric_vars(graph: &Graph, shape: &Shape, host: &str, show: &dyn Fn(&str) -> String) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    vars.insert("fabric".into(), shape.kind.as_str().to_string());
    vars.insert("fabric_order".into(), shape.order.iter().map(|h| show(h)).collect::<Vec<_>>().join(","));
    let ring = shape.ring_neighbours(host);
    let ring_rank = ring.and_then(|_| shape.order.iter().position(|h| h == host));
    vars.insert("ring_rank".into(), ring_rank.map(|i| i.to_string()).unwrap_or_default());
    for (side, nb) in [("prev", ring.map(|r| r.0)), ("next", ring.map(|r| r.1))] {
        let p = nb.and_then(|nb| crate::fabric::path_between(graph, host, nb)).filter(|p| p.2.is_direct());
        vars.insert(format!("ring_{side}"), nb.map(show).unwrap_or_default());
        vars.insert(format!("ring_{side}_addr"), p.as_ref().map(|p| p.1.ip.clone()).unwrap_or_default());
        vars.insert(format!("ring_{side}_ifname"), p.as_ref().and_then(|p| p.0.ifname.clone()).unwrap_or_default());
    }
    vars
}

/// A warning when the ranks do not walk the ring's cables. NCCL's ring (and
/// Mia's scripts) pass data rank to rank, so rank `i` and `i + 1` need a cable.
fn ring_order_warning(shape: &Shape, ranks: &[String], show: &dyn Fn(&str) -> String) -> Option<String> {
    if shape.kind != ShapeKind::Ring || shape.order.as_slice() == ranks {
        return None;
    }
    let listed: Vec<String> = ranks.iter().map(|h| show(h)).collect();
    Some(format!(
        "the ranks ({}) skip cables on the switchless ring: list the [[nodes]] in the cabled order {} \
         so each rank's neighbours are its cable neighbours",
        listed.join(" "),
        shape.describe(show)
    ))
}

/// The nodes in the order they start (and take their ranks).
pub(crate) fn start_order(r: &Recipe) -> Vec<&NodeSpec> {
    let order = r.cluster.as_ref().map(|c| c.order).unwrap_or_default();
    let (heads, workers): (Vec<&NodeSpec>, Vec<&NodeSpec>) = r.nodes.iter().partition(|n| n.role == Role::Head);
    match order {
        StartOrder::HeadFirst => heads.into_iter().chain(workers).collect(),
        StartOrder::WorkersFirst => workers.into_iter().chain(heads).collect(),
    }
}

pub fn node_name(r: &Recipe, n: &NodeSpec) -> String {
    format!("{}.{}", r.name, n.name)
}

/// The multi-node recipe among `recipes` that a run named `served_as` is a
/// node of, and that node's role.
pub fn node_of<'a>(recipes: impl IntoIterator<Item = &'a Recipe>, served_as: &str) -> Option<(&'a Recipe, Role)> {
    recipes.into_iter().find_map(|r| r.nodes.iter().find(|n| node_name(r, n) == served_as).map(|n| (r, n.role)))
}

/// Resolves every node: hosts, links, weights, placeholders.
pub fn plan(deck: &Deck, r: &Recipe, opts: &Opts) -> Result<Plan> {
    if r.nodes.is_empty() {
        bail!("{} has no [[nodes]]", r.name);
    }
    let auto: Vec<&str> = r.nodes.iter().filter(|n| n.host == AUTO).map(|n| n.name.as_str()).collect();
    let (resolved, fabric) = crate::pick::resolve(deck, r)?;
    let r = resolved.as_ref();
    let node = deck.cfg.node_name();
    let key = |h: &str| if crate::remote::is_local(h, &node) { LOCAL.to_string() } else { h.to_string() };
    for n in &r.nodes {
        crate::remote::check_host(&deck.paths, &key(&n.host), &node)?;
    }
    let head = r.nodes.iter().find(|n| n.role == Role::Head).ok_or_else(|| anyhow!("no head"))?;
    let head_key = key(&head.host);
    let mut keys: Vec<String> = r.nodes.iter().map(|n| key(&n.host)).collect();
    let idx = crate::weights::Index::fresh(&deck.paths, &deck.cfg, crate::weights::INDEX_TTL);
    let found = idx.home(r, &node);
    // Reason: every node's own recipe keeps the whole `[model]`, so the home would be
    // checked against the entire download; pin each node's share (or the explicit minimum).
    let node_memory_gb = crate::compat::required_memory_gb(r, found.as_ref().map(|h| h.size));
    let home = found.filter(|_| r.model.repo.is_some());
    if let Some(h) = &home {
        keys.push(h.host.clone());
    }
    let multi_host = r.nodes.iter().any(|n| key(&n.host) != head_key);
    let graph = match fabric {
        Some(g) => g,
        None if multi_host || home.as_ref().is_some_and(|h| keys.iter().any(|k| *k != h.host)) => {
            crate::topology::graph_for(&deck.paths, &deck.cfg, &keys)
        }
        None => crate::fabric::Graph::default(),
    };
    let mut warnings = vec![];
    let show = |h: &str| if h == LOCAL { node.clone() } else { h.to_string() };
    // Each host once, in rank order: the shape and ring order follow it.
    let mut ranks: Vec<String> = vec![];
    for n in start_order(r) {
        let k = key(&n.host);
        if !ranks.contains(&k) {
            ranks.push(k);
        }
    }
    let shape = crate::fabric::shape(&graph, &ranks);
    warnings.extend(ring_order_warning(&shape, &ranks, &show));
    let total = r.nodes.len();
    let image = r.engine.image.clone().unwrap_or_default();
    let first_other = r.nodes.iter().find(|n| key(&n.host) != head_key).map(|n| key(&n.host));
    let mut out = vec![];
    for (rank, n) in start_order(r).into_iter().enumerate() {
        let host = key(&n.host);
        // The head↔node link (for the head: its link to the first worker).
        let peer = if host == head_key { first_other.clone() } else { Some(host.clone()) };
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        let mut link_used = None;
        match peer {
            Some(p) if p != head_key => {
                let best = link_for(&graph, &head_key, &p, &show, &mut warnings);
                let (l, warn) = crate::placement::require_link(best.as_ref(), &head_key, &p, opts.allow_slow_link)?;
                warnings.extend(warn);
                let (h_end, p_end) = l.sides(&head_key).ok_or_else(|| anyhow!("link sides"))?;
                let me = if host == head_key { h_end } else { p_end };
                vars.insert("head_addr".into(), h_end.ip.clone());
                vars.insert("node_addr".into(), me.ip.clone());
                vars.insert("ifname".into(), me.ifname.clone().unwrap_or_default());
                vars.insert("hca".into(), me.hcas.join(","));
                vars.insert("gid_index".into(), me.gid_index.map(|g| g.to_string()).unwrap_or_default());
                link_used = Some(l.clone());
            }
            _ => {
                // Every node on one machine (tests, or a single box).
                for (k, v) in [("head_addr", "127.0.0.1"), ("node_addr", "127.0.0.1"), ("ifname", "lo")] {
                    vars.insert(k.into(), v.into());
                }
                vars.insert("hca".into(), String::new());
                vars.insert("gid_index".into(), String::new());
            }
        }
        let port = match n.role {
            Role::Head => n.port.unwrap_or(r.port()),
            Role::Worker => n.port.or(n.ready_port).unwrap_or(r.port() + rank as u16),
        };
        // Weights: the home bind-mounts its own cache; others mount its NFS export, when
        // their command mounts the weights at all (llama.cpp's rpc-server never does).
        let mounts = n.texts().flat_map(crate::vars::names).any(|v| matches!(v, "weights_mount" | "weights_volume"));
        let mut nfs = None;
        let weights = match &home {
            Some(h) if h.host == host || mounts => {
                let inv = idx.inventory(&h.host).cloned().unwrap_or_default();
                if h.host == host {
                    vars.insert("weights_volume".into(), inv.export_path.clone());
                    vars.insert("weights_at".into(), inv.mount_at.clone());
                    vars.insert("weights_mount".into(), format!("{}:{}", inv.export_path, inv.mount_at));
                    "home (local disk)".to_string()
                } else {
                    let best = link_for(&graph, &h.host, &host, &show, &mut warnings);
                    let (l, warn) =
                        crate::placement::require_link(best.as_ref(), &h.host, &host, opts.allow_slow_link)?;
                    warnings.extend(warn);
                    let (he, ne) = l.sides(&h.host).ok_or_else(|| anyhow!("link sides"))?;
                    let volume = crate::placement::volume_name(if h.host == LOCAL { &node } else { &h.host });
                    let item = crate::weights::find(&inv, r);
                    let commit = item.and_then(|it| it.revisions.first().map(|v| v.commit.clone()));
                    let repo = r.model.repo.clone().unwrap_or_default();
                    let hub = if inv.hub_rel.is_empty() { String::new() } else { format!("{}/", inv.hub_rel) };
                    let rel = format!(
                        "{hub}models--{}/snapshots/{}",
                        repo.replace('/', "--"),
                        commit.clone().unwrap_or_else(|| "<rev>".into())
                    );
                    let d = &deck.docker_bin();
                    let (t, f) = crate::placement::preflight_argv(d, &volume, &image, &rel);
                    nfs = Some(Nfs {
                        home: h.host.clone(),
                        volume: volume.clone(),
                        server: he.ip.clone(),
                        client: ne.ip.clone(),
                        export_path: inv.export_path.clone(),
                        mount_at: inv.mount_at.clone(),
                        repo,
                        commit,
                        snapshot_rel: rel,
                        export_line: crate::placement::export_line(&inv.export_path, &ne.ip),
                        export_cmds: crate::placement::export_cmds(&inv.export_path, &ne.ip),
                        volume_cmd: crate::placement::shell_join(&crate::placement::volume_argv(
                            d,
                            &volume,
                            &he.ip,
                            &inv.export_path,
                        )),
                        preflight_cmds: vec![crate::placement::shell_join(&t), crate::placement::shell_join(&f)],
                    });
                    vars.insert("weights_volume".into(), volume.clone());
                    vars.insert("weights_at".into(), inv.mount_at.clone());
                    vars.insert("weights_mount".into(), format!("{volume}:{}:ro", inv.mount_at));
                    format!("nfs from {} via {} (read-only, no copy)", h.host, l.label())
                }
            }
            _ => {
                for k in ["weights_volume", "weights_at", "weights_mount"] {
                    vars.insert(k.into(), String::new());
                }
                match home {
                    Some(_) => "not mounted (its command uses neither {weights_mount} nor {weights_volume})".into(),
                    None => "none".to_string(),
                }
            }
        };
        let served_as = node_name(r, n);
        vars.insert("rank".into(), rank.to_string());
        vars.insert("nodes".into(), total.to_string());
        vars.insert("node".into(), n.name.clone());
        vars.insert("role".into(), if n.role == Role::Head { "head".into() } else { "worker".into() });
        vars.insert("image".into(), image.clone());
        vars.insert("head_port".into(), r.port().to_string());
        vars.extend(fabric_vars(&graph, &shape, &host, &show));
        if let Some(p) = crate::vars::peer_past(n.texts(), total - 1) {
            bail!(
                "{}: [[nodes]] {}: {{{p}}} names a peer past the {} peer(s) each node of this {total}-node recipe has",
                r.name,
                n.name,
                total - 1
            );
        }
        if crate::vars::peer_past(n.texts(), 0).is_some() {
            // The other nodes in listing order, each as this node reaches it.
            let mut peers = vec![];
            for p in r.nodes.iter().filter(|p| p.name != n.name) {
                let at = key(&p.host);
                let addr = if at == host {
                    "127.0.0.1".to_string()
                } else {
                    let best = link_for(&graph, &host, &at, &show, &mut warnings);
                    let (l, warn) = crate::placement::require_link(best.as_ref(), &host, &at, opts.allow_slow_link)?;
                    warnings.extend(warn);
                    l.sides(&host).map(|(_, far)| far.ip.clone()).ok_or_else(|| anyhow!("link sides"))?
                };
                peers.push((show(&at), addr));
            }
            vars.extend(crate::vars::peer_vars(&peers));
        }
        let mut d = r.clone();
        d.name = served_as.clone();
        d.nodes = vec![];
        d.cluster = None;
        d.hardware.nodes = None;
        d.hardware.min_memory_gb = node_memory_gb;
        // Harness sync offers the recipe (its head serves it), never a node's own name.
        d.harness.chat = false;
        d.model.home = None;
        d.model.nfs_volume = None;
        d.engine.name = Engine::Custom;
        d.engine.port = Some(port);
        d.engine.command = n.command.iter().map(|a| subst(a, &vars)).collect();
        d.engine.env = n.env.iter().map(|(k, v)| (k.clone(), subst(v, &vars))).collect();
        d.engine.stop = n.stop.iter().map(|a| subst(a, &vars)).collect();
        d.engine.container = n.container.as_ref().map(|c| subst(c, &vars));
        d.engine.detached = n.detached;
        d.engine.logs = n.logs.iter().map(|a| subst(a, &vars)).collect();
        d.engine.args = vec![];
        d.engine.image = None;
        d.engine.entrypoint = vec![];
        d.engine.volumes = vec![];
        // What's left is filled where the node runs, or nowhere: say so now, not on the node.
        let at = |what: &str| format!("[[nodes]] {} {what}", n.name);
        let cmd = d.engine.command.iter().map(|s| (at("command"), s.as_str()));
        let env = d.engine.env.iter().map(|(k, v)| (at(&format!("env {k}")), v.as_str()));
        let rest = d.engine.stop.iter().chain(&d.engine.logs).chain(&d.engine.container);
        let rest = rest.map(|s| (at("stop, logs or container"), s.as_str()));
        crate::vars::check(&r.name, cmd.chain(env).chain(rest), crate::vars::on_machine)?;
        let command_line =
            shlex::try_join(d.engine.command.iter().map(String::as_str)).unwrap_or_else(|_| d.engine.command.join(" "));
        let gate = match (n.ready_port, n.role) {
            (Some(p), _) => format!("TCP {p} answers"),
            (None, Role::Head) if rank + 1 == total => "serves /v1/models".into(),
            (None, _) => format!("still running after {} s", n.ready_delay.unwrap_or(3)),
        };
        out.push(NodePlan {
            name: n.name.clone(),
            role: n.role,
            host,
            auto: auto.contains(&n.name.as_str()),
            rank,
            served_as,
            port,
            gate,
            link: link_used,
            weights,
            nfs,
            vars,
            recipe: d,
            command_line,
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    Ok(Plan { recipe: r.name.clone(), nodes: out, fabric: shape, warnings })
}

fn gate(deck: &Deck, t: &Target, n: &NodePlan, spec: &NodeSpec, timeout: Duration) -> Result<()> {
    let t0 = Instant::now();
    let delay = Duration::from_secs(spec.ready_delay.unwrap_or(3));
    loop {
        // A slow reply from a busy host is not a failure; the timeout below still applies.
        let state = match t.state(deck, &n.served_as, n.port) {
            Ok(s) => s,
            Err(_) if t0.elapsed() <= timeout => NodeState::Up("loading".into()),
            Err(e) => return Err(e),
        };
        if state == NodeState::Stopped {
            bail!("{} exited before its gate ({})", n.name, n.gate);
        }
        let ok = match spec.ready_port {
            Some(p) => t.port_open(p)?,
            None => t0.elapsed() >= delay,
        };
        if ok {
            return Ok(());
        }
        if t0.elapsed() > timeout {
            bail!("{} did not pass its gate ({}) within {} s", n.name, n.gate, timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Brings every node up in order; on a failure, stops the nodes it started
/// (newest first) and returns the failing node's log tail.
pub fn start(deck: &Deck, r: &Recipe, opts: &Opts, mut progress: impl FnMut(&str)) -> Result<Vec<String>> {
    let mut p = plan(deck, r, opts)?;
    for w in &p.warnings {
        progress(&format!("warning: {w}"));
    }
    let timeout = Duration::from_secs(r.cluster.as_ref().and_then(|c| c.gate_timeout).unwrap_or(120));
    let mut rec = Record {
        recipe: r.name.clone(),
        nodes: p
            .nodes
            .iter()
            .map(|n| NodeRec {
                name: n.name.clone(),
                role: n.role,
                host: n.host.clone(),
                served_as: n.served_as.clone(),
                port: n.port,
            })
            .collect(),
        phase: "starting".into(),
        error: None,
        progress: vec![],
    };
    rec.save(&deck.paths);
    // Reason: each node runs from its own checkout on its own machine; pinning them to the
    // commit this machine's checkout of the ref is at (which `lmdeck pull` moves) brings
    // every one of them to it, whatever they had checked out before.
    match crate::checkout::ensure(&deck.paths, r) {
        Ok(Some(o)) => {
            progress(&format!("checkout {}", o.describe()));
            for s in p.nodes.iter_mut().filter_map(|n| n.recipe.source.as_mut()) {
                s.git_ref = Some(o.commit().to_string());
            }
        }
        Ok(None) => {}
        Err(e) => {
            rec.phase = "failed".into();
            rec.error = Some(format!("{e:#}"));
            rec.save(&deck.paths);
            return Err(e);
        }
    }
    let mut started: Vec<(&NodePlan, Target)> = vec![];
    let mut lines = vec![];
    let fail = |e: anyhow::Error, started: &mut Vec<(&NodePlan, Target)>, who: &NodePlan, t: Option<&Target>| {
        let tail = t.map(|t| t.logs(deck, &who.served_as, 15)).unwrap_or_default();
        for (n, t) in started.iter().rev() {
            let _ = t.stop(deck, &n.served_as);
        }
        let msg = format!(
            "{e:#}\nstopped the {} node(s) already started, newest first{}",
            started.len(),
            if tail.is_empty() { String::new() } else { format!("\n--- {} log ---\n{}", who.name, tail.join("\n")) }
        );
        msg
    };
    for n in &p.nodes {
        let spec = r.nodes.iter().find(|s| s.name == n.name).expect("planned node");
        let (_, t) = match Target::of(deck, &n.host) {
            Ok(t) => t,
            Err(e) => {
                let m = fail(e, &mut started, n, None);
                rec.phase = "failed".into();
                rec.error = Some(m.clone());
                rec.save(&deck.paths);
                bail!(m);
            }
        };
        let step = (|| -> Result<Vec<String>> {
            let mut l = vec![];
            if let Some(nfs) = &n.nfs {
                l.extend(crate::placement::prepare_nfs(deck, nfs, &t, r.engine.image.as_deref().unwrap_or_default())?);
            }
            l.push(t.start_recipe(deck, &n.recipe, false, opts.force)?);
            Ok(l)
        })();
        match step {
            Ok(l) => {
                for x in &l {
                    progress(&format!("{}: {x}", n.name));
                }
                lines.extend(l);
            }
            Err(e) => {
                let m = fail(e, &mut started, n, Some(&t));
                rec.phase = "failed".into();
                rec.error = Some(m.clone());
                rec.save(&deck.paths);
                bail!("{}: {m}", n.name);
            }
        }
        if let Err(e) = gate(deck, &t, n, spec, timeout) {
            started.push((n, t));
            let (last, t) = started.pop().expect("just pushed");
            let tail = t.logs(deck, &last.served_as, 15);
            let _ = t.stop(deck, &last.served_as);
            let m = fail(e, &mut started, n, None);
            let m = if tail.is_empty() { m } else { format!("{m}\n--- {} log ---\n{}", n.name, tail.join("\n")) };
            rec.phase = "failed".into();
            rec.error = Some(m.clone());
            rec.save(&deck.paths);
            bail!(m);
        }
        progress(&format!("{}: passed gate ({})", n.name, n.gate));
        rec.progress.push(format!("{} up on {}", n.name, n.host));
        rec.save(&deck.paths);
        started.push((n, t));
    }
    rec.phase = "up".into();
    rec.save(&deck.paths);
    Ok(lines)
}

/// For the web and TUI: plans synchronously (so a bad link fails the
/// request), then brings nodes up on a thread; progress lands in the record.
pub fn start_background(deck: &Deck, r: &Recipe, opts: &Opts) -> Result<String> {
    plan(deck, r, opts)?;
    let (deck, r, opts) = (Deck::new(deck.paths.clone(), deck.cfg.clone()), r.clone(), opts.clone());
    let msg = format!("starting {} across {} nodes (see its status for progress)", r.name, r.nodes.len());
    std::thread::spawn(move || {
        let _ = start(&deck, &r, &opts, |_| {});
    });
    Ok(msg)
}

/// Stops every node, newest first.
pub fn stop(deck: &Deck, r: &Recipe) -> Result<String> {
    let nodes: Vec<NodeRec> = match Record::load(&deck.paths, &r.name) {
        Some(rec) => rec.nodes,
        // Reason: an auto node only has a machine once a start recorded one, and the record
        // is saved before any node starts, so without it no auto node can be running.
        None => start_order(r)
            .into_iter()
            .filter(|n| n.host != AUTO)
            .map(|n| NodeRec {
                name: n.name.clone(),
                role: n.role,
                host: n.host.clone(),
                served_as: node_name(r, n),
                port: 0,
            })
            .collect(),
    };
    let mut done = vec![];
    let mut errs = vec![];
    for n in nodes.iter().rev() {
        match Target::of(deck, &n.host).and_then(|(_, t)| t.stop(deck, &n.served_as)) {
            Ok(m) => done.push(format!("{} ({m})", n.name)),
            Err(e) => errs.push(format!("{}: {e:#}", n.name)),
        }
    }
    Record::remove(&deck.paths, &r.name);
    if !errs.is_empty() {
        bail!("stopped {}; failed: {}", done.join(", "), errs.join("; "));
    }
    Ok(format!("stopped {} (reverse order: {})", r.name, done.join(", ")))
}

/// Per-node status for snapshots: local nodes from their run records,
/// remote ones from the sampler's live file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeView {
    pub name: String,
    pub role: String,
    pub host: String,
    pub served_as: String,
    /// `loaded`, `loading`, `running`, `stopped`, `offline` or `unknown`.
    pub status: String,
}

pub fn views(paths: &Paths, r: &Recipe, node_name: &str) -> Vec<NodeView> {
    let live = crate::hosts::read_live(paths, Duration::from_secs(15));
    let rec = Record::load(paths, &r.name);
    start_order(r)
        .into_iter()
        .map(|n| {
            let host = match rec.as_ref().and_then(|x| x.host(&n.name)) {
                Some(h) => h.to_string(),
                None if crate::remote::is_local(&n.host, node_name) => LOCAL.to_string(),
                None => n.host.clone(),
            };
            let served_as = node_name_of(r, n);
            let port = rec
                .as_ref()
                .and_then(|x| x.nodes.iter().find(|m| m.name == n.name).map(|m| m.port))
                .filter(|p| *p > 0)
                .unwrap_or(r.port());
            let status = if host == AUTO {
                "stopped".into()
            } else if host == LOCAL {
                if crate::runner::RunRecord::load(paths, &served_as).is_some() {
                    crate::deck::status_word(&crate::runner::status(paths, &served_as, port)).to_string()
                } else {
                    "stopped".into()
                }
            } else {
                match live.as_ref().and_then(|l| l.hosts.get(&host)) {
                    Some((false, _)) => "offline".into(),
                    Some((true, running)) if running.contains(&served_as) => "running".into(),
                    Some((true, _)) => "stopped".into(),
                    None => "unknown".into(),
                }
            };
            NodeView {
                name: n.name.clone(),
                role: if n.role == Role::Head { "head".into() } else { "worker".into() },
                host,
                served_as,
                status,
            }
        })
        .collect()
}

fn node_name_of(r: &Recipe, n: &NodeSpec) -> String {
    node_name(r, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::{build_graph, fixtures};

    fn graph(set: &[(&str, &str)]) -> Graph {
        let f = fixtures::set(set);
        let refs: Vec<(String, &crate::fabric::Fabric)> = f.iter().map(|(k, f)| (k.clone(), f)).collect();
        build_graph(&refs, &[])
    }

    fn hosts(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn show(h: &str) -> String {
        h.to_string()
    }

    #[test]
    fn folded_ring_ranks_neighbours_and_cross_links() {
        let g = graph(&fixtures::FOLD4);
        let cabled = hosts(&["spark1", "spark2", "spark4", "spark3"]);
        let shape = crate::fabric::shape(&g, &cabled);
        assert_eq!(shape.kind, ShapeKind::Ring);
        assert!(ring_order_warning(&shape, &cabled, &show).is_none());
        // Ranks in rack order skip cables.
        let rack = hosts(&["spark1", "spark2", "spark3", "spark4"]);
        let w = ring_order_warning(&crate::fabric::shape(&g, &rack), &rack, &show).unwrap();
        assert!(w.contains("spark1 → spark2 → spark4 → spark3 → spark1"), "{w}");

        let v = fabric_vars(&g, &shape, "spark2", &show);
        assert_eq!(v["fabric"], "ring");
        assert_eq!(v["fabric_order"], "spark1,spark2,spark4,spark3");
        assert_eq!(v["ring_rank"], "1");
        assert_eq!(v["ring_prev"], "spark1");
        assert_eq!(v["ring_next"], "spark4");
        assert!(v["ring_prev_addr"].starts_with("10.41.") && v["ring_next_addr"].starts_with("10.41."), "{v:?}");
        assert!(!v["ring_next_ifname"].is_empty());

        // spark1 to spark4 sits across the ring: CX7 through spark2 or spark3, with a warning.
        let mut warnings = vec![];
        let l = link_for(&g, "spark1", "spark4", &show, &mut warnings).unwrap();
        assert_eq!(l.kind, LinkKind::Cx7);
        assert!(l.a.ip.starts_with("10.41.") && l.b.ip.starts_with("10.41."));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("share no cable"), "{}", warnings[0]);
        // Cable neighbours: direct, no warning.
        let mut warnings = vec![];
        let l = link_for(&g, "spark1", "spark3", &show, &mut warnings).unwrap();
        assert_eq!(l.kind, LinkKind::Cx7);
        assert!(warnings.is_empty());
    }

    #[test]
    fn plain_ring_triangle_pair_single_and_switch_vars() {
        let g = graph(&fixtures::RING4);
        let rack = hosts(&["spark1", "spark2", "spark3", "spark4"]);
        let shape = crate::fabric::shape(&g, &rack);
        assert!(ring_order_warning(&shape, &rack, &show).is_none());
        let v = fabric_vars(&g, &shape, "spark1", &show);
        assert_eq!((v["ring_prev"].as_str(), v["ring_next"].as_str()), ("spark4", "spark2"));

        let g = graph(&fixtures::TRI);
        let shape = crate::fabric::shape(&g, &hosts(&["spark1", "spark2", "spark3"]));
        let v = fabric_vars(&g, &shape, "spark1", &show);
        assert_eq!(v["fabric"], "mesh");
        assert_eq!(v["ring_prev"], "");
        let mut warnings = vec![];
        assert!(link_for(&g, "spark1", "spark3", &show, &mut warnings).is_some_and(|l| l.kind.is_direct()));

        let g = graph(&fixtures::DUO);
        let shape = crate::fabric::shape(&g, &hosts(&["spark1", "spark2"]));
        assert_eq!(fabric_vars(&g, &shape, "spark2", &show)["fabric"], "cable");

        let shape = crate::fabric::shape(&Graph::default(), &hosts(&["local"]));
        let v = fabric_vars(&Graph::default(), &shape, "local", &show);
        assert_eq!(
            (v["fabric"].as_str(), v["fabric_order"].as_str(), v["ring_rank"].as_str()),
            ("single", "local", "")
        );

        let g = graph(&fixtures::SWITCH8);
        let all: Vec<String> = (1..=8).map(|i| format!("spark{i}")).collect();
        let shape = crate::fabric::shape(&g, &all);
        let v = fabric_vars(&g, &shape, "spark8", &show);
        assert_eq!(v["fabric"], "switch");
        assert_eq!(v["fabric_order"].split(',').count(), 8);
        let mut warnings = vec![];
        assert!(link_for(&g, "spark1", "spark8", &show, &mut warnings).is_some_and(|l| l.kind.is_direct()));
        assert!(warnings.is_empty());
    }
}
