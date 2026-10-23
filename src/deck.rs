//! The shared service layer behind the TUI and the web dashboard: one
//! snapshot of every recipe (status, weights, fit) plus the actions
//! (start / stop / switch / pull) that both front-ends drive.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::collections::HashMap;
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

/// The recipes `list` numbers: the deck, or the built-ins while the deck is empty.
pub fn listed(cat: &Catalog) -> Vec<&Found> {
    let deck: Vec<&Found> = cat.deck().collect();
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
    let local = r.local_path(&cfg.models_dir());
    let dir = match (r.engine.name, local) {
        (Engine::LlamaCpp, Some(p)) => p.parent().map(Path::to_path_buf),
        (Engine::TabbyApi, Some(p)) => Some(p),
        _ => None,
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
    pub fn running(&self) -> usize {
        self.recipes.iter().filter(|r| r.is_up()).count()
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
        let mut updates = crate::updates::views(
            &self.paths,
            &crate::updates::Settings::from(&self.cfg),
            cat.entries.iter().map(|f| &f.recipe),
        );
        let mut recipes = vec![];
        for f in &cat.entries {
            let r = &f.recipe;
            let reference = reference(f);
            let index = listed_refs.iter().position(|x| *x == reference).map(|i| i + 1).unwrap_or(0);
            let p = self.presence(r);
            let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size));
            let (command, plan_error) = match engine::plan(r, &PlanCtx::new(&self.cfg, p.local)) {
                Ok(plan) => (Some(plan.command_line()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            };
            let status = runner::status_cached(&self.paths, &r.name, r.port(), &mut cache);
            let rec = if status.is_up() { RunRecord::load(&self.paths, &r.name) } else { None };
            recipes.push(RecipeView {
                index,
                name: r.name.clone(),
                reference,
                origin: f.origin.clone(),
                display_name: r.display().to_string(),
                description: r.description.clone(),
                tags: r.tags.clone(),
                engine: r.engine.name.as_str().into(),
                quant: r.quant_short(),
                model: r.model.repo.clone().or(r.model.path.clone()),
                revision: r.model.revision.clone(),
                port: r.port(),
                ctx: r.engine.ctx,
                endpoint: format!("http://localhost:{}/v1", r.port()),
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
        if r.nodes() > 1 {
            bail!("{} spans {} nodes; multi-node launch is not supported", r.name, r.nodes());
        }
        let hw = Hardware::detect();
        let inv = self.inventory(false);
        let p = self.presence(r);
        let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size));
        let plan = engine::plan(r, &PlanCtx::new(&self.cfg, p.local))?;
        match runner::status(&self.paths, &r.name, plan.port) {
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
        let mut msg = format!("starting {} on :{}", r.name, plan.port);
        if !stopped.is_empty() {
            msg.push_str(&format!(" (stopped {})", stopped.join(", ")));
        }
        Ok(msg)
    }

    pub fn stop(&self, target: &str) -> Result<String> {
        let cat = self.catalog();
        let name = match resolve(&cat, target) {
            Ok(f) => f.recipe.name.clone(),
            Err(_) if RunRecord::load(&self.paths, target).is_some() => target.to_string(),
            Err(e) => return Err(e),
        };
        if runner::stop(&self.paths, &name, &self.docker())? {
            Ok(format!("stopped {name}"))
        } else {
            bail!("{name} was not started by lmdeck")
        }
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
            if r.model.repo.is_none() {
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

    /// Resolves `target` to a recipe name (also accepts names of running servers).
    pub fn name_of(&self, target: &str) -> Result<String> {
        let cat = self.catalog();
        match resolve(&cat, target) {
            Ok(f) => Ok(f.recipe.name.clone()),
            Err(_) if RunRecord::load(&self.paths, target).is_some() => Ok(target.to_string()),
            Err(e) => Err(e),
        }
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
    fn tail_collapses_progress_and_ansi() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, "one\n\x1b[32mtwo\x1b[0m\n10%\r50%\r100%\nlast\n").unwrap();
        assert_eq!(tail_lines(&p, 10), vec!["one", "two", "100%", "last"]);
        assert_eq!(tail_lines(&p, 2), vec!["100%", "last"]);
        assert!(tail_lines(&dir.path().join("missing"), 5).is_empty());
    }
}
