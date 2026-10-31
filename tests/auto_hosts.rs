//! `host = "auto"` in `[[nodes]]`: lmdeck places workers on the head's CX7
//! fabric. On a switchless ring it takes the head's cable neighbours (next,
//! then previous), then goes on around the ring, and ranks its picks in
//! cabled order; on a cable it takes the peer; on a switch or a mesh, every
//! machine with a CX7 link in rack order (this machine, then hosts.toml).
//!
//! The peers' addresses then reach the commands as placeholders, so no
//! recipe names another machine: `{peer_addr}` / `{peer_host}`,
//! `{peer2_addr}` / `{peer2_host}`, … in node commands, and in a wrapped
//! recipe whose own scripts drive `[hardware] hosts` Sparks.

use lmdeck::fabric::{build_graph, fixtures, shape, Addr, Fabric, Graph, Nic, NicKind};
use lmdeck::{pick, vars};
use proptest::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Owned copies of `items`.
fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The graph of a built-in fixture set (`spark1`…`sparkN`).
fn fixture(set: &[(&str, &str)]) -> Graph {
    let f = fixtures::set(set);
    let refs: Vec<(String, &Fabric)> = f.iter().map(|(k, f)| (k.clone(), f)).collect();
    build_graph(&refs, &[])
}

/// The hosts of a candidate list, best first.
fn hosts(c: Vec<(String, usize)>) -> Vec<String> {
    c.into_iter().map(|(h, _)| h).collect()
}

/// Names a host for messages the way the CLI does (keys as they are here).
fn show(h: &str) -> String {
    h.to_string()
}

#[test]
fn a_ring_offers_next_then_previous_then_onward() {
    let rack = strings(&["spark1", "spark2", "spark3", "spark4"]);
    let g = fixture(&fixtures::RING4);
    assert_eq!(hosts(pick::candidates(&g, &rack, "spark1")), ["spark2", "spark4", "spark3"]);
    // A head elsewhere on the ring gets its own neighbours.
    assert_eq!(hosts(pick::candidates(&g, &rack, "spark3")), ["spark4", "spark2", "spark1"]);
    // Racked 1-2-3-4 but cabled 1-2-4-3-1: the cables decide.
    let g = fixture(&fixtures::FOLD4);
    assert_eq!(hosts(pick::candidates(&g, &rack, "spark1")), ["spark2", "spark3", "spark4"]);
}

#[test]
fn a_cable_a_mesh_and_a_switch_offer_cx7_hosts_in_rack_order() {
    assert_eq!(
        hosts(pick::candidates(&fixture(&fixtures::DUO), &strings(&["spark1", "spark2"]), "spark1")),
        ["spark2"]
    );
    let tri = fixture(&fixtures::TRI);
    assert_eq!(
        hosts(pick::candidates(&tri, &strings(&["spark1", "spark2", "spark3"]), "spark3")),
        ["spark1", "spark2"]
    );
    assert_eq!(
        hosts(pick::candidates(&tri, &strings(&["spark3", "spark2", "spark1"]), "spark3")),
        ["spark2", "spark1"]
    );
    let all: Vec<String> = (1..=8).map(|i| format!("spark{i}")).collect();
    let want: Vec<String> = all.iter().filter(|h| *h != "spark5").cloned().collect();
    assert_eq!(hosts(pick::candidates(&fixture(&fixtures::SWITCH8), &all, "spark5")), want);
}

