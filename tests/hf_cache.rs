//! Hugging Face weights, GGUFs included, live in the Hugging Face cache:
//! llama.cpp and TabbyAPI recipes load them from
//! `<hub>/models--org--name/snapshots/<rev>/…` first (the models dir keeps
//! local-only weights), `lmdeck pull` downloads there, `{model_dir}` names the
//! directory holding a recipe's first file, and the listings, the weights index
//! and the fit checks see each recipe's own file.

use lmdeck::models;
use lmdeck::recipe::Recipe;
use lmdeck::weights::{HostWeights, Index, Inventory, Item};
use proptest::prelude::*;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const VL: &str = "unsloth/Qwen3-VL-8B-Instruct-GGUF";
const Q4: &str = "Qwen3-VL-8B-Instruct-UD-Q4_K_XL.gguf";

/// One machine (called `ctl`): its home, models dir and HF hub cache in a temp dir.
struct Env {
    root: tempfile::TempDir,
}

impl Env {
    /// A machine with an empty deck, models dir and hub.
    fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        for d in ["home/config/recipes", "models", "hub"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        fs::write(root.path().join("home/config/config.toml"), "[node]\nname = \"ctl\"\n").unwrap();
        Env { root }
    }

    /// `p` under this machine's temp dir.
    fn path(&self, p: &str) -> PathBuf {
        self.root.path().join(p)
    }

    /// `lmdeck` as this machine runs it: `HF_HUB_CACHE` is the hub, nothing synced.
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lmdeck"));
        c.env("HOME", self.path("home"))
            .env("LMDECK_HOME", self.path("home"))
            .env("LMDECK_MODELS_DIR", self.path("models"))
            .env("HF_HUB_CACHE", self.path("hub"))
            .env("NO_COLOR", "1")
            .env("LMDECK_NO_SYNC", "1")
            .env("LMDECK_NO_UPDATE_CHECK", "1")
            .env_remove("HF_HOME")
            .env_remove("HF_TOKEN")
            .env_remove("LMDECK_DEMO");
        c
    }

    /// Stdout of `c` run with `args`; panics with both streams when it fails.
    fn ok(&self, mut c: Command, args: &[&str]) -> String {
        let o: Output = c.args(args).output().unwrap();
        let (so, se) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        assert!(o.status.success(), "lmdeck {args:?} failed\n--- stdout ---\n{so}\n--- stderr ---\n{se}");
        so.into_owned()
    }

    /// Stdout of `lmdeck args`, which must succeed.
    fn run(&self, args: &[&str]) -> String {
        self.ok(self.cmd(), args)
    }

    /// Stdout and stderr of `lmdeck args`, which must fail.
    fn fail(&self, args: &[&str]) -> String {
        let o = self.cmd().args(args).output().unwrap();
        assert!(!o.status.success(), "lmdeck {args:?} should fail: {}", String::from_utf8_lossy(&o.stdout));
        format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
    }

    /// Writes the deck recipe `name`.
    fn recipe(&self, name: &str, body: &str) {
        fs::write(self.path(&format!("home/config/recipes/{name}.toml")), body).unwrap();
    }

    /// `list --json`'s row for the deck recipe `name`.
    fn row(&self, name: &str) -> Value {
        let v: Value = serde_json::from_str(&self.run(&["list", "--json"])).unwrap();
        v.as_array().unwrap().iter().find(|r| r["name"] == name).cloned().unwrap_or_else(|| panic!("no {name} in {v}"))
    }
}

impl Drop for Env {
    /// Stops whatever a test started.
    fn drop(&mut self) {
        let _ = self.cmd().args(["stop", "--all"]).output();
    }
}

/// `repo`'s cache entry in `hub`.
fn repo_dir(hub: &Path, repo: &str) -> PathBuf {
    hub.join(format!("models--{}", repo.replace('/', "--")))
}

