//! What a machine's network looks like, detected read-only and without sudo,
//! and how the machines on `hosts.toml` are wired to each other.
//!
//! Each `lmdeck serve` answers `GET /api/fabric` with its own [`Fabric`]:
//! - NICs and IPv4/IPv6 addresses from `ip -j addr` (plus `ip -j link`),
//!   falling back to `/sys/class/net` when `ip` is missing;
//! - per NIC `/sys/class/net/<if>/{speed,operstate,mtu}` and the driver
//!   (`device/driver` symlink, `mlx5_core` on ConnectX-7);
//! - RDMA devices from `/sys/class/net/<if>/device/infiniband/*`, checked
//!   against `/sys/class/infiniband/*/device/net/*` and `rdma -j link` if
//!   present, and the RoCE v2 IPv4 GID index (same scan as Mia's
//!   `link_info`);
//! - `ibv_devices` and `lldpctl -f json` when installed, `nvidia-smi` GPUs.
//!
//! Missing tools are reported in [`Fabric::tools`], never as errors.
//!
//! The controller builds an L2 graph ([`build_graph`] / [`match_links`]):
//! hosts that share a subnet (or an LLDP bridge neighbour) form a *segment*.
//! Two hosts on a point-to-point segment are a direct cable; three or more,
//! or an LLDP-identified switch, become a synthetic switch node with star
//! edges. Any number of hosts, rails, switches and mixed CX7 / LAN /
//! Tailscale links. `[[link]]` entries in hosts.toml override detection.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::hardware::{parse_nvidia_smi, Gpu};
use crate::hosts::LinkOverride;
use crate::util::run_with_timeout;

/// What a NIC is, as far as linking hosts goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NicKind {
    /// mlx5 with an RDMA device: a ConnectX-7 port (RoCE capable).
    Cx7,
    /// Another RDMA-capable port.
    Rdma,
    Ethernet,
    Wifi,
    Tailscale,
    Loopback,
    /// docker0, bridges, veths: never a link between hosts.
    Virtual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Addr {
    pub ip: String,
    pub prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Nic {
    pub name: String,
    pub kind: NicKind,
    pub mac: Option<String>,
    /// `up`, `down`, `unknown`, …
    pub state: String,
    pub mtu: Option<u32>,
    /// Mb/s from sysfs (`None` when the driver reports -1 or nothing).
    pub speed_mbps: Option<u64>,
    pub driver: Option<String>,
    pub ipv4: Vec<Addr>,
    pub ipv6: Vec<Addr>,
    /// RDMA device on this port (`rocep1s0f1`).
    pub rdma: Option<String>,
    /// RoCE v2 GID index whose GID is this port's IPv4 address.
    pub gid_index: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Neighbor {
    pub ifname: String,
    pub chassis: Option<String>,
    pub port: Option<String>,
    /// LLDP advertised the Bridge capability (or the chassis name looks like a switch).
    #[serde(default)]
    pub is_bridge: bool,
}

/// Which optional tools were there.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tools {
    pub ip: bool,
    pub rdma: bool,
    pub ibv_devices: bool,
    pub lldpctl: bool,
    pub nvidia_smi: bool,
}

/// One machine's network, as its `/api/fabric` reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fabric {
    pub hostname: String,
    /// DMI `product_name` / `hw.model` when readable (e.g. "DGX Spark", "Mac14,13").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    /// Friendly device kind for icons: `spark`, `mac`, `macstudio`, `pc`, `unknown`.
    #[serde(default)]
    pub device_kind: String,
    pub nics: Vec<Nic>,
    pub gpus: Vec<Gpu>,
    pub lldp: Vec<Neighbor>,
    /// RDMA devices `ibv_devices` lists (empty when it is not installed).
    pub ibv_devices: Vec<String>,
    pub tools: Tools,
    /// Things that were missing or unreadable (informational).
    pub notes: Vec<String>,
    #[serde(default)]
    pub demo: bool,
}

impl Fabric {
    pub fn nic(&self, name: &str) -> Option<&Nic> {
        self.nics.iter().find(|n| n.name == name)
    }

    pub fn has_cx7(&self) -> bool {
        self.nics.iter().any(|n| n.kind == NicKind::Cx7)
    }
}

/// Where detection reads from: the real machine, or a fixture in tests.
pub trait Probe {
    fn read(&self, path: &str) -> Option<String>;
    /// Entry names in a directory (empty when missing).
    fn list(&self, dir: &str) -> Vec<String>;
    /// Target of a symlink.
    fn link(&self, path: &str) -> Option<String>;
    /// Stdout of a command that exited 0 (`None` when missing or failing).
    fn run(&self, cmd: &str) -> Option<String>;
    fn hostname(&self) -> String;
}

/// The running machine.
pub struct System;

impl Probe for System {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
    }
    fn list(&self, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
    fn link(&self, path: &str) -> Option<String> {
        std::fs::read_link(path).ok().map(|p| p.display().to_string())
    }
    fn run(&self, cmd: &str) -> Option<String> {
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        run_with_timeout(Command::new(argv[0]).args(&argv[1..]), Duration::from_secs(4))
    }
    fn hostname(&self) -> String {
        crate::util::hostname()
    }
}

/// A recorded machine: `{"hostname", "commands": {cmd: stdout}, "files": {path: text},
/// "dirs": {path: [names]}, "links": {path: target}}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Fixture {
    pub hostname: String,
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub dirs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub links: BTreeMap<String, String>,
}

impl Probe for Fixture {
    fn read(&self, path: &str) -> Option<String> {
        self.files.get(path).map(|s| s.trim().to_string())
    }
    fn list(&self, dir: &str) -> Vec<String> {
        self.dirs.get(dir).cloned().unwrap_or_default()
    }
    fn link(&self, path: &str) -> Option<String> {
        self.links.get(path).cloned()
    }
    fn run(&self, cmd: &str) -> Option<String> {
        self.commands.get(cmd).cloned()
    }
    fn hostname(&self) -> String {
        self.hostname.clone()
    }
}

/// Detects this machine's fabric (read-only, no sudo).
pub fn detect() -> Fabric {
    if !cfg!(target_os = "linux") {
        let mut f = Fabric { hostname: crate::util::hostname(), ..Default::default() };
        f.notes.push("fabric detection reads Linux sysfs and `ip`; this OS reports no NICs".into());
        return f;
    }
    detect_with(&System)
}

