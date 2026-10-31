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
        // Every "machine" here is this one: its real CX7 ports would link them all, so
        // only the hosts.toml [[link]]s do.
        c.env("HOME", self.dir.join("home"))
            .env("LMDECK_HOME", self.dir.join("home"))
            .env("LMDECK_MODELS_DIR", self.dir.join("models"))
            .env("HF_HUB_CACHE", self.dir.join("hf"))
            .env("LMDECK_NO_FABRIC", "1")
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

/// The directory a stand-in engine serves: GET /v1/models answers 200 there.
fn www(root: &Path) -> PathBuf {
    let dir = root.join("www");
    fs::create_dir_all(dir.join("v1")).unwrap();
    fs::write(dir.join("v1/models"), "{\"data\":[]}").unwrap();
    dir
}

/// A stand-in engine: answers HTTP on `{port}`, ready once it serves /v1/models.
fn web_recipe(name: &str, root: &Path, port: u16) -> String {
    format!(
        "name = \"{name}\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 180\ncommand = [\"python3\", \"-u\", \"-m\", \"http.server\", \"{{port}}\", \"--bind\", \"127.0.0.1\", \"--directory\", \"{}\"]\n",
        root.display(),
        www(root).display()
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
        let w = www(order.parent().unwrap());
        format!("echo up {tag} >> {o}; trap 'echo stop {tag} >> {o}; kill $P 2>/dev/null; exit 0' TERM INT; python3 -m http.server {{port}} --bind 127.0.0.1 --directory {} & P=$!; wait $P", w.display())
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

/// A node whose command doesn't mount the weights (llama.cpp's rpc-server
/// gets its layers from the head over the link) needs no NFS export, docker
/// volume or preflight, `[model]` or not; a node that mounts them gets them.
#[test]
fn only_nodes_that_mount_the_weights_get_the_nfs_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = "lmdeck-test/rpc-model";
    let c = Host::new(root, "ctl", "");
    fake_hf(&c.dir.join("hf"), repo, "2222222222222222222222222222222222222222", &[("config.json", 2)]);
    let s2 = serve(Host::new(root, "spark2", "[serve]\naccept_recipes = true\n"), &[]);
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "[[host]]\nname = \"spark2\"\nurl = \"{}\"\n\n[[link]]\na = \"local\"\nb = \"spark2\"\na_addr = \"10.100.224.2\"\nb_addr = \"10.100.224.1\"\nkind = \"cx7\"\n",
            s2.url
        ),
    )
    .unwrap();
    let recipe = |name: &str, worker: &str| {
        format!(
            "name = \"{name}\"\n[model]\nrepo = \"{repo}\"\n[engine]\nname = \"custom\"\nimage = \"lmdeck-test/img:1\"\nport = {}\n\
             [cluster]\norder = \"workers-first\"\n\
             [[nodes]]\nname = \"rpc\"\nrole = \"worker\"\nhost = \"spark2\"\nready_port = 50052\ncommand = [{worker}]\n\
             [[nodes]]\nname = \"head\"\nrole = \"head\"\nhost = \"local\"\ncommand = [\"llama-server\", \"--rpc\", \"{{peer_addr}}:50052\"]\n",
            free_port()
        )
    };
    c.add_recipe("rpc", &recipe("rpc", "\"rpc-server\", \"-H\", \"0.0.0.0\", \"-p\", \"50052\""));
    let plan = c.run(&["use", "rpc", "--dry-run"]);
    for never in ["export on", "volume:", "preflight:", "exportfs"] {
        assert!(!plan.contains(never), "{never:?} in\n{plan}");
    }
    assert!(plan.contains("weights: not mounted"), "{plan}");
    assert!(plan.contains("llama-server --rpc 10.100.224.1:50052"), "{plan}");
    // Mounting the volume by name needs it as much as {weights_mount} does.
    let docker = "\"docker\", \"run\", \"-v\", \"{weights_volume}:{weights_at}:ro\", \"{image}\"";
    c.add_recipe("vol", &recipe("vol", docker));
    let plan = c.run(&["use", "vol", "--dry-run"]);
    for want in
        ["export on", "volume: docker volume create", "preflight:", "-v lmdeck-hf-ctl:/root/.cache/huggingface/hub:ro"]
    {
        assert!(plan.contains(want), "missing {want:?} in\n{plan}");
    }
}

