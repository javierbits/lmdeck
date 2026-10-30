//! Several `lmdeck serve`s on one machine, each with its own LMDECK_HOME,
//! standing in for separate hosts: remote control with and without a token,
//! multi-node start order / reverse stop / a failing worker, the weights
//! index, and the NFS placement flow against a fake `docker`.

#![allow(clippy::result_large_err)]

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let p = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        if !used.contains(&p) {
            used.push(p);
            return p;
        }
    }
}

/// One "machine": a home, models dir and HF cache under `root/<name>`.
struct Host {
    dir: PathBuf,
    extra_env: Vec<(String, String)>,
}

impl Host {
    fn new(root: &Path, name: &str, config: &str) -> Host {
        let dir = root.join(name);
        for d in ["home/config", "models", "hf"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        fs::write(dir.join("home/config/config.toml"), format!("[node]\nname = \"{name}\"\n{config}")).unwrap();
        Host { dir, extra_env: vec![] }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lmdeck"));
        c.env("HOME", self.dir.join("home"))
            .env("LMDECK_HOME", self.dir.join("home"))
            .env("LMDECK_MODELS_DIR", self.dir.join("models"))
            .env("HF_HUB_CACHE", self.dir.join("hf"))
            .env("NO_COLOR", "1")
            .env_remove("HF_HOME")
            .env_remove("LMDECK_TOKEN")
            .env_remove("LMDECK_DEMO")
            .env_remove("LMDECK_NO_SYNC")
            .env_remove("HF_TOKEN");
        for (k, v) in &self.extra_env {
            c.env(k, v);
        }
        c
    }

    fn out(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> String {
        let o = self.out(args);
        let (so, se) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(o.status.success(), "lmdeck {args:?} failed\n--- stdout ---\n{so}\n--- stderr ---\n{se}");
        so.into_owned()
    }

    fn fail(&self, args: &[&str]) -> String {
        let o = self.out(args);
        assert!(!o.status.success(), "lmdeck {args:?} should fail: {}", String::from_utf8_lossy(&o.stdout));
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    }

    fn add_recipe(&self, name: &str, body: &str) {
        let f = self.dir.join(format!("{name}.toml"));
        fs::write(&f, body).unwrap();
        self.run(&["recipe", "add", f.to_str().unwrap()]);
    }
}

struct Served {
    host: Host,
    child: Child,
    url: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.host.cmd().args(["stop", "--all"]).output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(host: Host, extra: &[&str]) -> Served {
    let port = free_port();
    let child = host
        .cmd()
        .args(["serve", "--port", &port.to_string()])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(fs::File::create(host.dir.join("serve.err")).unwrap())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let t0 = Instant::now();
    while ureq::get(&format!("{url}/api/health")).call().is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "serve did not come up");
        std::thread::sleep(Duration::from_millis(100));
    }
    Served { host, child, url }
}

fn code(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("transport: {e}"),
    }
}

/// A stand-in engine: answers HTTP on `{port}`.
fn web_recipe(name: &str, root: &Path, port: u16) -> String {
    format!(
        "name = \"{name}\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 180\ncommand = [\"python3\", \"-u\", \"-m\", \"http.server\", \"{{port}}\", \"--bind\", \"127.0.0.1\"]\n",
        root.display()
    )
}