#[test]
fn picks_skip_taken_hosts_and_rank_in_cabled_order() {
    let rack = strings(&["spark1", "spark2", "spark3", "spark4"]);
    let g = fixture(&fixtures::RING4);
    let used = strings(&["spark1"]);
    assert_eq!(pick::choose(&g, &rack, "spark1", &used, 1, &show).unwrap(), ["spark2"]);
    assert_eq!(pick::choose(&g, &rack, "spark1", &used, 2, &show).unwrap(), ["spark2", "spark4"]);
    assert_eq!(pick::choose(&g, &rack, "spark1", &used, 3, &show).unwrap(), ["spark2", "spark3", "spark4"]);
    // Another node holds the head's next neighbour: the previous one comes first.
    let used = strings(&["spark1", "spark2"]);
    assert_eq!(pick::choose(&g, &rack, "spark1", &used, 1, &show).unwrap(), ["spark4"]);
    assert_eq!(pick::choose(&g, &rack, "spark1", &used, 2, &show).unwrap(), ["spark3", "spark4"]);
    // Deterministic.
    assert_eq!(pick::choose(&g, &rack, "spark1", &used, 2, &show).unwrap(), ["spark3", "spark4"]);
}

#[test]
fn too_few_machines_name_what_is_missing() {
    let rack = strings(&["spark1", "spark2", "spark3", "spark4"]);
    // spark3 doesn't answer: the ring is a chain spark4-spark1-spark2.
    let up: Vec<(&str, &str)> = fixtures::RING4.iter().copied().filter(|(h, _)| *h != "spark3").collect();
    let g = fixture(&up);
    assert_eq!(hosts(pick::candidates(&g, &rack, "spark1")), ["spark2", "spark4"]);
    let e = format!("{:#}", pick::choose(&g, &rack, "spark1", &strings(&["spark1"]), 3, &show).unwrap_err());
    assert!(e.contains("only 2") && e.contains("spark2, spark4") && e.contains("spark3"), "{e}");
    let e = format!("{:#}", pick::choose(&g, &rack, "spark1", &strings(&["spark1", "spark4"]), 2, &show).unwrap_err());
    assert!(e.contains("only 1") && e.contains("spark4") && e.contains("other nodes"), "{e}");
    // A head that isn't on the fabric has nobody to place around it.
    assert!(pick::candidates(&g, &rack, "spark3").is_empty());
    assert!(pick::choose(&g, &rack, "spark3", &[], 1, &show).is_err());
}

/// An up interface with one /30 (CX7) or /24 address.
fn nic(name: &str, kind: NicKind, ip: &str) -> Nic {
    Nic {
        name: name.into(),
        kind,
        mac: None,
        state: "up".into(),
        mtu: None,
        speed_mbps: (kind == NicKind::Cx7).then_some(200_000),
        driver: None,
        ipv4: vec![Addr { ip: ip.into(), prefix: if kind == NicKind::Cx7 { 30 } else { 24 } }],
        ipv6: vec![],
        rdma: None,
        gid_index: None,
    }
}

/// Spark `i` of a ring of `n`: cable `i` (to `i + 1`) on one port, cable
/// `i - 1` on the other, and Wi-Fi.
fn ring_spark(i: usize, n: usize) -> Fabric {
    let prev = (i + n - 1) % n;
    let nics = vec![
        nic("enp1s0f0np0", NicKind::Cx7, &format!("10.60.{i}.1")),
        nic("enp1s0f1np1", NicKind::Cx7, &format!("10.60.{prev}.2")),
        nic("wlP9s9", NicKind::Wifi, &format!("192.168.9.{}", 10 + i)),
    ];
    Fabric { hostname: format!("s{i}"), device_kind: "spark".into(), nics, ..Default::default() }
}