fn base(p: &str) -> String {
    Path::new(p).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Detection over any [`Probe`].
pub fn detect_with(p: &dyn Probe) -> Fabric {
    let mut f = Fabric { hostname: p.hostname(), ..Default::default() };
    // Interfaces and addresses.
    let addr_json = p.run("ip -j addr");
    let link_json = p.run("ip -j link");
    f.tools.ip = addr_json.is_some() || link_json.is_some();
    let mut nics: Vec<Nic> = vec![];
    let mut take = |arr: &[Value]| {
        for it in arr {
            let Some(name) = it.get("ifname").and_then(Value::as_str) else { continue };
            let idx = match nics.iter().position(|n| n.name == name) {
                Some(i) => i,
                None => {
                    nics.push(blank_nic(name));
                    nics.len() - 1
                }
            };
            let n = &mut nics[idx];
            if let Some(m) = it.get("address").and_then(Value::as_str) {
                n.mac = Some(m.to_string());
            }
            if let Some(s) = it.get("operstate").and_then(Value::as_str) {
                n.state = s.to_ascii_lowercase();
            }
            if let Some(m) = it.get("mtu").and_then(Value::as_u64) {
                n.mtu = Some(m as u32);
            }
            for a in it.get("addr_info").and_then(Value::as_array).into_iter().flatten() {
                let (Some(ip), Some(pl)) = (a.get("local").and_then(Value::as_str), a.get("prefixlen")) else {
                    continue;
                };
                let addr = Addr { ip: ip.to_string(), prefix: pl.as_u64().unwrap_or(0) as u8 };
                match a.get("family").and_then(Value::as_str) {
                    Some("inet") if !n.ipv4.contains(&addr) => n.ipv4.push(addr),
                    Some("inet6") if !n.ipv6.contains(&addr) => n.ipv6.push(addr),
                    _ => {}
                }
            }
        }
    };
    for j in [&link_json, &addr_json].into_iter().flatten() {
        match serde_json::from_str::<Vec<Value>>(j) {
            Ok(arr) => take(&arr),
            Err(e) => f.notes.push(format!("could not parse `ip -j` output: {e}")),
        }
    }
    if nics.is_empty() {
        if !f.tools.ip {
            f.notes.push("`ip` (iproute2) not found: NIC names from /sys/class/net, no addresses".into());
        }
        for name in p.list("/sys/class/net") {
            let mut n = blank_nic(&name);
            n.mac = p.read(&format!("/sys/class/net/{name}/address"));
            n.state = p.read(&format!("/sys/class/net/{name}/operstate")).unwrap_or_else(|| "unknown".into());
            n.mtu = p.read(&format!("/sys/class/net/{name}/mtu")).and_then(|s| s.parse().ok());
            nics.push(n);
        }
    }
    // rdma -j link: netdev ↔ RDMA device (when iproute2's rdma is there).
    let mut rdma_of: BTreeMap<String, String> = BTreeMap::new();
    if let Some(out) = p.run("rdma -j link") {
        f.tools.rdma = true;
        if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&out) {
            for l in arr {
                if let (Some(dev), Some(nd)) =
                    (l.get("ifname").and_then(Value::as_str), l.get("netdev").and_then(Value::as_str))
                {
                    rdma_of.insert(nd.to_string(), dev.split('/').next().unwrap_or(dev).to_string());
                }
            }
        }
    }
    // /sys/class/infiniband/<dev>/device/net/<netdev>.
    for dev in p.list("/sys/class/infiniband") {
        for nd in p.list(&format!("/sys/class/infiniband/{dev}/device/net")) {
            rdma_of.entry(nd).or_insert_with(|| dev.clone());
        }
    }
    for n in nics.iter_mut() {
        let sys = format!("/sys/class/net/{}", n.name);
        n.speed_mbps =
            p.read(&format!("{sys}/speed")).and_then(|s| s.parse::<i64>().ok()).filter(|v| *v > 0).map(|v| v as u64);
        if n.state.is_empty() || n.state == "unknown" {
            if let Some(s) = p.read(&format!("{sys}/operstate")) {
                n.state = s;
            }
        }
        n.driver = p.link(&format!("{sys}/device/driver")).map(|t| base(&t)).filter(|s| !s.is_empty());
        n.rdma =
            p.list(&format!("{sys}/device/infiniband")).into_iter().next().or_else(|| rdma_of.get(&n.name).cloned());
        if let (Some(dev), Some(v4)) = (&n.rdma, n.ipv4.first()) {
            n.gid_index = roce_v2_gid(p, dev, &v4.ip);
        }
        n.kind = classify(n);
    }
    if !nics.iter().any(|n| n.rdma.is_some()) {
        f.notes.push("no RDMA devices: no ConnectX-7 links on this machine (LAN / Tailscale only)".into());
    }
    nics.sort_by_key(|n| (kind_rank(n.kind), n.name.to_ascii_lowercase()));
    f.nics = nics;
    if let Some(out) = p.run("ibv_devices") {
        f.tools.ibv_devices = true;
        f.ibv_devices = out.lines().skip(2).filter_map(|l| l.split_whitespace().next().map(str::to_string)).collect();
    }
    if let Some(out) = p.run("lldpctl -f json") {
        f.tools.lldpctl = true;
        f.lldp = parse_lldp(&out);
    }
    if let Some(out) =
        p.run("nvidia-smi --query-gpu=name,memory.total,compute_cap,driver_version --format=csv,noheader,nounits")
    {
        f.tools.nvidia_smi = true;
        f.gpus = parse_nvidia_smi(&out);
    }
    f.product =
        p.read("/sys/class/dmi/id/product_name").or_else(|| p.run("sysctl -n hw.model")).filter(|s| !s.is_empty());
    f.device_kind = device_kind(f.product.as_deref(), &f.gpus, &f.hostname);
    f
}

/// Icon / label kind from DMI, GPU list and hostname hints.
pub fn device_kind(product: Option<&str>, gpus: &[Gpu], hostname: &str) -> String {
    let p = product.unwrap_or("").to_ascii_lowercase();
    let host = hostname.to_ascii_lowercase();
    if p.contains("dgx spark") || gpus.iter().any(|g| g.name.to_uppercase().contains("GB10")) || host.contains("spark")
    {
        return "spark".into();
    }
    if p.starts_with("mac")
        || p.contains("macbook")
        || p.contains("mac studio")
        || p.contains("macmini")
        || p.contains("mac pro")
    {
        if p.contains("studio") || host.contains("studio") {
            return "macstudio".into();
        }
        return "mac".into();
    }
    if gpus.iter().any(|g| g.vendor == "apple") {
        return if host.contains("studio") { "macstudio".into() } else { "mac".into() };
    }
    if !gpus.is_empty() || !p.is_empty() {
        return "pc".into();
    }
    "unknown".into()
}

fn blank_nic(name: &str) -> Nic {
    Nic {
        name: name.to_string(),
        kind: NicKind::Ethernet,
        mac: None,
        state: "unknown".into(),
        mtu: None,
        speed_mbps: None,
        driver: None,
        ipv4: vec![],
        ipv6: vec![],
        rdma: None,
        gid_index: None,
    }
}

fn kind_rank(k: NicKind) -> u8 {
    match k {
        NicKind::Cx7 => 0,
        NicKind::Rdma => 1,
        NicKind::Ethernet => 2,
        NicKind::Wifi => 3,
        NicKind::Tailscale => 4,
        NicKind::Virtual => 5,
        NicKind::Loopback => 6,
    }
}

