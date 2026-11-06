//! The shared service layer behind the TUI and the web dashboard: one
//! snapshot of every recipe (status, weights, fit) plus the actions
//! (start / stop / switch / pull) that both front-ends drive.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::catalog::{Catalog, BUILTIN};
use crate::compat::{self, Check, Level};
use crate::config::Config;
use crate::engine::{self, Inventory, PlanCtx};
use crate::hardware::{which, Hardware};
use crate::models::{self, hf_cache_dir, Presence};
use crate::paths::Paths;
use crate::recipe::{Engine, Recipe};
use crate::runner::{self, Probe, RunRecord, Status};
use crate::sources::{self, Found};
use crate::util::now_secs;

/// The port every recipe would be planned on: an explicit `[engine] port`
/// wins; recipes without one count up from `[serve] port_start` (default
/// 8000) — the deck first, then sources — skipping ports other non-builtin
/// recipes claim (their defaults included) and ports of running servers.
/// Built-ins are samples and neither take counted ports nor reserve theirs.
/// Keyed by `origin/name`.
pub fn planned_ports(cat: &Catalog, paths: &Paths, cfg: &Config) -> BTreeMap<String, u16> {
    let base = cfg.serve.port_start;
    let mut taken: BTreeSet<u16> = BTreeSet::new();
    let mut free: Vec<&Found> = vec![];
    for f in &cat.entries {
        // Built-ins are samples: they never take counted ports, and their own
        // ports don't reserve anything. Multi-node recipes plan one port per
        // node; only their base is reserved.
        // Reason: another host's deck recipe runs there, on its own planning.
        if f.origin == BUILTIN || crate::catalog::is_peer(f) {
            continue;
        }
        if !f.recipe.nodes.is_empty() || f.recipe.engine.port.is_some() {
            taken.insert(f.recipe.port());
        } else {
            free.push(f);
        }
    }
    for rec in RunRecord::all(paths) {
        taken.insert(rec.port);
    }
    // The deck counts first: your recipes get the low ports, then sources,
    // then built-ins.
    free.sort_by(|a, b| {
        (a.origin != crate::catalog::DECK, &a.origin, &a.recipe.name).cmp(&(
            b.origin != crate::catalog::DECK,
            &b.origin,
            &b.recipe.name,
        ))
    });
    let mut out = BTreeMap::new();
    let mut next = base;
    for f in free {
        while taken.contains(&next) {
            next = if next == u16::MAX { base } else { next + 1 };
        }
        taken.insert(next);
        out.insert(reference(f), next);
    }
    out
}

/// The port `r` would be planned on (see [`planned_ports`]): its own entry in
/// the catalog when it has one, else counted from `port_start` on its own.
pub fn planned_for(cat: &Catalog, paths: &Paths, cfg: &Config, r: &Recipe) -> u16 {
    if r.engine.port.is_some() {
        return r.port();
    }
    match cat.entries.iter().find(|f| f.recipe.name == r.name) {
        Some(f) => planned_port(&planned_ports(cat, paths, cfg), f),
        None => {
            let ports = planned_ports(cat, paths, cfg);
            let mut taken: BTreeSet<u16> = ports.values().copied().collect();
            taken.extend(cat.entries.iter().map(|f| f.recipe.port()));
            taken.extend(RunRecord::all(paths).into_iter().map(|x| x.port));
            let mut next = cfg.serve.port_start;
            while taken.contains(&next) {
                next = if next == u16::MAX { 1 } else { next + 1 };
            }
            next
        }
    }
}

/// The planned port of one catalog entry (its engine default when unset).
pub fn planned_port(ports: &BTreeMap<String, u16>, f: &Found) -> u16 {
    ports.get(&reference(f)).copied().unwrap_or_else(|| f.recipe.port())
}

/// Whether a server answering `/v1/models` with `ids` serves this recipe: by
/// its name, its model repo, or the repo's last part, ignoring case (wrapped
/// recipes serve under the upstream model name).
pub fn serves(ids: &[String], r: &Recipe) -> bool {
    let mut want = vec![r.name.to_lowercase()];
    if let Some(repo) = &r.model.repo {
        want.push(repo.to_lowercase());
        want.extend(repo.rsplit('/').next().map(str::to_lowercase));
    }
    ids.iter().any(|id| {
        let id = id.to_lowercase();
        let base = id.rsplit('/').next().unwrap_or(&id).to_string();
        want.iter().any(|w| *w == id || *w == base)
    })
}

/// Whether a live run record for a *different* recipe owns `port` or the same
/// container. Variants of one model (FP8 / NVFP4) share both, but only the one
/// lmdeck started is the server: the other must not adopt it as external.
pub(crate) fn owned_by_other(paths: &Paths, name: &str, port: u16, container: Option<&str>) -> bool {
    RunRecord::all(paths)
        .into_iter()
        .filter(|rec| rec.name != name)
        .any(|rec| (rec.port == port || container.is_some_and(|c| rec.container.as_deref() == Some(c))) && rec.alive())
}

/// A recipe's status on this machine, adopting servers lmdeck didn't start:
/// the recipe's container running, or its port answering with its model,
/// make it [`Status::External`]. With `docker` `None` only the port is asked
/// (the serve sampler's periodic scan, which shouldn't spawn docker per recipe).
pub fn local_status(
    paths: &Paths,
    docker: Option<&str>,
    r: &Recipe,
    port: u16,
    cache: &mut HashMap<u16, Probe>,
) -> Status {
    let s = runner::status_cached(paths, &r.name, port, cache);
    if s.is_up() || !r.nodes.is_empty() {
        return s;
    }
    let container = r.engine.container.as_deref().filter(|c| !c.is_empty());
    let owned_elsewhere = || owned_by_other(paths, &r.name, port, container);
    match cache.get(&port) {
        Some(Probe::Up(ids)) if serves(ids, r) => {
            // Reason: a variant that shares the port (and model id) answers too;
            // its port is taken by the recipe lmdeck actually started.
            return if owned_elsewhere() { Status::PortBusy } else { Status::External };
        }
        // Reason: the port answers for another model, so a container of this
        // name belongs to a recipe sharing it (a variant), not this one.
        Some(Probe::Up(ids)) if !ids.is_empty() => return s,
        _ => {}
    }
    // Reason: a recipe spanning several hosts runs a container of the same name on
    // every worker too; only its head, the one answering on the port, serves it.
    let spans_hosts = r.hardware.hosts.unwrap_or(1) > 1;
    if let (Some(docker), None, false) = (docker, std::env::var_os("LMDECK_NO_PROBE"), spans_hosts) {
        if let Some(c) = container {
            if runner::container_running(docker, c) {
                return if owned_elsewhere() { s } else { Status::External };
            }
        }
    }
    s
}

