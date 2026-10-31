//! Several `lmdeck serve`s on one machine, each with its own LMDECK_HOME,
//! standing in for separate hosts: a recipe with peers started on another
//! host (`--on`) takes them from the controller, which holds the host list
//! and the links (and runs the launcher for the ring the controller sees),
//! and an upload named like a built-in keeps its log and its stop by name.

mod common;

use common::*;
use std::fs;
use std::path::Path;
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

/// A recipe with peers and a ring launcher (`command_by_fabric`), started on
/// another host, runs the launcher for the ring the controller sees, though
/// the run host has no hosts.toml and on its own sees itself alone
/// (`single`). Without peers a recipe goes as written, and the run host
/// picks its own launcher, as before.
#[test]
fn a_start_on_another_host_runs_the_launcher_for_the_controllers_ring() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let names = ["a", "b", "c", "d"];
    let ring: Vec<Served> = names
        .iter()
        .map(|n| {
            let config = if *n == "a" { "[serve]\naccept_recipes = true\n" } else { "" };
            serve(Host::new(root, &format!("s{n}"), config), &[])
        })
        .collect();
    assert!(!ring[0].host.dir.join("home/config/hosts.toml").exists(), "a has no host list");
    // Cabled a-b-c-d-a, the cable after each host on 10.99.<its place>.0.
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
    // Each launcher writes its name and WORKER to `out`, then serves HTTP; `extra` is more TOML.
    let recipe = |name: &str, extra: &str, out: &Path| {
        let launcher = |tag: &str| {
            format!(
                "[\"sh\", \"-c\", \"echo {tag} $WORKER > {} && exec python3 -u -m http.server {{port}} --bind 127.0.0.1 --directory {}\"]",
                out.display(),
                www(root).display()
            )
        };
        format!(
            "name = \"{name}\"\n[engine]\nname = \"custom\"\nport = {}\nready_timeout = 60\ncommand = {}\n\
             [engine.command_by_fabric]\nring = {}\n{extra}",
            free_port(),
            launcher("single"),
            launcher("ring")
        )
    };
    let (out, plain_out) = (root.join("ringy.txt"), root.join("plain.txt"));
    c.add_recipe("ringy", &recipe("ringy", "[engine.env]\nWORKER = \"{peer_addr}\"\n[hardware]\nhosts = 2\n", &out));
    c.add_recipe("plain", &recipe("plain", "", &plain_out));

    let plan = c.run(&["use", "ringy", "--on", "a", "--dry-run"]);
    for want in ["env:  WORKER=10.99.1.2", "fabric: ring", "so a runs sh -c", "echo ring"] {
        assert!(plan.contains(want), "missing {want:?} in\n{plan}");
    }
    let up = c.run(&["use", "ringy", "--on", "a"]);
    assert!(up.contains("ringy is up on a"), "{up}");
    // a's next cable neighbour is b, at 10.99.1.2 on their cable.
    assert_eq!(fs::read_to_string(&out).unwrap_or_default().trim(), "ring 10.99.1.2", "{up}");
    c.run(&["stop", "ringy", "--on", "a"]);

    let up = c.run(&["use", "plain", "--on", "a"]);
    assert!(up.contains("plain is up on a"), "{up}");
    assert_eq!(fs::read_to_string(&plain_out).unwrap_or_default().trim(), "single", "{up}");
    c.run(&["stop", "plain", "--on", "a"]);
}