pub fn classify(n: &Nic) -> NicKind {
    let name = n.name.as_str();
    let driver = n.driver.as_deref().unwrap_or("");
    if name == "lo" {
        NicKind::Loopback
    } else if name.starts_with("tailscale") || n.ipv4.iter().any(|a| is_tailscale(&a.ip)) && driver.is_empty() {
        NicKind::Tailscale
    } else if ["docker", "br-", "veth", "virbr", "cni", "flannel", "vxlan", "tun", "tap"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        NicKind::Virtual
    } else if n.rdma.is_some() && driver.starts_with("mlx5") {
        NicKind::Cx7
    } else if n.rdma.is_some() {
        NicKind::Rdma
    } else if name.starts_with("wl") {
        NicKind::Wifi
    } else {
        NicKind::Ethernet
    }
}

/// 100.64.0.0/10, Tailscale's CGNAT range.
pub fn is_tailscale(ip: &str) -> bool {
    ip.parse::<Ipv4Addr>().is_ok_and(|a| a.octets()[0] == 100 && (a.octets()[1] & 0xc0) == 64)
}

/// The GID index whose GID is `::ffff:<ip>` and whose type is RoCE v2.
fn roce_v2_gid(p: &dyn Probe, dev: &str, ip: &str) -> Option<u32> {
    let want = ip.parse::<Ipv4Addr>().ok()?.octets();
    let tail = format!("ffff:{:02x}{:02x}:{:02x}{:02x}", want[0], want[1], want[2], want[3]);
    let gids = format!("/sys/class/infiniband/{dev}/ports/1/gids");
    let mut idx: Vec<u32> = p.list(&gids).iter().filter_map(|s| s.parse().ok()).collect();
    idx.sort_unstable();
    idx.into_iter().find(|i| {
        p.read(&format!("{gids}/{i}")).is_some_and(|g| g.to_ascii_lowercase().ends_with(&tail))
            && p.read(&format!("/sys/class/infiniband/{dev}/ports/1/gid_attrs/types/{i}"))
                .is_some_and(|t| t.contains("v2"))
    })
}

fn parse_lldp(text: &str) -> Vec<Neighbor> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return vec![] };
    let Some(ifaces) = v.pointer("/lldp/interface") else { return vec![] };
    let mut out = vec![];
    let mut one = |name: &str, body: &Value| {
        let chassis_obj = body.get("chassis").and_then(|c| c.as_object()).and_then(|m| m.iter().next());
        let chassis =
            chassis_obj.map(|(k, v)| v.pointer("/name/value").and_then(Value::as_str).unwrap_or(k).to_string());
        let bridge_cap = chassis_obj
            .and_then(|(_, v)| v.get("capability"))
            .map(|c| match c {
                Value::Array(a) => a.iter().any(|x| {
                    x.get("type").and_then(Value::as_str) == Some("Bridge")
                        && x.get("enabled").and_then(Value::as_bool) != Some(false)
                }),
                Value::Object(m) => m.get("type").and_then(Value::as_str) == Some("Bridge"),
                _ => false,
            })
            .unwrap_or(false);
        let name_looks = chassis.as_deref().is_some_and(|c| {
            let c = c.to_ascii_lowercase();
            ["switch", "tor", "leaf", "spine", "sn2100", "sn2200", "sn2700", "spectrum", "cumulus", "eos", "nxos"]
                .iter()
                .any(|k| c.contains(k))
        });
        let port = body
            .pointer("/port/id/value")
            .or_else(|| body.pointer("/port/descr/value"))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push(Neighbor { ifname: name.to_string(), chassis, port, is_bridge: bridge_cap || name_looks });
    };
    match ifaces {
        Value::Object(m) => m.iter().for_each(|(k, b)| one(k, b)),
        Value::Array(a) => {
            for it in a {
                if let Some(m) = it.as_object() {
                    m.iter().for_each(|(k, b)| one(k, b));
                }
            }
        }
        _ => {}
    }
    out
}

/// How two hosts are connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    /// A direct ConnectX-7 cable (RoCE): what NFS weights and NCCL want.
    Cx7,
    Lan,
    Tailscale,
}

impl LinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkKind::Cx7 => "cx7",
            LinkKind::Lan => "lan",
            LinkKind::Tailscale => "tailscale",
        }
    }

    /// The fast link the NFS and multi-node plans expect.
    pub fn is_direct(&self) -> bool {
        *self == LinkKind::Cx7
    }
}

/// One side of a link.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct End {
    pub host: String,
    pub ifname: Option<String>,
    pub ip: String,
    /// RDMA devices for NCCL (`NCCL_IB_HCA`): the port's own, plus twins on the same subnet.
    pub hcas: Vec<String>,
    pub gid_index: Option<u32>,
}

/// A link between two hosts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub a: End,
    pub b: End,
    pub kind: LinkKind,
    /// `10.10.1.0/30`.
    pub subnet: Option<String>,
    /// Slower side, Mb/s.
    pub speed_mbps: Option<u64>,
    /// From `[[link]]` in hosts.toml rather than detection.
    #[serde(default)]
    pub manual: bool,
    /// Whether `a` reached `b.ip` (ping / TCP), when checked.
    #[serde(default)]
    pub verified: Option<bool>,
    #[serde(default)]
    pub rtt_ms: Option<f64>,
}

impl Link {
    pub fn involves(&self, h: &str) -> bool {
        self.a.host == h || self.b.host == h
    }

    /// The link seen from `from`'s side: `(from end, other end)`.
    pub fn sides(&self, from: &str) -> Option<(&End, &End)> {
        if self.a.host == from {
            Some((&self.a, &self.b))
        } else if self.b.host == from {
            Some((&self.b, &self.a))
        } else {
            None
        }
    }

    pub fn gbps(&self) -> Option<f64> {
        self.speed_mbps.map(|m| m as f64 / 1000.0)
    }

    pub fn label(&self) -> String {
        let speed = self.gbps().map(|g| format!(" {g:.0}G")).unwrap_or_default();
        let ifs = match (&self.a.ifname, &self.b.ifname) {
            (Some(a), Some(b)) if a == b => a.clone(),
            (Some(a), Some(b)) => format!("{a}↔{b}"),
            _ => String::new(),
        };
        format!("{}{speed} {ifs}", self.kind.as_str()).trim().to_string()
    }
}

fn v4(ip: &str) -> Option<u32> {
    ip.parse::<Ipv4Addr>().ok().map(u32::from)
}

fn network(ip: u32, prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        ip & (u32::MAX << (32 - prefix.min(32) as u32))
    }
}

fn subnet_str(ip: u32, prefix: u8) -> String {
    format!("{}/{prefix}", Ipv4Addr::from(network(ip, prefix)))
}

/// A synthetic L2 switch / bridge inferred from a multi-member segment or LLDP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Switch {
    /// Stable id: `switch:<subnet>` (e.g. `switch:10.20.0.0/24`).
    pub id: String,
    /// LLDP chassis/sysName when known, else `cx7-switch 10.20.0.0/24` / `lan-switch …`.
    pub name: String,
    pub subnet: String,
    pub kind: LinkKind,
    pub ports: usize,
}