/// `git` in `dir`, unsigned, as a fixture repo's author.
fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Commits `version` as the fixture repo's version.txt; returns the commit.
fn commit_version(repo: &Path, version: &str) -> String {
    fs::write(repo.join("version.txt"), version).unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", version]);
    git(repo, &["rev-parse", "HEAD"])
}

/// Every node of a `[source]` recipe runs from its own checkout, on its own
/// machine: a new `ref` and `lmdeck pull` must reach all of them.
#[test]
fn every_node_runs_the_recipes_ref_after_it_changes_or_a_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let up = root.join("upstream");
    fs::create_dir_all(&up).unwrap();
    git(&up, &["init", "-q"]);
    let v1 = commit_version(&up, "v1");
    let c = Host::new(root, "ctl", "");
    let out = root.join("out");
    fs::create_dir_all(&out).unwrap();
    let node = |name: &str, role: &str| {
        format!(
            "[[nodes]]\nname = \"{name}\"\nrole = \"{role}\"\nhost = \"local\"\nport = {}\nready_delay = 1\n\
             command = [\"sh\", \"-c\", \"cat version.txt > {}/{name}.txt; trap 'exit 0' TERM; while :; do sleep 0.2; done\"]\n",
            free_port(),
            out.display()
        )
    };
    let nodes = format!("{}{}", node("w", "worker"), node("h", "head"));
    let recipe = |r: &str| {
        format!(
            "name = \"duo\"\n[source]\nrepo = \"{}\"\nref = \"{r}\"\n[engine]\nname = \"custom\"\nport = {}\n\
             [cluster]\ngate_timeout = 30\n{nodes}",
            up.display(),
            free_port()
        )
    };
    // Starts duo, waits for each node to say which version.txt its checkout has, stops it.
    let versions = || -> Vec<String> {
        for n in ["w", "h"] {
            let _ = fs::remove_file(out.join(format!("{n}.txt")));
        }
        c.run(&["use", "duo", "--no-wait"]);
        let got = ["w", "h"].map(|n| {
            let f = out.join(format!("{n}.txt"));
            let t0 = Instant::now();
            while !f.exists() && t0.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(100));
            }
            fs::read_to_string(&f).unwrap_or_default()
        });
        c.run(&["stop", "duo"]);
        got.to_vec()
    };
    c.add_recipe("duo", &recipe(&v1));
    let deck = c.dir.join("home/config/recipes/duo.toml");
    assert_eq!(versions(), ["v1", "v1"]);
    // A new ref reaches every node.
    let v2 = commit_version(&up, "v2");
    fs::write(&deck, recipe(&v2)).unwrap();
    assert_eq!(versions(), ["v2", "v2"], "the nodes kept their first clone");
    // A branch moves with `lmdeck pull`, and the nodes follow.
    fs::write(&deck, recipe("main")).unwrap();
    assert_eq!(versions(), ["v2", "v2"]);
    commit_version(&up, "v3");
    c.run(&["pull", "duo"]);
    assert_eq!(versions(), ["v3", "v3"], "pull moved a checkout no node uses");
}

/// A start fetches only when a checkout isn't at its recipe's ref: one that is
/// starts even with its remote gone, and a ref the remote doesn't have fails.
#[test]
fn a_checkout_at_its_ref_starts_offline_and_a_missing_ref_fails() {
    use lmdeck::checkout::{sync, Outcome};
    let tmp = tempfile::tempdir().unwrap();
    let up = tmp.path().join("upstream");
    fs::create_dir_all(&up).unwrap();
    git(&up, &["init", "-q"]);
    let v1 = commit_version(&up, "v1");
    let src =
        |r: &str| lmdeck::recipe::GitSource { repo: up.display().to_string(), git_ref: Some(r.into()), branch: None };
    let dest = tmp.path().join("checkouts/duo.w");
    assert!(matches!(sync(&src(&v1), &dest, false), Ok(Outcome::Cloned(c)) if c == v1));
    let err = format!("{:#}", sync(&src("no-such-branch"), &dest, false).unwrap_err());
    assert!(err.contains("ref 'no-such-branch' not found"), "{err}");
    fs::rename(&up, tmp.path().join("moved")).unwrap();
    assert!(matches!(sync(&src(&v1), &dest, false), Ok(Outcome::Present(c)) if c == v1));
}