proptest! {
    /// On a ring of any size, racked in any order: the picks are distinct,
    /// leave out the head and taken hosts, sit next to the head as one arc,
    /// and with every machine taken the ranks walk the cables.
    #[test]
    fn ring_picks_are_an_arc_around_the_head(
        n in 4usize..=10,
        rack in Just((0..10).collect::<Vec<usize>>()).prop_shuffle(),
        head in 0usize..10,
        count in 1usize..10,
    ) {
        let rack: Vec<usize> = rack.into_iter().filter(|i| *i < n).collect();
        let (head, count) = (head % n, 1 + count % (n - 1));
        let f: Vec<(String, Fabric)> = (0..n).map(|i| (format!("s{i}"), ring_spark(i, n))).collect();
        let refs: Vec<(String, &Fabric)> = f.iter().map(|(k, f)| (k.clone(), f)).collect();
        let g = build_graph(&refs, &[]);
        let rack: Vec<String> = rack.iter().map(|i| format!("s{i}")).collect();
        let head_key = format!("s{head}");
        let picks = pick::choose(&g, &rack, &head_key, std::slice::from_ref(&head_key), count, &show).unwrap();
        prop_assert_eq!(&picks, &pick::choose(&g, &rack, &head_key, std::slice::from_ref(&head_key), count, &show).unwrap());
        let idx: Vec<usize> = picks.iter().map(|p| p[1..].parse().unwrap()).collect();
        let mut seen = idx.clone();
        seen.sort_unstable();
        seen.dedup();
        prop_assert_eq!(seen.len(), count);
        prop_assert!(!idx.contains(&head));
        // Next, then previous: both cable neighbours once there are two picks.
        let near = [(head + 1) % n, (head + n - 1) % n];
        prop_assert!(near.iter().filter(|i| idx.contains(i)).count() >= count.min(2));
        // One arc: the head and its picks sit together, one run around the ring.
        let arc: std::collections::BTreeSet<usize> = idx.iter().copied().chain([head]).collect();
        let ends = arc.iter().filter(|i| !arc.contains(&((**i + 1) % n))).count();
        prop_assert!(ends == 1 || arc.len() == n, "{:?}", arc);
        if count == n - 1 {
            let mut ranks = vec![head_key.clone()];
            ranks.extend(picks.iter().cloned());
            prop_assert_eq!(shape(&g, &ranks).order, ranks);
        }
    }
}

/// An lmdeck home whose fabric is a built-in demo topology: this machine is
/// `spark1`, and hosts.toml lists `spark2`…`spark{n}` (never contacted).
struct Demo {
    root: PathBuf,
    topo: &'static str,
}

impl Demo {
    /// Sets up `root` for the `topo` fixtures (`ring4`, `fold4`, `duo`, `tri`).
    fn new(root: &Path, topo: &'static str, n: usize) -> Demo {
        fs::create_dir_all(root.join("home/config")).unwrap();
        fs::write(root.join("home/config/config.toml"), "[node]\nname = \"spark1\"\n").unwrap();
        let hosts: String =
            (2..=n).map(|i| format!("[[host]]\nname = \"spark{i}\"\nurl = \"http://127.0.0.1:9\"\n\n")).collect();
        fs::write(root.join("home/config/hosts.toml"), hosts).unwrap();
        Demo { root: root.to_path_buf(), topo }
    }

    /// Runs lmdeck against this home.
    fn lmdeck(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lmdeck"))
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("LMDECK_HOME", self.root.join("home"))
            .env("LMDECK_MODELS_DIR", self.root.join("models"))
            .env("HF_HUB_CACHE", self.root.join("hf"))
            .env("LMDECK_DEMO", "1")
            .env("LMDECK_DEMO_TOPO", self.topo)
            .env("LMDECK_NO_SYNC", "1")
            .env("LMDECK_NO_UPDATE_CHECK", "1")
            .env("NO_COLOR", "1")
            .env("USER", "tester")
            .env_remove("HF_HOME")
            .env_remove("LMDECK_FABRIC")
            .output()
            .unwrap()
    }

    /// stdout of a run that must succeed.
    fn run(&self, args: &[&str]) -> String {
        let o = self.lmdeck(args);
        let (so, se) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(o.status.success(), "lmdeck {args:?} failed\n{so}\n{se}");
        so.into_owned()
    }

    /// stdout and stderr of a run that must fail.
    fn fail(&self, args: &[&str]) -> String {
        let o = self.lmdeck(args);
        assert!(!o.status.success(), "lmdeck {args:?} should fail: {}", String::from_utf8_lossy(&o.stdout));
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    }