/// `body` at `p`, parents created.
fn put(p: &Path, body: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// `files` (path, bytes) in `repo`'s snapshot `commit`, as moved-in weights sit
/// there: plain files and no blobs. `refs` (`main`, a branch) name the commit.
fn cached(hub: &Path, repo: &str, commit: &str, refs: &[&str], files: &[(&str, usize)]) -> PathBuf {
    let dir = repo_dir(hub, repo);
    for r in refs {
        put(&dir.join("refs").join(r), commit.as_bytes());
    }
    let snap = dir.join("snapshots").join(commit);
    for (f, n) in files {
        put(&snap.join(f), &vec![0u8; *n]);
    }
    snap
}

/// The same laid out as `hf download` does it: blobs, and snapshot symlinks to them.
fn linked(hub: &Path, repo: &str, commit: &str, refs: &[&str], files: &[(&str, usize)]) -> PathBuf {
    let dir = repo_dir(hub, repo);
    for r in refs {
        put(&dir.join("refs").join(r), commit.as_bytes());
    }
    let snap = dir.join("snapshots").join(commit);
    for (f, n) in files {
        let blob = dir.join("blobs").join(format!("{commit}-{}", f.replace('/', "_")));
        put(&blob, &vec![0u8; *n]);
        fs::create_dir_all(snap.join(f).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&blob, snap.join(f)).unwrap();
    }
    snap
}

/// A llama.cpp recipe loading `files` (the first is the model) from `repo`, with `more` after `[engine]`.
fn llama(repo: &str, files: &[&str], more: &str) -> String {
    format!("[model]\nrepo = \"{repo}\"\nfiles = {files:?}\n[engine]\nname = \"llama.cpp\"\nport = 8109\n{more}")
}

#[test]
fn llama_cpp_loads_its_gguf_from_the_hf_cache_and_pulls_there() {
    let e = Env::new();
    let snap = cached(&e.path("hub"), VL, "c0ffee", &["main"], &[(Q4, 3000), ("mmproj-F16.gguf", 700)]);
    let args = "args = [\"--mmproj\", \"{model_dir}/mmproj-F16.gguf\"]\n";
    e.recipe("vl", &llama(VL, &[Q4, "mmproj-F16.gguf"], args));
    let plan = e.run(&["use", "vl", "--dry-run"]);
    assert!(plan.contains(&format!("--model {}", snap.join(Q4).display())), "{plan}");
    assert!(plan.contains(&format!("--mmproj {}", snap.join("mmproj-F16.gguf").display())), "{plan}");
    assert!(!plan.contains("--hf-repo"), "{plan}");
    // On disk, and sized by its own file, not the whole repo.
    let row = e.row("vl");
    assert_eq!((row["downloaded"].as_bool(), row["size_bytes"].as_u64()), (Some(true), Some(3000)), "{row}");
    // `pull` fills the cache: no --local-dir.
    let pull = e.run(&["pull", "vl", "--dry-run"]);
    assert!(pull.contains(&format!("download {VL} --include {Q4} --include mmproj-F16.gguf")), "{pull}");
    assert!(!pull.contains("--local-dir"), "{pull}");
}

#[test]
fn a_revision_and_split_shards_in_a_subfolder_resolve_through_the_hub_layout() {
    let e = Env::new();
    let repo = "unsloth/Big-GGUF";
    let shard = |i: u32| format!("UD-IQ4_NL/Big-UD-IQ4_NL-0000{i}-of-00002.gguf");
    // `main` is another commit: the recipe asks for v2.
    linked(&e.path("hub"), repo, "aaa111", &["main"], &[("README.md", 10)]);
    let files = [(shard(1), 4000), (shard(2), 1500), ("mmproj-F16.gguf".to_string(), 300)];
    let files: Vec<(&str, usize)> = files.iter().map(|(f, n)| (f.as_str(), *n)).collect();
    let snap = linked(&e.path("hub"), repo, "bbb222", &["v2"], &files);
    e.recipe(
        "big",
        &format!(
            "[model]\nrepo = \"{repo}\"\nrevision = \"v2\"\nfiles = [\"{}\"]\n[engine]\nname = \"llama.cpp\"\nport = 8110\n\
             args = [\"--mmproj\", \"{{model_dir}}/../mmproj-F16.gguf\"]\n",
            shard(1)
        ),
    );
    let plan = e.run(&["use", "big", "--dry-run"]);
    assert!(plan.contains(&format!("--model {}", snap.join(shard(1)).display())), "{plan}");
    assert!(plan.contains(&format!("--mmproj {}/UD-IQ4_NL/../mmproj-F16.gguf", snap.display())), "{plan}");
    // Both shards count, through their symlinks.
    assert_eq!(e.row("big")["size_bytes"], 5500);
}

#[test]
fn another_quant_of_the_repo_is_not_this_recipes_weights() {
    let e = Env::new();
    let repo = "LiquidAI/LFM2.5-24B-A2B-GGUF";
    let q4 = "LFM2.5-24B-A2B-Q4_K_M.gguf";
    let files = [(q4, 3000), ("LFM2.5-24B-A2B-Q5_K_M.gguf", 5000), ("README.md", 100)];
    cached(&e.path("hub"), repo, "0912d3", &["main"], &files);
    e.recipe("q4", &llama(repo, &[q4], ""));
    e.recipe("q8", &llama(repo, &["LFM2.5-24B-A2B-Q8_0.gguf"], ""));
    // Another host (offline) makes `list` consult the weights index.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    put(
        &e.path("home/config/hosts.toml"),
        format!("[[host]]\nname = \"gone\"\nurl = \"http://127.0.0.1:{dead}\"\n").as_bytes(),
    );
    let (four, eight) = (e.row("q4"), e.row("q8"));
    // On disk at the Q4 file's size: what the fit check counts, not the repo's.
    assert_eq!((four["downloaded"].as_bool(), four["size_bytes"].as_u64()), (Some(true), Some(3000)), "{four}");
    assert_eq!(eight["downloaded"], false, "{eight}");
    // The weights index has the Q4 here, and no Q8 anywhere.
    assert_eq!((&four["weights_on"], &eight["weights_on"]), (&json!(["local"]), &json!([])));
    // With nothing on disk for it, the Q8 still streams through llama-server.
    assert!(e.run(&["use", "q8", "--dry-run"]).contains("--hf-repo"));
    // The listings see the repo's GGUFs at their size.
    let found: Value = serde_json::from_str(&e.run(&["list", "-d", "--json"])).unwrap();
    let d = found.as_array().unwrap().iter().find(|d| d["id"] == repo).unwrap_or_else(|| panic!("{found}"));
    assert_eq!((d["size"].as_u64(), d["kind"].as_str()), (Some(8100), Some("gguf")), "{d}");
    let w: Value = serde_json::from_str(&e.run(&["weights", "--json"])).unwrap();
    let it = w["weights"].as_array().unwrap().iter().find(|i| i["model"] == repo).unwrap_or_else(|| panic!("{w}"));
    assert_eq!((it["size"].as_u64(), it["kind"].as_str()), (Some(8100), Some("gguf")), "{it}");
    // Each host reports the GGUFs a repo holds, so the index can match a recipe's file.
    let items = w["hosts"]["local"]["inventory"]["items"].as_array().unwrap();
    let item = items.iter().find(|i| i["id"] == repo).unwrap();
    assert_eq!(item["ggufs"], json!([[q4, 3000], ["LFM2.5-24B-A2B-Q5_K_M.gguf", 5000]]), "{item}");
}

