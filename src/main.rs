use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use lmdeck::catalog::{self, Catalog, BUILTIN, DECK};
use lmdeck::compat::{self, Level, Report};
use lmdeck::config::{Config, SourceEntry};
use lmdeck::deck::{self, listed, memory_need_gb, resolve, status_word, Deck};
use lmdeck::engine::{self, Inventory, PlanCtx};
use lmdeck::hardware::{which, Hardware};
use lmdeck::models::{self, hf_cache_dir, Presence};
use lmdeck::paths::Paths;
use lmdeck::recipe::{valid_name, Engine, Format, Recipe};
use lmdeck::runner::{self, Probe, Ready, RunRecord, Status};
use lmdeck::sources::{self, Found, SourceSpec};
use lmdeck::ui::{self, bold, cyan, dim, green, red, yellow, Table};
use lmdeck::util::{human_bytes, human_duration, human_gb, now_secs};

#[derive(Parser)]
#[command(name = "lmdeck", version, about = "A deck of local LLM recipes: list, switch, search, and check what fits.")]
struct Cli {
    /// Disable coloured output (also honours NO_COLOR).
    #[arg(long, global = true)]
    no_color: bool,
    /// Bare `lmdeck`: print the table and number prompt instead of the full-screen TUI.
    #[arg(long, global = true)]
    plain: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the deck with load / download status.
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Start a recipe (by name, origin/name, or # from `list`).
    #[command(visible_aliases = ["on", "start"])]
    Use(UseArgs),
    /// Stop every other lmdeck server, then start this one.
    Switch(UseArgs),
    /// Stop a running recipe.
    #[command(visible_alias = "off")]
    Stop {
        target: Option<String>,
        /// Stop everything lmdeck started.
        #[arg(long)]
        all: bool,
    },
    /// Download a recipe's weights (via the Hugging Face CLI).
    Pull {
        target: String,
        /// Print the download command only.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the last lines of a recipe's server log.
    Logs {
        target: String,
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
    },
    /// Find, add, inspect and run recipes.
    #[command(subcommand)]
    Recipe(RecipeCmd),
    /// Manage recipe sources (GitHub / Hugging Face repos, local dirs).
    #[command(subcommand)]
    Source(SourceCmd),
    /// Detect hardware and engines and check which recipes fit.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Full-screen dashboard (also what bare `lmdeck` opens in a terminal).
    Tui(TuiArgs),
    /// Serve the web dashboard and JSON API (localhost only by default).
    Serve {
        /// Interface to bind. Anything but loopback exposes start/stop to the network.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(short, long, default_value_t = lmdeck::web::DEFAULT_PORT)]
        port: u16,
        /// Require `Authorization: Bearer <token>` (generated automatically off-localhost).
        #[arg(long, env = "LMDECK_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Write a default config file.
    Init {
        /// Where model weights live (default ~/models).
        #[arg(long)]
        models_dir: Option<String>,
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args, Default)]
struct TuiArgs {
    /// omp (default), catppuccin or tokyo-night. Also LMDECK_THEME.
    #[arg(long, env = "LMDECK_THEME")]
    theme: Option<String>,
    /// Rounded powerline caps (needs a Nerd Font). Also LMDECK_NERD_FONT=1.
    #[arg(long)]
    nerd: bool,
    /// Start with every known recipe instead of just the deck.
    #[arg(short, long)]
    all: bool,
    /// Render one frame to an HTML file instead (for screenshots).
    #[arg(long, hide = true)]
    render_html: Option<PathBuf>,
    /// Render one frame as plain text to stdout instead.
    #[arg(long, hide = true)]
    render_text: bool,
    #[arg(long, hide = true, default_value_t = 150)]
    width: u16,
    #[arg(long, hide = true, default_value_t = 40)]
    height: u16,
    /// Tab for --render-*: deck, machine or sources.
    #[arg(long, hide = true, default_value = "deck")]
    tab: String,
}

#[derive(Args)]
struct ListArgs {
    /// List model weights found on disk instead (models dir + Hugging Face cache).
    #[arg(short, long)]
    downloaded: bool,
    /// Only servers that are running.
    #[arg(short, long)]
    loaded: bool,
    /// Every known recipe (deck, built-in and sources), not just the deck.
    #[arg(short, long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone)]
struct UseArgs {
    target: String,
    /// Stop other lmdeck servers first.
    #[arg(long)]
    only: bool,
    /// Return once launched instead of waiting for /v1/models.
    #[arg(long)]
    no_wait: bool,
    /// Ignore compatibility, engine and memory checks.
    #[arg(long)]
    force: bool,
    /// Print the launch plan without starting anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum RecipeCmd {
    /// Search every known recipe.
    Search {
        /// Words to match against name, model, engine, quant, tags and description.
        query: Vec<String>,
        #[arg(long)]
        engine: Option<String>,
        /// Quant format or label, e.g. gguf, exl3, nvfp4, Q4_K_M.
        #[arg(long)]
        quant: Option<String>,
        /// Only recipes that fit this machine.
        #[arg(long)]
        compatible: bool,
        /// Only recipes from this origin (deck, builtin, or a source name).
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Copy a recipe into the deck (from a source, a file, or a URL).
    Add {
        reference: String,
        /// Save under a different name.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// List every known recipe by origin.
    List {
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show a recipe, how it fits this machine, and its launch command.
    Show {
        reference: String,
        /// Print the recipe as TOML.
        #[arg(long)]
        toml: bool,
    },
    /// Start a recipe without adding it to the deck.
    Run(UseArgs),
    /// Remove a recipe from the deck.
    #[command(visible_alias = "rm")]
    Remove { name: String },
}

#[derive(Subcommand)]
enum SourceCmd {
    /// Add a source: github:owner/repo[/dir][@ref], hf:owner/repo, a URL, or a directory.
    Add {
        spec: String,
        #[arg(long)]
        name: Option<String>,
        /// Save without fetching.
        #[arg(long)]
        no_fetch: bool,
    },
    /// List sources.
    #[command(visible_alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Re-fetch one source, or all of them.
    #[command(visible_aliases = ["fetch", "sync"])]
    Update { name: Option<String> },
    /// Remove a source and its cached recipes.
    #[command(visible_alias = "rm")]
    Remove { name: String },
}

struct Ctx {
    paths: Paths,
    cfg: Config,
}

impl Ctx {
    fn load() -> Result<Ctx> {
        let paths = Paths::from_env();
        let cfg = Config::load(&paths)?;
        Ok(Ctx { paths, cfg })
    }

    fn catalog(&self) -> Catalog {
        Catalog::load(&self.paths, &self.cfg)
    }

    fn models_dir(&self) -> PathBuf {
        self.cfg.models_dir()
    }

    fn docker(&self) -> String {
        self.cfg.engines.docker.clone().unwrap_or_else(|| "docker".into())
    }

    fn presence(&self, r: &Recipe) -> Presence {
        models::presence(r, &self.models_dir(), &hf_cache_dir())
    }
}

fn main() {
    let cli = Cli::parse();
    ui::init_color(cli.no_color);
    if let Err(e) = run(cli) {
        eprintln!("{} {e:#}", red("error:"));
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let ctx = Ctx::load()?;
    match cli.cmd {
        None if !cli.plain
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::env::var("TERM").map_or(true, |t| t != "dumb") =>
        {
            tui(ctx, TuiArgs::default())
        }
        None => home(&ctx),
        Some(Cmd::Tui(a)) => tui(ctx, a),
        Some(Cmd::Serve { host, port, token }) => {
            lmdeck::web::run(Deck::new(ctx.paths, ctx.cfg), lmdeck::web::WebOptions { host, port, token })
        }
        Some(Cmd::List(a)) => list(&ctx, &a),
        Some(Cmd::Use(a)) => use_recipe(&ctx, &a),
        Some(Cmd::Switch(a)) => use_recipe(&ctx, &UseArgs { only: true, ..a }),
        Some(Cmd::Stop { target, all }) => stop(&ctx, target.as_deref(), all),
        Some(Cmd::Pull { target, dry_run }) => pull(&ctx, &target, dry_run),
        Some(Cmd::Logs { target, lines }) => logs(&ctx, &target, lines),
        Some(Cmd::Recipe(c)) => recipe_cmd(&ctx, c),
        Some(Cmd::Source(c)) => source_cmd(ctx, c),
        Some(Cmd::Doctor { json }) => doctor(&ctx, json),
        Some(Cmd::Init { models_dir, force }) => init(ctx, models_dir, force),
    }
}

// ───────────────────────────── tui ─────────────────────────────

fn tui(ctx: Ctx, a: TuiArgs) -> Result<()> {
    let nerd = a.nerd || std::env::var("LMDECK_NERD_FONT").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    let opts = lmdeck::tui::Options { theme: a.theme, nerd, all: a.all };
    let deck = Deck::new(ctx.paths, ctx.cfg);
    if a.render_html.is_some() || a.render_text {
        let tab = match a.tab.as_str() {
            "deck" => lmdeck::tui::Tab::Deck,
            "machine" | "doctor" => lmdeck::tui::Tab::Machine,
            "sources" => lmdeck::tui::Tab::Sources,
            other => bail!("unknown tab '{other}'"),
        };
        let buf = lmdeck::tui::render_snapshot(&deck, &opts, a.width, a.height, tab);
        if let Some(p) = a.render_html {
            std::fs::write(&p, lmdeck::tui::buffer_html(&buf, "lmdeck tui"))?;
        }
        if a.render_text {
            print!("{}", lmdeck::tui::buffer_text(&buf));
        }
        return Ok(());
    }
    lmdeck::tui::run(deck, opts)
}

// ───────────────────────────── list ─────────────────────────────

fn status_cell(s: &Status) -> String {
    match s {
        Status::Loaded => green("● loaded"),
        Status::Loading => yellow("◐ loading"),
        Status::External => cyan("● external"),
        Status::PortBusy => yellow("! port busy"),
        Status::Stopped => dim("○ stopped"),
    }
}

fn status_tag(s: &Status) -> String {
    match s {
        Status::Loaded => green("loaded"),
        Status::External => cyan("external"),
        Status::Loading => yellow("loading"),
        Status::PortBusy => yellow("port busy"),
        Status::Stopped => String::new(),
    }
}

fn size_cell(r: &Recipe, p: &Presence) -> String {
    if p.present() {
        human_bytes(p.size)
    } else {
        r.model.size_gb.map(|g| dim(&format!("~{}", human_gb(g)))).unwrap_or_else(|| dim("?"))
    }
}

fn downloaded_cell(p: &Presence) -> String {
    match (p.local, p.cached) {
        (true, _) => green("✓"),
        (_, true) => green("✓ hf"),
        _ => dim("✗"),
    }
}

fn deck_table(ctx: &Ctx, entries: &[&Found], loaded_only: bool, json_out: bool) -> Result<()> {
    let mut cache: HashMap<u16, Probe> = HashMap::new();
    let mut t = Table::new(&["#", "", "name", "engine", "quant", "size", "dl", "port", "origin"])
        .right(&[0, 5, 7])
        .max_width(2, 48);
    let mut rows = vec![];
    for (i, f) in entries.iter().enumerate() {
        let r = &f.recipe;
        let st = runner::status_cached(&ctx.paths, &r.name, r.port(), &mut cache);
        if loaded_only && !st.is_up() {
            continue;
        }
        let p = ctx.presence(r);
        rows.push(json!({
            "index": i + 1, "name": r.name, "origin": f.origin, "engine": r.engine.name.as_str(),
            "quant": r.quant_short(), "port": r.port(), "status": status_word(&st),
            "downloaded": p.present(), "size_bytes": p.size, "model": r.model.repo.clone().or(r.model.path.clone()),
        }));
        let glyph = match st {
            Status::Loaded | Status::External => green("●"),
            Status::Loading => yellow("◐"),
            Status::PortBusy => yellow("!"),
            Status::Stopped => dim("○"),
        };
        let mut name = bold(&r.name);
        if st != Status::Stopped {
            name = format!("{name} {}", status_tag(&st));
        }
        t.row(vec![
            (i + 1).to_string(),
            glyph,
            name,
            r.engine.name.as_str().into(),
            r.quant_short(),
            size_cell(r, &p),
            downloaded_cell(&p),
            r.port().to_string(),
            dim(&f.origin),
        ]);
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if t.is_empty() {
        println!("{}", dim(if loaded_only { "nothing is running" } else { "no recipes" }));
    } else {
        print!("{}", t.render());
    }
    Ok(())
}

fn list(ctx: &Ctx, a: &ListArgs) -> Result<()> {
    if a.downloaded {
        return list_downloaded(ctx, a.json);
    }
    let cat = ctx.catalog();
    let entries: Vec<&Found> = if a.all { cat.entries.iter().collect() } else { listed(&cat) };
    if a.loaded && !a.all {
        return list_loaded(ctx, &cat, a.json);
    }
    deck_table(ctx, &entries, a.loaded, a.json)?;
    if !a.json {
        hints(ctx, &cat, a.all);
    }
    Ok(())
}

fn hints(ctx: &Ctx, cat: &Catalog, all: bool) {
    if !all && cat.deck().next().is_none() {
        println!(
            "{}",
            dim("deck is empty: showing built-in examples. Add recipes with `lmdeck recipe add <name>`; find more with `lmdeck recipe search`.")
        );
    }
    let unfetched: Vec<&str> = ctx
        .cfg
        .sources
        .iter()
        .filter(|s| sources::read_index(&ctx.paths.sources_cache_dir().join(&s.name)).is_none())
        .map(|s| s.name.as_str())
        .collect();
    if !unfetched.is_empty() {
        println!(
            "{}",
            dim(&format!("source(s) not fetched yet: {} (run `lmdeck source update`)", unfetched.join(", ")))
        );
    }
    if !cat.errors.is_empty() {
        println!("{}", yellow(&format!("{} recipe file(s) failed to load; see `lmdeck doctor`", cat.errors.len())));
    }
}

fn list_loaded(ctx: &Ctx, cat: &Catalog, json_out: bool) -> Result<()> {
    let mut t = Table::new(&["", "name", "engine", "port", "pid", "up", "endpoint"]).right(&[3, 4, 5]);
    let mut rows = vec![];
    let mut seen = vec![];
    for rec in RunRecord::all(&ctx.paths) {
        let st = runner::status(&ctx.paths, &rec.name, rec.port);
        if !st.is_up() {
            continue;
        }
        seen.push(rec.name.clone());
        let pid = rec.pid.map(|p| p.to_string()).or(rec.container.clone()).unwrap_or_else(|| "-".into());
        let ep = format!("http://localhost:{}/v1", rec.port);
        rows.push(json!({"name": rec.name, "engine": rec.engine, "port": rec.port, "pid": rec.pid,
            "container": rec.container, "status": status_word(&st), "started_at": rec.started_at, "endpoint": ep}));
        t.row(vec![
            status_cell(&st),
            bold(&rec.name),
            rec.engine.clone(),
            rec.port.to_string(),
            pid,
            human_duration(now_secs().saturating_sub(rec.started_at)),
            ep,
        ]);
    }
    // Servers lmdeck didn't start but that serve a deck recipe's name.
    let mut cache = HashMap::new();
    for f in listed(cat) {
        if seen.contains(&f.recipe.name) {
            continue;
        }
        let st = runner::status_cached(&ctx.paths, &f.recipe.name, f.recipe.port(), &mut cache);
        if st == Status::External {
            let ep = format!("http://localhost:{}/v1", f.recipe.port());
            rows.push(json!({"name": f.recipe.name, "engine": f.recipe.engine.name.as_str(), "port": f.recipe.port(),
                "status": "external", "endpoint": ep}));
            t.row(vec![
                status_cell(&st),
                bold(&f.recipe.name),
                f.recipe.engine.name.as_str().into(),
                f.recipe.port().to_string(),
                "-".into(),
                "-".into(),
                ep,
            ]);
        }
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if t.is_empty() {
        println!("{}", dim("nothing is running"));
    } else {
        print!("{}", t.render());
    }
    Ok(())
}

fn list_downloaded(ctx: &Ctx, json_out: bool) -> Result<()> {
    let mut found = models::scan_models_dir(&ctx.models_dir());
    found.extend(models::scan_hf_cache(&hf_cache_dir()));
    if json_out {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(());
    }
    let cat = ctx.catalog();
    let mut t = Table::new(&["model", "kind", "size", "where", "recipes"]).right(&[2]).max_width(0, 60);
    let mut total = 0;
    for d in &found {
        total += d.size;
        let repo_of = |r: &Recipe| r.model.repo.clone().unwrap_or_default();
        let users: Vec<String> = cat
            .entries
            .iter()
            .filter(|f| {
                let repo = repo_of(&f.recipe);
                !repo.is_empty()
                    && (d.id == repo || d.id.starts_with(&format!("{repo}/")) || d.id.starts_with(&format!("{repo}-")))
            })
            .map(|f| f.recipe.name.clone())
            .take(3)
            .collect();
        t.row(vec![bold(&d.id), d.kind.clone(), human_bytes(d.size), dim(&d.location), users.join(", ")]);
    }
    if t.is_empty() {
        println!(
            "{}",
            dim(&format!("no models found in {} or {}", ctx.models_dir().display(), hf_cache_dir().display()))
        );
    } else {
        print!("{}", t.render());
        println!("{}", dim(&format!("{} models, {} total", found.len(), human_bytes(total))));
    }
    Ok(())
}

// ───────────────────────────── use / stop ─────────────────────────────

fn print_report(rep: &Report) {
    for c in &rep.checks {
        let mark = match c.level {
            Level::Ok => green("✓"),
            Level::Warn => yellow("!"),
            Level::Fail => red("✗"),
        };
        println!("  {mark} {:<11} {}", c.what, c.detail);
    }
}

fn use_recipe(ctx: &Ctx, a: &UseArgs) -> Result<()> {
    let cat = ctx.catalog();
    let found = resolve(&cat, &a.target)?;
    let r = &found.recipe;
    if r.nodes() > 1 {
        bail!("{} spans {} nodes; multi-node launch is not supported in lmdeck v0.1", r.name, r.nodes());
    }
    let hw = Hardware::detect();
    let inv = Inventory::detect(&ctx.cfg);
    let p = ctx.presence(r);
    let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size));
    let plan = engine::plan(r, &PlanCtx::new(&ctx.cfg, p.local))?;

    if a.dry_run {
        println!(
            "{} {} {}",
            bold("plan for"),
            cyan(&format!("{}/{}", found.origin, r.name)),
            dim(&format!("({})", r.engine.name))
        );
        print_report(&rep);
        for pre in &plan.pre {
            println!("  pre:  {}", shlex::try_join(pre.iter().map(String::as_str)).unwrap_or_default());
        }
        if let Some(cwd) = &plan.cwd {
            println!("  cwd:  {}", cwd.display());
        }
        for k in plan.env.keys() {
            println!("  env:  {k}");
        }
        println!("  run:  {}", plan.command_line());
        if let Some(s) = &plan.stop {
            println!("  stop: {}", s.join(" "));
        }
        println!("  url:  http://localhost:{}/v1", plan.port);
        return Ok(());
    }

    match runner::status(&ctx.paths, &r.name, plan.port) {
        Status::Loaded | Status::External => {
            println!("{} {} is already up at http://localhost:{}/v1", green("●"), bold(&r.name), plan.port);
            return Ok(());
        }
        Status::Loading => {
            println!("{} {} is still loading (see `lmdeck logs {}`)", yellow("◐"), r.name, r.name);
            return Ok(());
        }
        Status::PortBusy if !a.only => bail!(
            "port {} is already in use by another server; stop it, change engine.port, or use `lmdeck switch`",
            plan.port
        ),
        _ => {}
    }

    if !rep.compatible() && !a.force {
        print_report(&rep);
        bail!("{} doesn't fit this machine ({}); use --force to try anyway", r.name, rep.reasons(false).join("; "));
    }
    if let Err(why) = inv.can_launch(r) {
        if !a.force {
            bail!("{why}; see `lmdeck doctor` (or --force)");
        }
        eprintln!("{} {why}", yellow("warning:"));
    }
    let missing = if r.engine.name.needs_local_weights() { !p.local } else { !p.present() };
    if r.engine.name.needs_downloaded_weights() && missing && !a.force {
        bail!("{}'s weights are not on disk yet; run `lmdeck pull {}` first", r.name, r.name);
    }

    if a.only {
        for rec in RunRecord::all(&ctx.paths) {
            if rec.name != r.name {
                println!("{} stopping {}", dim("○"), rec.name);
                runner::stop(&ctx.paths, &rec.name, &ctx.docker())?;
            }
        }
        if let Status::PortBusy = runner::status(&ctx.paths, &r.name, plan.port) {
            bail!("port {} is still in use by a server lmdeck didn't start", plan.port);
        }
    }

    if let (Some(need), Some(avail)) = (memory_need_gb(r, &hw, &p), hw.available_memory_gb) {
        if need > avail && !a.force {
            bail!(
                "{} needs ~{} but only {} is free; stop something (`lmdeck switch`) or use --force",
                r.name,
                human_gb(need),
                human_gb(avail)
            );
        }
    }

    println!("{} starting {} {}", cyan("▶"), bold(&r.name), dim(&format!("({}, port {})", r.engine.name, plan.port)));
    println!("  {}", dim(&plan.command_line()));
    let rec = runner::start(&ctx.paths, &r.name, r.engine.name.as_str(), &plan)?;
    println!("  {}", dim(&format!("log: {}", rec.log.display())));
    if a.no_wait {
        println!("{} launched; check with `lmdeck list`", yellow("◐"));
        return Ok(());
    }
    let timeout = Duration::from_secs(r.engine.ready_timeout.unwrap_or(900));
    match runner::wait_ready(&rec, timeout, |el| eprintln!("  {}", dim(&format!("…waiting ({}s)", el.as_secs())))) {
        Ready::Ready => {
            println!(
                "{} {} is up at {}",
                green("●"),
                bold(&r.name),
                cyan(&format!("http://localhost:{}/v1", plan.port))
            );
            Ok(())
        }
        Ready::Died => {
            RunRecord::remove(&ctx.paths, &r.name);
            eprintln!("{}", runner::tail(&rec.log, 15));
            bail!("{} exited during startup (full log: {})", r.name, rec.log.display())
        }
        Ready::Timeout => {
            println!(
                "{} {} still loading after {}s; it keeps running (`lmdeck logs {}`)",
                yellow("◐"),
                r.name,
                timeout.as_secs(),
                r.name
            );
            Ok(())
        }
    }
}

fn stop(ctx: &Ctx, target: Option<&str>, all: bool) -> Result<()> {
    let names: Vec<String> = if all {
        RunRecord::all(&ctx.paths).into_iter().map(|r| r.name).collect()
    } else {
        let t = target.ok_or_else(|| anyhow!("say which recipe to stop, or --all"))?;
        let cat = ctx.catalog();
        let name = match resolve(&cat, t) {
            Ok(f) => f.recipe.name.clone(),
            Err(_) if RunRecord::load(&ctx.paths, t).is_some() => t.to_string(),
            Err(e) => return Err(e),
        };
        vec![name]
    };
    if names.is_empty() {
        println!("{}", dim("nothing to stop"));
    }
    for n in names {
        if runner::stop(&ctx.paths, &n, &ctx.docker())? {
            println!("{} stopped {}", dim("○"), bold(&n));
        } else {
            println!("{} {n} was not started by lmdeck", dim("○"));
        }
    }
    Ok(())
}

fn logs(ctx: &Ctx, target: &str, lines: usize) -> Result<()> {
    let cat = ctx.catalog();
    let name = resolve(&cat, target).map(|f| f.recipe.name.clone()).unwrap_or_else(|_| target.to_string());
    let path = ctx.paths.log_dir().join(format!("{name}.log"));
    if !path.exists() {
        bail!("no log for {name} at {}", path.display());
    }
    println!("{}", runner::tail(&path, lines));
    Ok(())
}

// ───────────────────────────── pull ─────────────────────────────

fn pull(ctx: &Ctx, target: &str, dry_run: bool) -> Result<()> {
    let cat = ctx.catalog();
    let r = &resolve(&cat, target)?.recipe;
    let argv = deck::pull_argv(&ctx.cfg, r)?;
    let line = shlex::try_join(argv.iter().map(String::as_str)).unwrap_or_else(|_| argv.join(" "));
    if dry_run {
        println!("{line}");
        return Ok(());
    }
    if which(&argv[0]).is_none() {
        let hint = deck::pull_tool_hint(r);
        bail!("{hint}; then run:\n  {line}");
    }
    println!("{} {}", cyan("↓"), dim(&line));
    let status = Command::new(&argv[0]).args(&argv[1..]).status().with_context(|| format!("running {}", argv[0]))?;
    if !status.success() {
        bail!("download failed ({status})");
    }
    let p = ctx.presence(r);
    println!("{} {} ({})", green("✓"), bold(&r.name), human_bytes(p.size));
    Ok(())
}

// ───────────────────────────── recipe ─────────────────────────────

fn recipe_cmd(ctx: &Ctx, c: RecipeCmd) -> Result<()> {
    match c {
        RecipeCmd::Search { query, engine, quant, compatible, source, json } => {
            recipe_search(ctx, &query, engine, quant, compatible, source, json)
        }
        RecipeCmd::Add { reference, name, force } => recipe_add(ctx, &reference, name, force),
        RecipeCmd::List { source, json } => recipe_list(ctx, source, json),
        RecipeCmd::Show { reference, toml } => recipe_show(ctx, &reference, toml),
        RecipeCmd::Run(a) => use_recipe(ctx, &a),
        RecipeCmd::Remove { name } => {
            if catalog::remove_from_deck(&ctx.paths, &name)? {
                println!("removed {name} from the deck");
                Ok(())
            } else {
                bail!("{name} is not in the deck")
            }
        }
    }
}

fn fit_cell(rep: &Report) -> String {
    match rep.level() {
        Level::Ok => green("✓"),
        Level::Warn => yellow("!"),
        Level::Fail => red("✗"),
    }
}

fn recipe_search(
    ctx: &Ctx,
    query: &[String],
    engine: Option<String>,
    quant: Option<String>,
    compatible: bool,
    source: Option<String>,
    json_out: bool,
) -> Result<()> {
    let cat = ctx.catalog();
    let engine: Option<Engine> = engine.map(|e| e.parse::<Engine>()).transpose()?;
    let words: Vec<String> = query.iter().flat_map(|q| q.split_whitespace()).map(str::to_lowercase).collect();
    let hw = Hardware::detect();
    let mut t = Table::new(&["fit", "recipe", "engine", "quant", "size", "description"])
        .right(&[4])
        .max_width(1, 44)
        .max_width(5, 60);
    let mut rows = vec![];
    for f in &cat.entries {
        let r = &f.recipe;
        let text = r.search_text().to_lowercase();
        if !words.iter().all(|w| text.contains(w)) {
            continue;
        }
        if engine.is_some_and(|e| e != r.engine.name) || source.as_deref().is_some_and(|s| s != f.origin) {
            continue;
        }
        if let Some(q) = &quant {
            if !r.quant_short().to_lowercase().contains(&q.to_lowercase()) {
                continue;
            }
        }
        let p = ctx.presence(r);
        let rep = compat::check(r, &hw, None, p.present().then_some(p.size));
        if compatible && !rep.compatible() {
            continue;
        }
        rows.push(json!({"ref": format!("{}/{}", f.origin, r.name), "name": r.name, "origin": f.origin,
            "engine": r.engine.name.as_str(), "quant": r.quant_short(), "fit": rep.level(),
            "reasons": rep.reasons(true), "model": r.model.repo, "description": r.description}));
        t.row(vec![
            fit_cell(&rep),
            format!("{}{}", dim(&format!("{}/", f.origin)), bold(&r.name)),
            r.engine.name.as_str().into(),
            r.quant_short(),
            size_cell(r, &p),
            r.description.clone().unwrap_or_default(),
        ]);
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if t.is_empty() {
        println!("{}", dim("no matching recipes"));
    } else {
        print!("{}", t.render());
        println!(
            "{}",
            dim(&format!(
                "{} recipe(s) · fit: ✓ ok  ! warning  ✗ won't fit · `lmdeck recipe show <ref>` for details",
                rows.len()
            ))
        );
    }
    hints(ctx, &cat, true);
    Ok(())
}

fn fetch_url(url: &str) -> Result<String> {
    let url = if let Some(rest) = url.strip_prefix("https://github.com/") {
        format!("https://raw.githubusercontent.com/{}", rest.replacen("/blob/", "/", 1))
    } else if url.starts_with("https://huggingface.co/") {
        url.replacen("/blob/", "/resolve/", 1)
    } else {
        url.to_string()
    };
    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .get(&url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    Ok(resp.into_string()?)
}

fn recipe_add(ctx: &Ctx, reference: &str, name: Option<String>, force: bool) -> Result<()> {
    let mut recipe = if catalog::is_file_ref(reference) {
        Recipe::load(Path::new(reference))?
    } else if reference.starts_with("https://") || reference.starts_with("http://") {
        let path_part = reference.split(['?', '#']).next().unwrap_or(reference);
        let format =
            Format::from_path(Path::new(path_part)).ok_or_else(|| anyhow!("URL must end in .toml, .yaml or .yml"))?;
        let stem = Path::new(path_part).file_stem().and_then(|s| s.to_str()).unwrap_or("recipe").to_string();
        Recipe::parse(&fetch_url(reference)?, format, &stem)?
    } else {
        let cat = ctx.catalog();
        let f = cat.resolve(reference)?;
        if f.origin == DECK && name.is_none() {
            bail!("{} is already in the deck", f.recipe.name);
        }
        f.recipe.clone()
    };
    if let Some(n) = name {
        if !valid_name(&n) {
            bail!("invalid name '{n}': use letters, digits, '.', '_' or '-'");
        }
        recipe.name = n;
    }
    let path = catalog::add_to_deck(&ctx.paths, &recipe, force)?;
    println!("{} added {} to the deck {}", green("✓"), bold(&recipe.name), dim(&format!("({})", path.display())));
    Ok(())
}

fn recipe_list(ctx: &Ctx, source: Option<String>, json_out: bool) -> Result<()> {
    let cat = ctx.catalog();
    let entries: Vec<&Found> = cat.entries.iter().filter(|f| source.as_deref().is_none_or(|s| s == f.origin)).collect();
    if json_out {
        let rows: Vec<_> = entries
            .iter()
            .map(|f| {
                json!({"ref": format!("{}/{}", f.origin, f.recipe.name), "name": f.recipe.name, "origin": f.origin,
                "file": f.file, "engine": f.recipe.engine.name.as_str(), "quant": f.recipe.quant_short(),
                "model": f.recipe.model.repo.clone().or(f.recipe.model.path.clone())})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let mut t = Table::new(&["origin", "name", "engine", "quant", "model"]).max_width(4, 56);
    for f in &entries {
        t.row(vec![
            dim(&f.origin),
            bold(&f.recipe.name),
            f.recipe.engine.name.as_str().into(),
            f.recipe.quant_short(),
            f.recipe.model.repo.clone().or(f.recipe.model.path.clone()).unwrap_or_default(),
        ]);
    }
    if t.is_empty() {
        println!("{}", dim("no recipes"));
    } else {
        print!("{}", t.render());
        println!("{}", dim(&format!("{} recipe(s)", entries.len())));
    }
    Ok(())
}

fn recipe_show(ctx: &Ctx, reference: &str, as_toml: bool) -> Result<()> {
    let cat = ctx.catalog();
    let f = resolve(&cat, reference)?;
    let r = &f.recipe;
    if as_toml {
        print!("{}", r.to_toml()?);
        return Ok(());
    }
    println!("{} {}", bold(r.display()), dim(&format!("{}/{}", f.origin, r.name)));
    if let Some(d) = &r.description {
        println!("  {d}");
    }
    println!();
    let kv = |k: &str, v: String| println!("  {:<12} {v}", dim(k));
    kv("file", f.file.clone());
    if let Some(repo) = &r.model.repo {
        kv("model", format!("hf:{repo}{}", r.model.revision.as_ref().map(|v| format!("@{v}")).unwrap_or_default()));
    }
    if let Some(p) = &r.model.path {
        kv("path", p.clone());
    }
    if !r.model.files.is_empty() {
        kv("files", r.model.files.join(", "));
    }
    kv("quant", r.quant_short());
    let p = ctx.presence(r);
    kv(
        "weights",
        if p.present() {
            format!("{} {}", green("✓"), human_bytes(p.size))
        } else {
            format!("{} {}", dim("✗ not downloaded"), size_cell(r, &p))
        },
    );
    kv(
        "engine",
        format!(
            "{} · port {}{}",
            r.engine.name,
            r.port(),
            r.engine.ctx.map(|c| format!(" · ctx {c}")).unwrap_or_default()
        ),
    );
    if let Some(img) = &r.engine.image {
        kv("image", img.clone());
    }
    if !r.tags.is_empty() {
        kv("tags", r.tags.join(", "));
    }
    let hw = Hardware::detect();
    let inv = Inventory::detect(&ctx.cfg);
    let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size));
    println!("\n  {} {}", bold("on this machine"), dim(&format!("({})", hw.platform)));
    print_report(&rep);
    match engine::plan(r, &PlanCtx::new(&ctx.cfg, p.local)) {
        Ok(plan) => println!("\n  {}\n  {}", bold("launch"), plan.command_line()),
        Err(e) => println!("\n  {} {e:#}", yellow("launch:")),
    }
    Ok(())
}

// ───────────────────────────── source ─────────────────────────────

fn source_cmd(mut ctx: Ctx, c: SourceCmd) -> Result<()> {
    match c {
        SourceCmd::Add { spec, name, no_fetch } => {
            let parsed = SourceSpec::parse(&spec)?;
            let name = name.unwrap_or_else(|| parsed.default_name());
            if !valid_name(&name) || name == DECK || name == BUILTIN {
                bail!("invalid source name '{name}'");
            }
            if let Some(existing) = ctx.cfg.source(&name) {
                bail!("a source named '{name}' already exists ({}); pass --name", existing.spec);
            }
            if !no_fetch {
                fetch_source(&ctx, &name, &parsed)?;
            }
            ctx.cfg.sources.push(SourceEntry { name: name.clone(), spec: parsed.canonical() });
            ctx.cfg.save(&ctx.paths)?;
            println!("{} added source {} {}", green("✓"), bold(&name), dim(&parsed.canonical()));
            Ok(())
        }
        SourceCmd::List { json } => {
            let mut rows = vec![];
            let mut t = Table::new(&["name", "spec", "recipes", "fetched"]).right(&[2]);
            for s in &ctx.cfg.sources {
                let dir = ctx.paths.sources_cache_dir().join(&s.name);
                let idx = sources::read_index(&dir);
                let n = idx.as_ref().map(|_| sources::load_dir(&s.name, &dir).0.len());
                let age =
                    idx.as_ref().map(|i| format!("{} ago", human_duration(now_secs().saturating_sub(i.fetched_at))));
                rows.push(
                    json!({"name": s.name, "spec": s.spec, "recipes": n, "fetched_at": idx.map(|i| i.fetched_at)}),
                );
                t.row(vec![
                    bold(&s.name),
                    s.spec.clone(),
                    n.map(|n| n.to_string()).unwrap_or_else(|| dim("-")),
                    age.unwrap_or_else(|| dim("never")),
                ]);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if t.is_empty() {
                println!("{}", dim("no sources; add one with `lmdeck source add github:owner/repo`"));
            } else {
                print!("{}", t.render());
            }
            Ok(())
        }
        SourceCmd::Update { name } => {
            let targets: Vec<SourceEntry> = match &name {
                Some(n) => vec![ctx.cfg.source(n).cloned().ok_or_else(|| anyhow!("no source named '{n}'"))?],
                None => ctx.cfg.sources.clone(),
            };
            if targets.is_empty() {
                println!("{}", dim("no sources configured"));
            }
            let mut failed = 0;
            for s in targets {
                if let Err(e) = SourceSpec::parse(&s.spec).and_then(|p| fetch_source(&ctx, &s.name, &p)) {
                    eprintln!("{} {}: {e:#}", red("✗"), s.name);
                    failed += 1;
                }
            }
            if failed > 0 {
                bail!("{failed} source(s) failed to update");
            }
            Ok(())
        }
        SourceCmd::Remove { name } => {
            let before = ctx.cfg.sources.len();
            ctx.cfg.sources.retain(|s| s.name != name);
            if ctx.cfg.sources.len() == before {
                bail!("no source named '{name}'");
            }
            ctx.cfg.save(&ctx.paths)?;
            let _ = std::fs::remove_dir_all(ctx.paths.sources_cache_dir().join(&name));
            println!("removed source {name}");
            Ok(())
        }
    }
}

fn fetch_source(ctx: &Ctx, name: &str, spec: &SourceSpec) -> Result<()> {
    eprint!("{} fetching {} … ", cyan("↓"), spec.canonical());
    let _ = std::io::stderr().flush();
    let dir = ctx.paths.sources_cache_dir().join(name);
    let idx = sources::sync(spec, &dir).inspect_err(|_| eprintln!())?;
    let (found, errors) = sources::load_dir(name, &dir);
    eprintln!("{} recipes from {} files", found.len(), idx.files.len());
    for (file, e) in errors.iter().take(5) {
        eprintln!("  {} {file}: {e}", yellow("!"));
    }
    if errors.len() > 5 {
        eprintln!("  {}", dim(&format!("…and {} more", errors.len() - 5)));
    }
    Ok(())
}

// ───────────────────────────── doctor / init ─────────────────────────────

fn doctor(ctx: &Ctx, json_out: bool) -> Result<()> {
    let hw = Hardware::detect();
    let inv = Inventory::detect(&ctx.cfg);
    let cat = ctx.catalog();
    let checked: Vec<(&Found, Report)> = listed(&cat)
        .into_iter()
        .map(|f| {
            let p = ctx.presence(&f.recipe);
            let rep = compat::check(&f.recipe, &hw, Some(&inv), p.present().then_some(p.size));
            (f, rep)
        })
        .collect();
    let paths = json!({
        "config": ctx.paths.config_file(), "recipes": ctx.paths.recipes_dir(), "cache": ctx.paths.cache_dir,
        "state": ctx.paths.state_dir, "models_dir": ctx.models_dir(), "hf_cache": hf_cache_dir(),
    });
    if json_out {
        let recipes: Vec<_> = checked
            .iter()
            .map(|(f, rep)| json!({"ref": format!("{}/{}", f.origin, f.recipe.name), "fit": rep.level(), "checks": rep.checks}))
            .collect();
        let errors: Vec<_> = cat.errors.iter().map(|(f, e)| json!({"file": f, "error": e})).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"version": env!("CARGO_PKG_VERSION"), "hardware": hw, "inventory": inv,
                "paths": paths, "recipes": recipes, "recipe_errors": errors})
            )?
        );
        return Ok(());
    }
    print!("{}", ui::banner());
    println!("\n{}", bold("machine"));
    println!("  {:<10} {} {}", dim("host"), hw.hostname, dim(&format!("({} {})", hw.os, hw.arch)));
    println!("  {:<10} {}", dim("platform"), bold(&hw.platform));
    println!("  {:<10} {} ({} cores)", dim("cpu"), hw.cpu, hw.cores);
    let mem = match hw.available_memory_gb {
        Some(a) => format!(
            "{} total, {} free{}",
            human_gb(hw.total_memory_gb),
            human_gb(a),
            if hw.unified_memory { " · unified" } else { "" }
        ),
        None => human_gb(hw.total_memory_gb),
    };
    println!("  {:<10} {mem}", dim("memory"));
    for g in &hw.gpus {
        let mut s = format!("{} {}", g.vendor, g.name);
        if let Some(m) = g.memory_gb {
            s.push_str(&format!(" · {}", human_gb(m)));
        }
        if let Some(cc) = &g.compute_capability {
            s.push_str(&format!(" · sm {cc}"));
        }
        if let Some(d) = &g.driver {
            s.push_str(&format!(" · driver {d}"));
        }
        println!("  {:<10} {s}", dim("gpu"));
    }

    println!("\n{}", bold("engines"));
    for s in inv.engines.values().chain(inv.tools.values()) {
        let mark = if s.available { green("✓") } else { dim("✗") };
        println!("  {mark} {:<14} {}", s.name, dim(&s.detail));
    }

    println!("\n{}", bold("paths"));
    for (k, v) in paths.as_object().into_iter().flatten() {
        println!("  {:<10} {}", dim(k), v.as_str().unwrap_or_default());
    }

    println!("\n{}", bold("sources"));
    if ctx.cfg.sources.is_empty() {
        println!("  {}", dim("none"));
    }
    for s in &ctx.cfg.sources {
        let state = match sources::read_index(&ctx.paths.sources_cache_dir().join(&s.name)) {
            Some(i) => format!(
                "{} files, fetched {} ago",
                i.files.len(),
                human_duration(now_secs().saturating_sub(i.fetched_at))
            ),
            None => yellow("not fetched (lmdeck source update)"),
        };
        println!("  {:<10} {} {}", s.name, dim(&s.spec), state);
    }

    let label = if cat.deck().next().is_some() { "deck" } else { "built-in recipes" };
    println!("\n{}", bold(&format!("{label} on this machine")));
    let mut t = Table::new(&["fit", "recipe", "engine", "notes"]).max_width(3, 80);
    for (f, rep) in &checked {
        let notes = rep.reasons(true);
        t.row(vec![
            fit_cell(rep),
            bold(&f.recipe.name),
            f.recipe.engine.name.as_str().into(),
            if notes.is_empty() { dim("ready") } else { notes.join("; ") },
        ]);
    }
    print!("{}", t.render());
    if !cat.errors.is_empty() {
        println!("\n{}", bold("recipe files that failed to load"));
        for (f, e) in &cat.errors {
            println!("  {} {f}: {}", yellow("!"), dim(e));
        }
    }
    Ok(())
}

fn init(mut ctx: Ctx, models_dir: Option<String>, force: bool) -> Result<()> {
    let path = ctx.paths.config_file();
    if path.exists() && !force {
        bail!("{} already exists (use --force to overwrite)", path.display());
    }
    if force {
        ctx.cfg = Config::default();
    }
    if let Some(m) = models_dir {
        ctx.cfg.models_dir = m;
    }
    let written = ctx.cfg.save(&ctx.paths)?;
    std::fs::create_dir_all(ctx.paths.recipes_dir())?;
    println!("{} wrote {}", green("✓"), written.display());
    println!("  next: `lmdeck source update` to fetch recipes, then `lmdeck recipe search`");
    Ok(())
}

// ───────────────────────────── home ─────────────────────────────

fn home(ctx: &Ctx) -> Result<()> {
    print!("{}", ui::banner());
    println!();
    let cat = ctx.catalog();
    let entries = listed(&cat);
    deck_table(ctx, &entries, false, false)?;
    hints(ctx, &cat, false);
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive || entries.is_empty() {
        return Ok(());
    }
    let stdin = std::io::stdin();
    loop {
        print!("{}", dim("toggle # (enter to quit): "));
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        let choice = line.trim();
        if choice.is_empty() || choice == "q" {
            return Ok(());
        }
        let Ok(f) = resolve(&cat, choice) else {
            println!("{}", yellow("pick a number from the table"));
            continue;
        };
        let name = f.recipe.name.clone();
        let st = runner::status(&ctx.paths, &name, f.recipe.port());
        let res = if matches!(st, Status::Loaded | Status::Loading) {
            stop(ctx, Some(&name), false)
        } else {
            use_recipe(ctx, &UseArgs { target: name, only: false, no_wait: false, force: false, dry_run: false })
        };
        if let Err(e) = res {
            eprintln!("{} {e:#}", red("error:"));
        }
        println!();
        deck_table(ctx, &entries, false, false)?;
    }
}
