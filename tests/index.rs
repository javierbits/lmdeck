//! The recipe index end to end against a fake HTTP server (no real network):
//! fetch, auth headers, the cache and its TTL, schema handling, `browse`
//! filters and `add <name>`.

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// What the fake server answers, and what it was asked.
#[derive(Default)]
struct State {
    /// path → (status, body)
    routes: Vec<(String, u16, String)>,
    /// (url, Authorization header, Accept header)
    seen: Vec<(String, Option<String>, Option<String>)>,
}

struct Server {
    base: String,
    state: Arc<Mutex<State>>,
    server: Arc<tiny_http::Server>,
}

impl Server {
    fn start() -> Server {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let (s, st) = (server.clone(), state.clone());
        std::thread::spawn(move || {
            for req in s.incoming_requests() {
                let h = |n: &'static str| req.headers().iter().find(|h| h.field.equiv(n)).map(|h| h.value.to_string());
                let (auth, accept) = (h("Authorization"), h("Accept"));
                let url = req.url().to_string();
                let mut g = st.lock().unwrap();
                g.seen.push((url.clone(), auth, accept));
                let path = url.split('?').next().unwrap_or("").to_string();
                let (code, body) =
                    g.routes.iter().find(|r| r.0 == path).map(|r| (r.1, r.2.clone())).unwrap_or((404, "nope".into()));
                drop(g);
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(code));
            }
        });
        Server { base, state, server }
    }

    fn route(&self, path: &str, code: u16, body: &str) {
        let mut g = self.state.lock().unwrap();
        g.routes.retain(|r| r.0 != path);
        g.routes.push((path.into(), code, body.into()));
    }

    fn seen(&self) -> Vec<(String, Option<String>, Option<String>)> {
        self.state.lock().unwrap().seen.clone()
    }

    fn stop(&self) {
        self.server.unblock();
    }
}

struct Env {
    root: TempDir,
}

impl Env {
    fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        for d in ["models", "hf", "home/config"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        Env { root }
    }

    fn path(&self, p: &str) -> std::path::PathBuf {
        self.root.path().join(p)
    }

