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
use crate::fabric::Link;
use crate::hosts::LOCAL;
use crate::ops::{NodeState, Target};
use crate::paths::Paths;
use crate::placement::Nfs;
pub use crate::recipe::NodeRole as Role;
use crate::recipe::{Engine, NodeSpec, Recipe, StartOrder};

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
}

/// One node's resolved launch.
#[derive(Debug, Clone, Serialize)]
pub struct NodePlan {
    pub name: String,
    pub role: Role,
    pub host: String,
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
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn describe(&self, node_name: &str) -> Vec<String> {
        let show = |h: &str| if h == LOCAL { format!("{node_name} (local)") } else { h.to_string() };
        let mut out = vec![format!("{}: {} nodes, start order:", self.recipe, self.nodes.len())];
        for (i, n) in self.nodes.iter().enumerate() {
            out.push(format!(
                "{}. {} [{}] on {} as {} (port {}), gate: {}",
                i + 1,
                n.name,
                if n.role == Role::Head { "head" } else { "worker" },
                show(&n.host),
                n.served_as,
                n.port,
                n.gate
            ));
            if let Some(l) = &n.link {
                out.push(format!("     link: {}", l.label()));
            }
            out.push(format!("     weights: {}", n.weights));
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

fn start_order(r: &Recipe) -> Vec<&NodeSpec> {
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

/// Resolves every node: hosts, links, weights, placeholders.
pub fn plan(deck: &Deck, r: &Recipe, opts: &Opts) -> Result<Plan> {
    if r.nodes.is_empty() {
        bail!("{} has no [[nodes]]", r.name);
    }
    let node = deck.cfg.node_name();
    let key = |h: &str| if crate::remote::is_local(h, &node) { LOCAL.to_string() } else { h.to_string() };
    for n in &r.nodes {
        crate::remote::check_host(&deck.paths, &key(&n.host), &node)?;
    }
    let head = r.nodes.iter().find(|n| n.role == Role::Head).ok_or_else(|| anyhow!("no head"))?;
    let head_key = key(&head.host);
    let mut keys: Vec<String> = r.nodes.iter().map(|n| key(&n.host)).collect();
    let idx = crate::weights::Index::fresh(&deck.paths, &deck.cfg, crate::weights::INDEX_TTL);
    let home = if r.model.repo.is_some() { idx.home(r, &node) } else { None };
    if let Some(h) = &home {
        keys.push(h.host.clone());
    }
    let multi_host = r.nodes.iter().any(|n| key(&n.host) != head_key);
    let graph = if multi_host || home.as_ref().is_some_and(|h| keys.iter().any(|k| *k != h.host)) {
        crate::topology::graph_for(&deck.paths, &deck.cfg, &keys)
    } else {
        crate::fabric::Graph::default()
    };
    let mut warnings = vec![];
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
                let best = crate::fabric::path_as_link(&graph, &head_key, &p);
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
        // Weights: the home bind-mounts its own cache; others mount its NFS export.
        let mut nfs = None;
        let weights = match &home {
            None => {
                vars.insert("weights_volume".into(), String::new());
                vars.insert("weights_at".into(), String::new());
                vars.insert("weights_mount".into(), String::new());
                "none".to_string()
            }
            Some(h) => {
                let inv = idx.inventory(&h.host).cloned().unwrap_or_default();
                if h.host == host {
                    vars.insert("weights_volume".into(), inv.export_path.clone());
                    vars.insert("weights_at".into(), inv.mount_at.clone());
                    vars.insert("weights_mount".into(), format!("{}:{}", inv.export_path, inv.mount_at));
                    "home (local disk)".to_string()
                } else {
                    let best = crate::fabric::path_as_link(&graph, &h.host, &host);
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
        };
        let served_as = node_name(r, n);
        vars.insert("rank".into(), rank.to_string());
        vars.insert("nodes".into(), total.to_string());
        vars.insert("node".into(), n.name.clone());
        vars.insert("role".into(), if n.role == Role::Head { "head".into() } else { "worker".into() });
        vars.insert("image".into(), image.clone());
        vars.insert("head_port".into(), r.port().to_string());
        let mut d = r.clone();
        d.name = served_as.clone();
        d.nodes = vec![];
        d.cluster = None;
        d.hardware.nodes = None;
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
    Ok(Plan { recipe: r.name.clone(), nodes: out, warnings })
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
    let p = plan(deck, r, opts)?;
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
        None => start_order(r)
            .into_iter()
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
            let host = if crate::remote::is_local(&n.host, node_name) { LOCAL.to_string() } else { n.host.clone() };
            let served_as = node_name_of(r, n);
            let port = rec
                .as_ref()
                .and_then(|x| x.nodes.iter().find(|m| m.name == n.name).map(|m| m.port))
                .filter(|p| *p > 0)
                .unwrap_or(r.port());
            let status = if host == LOCAL {
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
