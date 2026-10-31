//! What the multi-host tests share: a stand-in machine (`Host`) with its own
//! LMDECK_HOME, its `lmdeck serve` (`Served`), stand-in engines and their
//! recipes, and readers of the opencode.json a harness sync writes.

// Reason: every test binary compiles this module on its own and uses only some of it.
#![allow(dead_code)]

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn free_port() -> u16 {
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
pub struct Host {
    pub dir: PathBuf,
    pub extra_env: Vec<(String, String)>,
}

impl Host {
    pub fn new(root: &Path, name: &str, config: &str) -> Host {
        let dir = root.join(name);
        for d in ["home/config", "models", "hf"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        fs::write(dir.join("home/config/config.toml"), format!("[node]\nname = \"{name}\"\n{config}")).unwrap();
        Host { dir, extra_env: vec![] }
    }

    pub fn cmd(&self) -> Command {
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

    pub fn out(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    pub fn run(&self, args: &[&str]) -> String {
        let o = self.out(args);
        let (so, se) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(o.status.success(), "lmdeck {args:?} failed\n--- stdout ---\n{so}\n--- stderr ---\n{se}");
        so.into_owned()
    }

    pub fn fail(&self, args: &[&str]) -> String {
        let o = self.out(args);
        assert!(!o.status.success(), "lmdeck {args:?} should fail: {}", String::from_utf8_lossy(&o.stdout));
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    }

    pub fn add_recipe(&self, name: &str, body: &str) {
        let f = self.dir.join(format!("{name}.toml"));
        fs::write(&f, body).unwrap();
        self.run(&["recipe", "add", f.to_str().unwrap()]);
    }
}

pub struct Served {
    pub host: Host,
    pub child: Child,
    pub url: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.host.cmd().args(["stop", "--all"]).output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn serve(host: Host, extra: &[&str]) -> Served {
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

pub fn code(r: Result<ureq::Response, ureq::Error>) -> u16 {
    match r {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("transport: {e}"),
    }
}

/// The directory a stand-in engine serves: GET /v1/models answers 200 there.
pub fn www(root: &Path) -> PathBuf {
    let dir = root.join("www");
    fs::create_dir_all(dir.join("v1")).unwrap();
    fs::write(dir.join("v1/models"), "{\"data\":[]}").unwrap();
    dir
}

/// A stand-in engine: answers HTTP on `{port}`, ready once it serves /v1/models.
pub fn web_recipe(name: &str, root: &Path, port: u16) -> String {
    format!(
        "name = \"{name}\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 180\ncommand = [\"python3\", \"-u\", \"-m\", \"http.server\", \"{{port}}\", \"--bind\", \"127.0.0.1\", \"--directory\", \"{}\"]\n",
        root.display(),
        www(root).display()
    )
}

/// A shell node that records start / stop in `order` and then idles (or serves HTTP).
pub fn node_cmd(order: &Path, tag: &str, http: bool, fail: bool) -> String {
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

pub fn cluster_recipe(
    name: &str,
    root: &Path,
    port: u16,
    worker_port: u16,
    order: &Path,
    worker_fails: bool,
) -> String {
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

/// The provider ids the controller's opencode.json lists after a harness sync.
pub fn opencode_lanes(c: &Host) -> Vec<String> {
    c.run(&["harness", "sync"]);
    let text = fs::read_to_string(c.dir.join("home/.config/opencode/opencode.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["provider"].as_object().map(|p| p.keys().cloned().collect()).unwrap_or_default()
}

/// A fake HF cache entry: `models--org--name/{refs/main, snapshots/<commit>/…}`.
pub fn fake_hf(hf: &Path, repo: &str, commit: &str, files: &[(&str, usize)]) {
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

/// The provider ids `home`'s opencode.json lists, read as it is (no sync first).
pub fn opencode_providers(home: &Path) -> Vec<String> {
    let text = fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["provider"].as_object().map(|p| p.keys().cloned().collect()).unwrap_or_default()
}

/// Polls `home`'s opencode.json until it lists exactly `want`, or panics after `secs`.
pub fn wait_for_providers(home: &Path, want: &[&str], secs: u64) {
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
pub fn opencode_base_url(home: &Path, name: &str) -> String {
    let text = fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v["provider"][name]["options"]["baseURL"].as_str().unwrap_or_default().to_string()
}

/// The machine, with the opencode harness installed (its config directory exists).
pub fn with_opencode(h: Host) -> Host {
    fs::create_dir_all(h.dir.join("home/.config/opencode")).unwrap();
    h
}

/// A hosts.toml listing each `(name, url)`.
pub fn hosts_toml(hosts: &[(&str, &str)]) -> String {
    hosts.iter().map(|(n, u)| format!("[[host]]\nname = \"{n}\"\nurl = \"{u}\"\n\n")).collect()
}

/// Polls `url`'s `/api/status` until recipe `name` reports `loaded`, or panics after `secs`.
pub fn wait_loaded(url: &str, name: &str, secs: u64) {
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

/// A stand-in engine with peers (`[hardware] hosts`): writes its `{peer_addr}`
/// (as `WORKER`) to `out`, then serves HTTP like [`web_recipe`].
pub fn peer_recipe(name: &str, root: &Path, port: u16, hosts: u32, out: &Path) -> String {
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
pub fn controller_with_a_linked_to_b(root: &Path) -> (Served, Served, Host) {
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
