//! Where `lmdeck use` runs a model when no `--on` says: on the host that holds
//! its weights (the controller's weights index over its hosts), here when this
//! machine holds them too, and here when no host does. `--on local` runs it
//! here anyway, the dashboard's Start follows the same rule, and the built-in
//! DeepSeek 4x recipe goes to the Spark holding its checkpoint.

mod common;
use common::*;

use lmdeck::hosts::HostsFile;
use lmdeck::placement::Pick;
use lmdeck::recipe::Recipe;
use lmdeck::weights::{HostWeights, Index, Inventory, Item};
use serde_json::json;
use std::fs;
use std::path::Path;

const REPO: &str = "lmdeck-test/tiny-model";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A stand-in engine whose weights are `REPO` and which answers HTTP like `web_recipe`.
fn weights_recipe(name: &str, root: &Path, port: u16) -> String {
    format!(
        "name = \"{name}\"\n[model]\nrepo = \"{REPO}\"\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 60\ncommand = [\"python3\", \"-u\", \"-m\", \"http.server\", \"{{port}}\", \"--bind\", \"127.0.0.1\", \"--directory\", \"{}\"]\n",
        www(root).display()
    )
}

/// Puts `REPO`'s weights in `host`'s Hugging Face cache.
fn give_weights(host: &Host) {
    fake_hf(&host.dir.join("hf"), REPO, COMMIT, &[("config.json", 2), ("model.safetensors", 1000)]);
}

/// A controller that knows `a` (which holds the weights and runs recipes it is
/// sent) and `b` (which holds nothing), and has the recipe `tiny`.
fn controller_with_weights_on_a(root: &Path) -> (Served, Served, Host) {
    let ha = Host::new(root, "sa", "[serve]\naccept_recipes = true\n");
    give_weights(&ha);
    let a = serve(ha, &[]);
    let b = serve(Host::new(root, "sb", "[serve]\naccept_recipes = true\n"), &[]);
    let c = Host::new(root, "ctl", "");
    fs::write(c.dir.join("home/config/hosts.toml"), hosts_toml(&[("a", &a.url), ("b", &b.url)])).unwrap();
    c.add_recipe("tiny", &weights_recipe("tiny", root, free_port()));
    (a, b, c)
}

/// With no `--on`, a model whose weights are on another host runs there: the
/// plan says so, the start lands on that host, and the stop finds it.
#[test]
fn use_without_on_runs_the_model_where_its_weights_are() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (a, _b, c) = controller_with_weights_on_a(root);

    let plan = c.run(&["use", "tiny", "--dry-run"]);
    assert!(plan.contains("tiny on a"), "the plan runs it on a, where its weights are:\n{plan}");
    let up = c.run(&["use", "tiny"]);
    assert!(up.contains("tiny is up on a"), "{up}");
    wait_loaded(&a.url, "tiny", 60);
    let down = c.run(&["stop", "tiny"]);
    assert!(down.contains("a:"), "the stop reaches a:\n{down}");
}

/// When this machine holds the weights too, the model runs here.
#[test]
fn use_runs_here_when_this_machine_has_the_weights_too() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (_a, _b, c) = controller_with_weights_on_a(root);
    give_weights(&c);

    let plan = c.run(&["use", "tiny", "--dry-run"]);
    assert!(!plan.contains("tiny on a") && !plan.contains("tiny on b"), "it stays here:\n{plan}");
    let up = c.run(&["use", "tiny"]);
    assert!(!up.contains(" on a"), "{up}");
    c.run(&["stop", "tiny"]);
}

/// When no host holds the weights, nothing moves: the plan stays here, as before.
#[test]
fn use_stays_here_when_no_host_has_the_weights() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let b = serve(Host::new(root, "sb", "[serve]\naccept_recipes = true\n"), &[]);
    let c = Host::new(root, "ctl", "");
    fs::write(c.dir.join("home/config/hosts.toml"), hosts_toml(&[("b", &b.url)])).unwrap();
    c.add_recipe("tiny", &weights_recipe("tiny", root, free_port()));

    let plan = c.run(&["use", "tiny", "--dry-run"]);
    assert!(!plan.contains("tiny on b"), "no host has them, so it stays here:\n{plan}");
}