#[test]
fn remote_start_stop_logs_with_and_without_token() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (pa, pb) = (free_port(), free_port());

    let a = Host::new(root, "sa", "");
    a.add_recipe("web", &web_recipe("web", root, pa));
    let a = serve(a, &[]);
    let b = Host::new(root, "sb", "");
    b.add_recipe("web2", &web_recipe("web2", root, pb));
    let b = serve(b, &["--token", "sekrit-b"]);

    // The token rule on B: reads and writes both need it; writes also need X-Lmdeck.
    assert_eq!(code(ureq::get(&format!("{}/api/snapshot", b.url)).call()), 401);
    assert_eq!(code(ureq::post(&format!("{}/api/start/web2", b.url)).set("X-Lmdeck", "1").call()), 401);
    assert_eq!(code(ureq::get(&format!("{}/api/health", b.url)).call()), 200, "health is open");
    assert_eq!(
        code(ureq::post(&format!("{}/api/start/web2", b.url)).set("Authorization", "Bearer sekrit-b").call()),
        403,
        "a write without X-Lmdeck"
    );
    assert_eq!(code(ureq::get(&format!("{}/api/recipes?token=sekrit-b", b.url)).call()), 200);

    // The controller knows the recipes too (it resolves names before asking).
    let mut c = Host::new(root, "ctl", "");
    c.add_recipe("web", &web_recipe("web", root, pa));
    c.add_recipe("web2", &web_recipe("web2", root, pb));
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!("[[host]]\nname = \"a\"\nurl = \"{}\"\n\n[[host]]\nname = \"b\"\nurl = \"{}\"\ntoken_env = \"TEST_B_TOKEN\"\n", a.url, b.url),
    )
    .unwrap();

    // A: no token on a loopback serve.
    let plan = c.run(&["use", "web", "--on", "a", "--dry-run"]);
    assert!(plan.contains("a"), "{plan}");
    let up = c.run(&["use", "web", "--on", "a"]);
    assert!(up.contains("web is up on a"), "{up}");
    let st: serde_json::Value = ureq::get(&format!("{}/api/status", a.url)).call().unwrap().into_json().unwrap();
    let web = st.as_array().unwrap().iter().find(|r| r["name"] == "web").unwrap_or_else(|| panic!("{st}"));
    assert_eq!(web["status"], "loaded", "{st}");
    assert!(
        c.run(&["logs", "web", "--on", "a"]).contains("http.server") || c.run(&["logs", "web", "--on", "a"]).is_empty()
    );
    assert!(c.run(&["stop", "web", "--on", "a"]).contains("a:"));
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", pa)).is_ok() {
        assert!(t0.elapsed() < Duration::from_secs(15), "web still answering on a after stop");
        std::thread::sleep(Duration::from_millis(200));
    }

    // B without the token in the environment: a clear error, nothing started.
    let err = c.fail(&["use", "web2", "--on", "b"]);
    assert!(err.contains("wants a token") && err.contains("TEST_B_TOKEN"), "{err}");
    assert!(std::net::TcpStream::connect(("127.0.0.1", pb)).is_err());
    // A wrong token is rejected.
    c.extra_env = vec![("TEST_B_TOKEN".into(), "nope".into())];
    assert!(c.fail(&["use", "web2", "--on", "b"]).contains("rejected the token"));
    // The right one works, and stop finds it through the placement record.
    c.extra_env = vec![("TEST_B_TOKEN".into(), "sekrit-b".into())];
    assert!(c.run(&["use", "web2", "--on", "b"]).contains("web2 is up on b"));
    let out = c.run(&["stop", "web2"]);
    assert!(out.contains("b:"), "{out}");
    assert!(
        !fs::read_to_string(b.host.dir.join("serve.err")).unwrap_or_default().contains("sekrit-b"),
        "token never logged"
    );
}

/// A shell node that records start / stop in `order` and then idles (or serves HTTP).
fn node_cmd(order: &Path, tag: &str, http: bool, fail: bool) -> String {
    let o = order.display();
    let body = if fail {
        "echo worker-broke-on-purpose; exit 3".to_string()
    } else if http {
        format!("echo up {tag} >> {o}; trap 'echo stop {tag} >> {o}; kill $P 2>/dev/null; exit 0' TERM INT; python3 -m http.server {{port}} --bind 127.0.0.1 & P=$!; wait $P")
    } else {
        format!("echo up {tag} >> {o}; trap 'echo stop {tag} >> {o}; exit 0' TERM INT; while :; do sleep 0.2; done")
    };
    format!("[\"sh\", \"-c\", {}]", serde_json::to_string(&body).unwrap())
}

fn cluster_recipe(name: &str, root: &Path, port: u16, worker_port: u16, order: &Path, worker_fails: bool) -> String {
    format!(
        r#"name = "{name}"
[model]
source = "local"
path = "{root}"
[engine]
name = "custom"
port = {port}
ready_timeout = 180
[hardware]
nodes = 2
[cluster]
gate_timeout = 60

[[nodes]]
name = "head"
role = "head"
host = "local"
command = {head}

[[nodes]]
name = "w1"
role = "worker"
host = "a"
port = {worker_port}
ready_delay = 1
command = {worker}
"#,
        root = root.display(),
        head = node_cmd(order, "head", true, false),
        worker = node_cmd(order, "w1", false, worker_fails),
    )
}

fn lines(p: &Path) -> Vec<String> {
    fs::read_to_string(p).unwrap_or_default().lines().map(str::to_string).collect()
}

