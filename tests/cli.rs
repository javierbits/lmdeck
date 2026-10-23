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
            .env_remove("HF_TOKEN")
            .env_remove("LMDECK_DEMO")
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
fn doctor_json_reports_hardware_and_recipes() {
    let e = Env::new();
    let out = e.run(&["doctor", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["hardware"]["total_memory_gb"].as_f64().unwrap() > 0.0);
    assert!(v["inventory"]["engines"]["llama.cpp"].is_object());
    assert_eq!(v["recipes"].as_array().unwrap().len(), 9);
    assert!(v["paths"]["config"].as_str().unwrap().ends_with("config/config.toml"));
    // Human output too.
    assert!(e.run(&["doctor"]).contains("engines"));
}

#[test]
fn local_source_with_toml_recipes() {
    let e = Env::new();
    let src = e.path("src");
    write(&src, "qwen3-4b.toml", QWEN_TOML);
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
    assert!(cfg.contains("/srv/models") && !cfg.contains("ai-models"), "{cfg}");
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
    let log = fs::read_to_string(e.path("home/state/logs/web.log")).unwrap_or_default();
    let probe = e.cmd().args(["list", "--json"]).env("LMDECK_DEBUG", "1").output().unwrap();
    assert!(
        out.contains(&format!("web is up at http://localhost:{port}/v1")),
        "{out}\n--- log ---\n{log}\n--- probe ---\n{}",
        String::from_utf8_lossy(&probe.stderr)
    );
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
        "[model]\nrepo = \"Qwen/Qwen3-8B-AWQ\"\n[engine]\nname = \"lmdeploy\"\nctx = 32768\nargs = [\"--model-format\", \"awq\"]\n",
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