/// The other host running `name` as the sampler last saw it, with its state there.
pub fn remote_status(live: &crate::hosts::LiveFile, name: &str) -> Option<(String, Status)> {
    let (host, _) = live
        .hosts
        .iter()
        .filter(|(h, _)| h.as_str() != crate::hosts::LOCAL)
        .find(|(_, (online, names))| *online && names.iter().any(|n| n == name))?;
    let state = live.states.get(host).and_then(|m| m.get(name)).map(String::as_str);
    let status = match state {
        Some("loading") => Status::Loading,
        Some("external") => Status::External,
        _ => Status::Loaded,
    };
    Some((host.clone(), status))
}

/// A recipe's status as this machine sees it: here (adopting what lmdeck
/// didn't start), else on the other host the sampler last saw it running on.
pub fn seen_status(
    paths: &Paths,
    docker: &str,
    r: &Recipe,
    port: u16,
    cache: &mut HashMap<u16, Probe>,
    live: Option<&crate::hosts::LiveFile>,
) -> (Status, Option<String>) {
    let s = local_status(paths, Some(docker), r, port, cache);
    if s.is_up() || !r.nodes.is_empty() {
        return (s, None);
    }
    match live.and_then(|l| remote_status(l, &r.name)) {
        Some((h, rs)) => (rs, Some(h)),
        None => (s, None),
    }
}

/// The endpoint of a server on `host` (a hosts.toml name; `None` for here).
pub fn endpoint_on(paths: &Paths, host: Option<&str>, port: u16) -> String {
    let url = host.and_then(|h| crate::hosts::HostsFile::load(paths).ok()?.get(h).map(|e| e.url.clone()));
    match url {
        Some(u) => format!("http://{}:{port}/v1", url_host(&u)),
        None => format!("http://localhost:{port}/v1"),
    }
}

/// The host part of a serve URL (`http://10.0.0.2:7979` → `10.0.0.2`).
pub fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split('/').next().unwrap_or(rest);
    if let Some(v6) = rest.strip_prefix('[') {
        return v6.split(']').next().unwrap_or(v6);
    }
    rest.rsplit_once(':').map(|(h, _)| h).unwrap_or(rest)
}

/// Why a start sent to `host` isn't running there: the last lines of its log
/// on that host, asked at most every 30 s per recipe.
fn remote_failure(paths: &Paths, host: &str, name: &str) -> String {
    static SEEN: Mutex<BTreeMap<String, (Instant, String)>> = Mutex::new(BTreeMap::new());
    let key = format!("{host}/{name}");
    if let Some((at, msg)) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if at.elapsed() < Duration::from_secs(30) {
            return msg.clone();
        }
    }
    let tail = crate::remote::Client::named(paths, host)
        .and_then(|c| c.logs(name, 40))
        .map(|lines| failure_lines(&lines))
        .unwrap_or_default();
    let msg = if tail.is_empty() {
        format!("started on {host}, but it isn't running there now; see `lmdeck logs {name}`")
    } else {
        format!("started on {host}, but it stopped: {tail}  (more: `lmdeck logs {name}`)")
    };
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now(), msg.clone()));
    msg
}

/// The lines of a log tail that say why a server stopped: the last error-ish
/// lines, else the last line.
pub fn failure_lines(lines: &[String]) -> String {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    let bad = |l: &str| {
        let l = l.to_lowercase();
        ["error", "fatal", "failed", "traceback", "exception", "killed", "no such", "not found", "denied", "exit"]
            .iter()
            .any(|w| l.contains(w))
    };
    let errs: Vec<&str> = lines.iter().copied().filter(|l| bad(l)).collect();
    let pick: Vec<&str> = if errs.is_empty() {
        lines.iter().rev().take(1).copied().collect()
    } else {
        errs.into_iter().rev().take(2).rev().collect()
    };
    let s = pick.join(" | ");
    if s.chars().count() > 300 {
        s.chars().take(300).collect::<String>() + "…"
    } else {
        s
    }
}

/// The recipes `list` numbers: the deck and the other hosts' decks, or the
/// built-ins while those are empty.
pub fn listed(cat: &Catalog) -> Vec<&Found> {
    let deck: Vec<&Found> = cat.deck().chain(cat.peers()).collect();
    if deck.is_empty() {
        cat.entries.iter().filter(|f| f.origin == BUILTIN).collect()
    } else {
        deck
    }
}

/// Resolves a name, `origin/name`, or a 1-based `#` from [`listed`].
pub fn resolve<'a>(cat: &'a Catalog, target: &str) -> Result<&'a Found> {
    if let Ok(n) = target.parse::<usize>() {
        let l = listed(cat);
        return l.get(n.wrapping_sub(1)).copied().ok_or_else(|| anyhow!("no recipe #{n} (the list has {})", l.len()));
    }
    cat.resolve(target)
}

pub fn status_word(s: &Status) -> &'static str {
    match s {
        Status::Loaded => "loaded",
        Status::Loading => "loading",
        Status::External => "external",
        Status::PortBusy => "port-busy",
        Status::Stopped => "stopped",
    }
}

/// Memory a launch will take, in GB (engines that pre-reserve a fraction of
/// unified memory count that fraction).
pub fn memory_need_gb(r: &Recipe, hw: &Hardware, p: &Presence) -> Option<f64> {
    if let Some(frac) = compat::reserved_fraction(r) {
        if hw.unified_memory || hw.gpus.is_empty() {
            return Some(frac * hw.total_memory_gb);
        }
    }
    compat::required_memory_gb(r, p.present().then_some(p.size))
}

