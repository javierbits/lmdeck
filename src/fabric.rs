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
//! The controller then matches peers ([`match_links`]): two addresses in the
//! same small subnet (/24 or narrower) on CX7 ports (mlx5 + RDMA) are a direct
//! cable. A 3-Spark triangle has one subnet per cable (Mia's `start-tp3.sh`),
//! so it comes out as three links. Shared LAN subnets and Tailscale are the
//! slower fallback. `[[link]]` entries in hosts.toml override detection.

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Neighbor {
    pub ifname: String,
    pub chassis: Option<String>,
    pub port: Option<String>,
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
    f
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
        let chassis = body.get("chassis").and_then(|c| c.as_object()).and_then(|m| {
            m.iter().next().map(|(k, v)| v.pointer("/name/value").and_then(Value::as_str).unwrap_or(k).to_string())
        });
        let port = body
            .pointer("/port/id/value")
            .or_else(|| body.pointer("/port/descr/value"))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push(Neighbor { ifname: name.to_string(), chassis, port });
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

/// Finds every link between the given hosts' fabrics, best first per pair.
/// `overrides` (hosts.toml `[[link]]`) replace detection for their pair.
pub fn match_links(hosts: &[(String, &Fabric)], overrides: &[LinkOverride]) -> Vec<Link> {
    let mut out: Vec<Link> = vec![];
    for (i, (an, af)) in hosts.iter().enumerate() {
        for (bn, bf) in hosts.iter().skip(i + 1) {
            let ovr: Vec<&LinkOverride> =
                overrides.iter().filter(|o| (&o.a == an && &o.b == bn) || (&o.a == bn && &o.b == an)).collect();
            if !ovr.is_empty() {
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
                    out.push(Link {
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
                continue;
            }
            out.extend(pair_links(an, af, bn, bf));
        }
    }
    out
}

fn usable(n: &Nic) -> bool {
    !matches!(n.kind, NicKind::Virtual | NicKind::Loopback) && n.state != "down"
}

fn pair_links(an: &str, af: &Fabric, bn: &str, bf: &Fabric) -> Vec<Link> {
    let mut found: Vec<Link> = vec![];
    for na in af.nics.iter().filter(|n| usable(n)) {
        for nb in bf.nics.iter().filter(|n| usable(n)) {
            if na.kind == NicKind::Tailscale && nb.kind == NicKind::Tailscale {
                if let (Some(x), Some(y)) = (na.ipv4.first(), nb.ipv4.first()) {
                    if !found.iter().any(|l| l.kind == LinkKind::Tailscale) {
                        found.push(link(an, na, &x.ip, bn, nb, &y.ip, LinkKind::Tailscale, None));
                    }
                }
                continue;
            }
            if na.kind == NicKind::Tailscale || nb.kind == NicKind::Tailscale {
                continue;
            }
            for x in &na.ipv4 {
                for y in &nb.ipv4 {
                    let (Some(xi), Some(yi)) = (v4(&x.ip), v4(&y.ip)) else { continue };
                    let prefix = x.prefix.min(y.prefix);
                    if xi == yi || !(8..=31).contains(&prefix) || network(xi, prefix) != network(yi, prefix) {
                        continue;
                    }
                    if (xi >> 16) == 0xa9fe {
                        continue; // 169.254/16 link-local
                    }
                    let direct = na.kind == NicKind::Cx7 && nb.kind == NicKind::Cx7 && prefix >= 24;
                    let kind = if direct { LinkKind::Cx7 } else { LinkKind::Lan };
                    let sub = subnet_str(xi, prefix);
                    if let Some(l) = found.iter_mut().find(|l| l.subnet.as_deref() == Some(sub.as_str())) {
                        // A second port pair on the same subnet: another NCCL rail.
                        for (end, nic) in [(&mut l.a, na), (&mut l.b, nb)] {
                            if let Some(h) = &nic.rdma {
                                if !end.hcas.contains(h) {
                                    end.hcas.push(h.clone());
                                }
                            }
                        }
                        continue;
                    }
                    found.push(link(an, na, &x.ip, bn, nb, &y.ip, kind, Some(sub)));
                }
            }
        }
    }
    found.sort_by_key(|p| (p.kind, std::cmp::Reverse(p.speed_mbps)));
    found
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
    links
        .iter()
        .filter(|l| l.involves(a) && l.involves(b))
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

    pub fn load(text: &str) -> super::Fabric {
        let f: super::Fixture = serde_json::from_str(text).expect("valid fixture");
        super::detect_with(&f)
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

    #[test]
    fn detects_cx7_ports_rdma_and_gids() {
        let f = fx("duo-spark1");
        assert_eq!(f.hostname, "spark1");
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
    fn two_sparks_one_cable() {
        let (a, b) = (fx("duo-spark1"), fx("duo-spark2"));
        let links = match_links(&[("spark1".into(), &a), ("spark2".into(), &b)], &[]);
        let best = best_link(&links, "spark2", "spark1").unwrap();
        assert_eq!(best.kind, LinkKind::Cx7);
        assert_eq!(best.subnet.as_deref(), Some("192.168.100.0/24"));
        let (me, peer) = best.sides("spark2").unwrap();
        assert_eq!((me.ip.as_str(), peer.ip.as_str()), ("192.168.100.11", "192.168.100.10"));
        assert_eq!(me.ifname.as_deref(), Some("enp1s0f1np1"));
        assert_eq!(me.hcas, vec!["rocep1s0f1".to_string(), "roceP2p1s0f1".to_string()], "the twin is a second rail");
        assert_eq!(me.gid_index, Some(3));
        assert_eq!(best.gbps(), Some(200.0));
        // The LAN and Tailscale are there too, as slower fallbacks.
        let kinds: Vec<LinkKind> = links.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LinkKind::Lan) && kinds.contains(&LinkKind::Tailscale), "{kinds:?}");
        assert!(!links.iter().any(|l| l.a.ifname.as_deref() == Some("docker0")), "docker0 is never a link");
    }

    #[test]
    fn three_sparks_triangle_one_subnet_per_cable() {
        let f: Vec<(String, Fabric)> = fixtures::TRI.iter().map(|(n, t)| (n.to_string(), fixtures::load(t))).collect();
        let refs: Vec<(String, &Fabric)> = f.iter().map(|(n, x)| (n.clone(), x)).collect();
        let links = match_links(&refs, &[]);
        let direct: Vec<&Link> = links.iter().filter(|l| l.kind == LinkKind::Cx7).collect();
        assert_eq!(direct.len(), 3, "{direct:#?}");
        let pair = |a: &str, b: &str| best_link(&links, a, b).unwrap();
        assert_eq!(pair("spark1", "spark2").subnet.as_deref(), Some("10.10.12.0/30"));
        assert_eq!(pair("spark1", "spark3").subnet.as_deref(), Some("10.10.13.0/30"));
        assert_eq!(pair("spark2", "spark3").subnet.as_deref(), Some("10.10.23.0/30"));
        let (s3, s1) = pair("spark1", "spark3").sides("spark3").unwrap();
        assert_eq!((s3.ip.as_str(), s1.ip.as_str()), ("10.10.13.2", "10.10.13.1"));
        assert_eq!(s3.ifname.as_deref(), Some("enp1s0f0np0"));
        assert!(pair("spark1", "spark2").kind.is_direct());
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
        let links = match_links(&[("spark1".into(), &a), ("box".into(), &lan)], &[]);
        let best = best_link(&links, "spark1", "box").unwrap();
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
        let links = match_links(&[("spark1".into(), &a), ("spark2".into(), &b)], &[o]);
        assert_eq!(links.len(), 1);
        let (s2, s1) = links[0].sides("spark2").unwrap();
        assert_eq!((s2.ip.as_str(), s1.ip.as_str()), ("10.0.0.2", "10.0.0.1"));
        assert_eq!(s2.hcas, vec!["rocep1s0f0".to_string()]);
        assert!(links[0].manual && links[0].kind == LinkKind::Cx7 && links[0].gbps() == Some(100.0));
    }

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale("100.64.0.1") && is_tailscale("100.127.255.1"));
        assert!(!is_tailscale("100.128.0.1") && !is_tailscale("10.0.0.1"));
        assert_eq!(subnet_str(v4("10.10.12.2").unwrap(), 30), "10.10.12.0/30");
    }
}