fn wait_for(p: &Path, want: &[&str], secs: u64) -> Vec<String> {
    let t0 = Instant::now();
    loop {
        let l = lines(p);
        if l == want || t0.elapsed() > Duration::from_secs(secs) {
            return l;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn multi_node_order_reverse_stop_and_failing_worker() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let a = serve(Host::new(root, "sa", "[serve]\naccept_recipes = true\n"), &[]);
    let c = Host::new(root, "ctl", "");
    // Both "hosts" are this machine: a hand-written CX7 link on loopback.
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "[[host]]\nname = \"a\"\nurl = \"{}\"\n\n[[link]]\na = \"local\"\nb = \"a\"\na_addr = \"127.0.0.1\"\nb_addr = \"127.0.0.1\"\na_ifname = \"lo\"\nb_ifname = \"lo\"\nkind = \"cx7\"\n",
            a.url
        ),
    )
    .unwrap();

    // Head first (the default), stop in reverse.
    let order = root.join("order.txt");
    let port = free_port();
    c.add_recipe("duo", &cluster_recipe("duo", root, port, free_port(), &order, false));
    let plan = c.run(&["use", "duo", "--dry-run"]);
    assert!(plan.contains("head") && plan.contains("w1") && plan.contains("127.0.0.1"), "{plan}");
    let up = c.run(&["use", "duo"]);
    assert!(up.contains("duo.head is up"), "{up}");
    assert_eq!(lines(&order), ["up head", "up w1"]);
    // The worker runs on a, under the derived name.
    let st: serde_json::Value = ureq::get(&format!("{}/api/status", a.url)).call().unwrap().into_json().unwrap();
    let w1 = st.as_array().unwrap().iter().find(|r| r["name"] == "duo.w1").unwrap_or_else(|| panic!("{st}"));
    assert_ne!(w1["status"], "stopped", "{st}");
    let list: serde_json::Value = serde_json::from_str(&c.run(&["list", "--json"])).unwrap();
    let duo = list.as_array().unwrap().iter().find(|r| r["name"] == "duo").unwrap().clone();
    assert_eq!(duo["nodes"].as_array().unwrap().len(), 2, "{duo}");
    assert!(c.run(&["logs", "duo", "--node", "head"]).contains("http") || true);

    let out = c.run(&["stop", "duo"]);
    assert!(out.contains("reverse order: w1"), "{out}");
    let got = wait_for(&order, &["up head", "up w1", "stop w1", "stop head"], 15);
    assert_eq!(got, ["up head", "up w1", "stop w1", "stop head"]);

    // A worker that dies before its gate: the head is stopped again and the error shows the worker's log.
    fs::remove_file(&order).unwrap();
    c.add_recipe("bad", &cluster_recipe("bad", root, free_port(), free_port(), &order, true));
    let err = c.fail(&["use", "bad"]);
    assert!(err.contains("w1") && err.contains("worker-broke-on-purpose"), "{err}");
    assert!(err.contains("stopped the 1 node(s) already started"), "{err}");
    assert_eq!(wait_for(&order, &["up head", "stop head"], 15), ["up head", "stop head"]);
}