/// The command that downloads a recipe's weights.
pub fn pull_argv(cfg: &Config, r: &Recipe) -> Result<Vec<String>> {
    if r.model.fetched_by_scripts() {
        bail!("{}'s own scripts fetch its weights ([model] pull = false); lmdeck doesn't download them", r.name);
    }
    let repo = r.model.repo.clone().ok_or_else(|| anyhow!("{} has no Hugging Face repo to pull", r.name))?;
    if r.engine.name == Engine::LmStudio {
        // LM Studio keeps its own model library; let it download.
        let lms = PlanCtx::new(cfg, false).bins.get("lmstudio").cloned().unwrap_or_else(|| "lms".into());
        return Ok(vec![lms, "get".into(), repo]);
    }
    if r.engine.name == Engine::TensorFold {
        // `tensorfold pull` fills the HF cache, checks every file against the Hub and
        // refuses checkpoints whose model_type has no native family.
        let tf = r
            .engine
            .bin
            .clone()
            .or_else(|| cfg.engines.tensorfold.clone())
            .or_else(|| PlanCtx::new(cfg, false).bins.get("tensorfold").cloned())
            .unwrap_or_else(|| "tensorfold".into());
        let target = match &r.model.revision {
            Some(rev) => format!("{repo}@{rev}"),
            None => repo,
        };
        return Ok(vec![tf, "pull".into(), target]);
    }
    let bin = if which("hf").is_some() {
        "hf"
    } else if which("huggingface-cli").is_some() {
        "huggingface-cli"
    } else {
        "hf"
    };
    let mut argv = vec![bin.to_string(), "download".into(), repo.clone()];
    if let Some(rev) = &r.model.revision {
        argv.extend(["--revision".into(), rev.clone()]);
    }
    for f in &r.model.files {
        argv.extend(["--include".into(), f.clone()]);
        if let Some((_, n)) = models::gguf_shard(f) {
            // Split GGUF: fetch every shard.
            if let Some((head, _)) = f.rsplit_once("-00001-of-") {
                argv.extend(["--include".into(), format!("{head}-*-of-{n:05}.gguf")]);
            }
        }
    }
    // Hugging Face downloads fill its cache; a `[model] path` gets them instead, since
    // that is where every engine then loads them from.
    let local = r.model.path.as_ref().and_then(|_| r.local_path(&cfg.models_dir()));
    let dir = match (r.engine.name, local) {
        (Engine::LlamaCpp, Some(p)) => Some(r.gguf_download_dir(&p)?),
        (_, p) => p,
    };
    if let Some(d) = dir {
        argv.extend(["--local-dir".into(), d.display().to_string()]);
    }
    Ok(argv)
}

/// Hint shown when the download tool for `r` is not installed.
pub fn pull_tool_hint(r: &Recipe) -> &'static str {
    match r.engine.name {
        Engine::LmStudio => {
            "LM Studio's `lms` CLI was not found (open LM Studio once, or run ~/.lmstudio/bin/lms bootstrap)"
        }
        Engine::TensorFold => {
            "TensorFold was not found (curl -fsSL https://tensorfold.dev/install.sh | sh, or on a Mac \
             brew install ashhart/tensorfold/tensorfold)"
        }
        _ => "the Hugging Face CLI is not installed (`pip install -U huggingface_hub`)",
    }
}

/// One recipe as the dashboards show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecipeView {
    /// 1-based position in `lmdeck list` (0 when not listed).
    pub index: usize,
    pub name: String,
    /// `origin/name`, unambiguous across origins.
    pub reference: String,
    pub origin: String,
    pub display_name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub engine: String,
    /// Model family for the grouped views (`Qwen`, `GLM`, …; see
    /// [`crate::recipe::FAMILY_RULES`]).
    pub family: &'static str,
    pub quant: String,
    pub model: Option<String>,
    pub revision: Option<String>,
    pub port: u16,
    pub ctx: Option<u64>,
    pub endpoint: String,
    /// In the deck (or a built-in shown while the deck is empty).
    pub listed: bool,
    pub status: Status,
    pub pid: Option<u32>,
    pub started_at: Option<u64>,
    pub downloaded: bool,
    pub cached: bool,
    pub size_bytes: u64,
    /// Advertised size when the weights are not on disk.
    pub size_gb: Option<f64>,
    pub need_gb: Option<f64>,
    pub fit: Level,
    pub checks: Vec<Check>,
    pub command: Option<String>,
    pub plan_error: Option<String>,
    /// A pull for this recipe is running.
    pub pulling: bool,
    /// Wrapped recipes: whether the `[source]` repo has newer commits
    /// (from the update-check cache; `None` for native recipes).
    pub update: Option<crate::updates::UpdateView>,
    /// Hosts whose disk holds the weights (`[node] name` for this machine),
    /// from the controller's weights index.
    pub weights_on: Vec<String>,
    /// Wrapped recipes: the `[source]` repo and ref (`lmdeck browse` matches index entries on them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// The one host the weights are served from (`[model] home`, else the
    /// first holder, this machine first; a placement's home when the index
    /// has no answer).
    pub weights_home: Option<String>,
    /// `[model] pull = false`: the recipe's own scripts fetch its weights,
    /// so `lmdeck pull` does not download them.
    pub fetched_by_scripts: bool,
    /// Multi-node recipes: one entry per `[[nodes]]`, in start order.
    pub nodes: Vec<crate::cluster::NodeView>,
    /// Started on another host by `--on` (placement record).
    pub placed_on: Option<String>,
    /// Multi-node: `starting`, `up` or `failed` (+ error), from the run record.
    pub cluster_phase: Option<String>,
    pub cluster_error: Option<String>,
    /// A chat model harness sync offers (`[harness] chat`, default true).
    pub chat: bool,
}

impl RecipeView {
    pub fn is_up(&self) -> bool {
        self.status.is_up()
    }

    pub fn fits(&self) -> bool {
        self.fit != Level::Fail
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceView {
    pub name: String,
    pub spec: String,
    pub recipes: Option<usize>,
    pub fetched_at: Option<u64>,
}

/// Everything a dashboard draws, taken in one go.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub version: String,
    pub generated_at: u64,
    pub hardware: Hardware,
    pub inventory: Inventory,
    pub recipes: Vec<RecipeView>,
    pub sources: Vec<SourceView>,
    pub recipe_errors: Vec<(String, String)>,
}

impl Snapshot {
    /// Recipes with a server answering, one per name: a built-in copy that
    /// shadows a deck recipe is the same recipe, not a second server.
    pub fn running(&self) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        self.recipes.iter().filter(|r| r.is_up() && seen.insert(r.name.as_str())).count()
    }

    pub fn find(&self, reference: &str) -> Option<&RecipeView> {
        self.recipes.iter().find(|r| r.reference == reference || r.name == reference)
    }
}

/// Shared state for long-lived front-ends (TUI, web server).
pub struct Deck {
    pub paths: Paths,
    pub cfg: Config,
    inventory: Mutex<Option<(Inventory, Instant)>>,
    pulls: Mutex<HashMap<String, Child>>,
}

const INVENTORY_TTL: Duration = Duration::from_secs(60);

impl Deck {
    pub fn new(paths: Paths, cfg: Config) -> Deck {
        Deck { paths, cfg, inventory: Mutex::new(None), pulls: Mutex::new(HashMap::new()) }
    }

    pub fn load() -> Result<Deck> {
        let paths = Paths::from_env();
        let cfg = Config::load(&paths)?;
        Ok(Deck::new(paths, cfg))
    }

    pub fn catalog(&self) -> Catalog {
        Catalog::load(&self.paths, &self.cfg)
    }

    pub fn docker_bin(&self) -> String {
        self.docker()
    }

    fn docker(&self) -> String {
        self.cfg.engines.docker.clone().unwrap_or_else(|| "docker".into())
    }

    fn presence(&self, r: &Recipe) -> Presence {
        models::presence(r, &self.cfg.models_dir(), &hf_cache_dir())
    }

