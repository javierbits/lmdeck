//! `lmdeck serve`: the JSON API, auth and the log stream, against the real binary.

// Test helpers hand ureq's own error back so tests can match on status codes.
#![allow(clippy::result_large_err)]

use lmdeck::catalog::BUILTIN_RECIPES;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn free_port() -> u16 {
    // Never hand out the same port twice in one test process: parallel tests
    // each start a server on "their" port.
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
    c.env("HOME", root.join("home"))
        .env("LMDECK_HOME", root.join("home"))
        .env("LMDECK_MODELS_DIR", root.join("models"))
        .env("HF_HUB_CACHE", root.join("hf"))
        .env("NO_COLOR", "1")
        .env_remove("LMDECK_TOKEN")
        .env_remove("LMDECK_DEMO")
        .env_remove("HF_TOKEN")
        // Never reach the real recipe index (or a gh token) from a test.
        .env("LMDECK_NO_GH", "1")
        .env("LMDECK_INDEX_URL", "http://127.0.0.1:9/index.toml")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN");
    c
}

fn serve(extra: &[&str], setup: impl FnOnce(&std::path::Path)) -> Served {
    serve_with(extra, &[], setup)
}

fn serve_with(extra: &[&str], envs: &[(&str, String)], setup: impl FnOnce(&std::path::Path)) -> Served {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("models")).unwrap();
    fs::create_dir_all(root.path().join("hf")).unwrap();
    setup(root.path());
    let port = free_port();
    let child = lmdeck_cmd(root.path())
        .args(["serve", "--port", &port.to_string()])
        .args(extra)
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
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
    let recipe =
        fs::read_to_string(format!("{}/recipes/qwen3.5-2b-tensorfold.toml", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let s = serve_with(&[], &[("LMDECK_NO_PROBE", "1".to_string())], |root| {
        let deck = root.join("home/config/recipes");
        fs::create_dir_all(&deck).unwrap();
        fs::write(deck.join("qwen3.5-2b-tensorfold.toml"), &recipe).unwrap();
    });
    let html = ureq::get(&format!("{}/", s.base)).call().unwrap();
    assert!(html.header("Content-Security-Policy").unwrap().contains("frame-ancestors 'none'"));
    let body = html.into_string().unwrap();
    assert!(body.contains("<title>lmdeck</title>") && body.contains("EventSource"));
    // The multi-host UI: topology, run-on planner, remote controls, weights badges.
    for hook in [
        "id=\"toposec\"",
        "api(\"topology\")",
        "plan/${ref}?on=",
        "hosts/${encodeURIComponent(host)}/stop/",
        "weights on disk: ",
        "nodes · start order",
        "id=\"tabs\"",
        "pane-library",
        "pane-stats",
        "pane-weights",
        "pane-sources",
        "pane-doctor",
        "recipe/remove/",
        "harness/hosts",
    ] {
        assert!(body.contains(hook), "dashboard is missing {hook}");
    }

    let snap = s.get("/api/snapshot").unwrap();
    assert_eq!(snap["version"], env!("CARGO_PKG_VERSION"));
    let recipes = snap["recipes"].as_array().unwrap();
    let expected = 1 + BUILTIN_RECIPES.len();
    assert_eq!(recipes.len(), expected);
    assert!(recipes.iter().all(|r| r["status"] == "stopped"));
    assert!(recipes
        .iter()
        .any(|r| r["reference"] == "deck/qwen3.5-2b-tensorfold" && r["engine"] == "tensorfold" && r["listed"] == true));
    assert!(recipes
        .iter()
        .filter(|r| r["reference"].as_str().is_some_and(|x| x.starts_with("builtin/")))
        .all(|r| r["listed"] == false));
    assert!(snap["hardware"]["total_memory_gb"].as_f64().unwrap() > 0.0);

    assert_eq!(s.get("/api/recipes").unwrap().as_array().unwrap().len(), expected);
    assert_eq!(
        s.get("/api/status").unwrap().as_array().unwrap().len(),
        1,
        "status lists the deck plus anything running"
    );
    let doc = s.get("/api/doctor").unwrap();
    assert!(doc["inventory"]["engines"]["tensorfold"].is_object(), "{doc}");
    let sources = s.get("/api/sources").unwrap();
    assert!(sources.as_array().unwrap().iter().all(|x| x["name"].is_string() && x["spec"].is_string()), "{sources}");
    let logs = s.get("/api/logs/deck/qwen3.5-2b-tensorfold?n=5").unwrap();
    assert_eq!(logs["name"], "qwen3.5-2b-tensorfold");
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
        // A minimal OpenAI-ish stub. It skips http.server's server_bind, which
        // calls getfqdn() and can stall for ~20 s on macOS CI runners.
        let stub = root.join("stub.py");
        fs::write(
            &stub,
            "import http.server, json, socketserver, sys\n\
             class S(http.server.ThreadingHTTPServer):\n    \
                 def server_bind(self):\n        \
                     socketserver.TCPServer.server_bind(self)\n        \
                     self.server_name, self.server_port = 'localhost', self.server_address[1]\n\
             class H(http.server.BaseHTTPRequestHandler):\n    \
                 def do_GET(self):\n        \
                     body = json.dumps({'object': 'list', 'data': [{'id': 'web'}]}).encode()\n        \
                     self.send_response(200)\n        \
                     self.send_header('Content-Type', 'application/json')\n        \
                     self.end_headers()\n        \
                     self.wfile.write(body)\n\
             print('hello-from-stub', flush=True)\n\
             S(('127.0.0.1', int(sys.argv[1])), H).serve_forever()\n",
        )
        .unwrap();
        let recipe = format!(
            "name = \"web\"\ndisplay_name = \"Stub server\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\ncommand = [\"python3\", \"-u\", \"{}\", \"{{port}}\"]\n",
            root.join("models").display(),
            stub.display()
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
        if t0.elapsed() > Duration::from_secs(45) {
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

#[test]
fn telemetry_and_hosts_aggregate_real_servers() {
    let a = serve(&["--token", "tok-a"], |_| {});
    let dead = free_port();
    let a_base = a.base.clone();
    let b = serve(&[], move |root| {
        let cfg = root.join("home/config");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(
            cfg.join("hosts.toml"),
            format!(
                "[[host]]\nname = \"a\"\nurl = \"{a_base}\"\ntoken = \"tok-a\"\n\n\
                 [[host]]\nname = \"wrong\"\nurl = \"{a_base}\"\ntoken = \"nope\"\n\n\
                 [[host]]\nname = \"dead\"\nurl = \"http://127.0.0.1:{dead}\"\n"
            ),
        )
        .unwrap();
    });

    // A's telemetry needs its token, and is real (not demo) data.
    let url = format!("{}/api/telemetry?history=1", a.base);
    assert_eq!(status(ureq::get(&url).call().unwrap_err()), 401);
    let t: serde_json::Value =
        ureq::get(&url).set("Authorization", "Bearer tok-a").call().unwrap().into_json().unwrap();
    assert_eq!(t["telemetry"]["demo"], false);
    assert!(t["telemetry"]["mem_total_gb"].as_f64().unwrap() > 0.0, "{t}");
    assert!(t["history"].as_array().unwrap().len() <= 1);

    let t0 = Instant::now();
    let hosts = loop {
        let h = b.get("/api/hosts").unwrap();
        if h["hosts"][1]["online"] == true || t0.elapsed() > Duration::from_secs(15) {
            break h;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let list = hosts["hosts"].as_array().unwrap();
    let names: Vec<&str> = list.iter().map(|h| h["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["local", "a", "wrong", "dead"], "{hosts}");
    assert_eq!(hosts["demo"], false);
    assert!(list[0]["local"] == true && list[0]["online"] == true);
    assert_eq!(list[1]["online"], true, "{hosts}");
    assert!(list[1]["telemetry"]["mem_total_gb"].as_f64().unwrap() > 0.0);
    assert_eq!(list[2]["online"], false);
    assert!(list[2]["error"].as_str().unwrap().contains("token"), "{hosts}");
    assert_eq!(list[3]["online"], false);
    assert!(list[3]["telemetry"].is_null());
}

#[test]
fn themes_api_lists_and_saves_to_config() {
    let s = serve(&[], |root| {
        fs::create_dir_all(root.join("home/config")).unwrap();
        fs::write(root.join("home/config/config.toml"), "# mine\ntheme = \"nord\"\n").unwrap();
    });
    let j = s.get("/api/themes").unwrap();
    assert_eq!(j["current"], "nord");
    assert_eq!(j["default"], "retro-82");
    let themes = j["themes"].as_array().unwrap();
    assert!(themes.len() >= 23, "{}", themes.len());
    assert_eq!(themes[0]["slug"], "omp");
    for t in themes {
        assert_eq!(t["swatch"].as_array().unwrap().len(), 8, "{t}");
        for var in ["--page", "--fg", "--accent", "--ok", "--on-pill"] {
            assert!(t["vars"][var].as_str().is_some_and(|v| v.starts_with('#')), "{var} in {t}");
        }
    }
    let latte = themes.iter().find(|t| t["slug"] == "catppuccin-latte").unwrap();
    assert_eq!(latte["light"], true);
    assert_eq!(latte["vars"]["color-scheme"], "light");
    assert!(latte["source"].as_str().unwrap().contains("themes/catppuccin-latte/colors.toml"));

    // Saving: forgiving names, canonical slug in the file, the rest kept.
    let r = ureq::post(&format!("{}/api/theme", s.base))
        .set("X-Lmdeck", "1")
        .send_string(r#"{"name":"Rosé Pine"}"#)
        .unwrap()
        .into_json::<serde_json::Value>()
        .unwrap();
    assert_eq!(r["theme"], "rose-pine");
    let cfg = fs::read_to_string(s.path("home/config/config.toml")).unwrap();
    assert_eq!(cfg, "# mine\ntheme = \"rose-pine\"\n");
    assert_eq!(s.get("/api/themes").unwrap()["current"], "rose-pine");
    assert!(s.lmdeck().args(["theme", "show"]).output().unwrap().stdout.starts_with("Rosé Pine".as_bytes()));

    let bad = |body: &str| {
        status(ureq::post(&format!("{}/api/theme", s.base)).set("X-Lmdeck", "1").send_string(body).unwrap_err())
    };
    assert_eq!(bad(r#"{"name":"solarized"}"#), 404);
    assert_eq!(bad("{nope"), 400);
    // Like every other POST it needs the header.
    let err = ureq::post(&format!("{}/api/theme", s.base)).send_string(r#"{"name":"nord"}"#).unwrap_err();
    assert_eq!(status(err), 403);
    assert_eq!(s.get("/api/themes").unwrap()["current"], "rose-pine");
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn wait_for(s: &Served, name: &str, state: &str) -> serde_json::Value {
    let t0 = Instant::now();
    loop {
        let recipes = s.get("/api/recipes").unwrap();
        let r = recipes.as_array().unwrap().iter().find(|r| r["name"] == name).unwrap().clone();
        if r["update"]["state"] == state {
            return r;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{name} never reached {state}: {r}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The dashboard reports upstream commits for wrapped recipes and its Pull
/// (with `{"latest":true}` for a pin) takes them.
#[test]
fn wrapped_recipe_updates_show_and_pull_clears() {
    let s = serve(&[], |root| {
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("start.sh"), "#!/usr/bin/env bash\nPORT=\"${PORT:-8124}\"\n").unwrap();
        fs::write(work.join("stop.sh"), "#!/usr/bin/env bash\n").unwrap();
        git(&work, &["init", "-q"]);
        git(&work, &["add", "."]);
        git(&work, &["commit", "-qm", "one"]);
        let bare = root.join("remote.git");
        git(root, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        for args in [["--name", "follow", "--ref", "main"].as_slice(), ["--name", "pinned"].as_slice()] {
            assert!(lmdeck_cmd(root).arg("add").arg(&bare).args(args).output().unwrap().status.success());
        }
        let native = fs::read_to_string(format!("{}/recipes/qwen3-4b-gguf.toml", env!("CARGO_MANIFEST_DIR")));
        fs::write(root.join("home/config/recipes/native.toml"), native.unwrap()).unwrap();
    });
    // The background check fills the cache on startup.
    wait_for(&s, "follow", "current");
    let native = s.get("/api/recipes").unwrap();
    assert!(native.as_array().unwrap().iter().any(|r| r["update"].is_null()), "native recipes have no update");

    let work = s.path("work");
    fs::write(work.join("two.txt"), "2\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "two"]);
    git(&work, &["push", "-q", "origin", "HEAD:main"]);
    assert!(s.lmdeck().arg("outdated").output().unwrap().status.success());
    let r = wait_for(&s, "follow", "behind");
    assert_eq!(r["update"]["follows"], "main", "{r}");
    wait_for(&s, "pinned", "pinned_newer");
    let html = ureq::get(&format!("{}/", s.base)).call().unwrap().into_string().unwrap();
    assert!(html.contains("⬆ Update") && html.contains("latest:true"));

    let j = s.post("/api/pull/follow").unwrap();
    assert_eq!(j["ok"], true, "{j}");
    wait_for(&s, "follow", "current");
    assert!(s.path("home/cache/checkouts/follow/two.txt").exists());
    // A plain pull keeps the pin; latest moves it.
    s.post("/api/pull/pinned").unwrap();
    wait_for(&s, "pinned", "pinned_newer");
    let j: serde_json::Value = ureq::post(&format!("{}/api/pull/pinned", s.base))
        .set("X-Lmdeck", "1")
        .send_string(r#"{"latest":true}"#)
        .unwrap()
        .into_json()
        .unwrap();
    assert!(j["message"].as_str().unwrap().contains("ref "), "{j}");
    wait_for(&s, "pinned", "current");
    assert!(s.path("home/cache/checkouts/pinned/two.txt").exists());
}

#[test]
fn a_peers_deck_shows_in_list_and_resolves_to_it() {
    let tiny = "[model]\nrepo = \"x/peer-model\"\nfiles = [\"a.gguf\"]\n[engine]\nname = \"llama.cpp\"\nport = 18999\n";
    let a = serve(&["--token", "tok-a"], |root| {
        let deck = root.join("home/config/recipes");
        fs::create_dir_all(&deck).unwrap();
        fs::write(deck.join("peer-only.toml"), tiny).unwrap();
    });
    // A's own deck over the API, with its file's TOML.
    let url = format!("{}/api/deck", a.base);
    assert_eq!(status(ureq::get(&url).call().unwrap_err()), 401);
    let deck: serde_json::Value =
        ureq::get(&url).set("Authorization", "Bearer tok-a").call().unwrap().into_json().unwrap();
    assert_eq!(deck, serde_json::json!([{"name": "peer-only", "toml": tiny}]));

    // B lists A as a host: `list` shows A's recipe, from A, after B's own (empty) deck.
    let root = tempfile::tempdir().unwrap();
    let cfg = root.path().join("home/config");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("hosts.toml"), format!("[[host]]\nname = \"a\"\nurl = \"{}\"\ntoken = \"tok-a\"\n", a.base))
        .unwrap();
    let out = lmdeck_cmd(root.path()).args(["list", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "a peer deck replaces the local examples: {rows:?}");
    assert_eq!((rows[0]["name"].as_str(), rows[0]["origin"].as_str()), (Some("peer-only"), Some("@a")));
    // Its dry run goes to A, the host whose deck has it.
    let out = lmdeck_cmd(root.path()).args(["use", "peer-only", "--dry-run"]).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("a's deck"), "{text}");
}

// ─────────────────── managing the deck from the dashboard ───────────────────

const INDEX: &str = r#"
schema = 1
[[entry]]
name = "tiny-qwen"
repo = "https://github.com/example/tiny-qwen"
description = "a small llama.cpp recipe"
recipe = "recipes/tiny-qwen.toml"
tags = { engine = "llama.cpp", quant = "gguf", family = "qwen3", sparks = 1 }
speed = { tok_s = 30.0, note = "fixture" }
[[entry]]
name = "big-two"
repo = "https://github.com/example/big-two"
description = "needs two sparks"
size_gb = 176.0
tags = { engine = "tensorfold", quant = "exl3", sparks = 2 }
speed = { tok_s = 60.0, note = "fixture" }
"#;

const TINY_RECIPE: &str = r#"
schema = 1
display_name = "Tiny Qwen"
[model]
repo = "unsloth/Qwen3-4B-GGUF"
files = ["Qwen3-4B-Q4_K_M.gguf"]
[quant]
format = "gguf"
label = "Q4_K_M"
[engine]
name = "llama.cpp"
port = 8090
"#;

/// A tiny index server, so `/api/browse` and `/api/add` never touch the net.
struct IndexServer {
    base: String,
    server: std::sync::Arc<tiny_http::Server>,
}

impl IndexServer {
    fn start() -> IndexServer {
        let server = std::sync::Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let s = server.clone();
        std::thread::spawn(move || {
            for req in s.incoming_requests() {
                let path = req.url().split('?').next().unwrap_or("").to_string();
                let body = match path.as_str() {
                    "/index.toml" => INDEX,
                    "/recipes/tiny-qwen.toml" => TINY_RECIPE,
                    _ => "nope",
                };
                let code = if body == "nope" { 404 } else { 200 };
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
            }
        });
        IndexServer { base, server }
    }
}

impl Drop for IndexServer {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

impl Served {
    fn post_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, ureq::Error> {
        ureq::post(&format!("{}{path}", self.base))
            .set("X-Lmdeck", "1")
            .send_json(body)?
            .into_json()
            .map_err(ureq::Error::from)
    }
}

#[test]
fn management_api_covers_the_cli_commands() {
    let index = IndexServer::start();
    let s = serve_with(
        &[],
        &[("LMDECK_INDEX_URL", format!("{}/index.toml", index.base)), ("LMDECK_NO_PROBE", "1".into())],
        |root| {
            let dir = root.join("home/config/recipes");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("local.toml"),
                "[model]\nsource = \"local\"\npath = \"models\"\n[engine]\nname = \"llama.cpp\"\nport = 8091\n",
            )
            .unwrap();
        },
    );

    // Browse: the whole index, a tag filter, and fits.
    let b = s.get("/api/browse").unwrap();
    assert_eq!(b["total"], 2, "{b}");
    assert_eq!(b["entries"].as_array().unwrap().len(), 2);
    assert!(b["tags"].as_array().unwrap().iter().any(|t| t["value"] == "exl3"), "{b}");
    let b = s.get("/api/browse?tag=exl3").unwrap();
    assert_eq!(b["entries"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(b["entries"][0]["name"], "big-two");
    let b = s.get("/api/browse?fits=1").unwrap();
    assert!(b["entries"].as_array().unwrap().iter().all(|e| e["fit"] != "fail"), "{b}");
    let b = s.get("/api/browse?q=qwen&sort=speed").unwrap();
    assert_eq!(b["entries"][0]["name"], "tiny-qwen", "{b}");

    // Install an index entry, then read and remove it.
    let j = s.post_json("/api/add", serde_json::json!({"repo": "tiny-qwen"})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(j["name"], "tiny-qwen");
    assert!(s.get("/api/snapshot").unwrap()["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["reference"] == "deck/tiny-qwen"));
    let err = s.post_json("/api/add", serde_json::json!({"repo": "tiny-qwen"})).unwrap_err();
    assert_eq!(status(err), 409, "a second add needs force");
    let r = s.get("/api/recipe/deck/tiny-qwen").unwrap();
    assert!(r["toml"].as_str().unwrap().contains("llama.cpp"), "{r}");
    let j = s.post("/api/recipe/remove/tiny-qwen").unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert!(!s.get("/api/snapshot").unwrap()["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["reference"] == "deck/tiny-qwen"));

    // Add a recipe file, then remove it.
    let file = s.path("extra.toml");
    fs::write(&file, "[engine]\nname = \"llama.cpp\"\nport = 8092\n[model]\nsource = \"local\"\npath = \"models\"\n")
        .unwrap();
    let j = s.post_json("/api/recipe/add", serde_json::json!({"reference": file, "name": "extra"})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(s.get("/api/recipe/extra").unwrap()["name"], "extra");
    s.post("/api/recipe/remove/extra").unwrap();

    // Sources: a local directory, update, then remove.
    let src = s.path("recipes-src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("from-source.toml"),
        "[model]\nsource = \"local\"\npath = \"models\"\n[engine]\nname = \"llama.cpp\"\nport = 8093\n",
    )
    .unwrap();
    let j = s.post_json("/api/source/add", serde_json::json!({"spec": src, "name": "mine"})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(j["recipes"], 1, "{j}");
    assert!(s.get("/api/snapshot").unwrap()["sources"].as_array().unwrap().iter().any(|x| x["name"] == "mine"));
    let j = s.post_json("/api/source/update", serde_json::json!({})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(j["results"][0]["name"], "mine", "{j}");
    let j = s.post("/api/source/remove/mine").unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert!(s.get("/api/snapshot").unwrap()["sources"].as_array().unwrap().is_empty());

    // Hosts: add without contacting it, then forget it.
    let j = s
        .post_json("/api/host/add", serde_json::json!({"name": "peer", "url": "http://127.0.0.1:9", "check": false}))
        .unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let hosts = fs::read_to_string(s.path("home/config/hosts.toml")).unwrap();
    assert!(hosts.contains("peer") && hosts.contains("127.0.0.1:9"), "{hosts}");
    let j = s.post("/api/host/remove/peer").unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let err = s.post("/api/host/remove/peer").unwrap_err();
    assert_eq!(status(err), 409);

    // Harness: list, enable, restrict hosts, export, hub, disable.
    let h = s.get("/api/harness").unwrap();
    assert!(h["harnesses"].as_array().unwrap().iter().any(|x| x["id"] == "opencode"), "{h}");
    let j = s.post_json("/api/harness/enable", serde_json::json!({"name": "opencode"})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let j = s.post_json("/api/harness/hosts", serde_json::json!({"hosts": []})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let j = s.post_json("/api/harness/export", serde_json::json!({"on": true})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let j = s.post_json("/api/harness/hub", serde_json::json!({"on": true})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let cfg = fs::read_to_string(s.path("home/config/config.toml")).unwrap();
    assert!(cfg.contains("opencode_export = true") && cfg.contains("hub = true"), "{cfg}");
    assert!(cfg.contains("\"opencode\""), "{cfg}");
    let j = s.post_json("/api/harness/disable", serde_json::json!({"name": "opencode"})).unwrap();
    assert_eq!(j["ok"], true, "{j}");
    let err = s.post_json("/api/harness/hosts", serde_json::json!({"hosts": ["nope"]})).unwrap_err();
    assert_eq!(status(err), 409);

    // Stop all (nothing is running, so it is a no-op with a clear message).
    let j = s.post("/api/stop-all").unwrap();
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(j["message"], "nothing to stop", "{j}");

    // Updates, doctor paths.
    let j = s.get("/api/outdated?fetch=0").unwrap();
    assert!(j["rows"].is_array(), "{j}");
    let d = s.get("/api/doctor").unwrap();
    assert!(d["paths"]["recipes"].is_string(), "{d}");
    assert!(d["updates"]["enabled"].is_boolean(), "{d}");
}