/// `--on local`, or this machine's own name, runs the model here though its
/// weights are on a: the plan is this machine's and says why, the start comes
/// up here, and a start without `--on` then finds it up here instead of
/// loading a second copy on a.
#[test]
fn use_on_local_runs_it_here_anyway() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (a, _b, c) = controller_with_weights_on_a(root);

    for on in ["local", "ctl"] {
        let plan = c.run(&["use", "tiny", "--on", on, "--dry-run"]);
        assert!(!plan.contains("tiny on a") && plan.contains("run:  python3"), "--on {on} plans it here:\n{plan}");
        assert!(plan.contains("where: here, as --on says (weights on a, not here)"), "{plan}");
    }
    let up = c.run(&["use", "tiny", "--on", "local"]);
    assert!(up.contains("tiny is up at http://localhost:"), "{up}");
    let again = c.run(&["use", "tiny"]);
    assert!(again.contains("tiny is already up") && again.contains("it runs here already"), "{again}");
    let on_a: serde_json::Value = ureq::get(&format!("{}/api/status", a.url)).call().unwrap().into_json().unwrap();
    assert!(!on_a.to_string().contains("tiny"), "nothing started on a: {on_a}");
    assert!(c.run(&["stop", "tiny"]).contains("stopped tiny"));
}

/// The dashboard's Start names no host and asks for the rule (`"auto": true`):
/// the model runs on a, where its weights are, and the dashboard's Stop finds
/// it there. A start without it, which is how a controller starts a recipe on
/// the host it picked, still runs here.
#[test]
fn the_dashboards_start_runs_the_model_where_its_weights_are() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (a, _b, c) = controller_with_weights_on_a(root);
    let ctl = serve(c, &[]);
    let post = |path: &str, body: serde_json::Value| -> serde_json::Value {
        let r = ureq::post(&format!("{}/api/{path}", ctl.url)).set("X-Lmdeck", "1").send_json(body);
        r.unwrap_or_else(|e| panic!("POST {path}: {e}")).into_json().unwrap()
    };

    let here = post("start/tiny", json!({}));
    assert_eq!(here["ok"], true, "{here}");
    wait_loaded(&ctl.url, "tiny", 60);
    post("stop/tiny", json!({}));

    let moved = post("start/tiny", json!({"auto": true}));
    let msg = moved["message"].as_str().unwrap_or_default();
    assert!(msg.starts_with("on a (weights on a, not here): "), "{moved}");
    wait_loaded(&a.url, "tiny", 60);
    let down = post("stop/tiny", json!({}));
    assert!(down["message"].as_str().unwrap_or_default().starts_with("a: stopped tiny"), "the stop reaches a: {down}");
    let st: serde_json::Value = ureq::get(&format!("{}/api/status", a.url)).call().unwrap().into_json().unwrap();
    let up = st.as_array().unwrap().iter().any(|r| r["name"] == "tiny" && r["status"] == "loaded");
    assert!(!up, "tiny is down on a: {st}");
}

/// Weights on two other hosts: a start goes to the weights home, the first of
/// them by name whatever order hosts.toml lists them in, every time; `[model]
/// home` picks the other.
#[test]
fn weights_on_two_hosts_go_to_the_weights_home() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let holder = |dir: &str| {
        let h = Host::new(root, dir, "[serve]\naccept_recipes = true\n");
        give_weights(&h);
        serve(h, &[])
    };
    let (b, a) = (holder("sb"), holder("sa"));
    let c = Host::new(root, "ctl", "");
    fs::write(c.dir.join("home/config/hosts.toml"), hosts_toml(&[("b", &b.url), ("a", &a.url)])).unwrap();
    c.add_recipe("tiny", &weights_recipe("tiny", root, free_port()));
    for _ in 0..2 {
        let plan = c.run(&["use", "tiny", "--dry-run"]);
        assert!(plan.contains("tiny on a\n") && plan.contains("why: weights on a, not here"), "{plan}");
    }
    let homed = weights_recipe("homed", root, free_port()).replace("[model]\n", "[model]\nhome = \"b\"\n");
    c.add_recipe("homed", &homed);
    let plan = c.run(&["use", "homed", "--dry-run"]);
    assert!(plan.contains("homed on b\n"), "{plan}");
}