/// One host's membership on an L2 segment (all of its ports on that subnet collapsed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub host: String,
    /// Fastest / first port's interface.
    pub ifname: Option<String>,
    /// Address used for NFS / NCCL on this segment.
    pub ip: String,
    pub hcas: Vec<String>,
    pub gid_index: Option<u32>,
    pub speed_mbps: Option<u64>,
    /// How many ports this host has on the segment (rails).
    #[serde(default)]
    pub rails: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentClass {
    /// Exactly two hosts; drawn as a direct cable.
    Cable,
    /// Three or more hosts, or an LLDP bridge: drawn as a switch with star edges.
    Switch,
}

/// One L2 segment: a shared subnet (or LLDP bridge domain) across hosts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub subnet: String,
    pub class: SegmentClass,
    pub kind: LinkKind,
    pub members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<Switch>,
}

/// The cluster fabric as segments, switch nodes and drawable edges.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub segments: Vec<Segment>,
    pub switches: Vec<Switch>,
    /// Drawable edges: host↔host for cables, host↔switch for switch ports.
    /// Tailscale pairs are also edges. Activity and placement walk these.
    pub links: Vec<Link>,
}

impl Graph {
    /// Every host key that appears on a segment or edge.
    pub fn hosts(&self) -> Vec<String> {
        let mut h = std::collections::BTreeSet::new();
        for s in &self.segments {
            for m in &s.members {
                h.insert(m.host.clone());
            }
        }
        for l in &self.links {
            if !l.a.host.starts_with("switch:") {
                h.insert(l.a.host.clone());
            }
            if !l.b.host.starts_with("switch:") {
                h.insert(l.b.host.clone());
            }
        }
        h.into_iter().collect()
    }
}

/// Builds the L2 graph for `hosts`. `overrides` (hosts.toml `[[link]]`) replace
/// detection for their pair and are emitted as manual cable edges.
pub fn build_graph(hosts: &[(String, &Fabric)], overrides: &[LinkOverride]) -> Graph {
    let mut g = Graph::default();
    // Manual overrides first: each is a cable edge; those pairs skip auto-detect.
    let mut skip_pairs: std::collections::BTreeSet<(String, String)> = Default::default();
    for (i, (an, af)) in hosts.iter().enumerate() {
        for (bn, bf) in hosts.iter().skip(i + 1) {
            let ovr: Vec<&LinkOverride> =
                overrides.iter().filter(|o| (&o.a == an && &o.b == bn) || (&o.a == bn && &o.b == an)).collect();
            if ovr.is_empty() {
                continue;
            }
            let mut pair = [an.clone(), bn.clone()];
            pair.sort();
            skip_pairs.insert((pair[0].clone(), pair[1].clone()));
            for o in ovr {
                let sa = (o.a_addr.clone(), o.a_ifname.clone(), o.a_hca.clone(), o.a_gid_index);
                let sb = (o.b_addr.clone(), o.b_ifname.clone(), o.b_hca.clone(), o.b_gid_index);
                let (a, b) = if &o.a == an { (sa, sb) } else { (sb, sa) };
                let kind = match o.kind.as_deref() {
                    Some("lan") => LinkKind::Lan,
                    Some("tailscale") => LinkKind::Tailscale,
                    _ => LinkKind::Cx7,
                };
                type Side = (String, Option<String>, Option<String>, Option<u32>);
                let end = |host: &str, (ip, ifname, hca, gid): Side, f: &Fabric| {
                    let nic = ifname.as_deref().and_then(|n| f.nic(n));
                    End {
                        host: host.to_string(),
                        ifname,
                        ip,
                        hcas: hca.or_else(|| nic.and_then(|n| n.rdma.clone())).into_iter().collect(),
                        gid_index: gid.or_else(|| nic.and_then(|n| n.gid_index)),
                    }
                };
                g.links.push(Link {
                    a: end(an, a, af),
                    b: end(bn, b, bf),
                    kind,
                    subnet: None,
                    speed_mbps: o.gbps.map(|g| (g * 1000.0) as u64),
                    manual: true,
                    verified: None,
                    rtt_ms: None,
                });
            }
        }
    }
    // Collect every usable IPv4 on every host, keyed by (network, prefix).
    #[derive(Clone)]
    struct Port {
        host: String,
        nic: Nic,
        ip: String,
        prefix: u8,
        net: u32,
    }
    let mut ports: Vec<Port> = vec![];
    for (name, f) in hosts {
        for n in f.nics.iter().filter(|n| usable(n)) {
            if n.kind == NicKind::Tailscale {
                continue;
            }
            for a in &n.ipv4 {
                let Some(ip) = v4(&a.ip) else { continue };
                if !(8..=31).contains(&a.prefix) || (ip >> 16) == 0xa9fe {
                    continue;
                }
                ports.push(Port {
                    host: name.clone(),
                    nic: n.clone(),
                    ip: a.ip.clone(),
                    prefix: a.prefix,
                    net: network(ip, a.prefix),
                });
            }
        }
    }
    // Group by (net, prefix). Collapse multiple ports of the same host.
    let mut groups: BTreeMap<(u32, u8), Vec<Port>> = BTreeMap::new();
    for p in ports {
        groups.entry((p.net, p.prefix)).or_default().push(p);
    }
    for ((net, prefix), group) in groups {
        let subnet = subnet_str(net, prefix);
        // Unique hosts on this subnet.
        let mut by_host: BTreeMap<String, Vec<Port>> = BTreeMap::new();
        for p in group {
            by_host.entry(p.host.clone()).or_default().push(p);
        }
        if by_host.len() < 2 {
            continue;
        }
        // Skip a pair that already has a manual override.
        if by_host.len() == 2 {
            let mut names: Vec<String> = by_host.keys().cloned().collect();
            names.sort();
            if skip_pairs.contains(&(names[0].clone(), names[1].clone())) {
                continue;
            }
        }
        let mut members: Vec<Member> = vec![];
        let mut any_cx7 = true;
        let mut lldp_bridge: Option<String> = None;
        for (host, ps) in &by_host {
            // Prefer the fastest CX7 port as the primary.
            let mut ordered = ps.clone();
            ordered.sort_by_key(|p| {
                (
                    if p.nic.kind == NicKind::Cx7 { 0u8 } else { 1 },
                    std::cmp::Reverse(p.nic.speed_mbps.unwrap_or(0)),
                    p.nic.name.to_ascii_lowercase(),
                )
            });
            let primary = &ordered[0];
            if primary.nic.kind != NicKind::Cx7 {
                any_cx7 = false;
            }
            let mut hcas = vec![];
            for p in &ordered {
                if let Some(h) = &p.nic.rdma {
                    if !hcas.contains(h) {
                        hcas.push(h.clone());
                    }
                }
            }
            let speed = ordered.iter().filter_map(|p| p.nic.speed_mbps).max();
            members.push(Member {
                host: host.clone(),
                ifname: Some(primary.nic.name.clone()),
                ip: primary.ip.clone(),
                hcas,
                gid_index: primary.nic.gid_index,
                speed_mbps: speed,
                rails: ordered.len(),
            });
            // LLDP bridge on any of this host's ports in the group?
            if let Some((_, f)) = hosts.iter().find(|(n, _)| n == host) {
                for p in &ordered {
                    for nb in f.lldp.iter().filter(|n| n.ifname == p.nic.name && n.is_bridge) {
                        if lldp_bridge.is_none() {
                            lldp_bridge = nb.chassis.clone();
                        }
                    }
                }
            }
        }
        members.sort_by(|a, b| a.host.cmp(&b.host));
        let kind = if any_cx7
            && members.iter().all(|m| {
                hosts
                    .iter()
                    .find(|(n, _)| n == &m.host)
                    .and_then(|(_, f)| m.ifname.as_ref().and_then(|i| f.nic(i)).map(|n| n.kind == NicKind::Cx7))
                    .unwrap_or(false)
            }) {
            LinkKind::Cx7
        } else {
            LinkKind::Lan
        };
        // Classification: 2 hosts + (/30|/31 or no LLDP bridge) → cable; else switch.
        let is_p2p_prefix = prefix >= 30;
        let class = if members.len() == 2 && (is_p2p_prefix || lldp_bridge.is_none()) {
            SegmentClass::Cable
        } else {
            SegmentClass::Switch
        };
        let switch = if class == SegmentClass::Switch {
            let id = format!("switch:{subnet}");
            let name = lldp_bridge.clone().unwrap_or_else(|| {
                let tag = if kind == LinkKind::Cx7 { "cx7-switch" } else { "lan-switch" };
                format!("{tag} {subnet}")
            });
            Some(Switch { id, name, subnet: subnet.clone(), kind, ports: members.len() })
        } else {
            None
        };
        if let Some(sw) = &switch {
            g.switches.push(sw.clone());
            for m in &members {
                let speed = m.speed_mbps;
                g.links.push(Link {
                    a: End {
                        host: m.host.clone(),
                        ifname: m.ifname.clone(),
                        ip: m.ip.clone(),
                        hcas: m.hcas.clone(),
                        gid_index: m.gid_index,
                    },
                    b: End {
                        host: sw.id.clone(),
                        ifname: None,
                        ip: subnet.split('/').next().unwrap_or(&subnet).to_string(),
                        hcas: vec![],
                        gid_index: None,
                    },
                    kind,
                    subnet: Some(subnet.clone()),
                    speed_mbps: speed,
                    manual: false,
                    verified: None,
                    rtt_ms: None,
                });
            }
        } else if members.len() == 2 {
            let (a, b) = (&members[0], &members[1]);
            let speed = match (a.speed_mbps, b.speed_mbps) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            };
            g.links.push(Link {
                a: End {
                    host: a.host.clone(),
                    ifname: a.ifname.clone(),
                    ip: a.ip.clone(),
                    hcas: a.hcas.clone(),
                    gid_index: a.gid_index,
                },
                b: End {
                    host: b.host.clone(),
                    ifname: b.ifname.clone(),
                    ip: b.ip.clone(),
                    hcas: b.hcas.clone(),
                    gid_index: b.gid_index,
                },
                kind,
                subnet: Some(subnet.clone()),
                speed_mbps: speed,
                manual: false,
                verified: None,
                rtt_ms: None,
            });
        }
        g.segments.push(Segment { subnet, class, kind, members, switch });
    }
    // Tailscale: one edge per pair that both have a Tailscale address.
    for (i, (an, af)) in hosts.iter().enumerate() {
        for (bn, bf) in hosts.iter().skip(i + 1) {
            let mut pair = [an.clone(), bn.clone()];
            pair.sort();
            if skip_pairs.contains(&(pair[0].clone(), pair[1].clone())) {
                continue;
            }
            let ta = af.nics.iter().find(|n| n.kind == NicKind::Tailscale && !n.ipv4.is_empty());
            let tb = bf.nics.iter().find(|n| n.kind == NicKind::Tailscale && !n.ipv4.is_empty());
            if let (Some(na), Some(nb)) = (ta, tb) {
                g.links.push(link(an, na, &na.ipv4[0].ip, bn, nb, &nb.ipv4[0].ip, LinkKind::Tailscale, None));
            }
        }
    }
    g.links.sort_by_key(|p| (p.kind, std::cmp::Reverse(p.speed_mbps), p.a.host.clone(), p.b.host.clone()));
    g.switches.sort_by(|a, b| a.id.cmp(&b.id));
    g.segments.sort_by(|a, b| a.subnet.cmp(&b.subnet));
    g
}

