//! Planning multi-node (`[[nodes]]`) recipes: what each node's own recipe
//! asks of its machine.
//!
//! A model split over N Sparks needs about 1/N of its weights on each one,
//! so each node is checked against its share, not the whole download.

use lmdeck::cluster::{self, Opts};
use lmdeck::compat::{self, Level};
use lmdeck::config::Config;
use lmdeck::deck::Deck;
use lmdeck::hardware::{parse_nvidia_smi, Hardware};
use lmdeck::paths::Paths;
use lmdeck::recipe::Recipe;
use proptest::prelude::*;

/// One GiB, the unit lmdeck turns on-disk bytes into GB with.
const GIB: u64 = 1 << 30;

/// A DGX Spark as `lmdeck` detects one: 119.7 GB of unified memory.
fn spark() -> Hardware {
    Hardware::assemble(
        "linux".into(),
        "aarch64".into(),
        "spark".into(),
        String::new(),
        20,
        119.7,
        Some(110.0),
        parse_nvidia_smi("NVIDIA GB10, [N/A], 12.1, 580\n"),
    )
}

/// A deck rooted in `root` (config, cache and state under it) whose machine is
/// called `ctl` and whose models dir is empty.
fn deck(root: &std::path::Path) -> Deck {
    let home = root.display().to_string();
    let paths = Paths::resolve(|k| (k == "LMDECK_HOME").then(|| home.clone()), root.to_path_buf());
    let mut cfg = Config::default();
    cfg.node.name = Some("ctl".into());
    cfg.models_dir = root.join("models").display().to_string();
    Deck::new(paths, cfg)
}

/// A recipe split over `nodes` machines (every node on this one, so planning
/// needs no network), its weights `size_gb` big, plus `hardware` lines.
fn split(nodes: usize, size_gb: f64, hardware: &str) -> Recipe {
    let mut t = format!(
        "name = \"split\"\n[model]\nrepo = \"lmdeck-test/huge-model\"\nsize_gb = {size_gb}\n\
         [engine]\nname = \"custom\"\nport = 8999\n[hardware]\nnodes = {nodes}\n{hardware}\n"
    );
    for i in 0..nodes {
        let role = if i == 0 { "head" } else { "worker" };
        t.push_str(&format!(
            "[[nodes]]\nname = \"n{i}\"\nrole = \"{role}\"\nhost = \"local\"\ncommand = [\"sh\", \"-c\", \"sleep 1\"]\n"
        ));
    }
    Recipe::parse(&t, "split").unwrap()
}

#[test]
fn each_node_of_a_split_model_is_checked_against_its_share() {
    let tmp = tempfile::tempdir().unwrap();
    // 180 GB: well above one Spark (×1.2 = 216 GB), below two.
    let r = split(2, 180.0, "");
    let plan = cluster::plan(&deck(tmp.path()), &r, &Opts::default()).unwrap();
    assert_eq!(plan.nodes.len(), 2);
    for n in &plan.nodes {
        assert_eq!(compat::required_memory_gb(&n.recipe, None), Some(90.0), "{}", n.name);
        let rep = compat::check(&n.recipe, &spark(), None, None);
        assert!(rep.compatible(), "{}: {:?}", n.name, rep.reasons(true));
        // The head holds the whole download on its disk: still only its share.
        let rep = compat::check(&n.recipe, &spark(), None, Some(180 * GIB));
        assert!(rep.compatible(), "{} with the weights on disk: {:?}", n.name, rep.reasons(true));
    }
    let desc = plan.describe("ctl").join("\n");
    assert!(desc.contains("memory: needs ~90 GB"), "{desc}");
}

#[test]
fn a_share_that_does_not_fit_one_spark_is_still_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let plan = cluster::plan(&deck(tmp.path()), &split(2, 300.0, ""), &Opts::default()).unwrap();
    for n in &plan.nodes {
        let rep = compat::check(&n.recipe, &spark(), None, Some(300 * GIB));
        assert_eq!(rep.level(), Level::Fail, "{}: {:?}", n.name, rep.checks);
        assert!(rep.reasons(false).iter().any(|r| r.contains("needs ~150 GB")), "{:?}", rep.reasons(false));
    }
}

#[test]
fn an_explicit_minimum_is_what_each_node_needs() {
    let tmp = tempfile::tempdir().unwrap();
    // `min_memory_gb` on a multi-node recipe is per machine: never divided.
    let plan = cluster::plan(&deck(tmp.path()), &split(3, 600.0, "min_memory_gb = 100"), &Opts::default()).unwrap();
    for n in &plan.nodes {
        assert_eq!(n.recipe.hardware.min_memory_gb, Some(100.0), "{}", n.name);
    }
    // Three nodes, no minimum: a third each.
    let plan = cluster::plan(&deck(tmp.path()), &split(3, 300.0, ""), &Opts::default()).unwrap();
    assert!(plan.nodes.iter().all(|n| n.recipe.hardware.min_memory_gb == Some(100.0)));
    // No size anywhere: nothing to pin, the node estimates from what it has.
    let mut r = split(2, 1.0, "");
    r.model.size_gb = None;
    let plan = cluster::plan(&deck(tmp.path()), &r, &Opts::default()).unwrap();
    assert!(plan.nodes.iter().all(|n| n.recipe.hardware.min_memory_gb.is_none()));
}

proptest! {
    /// Split over N machines, a model needs size / N on each, which fits a
    /// Spark exactly when the share does.
    #[test]
    fn the_share_is_the_size_over_the_machines(size in 1.0f64..2000.0, nodes in 2usize..9) {
        let r = split(nodes, size, "");
        let need = compat::required_memory_gb(&r, None).unwrap();
        prop_assert!((need - size / nodes as f64).abs() < 1e-9);
        let one = split(1, size, "");
        prop_assert!(need < compat::required_memory_gb(&one, None).unwrap());
        let fits = compat::check(&r, &spark(), None, None).compatible();
        prop_assert_eq!(fits, need <= spark().model_memory_gb());
    }
}
