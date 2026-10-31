//! Several `lmdeck serve`s on one machine, each with its own LMDECK_HOME,
//! standing in for separate hosts, and the agent harnesses on each: a lane
//! per running model that chats (a multi-node one by its recipe's name), a
//! start or stop on another host, the hub's push to every host and what a
//! push needs, and the models a host keeps through a missed look.

mod common;

use common::*;
use std::fs;
use std::time::{Duration, Instant};

/// A multi-node recipe syncs to the agent harnesses as the model it serves
/// (its recipe name, `--served-model-name <recipe>`), not as lmdeck's
/// per-node names, and a worker, which serves no API, not at all.
#[test]
fn multi_node_lanes_are_the_recipe_not_its_nodes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = serve(Host::new(root, "sa", "[serve]\naccept_recipes = true\n"), &[]);
    let c = Host::new(root, "ctl", "");
    fs::create_dir_all(c.dir.join("home/.config/opencode")).unwrap();
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "[[host]]\nname = \"a\"\nurl = \"{}\"\n\n[[link]]\na = \"local\"\nb = \"a\"\na_addr = \"127.0.0.1\"\nb_addr = \"127.0.0.1\"\na_ifname = \"lo\"\nb_ifname = \"lo\"\nkind = \"cx7\"\n",
            a.url
        ),
    )
    .unwrap();
    let order = root.join("order.txt");
    // The head here, its worker on a.
    let (duo_port, rev_port) = (free_port(), free_port());
    c.add_recipe("duo", &cluster_recipe("duo", root, duo_port, free_port(), &order, false));
    // The other way around: the head on a, the worker here.
    let rev = cluster_recipe("rev", root, rev_port, free_port(), &order, false)
        .replace("host = \"local\"", "host = \"HEAD\"")
        .replace("host = \"a\"", "host = \"local\"")
        .replace("host = \"HEAD\"", "host = \"a\"");
    c.add_recipe("rev", &rev);
    c.run(&["use", "duo"]);
    c.run(&["use", "rev"]);
    assert_eq!(opencode_lanes(&c), ["duo", "rev"]);
    let text = fs::read_to_string(c.dir.join("home/.config/opencode/opencode.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(v["provider"]["duo"]["options"]["baseURL"].as_str().unwrap().ends_with(&format!(":{duo_port}/v1")), "{v}");
    assert_eq!(v["provider"]["rev"]["options"]["baseURL"], format!("http://127.0.0.1:{rev_port}/v1"), "{v}");
    assert!(v["provider"]["duo"]["models"].get("duo").is_some(), "{v}");
    // Stopped, the lane goes.
    c.run(&["stop", "duo"]);
    assert_eq!(opencode_lanes(&c), ["rev"]);
    c.run(&["stop", "rev"]);
}

/// A recipe that isn't a chat model (a decision or classifier server) stays
/// out of the agent harnesses with `[harness] chat = false`, whether it runs
/// here or on another host, and so does a multi-node one.
#[test]
fn recipes_that_dont_chat_stay_out_of_the_harnesses() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = serve(Host::new(root, "sa", "[serve]\naccept_recipes = true\n"), &[]);
    let c = Host::new(root, "ctl", "");
    fs::create_dir_all(c.dir.join("home/.config/opencode")).unwrap();
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "[[host]]\nname = \"a\"\nurl = \"{}\"\n\n[[link]]\na = \"local\"\nb = \"a\"\na_addr = \"127.0.0.1\"\nb_addr = \"127.0.0.1\"\nkind = \"cx7\"\n",
            a.url
        ),
    )
    .unwrap();
    let quiet = "\n[harness]\nchat = false\n";
    c.add_recipe("chat", &web_recipe("chat", root, free_port()));
    c.add_recipe("clef", &format!("{}{quiet}", web_recipe("clef", root, free_port())));
    c.add_recipe("clef2", &format!("{}{quiet}", web_recipe("clef2", root, free_port())));
    let order = root.join("order.txt");
    c.add_recipe(
        "judge",
        &format!("{}{quiet}", cluster_recipe("judge", root, free_port(), free_port(), &order, false)),
    );
    c.run(&["use", "chat"]);
    c.run(&["use", "clef"]);
    c.run(&["use", "clef2", "--on", "a"]);
    c.run(&["use", "judge"]);
    assert_eq!(opencode_lanes(&c), ["chat"]);
    // Only a boolean says it.
    let bad = root.join("bad.toml");
    fs::write(&bad, format!("{}\n[harness]\nchat = \"no\"\n", web_recipe("bad", root, free_port()))).unwrap();
    let err = c.fail(&["recipe", "add", bad.to_str().unwrap()]);
    assert!(err.contains("chat"), "{err}");
    c.run(&["stop", "judge"]);
    c.run(&["stop", "clef2"]);
    // Reason: chat and clef run here, outside any serve, so nothing else stops them.
    c.run(&["stop", "chat"]);
    c.run(&["stop", "clef"]);
}