/// A weights index holding `REPO` on each of `holders` (`(host, online)`).
fn index_of(holders: &[(&str, bool)]) -> Index {
    let mut idx = Index::default();
    for (host, online) in holders {
        let item = Item {
            id: REPO.into(),
            location: "hf-cache".into(),
            kind: "hf".into(),
            path: format!("/hf/{host}/models--lmdeck-test--tiny-model"),
            size: 1002,
            revisions: vec![],
            ggufs: vec![],
        };
        let inventory = Inventory { host: host.to_string(), items: vec![item], ..Default::default() };
        idx.hosts.insert(
            host.to_string(),
            HostWeights { online: *online, inventory: Some(inventory), ..Default::default() },
        );
    }
    idx
}

/// The pick from a weights index: here when this machine holds the weights,
/// the weights home when hosts.toml lists it, here otherwise; each with why.
#[test]
fn the_pick_follows_the_weights_home() {
    let mut hosts = HostsFile::default();
    for h in ["a", "b"] {
        hosts.add(h, "http://127.0.0.1:9", None).unwrap();
    }
    let body =
        |model: &str| format!("[model]\nrepo = \"{REPO}\"\n{model}[engine]\nname = \"custom\"\ncommand = [\"true\"]\n");
    let tiny = Recipe::parse(&body(""), "tiny").unwrap();
    let pick = |idx: &Index, r: &Recipe| {
        let p = Pick::from_index(idx, &hosts, r, "ctl");
        (p.host, p.why)
    };
    let both = index_of(&[("b", true), ("a", true)]);
    assert_eq!(pick(&both, &tiny), ("a".into(), "weights on a, not here".into()), "the first by name");
    let homed = Recipe::parse(&body("home = \"b\"\n"), "tiny").unwrap();
    assert_eq!(pick(&both, &homed).0, "b");
    let mine = Recipe::parse(&body("home = \"ctl\"\n"), "tiny").unwrap();
    assert_eq!(pick(&both, &mine), ("local".into(), "[model] home ctl doesn't hold the weights".into()));
    assert_eq!(pick(&index_of(&[("local", true), ("a", true)]), &tiny), ("local".into(), "weights here".into()));
    assert_eq!(pick(&index_of(&[("x", true)]), &tiny).1, "weights on x, which hosts.toml doesn't list");
    assert_eq!(pick(&index_of(&[]), &tiny), ("local".into(), "no known host has the weights".into()));
    let quiet = pick(&index_of(&[("a", false)]), &tiny);
    assert_eq!(quiet, ("a".into(), "weights on a, not here; a didn't answer just now".into()));
}

const DSV41: &str = "deepseek-v4.1-flash-1017-4x-spark-tensorfold";
const DSV41_REPO: &str = "deepseek-ai/DeepSeek-V4.1-Flash-1017";
const DSV41_REV: &str = "1bae0e1088993b9b82de08f59de5be5a34e42170";

