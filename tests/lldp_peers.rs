//! Sparks cabled to each other read as cables, not switches, even though each
//! one announces itself over LLDP as a bridge.
//!
//! DGX OS runs lldpd, which reports the Bridge capability on any machine with a
//! Linux bridge (Docker's `docker0` and `br-*`) and Router when `ip_forward=1`.
//! So the neighbour at the far end of a CX7 cable is the other Spark announcing
//! Bridge + Router, and each Spark also hears its own frames on the twin netdev
//! of the same QSFP port. The data mirrors `lldpctl` on a real four-Spark ring
//! cabled spark-gigabyte-faraday-ember-spark with a /24 per netdev pair.

use lmdeck::fabric::{
    build_graph, shape, Addr, Fabric, Graph, LinkKind, Neighbor, Nic, NicKind, SegmentClass, ShapeKind,
};
use proptest::prelude::*;

/// The four CX7 netdevs of a Spark: two per QSFP port.
const CX7: [&str; 4] = ["enp1s0f0np0", "enp1s0f1np1", "enP2p1s0f0np0", "enP2p1s0f1np1"];

/// An up interface with one /24 address.
fn nic(name: &str, kind: NicKind, ip: &str) -> Nic {
    let speed_mbps = (kind == NicKind::Cx7).then_some(200_000);
    Nic {
        name: name.into(),
        kind,
        mac: None,
        state: "up".into(),
        mtu: None,
        speed_mbps,
        driver: None,
        ipv4: vec![Addr { ip: ip.into(), prefix: 24 }],
        ipv6: vec![],
        rdma: None,
        gid_index: None,
    }
}

/// A Spark with addresses on its CX7 netdevs (in [`CX7`] order) and on Wi-Fi.
fn spark(hostname: &str, cx7: &[&str], wifi: &str) -> Fabric {
    let mut nics: Vec<Nic> = CX7.iter().zip(cx7).map(|(n, ip)| nic(n, NicKind::Cx7, ip)).collect();
    nics.push(nic("wlP9s9", NicKind::Wifi, wifi));
    Fabric { hostname: hostname.into(), device_kind: "spark".into(), nics, ..Default::default() }
}

/// One LLDP neighbour announcing Bridge (+ Router), as lmdeck reads `lldpctl`.
fn heard(ifname: &str, chassis: &str) -> Neighbor {
    Neighbor { ifname: ifname.into(), chassis: Some(chassis.into()), port: None, is_bridge: true }
}

/// Fills each host's LLDP table the way the real cluster reports it: on every
/// port, each other host on that /24, plus the host itself on its CX7 ports.
/// `chassis` turns a machine hostname into the name its lldpd announces.
fn run_lldpd(hosts: &mut [(String, Fabric)], chassis: impl Fn(&str) -> String) {
    let all: Vec<Fabric> = hosts.iter().map(|(_, f)| f.clone()).collect();
    let net = |n: &Nic| n.ipv4[0].ip.rsplit_once('.').map(|(net, _)| net.to_string());
    for (i, (_, f)) in hosts.iter_mut().enumerate() {
        let mut lldp = vec![];
        for n in &f.nics {
            for (j, other) in all.iter().enumerate() {
                let peer = j != i && other.nics.iter().any(|o| net(o) == net(n));
                // Reason: the twin netdev of the same QSFP port echoes the host's own frames.
                let itself = j == i && n.kind == NicKind::Cx7;
                if peer || itself {
                    lldp.push(heard(&n.name, &chassis(&other.hostname)));
                }
            }
        }
        f.lldp = lldp;
    }
}

/// The real cluster, keyed the way `lmdeck hosts topology` keys it (the
/// controller is `local`): cabled spark-gigabyte-faraday-ember-spark.
fn cluster() -> Vec<(String, Fabric)> {
    let controller = spark("spark", &["10.100.132.1", "10.100.128.2", "10.100.133.1", "10.100.129.2"], "192.168.1.102");
    let gigabyte =
        spark("gigabyte", &["10.100.128.1", "10.100.134.1", "10.100.129.1", "10.100.135.1"], "192.168.1.132");
    let faraday = spark("faraday", &["10.100.134.2", "10.100.136.1", "10.100.135.2", "10.100.137.1"], "192.168.1.170");
    let ember = spark("ember", &["10.100.136.2", "10.100.132.2", "10.100.137.2", "10.100.133.2"], "192.168.1.152");
    vec![
        ("local".into(), controller),
        ("gigabyte".into(), gigabyte),
        ("faraday".into(), faraday),
        ("ember".into(), ember),
    ]
}

/// The graph topology builds from these fabrics (no hosts.toml overrides).
fn graph(hosts: &[(String, Fabric)]) -> Graph {
    let refs: Vec<(String, &Fabric)> = hosts.iter().map(|(n, f)| (n.clone(), f)).collect();
    build_graph(&refs, &[])
}