/// A model this machine starts or stops on another host (`--on`) reaches this
/// machine's agent harnesses right away, as a local start or stop does, with
/// no `lmdeck harness sync` in between. Its base URL is the other host's.
#[test]
fn remote_start_and_stop_sync_the_controllers_harnesses() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let port = free_port();
    let a = Host::new(root, "sa", "");
    a.add_recipe("web", &web_recipe("web", root, port));
    let a = serve(a, &[]);
    let c = with_opencode(Host::new(root, "ctl", ""));
    c.add_recipe("web", &web_recipe("web", root, port));
    fs::write(c.dir.join("home/config/hosts.toml"), hosts_toml(&[("a", &a.url)])).unwrap();
    let home = c.dir.join("home");

    let up = c.run(&["use", "web", "--on", "a"]);
    assert!(up.contains("web is up on a"), "{up}");
    assert_eq!(opencode_providers(&home), ["web"], "after `use web --on a`:\n{up}");
    assert_eq!(opencode_base_url(&home, "web"), format!("http://127.0.0.1:{port}/v1"));

    let down = c.run(&["stop", "web", "--on", "a"]);
    assert!(opencode_providers(&home).is_empty(), "after `stop web --on a`:\n{down}");
}

/// The hub (the machine with the host list and `[harness] hub = true`) keeps
/// every host's agent harnesses in step with every model running anywhere. A
/// start or stop made on another host, not through the hub, reaches a third
/// host that has no host list of its own, and the hub itself.
/// `LMDECK_HARNESS_SYNC_SECS` sets how often the hub's serve looks.
#[test]
fn the_hub_pushes_every_running_model_to_every_hosts_harnesses() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let port = free_port();
    let a = Host::new(root, "sa", "");
    a.add_recipe("web", &web_recipe("web", root, port));
    let a = serve(with_opencode(a), &[]);
    let b = serve(with_opencode(Host::new(root, "sb", "")), &[]);
    let mut h = with_opencode(Host::new(root, "hub", "[harness]\nhub = true\n"));
    fs::write(h.dir.join("home/config/hosts.toml"), hosts_toml(&[("a", &a.url), ("b", &b.url)])).unwrap();
    h.extra_env = vec![("LMDECK_HARNESS_SYNC_SECS".into(), "1".into())];
    let h = serve(h, &[]);
    let homes = [a.host.dir.join("home"), b.host.dir.join("home"), h.host.dir.join("home")];

    // Started on a through a's own API: neither the hub nor b runs anything.
    let started = code(ureq::post(&format!("{}/api/start/web", a.url)).set("X-Lmdeck", "1").call());
    assert!((200..300).contains(&started), "start on a: {started}");
    for home in &homes {
        wait_for_providers(home, &["web"], 30);
    }
    // b and the hub reach it at a's address from the hub's host list.
    for home in &homes[1..] {
        assert_eq!(opencode_base_url(home, "web"), format!("http://127.0.0.1:{port}/v1"), "{}", home.display());
    }

    let stopped = code(ureq::post(&format!("{}/api/stop/web", a.url)).set("X-Lmdeck", "1").call());
    assert!((200..300).contains(&stopped), "stop on a: {stopped}");
    for home in &homes {
        wait_for_providers(home, &[], 30);
    }
}

/// A model the hub starts or stops on another host (`--on`) reaches every
/// host's harnesses right away: the hub pushes after the start and after the
/// stop, without waiting for its serve's next look (no hub serve runs here).
#[test]
fn a_start_from_the_hub_reaches_every_hosts_harnesses_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let port = free_port();
    let a = Host::new(root, "sa", "");
    a.add_recipe("web", &web_recipe("web", root, port));
    let a = serve(with_opencode(a), &[]);
    let b = serve(with_opencode(Host::new(root, "sb", "")), &[]);
    let h = with_opencode(Host::new(root, "hub", "[harness]\nhub = true\n"));
    h.add_recipe("web", &web_recipe("web", root, port));
    fs::write(h.dir.join("home/config/hosts.toml"), hosts_toml(&[("a", &a.url), ("b", &b.url)])).unwrap();
    let homes = [a.host.dir.join("home"), b.host.dir.join("home"), h.dir.join("home")];

    let up = h.run(&["use", "web", "--on", "a"]);
    assert!(up.contains("web is up on a"), "{up}");
    for home in &homes {
        assert_eq!(opencode_providers(home), ["web"], "{}: after `use web --on a`:\n{up}", home.display());
    }
    let down = h.run(&["stop", "web", "--on", "a"]);
    for home in &homes {
        assert!(opencode_providers(home).is_empty(), "{}: after `stop web --on a`:\n{down}", home.display());
    }
}