/// Back-compat: drawable + placement edges from [`build_graph`].
pub fn match_links(hosts: &[(String, &Fabric)], overrides: &[LinkOverride]) -> Vec<Link> {
    build_graph(hosts, overrides).links
}

fn usable(n: &Nic) -> bool {
    !matches!(n.kind, NicKind::Virtual | NicKind::Loopback) && n.state != "down"
}

/// Ends of the best path between two *hosts* (never switch ids): prefers a
/// shared segment (cable or through one switch), then the fastest edge path.
/// Returns `(end_at_a, end_at_b, kind, speed)`.
pub fn path_between(graph: &Graph, a: &str, b: &str) -> Option<(End, End, LinkKind, Option<u64>)> {
    if a == b {
        return None;
    }
    // Same segment: use each host's Member (works for cable and switch).
    let mut best: Option<(End, End, LinkKind, Option<u64>)> = None;
    for seg in &graph.segments {
        let ma = seg.members.iter().find(|m| m.host == a);
        let mb = seg.members.iter().find(|m| m.host == b);
        if let (Some(ma), Some(mb)) = (ma, mb) {
            let speed = match (ma.speed_mbps, mb.speed_mbps) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            };
            let cand = (
                End {
                    host: ma.host.clone(),
                    ifname: ma.ifname.clone(),
                    ip: ma.ip.clone(),
                    hcas: ma.hcas.clone(),
                    gid_index: ma.gid_index,
                },
                End {
                    host: mb.host.clone(),
                    ifname: mb.ifname.clone(),
                    ip: mb.ip.clone(),
                    hcas: mb.hcas.clone(),
                    gid_index: mb.gid_index,
                },
                seg.kind,
                speed,
            );
            let better = match &best {
                None => true,
                Some((_, _, k, sp)) => (seg.kind, std::cmp::Reverse(speed)) < (*k, std::cmp::Reverse(*sp)),
            };
            if better {
                best = Some(cand);
            }
        }
    }
    if best.is_some() {
        return best;
    }
    // Manual / tailscale cable edge directly between them.
    let edge = graph
        .links
        .iter()
        .filter(|l| {
            l.involves(a) && l.involves(b) && !l.a.host.starts_with("switch:") && !l.b.host.starts_with("switch:")
        })
        .min_by_key(|l| (l.kind, std::cmp::Reverse(l.speed_mbps)));
    if let Some(l) = edge {
        let (me, peer) = l.sides(a)?;
        return Some((me.clone(), peer.clone(), l.kind, l.speed_mbps));
    }
    None
}

/// A host↔host [`Link`] synthesised from [`path_between`] (cable or through a switch).
pub fn path_as_link(graph: &Graph, a: &str, b: &str) -> Option<Link> {
    let (ea, eb, kind, speed) = path_between(graph, a, b)?;
    Some(Link { a: ea, b: eb, kind, subnet: None, speed_mbps: speed, manual: false, verified: None, rtt_ms: None })
}