#[test]
fn local_only_ggufs_stay_in_the_models_dir_and_hf_home_moves_the_hub() {
    let e = Env::new();
    let repo = "javierbits/Javi0.3-4B-GGUF";
    let local = e.path("models").join(repo);
    put(&local.join("Javi0.3-4B.Q8_0.gguf"), &[0u8; 2000]);
    e.recipe("javi", &llama(repo, &["Javi0.3-4B.Q8_0.gguf"], "args = [\"--mmproj\", \"{model_dir}/mm.gguf\"]\n"));
    let plan = e.run(&["use", "javi", "--dry-run"]);
    assert!(plan.contains(&format!("--model {}", local.join("Javi0.3-4B.Q8_0.gguf").display())), "{plan}");
    assert!(plan.contains(&format!("--mmproj {}", local.join("mm.gguf").display())), "{plan}");
    // HF_HOME without HF_HUB_CACHE: the hub is $HF_HOME/hub, and the cache comes first.
    let hf_home = e.path("hfhome");
    let snap = cached(&hf_home.join("hub"), repo, "abc123", &["main"], &[("Javi0.3-4B.Q8_0.gguf", 2500)]);
    let mut c = e.cmd();
    c.env_remove("HF_HUB_CACHE").env("HF_HOME", &hf_home);
    let plan = e.ok(c, &["use", "javi", "--dry-run"]);
    assert!(plan.contains(&format!("--model {}", snap.join("Javi0.3-4B.Q8_0.gguf").display())), "{plan}");
    assert!(plan.contains(&format!("--mmproj {}", snap.join("mm.gguf").display())), "{plan}");
}

