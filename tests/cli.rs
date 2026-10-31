//! End-to-end tests of the `lmdeck` binary. No network: sources are local dirs.

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use tempfile::TempDir;

struct Env {
    root: TempDir,
}

impl Env {
    fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("models")).unwrap();
        fs::create_dir_all(root.path().join("hf")).unwrap();
        Env { root }
    }

    fn path(&self, p: &str) -> std::path::PathBuf {
        self.root.path().join(p)
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("lmdeck").unwrap();
        c.env("HOME", self.path("home"))
            .env("LMDECK_HOME", self.path("home"))
            .env("LMDECK_MODELS_DIR", self.path("models"))
            .env("HF_HUB_CACHE", self.path("hf"))
            .env("NO_COLOR", "1")
            .env_remove("HF_TOKEN")
            .env_remove("LMDECK_DEMO")
            .env_remove("LMDECK_DEMO_TOPO")
            .env_remove("LMDECK_FABRIC")
            .env_remove("LMDECK_THEME");
        c
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap()
    }
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), body).unwrap();
}

const QWEN_TOML: &str = r#"# Qwen3 4B Q4 on llama.cpp
display_name = "Qwen3 4B"
tags = ["qwen3"]

[model]
repo = "unsloth/Qwen3-4B-GGUF"
files = ["Qwen3-4B-Q4_K_M.gguf"]

[quant]
format = "gguf"
label = "Q4_K_M"

[engine]
name = "llama.cpp"
port = 8090
ctx = 16384
args = ["--n-gpu-layers", "99", "-fit", "off"]
"#;

const DUAL_TOML: &str = r#"[model]
repo = "nvidia/Qwen3.6-35B-A3B-NVFP4"

[engine]
name = "vllm"
port = 8200

[hardware]
nodes = 2
"#;

#[test]
fn help_and_version() {
    let e = Env::new();
    e.cmd().arg("--help").assert().success().stdout(predicate::str::contains("recipe"));
    e.cmd().arg("--version").assert().success().stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn bare_invocation_shows_wordmark_and_builtins() {
    let e = Env::new();
    let out = e.run(&[]);
    assert!(out.contains("██╗     ███╗   ███╗"), "{out}");
    assert!(out.contains("qwen3-4b-gguf"));
    assert!(out.contains("deck is empty"));
    assert!(!out.contains('\x1b'), "NO_COLOR respected");
}

#[test]
fn port_start_counts_portless_recipes() {
    let e = Env::new();
    let cfg = e.path("home/config");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("config.toml"), "[serve]\nport_start = 8080\n").unwrap();
    let deck = e.path("home/config/recipes");
    fs::create_dir_all(&deck).unwrap();
    let body = "[model]\nrepo = \"x/y\"\nfiles = [\"a.gguf\"]\n\n[engine]\nname = \"llama.cpp\"\n";
    write(&deck, "alpha.toml", body);
    write(&deck, "bravo.toml", body);
    write(&deck, "explicit.toml", &body.replace("llama.cpp\"\n", "llama.cpp\"\nport = 9000\n"));
    let list = e.run(&["list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    let port_of = |name: &str| {
        v.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name && r["origin"] == "deck")
            .map(|r| r["port"].as_u64().unwrap())
            .unwrap()
    };
    assert_eq!(port_of("alpha"), 8080);
    assert_eq!(port_of("bravo"), 8081);
    assert_eq!(port_of("explicit"), 9000, "an explicit [engine] port wins");
}