    /// Engine inventory, re-probed at most once a minute unless `refresh`.
    pub fn inventory(&self, refresh: bool) -> Inventory {
        let mut g = self.inventory.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((inv, at)) = g.as_ref() {
            if !refresh && at.elapsed() < INVENTORY_TTL {
                return inv.clone();
            }
        }
        let inv = Inventory::detect(&self.cfg);
        *g = Some((inv.clone(), Instant::now()));
        inv
    }

    /// Whether a pull for `name` is still running (reaping finished ones).
    pub fn pulling(&self, name: &str) -> bool {
        let mut g = self.pulls.lock().unwrap_or_else(|e| e.into_inner());
        match g.get_mut(name).map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                g.remove(name);
                false
            }
            None => false,
        }
    }

    /// One consistent picture of the machine and every recipe.
    /// `refresh_inventory` re-probes engines (slow-ish: spawns python).
    pub fn snapshot(&self, refresh_inventory: bool) -> Snapshot {
        let cat = self.catalog();
        let hw = Hardware::detect();
        let inv = self.inventory(refresh_inventory);
        let listed_refs: Vec<String> = listed(&cat).iter().map(|f| reference(f)).collect();
        let mut cache: HashMap<u16, Probe> = HashMap::new();
        let ports = planned_ports(&cat, &self.paths, &self.cfg);
        let mut updates = crate::updates::views(
            &self.paths,
            &crate::updates::Settings::from(&self.cfg),
            cat.entries.iter().map(|f| &f.recipe),
        );
        let mut recipes = vec![];
        let node = self.cfg.node_name();
        let mut widx = crate::weights::Index::load(&self.paths);
        if !widx.hosts.contains_key(crate::hosts::LOCAL) && !widx.demo {
            let inv = crate::weights::local(&self.cfg);
            widx.hosts.insert(
                crate::hosts::LOCAL.into(),
                crate::weights::HostWeights { online: true, error: None, at: now_secs(), inventory: Some(inv) },
            );
        }
        let show = |k: &str| if k == crate::hosts::LOCAL { node.clone() } else { k.to_string() };
        let live = crate::hosts::read_live(&self.paths, Duration::from_secs(15));
        let host_urls: BTreeMap<String, String> = crate::hosts::HostsFile::load(&self.paths)
            .map(|f| f.hosts.into_iter().map(|h| (h.name, h.url)).collect())
            .unwrap_or_default();
        let docker = self.docker();
        let fleet = std::cell::OnceCell::new();
        for f in &cat.entries {
            let r = &f.recipe;
            let reference = reference(f);
            let index = listed_refs.iter().position(|x| *x == reference).map(|i| i + 1).unwrap_or(0);
            let p = self.presence(r);
            let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size))
                .with_fleet(r, || *fleet.get_or_init(|| crate::hosts::fleet(&self.paths, &hw.platform)));
            // A multi-node recipe has one launch command per node, shown with its nodes.
            let planned = planned_port(&ports, f);
            let (command, plan_error) =
                match engine::plan(r, &PlanCtx::new(&self.cfg, p.local).with_port(ports.get(&reference).copied())) {
                    _ if !r.nodes.is_empty() => (None, None),
                    Ok(plan) => (Some(plan.command_line()), None),
                    Err(e) => (None, Some(format!("{e:#}"))),
                };
            let mut status = local_status(&self.paths, Some(&docker), r, planned, &mut cache);
            let nodes = if r.nodes.is_empty() { vec![] } else { crate::cluster::views(&self.paths, r, &node) };
            let crec = (!r.nodes.is_empty()).then(|| crate::cluster::Record::load(&self.paths, &r.name)).flatten();
            if !nodes.is_empty() {
                let up = |s: &str| matches!(s, "loaded" | "running" | "loading");
                let head_ok =
                    nodes.iter().any(|n| n.role == "head" && matches!(n.status.as_str(), "loaded" | "running"));
                status = if nodes.iter().all(|n| up(&n.status)) && head_ok {
                    Status::Loaded
                } else if nodes.iter().any(|n| up(&n.status)) || crec.as_ref().is_some_and(|c| c.phase == "starting") {
                    Status::Loading
                } else {
                    Status::Stopped
                };
            }
            let placed =
                crate::placement::Record::load(&self.paths, &r.name).filter(|p| p.run_host != crate::hosts::LOCAL);
            let placed_on = placed.as_ref().and_then(|p| {
                let running =
                    live.as_ref().and_then(|l| l.hosts.get(&p.run_host)).is_some_and(|(_, v)| v.contains(&p.served_as));
                running.then(|| p.run_host.clone())
            });
            // A run on another host (placed there, or started there) shows its state there.
            let mut remote_on = None;
            if !status.is_up() && r.nodes.is_empty() {
                let names = [Some(r.name.as_str()), placed.as_ref().map(|p| p.served_as.as_str())];
                if let Some((h, s)) =
                    live.as_ref().and_then(|l| names.iter().flatten().find_map(|n| remote_status(l, n)))
                {
                    status = s;
                    remote_on = Some(h);
                }
            }
            // Reason: a start the TUI or dashboard sent to another host returns once it is
            // spawned there; when it dies while loading, say so instead of a silent "stopped".
            let placement_error = placed.as_ref().filter(|_| !status.is_up()).and_then(|p| {
                let host_up = live.as_ref().and_then(|l| l.hosts.get(&p.run_host)).is_some_and(|(on, _)| *on);
                let settled =
                    crate::placement::Record::age(&self.paths, &r.name).is_some_and(|a| a > Duration::from_secs(20));
                (host_up && settled).then(|| remote_failure(&self.paths, &p.run_host, &p.served_as))
            });
            let placed_on = placed_on.or(remote_on.clone());
            let rec = if status.is_up() { RunRecord::load(&self.paths, &r.name) } else { None };
            let port = rec.as_ref().map(|x| x.port).unwrap_or(planned);
            let endpoint = match remote_on.as_ref().and_then(|h| host_urls.get(h)) {
                Some(url) => format!("http://{}:{port}/v1", url_host(url)),
                None => format!("http://localhost:{port}/v1"),
            };
            let locs = widx.locate(r);
            // Reason: the index drops a host that is offline, but a placement made for
            // this run recorded where the weights were served from; keep saying so
            // instead of "not downloaded".
            let weights_home = widx
                .home(r, &node)
                .map(|l| show(&l.host))
                .or_else(|| placed.as_ref().map(|p| show(&p.home)).filter(|h| !h.is_empty()));
            recipes.push(RecipeView {
                index,
                name: r.name.clone(),
                reference,
                origin: f.origin.clone(),
                display_name: r.display().to_string(),
                family: r.family(),
                description: r.description.clone(),
                tags: r.tags.clone(),
                engine: r.engine.name.as_str().into(),
                quant: r.quant_short(),
                model: r.model.repo.clone().or(r.model.path.clone()),
                revision: r.model.revision.clone(),
                port,
                ctx: r.engine.ctx,
                endpoint,
                listed: index > 0,
                status,
                pid: rec.as_ref().and_then(|r| r.pid),
                started_at: rec.as_ref().map(|r| r.started_at),
                downloaded: p.present(),
                cached: p.cached,
                size_bytes: p.size,
                size_gb: r.model.size_gb,
                need_gb: memory_need_gb(r, &hw, &p),
                fit: rep.level(),
                checks: rep.checks,
                command,
                plan_error,
                pulling: self.pulling(&r.name),
                update: updates.remove(&r.name),
                weights_on: locs.iter().map(|l| show(&l.host)).collect(),
                source_repo: r.source.as_ref().map(|s| s.repo.clone()),
                source_ref: r.source.as_ref().and_then(|s| s.git_ref.clone()),
                weights_home,
                fetched_by_scripts: r.model.fetched_by_scripts(),
                nodes,
                placed_on,
                cluster_phase: crec.as_ref().map(|c| c.phase.clone()),
                cluster_error: crec.and_then(|c| c.error).or(placement_error),
                chat: r.harness.chat,
            });
        }
        // Listed recipes first, in list order; then the rest by origin.
        recipes.sort_by(|a, b| match (a.index, b.index) {
            (0, 0) => (a.origin != BUILTIN, &a.origin, &a.name).cmp(&(b.origin != BUILTIN, &b.origin, &b.name)),
            (0, _) => std::cmp::Ordering::Greater,
            (_, 0) => std::cmp::Ordering::Less,
            (x, y) => x.cmp(&y),
        });
        let sources = self
            .cfg
            .sources
            .iter()
            .map(|s| {
                let dir = self.paths.sources_cache_dir().join(&s.name);
                let idx = sources::read_index(&dir);
                SourceView {
                    name: s.name.clone(),
                    spec: s.spec.clone(),
                    recipes: idx.as_ref().map(|_| sources::load_dir(&s.name, &dir).0.len()),
                    fetched_at: idx.map(|i| i.fetched_at),
                }
            })
            .collect();
        Snapshot {
            version: env!("CARGO_PKG_VERSION").into(),
            generated_at: now_secs(),
            hardware: hw,
            inventory: inv,
            recipes,
            sources,
            recipe_errors: cat.errors.clone(),
        }
    }

    /// Starts a recipe in the background (does not wait for it to load).
    /// `only` stops every other lmdeck server first (a switch).
    pub fn start(&self, target: &str, only: bool, force: bool) -> Result<String> {
        let cat = self.catalog();
        let found = resolve(&cat, target)?;
        let r = &found.recipe;
        if !r.nodes.is_empty() {
            return crate::cluster::start_background(
                self,
                r,
                &crate::cluster::Opts { only, force, ..Default::default() },
            );
        }
        self.start_recipe(r, None, only, force)
    }

    /// Starts a recipe that need not be in the catalog (an uploaded or derived
    /// one). `picked` names the peers a controller picked around this machine
    /// and filled in: with them, this machine needs no hosts.toml of its own.
    pub fn start_recipe(&self, r: &Recipe, picked: Option<&[String]>, only: bool, force: bool) -> Result<String> {
        if r.nodes() > 1 {
            bail!("{} spans {} nodes; give it [[nodes]] to launch it across hosts", r.name, r.nodes());
        }
        let hw = Hardware::detect();
        let inv = self.inventory(false);
        let p = self.presence(r);
        let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size)).with_fleet(r, || match picked {
            // Reason: the controller found them answering on the CX7 fabric around this
            // machine; this machine and they are the Sparks the recipe's scripts drive.
            Some(names) => compat::Fleet { online: (names.len() as u32).saturating_add(1) },
            None => crate::hosts::fleet(&self.paths, &hw.platform),
        });
        let cat = self.catalog();

        let port = planned_for(&cat, &self.paths, &self.cfg, r);
        let fabric = crate::topology::fabric_for(&self.paths, &self.cfg, r).map(|f| f.kind.as_str().to_string());
        let peers = crate::pick::peers(&self.paths, &self.cfg, r, crate::hosts::LOCAL)?;
        let ctx = PlanCtx::new(&self.cfg, p.local).with_port(Some(port)).with_fabric(fabric);
        let plan = engine::plan(r, &ctx.with_vars(crate::vars::peer_vars(&peers.hosts)))?;
        match local_status(&self.paths, Some(&self.docker()), r, plan.port, &mut HashMap::new()) {
            Status::Loaded | Status::External => return Ok(format!("{} is already up on :{}", r.name, plan.port)),
            Status::Loading => return Ok(format!("{} is still loading", r.name)),
            Status::PortBusy if !only => {
                bail!("port {} is in use by another server; stop it or switch instead", plan.port)
            }
            _ => {}
        }
        if !rep.compatible() && !force {
            bail!("{} doesn't fit this machine ({})", r.name, rep.reasons(false).join("; "));
        }
        if let Err(why) = inv.can_launch(r) {
            if !force {
                bail!("{why}");
            }
        }
        let missing = if r.engine.name.needs_local_weights() { !p.local } else { !p.present() };
        if r.engine.name.needs_downloaded_weights() && missing && !force {
            bail!("{}'s weights are not on disk yet; pull first", r.name);
        }
        let mut stopped = vec![];
        if only {
            for rec in RunRecord::all(&self.paths) {
                if rec.name != r.name {
                    runner::stop(&self.paths, &rec.name, &self.docker())?;
                    stopped.push(rec.name);
                }
            }
            if let Status::PortBusy = runner::status(&self.paths, &r.name, plan.port) {
                bail!("port {} is still in use by a server lmdeck didn't start", plan.port);
            }
        }
        if let (Some(need), Some(avail)) = (memory_need_gb(r, &hw, &p), hw.available_memory_gb) {
            if need > avail && !force {
                bail!("{} needs ~{need:.1} GB but only {avail:.1} GB is free; stop something first", r.name);
            }
        }
        crate::checkout::ensure(&self.paths, r)?;
        runner::start(&self.paths, &r.name, r.engine.name.as_str(), &plan)?;
        let mut msg =
            format!("starting {} on :{}{}", r.name, plan.port, crate::harness::sync_note(&self.paths, &self.cfg));
        if !stopped.is_empty() {
            msg.push_str(&format!(" (stopped {})", stopped.join(", ")));
        }
        Ok(msg)
    }

    pub fn stop(&self, target: &str) -> Result<String> {
        let cat = self.catalog();
        let name = match resolve(&cat, target) {
            Ok(f) if !f.recipe.nodes.is_empty() => return crate::cluster::stop(self, &f.recipe),
            Ok(f) => f.recipe.name.clone(),
            Err(_) if self.is_run_name(&cat, target) => target.to_string(),
            Err(e) => return Err(e),
        };
        // Reason: the TUI's and the dashboard's starts go to the host holding the weights
        // now, so their stop goes there too, as the CLI's `stop` does for any placed run.
        if let Some(p) = crate::placement::Record::load(&self.paths, &name) {
            if p.run_host != crate::hosts::LOCAL && RunRecord::load(&self.paths, &name).is_none() {
                let m = crate::remote::Client::named(&self.paths, &p.run_host)?.action("stop", &p.served_as, false)?;
                crate::placement::Record::remove(&self.paths, &name);
                return Ok(format!("{}: {m}{}", p.run_host, crate::harness::sync_note(&self.paths, &self.cfg)));
            }
        }
        if runner::stop(&self.paths, &name, &self.docker())? {
            return Ok(format!("stopped {name}{}", crate::harness::sync_note(&self.paths, &self.cfg)));
        }
        if let Ok(f) = resolve(&cat, &name) {
            if let Some(m) = self.stop_adopted(&cat, &f.recipe)? {
                return Ok(format!("{m}{}", crate::harness::sync_note(&self.paths, &self.cfg)));
            }
        }
        // Started on another host (by hand, or by another controller): stop it there.
        if let Some((host, _)) =
            crate::hosts::read_live(&self.paths, Duration::from_secs(60)).and_then(|l| remote_status(&l, &name))
        {
            let m = crate::remote::Client::named(&self.paths, &host)?.action("stop", &name, false)?;
            return Ok(format!("{host}: {m}{}", crate::harness::sync_note(&self.paths, &self.cfg)));
        }
        bail!("{name} was not started by lmdeck, and isn't running here or on any host lmdeck knows")
    }

    /// Stops a server lmdeck didn't start but recognises as `r` (see
    /// [`local_status`]): with the recipe's own stop command when it has one,
    /// else `docker stop` on its container. `None` when it isn't running here.
    fn stop_adopted(&self, cat: &Catalog, r: &Recipe) -> Result<Option<String>> {
        let docker = self.docker();
        let port = planned_for(cat, &self.paths, &self.cfg, r);
        if local_status(&self.paths, Some(&docker), r, port, &mut HashMap::new()) != Status::External {
            return Ok(None);
        }
        let p = self.presence(r);
        let fabric = crate::topology::fabric_for(&self.paths, &self.cfg, r).map(|f| f.kind.as_str().to_string());
        let plan = crate::pick::peers(&self.paths, &self.cfg, r, crate::hosts::LOCAL).and_then(|peers| {
            let ctx = PlanCtx::new(&self.cfg, p.local).with_port(Some(port)).with_fabric(fabric);
            engine::plan(r, &ctx.with_vars(crate::vars::peer_vars(&peers.hosts)))
        });
        let (stop, cwd, env, planned) = match plan {
            Ok(plan) => (plan.stop, plan.cwd, plan.env, plan.container),
            Err(_) => (None, None, BTreeMap::new(), None),
        };
        let container = planned.or(r.engine.container.clone()).filter(|c| !c.is_empty());
        if stop.as_ref().is_none_or(|s| s.is_empty()) && container.is_none() {
            bail!(
                "{} is served on :{port} by a process lmdeck didn't start and can't stop; stop it where it runs",
                r.name
            );
        }
        // A record for what is running, so `runner::stop` stops it the usual way.
        let rec = RunRecord {
            name: r.name.clone(),
            engine: r.engine.name.as_str().into(),
            pid: None,
            container,
            port,
            started_at: now_secs(),
            log: self.paths.log_dir().join(format!("{}.log", r.name)),
            argv: vec![],
            stop,
            oneshot: false,
            cwd,
            env,
            detached: true,
            exit_file: None,
        };
        rec.save(&self.paths)?;
        runner::stop(&self.paths, &r.name, &docker)?;
        Ok(Some(format!("stopped {} (it was running outside lmdeck)", r.name)))
    }

    /// Downloads a recipe's weights in the background, logging to [`Deck::pull_log`].
    /// A wrapped recipe's checkout is updated first; `latest` also moves a
    /// pinned `ref` to the upstream head (see [`update_source`]).
    pub fn pull(&self, target: &str, latest: bool) -> Result<String> {
        let cat = self.catalog();
        let found = resolve(&cat, target)?;
        let r = &found.recipe;
        if self.pulling(&r.name) {
            return Ok(format!("{} is already downloading", r.name));
        }
        // A wrapped recipe's `[source]` is updated in place; its scripts fetch their own weights.
        if let Some(msg) = update_source(&self.paths, &self.cfg, found, latest)? {
            if r.model.repo.is_none() || r.model.fetched_by_scripts() {
                return Ok(format!("{}: {msg}", r.name));
            }
        }
        let argv = pull_argv(&self.cfg, r)?;
        if which(&argv[0]).is_none() {
            bail!("{}", pull_tool_hint(r));
        }
        let log = self.pull_log(&r.name);
        if let Some(d) = log.parent() {
            std::fs::create_dir_all(d)?;
        }
        let out = File::create(&log)?;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn().map_err(|e| anyhow!("could not run {}: {e}", argv[0]))?;
        self.pulls.lock().unwrap_or_else(|e| e.into_inner()).insert(r.name.clone(), child);
        Ok(format!("pulling {}", r.name))
    }

    pub fn server_log(&self, name: &str) -> PathBuf {
        RunRecord::load(&self.paths, name)
            .map(|r| r.log)
            .unwrap_or_else(|| self.paths.log_dir().join(format!("{name}.log")))
    }

    pub fn pull_log(&self, name: &str) -> PathBuf {
        self.paths.log_dir().join(format!("{name}.pull.log"))
    }

    /// The log worth showing for `name`: the pull log while a download runs
    /// (or when it is the newer file), else the server log.
    pub fn log_for(&self, name: &str) -> PathBuf {
        let server = self.server_log(name);
        let pull = self.pull_log(name);
        if self.pulling(name) {
            return pull;
        }
        let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match (mtime(&server), mtime(&pull)) {
            (Some(s), Some(p)) if p > s => pull,
            (None, Some(_)) => pull,
            _ => server,
        }
    }

    /// Where `name`'s log is when its run lives on another host: that host and
    /// the name the run goes by there (a start placed on the weights home).
    /// `None` when this machine has the log worth showing: a run or a pull
    /// here, or a log written here after the placement.
    pub fn remote_log(&self, name: &str) -> Option<(String, String)> {
        let p = crate::placement::Record::load(&self.paths, name).filter(|p| p.run_host != crate::hosts::LOCAL)?;
        if RunRecord::load(&self.paths, name).is_some() || self.pulling(name) {
            return None;
        }
        let placed = crate::placement::Record::age(&self.paths, name)?;
        let local =
            std::fs::metadata(self.log_for(name)).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if local.is_some_and(|age| age < placed) {
            return None;
        }
        Some((p.run_host, p.served_as))
    }

    /// Resolves `target` to a recipe name (also accepts names of running
    /// servers, and a name several recipes share).
    pub fn name_of(&self, target: &str) -> Result<String> {
        let cat = self.catalog();
        match resolve(&cat, target) {
            Ok(f) => Ok(f.recipe.name.clone()),
            Err(_) if self.is_run_name(&cat, target) => Ok(target.to_string()),
            Err(e) => Err(e),
        }
    }

    /// Whether `target` is the name a run goes by here although it names no
    /// one recipe: a run record has it, or several origins share it.
    fn is_run_name(&self, cat: &Catalog, target: &str) -> bool {
        // Reason: a controller's upload of a built-in (its peers filled in, or its weights
        // over NFS) sits beside the built-in. Starting it by that bare name stays ambiguous,
        // but its log and run record go by the name alone, and must outlive the run.
        RunRecord::load(&self.paths, target).is_some() || cat.entries.iter().any(|f| f.recipe.name == target)
    }
}