/// Returns one lane of a push body: model `name` served at `host:port`.
fn pushed_lane(name: &str, display: &str, host: &str, port: u16) -> serde_json::Value {
    serde_json::json!({"name": name, "display": display, "host": host, "port": port, "ctx": 65536,
                       "vision": false, "remote": true, "chat": true})
}

/// A push is a write: without the token it is refused (401, a wrong one
/// too), with the token but without `X-Lmdeck` as well (403), and a refused
/// push reaches neither the harnesses nor the stored push. One lane a config
/// couldn't hold refuses the whole push (400). The right request lands.
#[test]
fn a_push_needs_the_token_and_the_header() {
    let tmp = tempfile::tempdir().unwrap();
    let b = serve(with_opencode(Host::new(tmp.path(), "sb", "")), &["--token", "sekrit-b"]);
    let home = b.host.dir.join("home");
    let stored = home.join("state/harness-lanes.json");
    let url = format!("{}/api/harness/lanes", b.url);
    let body = |lanes: serde_json::Value| serde_json::json!({"from": "hub", "lanes": lanes});
    let good = body(serde_json::json!([pushed_lane("glm", "GLM", "10.0.0.7", 8001)]));

    assert_eq!(code(ureq::post(&url).set("X-Lmdeck", "1").send_json(good.clone())), 401, "no token");
    let wrong = ureq::post(&url).set("X-Lmdeck", "1").set("Authorization", "Bearer nope").send_json(good.clone());
    assert_eq!(code(wrong), 401, "a wrong token");
    let bare = ureq::post(&url).set("Authorization", "Bearer sekrit-b").send_json(good.clone());
    assert_eq!(code(bare), 403, "no X-Lmdeck header");
    let quoted = body(serde_json::json!([
        pushed_lane("glm", "GLM", "10.0.0.7", 8001),
        pushed_lane("evil", "x\"\n[model.evil]", "10.0.0.7", 8002)
    ]));
    let r = ureq::post(&url).set("X-Lmdeck", "1").set("Authorization", "Bearer sekrit-b").send_json(quoted);
    assert_eq!(code(r), 400, "a display name grok's config can't hold");
    assert!(!stored.exists(), "a refused push was stored");
    assert!(opencode_providers(&home).is_empty(), "a refused push reached opencode");

    let ok = ureq::post(&url).set("X-Lmdeck", "1").set("Authorization", "Bearer sekrit-b").send_json(good);
    assert_eq!(code(ok), 200);
    assert!(stored.exists());
    assert_eq!(opencode_providers(&home), ["glm"]);
    assert_eq!(opencode_base_url(&home, "glm"), "http://10.0.0.7:8001/v1");
}

/// What the hub pushed survives this machine's own syncs: `lmdeck harness
/// sync`, a local start and a local stop keep the pushed lanes; a local model
/// the hub didn't list joins them; a pushed lane wins over a local one of the
/// same name (it carries the address from the hub's host list).
#[test]
fn a_local_sync_keeps_what_the_hub_pushed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (pw, ps) = (free_port(), free_port());
    let b = with_opencode(Host::new(root, "sb", ""));
    b.add_recipe("web", &web_recipe("web", root, pw));
    b.add_recipe("solo", &web_recipe("solo", root, ps));
    let b = serve(b, &[]);
    let home = b.host.dir.join("home");
    let lanes =
        serde_json::json!([pushed_lane("glm", "GLM", "10.0.0.7", 8001), pushed_lane("web", "web", "10.0.0.8", pw)]);
    let pushed = ureq::post(&format!("{}/api/harness/lanes", b.url))
        .set("X-Lmdeck", "1")
        .send_json(serde_json::json!({"from": "hub", "lanes": lanes}));
    assert_eq!(code(pushed), 200);
    assert_eq!(opencode_providers(&home), ["glm", "web"]);

    b.host.run(&["harness", "sync"]);
    assert_eq!(opencode_providers(&home), ["glm", "web"], "`harness sync` wiped the push");
    let up = b.host.run(&["use", "web"]);
    let up2 = b.host.run(&["use", "solo"]);
    assert_eq!(opencode_providers(&home), ["glm", "web", "solo"], "{up}\n{up2}");
    assert_eq!(opencode_base_url(&home, "web"), format!("http://10.0.0.8:{pw}/v1"), "the pushed lane wins");
    b.host.run(&["stop", "solo"]);
    assert_eq!(opencode_providers(&home), ["glm", "web"], "a local stop wiped the push");
    assert!(b.host.run(&["harness"]).contains("pushed here: 2 model(s) from hub"));
}