#[allow(clippy::too_many_arguments)]
fn link(an: &str, na: &Nic, aip: &str, bn: &str, nb: &Nic, bip: &str, kind: LinkKind, subnet: Option<String>) -> Link {
    let end = |host: &str, n: &Nic, ip: &str| End {
        host: host.to_string(),
        ifname: Some(n.name.clone()),
        ip: ip.to_string(),
        hcas: n.rdma.clone().into_iter().collect(),
        gid_index: n.gid_index,
    };
    let speed = match (na.speed_mbps, nb.speed_mbps) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    };
    Link {
        a: end(an, na, aip),
        b: end(bn, nb, bip),
        kind,
        subnet,
        speed_mbps: speed,
        manual: false,
        verified: None,
        rtt_ms: None,
    }
}

/// The best link between `a` and `b` (direct first, then LAN, then Tailscale;
/// a link that failed verification comes last).
pub fn best_link<'a>(links: &'a [Link], a: &str, b: &str) -> Option<&'a Link> {
    // Only host↔host edges (cables / overrides / tailscale). Paths through a
    // switch use [`path_between`] on a [`Graph`].
    links
        .iter()
        .filter(|l| l.involves(a) && l.involves(b))
        .filter(|l| !l.a.host.starts_with("switch:") && !l.b.host.starts_with("switch:"))
        .min_by_key(|l| (l.verified == Some(false), !l.manual, l.kind, std::cmp::Reverse(l.speed_mbps)))
}

/// Whether this machine reaches `addr`: one ICMP echo (`ping` needs no root
/// on Linux), else a TCP connect to 22 or 2049 where "refused" also proves
/// the host is there.
pub fn reach(addr: &str) -> (bool, Option<f64>) {
    if addr.parse::<std::net::IpAddr>().is_err() {
        return (false, None);
    }
    if let Some(out) = run_with_timeout(Command::new("ping").args(["-c", "1", "-W", "1", addr]), Duration::from_secs(3))
    {
        let rtt = out.split("time=").nth(1).and_then(|s| s.split_whitespace().next()).and_then(|v| v.parse().ok());
        return (true, rtt);
    }
    for port in [22u16, 2049] {
        let sa = match format!("{addr}:{port}").parse::<std::net::SocketAddr>() {
            Ok(s) => s,
            Err(_) => format!("[{addr}]:{port}").parse().expect("ipv6 socket addr"),
        };
        let t0 = std::time::Instant::now();
        match std::net::TcpStream::connect_timeout(&sa, Duration::from_millis(800)) {
            Ok(_) => return (true, Some(t0.elapsed().as_secs_f64() * 1000.0)),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                return (true, Some(t0.elapsed().as_secs_f64() * 1000.0))
            }
            Err(_) => {}
        }
    }
    (false, None)
}

/// Built-in fixtures (also used by `LMDECK_DEMO=1`): constructed in the
/// format DGX Spark's `ip -j addr` and sysfs produce, not captured from hardware.
pub mod fixtures {
    pub const TRI: [(&str, &str); 3] = [
        ("spark1", include_str!("../tests/fixtures/fabric/tri-spark1.json")),
        ("spark2", include_str!("../tests/fixtures/fabric/tri-spark2.json")),
        ("spark3", include_str!("../tests/fixtures/fabric/tri-spark3.json")),
    ];
    pub const DUO: [(&str, &str); 2] = [
        ("spark1", include_str!("../tests/fixtures/fabric/duo-spark1.json")),
        ("spark2", include_str!("../tests/fixtures/fabric/duo-spark2.json")),
    ];
    pub const SWITCH8: [(&str, &str); 8] = [
        ("spark1", include_str!("../tests/fixtures/fabric/sw8-spark1.json")),
        ("spark2", include_str!("../tests/fixtures/fabric/sw8-spark2.json")),
        ("spark3", include_str!("../tests/fixtures/fabric/sw8-spark3.json")),
        ("spark4", include_str!("../tests/fixtures/fabric/sw8-spark4.json")),
        ("spark5", include_str!("../tests/fixtures/fabric/sw8-spark5.json")),
        ("spark6", include_str!("../tests/fixtures/fabric/sw8-spark6.json")),
        ("spark7", include_str!("../tests/fixtures/fabric/sw8-spark7.json")),
        ("spark8", include_str!("../tests/fixtures/fabric/sw8-spark8.json")),
    ];

    pub fn load(text: &str) -> super::Fabric {
        let f: super::Fixture = serde_json::from_str(text).expect("valid fixture");
        super::detect_with(&f)
    }

    pub fn set(items: &[(&str, &str)]) -> Vec<(String, super::Fabric)> {
        items.iter().map(|(n, t)| ((*n).to_string(), load(t))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(name: &str) -> Fabric {
        let text = std::fs::read_to_string(format!("{}/tests/fixtures/fabric/{name}.json", env!("CARGO_MANIFEST_DIR")))
            .unwrap();
        fixtures::load(&text)
    }

    fn refs(f: &[(String, Fabric)]) -> Vec<(String, &Fabric)> {
        f.iter().map(|(n, x)| (n.clone(), x)).collect()
    }

    #[test]
    fn detects_cx7_ports_rdma_gids_and_device() {
        let f = fx("duo-spark1");
        assert_eq!(f.hostname, "spark-01.lab");
        assert_eq!(f.product.as_deref(), Some("DGX Spark"));
        assert_eq!(f.device_kind, "spark");
        let cx = f.nic("enp1s0f1np1").unwrap();
        assert_eq!(cx.kind, NicKind::Cx7);
        assert_eq!(cx.driver.as_deref(), Some("mlx5_core"));
        assert_eq!(cx.rdma.as_deref(), Some("rocep1s0f1"));
        assert_eq!(cx.speed_mbps, Some(200_000));
        assert_eq!(cx.gid_index, Some(3));
        assert_eq!(cx.ipv4, vec![Addr { ip: "192.168.100.10".into(), prefix: 24 }]);
        assert_eq!(f.nic("enP7s7").unwrap().kind, NicKind::Ethernet);
        assert_eq!(f.nic("tailscale0").unwrap().kind, NicKind::Tailscale);
        assert_eq!(f.nic("docker0").unwrap().kind, NicKind::Virtual);
        assert_eq!(f.nic("enp1s0f0np0").unwrap().state, "down");
        assert!(f.tools.ip && !f.tools.lldpctl && f.tools.nvidia_smi);
        assert_eq!(f.gpus[0].name, "NVIDIA GB10");
        assert_eq!(f.nics[0].kind, NicKind::Cx7, "CX7 ports sort first");
    }

    #[test]
    fn two_sparks_one_cable_with_dual_rails() {
        let f = fixtures::set(&fixtures::DUO);
        let g = build_graph(&refs(&f), &[]);
        let cx7_segs: Vec<&Segment> = g.segments.iter().filter(|s| s.kind == LinkKind::Cx7).collect();
        assert_eq!(cx7_segs.len(), 1, "{:#?}", g.segments);
        assert_eq!(cx7_segs[0].class, SegmentClass::Cable);
        assert!(g.switches.is_empty(), "two hosts on /24 without LLDP bridge → cable, not switch");
        let best = best_link(&g.links, "spark2", "spark1").unwrap();
        assert_eq!(best.kind, LinkKind::Cx7);
        assert_eq!(best.subnet.as_deref(), Some("192.168.100.0/24"));
        let (me, peer) = best.sides("spark2").unwrap();
        assert_eq!((me.ip.as_str(), peer.ip.as_str()), ("192.168.100.11", "192.168.100.10"));
        assert_eq!(me.ifname.as_deref(), Some("enp1s0f1np1"));
        assert_eq!(me.hcas, vec!["rocep1s0f1".to_string(), "roceP2p1s0f1".to_string()], "both rails");
        assert_eq!(me.gid_index, Some(3));
        assert_eq!(best.gbps(), Some(200.0));
        let kinds: Vec<LinkKind> = g.links.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LinkKind::Lan) && kinds.contains(&LinkKind::Tailscale), "{kinds:?}");
        assert!(!g.links.iter().any(|l| l.a.ifname.as_deref() == Some("docker0")));
        let (a, b, k, _) = path_between(&g, "spark1", "spark2").unwrap();
        assert_eq!(k, LinkKind::Cx7);
        assert_eq!(a.ip, "192.168.100.10");
        assert_eq!(b.ip, "192.168.100.11");
    }