    /// Adds a recipe to the deck; the error text when it is refused.
    fn add(&self, name: &str, body: &str) -> Result<(), String> {
        let f = self.root.join(format!("{name}.toml"));
        fs::write(&f, body).unwrap();
        let o = self.lmdeck(&["recipe", "add", f.to_str().unwrap()]);
        if o.status.success() {
            return Ok(());
        }
        Err(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    }
}

/// A recipe named `name` with a head on this machine and one worker per entry
/// of `workers` (its host), each echoing `body`.
fn nodes_recipe(name: &str, workers: &[&str], body: &str) -> String {
    let mut t = format!(
        "name = \"{name}\"\n[engine]\nname = \"custom\"\nport = 8999\n\n\
         [[nodes]]\nname = \"head\"\nrole = \"head\"\nhost = \"local\"\ncommand = [\"sh\", \"-c\", \"echo head {body}\"]\n"
    );
    for (i, h) in workers.iter().enumerate() {
        t.push_str(&format!(
            "\n[[nodes]]\nname = \"w{}\"\nrole = \"worker\"\nhost = \"{h}\"\ncommand = [\"sh\", \"-c\", \"echo w{} {body}\"]\n",
            i + 1,
            i + 1
        ));
    }
    t
}

#[test]
fn auto_workers_follow_the_ring_in_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Demo::new(tmp.path(), "ring4", 4);
    d.add("quad", &nodes_recipe("quad", &["auto", "auto", "auto"], "{rank}")).unwrap();
    let plan = d.run(&["use", "quad", "--dry-run"]);
    for want in [
        "2. w1 [worker] on spark2 (auto)",
        "3. w2 [worker] on spark3 (auto)",
        "4. w3 [worker] on spark4 (auto)",
        "cabled order spark1 (local) → spark2 → spark3 → spark4 → spark1",
    ] {
        assert!(plan.contains(want), "missing {want:?} in\n{plan}");
    }
    assert!(!plan.contains("skip cables"), "{plan}");
    // Two workers: the head's two cable neighbours.
    d.add("trio", &nodes_recipe("trio", &["auto", "auto"], "{rank}")).unwrap();
    let plan = d.run(&["use", "trio", "--dry-run"]);
    assert!(plan.contains("w1 [worker] on spark2 (auto)") && plan.contains("w2 [worker] on spark4 (auto)"), "{plan}");

    // Folded: racked 1-2-3-4, cabled 1-2-4-3-1.
    let tmp = tempfile::tempdir().unwrap();
    let d = Demo::new(tmp.path(), "fold4", 4);
    d.add("quad", &nodes_recipe("quad", &["auto", "auto", "auto"], "{rank}")).unwrap();
    let plan = d.run(&["use", "quad", "--dry-run"]);
    let at = |s: &str| plan.find(s).unwrap_or_else(|| panic!("missing {s:?} in\n{plan}"));
    assert!(at("on spark2 (auto)") < at("on spark4 (auto)") && at("on spark4 (auto)") < at("on spark3 (auto)"));
    assert!(!plan.contains("skip cables"), "{plan}");
}

#[test]
fn auto_skips_named_hosts_and_needs_enough_machines() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Demo::new(tmp.path(), "ring4", 4);
    d.add("mixed", &nodes_recipe("mixed", &["spark2", "auto"], "{rank}")).unwrap();
    let plan = d.run(&["use", "mixed", "--dry-run"]);
    assert!(plan.contains("w1 [worker] on spark2 ") && plan.contains("w2 [worker] on spark4 (auto)"), "{plan}");

    d.add("five", &nodes_recipe("five", &["auto", "auto", "auto", "auto"], "{rank}")).unwrap();
    let err = d.fail(&["use", "five", "--dry-run"]);
    assert!(
        err.contains("host = \"auto\"") && err.contains("only 3") && err.contains("spark2, spark4, spark3"),
        "{err}"
    );

    // The head stays where the recipe puts it.
    let head_auto = nodes_recipe("bad", &["local"], "x").replacen("host = \"local\"", "host = \"auto\"", 1);
    let err = d.add("bad", &head_auto).unwrap_err();
    assert!(err.contains("the head needs a host"), "{err}");
}