#[test]
fn doctor_json_reports_hardware_and_recipes() {
    let e = Env::new();
    let out = e.run(&["doctor", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["hardware"]["total_memory_gb"].as_f64().unwrap() > 0.0);
    assert!(v["inventory"]["engines"]["llama.cpp"].is_object());
    assert_eq!(v["recipes"].as_array().unwrap().len(), 10);
    assert!(v["paths"]["config"].as_str().unwrap().ends_with("config/config.toml"));
    // Human output too.
    assert!(e.run(&["doctor"]).contains("engines"));
}

#[test]
fn local_source_with_toml_recipes() {
    let e = Env::new();
    let src = e.path("src");
    // A free port: the fixture's 8090 can be taken by a real server on the box.
    let port = free_port();
    write(&src, "qwen3-4b.toml", &QWEN_TOML.replace("8090", &port.to_string()));
    write(&src, "dual.toml", DUAL_TOML);
    write(&src, "notes.toml", "title = \"not a recipe\"\n");
    // YAML is no longer read; sources skip it.
    write(&src, "old.yaml", "runtime: llama\nmodel: x/y\n");
    write(&src.join("scripts"), "README.md", "# nope\n");

    let out = e.cmd().args(["source", "add", src.to_str().unwrap(), "--name", "mine"]).assert().success();
    let err = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(err.contains("2 recipes"), "{err}");
    assert!(e.run(&["source", "list"]).contains("mine"));
    e.cmd().args(["source", "add", src.to_str().unwrap(), "--name", "mine"]).assert().failure();

    let found = e.run(&["recipe", "search", "qwen3", "gguf", "--source", "mine", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&found).unwrap();
    let refs: Vec<&str> = v.as_array().unwrap().iter().map(|r| r["ref"].as_str().unwrap()).collect();
    assert_eq!(refs, ["mine/qwen3-4b"]);
    let gguf = e.run(&["recipe", "search", "--quant", "Q4_K_M", "--engine", "llama.cpp"]);
    assert!(gguf.contains("mine/qwen3-4b") && gguf.contains("builtin/qwen3-4b-gguf"), "{gguf}");

    // Add to the deck; it is now #1 in `list`.
    e.run(&["recipe", "add", "mine/qwen3-4b"]);
    e.cmd().args(["recipe", "add", "mine/qwen3-4b"]).assert().failure();
    assert!(e.path("home/config/recipes/qwen3-4b.toml").exists());
    let list = e.run(&["list"]);
    assert!(list.contains("qwen3-4b") && !list.contains("deck is empty"), "{list}");
    let deck: serde_json::Value = serde_json::from_str(&e.run(&["list", "--json"])).unwrap();
    assert_eq!(deck[0]["name"], "qwen3-4b");
    assert_eq!(deck[0]["status"], "stopped");
    assert_eq!(deck[0]["downloaded"], false);

    let show = e.run(&["recipe", "show", "qwen3-4b"]);
    assert!(show.contains("hf:unsloth/Qwen3-4B-GGUF") && show.contains("gguf Q4_K_M"), "{show}");
    let toml = e.run(&["recipe", "show", "1", "--toml"]);
    assert!(toml.contains("name = \"llama.cpp\""), "{toml}");

    // Dry runs: weights are missing, so llama.cpp streams from HF.
    let plan = e.run(&["use", "1", "--dry-run"]);
    assert!(
        plan.contains(&format!("--port {port}")) && plan.contains("--n-gpu-layers 99") && plan.contains("-fit off"),
        "{plan}"
    );
    // Into the Hugging Face cache: no --local-dir.
    let pull = e.run(&["pull", "qwen3-4b", "--dry-run"]);
    assert!(pull.contains("download unsloth/Qwen3-4B-GGUF --include Qwen3-4B-Q4_K_M.gguf"), "{pull}");
    assert!(!pull.contains("--local-dir"), "{pull}");

    // Once the GGUF is on disk, the plan points at the file.
    write(&e.path("models/unsloth/Qwen3-4B-GGUF"), "Qwen3-4B-Q4_K_M.gguf", "GGUF");
    let plan = e.run(&["use", "qwen3-4b", "--dry-run"]);
    assert!(plan.contains("--model ") && plan.contains("models/unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf"), "{plan}");
    let dl = e.run(&["list", "--downloaded"]);
    assert!(dl.contains("unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf") && dl.contains("qwen3-4b"), "{dl}");

    // Multi-node recipes without [[nodes]] are refused.
    e.cmd().args(["use", "mine/dual"]).assert().failure().stderr(predicate::str::contains("give it [[nodes]] entries"));

    e.run(&["recipe", "remove", "qwen3-4b"]);
    e.run(&["source", "remove", "mine"]);
    assert!(!e.run(&["source", "list"]).contains("mine"));
    assert!(e.run(&["recipe", "list"]).contains("builtin"));
}

#[test]
fn tensorfold_recipe_plan_pull_and_doctor() {
    let e = Env::new();
    let plan = e.run(&["use", "qwen3.5-2b-tensorfold", "--dry-run"]);
    assert!(
        plan.contains(
            "serve mlx-community/Qwen3.5-2B-MLX-4bit --name qwen3.5-2b-tensorfold --host 0.0.0.0 --port 8086 \
             --context 8192 --no-thinking"
        ),
        "{plan}"
    );
    let pull = e.run(&["pull", "qwen3.5-2b-tensorfold", "--dry-run"]);
    assert!(pull.contains("pull mlx-community/Qwen3.5-2B-MLX-4bit@93760be4f1f69842a46bc13dbdc0f19e291392a3"), "{pull}");
    assert!(!pull.contains("hf download"), "{pull}");
    let found = e.run(&["recipe", "search", "--engine", "tf", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&found).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2, "{found}");
    // A stand-in `tensorfold-native` (the Linux archive's binary name) is detected with its version.
    let bin = e.path("bin");
    write(&bin, "tensorfold-native", "#!/bin/sh\necho 'tensorfold-native 1.1.2'\n");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("tensorfold-native"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = e.cmd().env("PATH", &path).args(["doctor", "--json"]).assert().success().get_output().stdout.clone();
    let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let tf = &doc["inventory"]["engines"]["tensorfold"];
    assert_eq!(tf["available"], true, "{tf}");
    assert!(tf["detail"].as_str().unwrap().ends_with("tensorfold-native (1.1.2)"), "{tf}");
    let out = e.cmd().env("PATH", &path).args(["pull", "qwen3.5-2b-tensorfold", "--dry-run"]).assert().success();
    let pull = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(pull.contains("bin/tensorfold-native pull mlx-community/"), "{pull}");
}

#[test]
fn unknown_and_ambiguous_references() {
    let e = Env::new();
    e.cmd().args(["use", "does-not-exist"]).assert().failure().stderr(predicate::str::contains("no recipe"));
    e.cmd().args(["use", "99", "--dry-run"]).assert().failure().stderr(predicate::str::contains("no recipe #99"));
    e.cmd().args(["recipe", "show", "qwen3-4b"]).assert().failure().stderr(predicate::str::contains("did you mean"));
    e.cmd().args(["source", "add", "ftp://x"]).assert().failure();
}

#[test]
fn init_writes_config_once() {
    let e = Env::new();
    e.run(&["init", "--models-dir", "/srv/models"]);
    let cfg = fs::read_to_string(e.path("home/config/config.toml")).unwrap();
    assert!(cfg.contains("/srv/models") && !cfg.contains("ai-models"), "{cfg}");
    e.cmd().arg("init").assert().failure();
    e.run(&["init", "--force"]);
}

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

#[test]
fn start_list_stop_custom_server() {
    let e = Env::new();
    let port = free_port();
    // http.server serves this dir: GET /v1/models answers 200, which is what "ready" takes.
    write(&e.path("www/v1"), "models", "{\"data\":[{\"id\":\"web\"}]}");
    let recipe = format!(
        "name = \"web\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 180\ncommand = [\"python3\", \"-u\", \"-m\", \"http.server\", \"{{port}}\", \"--bind\", \"127.0.0.1\", \"--directory\", \"{}\"]\n",
        e.path("models").display(),
        e.path("www").display()
    );
    let file = e.path("web.toml");
    fs::write(&file, recipe).unwrap();
    e.run(&["recipe", "add", file.to_str().unwrap()]);

    // A generous timeout: on GitHub's macOS runners (Homebrew Python 3.15) a fresh
    // `python3 -m http.server` has taken ~40 s to answer its first request.
    let out = e.run(&["use", "web"]);
    let log = fs::read_to_string(e.path("home/state/logs/web.log")).unwrap_or_default();
    if !out.contains(&format!("web is up at http://localhost:{port}/v1")) {
        let probe = e.cmd().args(["list", "--json"]).env("LMDECK_DEBUG", "1").output().unwrap();
        let lsof = std::process::Command::new("lsof").args(["-nP", &format!("-iTCP:{port}")]).output();
        let ps = std::process::Command::new("ps").args(["-axo", "pid,pgid,stat,etime,command"]).output();
        let txt = |o: std::io::Result<std::process::Output>| {
            o.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
        };
        panic!(
            "{out}\n--- log ---\n{log}\n--- probe ---\n{}\n--- lsof ---\n{}\n--- ps ---\n{}",
            String::from_utf8_lossy(&probe.stderr),
            txt(lsof),
            txt(ps)
                .lines()
                .filter(|l| l.contains("python") || l.contains("lmdeck") || l.contains("sh"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let v: serde_json::Value = serde_json::from_str(&e.run(&["list", "--json"])).unwrap();
    assert_eq!(v[0]["status"], "loaded");
    let loaded = e.run(&["list", "--loaded"]);
    assert!(loaded.contains("web") && loaded.contains(&port.to_string()), "{loaded}");
    assert!(e.run(&["use", "web"]).contains("already up"));

    assert!(e.run(&["stop", "web"]).contains("stopped web"));
    let v: serde_json::Value = serde_json::from_str(&e.run(&["list", "--json"])).unwrap();
    assert_eq!(v[0]["status"], "stopped");
    assert!(e.run(&["stop", "--all"]).contains("nothing to stop"));
    assert!(e.run(&["logs", "web"]).contains("http.server"));
}

#[test]
fn crashing_server_reports_log() {
    let e = Env::new();
    let recipe = format!(
        "name = \"boom\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {}\ncommand = [\"sh\", \"-c\", \"echo kaboom-from-server; exit 7\"]\n",
        e.path("models").display(),
        free_port()
    );
    let file = e.path("boom.toml");
    fs::write(&file, recipe).unwrap();
    e.run(&["recipe", "add", file.to_str().unwrap()]);
    e.cmd()
        .args(["use", "boom"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("kaboom-from-server").and(predicate::str::contains("exited during startup")));
}

#[test]
fn host_add_list_remove() {
    let env = Env::new();
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let out = env.run(&["host", "add", "spark9", &format!("http://127.0.0.1:{dead}/"), "--token", "t0k"]);
    assert!(out.contains("added host spark9") && out.contains("not reachable yet: connection refused"), "{out}");
    let file = std::fs::read_to_string(env.path("home/config/hosts.toml")).unwrap();
    assert!(file.contains(&format!("url = \"http://127.0.0.1:{dead}\"")) && file.contains("token = \"t0k\""), "{file}");
    let out = env.cmd().args(["host", "add", "x", "spark1:7979"]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("http://spark1:7979"));

    let list = env.run(&["host", "list"]);
    assert!(list.contains("local") && list.contains("spark9") && list.contains("connection refused"), "{list}");
    let json: serde_json::Value = serde_json::from_str(&env.run(&["hosts", "ls", "--json"])).unwrap();
    assert_eq!(json[0]["online"], true);
    assert_eq!(json[1]["online"], false);
    assert!(json[1].get("history").is_none());
    assert!(!json.to_string().contains("t0k"), "tokens are never printed");

    assert!(env.run(&["host", "rm", "spark9"]).contains("removed host spark9"));
    assert!(env.run(&["host", "list"]).contains("no other hosts"));
}

#[test]
fn theme_set_list_show_and_sync() {
    let e = Env::new();
    assert!(e.run(&["theme", "show"]).contains("omp (oh-my-pi)"));
    let list = e.run(&["theme", "list"]);
    for slug in ["omp", "catppuccin", "catppuccin-latte", "rose-pine", "tokyo-night", "white", "vantablack"] {
        assert!(list.contains(slug), "{slug} missing from\n{list}");
    }
    // Forgiving names, saved as the canonical slug.
    assert!(e.run(&["theme", "set", "Rosé Pine"]).contains("rose-pine"));
    let cfg = fs::read_to_string(e.path("home/config/config.toml")).unwrap();
    assert!(cfg.contains("theme = \"rose-pine\""), "{cfg}");
    let show = e.run(&["theme", "show"]);
    assert!(show.contains("Rosé Pine") && show.contains("themes/rose-pine/colors.toml"), "{show}");
    assert!(e.run(&["theme", "list"]).contains("● │ rose-pine"));
    e.run(&["theme", "set", "TOKYO night"]);
    assert!(fs::read_to_string(e.path("home/config/config.toml")).unwrap().contains("theme = \"tokyo-night\""));
    // The TUI picks the saved theme up; --theme / LMDECK_THEME override it.
    let frame = e.run(&["tui", "--render-text", "--picker", "--width", "120", "--height", "40"]);
    assert!(frame.contains("▸ Tokyo Night"), "{frame}");
    let frame = e.run(&["tui", "--render-text", "--picker", "--theme", "nord"]);
    assert!(frame.contains("▸ Nord"), "{frame}");
    e.cmd()
        .args(["tui", "--render-text", "--picker"])
        .env("LMDECK_THEME", "kanagawa")
        .assert()
        .success()
        .stdout(predicate::str::contains("▸ Kanagawa"));
    e.cmd()
        .args(["theme", "set", "solarized"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown theme 'solarized'").and(predicate::str::contains("rose-pine")));
    e.cmd().args(["tui", "--render-text", "--theme", "nope"]).assert().failure();
    // Omarchy's current theme.
    let home = e.path("fakehome");
    write(&home.join(".local/state/omarchy/current"), "theme.name", "everforest\n");
    e.cmd().args(["theme", "sync-omarchy"]).env("HOME", &home).assert().success();
    assert!(e.run(&["theme", "show"]).contains("Everforest"));
    e.run(&["theme", "reset"]);
    assert!(e.run(&["theme", "show"]).contains("omp (oh-my-pi)"));
}

#[test]
fn yaml_recipes_are_rejected() {
    let e = Env::new();
    let file = e.path("old.yaml");
    fs::write(&file, "runtime: llama\nmodel: unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf\n").unwrap();
    e.cmd()
        .args(["recipe", "add", file.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("YAML recipes are no longer supported"));
    e.cmd()
        .args(["recipe", "add", "https://example.com/r.yaml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("YAML recipes are no longer supported"));
    // A stray YAML file in the deck is reported, not silently ignored.
    write(&e.path("home/config/recipes"), "stray.yml", "runtime: llama\n");
    let doc = e.run(&["doctor"]);
    assert!(doc.contains("stray.yml") && doc.contains("YAML recipes are no longer supported"), "{doc}");
}

#[test]
fn lmdeploy_dry_run() {
    let e = Env::new();
    let file = e.path("lmd.toml");
    fs::write(
        &file,
        "[model]\nrepo = \"Qwen/Qwen3-8B-AWQ\"\n[engine]\nname = \"lmdeploy\"\nport = 23333\nctx = 32768\nargs = [\"--model-format\", \"awq\"]\n",
    )
    .unwrap();
    e.run(&["recipe", "add", file.to_str().unwrap()]);
    let plan = e.run(&["use", "lmd", "--dry-run"]);
    assert!(
        plan.contains(
            "lmdeploy serve api_server Qwen/Qwen3-8B-AWQ --model-name lmd --server-name 0.0.0.0 --server-port 23333 \
             --session-len 32768 --model-format awq"
        ),
        "{plan}"
    );
    let found = e.run(&["recipe", "search", "--engine", "turbomind"]);
    assert!(found.contains("lmd"), "{found}");

    // The repo example: docker, no `{model}` in args, one HF cache mount.
    let ex = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ornith-122b-awq-lmdeploy.toml");
    e.run(&["recipe", "add", ex]);
    let plan = e.run(&["use", "ornith-1.5-122b-awq-lmdeploy-spark", "--dry-run", "--force"]);
    assert!(plan.contains("openmmlab/lmdeploy:latest lmdeploy serve api_server"), "{plan}");
    assert!(plan.contains("--session-len 262144 --model-format awq"), "{plan}");
    assert_eq!(plan.matches("/root/.cache/huggingface").count(), 1, "{plan}");
}

#[test]
fn four_spark_recipe_picks_its_launcher_from_the_fabric() {
    let e = Env::new();
    let name = "deepseek-v4.1-flash-1017-4x-spark-tensorfold";
    // This machine is spark1; hosts.toml lists the three Sparks the recipe hands Mia's scripts
    // (never contacted: the demo topologies answer for them).
    write(&e.path("home/config"), "config.toml", "[node]\nname = \"spark1\"\n");
    let hosts: String =
        (2..=4).map(|i| format!("[[host]]\nname = \"spark{i}\"\nurl = \"http://127.0.0.1:9\"\n\n")).collect();
    write(&e.path("home/config"), "hosts.toml", &hosts);
    let dry = |topo: Option<&str>, fabric: Option<&str>| {
        let mut c = e.cmd();
        if let Some(t) = topo {
            c.env("LMDECK_DEMO", "1").env("LMDECK_DEMO_TOPO", t);
        }
        if let Some(f) = fabric {
            c.env("LMDECK_FABRIC", f);
        }
        let out = c.args(["use", name, "--dry-run", "--force"]).assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap()
    };
    // The folded ring (rack 1-2-3-4, cabled 1-2-4-3-1): the ring launcher, workers in cabled order.
    let plan = dry(Some("fold4"), None);
    assert!(plan.contains("run:  ./start-switchless.sh"), "{plan}");
    assert!(plan.contains("→ spark2 → spark4 → spark3 →"), "{plan}");
    assert!(plan.contains("workers in cabled order: spark2, spark4, spark3"), "{plan}");
    let plan = dry(Some("ring4"), None);
    assert!(plan.contains("run:  ./start-switchless.sh"), "{plan}");
    assert!(plan.contains("workers in cabled order: spark2, spark3, spark4"), "{plan}");
    // Four of eight on a switch: the default launcher.
    let plan = dry(Some("switch8"), None);
    assert!(plan.contains("fabric: switch") && plan.contains("run:  ./start.sh"), "{plan}");
    // A lone machine has no Sparks to hand the scripts: the plan stops and says how many it needs.
    let lone = Env::new();
    lone.cmd()
        .args(["use", name, "--dry-run", "--force"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("hosts = 4").and(predicate::str::contains("3 more Spark")));
    // LMDECK_FABRIC sets the shape by hand, over the switch the demo shows.
    let plan = dry(Some("switch8"), Some("ring"));
    assert!(plan.contains("run:  ./start-switchless.sh") && plan.contains("set by LMDECK_FABRIC"), "{plan}");
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

/// `lmdeck add` on a local repo with start.sh / stop.sh, then a detached
/// start (start.sh backgrounds a server and exits) and a stop via stop.sh.
#[test]
fn add_wraps_a_repo_and_runs_it_detached() {
    let e = Env::new();
    let port = free_port();
    let repo = e.path("my-server");
    write(
        &repo,
        "start.sh",
        &format!(
            "#!/usr/bin/env bash\nset -e\nPORT=\"${{PORT:-{port}}}\"\nHOST=\"${{HOST:-127.0.0.1}}\"\n\
             nohup python3 -m http.server \"$PORT\" --bind 127.0.0.1 >server.out 2>&1 &\n\
             echo $! > server.pid\necho started-in-$(basename \"$PWD\")\n"
        ),
    );
    write(
        &repo,
        "stop.sh",
        "#!/usr/bin/env bash\nkill \"$(cat server.pid)\" && rm server.pid\necho stopped-by-script\n",
    );
    use std::os::unix::fs::PermissionsExt;
    for f in ["start.sh", "stop.sh"] {
        fs::set_permissions(repo.join(f), fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(&repo, ".gitignore", "server.*\n");
    // What http.server answers GET /v1/models with from the checkout: a 200, so it reads as up.
    write(&repo.join("v1"), "models", "{\"data\":[]}");
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);

    let out = e.run(&["add", repo.to_str().unwrap()]);
    assert!(out.contains("added my-server") && out.contains(&format!("{port} (from PORT)")), "{out}");
    let file = e.path("home/config/recipes/my-server.toml");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("[source]") && text.contains("command = [\"./start.sh\"]"), "{text}");
    // `nohup … &` in start.sh: it hands the server off, so the wrapper is detached.
    assert!(text.contains("detached = true") && text.contains(&format!("port = {port}")), "{text}");
    e.cmd().args(["add", repo.to_str().unwrap()]).assert().failure().stderr(predicate::str::contains("--force"));

    let checkout = e.path("home/cache/checkouts/my-server");
    let plan = e.run(&["use", "my-server", "--dry-run"]);
    assert!(plan.contains(&format!("cwd:  {}", checkout.display())), "{plan}");
    assert!(plan.contains(&format!("env:  PORT={port}")) && plan.contains("stop: ./stop.sh"), "{plan}");

    let out = e.run(&["use", "my-server"]);
    assert!(out.contains(&format!("my-server is up at http://localhost:{port}/v1")), "{out}");
    assert!(checkout.join("server.pid").exists());
    let v: serde_json::Value = serde_json::from_str(&e.run(&["list", "--json"])).unwrap();
    assert_eq!(v[0]["status"], "loaded", "{v}");
    assert!(e.run(&["stop", "my-server"]).contains("stopped my-server"));
    assert!(!checkout.join("server.pid").exists(), "stop.sh ran in the checkout");
    let logs = e.run(&["logs", "my-server"]);
    assert!(logs.contains("started-in-my-server") && logs.contains("stopped-by-script"), "{logs}");

    // `pull` updates the checkout to the new commit... but the recipe pins the old one.
    write(&repo, "README.md", "v2\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "v2"]);
    let out = e.run(&["pull", "my-server"]);
    assert!(out.contains("checkout"), "{out}");
    assert!(!checkout.join("README.md").exists(), "still on the pinned commit");
    e.run(&["add", repo.to_str().unwrap(), "--force"]);
    e.run(&["pull", "my-server"]);
    assert!(checkout.join("README.md").exists(), "re-adding pins the new commit");
}

/// A bare repo as the "remote" plus a working clone to push new commits from.
fn bare_remote(e: &Env) -> (std::path::PathBuf, std::path::PathBuf) {
    let work = e.path("work");
    write(&work, "start.sh", "#!/usr/bin/env bash\nPORT=\"${PORT:-8123}\"\nnohup sleep 1 >/dev/null 2>&1 &\n");
    write(&work, "stop.sh", "#!/usr/bin/env bash\necho stop\n");
    git(&work, &["init", "-q"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "one"]);
    let bare = e.path("remote.git");
    git(e.root.path(), &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    (work, bare)
}

fn push_commit(work: &Path, file: &str) {
    write(work, file, "new\n");
    git(work, &["add", "."]);
    git(work, &["commit", "-qm", file]);
    git(work, &["push", "-q", "origin", "HEAD:main"]);
}

/// `outdated --json` rows for deck recipes (the built-in wrapper is listed too, as not cloned).
fn outdated(e: &Env) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(&e.run(&["outdated", "--json"])).unwrap();
    let builtin = v.as_array().unwrap().iter().find(|r| r["ref"].as_str().unwrap().starts_with("builtin/")).unwrap();
    assert_eq!(builtin["update"]["state"], "not_cloned");
    v.as_array().unwrap().iter().filter(|r| r["ref"].as_str().unwrap().starts_with("deck/")).cloned().collect()
}

/// New commits upstream of a wrapped recipe show up as "⬆ update" and
/// `lmdeck pull` clears them; pinned recipes need `pull --latest`.
#[test]
fn wrapped_recipes_flag_upstream_commits() {
    let e = Env::new();
    let (work, bare) = bare_remote(&e);
    let bare_s = bare.to_str().unwrap();
    e.run(&["add", bare_s, "--name", "follow", "--ref", "main"]);
    e.run(&["add", bare_s, "--name", "pinned"]);
    let v = outdated(&e);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    assert!(v.as_array().unwrap().iter().all(|r| r["update"]["state"] == "current"), "{v}");
    assert!(!e.run(&["list"]).contains('⬆'));

    push_commit(&work, "two.txt");
    // Cached for ttl_hours; `outdated` always asks.
    assert!(!e.run(&["list"]).contains('⬆'), "fresh cache: no re-check yet");
    let text = e.run(&["outdated"]);
    assert!(text.contains("⬆ update") && text.contains("⬆ newer (pinned)"), "{text}");
    assert!(text.contains("lmdeck pull follow") && text.contains("lmdeck pull --latest pinned"), "{text}");
    let list = e.run(&["list"]);
    assert!(list.contains("follow ⬆ update") && list.contains("pinned ⬆ newer"), "{list}");
    let v: serde_json::Value = serde_json::from_str(&e.run(&["recipe", "list", "--json"])).unwrap();
    let row = v.as_array().unwrap().iter().find(|r| r["name"] == "follow").unwrap();
    assert_eq!(row["update"]["state"], "behind", "{row}");
    assert!(v.as_array().unwrap().iter().any(|r| r["update"].is_null()), "native recipes: no update field");
    let show = e.run(&["recipe", "show", "follow"]);
    assert!(show.contains("⬆ update available (main)"), "{show}");
    let v = outdated_fetch(&e);
    assert_eq!(v["follow"]["update"]["behind"], 1, "{v}");

    // pull clears the branch recipe; the pinned one keeps its pin...
    e.run(&["pull", "follow"]);
    assert!(e.path("home/cache/checkouts/follow/two.txt").exists());
    let out = e.run(&["pull", "pinned"]);
    assert!(out.contains("pull --latest pinned"), "{out}");
    assert!(!e.path("home/cache/checkouts/pinned/two.txt").exists());
    let list = e.run(&["list"]);
    assert!(!list.contains("follow ⬆") && list.contains("pinned ⬆ newer"), "{list}");
    // ...until --latest moves it (rewriting the deck recipe's ref).
    let before = fs::read_to_string(e.path("home/config/recipes/pinned.toml")).unwrap();
    let out = e.run(&["pull", "--latest", "pinned"]);
    assert!(out.contains("ref "), "{out}");
    let after = fs::read_to_string(e.path("home/config/recipes/pinned.toml")).unwrap();
    assert_ne!(before, after);
    assert!(e.path("home/cache/checkouts/pinned/two.txt").exists());
    assert!(!e.run(&["list"]).contains('⬆'));
    let v = outdated(&e);
    assert!(v.as_array().unwrap().iter().all(|r| r["update"]["state"] == "current"), "{v}");

    // A remote that is gone: unknown, never an error; listings still work.
    fs::rename(&bare, e.path("moved.git")).unwrap();
    let text = e.run(&["outdated"]);
    assert!(text.contains("? unknown"), "{text}");
    assert!(outdated(&e).as_array().unwrap().iter().all(|r| r["update"]["state"] == "unknown"));
    e.run(&["list"]);
    e.run(&["recipe", "show", "follow"]);

    // Checks can be turned off.
    let out =
        e.cmd().args(["outdated"]).env("LMDECK_NO_UPDATE_CHECK", "1").assert().success().get_output().stdout.clone();
    assert!(String::from_utf8(out).unwrap().contains("checks are off"));
}

fn outdated_fetch(e: &Env) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(&e.run(&["outdated", "--json", "--fetch"])).unwrap();
    v.as_array().unwrap().iter().map(|r| (r["name"].as_str().unwrap().to_string(), r.clone())).collect()
}

/// `[updates] ttl_hours = 0`: listings re-check every time.
#[test]
fn zero_ttl_rechecks_on_list() {
    let e = Env::new();
    let (work, bare) = bare_remote(&e);
    write(&e.path("home/config"), "config.toml", "[updates]\nttl_hours = 0\n");
    e.run(&["add", bare.to_str().unwrap(), "--name", "w", "--ref", "main"]);
    assert!(!e.run(&["list"]).contains('⬆'));
    push_commit(&work, "x.txt");
    assert!(e.run(&["list"]).contains("w ⬆ update"));
}

#[test]
fn tui_run_on_picker_and_topology_frames() {
    let e = Env::new();
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    e.run(&["host", "add", "spark9", &format!("http://127.0.0.1:{dead}")]);
    let frame =
        e.run(&["tui", "--render-text", "--run-on", "--select", "qwen3-4b-gguf", "--width", "130", "--height", "36"]);
    for want in ["Run qwen3-4b-gguf on", "⌂", "spark9", "no weights", "weights never move", "RUN ON"] {
        assert!(frame.contains(want), "missing {want:?} in\n{frame}");
    }
    let frame = e.run(&["tui", "--render-text", "--tab", "topology", "--width", "130", "--height", "30"]);
    for want in ["Topology", "controller", "spark9", "offline", "(no links matched)"] {
        assert!(frame.contains(want), "missing {want:?} in\n{frame}");
    }
}

#[test]
fn doctor_reports_newer_schemas_and_skips_them() {
    let e = Env::new();
    let deck = e.path("home/config/recipes");
    write(&deck, "mine.toml", &format!("schema = 1\n{QWEN_TOML}"));
    write(&deck, "future.toml", &format!("schema = 9\nmin_lmdeck = \"4.0.0\"\n{QWEN_TOML}"));
    let list = e.run(&["list"]);
    assert!(list.contains("mine") && !list.contains("future"), "{list}");
    let doc = e.run(&["doctor"]);
    assert!(doc.contains("schemas"), "{doc}");
    assert!(doc.contains("future.toml") && doc.contains("needs lmdeck >= 4.0.0 / schema 9"), "{doc}");
    let j: serde_json::Value = serde_json::from_str(&e.run(&["doctor", "--json"])).unwrap();
    assert_eq!(j["schema"], 1);
    assert_eq!(j["unsupported_schemas"].as_array().unwrap().len(), 1);
    // A newer config: defaults, a warning, and doctor names it.
    write(&e.path("home/config"), "config.toml", "schema = 2\nmodels_dir = \"/nowhere\"\n");
    let out = e.cmd().args(["doctor"]).assert().success().get_output().clone();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(stderr.contains("needs lmdeck newer than") && stderr.contains("using the defaults"), "{stderr}");
    assert!(stdout.contains("config.toml"), "{stdout}");
}

#[test]
fn adding_a_repo_a_builtin_wraps_adds_the_builtin_offline() {
    let e = Env::new();
    let out = e.run(&["add", "MiaAI-Lab/DeepSeek-V4.1-Flash-1017-4x-DGX-Sparks-TensorFold"]);
    assert!(out.contains("built-in recipe deepseek-v4.1-flash-1017-4x-spark-tensorfold already wraps"), "{out}");
    let text =
        fs::read_to_string(e.path("home/config/recipes/deepseek-v4.1-flash-1017-4x-spark-tensorfold.toml")).unwrap();
    assert!(text.contains("start-switchless.sh") && text.contains("hosts = 4"), "{text}");
    assert!(!e.path("home/cache/checkouts").join("deepseek-v4.1-flash-1017-4x-spark-tensorfold").exists());
}

/// `[harness] hub = true` survives every other `lmdeck harness` setting (each
/// one rewrites the whole `[harness]` table), `lmdeck harness` shows it, off
/// removes it, and anything but on or off is refused.
#[test]
fn the_hub_flag_survives_the_other_harness_settings() {
    let e = Env::new();
    let config = || fs::read_to_string(e.path("home/config/config.toml")).unwrap();
    assert!(e.run(&["harness", "hub", "on"]).contains("hub on"));
    e.run(&["harness", "enable", "omp"]);
    e.run(&["harness", "export", "on"]);
    e.run(&["harness", "hosts", "none"]);
    assert!(config().contains("hub = true") && config().contains("\"omp\""), "{}", config());
    assert!(e.run(&["harness"]).contains("hub: on"));
    assert!(e.run(&["harness", "hub", "off"]).contains("hub off"));
    assert!(!config().contains("hub ="), "{}", config());
    assert!(e.run(&["harness"]).contains("hub: off"));
    e.cmd().args(["harness", "hub", "maybe"]).assert().failure().stderr(predicate::str::contains("'on' or 'off'"));
}

/// A host taken out of hosts.toml leaves the hub's record at the next push
/// instead of counting as one that missed a look, while a host still listed
/// that doesn't answer adds up its misses there.
#[test]
fn a_host_taken_out_of_hosts_toml_leaves_the_hubs_record() {
    let e = Env::new();
    e.run(&["harness", "hub", "on"]);
    // Nothing listens on either port: every look at them fails.
    let host = |name: &str| format!("[[host]]\nname = \"{name}\"\nurl = \"http://127.0.0.1:{}\"\n\n", free_port());
    let (a, b) = (host("a"), host("b"));
    let sync = || e.cmd().env_remove("LMDECK_NO_SYNC").args(["harness", "sync"]).assert().success();
    let misses = || {
        let text = fs::read_to_string(e.path("home/state/harness-hub.json")).unwrap();
        let record: lmdeck::hub::LastPush = serde_json::from_str(&text).unwrap();
        record.hosts.hosts.into_iter().map(|(h, s)| (h, s.misses)).collect::<Vec<_>>()
    };
    write(&e.path("home/config"), "hosts.toml", &format!("{a}{b}"));
    sync();
    assert_eq!(misses(), [("a".to_string(), 1), ("b".to_string(), 1)]);
    write(&e.path("home/config"), "hosts.toml", &b);
    sync();
    assert_eq!(misses(), [("b".to_string(), 2)], "a is still in the record");
}
