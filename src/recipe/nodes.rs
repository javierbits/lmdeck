//! The multi-node part of a recipe: `[cluster]` and `[[nodes]]`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Start order for `[[nodes]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartOrder {
    /// Head first (Ray: the head must be up before workers join).
    #[default]
    HeadFirst,
    /// Workers first (engines whose head waits for every worker to dial in).
    WorkersFirst,
}

/// `[cluster]`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClusterSpec {
    #[serde(default)]
    pub order: StartOrder,
    /// Seconds each node gets to pass its gate (default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_timeout: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    Head,
    Worker,
}

/// One `[[nodes]]` entry: what runs on one machine. Commands and env take
/// the engine placeholders plus `{head_addr}`, `{node_addr}`, `{ifname}`,
/// `{hca}`, `{gid_index}`, `{rank}`, `{nodes}`, `{node}`, `{image}`,
/// `{head_port}`, `{weights_mount}`, `{weights_volume}`, `{weights_at}`, the
/// fabric ones and the other nodes' `{peer_addr}` / `{peer_host}`,
/// `{peer2_addr}` / `{peer2_host}`, … (see docs/MULTIHOST.md).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub name: String,
    pub role: NodeRole,
    /// `local` / this machine's `[node] name`, a hosts.toml name, or
    /// [`AUTO`]: a worker lmdeck places on the head's CX7 fabric (`crate::pick`).
    pub host: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// Port the node's status is probed on (the head defaults to the
    /// recipe's `engine.port`; a worker to its `ready_port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Gate: the next node starts once this TCP port answers on the node
    /// (e.g. Ray's GCS on 6379). Without it, the gate is "still running
    /// after `ready_delay` seconds" (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_delay: Option<u64>,
}

impl NodeSpec {
    /// Every string that takes placeholders: command, env values, stop, container and logs.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.command
            .iter()
            .chain(self.env.values())
            .chain(&self.stop)
            .chain(&self.container)
            .chain(&self.logs)
            .map(String::as_str)
    }
}

/// `host = "auto"`: lmdeck picks the node's machine when it plans the start.
pub const AUTO: &str = "auto";