#[test]
fn placeholders_leave_shell_and_json_braces_alone() {
    assert_eq!(vars::names("--port={port} --rpc {peer2_addr}:50052"), ["port", "peer2_addr"]);
    assert!(vars::names("${HOME}/b ${WORKER_IP:?set it} {\"method\":\"mtp\"} {Upper} {a b} {}").is_empty());
    assert_eq!(vars::names("{{model}}"), ["model"]);
    assert_eq!(vars::peer_index("peer_addr"), Some(1));
    assert_eq!(vars::peer_index("peer3_host"), Some(3));
    for not in ["peer1_addr", "peer_ip", "peers_addr", "peer2"] {
        assert_eq!(vars::peer_index(not), None, "{not}");
    }
    let v = vars::peer_vars(&[("gigabyte".into(), "10.0.0.2".into()), ("ember".into(), "10.0.1.2".into())]);
    let want =
        [("peer2_addr", "10.0.1.2"), ("peer2_host", "ember"), ("peer_addr", "10.0.0.2"), ("peer_host", "gigabyte")];
    assert_eq!(v.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), want);
}

proptest! {
    /// Every `{name}` in a text is a placeholder unless a shell owns it (`${…}`).
    #[test]
    fn placeholders_are_found_unless_a_shell_owns_them(
        parts in prop::collection::vec(("[a-z][a-z0-9_]{0,12}", "[ -#%-z|~]{0,8}", any::<bool>()), 0..6)
    ) {
        let text: String = parts
            .iter()
            .map(|(name, filler, shell)| format!("{filler}{}{{{name}}}", if *shell { "$" } else { "" }))
            .collect();
        let want: Vec<&str> = parts.iter().filter(|(_, _, shell)| !shell).map(|(n, _, _)| n.as_str()).collect();
        prop_assert_eq!(vars::names(&text), want);
    }
}

#[test]
fn nodes_get_their_peers_in_node_order() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Demo::new(tmp.path(), "ring4", 4);
    let body = "{peer_host}={peer_addr} {peer2_host}={peer2_addr}";
    d.add("peers", &nodes_recipe("peers", &["auto", "auto"], body)).unwrap();
    let plan = d.run(&["use", "peers", "--dry-run"]);
    // The head on spark1 reaches w1 (spark2) and w2 (spark4) on their cables.
    assert!(plan.contains("echo head spark2=10.40.1.2 spark4=10.40.4.1"), "{plan}");
    // w1 on spark2: the head, then w2 across the ring (through spark1).
    assert!(plan.contains("echo w1 spark1=10.40.1.1 spark4=10.40.4.1"), "{plan}");
    // w2 on spark4: the head, then w1.
    assert!(plan.contains("echo w2 spark1=10.40.4.2 spark2=10.40.1.2"), "{plan}");

    // A 2-node recipe has one peer per node.
    d.add("pair", &nodes_recipe("pair", &["auto"], "{peer2_addr}")).unwrap();
    let err = d.fail(&["use", "pair", "--dry-run"]);
    assert!(err.contains("{peer2_addr}") && err.contains("1 peer"), "{err}");
}

/// A wrapped recipe (`./start.sh` drives the other Sparks itself) with
/// environment lines `env` and, when given, `[hardware] hosts`.
fn wrapped(name: &str, hosts: Option<u32>, env: &str) -> String {
    let hw = hosts.map(|h| format!("[hardware]\nhosts = {h}\n")).unwrap_or_default();
    format!("name = \"{name}\"\n[engine]\nname = \"custom\"\ncommand = [\"./start.sh\"]\nport = 8890\n[engine.env]\n{env}\n{hw}")
}