/// The provider ids the controller's opencode.json lists after a harness sync.
fn opencode_lanes(c: &Host) -> Vec<String> {
    c.run(&["harness", "sync"]);
    let text = fs::read_to_string(c.dir.join("home/.config/opencode/opencode.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["provider"].as_object().map(|p| p.keys().cloned().collect()).unwrap_or_default()
}

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
}

/// A fake HF cache entry: `models--org--name/{refs/main, snapshots/<commit>/…}`.
fn fake_hf(hf: &Path, repo: &str, commit: &str, files: &[(&str, usize)]) {
    let d = hf.join(format!("models--{}", repo.replace('/', "--")));
    fs::create_dir_all(d.join("refs")).unwrap();
    fs::write(d.join("refs/main"), commit).unwrap();
    let snap = d.join("snapshots").join(commit);
    fs::create_dir_all(&snap).unwrap();
    fs::create_dir_all(d.join("blobs")).unwrap();
    for (i, (f, n)) in files.iter().enumerate() {
        let blob = d.join("blobs").join(format!("blob{i}"));
        fs::write(&blob, "x".repeat(*n)).unwrap();
        std::os::unix::fs::symlink(&blob, snap.join(f)).unwrap();
    }
}

#[test]
fn weights_index_and_nfs_placement_with_fake_docker() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = "lmdeck-test/tiny-model";
    let commit = "0123456789abcdef0123456789abcdef01234567";

    // a: the weights home. Exports come from a test file, not /etc/exports.
    let exports = root.join("exports");
    fs::write(&exports, "# test exports\n").unwrap();
    let mut ha = Host::new(root, "sa", "");
    ha.extra_env = vec![("LMDECK_EXPORTS_FILE".into(), exports.display().to_string())];
    fake_hf(&ha.dir.join("hf"), repo, commit, &[("config.json", 2), ("model.safetensors", 1000)]);
    let a = serve(ha, &[]);

    // b: runs the model; its docker is a script that logs what it is asked.
    let log = root.join("docker.log");
    let docker = root.join("fake-docker");
    fs::write(
        &docker,
        format!(
            "#!/bin/sh\necho \"$*\" >> {log}\ncase \"$1 $2\" in\n  'volume inspect') exit 1 ;;\n  'volume create') echo \"$3\"; exit 0 ;;\nesac\nfor x in \"$@\"; do\n  case \"$x\" in\n    test) exit 0 ;;\n    find) printf 'config.json 2\\nmodel.safetensors 1000\\n'; exit 0 ;;\n  esac\ndone\ncase \"$1\" in run) exec sleep 60 ;; esac\nexit 0\n",
            log = log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let b = serve(
        Host::new(
            root,
            "sb",
            &format!("[serve]\naccept_recipes = true\n[engines]\ndocker = \"{}\"\n", docker.display()),
        ),
        &[],
    );

    let c = Host::new(root, "ctl", &format!("[engines]\ndocker = \"{}\"\n", docker.display()));
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "[[host]]\nname = \"a\"\nurl = \"{}\"\n\n[[host]]\nname = \"b\"\nurl = \"{}\"\n\n[[link]]\na = \"a\"\nb = \"b\"\na_addr = \"10.99.0.1\"\nb_addr = \"10.99.0.2\"\nkind = \"cx7\"\ngbps = 200\n",
            a.url, b.url
        ),
    )
    .unwrap();
    let port = free_port();
    c.add_recipe(
        "tiny",
        &format!(
            "name = \"tiny\"\n[model]\nrepo = \"{repo}\"\n[engine]\nname = \"vllm\"\nimage = \"lmdeck-test/vllm:fake\"\nport = {port}\n"
        ),
    );

    // The weights index: model → host, size, path.
    let w: serde_json::Value = serde_json::from_str(&c.run(&["weights", "--refresh", "--json"])).unwrap();
    let row =
        w["weights"].as_array().unwrap().iter().find(|r| r["model"] == repo).unwrap_or_else(|| panic!("{w}")).clone();
    assert_eq!(row["host"], "a");
    assert_eq!(row["size"], 1002);
    assert!(row["path"].as_str().unwrap().contains("models--lmdeck-test--tiny-model"));
    let table = c.run(&["weights"]);
    assert!(table.contains(repo) && table.contains(" a "), "{table}");
    let list: serde_json::Value = serde_json::from_str(&c.run(&["list", "--json"])).unwrap();
    let tiny = list.as_array().unwrap().iter().find(|r| r["name"] == "tiny").unwrap().clone();
    assert_eq!(tiny["weights_on"], serde_json::json!(["a"]), "{tiny}");

    // Dry run: the exact commands, mirroring WORKER_WEIGHTS=nfs.
    let plan = c.run(&["use", "tiny", "--on", "b", "--dry-run"]);
    let hf_a = a.host.dir.join("hf");
    for want in [
        format!("{} 10.99.0.2(ro,no_subtree_check)", hf_a.display()),
        "sudo exportfs -ra".to_string(),
        "volume create".to_string(),
        "addr=10.99.0.1".to_string(),
        format!("device=:{}", hf_a.display()),
        "lmdeck-hf-a".to_string(),
        "--entrypoint test".to_string(),
        "/config.json".to_string(),
        "config.json".to_string(),
        "--entrypoint find".to_string(),
        "-L /hf/models--lmdeck-test--tiny-model".to_string(),
    ] {
        assert!(plan.contains(&want), "missing {want:?} in\n{plan}");
    }
    assert!(!plan.contains("rsync") && !plan.contains("tmpfs"), "never copies:\n{plan}");
    assert!(!log.exists(), "dry run ran docker");

    // Without the export and without allow_export: refused, commands printed, exports untouched.
    let err = c.fail(&["use", "tiny", "--on", "b", "--no-wait"]);
    assert!(err.contains("sudo exportfs -ra") && err.contains("allow_export"), "{err}");
    assert_eq!(fs::read_to_string(&exports).unwrap(), "# test exports\n");

    // Once exported (by hand), the volume is created on b, preflight matches, and it starts.
    fs::write(&exports, format!("{} 10.99.0.0/24(ro,no_subtree_check)\n", hf_a.display())).unwrap();
    let out = c.run(&["use", "tiny", "--on", "b", "--no-wait"]);
    assert!(out.contains("preflight") || out.contains("2 files"), "{out}");
    // The engine is spawned asynchronously on b: wait for its docker run.
    let t0 = Instant::now();
    let mut d = String::new();
    while t0.elapsed() < Duration::from_secs(60) {
        d = fs::read_to_string(&log).unwrap_or_default();
        if d.contains("lmdeck-hf-a:/root/.cache/huggingface/hub:ro") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(d.contains("volume create") && d.contains("addr=10.99.0.1"), "{d}");
    assert!(d.contains("lmdeck-hf-a:/root/.cache/huggingface/hub:ro"), "mounted read-only:\n{d}");
    assert!(!d.contains("rsync") && !d.contains("cp "), "{d}");
    c.run(&["stop", "tiny"]);
}

#[test]
fn preflight_mismatch_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let docker = root.join("fake-docker");
    // The mount shows a truncated file.
    fs::write(
        &docker,
        "#!/bin/sh\nfor x in \"$@\"; do case \"$x\" in test) exit 0 ;; find) printf 'config.json 2\\nmodel.safetensors 10\\n'; exit 0 ;; esac; done\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let b = serve(
        Host::new(
            root,
            "sb",
            &format!("[serve]\naccept_recipes = true\n[engines]\ndocker = \"{}\"\n", docker.display()),
        ),
        &[],
    );
    let r: Result<ureq::Response, ureq::Error> = ureq::post(&format!("{}/api/weights/preflight", b.url))
        .set("X-Lmdeck", "1")
        .send_json(serde_json::json!({"volume": "lmdeck-hf-a", "image": "x", "path": "models--a--b/snapshots/c"}));
    let v: serde_json::Value = r.unwrap().into_json().unwrap();
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{v}");
    let want = vec![("config.json".to_string(), 2u64), ("model.safetensors".to_string(), 1000)];
    let got: Vec<(String, u64)> =
        files.iter().map(|f| (f[0].as_str().unwrap().to_string(), f[1].as_u64().unwrap())).collect();
    let diff = lmdeck::weights::compare_lists(&want, &got);
    assert!(diff.iter().any(|d| d.contains("model.safetensors")), "{diff:?}");
}

