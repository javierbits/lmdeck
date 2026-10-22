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
        c.env("LMDECK_HOME", self.path("home"))
            .env("LMDECK_MODELS_DIR", self.path("models"))
            .env("HF_HUB_CACHE", self.path("hf"))
            .env("NO_COLOR", "1")
            .env_remove("HF_TOKEN");
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

const LMSWITCH_YAML: &str = r#"# Qwen3 4B Q4 on llama.cpp
runtime: llama
display_name: "Qwen3 4B"
model: "unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf"
port: 8090
ctx: 16384
gpu_layers: 99
"#;

const DUAL_YAML: &str = r#"runtime: vllm-dual
model: "nvidia/Qwen3.6-35B-A3B-NVFP4"
port: 8200
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
fn doctor_json_reports_hardware_and_recipes() {
    let e = Env::new();
    let out = e.run(&["doctor", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["hardware"]["total_memory_gb"].as_f64().unwrap() > 0.0);
    assert!(v["inventory"]["engines"]["llama.cpp"].is_object());
    assert_eq!(v["recipes"].as_array().unwrap().len(), 8);
    assert!(v["paths"]["config"].as_str().unwrap().ends_with("config/config.toml"));
    // Human output too.
    assert!(e.run(&["doctor"]).contains("engines"));
}

#[test]
fn local_source_with_lmswitch_yaml() {
    let e = Env::new();
    let src = e.path("src");
    write(&src, "qwen3-4b.yaml", LMSWITCH_YAML);
    write(&src, "dual.yaml", DUAL_YAML);
    write(&src, "notes.yaml", "title: not a recipe\n");
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
    assert!(plan.contains("--hf-repo unsloth/Qwen3-4B-GGUF --hf-file Qwen3-4B-Q4_K_M.gguf"), "{plan}");
    assert!(plan.contains("--port 8090") && plan.contains("--n-gpu-layers 99") && plan.contains("-fit off"), "{plan}");
    let pull = e.run(&["pull", "qwen3-4b", "--dry-run"]);
    assert!(pull.contains("download unsloth/Qwen3-4B-GGUF --include Qwen3-4B-Q4_K_M.gguf --local-dir"), "{pull}");

    // Once the GGUF is on disk, the plan points at the file.
    write(&e.path("models/unsloth/Qwen3-4B-GGUF"), "Qwen3-4B-Q4_K_M.gguf", "GGUF");
    let plan = e.run(&["use", "qwen3-4b", "--dry-run"]);
    assert!(plan.contains("--model ") && plan.contains("models/unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf"), "{plan}");
    let dl = e.run(&["list", "--downloaded"]);
    assert!(dl.contains("unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf") && dl.contains("qwen3-4b"), "{dl}");

    // Multi-node recipes are refused.
    e.cmd().args(["use", "mine/dual"]).assert().failure().stderr(predicate::str::contains("multi-node"));

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
    assert!(cfg.contains("/srv/models") && cfg.contains("github:jvr0x/ai-models"), "{cfg}");
    e.cmd().arg("init").assert().failure();
    e.run(&["init", "--force"]);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn start_list_stop_custom_server() {
    let e = Env::new();
    let port = free_port();
    let recipe = format!(
        "name = \"web\"\n[model]\nsource = \"local\"\npath = \"{}\"\n[engine]\nname = \"custom\"\nport = {port}\nready_timeout = 30\ncommand = [\"python3\", \"-m\", \"http.server\", \"{{port}}\", \"--bind\", \"127.0.0.1\"]\n",
        e.path("models").display()
    );
    let file = e.path("web.toml");
    fs::write(&file, recipe).unwrap();
    e.run(&["recipe", "add", file.to_str().unwrap()]);

    let out = e.run(&["use", "web"]);
    assert!(out.contains(&format!("web is up at http://localhost:{port}/v1")), "{out}");
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