#[test]
fn tabbyapi_serves_the_cached_branch() {
    let e = Env::new();
    let repo = "turboderp/Qwen3-30B-A3B-exl3";
    let snap = cached(&e.path("hub"), repo, "e3c0", &["4.0bpw"], &[("config.json", 10), ("model.safetensors", 900)]);
    e.recipe(
        "tabby",
        &format!("[model]\nrepo = \"{repo}\"\nrevision = \"4.0bpw\"\n[engine]\nname = \"tabbyapi\"\nport = 5005\n"),
    );
    let plan = e.run(&["use", "tabby", "--dry-run"]);
    let dir = snap.parent().unwrap().display().to_string();
    assert!(plan.contains(&format!("--model-dir {dir} --model-name e3c0")), "{plan}");
    let row = e.row("tabby");
    assert_eq!((row["downloaded"].as_bool(), row["size_bytes"].as_u64()), (Some(true), Some(910)), "{row}");
    let pull = e.run(&["pull", "tabby", "--dry-run"]);
    assert!(pull.contains(&format!("download {repo} --revision 4.0bpw")) && !pull.contains("--local-dir"), "{pull}");
}

#[test]
fn model_dir_without_the_weights_on_this_machine_fails_the_plan() {
    let e = Env::new();
    e.recipe("vl", &llama(VL, &[Q4], "args = [\"--mmproj\", \"{model_dir}/mmproj-F16.gguf\"]\n"));
    let err = e.fail(&["use", "vl", "--dry-run"]);
    let missing = format!("{VL}/{Q4} isn't on this machine");
    assert!(err.contains("{model_dir} can't be filled") && err.contains(&missing), "{err}");
    assert!(err.contains("`lmdeck pull` downloads it"), "{err}");
}

/// The `--local-dir` of `pull --dry-run` for the deck recipe `name`, if any.
fn local_dir(e: &Env, name: &str) -> Option<PathBuf> {
    let argv = shlex::split(e.run(&["pull", name, "--dry-run"]).trim()).unwrap_or_default();
    argv.windows(2).find(|w| w[0] == "--local-dir").map(|w| PathBuf::from(&w[1]))
}

/// A llama.cpp recipe loading `repo`'s `first` file from its `[model] path` `path`.
fn at_path(repo: &str, first: &str, path: &str) -> String {
    llama(repo, &[first], "").replace("[engine]", &format!("path = \"{path}\"\n[engine]"))
}

#[test]
fn a_gguf_in_a_repo_subfolder_downloads_to_its_path_without_nesting() {
    let e = Env::new();
    let repo = "unsloth/Qwen3.5-397B-A17B-GGUF";
    let first = "UD-IQ4_NL/Qwen3.5-397B-A17B-UD-IQ4_NL-00001-of-00005.gguf";
    let path = format!("{repo}/{first}");
    e.recipe("dual", &at_path(repo, first, &path));
    let dir = local_dir(&e, "dual").expect("a [model] path downloads into the models dir");
    // `hf download` keeps the file's place in the repo (UD-IQ4_NL/…) under --local-dir.
    assert_eq!(dir.join(first), e.path("models").join(&path), "--local-dir {}", dir.display());
    // Where the download lands is what the run loads.
    put(&dir.join(first), b"GGUF");
    let plan = e.run(&["use", "dual", "--dry-run"]);
    assert!(plan.contains(&format!("--model {}", e.path("models").join(&path).display())), "{plan}");
}

#[test]
fn a_gguf_path_at_the_repo_root_or_named_apart_from_its_file() {
    let e = Env::new();
    // At the repo's root: the GGUF's own directory, as before.
    e.recipe("root", &at_path("o/R-GGUF", "R-Q4.gguf", "mine/R-GGUF/R-Q4.gguf"));
    assert_eq!(local_dir(&e, "root"), Some(e.path("models/mine/R-GGUF")));
    // A path that doesn't end where `hf download` puts the file would never be found.
    e.recipe("off", &at_path("o/R-GGUF", "UD-Q2/R-00001-of-00002.gguf", "mine/R.gguf"));
    let err = e.fail(&["pull", "off", "--dry-run"]);
    assert!(err.contains("mine/R.gguf") && err.contains("UD-Q2/R-00001-of-00002.gguf"), "{err}");
}

/// Free TCP ports for the nodes, never the same twice in this process.
fn free_port() -> u16 {
    static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(vec![]);
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let p = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        if !used.contains(&p) {
            used.push(p);
            return p;
        }
    }
}