impl Drop for Deck {
    fn drop(&mut self) {
        // Downloads outlive the dashboard on purpose; just don't leave zombies.
        if let Ok(mut g) = self.pulls.lock() {
            for c in g.values_mut() {
                let _ = c.try_wait();
            }
        }
    }
}

/// `pull` for a wrapped recipe: fetch and check out its `[source]` again.
/// With `latest`, a recipe pinned to a commit or tag first has its `ref`
/// rewritten to the tracked branch's head (deck recipes only). Refreshes the
/// update-check cache afterwards. `None` for recipes without a `[source]`.
pub fn update_source(paths: &Paths, cfg: &Config, found: &Found, latest: bool) -> Result<Option<String>> {
    let r = &found.recipe;
    if r.source.is_none() {
        return Ok(None);
    }
    let mut bumped = None;
    let mut recipe = r.clone();
    if latest {
        if found.origin != crate::catalog::DECK {
            bail!(
                "{} comes from {}; copy it into the deck first (lmdeck recipe add {}) so its ref can be moved",
                r.name,
                found.origin,
                reference(found)
            );
        }
        let timeout = crate::updates::Settings::from(cfg).timeout;
        if let Some((old, new)) = crate::updates::bump_pin(Path::new(&found.file), r, timeout)? {
            recipe.source.as_mut().expect("has a source").git_ref = Some(new.clone());
            bumped = Some((old, new));
        }
    }
    let outcome = crate::checkout::update(paths, &recipe)?.expect("has a source");
    crate::updates::after_pull(paths, cfg, &recipe);
    let mut msg = format!("checkout {}", outcome.describe());
    if let Some((old, new)) = bumped {
        msg.push_str(&format!(" · ref {} → {}", crate::updates::short(&old), crate::updates::short(&new)));
    }
    Ok(Some(msg))
}

