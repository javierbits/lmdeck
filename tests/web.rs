//! `lmdeck serve`: the JSON API, auth and the log stream, against the real binary.

// Test helpers hand ureq's own error back so tests can match on status codes.
#![allow(clippy::result_large_err)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Served {
    root: TempDir,
    child: Child,
    base: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        // Stop anything the test started, then the server.
        let _ = self.lmdeck().args(["stop", "--all"]).output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Served {
    fn path(&self, p: &str) -> PathBuf {
        self.root.path().join(p)
    }

    fn lmdeck(&self) -> Command {
        lmdeck_cmd(self.root.path())
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, ureq::Error> {
        ureq::get(&format!("{}{path}", self.base)).call()?.into_json().map_err(ureq::Error::from)
    }

    fn post(&self, path: &str) -> Result<serde_json::Value, ureq::Error> {
        ureq::post(&format!("{}{path}", self.base)).set("X-Lmdeck", "1").call()?.into_json().map_err(ureq::Error::from)
    }
}

fn lmdeck_cmd(root: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lmdeck"));
    c.env("LMDECK_HOME", root.join("home"))
        .env("LMDECK_MODELS_DIR", root.join("models"))
        .env("HF_HUB_CACHE", root.join("hf"))
        .env("NO_COLOR", "1")
        .env_remove("LMDECK_TOKEN")
        .env_remove("HF_TOKEN");
    c
}

fn serve(extra: &[&str], setup: impl FnOnce(&std::path::Path)) -> Served {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("models")).unwrap();
    fs::create_dir_all(root.path().join("hf")).unwrap();
    setup(root.path());
    let port = free_port();
    let child = lmdeck_cmd(root.path())
        .args(["serve", "--port", &port.to_string()])
        .args(extra)
        .env("LMDECK_DEBUG", "1")
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.path().join("serve.err")).unwrap())
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let s = Served { root, child, base };
    let t0 = Instant::now();
    while ureq::get(&format!("{}/api/health", s.base)).call().is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "server did not come up");
        std::thread::sleep(Duration::from_millis(100));
    }
    s
}

fn status(e: ureq::Error) -> u16 {
    match e {
        ureq::Error::Status(code, _) => code,
        other => panic!("transport error: {other}"),
    }
}

#[test]
fn dashboard_and_read_endpoints() {
    let s = serve(&[], |_| {});
    let html = ureq::get(&format!("{}/", s.base)).call().unwrap();
    assert!(html.header("Content-Security-Policy").unwrap().contains("frame-ancestors 'none'"));
    let body = html.into_string().unwrap();
    assert!(body.contains("<title>lmdeck</title>") && body.contains("EventSource"));

    let snap = s.get("/api/snapshot").unwrap();
    assert_eq!(snap["version"], env!("CARGO_PKG_VERSION"));
    let recipes = snap["recipes"].as_array().unwrap();
    assert_eq!(recipes.len(), 8, "the built-ins");
    assert!(recipes.iter().all(|r| r["status"] == "stopped" && r["listed"] == true));
    assert!(recipes.iter().any(|r| r["reference"] == "builtin/qwen3.5-2b-tensorfold" && r["engine"] == "tensorfold"));
    assert!(snap["hardware"]["total_memory_gb"].as_f64().unwrap() > 0.0);

    assert_eq!(s.get("/api/recipes").unwrap().as_array().unwrap().len(), 8);
    assert_eq!(s.get("/api/status").unwrap().as_array().unwrap().len(), 8);
    let doc = s.get("/api/doctor").unwrap();
    assert!(doc["inventory"]["engines"]["tensorfold"].is_object(), "{doc}");
    let sources = s.get("/api/sources").unwrap();
    assert!(sources.as_array().unwrap().iter().all(|x| x["name"].is_string() && x["spec"].is_string()), "{sources}");
    let logs = s.get("/api/logs/builtin/qwen3-4b-gguf?n=5").unwrap();
    assert_eq!(logs["name"], "qwen3-4b-gguf");
    assert!(logs["lines"].as_array().unwrap().is_empty());

    assert_eq!(status(s.get("/api/nope").unwrap_err()), 404);
    assert_eq!(status(s.get("/api/logs/does-not-exist").unwrap_err()), 404);
    assert_eq!(status(s.get("/elsewhere").unwrap_err()), 404);
}