/// The built-in DeepSeek 4x recipe names the checkpoint Mia's scripts load,
/// at its revision, with `pull = false`: lmdeck picks the Spark holding it
/// (ember, not spark) and never downloads it (`pull` updates the checkout only).
#[test]
fn the_deepseek_builtin_names_its_checkpoint_and_never_pulls_it() {
    let r = lmdeck::catalog::builtin().into_iter().find(|f| f.recipe.name == DSV41).unwrap().recipe;
    assert_eq!(r.model.repo.as_deref(), Some(DSV41_REPO));
    assert_eq!(r.model.revision.as_deref(), Some(DSV41_REV));
    assert!(r.model.fetched_by_scripts());

    let mut hosts = HostsFile::default();
    for h in ["ember", "gigabyte", "faraday"] {
        hosts.add(h, "http://127.0.0.1:9", None).unwrap();
    }
    let mut idx = index_of(&[]);
    let item = Item {
        id: DSV41_REPO.into(),
        location: "hf-cache".into(),
        kind: "hf".into(),
        path: "/home/u/.cache/huggingface/hub/models--deepseek-ai--DeepSeek-V4.1-Flash-1017".into(),
        size: 510_000_000_000,
        revisions: vec![lmdeck::weights::Revision { commit: DSV41_REV.into(), refs: vec!["main".into()] }],
        ggufs: vec![],
    };
    let inventory = Inventory { host: "ember".into(), items: vec![item], ..Default::default() };
    idx.hosts.insert("ember".into(), HostWeights { online: true, inventory: Some(inventory), ..Default::default() });
    let p = Pick::from_index(&idx, &hosts, &r, "spark");
    assert_eq!((p.host.as_str(), p.why.as_str()), ("ember", "weights on ember, not here"));

    let tmp = tempfile::tempdir().unwrap();
    let spark = Host::new(tmp.path(), "spark", "");
    let pull = spark.run(&["pull", DSV41, "--dry-run"]);
    assert!(pull.contains("git clone") && !pull.contains(DSV41_REPO), "only the checkout:\n{pull}");
}

/// From a controller whose Sparks are cabled in a ring (a-b-c-d-a), the
/// built-in DeepSeek 4x recipe started without --on goes to the Spark holding
/// its checkpoint (a), with its three peers picked around a by the controller
/// and the ring's launcher. An explicit --on to a Spark without the checkpoint
/// still plans, as before the recipe named it, and warns that the scripts
/// fetch their own copy there.
#[test]
fn the_deepseek_builtin_plans_on_the_spark_holding_its_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let names = ["a", "b", "c", "d"];
    let ring: Vec<Served> = names
        .iter()
        .map(|n| {
            let h = Host::new(root, &format!("s{n}"), "[serve]\naccept_recipes = true\n");
            if *n == "a" {
                fake_hf(&h.dir.join("hf"), DSV41_REPO, DSV41_REV, &[("config.json", 2)]);
            }
            serve(h, &[])
        })
        .collect();
    let hosts: Vec<(&str, &str)> = names.iter().zip(&ring).map(|(n, s)| (*n, s.url.as_str())).collect();
    let links: String = (0..4)
        .map(|i| {
            format!(
                "[[link]]\na = \"{}\"\nb = \"{}\"\na_addr = \"10.99.{n}.1\"\nb_addr = \"10.99.{n}.2\"\nkind = \"cx7\"\n",
                names[i],
                names[(i + 1) % 4],
                n = i + 1
            )
        })
        .collect();
    let c = Host::new(root, "ctl", "");
    fs::write(c.dir.join("home/config/hosts.toml"), format!("{}{links}", hosts_toml(&hosts))).unwrap();

    let plan = c.run(&["use", DSV41, "--dry-run"]);
    for want in [
        format!("{DSV41} on a\n"),
        "why: weights on a, not here".into(),
        "weights: on the run host's disk".into(),
        "peers: 1. b 10.99.1.2".into(),
        "so a runs ./start-switchless.sh".into(),
    ] {
        assert!(plan.contains(&want), "missing {want:?} in\n{plan}");
    }
    let plan = c.run(&["use", DSV41, "--on", "b", "--dry-run"]);
    for want in [
        "weights: on a, not mounted (the recipe's own scripts fetch them)",
        "warning: b doesn't hold the weights (a has them",
    ] {
        assert!(plan.contains(want), "missing {want:?} in\n{plan}");
    }
}