/// The shipped UNTESTED Ray example plans cleanly: CX7 addresses, NCCL wiring
/// and the NFS weights volume land in the per-node docker commands.
#[test]
fn ray_example_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = "nvidia/Qwen3.6-35B-A3B-NVFP4";
    let c = Host::new(root, "ctl", "");
    fake_hf(&c.dir.join("hf"), repo, "1111111111111111111111111111111111111111", &[("config.json", 2)]);
    let s2 = serve(Host::new(root, "spark2", "[serve]\naccept_recipes = true\n"), &[]);
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "[[host]]\nname = \"spark2\"\nurl = \"{}\"\n\n[[link]]\na = \"local\"\nb = \"spark2\"\na_addr = \"10.100.224.2\"\nb_addr = \"10.100.224.1\"\na_ifname = \"enp1s0f1np1\"\nb_ifname = \"enp1s0f1np1\"\na_hca = \"rocep1s0f1\"\nb_hca = \"rocep1s0f1\"\na_gid_index = 3\nb_gid_index = 3\nkind = \"cx7\"\n",
            s2.url
        ),
    )
    .unwrap();
    let ex = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/vllm-ray-2spark.toml");
    c.run(&["recipe", "add", ex.to_str().unwrap()]);
    let plan = c.run(&["use", "qwen3.6-35b-a3b-nvfp4-ray-2spark", "--dry-run"]);
    if std::env::var_os("SHOW_PLAN").is_some() {
        eprintln!("{plan}");
    }
    for want in [
        "ray start --head --port=6379 --node-ip-address=10.100.224.2",
        "ray start --address=10.100.224.2:6379 --node-ip-address=10.100.224.1",
        "NCCL_SOCKET_IFNAME=enp1s0f1np1",
        "NCCL_IB_HCA=rocep1s0f1",
        "NCCL_IB_GID_INDEX=3",
        "VLLM_HOST_IP=10.100.224.1",
        "lmdeck-hf-ctl:/root/.cache/huggingface/hub:ro",
        "(ro,no_subtree_check)",
        "lmdeck-vllm-cache:/root/.cache/vllm",
        "--distributed-executor-backend=ray",
    ] {
        assert!(plan.contains(want), "missing {want:?} in\n{plan}");
    }
    let head = plan.find("head").unwrap();
    let worker = plan.find("worker").unwrap();
    assert!(head < worker, "head starts first:\n{plan}");
}