#[test]
fn wrapped_scripts_get_the_sparks_lmdeck_picks() {
    let tmp = tempfile::tempdir().unwrap();
    let d = Demo::new(tmp.path(), "ring4", 4);
    let env = "WORKER = \"{user}@{peer_addr}\"\nWORKER2 = \"{user}@{peer2_addr}\"\nFABRIC_PEER = \"{peer_addr}\"\n\
               NAMES = \"{peer_host},{peer2_host}\"\nAT = \"{home}/cache\"";
    d.add("tf3", &wrapped("tf3", Some(3), env)).unwrap();
    let plan = d.run(&["use", "tf3", "--dry-run"]);
    let home = tmp.path().join("home");
    for want in [
        "env:  WORKER=tester@10.40.1.2".to_string(),
        "env:  WORKER2=tester@10.40.4.1".to_string(),
        "env:  FABRIC_PEER=10.40.1.2".to_string(),
        "env:  NAMES=spark2,spark4".to_string(),
        format!("env:  AT={}/cache", home.display()),
    ] {
        assert!(plan.contains(&want), "missing {want:?} in\n{plan}");
    }

    // Peers need the Spark count, and only that many exist.
    d.add("nohosts", &wrapped("nohosts", None, "WORKER = \"{peer_addr}\"")).unwrap();
    let err = d.fail(&["use", "nohosts", "--dry-run"]);
    assert!(err.contains("{peer_addr}") && err.contains("hosts"), "{err}");
    d.add("toofar", &wrapped("toofar", Some(3), "W3 = \"{peer3_addr}\"")).unwrap();
    let err = d.fail(&["use", "toofar", "--dry-run"]);
    assert!(err.contains("{peer3_addr}") && err.contains("2 peer"), "{err}");
    // More Sparks than the fabric has.
    d.add("six", &wrapped("six", Some(6), "W = \"{peer_addr}\"")).unwrap();
    let err = d.fail(&["use", "six", "--dry-run"]);
    assert!(err.contains("only 3") && err.contains("hosts = 6"), "{err}");
}

/// The built-in DeepSeek-V4.1-Flash-1017 4x recipe wraps Mia's scripts, which
/// read their workers from WORKER, WORKER2 and WORKER3 (the environment first,
/// then scripts/local.sh). lmdeck hands them the Sparks it picks, in cabled
/// order, so the recipe starts without a hand-written scripts/local.sh. It
/// also points HF_CACHE and NFS_PATH at the head's Hugging Face home, so the
/// workers mount it by its real path (a plain export, no fsid=0 root).
#[test]
fn builtin_four_spark_recipe_hands_mias_scripts_its_workers() {
    let name = "deepseek-v4.1-flash-1017-4x-spark-tensorfold";
    // A ring cabled in rack order, and a folded one (1-2-4-3-1); the Spark across
    // the ring is reached through spark2 either way.
    let rings =
        [("ring4", ["10.40.1.2", "10.40.2.2", "10.40.4.1"]), ("fold4", ["10.41.1.2", "10.41.2.2", "10.41.4.1"])];
    for (topo, want) in rings {
        let tmp = tempfile::tempdir().unwrap();
        let d = Demo::new(tmp.path(), topo, 4);
        let plan = d.run(&["use", name, "--dry-run"]);
        assert!(plan.contains("run:  ./start-switchless.sh"), "{topo}:\n{plan}");
        for (var, addr) in ["WORKER", "WORKER2", "WORKER3"].into_iter().zip(want) {
            let line = format!("env:  {var}={addr}");
            assert!(plan.contains(&line), "{topo}: missing {line:?} in\n{plan}");
        }
        let hf = tmp.path().join("home/.cache/huggingface");
        for var in ["HF_CACHE", "NFS_PATH"] {
            let line = format!("env:  {var}={}", hf.display());
            assert!(plan.contains(&line), "{topo}: missing {line:?} in\n{plan}");
        }
    }
    // Fewer Sparks than the recipe drives: the plan stops and says how many it needs.
    let tmp = tempfile::tempdir().unwrap();
    let d = Demo::new(tmp.path(), "tri", 3);
    let err = d.fail(&["use", name, "--dry-run"]);
    assert!(err.contains("hosts = 4") && err.contains("3 more Spark"), "{err}");
}