fn reference(f: &Found) -> String {
    format!("{}/{}", f.origin, f.recipe.name)
}

/// The last `n` lines of a log, ANSI codes stripped and `\r` progress bars
/// collapsed (only the last 256 KiB is read).
/// A follower's read of a log: whole lines, and the byte offset the next
/// read starts at. `reset` says the file got shorter than the offset asked
/// for (a new run rewrote it), so these lines are its tail again.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct LogChunk {
    pub lines: Vec<String>,
    pub offset: u64,
    #[serde(default)]
    pub reset: bool,
}

/// Lines of `path` from byte `offset` on (at most 1 MiB a read), or with no
/// offset its last `n` lines. Only whole lines are returned: a line still
/// being written comes with the next read.
pub fn log_since(path: &Path, offset: Option<u64>, n: usize) -> LogChunk {
    const MAX: u64 = 1 << 20;
    let Ok(mut f) = File::open(path) else { return LogChunk::default() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let reset = offset.is_some_and(|o| o > len);
    let tail = offset.is_none() || reset;
    let start = match offset {
        Some(o) if !reset => o,
        _ => len.saturating_sub(256 * 1024),
    };
    let end = if tail { len } else { (start + MAX).min(len) };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return LogChunk { offset: start, reset, ..LogChunk::default() };
    }
    let mut buf = Vec::new();
    let _ = (&mut f).take(end - start).read_to_end(&mut buf);
    let full = buf.len() as u64 == MAX && !tail;
    let cut = match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => i + 1,
        // Reason: a line longer than a whole read would never end; hand it over in pieces.
        None if full => buf.len(),
        None => 0,
    };
    buf.truncate(cut);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .split('\n')
        .skip(usize::from(tail && start > 0))
        .map(|l| crate::ui::strip_ansi(l.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("")))
        .collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if tail {
        let skip = lines.len().saturating_sub(n);
        lines.drain(..skip);
    }
    LogChunk { lines, offset: start + cut as u64, reset }
}