    fn config(&self, body: &str) {
        fs::write(self.path("home/config/config.toml"), body).unwrap();
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("lmdeck").unwrap();
        c.env("HOME", self.path("home"))
            .env("LMDECK_HOME", self.path("home"))
            .env("LMDECK_MODELS_DIR", self.path("models"))
            .env("HF_HUB_CACHE", self.path("hf"))
            .env("NO_COLOR", "1")
            .env("LMDECK_NO_GH", "1")
            .env("LMDECK_NO_UPDATE_CHECK", "1")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .env_remove("LMDECK_INDEX_URL")
            .env_remove("LMDECK_GITHUB_API")
            .env_remove("LMDECK_DEMO");
        c
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.cmd().args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

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

fn names(v: &serde_json::Value) -> Vec<String> {
    v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn fetches_caches_and_honours_the_ttl() {
    let s = Server::start();
    s.route("/index.toml", 200, INDEX);
    let e = Env::new();
    e.config(&format!("[index]\nurl = \"{}/index.toml\"\nttl_hours = 6\n", s.base));
    let v = e.json(&["browse", "--json"]);
    assert_eq!(names(&v), ["big-two", "tiny-qwen"]);
    assert_eq!(v["from_cache"], false);
    assert!(e.path("home/cache/index.toml").exists());
    // Fresh cache: no second request.
    let v = e.json(&["browse", "--json"]);
    assert_eq!(v["from_cache"], true);
    assert_eq!(s.seen().len(), 1);
    // --refresh always fetches; ttl 0 fetches every time.
    e.json(&["browse", "--json", "--refresh"]);
    assert_eq!(s.seen().len(), 2);
    e.config(&format!("[index]\nurl = \"{}/index.toml\"\nttl_hours = 0\n", s.base));
    e.json(&["browse", "--json"]);
    e.json(&["browse", "--json"]);
    assert_eq!(s.seen().len(), 4);
    // No token configured: no Authorization header sent anywhere.
    assert!(s.seen().iter().all(|x| x.1.is_none()), "{:?}", s.seen());
    s.stop();
}

#[test]
fn falls_back_to_the_cache_with_a_hint() {
    let s = Server::start();
    s.route("/index.toml", 200, INDEX);
    let e = Env::new();
    e.config(&format!("[index]\nurl = \"{}/index.toml\"\nttl_hours = 0\n", s.base));
    e.json(&["browse", "--json"]);
    // The server now fails: the cached copy is still served, with why.
    s.route("/index.toml", 500, "boom");
    let v = e.json(&["browse", "--json"]);
    assert_eq!(names(&v), ["big-two", "tiny-qwen"]);
    assert_eq!(v["from_cache"], true);
    let hint = v["hint"].as_str().unwrap();
    assert!(hint.contains("HTTP 500") && hint.contains("cached copy"), "{hint}");
    // A newer-schema index is refused and the old cache kept.
    s.route("/index.toml", 200, "schema = 9\nmin_lmdeck = \"9.0.0\"\n[[entry]]\nname = \"x\"\nrepo = \"r\"\n");
    let v = e.json(&["browse", "--json"]);
    assert_eq!(names(&v), ["big-two", "tiny-qwen"]);
    let hint = v["hint"].as_str().unwrap();
    assert!(hint.contains("unsupported schema") && hint.contains("9.0.0"), "{hint}");
    assert!(fs::read_to_string(e.path("home/cache/index.toml")).unwrap().contains("tiny-qwen"));
    let d = e.json(&["doctor", "--json"]);
    let un = d["unsupported_schemas"].to_string();
    assert!(un.contains("recipe index") && un.contains("schema 9"), "{un}");
    // Server gone entirely, and --offline: still the cache, never an error.
    s.stop();
    let v = e.json(&["browse", "--json", "--offline"]);
    assert_eq!(names(&v), ["big-two", "tiny-qwen"]);
    // Nothing cached and nothing reachable: empty, with a hint, exit 0.
    let e2 = Env::new();
    e2.config("[index]\nurl = \"http://127.0.0.1:9/index.toml\"\ntimeout_secs = 1\n");
    let v = e2.json(&["browse", "--json"]);
    assert_eq!(v["total"], 0);
    assert!(v["hint"].as_str().unwrap().contains("couldn't fetch the index"), "{v}");
}

#[test]
fn github_urls_use_the_contents_api_with_a_token() {
    let s = Server::start();
    s.route("/repos/o/private/contents/index.toml", 200, INDEX);
    let e = Env::new();
    e.config("[index]\nurl = \"https://github.com/o/private/blob/main/index.toml\"\n");
    let out = e
        .cmd()
        .env("LMDECK_GITHUB_API", &s.base)
        .env("GITHUB_TOKEN", "test-token-123")
        .args(["browse", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let seen = s.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, "/repos/o/private/contents/index.toml?ref=main");
    assert_eq!(seen[0].1.as_deref(), Some("Bearer test-token-123"));
    assert_eq!(seen[0].2.as_deref(), Some("application/vnd.github.raw"));
    s.stop();
}

#[test]
fn token_env_is_sent_to_a_configured_url() {
    let s = Server::start();
    s.route("/index.toml", 200, INDEX);
    let e = Env::new();
    e.config(&format!("[index]\nurl = \"{}/index.toml\"\ntoken_env = \"MY_INDEX_TOKEN\"\n", s.base));
    let out = e
        .cmd()
        .env("MY_INDEX_TOKEN", "abc")
        .env("GITHUB_TOKEN", "must-not-leak")
        .args(["browse", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let seen = s.seen();
    assert_eq!(seen[0].1.as_deref(), Some("Bearer abc"), "{seen:?}");
    s.stop();
}

#[test]
fn browse_filters_sorts_and_reports_fit() {
    let s = Server::start();
    s.route("/index.toml", 200, INDEX);
    let e = Env::new();
    e.config(&format!("[index]\nurl = \"{}/index.toml\"\n", s.base));
    assert_eq!(names(&e.json(&["browse", "--json", "--sort", "speed"])), ["big-two", "tiny-qwen"]);
    assert_eq!(names(&e.json(&["browse", "--json", "--sort", "sparks"])), ["tiny-qwen", "big-two"]);
    assert_eq!(names(&e.json(&["browse", "--json", "--tag", "exl3"])), ["big-two"]);
    assert_eq!(names(&e.json(&["browse", "--json", "-t", "engine=llama.cpp"])), ["tiny-qwen"]);
    assert_eq!(names(&e.json(&["browse", "qwen", "--json"])), ["tiny-qwen"]);
    let v = e.json(&["browse", "--json"]);
    let big = &v["entries"][0];
    assert_eq!(big["fit"], "fail");
    // Only Sparks count (this machine, if it is one; no hosts here), never a Mac or phone.
    assert!(big["fit_reason"].as_str().unwrap().contains("of 2 Sparks online"), "{big}");

    assert_eq!(big["installed"], "no");
    assert!(!names(&e.json(&["browse", "--json", "--fits"])).contains(&"big-two".to_string()));
    let text = String::from_utf8(e.cmd().args(["browse"]).output().unwrap().stdout).unwrap();
    assert!(text.contains("recipe index 2 of 2") && text.contains("lmdeck add tiny-qwen"), "{text}");
    e.cmd().args(["browse", "--sort", "size"]).assert().failure();
    s.stop();
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn add_resolves_names_through_the_index() {
    let s = Server::start();
    let e = Env::new();
    // A wrapped entry points at a local git repo with start.sh / stop.sh.
    let repo = e.path("wrapped-repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("start.sh"), "#!/usr/bin/env bash\nPORT=\"${PORT:-8123}\"\necho hi\n").unwrap();
    fs::write(repo.join("stop.sh"), "#!/usr/bin/env bash\necho bye\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let index = format!(
        "{INDEX}\n[[entry]]\nname = \"wrapped-one\"\nrepo = \"{}\"\ndescription = \"start.sh\"\n",
        repo.display()
    );
    s.route("/index.toml", 200, &index);
    s.route("/recipes/tiny-qwen.toml", 200, TINY_RECIPE);
    e.config(&format!("[index]\nurl = \"{}/index.toml\"\n", s.base));

    // An entry with a native recipe link: fetched next to the index.
    e.cmd().args(["add", "tiny-qwen"]).assert().success();
    let deck = fs::read_to_string(e.path("home/config/recipes/tiny-qwen.toml")).unwrap();
    assert!(deck.contains("Qwen3-4B-Q4_K_M.gguf"), "{deck}");
    assert!(s.seen().iter().any(|x| x.0 == "/recipes/tiny-qwen.toml"));
    let v = e.json(&["browse", "--json", "tiny"]);
    assert_eq!(v["entries"][0]["installed"], "deck");
    // Again without --force: refused.
    e.cmd().args(["add", "tiny-qwen"]).assert().failure();

    // An entry without one: the repo is wrapped under the entry's name.
    e.cmd().args(["add", "wrapped-one"]).assert().success();
    let w = fs::read_to_string(e.path("home/config/recipes/wrapped-one.toml")).unwrap();
    assert!(w.contains("start.sh"), "{w}");
    let v = e.json(&["browse", "--json", "wrapped"]);
    assert_eq!(v["entries"][0]["installed"], "deck");

    // Unknown names suggest the closest entries.
    let out = e.cmd().args(["add", "tiny"]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not in the recipe index") && err.contains("did you mean: tiny-qwen"), "{err}");
    s.stop();
}