/// Asserts every CX7 segment is a direct cable and the hosts form one ring
/// walked in `order`.
fn assert_ring(g: &Graph, order: &[&str]) {
    let cx7: Vec<_> = g.segments.iter().filter(|s| s.kind == LinkKind::Cx7).collect();
    assert!(!cx7.is_empty(), "no CX7 segments");
    for s in &cx7 {
        assert_eq!(s.class, SegmentClass::Cable, "{} should be a cable, read as {:?}", s.subnet, s.switch);
    }
    assert!(g.switches.iter().all(|s| s.kind != LinkKind::Cx7), "CX7 switches in a switchless ring: {:?}", g.switches);
    let rack: Vec<String> = order.iter().map(|h| h.to_string()).collect();
    let s = shape(g, &rack);
    assert_eq!(s.kind, ShapeKind::Ring);
    assert_eq!(s.order, rack);
}

#[test]
fn control_the_real_ring_without_lldp_is_a_ring() {
    assert_ring(&graph(&cluster()), &["local", "gigabyte", "faraday", "ember"]);
}

#[test]
fn sparks_announcing_bridge_and_router_stay_cables() {
    let mut hosts = cluster();
    // As on the real cluster: faraday's lldpd announces its FQDN.
    run_lldpd(&mut hosts, |h| if h == "faraday" { "faraday.lan".into() } else { h.into() });
    assert_ring(&graph(&hosts), &["local", "gigabyte", "faraday", "ember"]);
}

#[test]
fn peers_match_ignoring_case_and_domain_and_by_machine_hostname() {
    let mut hosts = cluster();
    // Keys that aren't the machine hostname: the controller is `local`, and
    // faraday is listed under an alias in hosts.toml.
    hosts[2].0 = "node-3".into();
    run_lldpd(&mut hosts, |h| match h {
        "spark" => "SPARK.lan".into(),
        "gigabyte" => "Gigabyte.local".into(),
        other => other.to_uppercase(),
    });
    assert_ring(&graph(&hosts), &["local", "gigabyte", "node-3", "ember"]);
}

#[test]
fn a_real_switch_or_an_unknown_bridge_is_still_a_switch() {
    for chassis in ["SN2100-TOR", "pve-node"] {
        let mut hosts = vec![
            ("local".to_string(), spark("spark", &["10.50.0.1"], "192.168.1.102")),
            ("gigabyte".to_string(), spark("gigabyte", &["10.50.0.2"], "192.168.1.132")),
        ];
        hosts[0].1.lldp.push(heard("enp1s0f0np0", chassis));
        let g = graph(&hosts);
        let seg = g.segments.iter().find(|s| s.subnet == "10.50.0.0/24").expect("the CX7 segment");
        assert_eq!(seg.class, SegmentClass::Switch, "{chassis} is a bridge that isn't one of the hosts");
        assert_eq!(seg.switch.as_ref().map(|s| s.name.as_str()), Some(chassis));
    }
}

#[test]
fn the_wifi_switch_is_not_named_after_a_spark() {
    let mut hosts = cluster();
    run_lldpd(&mut hosts, |h| h.into());
    let g = graph(&hosts);
    let wifi = g.switches.iter().find(|s| s.subnet == "192.168.1.0/24").expect("four hosts on Wi-Fi share a switch");
    assert_eq!(wifi.name, "lan-switch 192.168.1.0/24");
}

proptest! {
    /// Rings of any size stay rings, whatever case and domain lldpd names the
    /// hosts with, and whatever the hostnames look like (`storage` contains
    /// `tor`, `leafy` contains `leaf`).
    #[test]
    fn rings_of_any_size_with_lldpd_stay_rings(
        n in 4usize..=10,
        stem in prop::sample::select(vec!["spark", "storage", "leafy"]),
        suffix in prop::sample::select(vec!["", ".lan", ".local"]),
        upper in any::<bool>(),
    ) {
        // Host i owns cable i (to host i+1) on f0, and cable i-1 on f1.
        let mut hosts: Vec<(String, Fabric)> = (0..n)
            .map(|i| {
                let prev = (i + n - 1) % n;
                let (a, b) = (format!("10.200.{i}.1"), format!("10.200.{prev}.2"));
                (format!("h{i}"), spark(&format!("{stem}-{i}"), &[a.as_str(), b.as_str()], &format!("192.168.7.{}", 10 + i)))
            })
            .collect();
        run_lldpd(&mut hosts, |h| {
            let c = format!("{h}{suffix}");
            if upper { c.to_uppercase() } else { c }
        });
        let order: Vec<String> = (0..n).map(|i| format!("h{i}")).collect();
        let order: Vec<&str> = order.iter().map(String::as_str).collect();
        assert_ring(&graph(&hosts), &order);
    }
}