    #[test]
    fn point_to_point_slash30s_are_cables_not_a_mesh() {
        let f = fixtures::set(&fixtures::TRI);
        let g = build_graph(&refs(&f), &[]);
        let cables: Vec<&Segment> =
            g.segments.iter().filter(|s| s.class == SegmentClass::Cable && s.kind == LinkKind::Cx7).collect();
        assert_eq!(cables.len(), 3, "{:#?}", g.segments);
        assert!(g.switches.iter().all(|s| s.kind != LinkKind::Cx7), "no CX7 switch in a triangle of /30s");
        let links = &g.links;
        let pair = |a: &str, b: &str| best_link(links, a, b).unwrap();
        assert_eq!(pair("spark1", "spark2").subnet.as_deref(), Some("10.10.12.0/30"));
        assert_eq!(pair("spark1", "spark3").subnet.as_deref(), Some("10.10.13.0/30"));
        assert_eq!(pair("spark2", "spark3").subnet.as_deref(), Some("10.10.23.0/30"));
        let (s3, s1) = pair("spark1", "spark3").sides("spark3").unwrap();
        assert_eq!((s3.ip.as_str(), s1.ip.as_str()), ("10.10.13.2", "10.10.13.1"));
        assert_eq!(s3.ifname.as_deref(), Some("enp1s0f0np0"));
        // No phantom host↔host CX7 edge that isn't a cable segment.
        let cx7_host_edges = links
            .iter()
            .filter(|l| l.kind == LinkKind::Cx7 && !l.a.host.starts_with("switch:") && !l.b.host.starts_with("switch:"))
            .count();
        assert_eq!(cx7_host_edges, 3);
    }

    #[test]
    fn eight_sparks_on_one_subnet_become_one_switch() {
        let f = fixtures::set(&fixtures::SWITCH8);
        let g = build_graph(&refs(&f), &[]);
        let sw: Vec<&Switch> = g.switches.iter().filter(|s| s.kind == LinkKind::Cx7).collect();
        assert_eq!(sw.len(), 1, "{:#?}", g.switches);
        assert_eq!(sw[0].ports, 8);
        assert_eq!(sw[0].name, "SN2100-TOR", "LLDP chassis wins");
        assert_eq!(sw[0].subnet, "10.20.0.0/24");
        // Star edges only: 8 host↔switch, no C(8,2)=28 host↔host CX7 edges.
        let star = g
            .links
            .iter()
            .filter(|l| l.kind == LinkKind::Cx7 && (l.a.host.starts_with("switch:") || l.b.host.starts_with("switch:")))
            .count();
        assert_eq!(star, 8, "{:#?}", g.links);
        let phantom = g
            .links
            .iter()
            .filter(|l| l.kind == LinkKind::Cx7 && !l.a.host.starts_with("switch:") && !l.b.host.starts_with("switch:"))
            .count();
        assert_eq!(phantom, 0, "no pairwise CX7 edges through the switch");
        // Placement still finds addr/ifname/hca between any pair via the segment.
        let (a, b, k, sp) = path_between(&g, "spark1", "spark8").unwrap();
        assert_eq!(k, LinkKind::Cx7);
        assert_eq!(sp, Some(200_000));
        assert_eq!(a.ip, "10.20.0.11");
        assert_eq!(b.ip, "10.20.0.18");
        assert_eq!(a.ifname.as_deref(), Some("enp1s0f0np0"));
        assert!(!a.hcas.is_empty());
        assert_eq!(g.hosts().len(), 8);
        assert!(fx("sw8-spark1").tools.lldpctl);
        assert!(fx("sw8-spark1").lldp.iter().any(|n| n.is_bridge && n.chassis.as_deref() == Some("SN2100-TOR")));
    }

    #[test]
    fn triangle_plus_switch_segment_mixed() {
        let mut f = fixtures::set(&fixtures::TRI);
        f.push(("spark4".into(), fx("mix-spark4")));
        // Replace spark1 with the mix fixture that also joins the switch subnet.
        f[0] = ("spark1".into(), fx("mix-spark1"));
        let g = build_graph(&refs(&f), &[]);
        let cables = g.segments.iter().filter(|s| s.class == SegmentClass::Cable && s.kind == LinkKind::Cx7).count();
        let sw = g.segments.iter().filter(|s| s.class == SegmentClass::Switch && s.kind == LinkKind::Cx7).count();
        assert_eq!(cables, 3, "triangle /30s stay cables: {:#?}", g.segments);
        assert_eq!(sw, 1, "10.30.0.0/24 is a switch: {:#?}", g.segments);
        let leaf = g.switches.iter().find(|s| s.subnet == "10.30.0.0/24").unwrap();
        assert_eq!(leaf.name, "leaf-a");
        assert_eq!(leaf.ports, 2);
        let (a, b, k, _) = path_between(&g, "spark1", "spark4").unwrap();
        assert_eq!(k, LinkKind::Cx7);
        assert!(a.ip.starts_with("10.30.0."));
        assert!(b.ip.starts_with("10.30.0."));
        // Triangle path still works.
        let (_, _, k, _) = path_between(&g, "spark2", "spark3").unwrap();
        assert_eq!(k, LinkKind::Cx7);
    }