pub fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(mut f) = File::open(path) else { return vec![] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(256 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .split('\n')
        .skip(usize::from(start > 0))
        .map(|l| crate::ui::strip_ansi(l.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("")))
        .collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(n);
    lines.split_off(skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_since_follows_whole_lines_and_resets_on_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, "a\nb\nc\npart").unwrap();
        let first = log_since(&p, None, 2);
        assert_eq!(first.lines, ["b", "c"]);
        assert_eq!(first.offset, 6);
        assert!(!first.reset);
        std::fs::write(&p, "a\nb\nc\npartial\nd\n").unwrap();
        let next = log_since(&p, Some(first.offset), 2);
        assert_eq!(next.lines, ["partial", "d"]);
        assert_eq!(log_since(&p, Some(next.offset), 2).lines, Vec::<String>::new());
        std::fs::write(&p, "new\n").unwrap();
        let again = log_since(&p, Some(next.offset), 10);
        assert!(again.reset);
        assert_eq!(again.lines, ["new"]);
        assert_eq!(log_since(&dir.path().join("none.log"), Some(3), 5), LogChunk::default());
    }

    #[test]
    fn a_server_serves_a_recipe_by_name_repo_or_repo_basename() {
        let r = found("deck", "ds-tf", None).recipe;
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(serves(&ids(&["ds-tf"]), &r));
        assert!(serves(&ids(&["x/y"]), &r));
        assert!(serves(&ids(&["Y"]), &r), "the upstream model name, any case");
        assert!(serves(&ids(&["/models/y"]), &r), "a path ending in the repo name");
        assert!(!serves(&ids(&["other", "x/yy"]), &r));
        assert!(!serves(&[], &r));
    }

    #[test]
    fn a_variant_sharing_a_live_records_port_is_taken_not_adopted() {
        use std::io::{Read, Write};
        let (_d, paths) = sandbox_paths();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().flatten().take(4) {
                let mut s = s;
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let body = r#"{"object":"list","data":[{"id":"y"}]}"#;
                let _ = s.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                        .as_bytes(),
                );
            }
        });
        // Two recipes share the port and model repo (FP8 / NVFP4 twins); only
        // `alpha` has a run record, so `beta` must see the port as taken.
        let alpha = found("deck", "alpha", Some(port)).recipe;
        let beta = found("deck", "beta", Some(port)).recipe;
        let rec = serde_json::json!({
            "name": "alpha", "engine": "llama.cpp", "pid": std::process::id(),
            "port": port, "started_at": 0, "log": "/tmp/x.log", "argv": ["x"], "oneshot": false,
        });
        std::fs::write(paths.run_dir().join("alpha.json"), serde_json::to_string(&rec).unwrap()).unwrap();
        let mut cache = HashMap::new();
        assert_eq!(local_status(&paths, None, &beta, port, &mut cache), Status::PortBusy);
        // With no record, the server is adopted as external.
        super::RunRecord::remove(&paths, &alpha.name);
        let mut cache = HashMap::new();
        assert_eq!(local_status(&paths, None, &beta, port, &mut cache), Status::External);
    }

    #[test]
    fn remote_status_reads_the_hosts_state_and_skips_this_machine() {
        let mut live = crate::hosts::LiveFile::default();
        live.hosts.insert("local".into(), (true, vec!["m".into()]));
        assert_eq!(remote_status(&live, "m"), None, "a local run isn't remote");
        live.hosts.insert("ember".into(), (true, vec!["m".into()]));
        assert_eq!(remote_status(&live, "m"), Some(("ember".into(), Status::Loaded)), "older hosts say no state");
        live.states.insert("ember".into(), [("m".to_string(), "loading".to_string())].into());
        assert_eq!(remote_status(&live, "m"), Some(("ember".into(), Status::Loading)));
        live.hosts.insert("ember".into(), (false, vec!["m".into()]));
        assert_eq!(remote_status(&live, "m"), None, "an offline host's last list doesn't count");
    }

    #[test]
    fn url_host_takes_the_address_of_a_serve_url() {
        assert_eq!(url_host("http://10.100.132.2:7979"), "10.100.132.2");
        assert_eq!(url_host("http://ember.local:7979/"), "ember.local");
        assert_eq!(url_host("http://[fe80::1]:7979"), "fe80::1");
        assert_eq!(url_host("ember"), "ember");
    }

    #[test]
    fn failure_lines_prefer_the_last_errors() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(failure_lines(&l(&["loading", "[4/5] Loading weights", ""])), "[4/5] Loading weights");
        assert_eq!(
            failure_lines(&l(&["ok", "ERROR: no NVFP4 weights at /x", "cleanup", "worker exited 1"])),
            "ERROR: no NVFP4 weights at /x | worker exited 1"
        );
        assert_eq!(failure_lines(&[]), "");
        assert!(failure_lines(&l(&[&"error ".repeat(100)])).ends_with('…'));
    }

    fn found(origin: &str, name: &str, port: Option<u16>) -> Found {
        let engine = match port {
            Some(p) => format!("[engine]\nname = \"llama.cpp\"\nport = {p}\n"),
            None => "[engine]\nname = \"llama.cpp\"\n".to_string(),
        };
        let toml = format!("[model]\nrepo = \"x/y\"\nfiles = [\"a.gguf\"]\n{engine}");
        Found { recipe: Recipe::parse(&toml, name).unwrap(), origin: origin.into(), file: format!("{name}.toml") }
    }

    fn test_cat(entries: Vec<Found>) -> Catalog {
        Catalog { entries, errors: vec![] }
    }

    fn sandbox_paths() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("home");
        std::fs::create_dir_all(root.join("state/run")).unwrap();
        let home = root.display().to_string();
        let paths = Paths::resolve(|k| if k == "LMDECK_HOME" { Some(home.clone()) } else { None }, root);
        (d, paths)
    }

    #[test]
    fn portless_recipes_count_from_port_start() {
        let (_d, paths) = sandbox_paths();
        let cat = test_cat(vec![
            found("deck", "alpha", Some(9000)),
            found("deck", "bravo", None),
            found("deck", "charlie", None),
            found("builtin", "bravo", None), // same name, other origin: counted too
        ]);
        // The default counts from 8000. The deck counts first: deck/bravo,
        // deck/charlie. deck/alpha has an explicit port (9000), which is
        // reserved but not counted. Built-ins are ignored: builtin/bravo
        // keeps its engine default.
        let cfg = Config::default();
        assert_eq!(cfg.serve.port_start, 8000);
        let ports = planned_ports(&cat, &paths, &cfg);
        assert_eq!(ports["deck/bravo"], 8000);
        assert_eq!(ports["deck/charlie"], 8001);
        assert!(!ports.contains_key("builtin/bravo"));
        assert_eq!(planned_port(&ports, &cat.entries[0]), 9000, "explicit port wins");
        assert_eq!(planned_port(&ports, &cat.entries[3]), 8080, "built-ins keep their default");
        // an explicit base moves the counting
        let mut cfg = Config::default();
        cfg.serve.port_start = 8080;
        let ports = planned_ports(&cat, &paths, &cfg);
        assert_eq!(ports["deck/bravo"], 8080);
        assert_eq!(ports["deck/charlie"], 8081);
    }

    #[test]
    fn running_servers_hold_their_port() {
        let (_d, paths) = sandbox_paths();
        let cat = test_cat(vec![found("deck", "alpha", None), found("deck", "bravo", None)]);
        let rec = serde_json::json!({
            "name": "old", "engine": "llama.cpp", "pid": std::process::id(),
            "port": 8080, "started_at": 0, "log": "/tmp/x.log", "argv": ["x"], "oneshot": false,
        });
        std::fs::write(paths.run_dir().join("old.json"), serde_json::to_string(&rec).unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.serve.port_start = 8080;
        let ports = planned_ports(&cat, &paths, &cfg);
        assert_eq!(ports["deck/alpha"], 8081, "8080 is taken by a running server");
        assert_eq!(ports["deck/bravo"], 8082);
    }

    #[test]
    fn tail_collapses_progress_and_ansi() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, "one\n\x1b[32mtwo\x1b[0m\n10%\r50%\r100%\nlast\n").unwrap();
        assert_eq!(tail_lines(&p, 10), vec!["one", "two", "100%", "last"]);
        assert_eq!(tail_lines(&p, 2), vec!["100%", "last"]);
        assert!(tail_lines(&dir.path().join("missing"), 5).is_empty());
    }
}