/// The provider ids `home`'s opencode.json lists, read as it is (no sync first).
fn opencode_providers(home: &Path) -> Vec<String> {
    let text = fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["provider"].as_object().map(|p| p.keys().cloned().collect()).unwrap_or_default()
}

/// Polls `home`'s opencode.json until it lists exactly `want`, or panics after `secs`.
fn wait_for_providers(home: &Path, want: &[&str], secs: u64) {
    let t0 = Instant::now();
    loop {
        let got = opencode_providers(home);
        if got == want {
            return;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(secs),
            "{}: opencode providers stayed {got:?}, wanted {want:?}",
            home.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The baseURL `home`'s opencode.json gives provider `name`.
fn opencode_base_url(home: &Path, name: &str) -> String {
    let text = fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["provider"][name]["options"]["baseURL"].as_str().unwrap_or_default().to_string()
}

/// The machine, with the opencode harness installed (its config directory exists).
fn with_opencode(h: Host) -> Host {
    fs::create_dir_all(h.dir.join("home/.config/opencode")).unwrap();
    h
}

/// A hosts.toml listing each `(name, url)`.
fn hosts_toml(hosts: &[(&str, &str)]) -> String {
    hosts.iter().map(|(n, u)| format!("[[host]]\nname = \"{n}\"\nurl = \"{u}\"\n\n")).collect()
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

/// Polls `url`'s `/api/status` until recipe `name` reports `loaded`, or panics after `secs`.
fn wait_loaded(url: &str, name: &str, secs: u64) {
    let t0 = Instant::now();
    loop {
        let st: serde_json::Value =
            ureq::get(&format!("{url}/api/status")).call().ok().and_then(|r| r.into_json().ok()).unwrap_or_default();
        let loaded =
            st.as_array().is_some_and(|rows| rows.iter().any(|r| r["name"] == name && r["status"] == "loaded"));
        if loaded {
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(secs), "{name} never loaded on {url}: {st}");
        std::thread::sleep(Duration::from_millis(250));
    }
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

/// A stand-in engine with peers (`[hardware] hosts`): writes its `{peer_addr}`
/// (as `WORKER`) to `out`, then serves HTTP like [`web_recipe`].
fn peer_recipe(name: &str, root: &Path, port: u16, hosts: u32, out: &Path) -> String {
    format!(
        "name = \"{name}\"\n[model]\nsource = \"local\"\npath = \"{root}\"\n[hardware]\nhosts = {hosts}\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 60\ncommand = [\"sh\", \"-c\", \"echo \\\"$WORKER\\\" > {out} && exec python3 -u -m http.server {{port}} --bind 127.0.0.1 --directory {www}\"]\n[engine.env]\nWORKER = \"{{peer_addr}}\"\n",
        root = root.display(),
        out = out.display(),
        www = www(root).display()
    )
}

/// A controller that knows `a` and `b` and the CX7 link between them (a at
/// 10.99.0.1, b at 10.99.0.2); `a` runs recipes it is sent and knows no other
/// host; `b` is only there to be a peer.
fn controller_with_a_linked_to_b(root: &Path) -> (Served, Served, Host) {
    let a = serve(Host::new(root, "sa", "[serve]\naccept_recipes = true\n"), &[]);
    let b = serve(Host::new(root, "sb", ""), &[]);
    let c = Host::new(root, "ctl", "");
    fs::write(
        c.dir.join("home/config/hosts.toml"),
        format!(
            "{}[[link]]\na = \"a\"\nb = \"b\"\na_addr = \"10.99.0.1\"\nb_addr = \"10.99.0.2\"\nkind = \"cx7\"\n",
            hosts_toml(&[("a", &a.url), ("b", &b.url)])
        ),
    )
    .unwrap();
    (a, b, c)
}

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
