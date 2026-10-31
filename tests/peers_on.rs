//! Several `lmdeck serve`s on one machine, each with its own LMDECK_HOME,
//! standing in for separate hosts: a recipe with peers started on another
//! host (`--on`) takes them from the controller, which holds the host list
//! and the links, and an upload named like a built-in keeps its log and its
//! stop by name.

mod common;

use common::*;
use std::fs;
use std::time::{Duration, Instant};

/// A recipe with peers started on another host takes them from the controller,
/// which holds the host list and the links, so the run host needs no
/// hosts.toml of its own: `{peer_addr}` arrives filled with the peer's address
/// on the run host's link, and the controller's plan shows it.
#[test]
fn a_start_on_another_host_takes_its_peers_from_the_controller() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (_a, _b, c) = controller_with_a_linked_to_b(root);
    let out = root.join("worker.txt");
    c.add_recipe("pair", &peer_recipe("pair", root, free_port(), 2, &out));

    let plan = c.run(&["use", "pair", "--on", "a", "--dry-run"]);
    assert!(plan.contains("WORKER=10.99.0.2"), "the plan shows the peer a gets:\n{plan}");
    let up = c.run(&["use", "pair", "--on", "a"]);
    assert!(up.contains("pair is up on a"), "{up}");
    assert_eq!(fs::read_to_string(&out).unwrap_or_default().trim(), "10.99.0.2", "{up}");
    c.run(&["stop", "pair", "--on", "a"]);
}

/// Too few Sparks around the run host for a recipe's peers: the controller
/// says how many it needs, and nothing starts on the run host.
#[test]
fn too_few_peers_around_the_run_host_stop_the_start_on_the_controller() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (_a, _b, c) = controller_with_a_linked_to_b(root);
    let out = root.join("worker.txt");
    c.add_recipe("trio", &peer_recipe("trio", root, free_port(), 3, &out));

    let err = c.fail(&["use", "trio", "--on", "a"]);
    assert!(err.contains("hosts = 3") && err.contains("2 more Spark"), "{err}");
    assert!(!out.exists(), "nothing started on a");
}

/// A run host with its own copy of a recipe with peers (as with a built-in
/// on both machines) still gets the controller's peers: the controller
/// uploads the resolved recipe instead of starting that copy by name, whose
/// peers the run host can't pick, and the run host fills only its own
/// placeholders (`{home}` here). A run host that takes no uploads refuses
/// it, and its own copy never starts.
#[test]
fn a_run_host_that_knows_a_recipe_still_takes_its_peers_from_the_controller() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (a, b, c) = controller_with_a_linked_to_b(root);
    let out = root.join("worker.txt");
    let pair = peer_recipe("pair", root, free_port(), 2, &out)
        .replace("WORKER = \"{peer_addr}\"", "WORKER = \"{peer_addr} {home}\"");
    for h in [&a.host, &b.host, &c] {
        h.add_recipe("pair", &pair);
    }

    let up = c.run(&["use", "pair", "--on", "a"]);
    assert!(up.contains("pair is up on a"), "{up}");
    let want = format!("10.99.0.2 {}", a.host.dir.join("home").display());
    assert_eq!(fs::read_to_string(&out).unwrap_or_default().trim(), want, "{up}");
    c.run(&["stop", "pair", "--on", "a"]);

    fs::remove_file(&out).unwrap();
    let err = c.fail(&["use", "pair", "--on", "b"]);
    assert!(err.contains("accept_recipes"), "{err}");
    assert!(!out.exists(), "b started its own copy");
}

/// A controller's upload named like a built-in (the built-in with its peers
/// filled in) sits beside the built-in on the run host. Its log and its stop
/// still go by that name after the run has exited, which is when the
/// controller fetches the log tail of a start that died.
#[test]
fn an_upload_named_like_a_built_in_keeps_its_log_and_stop_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let a = serve(Host::new(tmp.path(), "sa", "[serve]\naccept_recipes = true\n"), &[]);
    let name = "qwen3-4b-gguf";
    let recipe = format!(
        "name = \"{name}\"\n[engine]\nname = \"custom\"\nport = {}\ncommand = [\"sh\", \"-c\", \"echo dying; exit 3\"]\n",
        free_port()
    );
    let api = |path: &str| format!("{}/api/{path}", a.url);
    let up = ureq::post(&api("run")).set("X-Lmdeck", "1").send_json(serde_json::json!({"recipe": recipe}));
    assert_eq!(code(up), 200);
    // Asking for its status drops the run record once the run has exited.
    let t0 = Instant::now();
    loop {
        let st: serde_json::Value =
            ureq::get(&api(&format!("status?name={name}"))).call().unwrap().into_json().unwrap();
        if !matches!(st[0]["status"].as_str(), Some("loading" | "loaded")) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{st}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let logs = ureq::get(&api(&format!("logs/{name}"))).call().unwrap_or_else(|e| panic!("logs by name: {e}"));
    let logs: serde_json::Value = logs.into_json().unwrap();
    assert!(logs["lines"].to_string().contains("dying"), "{logs}");
    let Err(ureq::Error::Status(409, r)) = ureq::post(&api(&format!("stop/{name}"))).set("X-Lmdeck", "1").call() else {
        panic!("a stop of a run that has exited is refused");
    };
    let err = r.into_string().unwrap();
    assert!(err.contains("was not started"), "{err}");
}
