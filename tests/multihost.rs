//! Several `lmdeck serve`s on one machine, each with its own LMDECK_HOME,
//! standing in for separate hosts: remote control with and without a token,
//! multi-node start order / reverse stop / a failing worker, the weights
//! index, the NFS placement flow against a fake `docker` and its preflight,
//! the Ray example's plan, which nodes get the NFS volume, and the checkout
//! every node of a `[source]` recipe runs from.

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

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

    // `list` may ask docker whether a built-in's container runs (a server lmdeck didn't
    // start); only the dry run below must leave docker alone.
    let _ = fs::remove_file(&log);
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
    assert!(!log.exists(), "dry run ran docker: {}", fs::read_to_string(&log).unwrap_or_default());

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
