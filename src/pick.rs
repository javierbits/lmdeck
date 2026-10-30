//! Picking machines on the ConnectX-7 fabric for `host = "auto"` in `[[nodes]]`.
//!
//! The candidates for a head are the machines with a CX7 path to it, best
//! first. On a switchless ring: its cable neighbours (next, then previous in
//! the cabled order), then onward around the ring from next. On any other
//! shape (a cable, a mesh, a switch, a mix): the machines sharing a CX7
//! segment with it, then the ones reached through other machines' cables,
//! each group in rack order (this machine, then hosts.toml). Machines that
//! don't answer or have no CX7 path are never picked. Picks are
//! deterministic, and ranked so that around a ring they follow the cables
//! from the head, the order NCCL passes data in.

use anyhow::{anyhow, bail, Result};
use std::borrow::Cow;

use crate::deck::Deck;
use crate::fabric::{Graph, LinkKind, ShapeKind};
use crate::hosts::{HostsFile, LOCAL};
use crate::recipe::{NodeRole, Recipe, AUTO};

/// Whether `host` has a CX7 port on `graph`: a member of a CX7 segment, or an
/// end of a CX7 link.
pub fn on_cx7(graph: &Graph, host: &str) -> bool {
    graph.segments.iter().any(|s| s.kind == LinkKind::Cx7 && s.members.iter().any(|m| m.host == host))
        || graph.links.iter().any(|l| l.kind == LinkKind::Cx7 && l.involves(host))
}

/// This machine (`local`), then every hosts.toml host: the rack order.
pub fn rack(deck: &Deck) -> Vec<String> {
    let node = deck.cfg.node_name();
    let mut keys = vec![LOCAL.to_string()];
    for e in HostsFile::load(&deck.paths).map(|f| f.hosts).unwrap_or_default() {
        let k = if crate::remote::is_local(&e.name, &node) { LOCAL.to_string() } else { e.name };
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

/// The machines of `rack` a head on `head` may take, best first, each with
/// its rank: around a ring, its steps from the head in cabled order;
/// otherwise its place in the list. Empty when the head has no CX7 port.
pub fn candidates(graph: &Graph, rack: &[String], head: &str) -> Vec<(String, usize)> {
    let cx7: Vec<String> = rack.iter().filter(|h| on_cx7(graph, h)).cloned().collect();
    if !cx7.iter().any(|h| h == head) {
        return vec![];
    }
    let shape = crate::fabric::shape(graph, &cx7);
    if let (ShapeKind::Ring, Some(i)) = (shape.kind, shape.order.iter().position(|h| h == head)) {
        let n = shape.order.len();
        // Reason: next and previous share a cable with the head, and going on from next
        // keeps the picks one arc of the ring around it.
        return [1, n - 1].into_iter().chain(2..n - 1).map(|k| (shape.order[(i + k) % n].clone(), k)).collect();
    }
    let direct = |h: &&String| crate::fabric::path_between(graph, head, h).is_some_and(|p| p.2 == LinkKind::Cx7);
    let (near, far): (Vec<&String>, Vec<&String>) = cx7.iter().filter(|h| *h != head).partition(direct);
    let far = far.into_iter().filter(|h| crate::fabric::cable_path(graph, head, h).is_some());
    near.into_iter().chain(far).enumerate().map(|(i, h)| (h.clone(), i)).collect()
}

/// Picks `count` of the [`candidates`] for a head on `head` that `used`
/// doesn't hold, ranked (around a ring, in cabled order from the head).
/// When there are too few, says which machines are free, taken by other
/// nodes, and offline or off the head's CX7 fabric.
pub fn choose(
    graph: &Graph,
    rack: &[String],
    head: &str,
    used: &[String],
    count: usize,
    show: &dyn Fn(&str) -> String,
) -> Result<Vec<String>> {
    let all = candidates(graph, rack, head);
    let mut free: Vec<&(String, usize)> = all.iter().filter(|(h, _)| !used.contains(h)).collect();
    if free.len() < count {
        let list = |v: Vec<&String>| v.iter().map(|h| show(h)).collect::<Vec<_>>().join(", ");
        let taken: Vec<&String> = all.iter().map(|(h, _)| h).filter(|h| used.contains(h)).collect();
        let away: Vec<&String> = rack.iter().filter(|h| *h != head && !all.iter().any(|(c, _)| c == *h)).collect();
        let mut why = format!(
            "only {} machine(s) on {}'s CX7 fabric are free ({})",
            free.len(),
            show(head),
            if free.is_empty() { "none".into() } else { list(free.iter().map(|(h, _)| h).collect()) }
        );
        if !taken.is_empty() {
            why.push_str(&format!("; taken by other nodes: {}", list(taken)));
        }
        if !away.is_empty() {
            why.push_str(&format!("; offline or without a CX7 path to {}: {}", show(head), list(away)));
        }
        bail!(why);
    }
    free.truncate(count);
    free.sort_by_key(|(_, rank)| *rank);
    Ok(free.into_iter().map(|(h, _)| h.clone()).collect())
}

/// Gives each `host = "auto"` node of `r` a machine, in start order. Returns
/// the recipe with every host named, and the fabric graph across this
/// machine and hosts.toml it picked from (`None` when no node is auto).
pub fn resolve<'a>(deck: &Deck, r: &'a Recipe) -> Result<(Cow<'a, Recipe>, Option<Graph>)> {
    let auto: Vec<&str> =
        crate::cluster::start_order(r).into_iter().filter(|n| n.host == AUTO).map(|n| n.name.as_str()).collect();
    if auto.is_empty() {
        return Ok((Cow::Borrowed(r), None));
    }
    let node = deck.cfg.node_name();
    let key = |h: &str| if crate::remote::is_local(h, &node) { LOCAL.to_string() } else { h.to_string() };
    let show = |h: &str| if h == LOCAL { node.clone() } else { h.to_string() };
    let head =
        r.nodes.iter().find(|n| n.role == NodeRole::Head).map(|n| key(&n.host)).ok_or_else(|| anyhow!("no head"))?;
    let used: Vec<String> = r.nodes.iter().filter(|n| n.host != AUTO).map(|n| key(&n.host)).collect();
    let rack = rack(deck);
    let graph = crate::topology::graph_for(&deck.paths, &deck.cfg, &rack);
    let picked = choose(&graph, &rack, &head, &used, auto.len(), &show).map_err(|e| {
        anyhow!(
            "{}: {} [[nodes]] use host = \"auto\" but {e:#}. Add Sparks to hosts.toml (`lmdeck setup`), cable them, or name the hosts",
            r.name,
            auto.len()
        )
    })?;
    let mut out = r.clone();
    for n in out.nodes.iter_mut() {
        if let Some(i) = auto.iter().position(|a| *a == n.name) {
            n.host = picked[i].clone();
        }
    }
    Ok((Cow::Owned(out), Some(graph)))
}