/// The hub keeps a host's models through a missed look whatever triggers the
/// push: a `harness sync` (as a start or a stop) while that host doesn't
/// answer keeps its last-known models in every harness, and the third miss in
/// a row drops them, the same rule the serve's poller follows.
#[test]
fn a_host_that_misses_a_push_keeps_its_models_for_two_more() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = Host::new(root, "sa", "");
    a.add_recipe("web", &web_recipe("web", root, free_port()));
    let mut a = serve(a, &[]);
    let b = serve(with_opencode(Host::new(root, "sb", "")), &[]);
    let h = with_opencode(Host::new(root, "hub", "[harness]\nhub = true\n"));
    fs::write(h.dir.join("home/config/hosts.toml"), hosts_toml(&[("a", &a.url), ("b", &b.url)])).unwrap();
    let home = b.host.dir.join("home");

    let started = code(ureq::post(&format!("{}/api/start/web", a.url)).set("X-Lmdeck", "1").call());
    assert!((200..300).contains(&started), "start on a: {started}");
    wait_loaded(&a.url, "web", 60);
    h.run(&["harness", "sync"]);
    assert_eq!(opencode_providers(&home), ["web"], "pushed while a answers");

    // a's serve goes away; its model doesn't matter, only that a can't be asked.
    let _ = a.child.kill();
    let _ = a.child.wait();
    h.run(&["harness", "sync"]);
    assert_eq!(opencode_providers(&home), ["web"], "after a missed one push");
    h.run(&["harness", "sync"]);
    assert_eq!(opencode_providers(&home), ["web"], "after a missed two pushes");
    h.run(&["harness", "sync"]);
    assert!(opencode_providers(&home).is_empty(), "after a missed three pushes in a row");
}

/// The hub's serve starts from the hub's record: on its first look after a
/// (re)start, a host that doesn't answer keeps the models the last push saw
/// on it, and every host gets them.
#[test]
fn the_hubs_first_look_after_a_restart_keeps_a_host_that_doesnt_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = Host::new(root, "sa", "");
    a.add_recipe("web", &web_recipe("web", root, free_port()));
    let mut a = serve(a, &[]);
    let b = serve(Host::new(root, "sb", ""), &[]);
    let mut h = Host::new(root, "hub", "[harness]\nhub = true\n");
    fs::write(h.dir.join("home/config/hosts.toml"), hosts_toml(&[("a", &a.url), ("b", &b.url)])).unwrap();
    let started = code(ureq::post(&format!("{}/api/start/web", a.url)).set("X-Lmdeck", "1").call());
    assert!((200..300).contains(&started), "start on a: {started}");
    wait_loaded(&a.url, "web", 60);
    h.run(&["harness", "sync"]);
    let _ = a.child.kill();
    let _ = a.child.wait();
    // b forgets the push, so what it stores next is what the serve's first look pushed.
    let stored = b.host.dir.join("home/state/harness-lanes.json");
    fs::remove_file(&stored).unwrap();
    // Reason: an hour between looks leaves the first look the only one this test sees.
    h.extra_env = vec![("LMDECK_HARNESS_SYNC_SECS".into(), "3600".into())];
    let _h = serve(h, &[]);
    let t0 = Instant::now();
    while !stored.exists() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the hub's serve never pushed to b");
        std::thread::sleep(Duration::from_millis(100));
    }
    let pushed: serde_json::Value = serde_json::from_str(&fs::read_to_string(&stored).unwrap()).unwrap();
    let names: Vec<&str> = pushed["lanes"].as_array().unwrap().iter().filter_map(|l| l["name"].as_str()).collect();
    assert_eq!(names, ["web"], "the first look dropped a's model: {pushed}");
}