#[test]
fn node_commands_get_model_dir_filled_on_their_machine() {
    let e = Env::new();
    let repo = "unsloth/Qwen3.5-397B-A17B-GGUF";
    let first = "UD-IQ4_NL/Qwen3.5-397B-A17B-UD-IQ4_NL-00001-of-00002.gguf";
    let files = [(first, 10), ("UD-IQ4_NL/Qwen3.5-397B-A17B-UD-IQ4_NL-00002-of-00002.gguf", 10)];
    let snap = cached(&e.path("hub"), repo, "da33c1", &["main"], &files);
    let out = e.path("out");
    fs::create_dir_all(&out).unwrap();
    let node = |name: &str, role: &str, port: u16| {
        format!(
            "[[nodes]]\nname = \"{name}\"\nrole = \"{role}\"\nhost = \"local\"\nport = {port}\nready_delay = 1\n\
             command = [\"sh\", \"-c\", \"echo {{model_dir}} > {}/{name}.txt; trap 'exit 0' TERM; while :; do sleep 0.2; done\"]\n",
            out.display()
        )
    };
    let head = free_port();
    e.recipe(
        "dual",
        &format!(
            "[model]\nrepo = \"{repo}\"\nfiles = [\"{first}\"]\n[engine]\nname = \"custom\"\nport = {head}\n\
             [cluster]\norder = \"workers-first\"\ngate_timeout = 30\n{}{}",
            node("rpc", "worker", free_port()),
            node("head", "head", head)
        ),
    );
    // The controller checks the placeholder and leaves it to each node's machine.
    let dry = e.run(&["use", "dual", "--dry-run"]);
    assert!(dry.contains("echo {model_dir} >"), "{dry}");
    e.run(&["use", "dual", "--no-wait"]);
    let want = format!("{}\n", snap.join("UD-IQ4_NL").display());
    for n in ["rpc", "head"] {
        let f = out.join(format!("{n}.txt"));
        let t0 = Instant::now();
        while fs::read_to_string(&f).unwrap_or_default() != want && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(fs::read_to_string(&f).unwrap_or_default(), want, "{n}");
    }
    e.run(&["stop", "dual"]);
}

#[test]
fn the_index_sizes_a_recipe_by_its_own_gguf() {
    let item = Item {
        id: "o/r-GGUF".into(),
        location: "hf-cache".into(),
        kind: "gguf".into(),
        path: "/hf/models--o--r-GGUF".into(),
        size: 8100,
        revisions: vec![],
        ggufs: vec![("a-Q4.gguf".into(), 3000), ("a-Q5.gguf".into(), 5000)],
    };
    let index = |item: &Item| {
        let inv = Inventory { host: "spark2".into(), items: vec![item.clone()], ..Default::default() };
        let mut idx = Index::default();
        idx.hosts.insert("spark2".into(), HostWeights { online: true, inventory: Some(inv), ..Default::default() });
        idx
    };
    let at = |idx: &Index, r: &Recipe| idx.locate(r).into_iter().map(|l| (l.host, l.size)).collect::<Vec<_>>();
    let q4 = Recipe::parse(&llama("o/r-GGUF", &["a-Q4.gguf"], ""), "q4").unwrap();
    let q8 = Recipe::parse(&llama("o/r-GGUF", &["a-Q8.gguf"], ""), "q8").unwrap();
    assert_eq!(at(&index(&item), &q4), [("spark2".to_string(), 3000)]);
    assert!(at(&index(&item), &q8).is_empty());
    // An older host lists no GGUFs: its repo stands for every quant, as before.
    let old = Item { ggufs: vec![], kind: "hf".into(), ..item };
    assert_eq!(at(&index(&old), &q8), [("spark2".to_string(), 8100)]);
}

#[test]
fn a_directory_path_holds_the_repo_files_for_model_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let models = tmp.path().join("models");
    put(&models.join("mine/Big-GGUF/UD-Q2/Big-00001-of-00002.gguf"), b"x");
    let r = Recipe::parse(
        "[model]\nrepo = \"o/Big-GGUF\"\npath = \"mine/Big-GGUF\"\nfiles = [\"UD-Q2/Big-00001-of-00002.gguf\"]\n\
         [engine]\nname = \"custom\"\ncommand = [\"srv\", \"{model_dir}\"]\n",
        "d",
    )
    .unwrap();
    // The cache isn't consulted for a recipe with a path, even when the repo is there.
    cached(&tmp.path().join("hub"), "o/Big-GGUF", "c1", &["main"], &[("UD-Q2/Big-00001-of-00002.gguf", 1)]);
    assert_eq!(r.model_dir(&models, &tmp.path().join("hub")).unwrap(), models.join("mine/Big-GGUF/UD-Q2"));
    let none = Recipe::parse("[engine]\nname = \"custom\"\ncommand = [\"srv\", \"{model_dir}\"]\n", "n").unwrap();
    let e = format!("{:#}", none.model_dir(&models, tmp.path()).unwrap_err());
    assert!(e.contains("names no weights"), "{e}");
}

