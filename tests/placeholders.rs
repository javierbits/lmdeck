//! Host-config placeholders: a command names what its machine has configured
//! (`{hf_home}`, the `[engines]` binaries and their directories) instead of
//! a path, and a placeholder lmdeck can't fill fails the plan with a message
//! instead of reaching the command as `{x}`.

use lmdeck::config::EngineOverrides;
use lmdeck::engine::{plan, PlanCtx};
use lmdeck::recipe::Recipe;
use proptest::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A planning context like `PlanCtx::new`'s, with `overrides` as `[engines]`.
fn ctx(overrides: EngineOverrides) -> PlanCtx {
    PlanCtx {
        models_dir: "/m".into(),
        host: "0.0.0.0".into(),
        overrides,
        home: "/home/u".into(),
        local_present: false,
        bins: Default::default(),
        checkouts_dir: "/c/checkouts".into(),
        port: None,
        fabric: None,
        vars: Default::default(),
    }
}

/// A custom recipe running `command` (a TOML array), with `more` TOML after `[engine]`.
fn custom(command: &str, more: &str) -> Recipe {
    Recipe::parse(&format!("name = \"t\"\n[engine]\nname = \"custom\"\ncommand = {command}\n{more}"), "t").unwrap()
}

/// An executable file `name` in `dir` (created with its parents).
fn exe(dir: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// The message of a plan that must fail.
fn refused(r: &Recipe, c: &PlanCtx) -> String {
    format!("{:#}", plan(r, c).expect_err("the plan should fail"))
}

#[test]
fn commands_name_the_binaries_this_machine_has() {
    let tmp = tempfile::tempdir().unwrap();
    let build = tmp.path().join("llama.cpp/build/bin");
    let llama = exe(&build, "llama-server");
    let python = exe(&tmp.path().join("venv/bin"), "python");
    let docker = exe(&tmp.path().join("bin"), "docker");
    let tabby = tmp.path().join("tabbyAPI");
    fs::create_dir_all(&tabby).unwrap();
    fs::write(tabby.join("main.py"), "").unwrap();
    let s = |p: &Path| p.display().to_string();
    let o = EngineOverrides {
        llama_server: Some(s(&llama)),
        python: Some(s(&python)),
        docker: Some(s(&docker)),
        tabbyapi_dir: Some(s(&tabby)),
        ..Default::default()
    };
    let r = custom(
        r#"["{llama_server_dir}/rpc-server", "-p", "50052"]"#,
        "[engine.env]\nLS = \"{llama_server}\"\nPY = \"{python}\"\nVENV = \"{python_dir}\"\nD = \"{docker}\"\nTABBY = \"{tabbyapi_dir}\"",
    );
    let p = plan(&r, &ctx(o)).unwrap();
    assert_eq!(p.argv, [format!("{}/rpc-server", build.display()), "-p".into(), "50052".into()]);
    assert_eq!(p.env["LS"], s(&llama));
    assert_eq!(p.env["PY"], s(&python));
    assert_eq!(p.env["VENV"], s(&tmp.path().join("venv/bin")));
    assert_eq!(p.env["D"], s(&docker));
    assert_eq!(p.env["TABBY"], s(&tabby));
}

#[test]
fn unknown_or_missing_placeholders_fail_the_plan() {
    let c = ctx(EngineOverrides::default());
    let e = refused(&custom(r#"["srv", "--model", "{modle}"]"#, ""), &c);
    assert!(e.contains("{modle}") && e.contains("the command"), "{e}");
    let e = refused(&custom(r#"["srv"]"#, "[engine.env]\nX = \"{peer_adr}\""), &c);
    assert!(e.contains("{peer_adr}") && e.contains("env X"), "{e}");
    // Multi-node placeholders mean nothing to a one-machine recipe.
    let e = refused(&custom(r#"["srv", "{head_addr}"]"#, ""), &c);
    assert!(e.contains("{head_addr}"), "{e}");
    // A binary this machine doesn't have says which [engines] key to set.
    let o = EngineOverrides { vllm: Some("/nonexistent/lmdeck-test/vllm".into()), ..Default::default() };
    let e = refused(&custom(r#"["{vllm}", "serve"]"#, ""), &ctx(o));
    assert!(e.contains("{vllm}") && e.contains("[engines] vllm"), "{e}");
    let o = EngineOverrides { tabbyapi_dir: Some("/nonexistent/lmdeck-test/tabby".into()), ..Default::default() };
    let e = refused(&custom(r#"["sh", "-c", "cd {tabbyapi_dir}"]"#, ""), &ctx(o));
    assert!(e.contains("{tabbyapi_dir}") && e.contains("[engines] tabbyapi_dir"), "{e}");
}

#[test]
fn shell_and_json_braces_and_unplanned_peers_pass_through() {
    let c = ctx(EngineOverrides::default());
    let r = custom(r#"["sh", "-c", "exec \"${HOME}/x\" --cfg '{\"a\":1}' ${PORT:-8080} {NOT_OURS}"]"#, "");
    assert!(plan(&r, &c).is_ok());
    // One machine's plan leaves the peers to the start (`lmdeck use` picks them).
    let r = custom(r#"["./start.sh"]"#, "[engine.env]\nW = \"{peer_addr}\"\n[hardware]\nhosts = 2");
    assert_eq!(plan(&r, &c).unwrap().env["W"], "{peer_addr}");
    // `{home}` is this machine's home, `{hf_home}` the Hugging Face home under it.
    let r = custom(r#"["ls", "{home}", "{hf_home}"]"#, "");
    let p = plan(&r, &c).unwrap();
    assert_eq!(p.argv[1], "/home/u");
    assert!(std::env::var_os("HF_HOME").is_some() || p.argv[2] == "/home/u/.cache/huggingface", "{:?}", p.argv);
}

proptest! {
    /// A plan fails exactly when a placeholder is not one lmdeck fills.
    #[test]
    fn a_plan_fails_exactly_on_an_unknown_name(
        names in prop::collection::vec(
            prop::sample::select(vec!["name", "port", "model", "models_dir", "home", "hf_home", "modle", "ports", "gpu"]),
            1..5,
        )
    ) {
        let unknown = ["modle", "ports", "gpu"];
        let args: Vec<String> = names.iter().map(|n| format!("\"{{{n}}}\"")).collect();
        let r = custom(&format!("[\"srv\", {}]", args.join(", ")), "");
        let ok = plan(&r, &ctx(EngineOverrides::default())).is_ok();
        prop_assert_eq!(ok, !names.iter().any(|n| unknown.contains(n)));
    }
}

/// Runs lmdeck with its home in `root/home`, plus `env`.
fn lmdeck(root: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lmdeck"));
    c.args(args)
        .env("HOME", root.join("home"))
        .env("LMDECK_HOME", root.join("home"))
        .env("LMDECK_MODELS_DIR", root.join("models"))
        .env("HF_HUB_CACHE", root.join("hf"))
        .env("LMDECK_NO_SYNC", "1")
        .env("LMDECK_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .env_remove("HF_HOME");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

/// Adds the recipe `body` to the deck in `root`.
fn add(root: &Path, name: &str, body: &str) {
    let f = root.join(format!("{name}.toml"));
    fs::write(&f, body).unwrap();
    let o = lmdeck(root, &[], &["recipe", "add", f.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn hf_home_is_the_one_lmdeck_mounts() {
    let tmp = tempfile::tempdir().unwrap();
    add(tmp.path(), "t", &custom_text("[\"./start.sh\"]", "[engine.env]\nHF = \"{hf_home}\""));
    let out = |env: &[(&str, &str)]| {
        String::from_utf8_lossy(&lmdeck(tmp.path(), env, &["use", "t", "--dry-run"]).stdout).into_owned()
    };
    let home = tmp.path().join("home");
    assert!(out(&[]).contains(&format!("env:  HF={}/.cache/huggingface", home.display())), "{}", out(&[]));
    assert!(out(&[("HF_HOME", "/data/hf")]).contains("env:  HF=/data/hf"), "{}", out(&[("HF_HOME", "/data/hf")]));
}

/// The text of [`custom`]'s recipe.
fn custom_text(command: &str, more: &str) -> String {
    format!("name = \"t\"\n[engine]\nname = \"custom\"\ncommand = {command}\n{more}")
}

#[test]
fn a_node_command_is_checked_when_the_cluster_is_planned() {
    let tmp = tempfile::tempdir().unwrap();
    let nodes = |worker: &str| {
        format!(
            "name = \"duo\"\n[engine]\nname = \"custom\"\nport = 8999\n\
             [[nodes]]\nname = \"head\"\nrole = \"head\"\nhost = \"local\"\ncommand = [\"sh\", \"-c\", \"sleep 1\"]\n\
             [[nodes]]\nname = \"w1\"\nrole = \"worker\"\nhost = \"local\"\ncommand = [{worker}]\n"
        )
    };
    // Machine settings are filled where the node runs, so the plan keeps them.
    add(tmp.path(), "duo", &nodes("\"{llama_server_dir}/rpc-server\", \"-p\", \"{port}\""));
    let o = lmdeck(tmp.path(), &[], &["use", "duo", "--dry-run"]);
    let plan = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success() && plan.contains("{llama_server_dir}/rpc-server") && plan.contains("{port}"), "{plan}");
    // A name nothing fills fails the plan, naming the node.
    let f = tmp.path().join("bad.toml");
    fs::write(&f, nodes("\"rpc-server\", \"-H\", \"{head_adr}\"").replace("duo", "bad")).unwrap();
    assert!(lmdeck(tmp.path(), &[], &["recipe", "add", f.to_str().unwrap()]).status.success());
    let o = lmdeck(tmp.path(), &[], &["use", "bad", "--dry-run"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success() && err.contains("{head_adr}") && err.contains("[[nodes]] w1"), "{err}");
}