    #[test]
    fn pc_with_two_gpus_and_no_rdma() {
        let f = fx("pc-2x-rtx");
        assert_eq!(f.gpus.len(), 2);
        assert_eq!(f.gpus[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(f.device_kind, "pc");
        assert_eq!(f.product.as_deref(), Some("Precision 7960"));
        assert!(!f.has_cx7());
    }

    #[test]
    fn no_rdma_falls_back_to_lan_and_says_why() {
        let (a, b) = (fx("duo-spark1"), fx("nordma"));
        assert!(!b.has_cx7());
        assert!(b.notes.iter().any(|n| n.contains("no RDMA")), "{:?}", b.notes);
        assert!(!b.tools.ip, "the fixture has no iproute2");
        let links = match_links(&[("spark1".into(), &a), ("box".into(), &b)], &[]);
        let best = best_link(&links, "spark1", "box");
        assert!(best.is_none(), "no addresses without `ip`: no link can be matched");
        let lan = fx("nordma-ip");
        let g = build_graph(&[("spark1".into(), &a), ("box".into(), &lan)], &[]);
        let best = best_link(&g.links, "spark1", "box").unwrap();
        assert_eq!(best.kind, LinkKind::Lan);
        assert!(!best.kind.is_direct());
    }

    #[test]
    fn overrides_win() {
        let (a, b) = (fx("duo-spark1"), fx("duo-spark2"));
        let o = LinkOverride {
            a: "spark2".into(),
            b: "spark1".into(),
            a_addr: "10.0.0.2".into(),
            b_addr: "10.0.0.1".into(),
            a_ifname: Some("enp1s0f0np0".into()),
            gbps: Some(100.0),
            ..Default::default()
        };
        let g = build_graph(&[("spark1".into(), &a), ("spark2".into(), &b)], &[o]);
        let manuals: Vec<&Link> = g.links.iter().filter(|l| l.manual).collect();
        assert_eq!(manuals.len(), 1);
        let (s2, s1) = manuals[0].sides("spark2").unwrap();
        assert_eq!((s2.ip.as_str(), s1.ip.as_str()), ("10.0.0.2", "10.0.0.1"));
        assert_eq!(s2.hcas, vec!["rocep1s0f0".to_string()]);
        assert!(manuals[0].manual && manuals[0].kind == LinkKind::Cx7 && manuals[0].gbps() == Some(100.0));
        // Auto CX7 segment between them is skipped when an override exists.
        assert!(!g.segments.iter().any(|s| s.kind == LinkKind::Cx7 && s.members.len() == 2));
    }

    #[test]
    fn tailscale_range_and_subnet_str() {
        assert!(is_tailscale("100.64.0.1") && is_tailscale("100.127.255.1"));
        assert!(!is_tailscale("100.128.0.1") && !is_tailscale("10.0.0.1"));
        assert_eq!(subnet_str(v4("10.10.12.2").unwrap(), 30), "10.10.12.0/30");
    }

    /// Generates arbitrary host/segment topologies and checks classification invariants.
    #[test]
    fn random_topologies_obey_segment_invariants() {
        let mut seed = 0xC0FFEE_u64;
        let mut rng = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed
        };
        for case in 0..40 {
            let n_hosts = (rng() % 16) as usize + 1; // 1..=16
            let mut fabrics: Vec<(String, Fabric)> = (0..n_hosts)
                .map(|i| {
                    let mut f = Fabric {
                        hostname: format!("host-{i}.lab"),
                        device_kind: "spark".into(),
                        product: Some("DGX Spark".into()),
                        ..Default::default()
                    };
                    f.nics.push(Nic {
                        name: "eth0".into(),
                        kind: NicKind::Ethernet,
                        state: "up".into(),
                        ipv4: vec![Addr { ip: format!("192.168.9.{}", 10 + i), prefix: 24 }],
                        ..blank_nic("eth0")
                    });
                    (format!("h{i}"), f)
                })
                .collect();
            // Sprinkle CX7 segments: some /30 cables, some multi-member switch subnets.
            let n_seg = (rng() % 5) as usize;
            for s in 0..n_seg {
                let members = (rng() % n_hosts as u64) as usize + 1;
                let members = members.min(n_hosts).max(1);
                let use_switch = members >= 3 || (rng() % 3 == 0 && members >= 2);
                let prefix = if use_switch && members >= 3 {
                    24
                } else if members == 2 {
                    30
                } else {
                    24
                };
                let base = 10 * 256 * 256 + (s as u32 + 1) * 256;
                let mut idxs: Vec<usize> = (0..n_hosts).collect();
                // shuffle
                for i in (1..idxs.len()).rev() {
                    let j = (rng() as usize) % (i + 1);
                    idxs.swap(i, j);
                }
                let bridge = use_switch && members >= 3;
                for (k, &hi) in idxs.iter().take(members).enumerate() {
                    let ip = Ipv4Addr::from(base + 10 + k as u32).to_string();
                    let ifname = format!("cx7s{s}");
                    fabrics[hi].1.nics.push(Nic {
                        name: ifname.clone(),
                        kind: NicKind::Cx7,
                        state: "up".into(),
                        driver: Some("mlx5_core".into()),
                        rdma: Some(format!("roce{s}{k}")),
                        speed_mbps: Some(200_000),
                        gid_index: Some(3),
                        ipv4: vec![Addr { ip, prefix }],
                        ..blank_nic(&ifname)
                    });
                    if bridge {
                        fabrics[hi].1.lldp.push(Neighbor {
                            ifname,
                            chassis: Some(format!("tor-{s}")),
                            port: Some(format!("Eth1/{k}")),
                            is_bridge: true,
                        });
                        fabrics[hi].1.tools.lldpctl = true;
                    }
                }
            }
            let g = build_graph(&refs(&fabrics), &[]);
            // Invariants.
            for seg in &g.segments {
                match seg.class {
                    SegmentClass::Cable => {
                        assert_eq!(seg.members.len(), 2, "case {case}: cable members");
                        assert!(seg.switch.is_none());
                    }
                    SegmentClass::Switch => {
                        assert!(seg.members.len() >= 2, "case {case}: switch members");
                        assert!(seg.switch.is_some());
                        assert_eq!(seg.switch.as_ref().unwrap().ports, seg.members.len());
                    }
                }
                // Every member host exists.
                for m in &seg.members {
                    assert!(fabrics.iter().any(|(n, _)| n == &m.host), "case {case}: unknown {}", m.host);
                }
            }
            // No phantom host↔host edges on a switch segment's subnet.
            for seg in g.segments.iter().filter(|s| s.class == SegmentClass::Switch) {
                let phantoms = g
                    .links
                    .iter()
                    .filter(|l| {
                        l.subnet.as_deref() == Some(seg.subnet.as_str())
                            && !l.a.host.starts_with("switch:")
                            && !l.b.host.starts_with("switch:")
                    })
                    .count();
                assert_eq!(phantoms, 0, "case {case}: phantom edges on {}", seg.subnet);
                let stars = g
                    .links
                    .iter()
                    .filter(|l| {
                        l.subnet.as_deref() == Some(seg.subnet.as_str())
                            && (l.a.host.starts_with("switch:") || l.b.host.starts_with("switch:"))
                    })
                    .count();
                assert_eq!(stars, seg.members.len(), "case {case}: star count");
            }
            // path_between works for every co-segmented pair.
            for seg in &g.segments {
                for i in 0..seg.members.len() {
                    for j in i + 1..seg.members.len() {
                        let p = path_between(&g, &seg.members[i].host, &seg.members[j].host);
                        assert!(p.is_some(), "case {case}: path {} ↔ {}", seg.members[i].host, seg.members[j].host);
                    }
                }
            }
            let _ = case;
        }
    }
}