proptest! {
    /// With a `[model] path`, a llama.cpp GGUF lands at that path wherever it sits in its repo.
    #[test]
    fn a_gguf_downloads_to_its_path(
        subs in prop::collection::vec("[A-Za-z0-9_]{1,6}", 0..4),
        file in "[A-Za-z][A-Za-z0-9_.]{0,16}",
        root in "[a-z]{1,8}",
    ) {
        let first = subs.iter().map(|s| format!("{s}/")).collect::<String>() + &file + ".gguf";
        let path = format!("/srv/{root}/{first}");
        let r = Recipe::parse(&at_path("o/R-GGUF", &first, &path), "p").unwrap();
        let argv = lmdeck::deck::pull_argv(&lmdeck::config::Config::default(), &r).unwrap();
        let dir = argv.windows(2).find(|w| w[0] == "--local-dir").map(|w| PathBuf::from(&w[1]));
        prop_assert_eq!(dir.map(|d| d.join(&first)), Some(PathBuf::from(&path)));
    }

    /// `{model_dir}` is the directory holding the first file wherever it sits in
    /// the snapshot the revision names, for any engine; llama.cpp loads that file.
    #[test]
    fn model_dir_is_the_first_files_directory(
        org in "[a-z][a-z0-9]{0,8}",
        name in "[A-Za-z][A-Za-z0-9.]{0,12}",
        commit in "[0-9a-f]{6,40}",
        rev in prop::option::of("[a-z][a-z0-9.]{0,7}"),
        subs in prop::collection::vec("[A-Za-z0-9_]{1,6}", 0..3),
        file in "[A-Za-z][A-Za-z0-9_.-]{0,16}",
        custom in any::<bool>(),
    ) {
        let hub = tempfile::tempdir().unwrap();
        let repo = format!("{org}/{name}");
        let first = subs.iter().map(|s| format!("{s}/")).collect::<String>() + &file + ".gguf";
        cached(hub.path(), &repo, &commit, &[rev.as_deref().unwrap_or("main")], &[(&first, 10)]);
        let revision = rev.map(|r| format!("revision = \"{r}\"\n")).unwrap_or_default();
        let engine = if custom { "custom\"\ncommand = [\"srv\"]" } else { "llama.cpp\"\nport = 8109" };
        let text = format!("[model]\nrepo = \"{repo}\"\n{revision}files = [\"{first}\"]\n[engine]\nname = \"{engine}\n");
        let r = Recipe::parse(&text, "p").unwrap();
        let want = repo_dir(hub.path(), &repo).join("snapshots").join(&commit).join(&first);
        let models = Path::new("/nonexistent/lmdeck-test/models");
        prop_assert_eq!(r.model_dir(models, hub.path()).unwrap(), want.parent().unwrap());
        let loads = r.weights_path(models, hub.path());
        prop_assert_eq!(loads, (!custom).then_some(want));
    }

    /// Wherever a llama.cpp recipe's GGUF sits in its snapshot, and whatever else the
    /// repo holds, the recipe is on disk at exactly its own file's size.
    #[test]
    fn a_cached_gguf_is_present_at_its_own_size(
        org in "[a-z][a-z0-9]{0,8}",
        name in "[A-Za-z][A-Za-z0-9.]{0,12}-GGUF",
        commit in "[0-9a-f]{6,40}",
        sub in prop::option::of("[A-Za-z0-9_]{1,8}"),
        file in "[A-Za-z][A-Za-z0-9_.-]{0,16}",
        size in 1usize..5000,
        other in 0usize..5000,
    ) {
        let hub = tempfile::tempdir().unwrap();
        let repo = format!("{org}/{name}");
        let first = match &sub { Some(s) => format!("{s}/{file}.gguf"), None => format!("{file}.gguf") };
        cached(hub.path(), &repo, &commit, &["main"], &[(&first, size), ("other~quant.gguf", other)]);
        let r = Recipe::parse(&llama(&repo, &[&first], ""), "p").unwrap();
        let p = models::presence(&r, Path::new("/nonexistent/lmdeck-test/models"), hub.path());
        prop_assert!(p.local && p.cached, "{p:?}");
        prop_assert_eq!(p.size, size as u64);
    }
}