#[test]
fn guards_against_cross_site_and_rebinding() {
    let s = serve(&[], |_| {});
    // POST without the custom header (what a cross-site form would send).
    let err = ureq::post(&format!("{}/api/start/qwen3-4b-gguf", s.base)).call().unwrap_err();
    assert_eq!(status(err), 403);
    // A DNS-rebinding page reaches us with its own Host header.
    let err = ureq::get(&format!("{}/api/snapshot", s.base)).set("Host", "evil.example:80").call().unwrap_err();
    assert_eq!(status(err), 403);
    // Unknown verbs and bad bodies.
    let err = s.post("/api/explode/qwen3-4b-gguf").unwrap_err();
    assert_eq!(status(err), 404);
    let err = ureq::post(&format!("{}/api/start/qwen3-4b-gguf", s.base))
        .set("X-Lmdeck", "1")
        .send_string("{not json")
        .unwrap_err();
    assert_eq!(status(err), 400);
    // Refused actions come back as 409 with the reason.
    match s.post("/api/pull/no-such-recipe").unwrap_err() {
        ureq::Error::Status(409, r) => {
            let v: serde_json::Value = r.into_json().unwrap();
            assert_eq!(v["ok"], false);
            assert!(v["error"].as_str().unwrap().contains("no-such-recipe"), "{v}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn token_is_required_when_set() {
    let s = serve(&["--token", "s3cret-token"], |_| {});
    assert!(s.get("/api/health").is_ok(), "health stays open");
    assert!(ureq::get(&format!("{}/", s.base)).call().is_ok(), "the page itself carries no data");
    assert_eq!(status(s.get("/api/snapshot").unwrap_err()), 401);
    let bad = ureq::get(&format!("{}/api/status", s.base)).set("Authorization", "Bearer nope").call().unwrap_err();
    assert_eq!(status(bad), 401);
    let ok = ureq::get(&format!("{}/api/status", s.base)).set("Authorization", "Bearer s3cret-token").call();
    assert!(ok.is_ok());
    assert!(s.get("/api/status?token=s3cret-token").is_ok(), "EventSource-style query token");
}

#[test]
fn start_stream_logs_and_stop_a_server() {
    let port = free_port();
    let s = serve(&[], |root| {
        let recipe = format!(
            "name = \"web\"\ndisplay_name = \"Stub server\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\ncommand = [\"sh\", \"-c\", \"echo hello-from-stub; exec python3 -m http.server {port} --bind 127.0.0.1\"]\n",
            root.join("models").display()
        );
        let dir = root.join("home/config/recipes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("web.toml"), recipe).unwrap();
    });
    let _ = s.path("home");
    let snap = s.get("/api/snapshot").unwrap();
    let web = snap["recipes"].as_array().unwrap().iter().find(|r| r["name"] == "web").cloned().unwrap();
    assert_eq!(web["reference"], "deck/web");
    assert_eq!(web["listed"], true);

    let r = s.post("/api/start/deck/web").unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["message"].as_str().unwrap().contains("starting web"), "{r}");

    // The SSE stream replays the log and then follows it.
    let resp = ureq::get(&format!("{}/api/logs/web/stream", s.base)).call().unwrap();
    assert_eq!(resp.content_type(), "text/event-stream");
    let mut reader = BufReader::new(resp.into_reader());
    let t0 = Instant::now();
    let mut seen = String::new();
    while !seen.contains("data: hello-from-stub") {
        assert!(t0.elapsed() < Duration::from_secs(20), "no log line in {seen}");
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        seen.push_str(&line);
    }
    assert!(seen.starts_with("event: reset\n"), "{seen}");

    let t0 = Instant::now();
    loop {
        let st = s.get("/api/status").unwrap();
        let row = st.as_array().unwrap().iter().find(|r| r["name"] == "web").cloned().unwrap();
        if row["status"] == "loaded" {
            break;
        }
        if t0.elapsed() > Duration::from_secs(20) {
            let direct = ureq::get(&format!("http://127.0.0.1:{port}/v1/models")).call();
            let logs = s.get("/api/logs/web").unwrap_or_default();
            let list = s.lmdeck().args(["list", "-a"]).output().unwrap();
            let t1 = Instant::now();
            let again = s.get("/api/status");
            eprintln!("status again after {:?}: {again:?}", t1.elapsed());
            eprintln!("serve stderr:\n{}", fs::read_to_string(s.path("serve.err")).unwrap_or_default());
            panic!(
                "never loaded: {row}\ndirect probe: {direct:?}\nlogs: {logs}\nlist: {}{}",
                String::from_utf8_lossy(&list.stdout),
                String::from_utf8_lossy(&list.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let again = s.post("/api/start/web").unwrap();
    assert!(again["message"].as_str().unwrap().contains("already up"), "{again}");
    let logs = s.get("/api/logs/web").unwrap();
    assert!(logs["lines"].as_array().unwrap().iter().any(|l| l == "hello-from-stub"), "{logs}");

    let stop = s.post("/api/stop/web").unwrap();
    assert_eq!(stop["message"], "stopped web");
    let st = s.get("/api/status").unwrap();
    assert_eq!(st.as_array().unwrap().iter().find(|r| r["name"] == "web").unwrap()["status"], "stopped");
    assert_eq!(status(s.post("/api/stop/web").unwrap_err()), 409, "already stopped");
}

#[test]
fn non_loopback_bind_generates_a_token() {
    let root = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = lmdeck_cmd(root.path())
        .args(["serve", "--host", "0.0.0.0", "--port", &port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut banner = String::new();
    for _ in 0..2 {
        out.read_line(&mut banner).unwrap();
    }
    let token = banner.split("#token=").nth(1).map(|t| t.trim().to_string()).expect(&banner);
    assert_eq!(token.len(), 32, "{banner}");
    let base = format!("http://127.0.0.1:{port}");
    assert_eq!(status(ureq::get(&format!("{base}/api/status")).call().unwrap_err()), 401);
    assert!(ureq::get(&format!("{base}/api/status")).set("Authorization", &format!("Bearer {token}")).call().is_ok());
    let _ = child.kill();
    let mut err = String::new();
    let _ = std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut err);
    assert!(err.contains("exposes start/stop controls"), "{err}");
    let _ = child.wait();
}
