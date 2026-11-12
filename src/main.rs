use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use lmdeck::catalog::{self, Catalog, BUILTIN, DECK};
use lmdeck::checkout;
use lmdeck::compat::{self, Level, Report};
use lmdeck::config::{Config, SourceEntry};
use lmdeck::deck::{self, listed, memory_need_gb, resolve, status_word, Deck};
use lmdeck::engine::{self, Inventory, PlanCtx};
use lmdeck::fabric;
use lmdeck::hardware::{which, Hardware};
use lmdeck::hosts::{self, HostView, HostsFile};
use lmdeck::models::{self, hf_cache_dir, Presence};
use lmdeck::paths::{home_dir, Paths};
use lmdeck::recipe::{valid_name, Engine, Recipe};
use lmdeck::runner::{self, Probe, Ready, RunRecord, Status};
use lmdeck::sources::{self, Found, SourceSpec};
use lmdeck::topology;
use lmdeck::ui::{self, bold, cyan, dim, green, red, yellow, Table};
use lmdeck::updates::{self, UpdateView};
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
        /// Stop it on this host (through its `lmdeck serve`).
        #[arg(long, value_name = "HOST")]
        on: Option<String>,
    },
    /// Add a repo to the deck: its lmdeck.toml if present, else wrap start.sh / stop.sh.
    Add {
        /// A name from the recipe index (`lmdeck browse`), GitHub owner/repo,
        /// a GitHub or git URL, or a local directory.
        repo: String,
        /// Recipe name (default: the repo name).
        #[arg(long)]
        name: Option<String>,
        /// Branch, tag or commit to pin (default: the default branch's current commit).
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Overwrite a deck recipe with the same name.
        #[arg(long)]
        force: bool,
    },
    /// Download a recipe's weights (via the Hugging Face CLI), or update a wrapped recipe's checkout.
    Pull {
        target: String,
        /// Print the download command only.
        #[arg(long)]
        dry_run: bool,
        /// Wrapped recipes pinned to a commit or tag: move the pin to the
        /// upstream branch head (rewrites `ref` in the deck recipe).
        #[arg(long)]
        latest: bool,
    },
    /// Browse the recipe index: search, filter by tag, see what fits and install.
    Browse {
        /// Fuzzy search over names, descriptions, repos and tags.
        query: Option<String>,
        /// Only entries with this tag (`exl3`, `engine=vllm`, `sparks=2`); repeatable.
        #[arg(long = "tag", short = 't')]
        tags: Vec<String>,
        /// name, sparks or speed (fastest first).
        #[arg(long, default_value = "name")]
        sort: String,
        /// Fetch the index now even if the cache is fresh.
        #[arg(long)]
        refresh: bool,
        /// Use the cached index only (no network).
        #[arg(long, conflicts_with = "refresh")]
        offline: bool,
        /// Only entries that fit this machine (and your hosts).
        #[arg(long)]
        fits: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check wrapped recipes' repos for new commits (always asks the remotes).
    #[command(visible_alias = "updates")]
    Outdated {
        /// Only these recipes (default: every wrapped recipe).
        targets: Vec<String>,
        /// Fetch into the checkouts to count commits behind (does not move them).
        #[arg(long)]
        fetch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show the last lines of a recipe's server log.
    Logs {
        target: String,
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
        /// Read it from this host.
        #[arg(long, value_name = "HOST")]
        on: Option<String>,
        /// Multi-node recipes: this node's log (default: the head).
        #[arg(long)]
        node: Option<String>,
        /// Keep printing new lines as they are written (also from another host).
        #[arg(short, long)]
        follow: bool,
    },
    /// Which host holds which model's weights (asks every host).
    Weights {
        /// Ignore the cached index (refreshed every 60 s).
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Find, add, inspect and run recipes.
    #[command(subcommand)]
    Recipe(RecipeCmd),
    /// Manage recipe sources (GitHub / Hugging Face repos, local dirs).
    #[command(subcommand)]
    Source(SourceCmd),
    /// Other machines running `lmdeck serve` (DGX Sparks, Macs) to watch.
    #[command(subcommand, visible_alias = "hosts")]
    Host(HostCmd),
    /// Detect hardware and engines and check which recipes fit.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Install an inference engine into lmdeck's own cache (no sudo).
    Engine {
        #[command(subcommand)]
        cmd: EngineCmd,
    },
    /// Colour themes for the TUI and web dashboard (Omarchy's, plus omp).
    #[command(subcommand)]
    Theme(ThemeCmd),
    /// Agent harnesses (opencode, omp, grok, pi, hermes) lmdeck points at served models.
    Harness {
        #[command(subcommand)]
        cmd: Option<HarnessCmd>,
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
    /// Run the setup wizard again: this machine, the other Sparks on the CX7
    /// links (and lmdeck on them over SSH), recipe sources.
    Setup,
}

#[derive(Subcommand)]
enum ThemeCmd {
    /// Every theme, with swatches; the current one is marked.
    #[command(visible_alias = "ls")]
    List,
    /// Save a theme to config.toml (names are forgiving: "Rosé Pine", rose-pine, rose).
    Set { name: String },
    /// Show the current theme (or the named one): its colours and source.
    Show { name: Option<String> },
    /// Use the theme Omarchy is currently set to, if it is one lmdeck has.
    SyncOmarchy,
    /// Forget the saved theme (back to omp).
    Reset,
}

#[derive(Subcommand)]
enum HarnessCmd {
    /// Show every known harness: detected, enabled, config path.
    #[command(visible_alias = "ls")]
    List,
    /// Sync every enabled harness now.
    Sync,
    /// Start syncing a harness (its config is written on the next serve).
    Enable { name: String },
    /// Stop syncing a harness (entries already written stay until rewritten).
    Disable { name: String },
    /// Which hosts' running models are included in the sync: `all`, `none`,
    /// or a list of hosts.toml names.
    Hosts { hosts: Vec<String> },
    /// Mirror opencode.json to ~/.local/share/opencode-export/ (the copy the
    /// Mac's pull job fetches): `on` or `off`.
    Export { on: String },
    /// Make this machine the hub, which pushes every host's running models to
    /// every host in hosts.toml after each start or stop (its serve also
    /// looks for changes made elsewhere): `on` or `off`.
    Hub { on: String },
}

#[derive(Args, Default)]
struct TuiArgs {
    /// Theme for this run only (see `lmdeck theme list`; `lmdeck theme set` saves one). Also LMDECK_THEME.
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
    /// For --render-*: show the theme picker.
    #[arg(long, hide = true)]
    picker: bool,
    /// For --render-*: select this recipe.
    #[arg(long, hide = true)]
    select: Option<String>,
    /// For --render-*: select this host on the hosts tab.
    #[arg(long, hide = true)]
    host: Option<String>,
    /// For --render-*: open the run-on picker for the selected recipe.
    #[arg(long, hide = true)]
    run_on: bool,
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
    /// Don't group the table by model family (Qwen, GLM, …).
    #[arg(long)]
    flat: bool,
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
    /// Install a missing engine without asking.
    #[arg(long)]
    yes: bool,
    /// Never install or pull anything missing; fail with the command to run.
    #[arg(long)]
    no_install: bool,
    /// Run on this host (a hosts.toml name, or `local`). The weights stay on
    /// their home and are mounted read-only over NFS on the CX7 link.
    #[arg(long, value_name = "HOST")]
    on: Option<String>,
    /// Allow streaming weights or a multi-node run over LAN / Tailscale when
    /// there is no ConnectX-7 link.
    #[arg(long)]
    allow_slow_link: bool,
}

#[derive(Subcommand)]
enum EngineCmd {
    /// Install a managed engine build. Currently llama.cpp.
    Install {
        /// Engine name (llama.cpp).
        engine: String,
        /// Reinstall even when the managed build is already present.
        #[arg(long)]
        force: bool,
    },
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

#[derive(Subcommand)]
enum HostCmd {
    /// Add (or replace) a host: `lmdeck host add spark1 http://spark1:7979 --token …`.
    Add {
        name: String,
        /// Base URL of the host's `lmdeck serve`, scheme and port included.
        url: String,
        /// The token that host's `lmdeck serve --token` expects (stored in hosts.toml, mode 0600).
        #[arg(long)]
        token: Option<String>,
        /// Read the token from this environment variable instead (only the name is stored).
        #[arg(long, value_name = "VAR")]
        token_env: Option<String>,
        /// This host's address on the fast (CX7) link, when auto-detection can't tell.
        #[arg(long, value_name = "IP")]
        link_addr: Option<String>,
        /// Save without contacting the host.
        #[arg(long)]
        no_check: bool,
    },
    /// Poll every host once and show what it reports.
    #[command(visible_aliases = ["ls", "check"])]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Forget a host.
    #[command(visible_alias = "rm")]
    Remove { name: String },
    /// Make every host's hosts.toml list the whole cluster: reads missing
    /// tokens over ssh, then pushes each host the others and this machine.
    /// Only adds or updates entries; safe to rerun.
    Sync {
        /// Show what each host would get; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge hosts (a hosts.toml read from stdin) into this machine's
    /// hosts.toml; what `host sync` runs on each peer.
    #[command(hide = true)]
    Import {
        /// This machine's name(s) as the sender knows it; never added.
        #[arg(long)]
        skip: Vec<String>,
    },
    /// How the hosts are cabled: CX7 / LAN / Tailscale links, detected from each host's NICs.
    #[command(visible_alias = "topo")]
    Topology {
        #[arg(long)]
        json: bool,
        /// Skip the ping / TCP check of each link.
        #[arg(long)]
        no_verify: bool,
    },
}

struct Ctx {
    paths: Paths,
    cfg: Config,
}

impl Ctx {
    fn load() -> Result<Ctx> {
        let paths = Paths::from_env();
        let cfg = Config::load(&paths)?;
        if let Some(w) = &cfg.skipped {
            eprintln!("{} {w}; using the defaults", yellow("warning:"));
        }
        Ok(Ctx { paths, cfg })
    }

    fn catalog(&self) -> Catalog {
        lmdeck::peerdeck::PeerDecks::fresh(&self.paths, lmdeck::peerdeck::TTL);
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
    let mut ctx = Ctx::load()?;
    // The first-run harness prompt runs once, before anything that serves or
    // shows models; it never touches non-interactive runs.
    if matches!(
        cli.cmd,
        None | Some(Cmd::Tui(_))
            | Some(Cmd::Serve { .. })
            | Some(Cmd::Use(_))
            | Some(Cmd::Switch(_))
            | Some(Cmd::List(_))
            | Some(Cmd::Init { .. })
    ) {
        first_run(&mut ctx);
    }
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
        Some(Cmd::Theme(c)) => theme_cmd(&ctx, c),
        Some(Cmd::Serve { host, port, token }) => {
            let token = token.filter(|t| !t.is_empty()).or_else(|| {
                ctx.cfg.serve.token_env.as_deref().and_then(|n| std::env::var(n).ok()).filter(|t| !t.is_empty())
            });
            lmdeck::web::run(Deck::new(ctx.paths, ctx.cfg), lmdeck::web::WebOptions { host, port, token })
        }
        Some(Cmd::List(a)) => list(&ctx, &a),
        Some(Cmd::Use(a)) => use_recipe(&ctx, &a),
        Some(Cmd::Switch(a)) => use_recipe(&ctx, &UseArgs { only: true, ..a }),
        Some(Cmd::Stop { target, all, on }) => stop(&ctx, target.as_deref(), all, on.as_deref()),
        Some(Cmd::Add { repo, name, git_ref, force }) => add_wrapper(&ctx, &repo, name, git_ref, force),
        Some(Cmd::Pull { target, dry_run, latest }) => pull(&ctx, &target, dry_run, latest),
        Some(Cmd::Browse { query, tags, sort, refresh, offline, fits, json }) => {
            browse(&ctx, query.as_deref().unwrap_or(""), &tags, &sort, refresh, offline, fits, json)
        }
        Some(Cmd::Outdated { targets, fetch, json }) => outdated(&ctx, &targets, fetch, json),
        Some(Cmd::Logs { target, lines, on, node, follow }) => {
            logs(&ctx, &target, lines, on.as_deref(), node.as_deref(), follow)
        }
        Some(Cmd::Weights { refresh, json }) => weights_cmd(&ctx, refresh, json),
        Some(Cmd::Recipe(c)) => recipe_cmd(&ctx, c),
        Some(Cmd::Source(c)) => source_cmd(ctx, c),
        Some(Cmd::Host(c)) => host_cmd(&ctx, c),
        Some(Cmd::Doctor { json }) => doctor(&ctx, json),
        Some(Cmd::Engine { cmd }) => engine_cmd(&ctx, cmd),
        Some(Cmd::Harness { cmd }) => harness_cmd(&ctx, cmd),
        Some(Cmd::Init { models_dir, force }) => init(ctx, models_dir, force),
        Some(Cmd::Setup) => {
            if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
                anyhow::bail!("`lmdeck setup` asks questions; run it in a terminal");
            }
            setup_wizard(&mut ctx);
            Ok(())
        }
    }
}

// ───────────────────────────── harnesses ─────────────────────────────

/// The once-only first-run prompts: the setup wizard (machine, cluster,
/// sources, usage statistics) on first use, then which agent harnesses lmdeck
/// should point at served models. Both save their answers to config.toml.
fn first_run(ctx: &mut Ctx) {
    if lmdeck::setup::should_run(&ctx.cfg) {
        setup_wizard(ctx);
    }
    if !lmdeck::harness::should_ask(&ctx.cfg) {
        return;
    }
    if let Some(choice) = lmdeck::harness::wizard(&ctx.cfg) {
        let mut h = ctx.cfg.harness.clone();
        h.sync = choice.harnesses;
        h.hosts = choice.hosts;
        h.asked = true;
        match Config::save_harness(&ctx.paths, &h) {
            Ok(file) => {
                ctx.cfg.harness = h;
                println!("{} {}", green("✓"), dim(&format!("saved to {}", file.display())));
            }
            Err(e) => eprintln!("{} {e:#}", yellow("warning:")),
        }
    }
}

/// The setup wizard, then the sources it picked are fetched and saved.
fn setup_wizard(ctx: &mut Ctx) {
    {
        match lmdeck::setup::wizard(&ctx.paths, &ctx.cfg) {
            Ok(outcome) => {
                // The wizard saved `[setup] done = true`; keep the in-memory copy
                // in step so a full config save below doesn't drop it.
                ctx.cfg.setup.done = true;
                ctx.cfg.stats.record = outcome.stats_record;
                if let Some(dir) = &outcome.models_dir {
                    ctx.cfg.models_dir = dir.clone();
                }
                for (name, spec) in &outcome.sources {
                    if ctx.cfg.sources.iter().any(|s| &s.name == name) {
                        continue;
                    }
                    let Ok(parsed) = lmdeck::sources::SourceSpec::parse(spec) else { continue };
                    if let Err(e) = fetch_source(ctx, name, &parsed) {
                        eprintln!("{} source {name}: {e:#}", yellow("!"));
                    }
                    ctx.cfg.sources.push(SourceEntry { name: name.clone(), spec: spec.clone() });
                }
                if !outcome.sources.is_empty() || outcome.models_dir.is_some() {
                    if let Err(e) = ctx.cfg.save(&ctx.paths) {
                        eprintln!("{} {e:#}", yellow("warning:"));
                    }
                }
            }
            Err(e) => eprintln!("{} setup: {e:#}", yellow("warning:")),
        }
    }
}

fn harness_cmd(ctx: &Ctx, c: Option<HarnessCmd>) -> Result<()> {
    let home = lmdeck::paths::home_dir();
    let known_list = lmdeck::harness::KNOWN.iter().map(|h| h.id).collect::<Vec<_>>().join(", ");
    let hosts_line = |cfg: &Config| -> String {
        match (&cfg.harness.hosts, configured_hosts(ctx)) {
            (Some(h), _) if h.is_empty() => "none (local models only)".to_string(),
            (Some(h), _) => h.join(", "),
            (None, known) if known.is_empty() => "no hosts configured".to_string(),
            (None, known) => format!("all configured ({})", known.join(", ")),
        }
    };
    let list_view = |ctx: &Ctx| -> Result<()> {
        let enabled = lmdeck::harness::enabled(&ctx.cfg);
        let mut t = Table::new(&["harness", "state", "config"]);
        for h in lmdeck::harness::KNOWN {
            let path = lmdeck::harness::config_path(h.id, &home).unwrap_or_default();
            let state = if enabled.contains(&h.id) {
                if lmdeck::harness::detected(h.id, &home) {
                    green("synced")
                } else {
                    dim("not installed")
                }
            } else {
                dim("off")
            };
            t.row(vec![bold(h.label), state, dim(&path.display().to_string())]);
        }
        println!("{}", t.render());
        println!("{} {}", dim("hosts:"), hosts_line(&ctx.cfg));
        println!(
            "{} {}",
            dim("opencode-export:"),
            if ctx.cfg.harness.opencode_export { green("on") } else { dim("off") }
        );
        println!("{} {}", dim("hub:"), if ctx.cfg.harness.hub { green("on") } else { dim("off") });
        if let Some(p) = lmdeck::hub::Pushed::load(&ctx.paths).filter(|_| !ctx.cfg.harness.hub) {
            let age = human_duration(now_secs().saturating_sub(p.at));
            println!("{} {} model(s) from {} ({age} ago)", dim("pushed here:"), p.lanes.len(), p.from);
        }
        if !ctx.cfg.harness.asked {
            println!("{}", dim("the first-run prompt hasn't run yet; everything detected gets synced."));
        }
        Ok(())
    };
    match c {
        Some(HarnessCmd::List) => return list_view(ctx),
        Some(HarnessCmd::Sync) => {
            // On the hub this also pushes to every host.
            let report = lmdeck::hub::Hub::new(&ctx.paths, &ctx.cfg).sync();
            if report.synced.is_empty() {
                println!("{}", dim("harness sync is off (LMDECK_NO_SYNC, or none enabled)"));
            }
            for h in &report.took {
                println!("{} pushed", dim(&format!("· {h}")));
            }
            for m in &report.missed {
                println!("{} {m}", dim("· push failed:"));
            }
            for s in report.synced {
                let state = if !s.installed {
                    "not installed"
                } else if let Some(e) = &s.error {
                    e
                } else if s.changed {
                    "updated"
                } else {
                    "unchanged"
                };
                println!("{} {state}", dim(&format!("· {}", s.id)));
            }
        }
        Some(HarnessCmd::Enable { name }) => {
            let h = lmdeck::harness::known(&name)
                .ok_or_else(|| anyhow!("unknown harness '{name}' (known: {known_list})"))?;
            let mut hc = ctx.cfg.harness.clone();
            hc.asked = true;
            if !hc.sync.iter().any(|s| s == h.id) {
                hc.sync.push(h.id.to_string());
            }
            Config::save_harness(&ctx.paths, &hc)?;
            let path = lmdeck::harness::config_path(h.id, &home).unwrap_or_default();
            println!("{} {} — written on the next serve ({})", green("✓"), h.label, dim(&path.display().to_string()));
        }
        Some(HarnessCmd::Disable { name }) => {
            let h = lmdeck::harness::known(&name)
                .ok_or_else(|| anyhow!("unknown harness '{name}' (known: {known_list})"))?;
            let mut hc = ctx.cfg.harness.clone();
            hc.asked = true;
            hc.sync.retain(|s| s.as_str() != h.id);
            Config::save_harness(&ctx.paths, &hc)?;
        }
        Some(HarnessCmd::Hosts { hosts }) => {
            let known = configured_hosts(ctx);
            let set: Option<Vec<String>> = match hosts.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                [] => {
                    println!("{} {}", dim("hosts:"), hosts_line(&ctx.cfg));
                    return Ok(());
                }
                ["all"] => None,
                ["none"] => Some(vec![]),
                _ => {
                    let unknown: Vec<&String> = hosts.iter().filter(|h| !known.contains(h)).collect();
                    if !unknown.is_empty() {
                        bail!(
                            "no host{} {} in hosts.toml (known: {})",
                            if unknown.len() > 1 { "s" } else { "" },
                            unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                            known.join(", ")
                        );
                    }
                    Some(hosts.clone())
                }
            };
            let mut hc = ctx.cfg.harness.clone();
            hc.hosts = set.clone();
            hc.asked = true;
            Config::save_harness(&ctx.paths, &hc)?;
            println!(
                "{} sync hosts: {}",
                green("✓"),
                match &set {
                    None => "all configured hosts".to_string(),
                    Some(h) if h.is_empty() => "none (local models only)".to_string(),
                    Some(h) => h.join(", "),
                }
            );
        }
        Some(HarnessCmd::Export { on }) => {
            let on = match on.to_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => true,
                "off" | "false" | "no" | "0" => false,
                other => bail!("opencode-export is 'on' or 'off' (got '{other}')"),
            };
            let mut hc = ctx.cfg.harness.clone();
            hc.opencode_export = on;
            Config::save_harness(&ctx.paths, &hc)?;
            let where_ = home.join(".local/share/opencode-export");
            println!(
                "{} opencode-export {} (mirrors opencode.json to {})",
                green("✓"),
                if on { "on" } else { "off" },
                dim(&where_.display().to_string())
            );
        }
        Some(HarnessCmd::Hub { on }) => {
            let on = match on.to_lowercase().as_str() {
                "on" | "true" | "yes" | "1" => true,
                "off" | "false" | "no" | "0" => false,
                other => bail!("hub is 'on' or 'off' (got '{other}')"),
            };
            let mut hc = ctx.cfg.harness.clone();
            hc.hub = on;
            Config::save_harness(&ctx.paths, &hc)?;
            if !on {
                println!("{} hub off (hosts keep the lanes it pushed last until another push)", green("✓"));
                return Ok(());
            }
            let every = lmdeck::hub::Poller::interval()
                .map_or("never (LMDECK_HARNESS_SYNC_SECS=0)".to_string(), |d| format!("every {} s", d.as_secs()));
            println!(
                "{} hub on: each start or stop here pushes every host's running models to every host in hosts.toml; \
                 `lmdeck serve` here also looks {every}",
                green("✓")
            );
            if configured_hosts(ctx).is_empty() {
                println!("{}", dim("no other hosts in hosts.toml yet (`lmdeck host add`)"));
            }
        }
        None => list_view(ctx)?,
    }
    Ok(())
}

/// The non-local host names in hosts.toml.
fn configured_hosts(ctx: &Ctx) -> Vec<String> {
    let node = ctx.cfg.node_name();
    lmdeck::hosts::HostsFile::load(&ctx.paths)
        .map(|f| f.hosts.iter().filter(|e| !lmdeck::remote::is_local(&e.name, &node)).map(|e| e.name.clone()).collect())
        .unwrap_or_default()
}
// ───────────────────────────── hosts ─────────────────────────────

fn host_row(h: &HostView) -> Vec<String> {
    let t = h.telemetry.as_ref();
    let opt = |v: Option<String>| v.unwrap_or_else(|| dim("-"));
    let status = if h.online {
        green(&format!("● up {}ms", h.latency_ms.unwrap_or(0)))
    } else {
        red(&format!("○ {}", h.error.as_deref().unwrap_or("offline")))
    };
    vec![
        bold(&h.name),
        h.url.clone().unwrap_or_else(|| dim("this machine")),
        status,
        opt(t.map(|t| t.platform.clone()).filter(|p| !p.is_empty())),
        opt(t.and_then(|t| Some(format!("{:.0}/{:.0}G", t.mem_used_gb?, t.mem_total_gb)))),
        opt(t.and_then(|t| t.temp()).map(|c| format!("{c:.0}°C"))),
        opt(t.and_then(|t| t.power()).map(|w| format!("{w:.0}W"))),
        opt(t.and_then(|t| t.gpu_util()).map(|u| format!("{u:.0}%"))),
        opt(t
            .map(|t| t.models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(","))
            .filter(|s| !s.is_empty())),
    ]
}

fn host_cmd(ctx: &Ctx, c: HostCmd) -> Result<()> {
    let mut file = HostsFile::load(&ctx.paths)?;
    match c {
        HostCmd::Add { name, url, token, token_env, link_addr, no_check } => {
            let mut e = file.add(&name, &url, token)?;
            if token_env.is_some() || link_addr.is_some() {
                if let Some(slot) = file.hosts.iter_mut().find(|h| h.name == e.name) {
                    if let Some(v) = token_env {
                        slot.token_env = Some(v);
                    }
                    if let Some(a) = link_addr {
                        if a.parse::<std::net::IpAddr>().is_err() {
                            bail!("--link-addr must be an IP address");
                        }
                        slot.link_addr = Some(a);
                    }
                    e = slot.clone();
                }
            }
            let path = file.save(&ctx.paths)?;
            println!("{} added host {} {} {}", green("✓"), bold(&e.name), e.url, dim(&format!("({})", path.display())));
            if !no_check {
                match hosts::fetch(&e, u64::MAX, hosts::FETCH_TIMEOUT) {
                    Ok((r, ms)) => {
                        let t = r.telemetry.unwrap_or_default();
                        println!("  {} reachable in {ms} ms: {} · {}", green("●"), t.hostname, t.platform);
                    }
                    Err(err) => println!(
                        "  {} not reachable yet: {err}\n  {}",
                        yellow("○"),
                        dim("on that machine run `lmdeck serve --host <its CX7 or LAN IP> --token <token>`")
                    ),
                }
            }
            Ok(())
        }
        HostCmd::Topology { json, no_verify } => {
            let views = hosts::snapshot_once(&ctx.paths, true);
            let t = lmdeck::topology::build(&ctx.paths, &ctx.cfg, &views, !no_verify);
            if json {
                println!("{}", serde_json::to_string_pretty(&t)?);
            } else {
                print!("{}", lmdeck::topology::render_text(&t));
            }
            Ok(())
        }
        HostCmd::Import { mut skip } => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
            let incoming: HostsFile = toml::from_str(&text).context("parsing the hosts on stdin")?;
            skip.push(ctx.cfg.node_name());
            let (added, updated) = file.merge(&incoming.hosts, &skip);
            if added.is_empty() && updated.is_empty() {
                println!("up to date");
                return Ok(());
            }
            file.save(&ctx.paths)?;
            let mut said = vec![];
            if !added.is_empty() {
                said.push(format!("added {}", added.join(", ")));
            }
            if !updated.is_empty() {
                said.push(format!("updated {}", updated.join(", ")));
            }
            println!("{}", said.join("; "));
            Ok(())
        }
        HostCmd::Sync { dry_run } => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            let own = lmdeck::mesh::own_entry(&ctx.paths, &home, &ctx.cfg.node_name());
            if own.is_none() {
                println!(
                    "{}",
                    yellow(
                        "! this machine has no `lmdeck serve` service on a network address; the others won't list it"
                    )
                );
            }
            let rep = lmdeck::mesh::sync(&ctx.paths, &mut lmdeck::ssh::System::default(), own, dry_run)?;
            for l in &rep.lines {
                println!("  {} {l}", green("✓"));
            }
            for l in &rep.failed {
                println!("  {} {l}", red("✗"));
            }
            if !rep.failed.is_empty() {
                bail!("{} host(s) not updated", rep.failed.len());
            }
            Ok(())
        }
        HostCmd::Remove { name } => {
            if !file.remove(&name) {
                bail!("no host named '{name}'");
            }
            file.save(&ctx.paths)?;
            println!("{} removed host {}", green("✓"), bold(&name));
            Ok(())
        }
        HostCmd::List { json } => {
            let views = hosts::snapshot_once(&ctx.paths, true);
            let demo = views.iter().any(|h| h.telemetry.as_ref().is_some_and(|t| t.demo));
            if json {
                let mut v = serde_json::to_value(&views)?;
                for h in v.as_array_mut().into_iter().flatten() {
                    h.as_object_mut().map(|o| o.remove("history"));
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            if demo {
                println!("{}", yellow("DEMO DATA (LMDECK_DEMO is set): these numbers are synthetic"));
            }
            let mut t = Table::new(&["host", "url", "status", "platform", "mem", "temp", "power", "gpu", "models"])
                .right(&[4, 5, 6, 7])
                .max_width(8, 40);
            for h in &views {
                t.row(host_row(h));
            }
            print!("{}", t.render());
            if file.hosts.is_empty() {
                println!(
                    "{}",
                    dim("no other hosts; add one with `lmdeck host add spark1 http://spark1:7979 --token …`")
                );
            }
            Ok(())
        }
    }
}

// ───────────────────────────── tui ─────────────────────────────

fn tui(ctx: Ctx, a: TuiArgs) -> Result<()> {
    let nerd = a.nerd || std::env::var("LMDECK_NERD_FONT").is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    let opts = lmdeck::tui::Options {
        theme: a.theme,
        nerd,
        all: a.all,
        picker: a.picker,
        select: a.select,
        host: a.host,
        run_on: a.run_on,
    };
    let deck = Deck::new(ctx.paths, ctx.cfg);
    if a.render_html.is_some() || a.render_text {
        let tab = match a.tab.as_str() {
            "deck" => lmdeck::tui::Tab::Deck,
            "hosts" => lmdeck::tui::Tab::Hosts,
            "topology" | "topo" => lmdeck::tui::Tab::Topology,
            "machine" | "doctor" => lmdeck::tui::Tab::Machine,
            "sources" => lmdeck::tui::Tab::Sources,
            "stats" => lmdeck::tui::Tab::Stats,
            other => bail!("unknown tab '{other}'"),
        };
        let buf = lmdeck::tui::render_snapshot(&deck, &opts, a.width, a.height, tab)?;
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

// ───────────────────────────── theme ─────────────────────────────

/// A coloured block (truecolor), or nothing when colour is off.
fn swatch_block(c: u32) -> String {
    if !ui::color_enabled() {
        return String::new();
    }
    format!("\x1b[48;2;{};{};{}m  \x1b[0m", c >> 16, (c >> 8) & 0xff, c & 0xff)
}

fn theme_cmd(ctx: &Ctx, c: ThemeCmd) -> Result<()> {
    use lmdeck::theme;
    let env = std::env::var("LMDECK_THEME").ok().filter(|v| !v.trim().is_empty());
    let current = theme::resolve(None, ctx.cfg.theme.as_deref())?;
    let saved = |p: &theme::Palette| -> Result<()> {
        let file = Config::save_theme(&ctx.paths, Some(p.slug))?;
        println!("{} theme {} ({}) saved to {}", green("✓"), bold(p.name), p.slug, file.display());
        if let Some(e) = &env {
            println!("{}", yellow(&format!("note: LMDECK_THEME={e} is set and overrides this for `lmdeck tui`")));
        }
        Ok(())
    };
    match c {
        ThemeCmd::List => {
            let mut t = Table::new(&["", "theme", "name", "", "colours"]);
            for p in theme::all() {
                let mark = if p.slug == current.slug { green("●") } else { String::new() };
                let sw: String = p.swatch().iter().map(|&c| swatch_block(c)).collect();
                t.row(vec![
                    mark,
                    if p.slug == current.slug { bold(p.slug) } else { p.slug.to_string() },
                    p.name.to_string(),
                    dim(if p.light { "light" } else { "dark" }),
                    sw,
                ]);
            }
            print!("{}", t.render());
            println!("{}", dim("lmdeck theme set <name> · themes from Omarchy (MIT) · https://omarchy.org"));
        }
        ThemeCmd::Set { name } => saved(theme::by_name(&name).ok_or_else(|| theme::unknown(&name))?)?,
        ThemeCmd::Show { name } => {
            let p = match name.as_deref() {
                Some(n) => theme::by_name(n).ok_or_else(|| theme::unknown(n))?,
                None => current,
            };
            println!("{} ({}, {})", bold(p.name), p.slug, if p.light { "light" } else { "dark" });
            println!("{}", dim(&format!("source: {}", p.source())));
            if name.is_none() {
                let why = match &ctx.cfg.theme {
                    Some(t) => format!("set in {} (theme = \"{t}\")", ctx.paths.config_file().display()),
                    None => "default (no theme in config.toml)".to_string(),
                };
                println!("{}", dim(&why));
                if let Some(e) = &env {
                    println!("{}", yellow(&format!("LMDECK_THEME={e} overrides it for `lmdeck tui`")));
                }
            }
            let roles = [
                ("background", p.bg),
                ("panel", p.panel),
                ("bar", p.bar_bg),
                ("selection", p.selected_bg),
                ("foreground", p.fg),
                ("muted", p.muted),
                ("dim", p.dim),
                ("accent", p.accent),
                ("border", p.border),
                ("ok", p.ok),
                ("warn", p.warn),
                ("error", p.err),
                ("info", p.info),
                ("magenta", p.magenta),
                ("blue", p.blue),
            ];
            for (role, c) in roles {
                println!("  {} {role:<11} #{c:06x}", swatch_block(c));
            }
        }
        ThemeCmd::SyncOmarchy => {
            let name = theme::omarchy_current(&home_dir())
                .ok_or_else(|| anyhow!("no Omarchy theme found (~/.local/state/omarchy/current/theme.name)"))?;
            let p = theme::by_name(&name)
                .ok_or_else(|| anyhow!("Omarchy is on '{name}', which lmdeck doesn't have (lmdeck theme list)"))?;
            saved(p)?;
        }
        ThemeCmd::Reset => {
            Config::save_theme(&ctx.paths, None)?;
            println!("{} theme reset to {}", green("✓"), theme::DEFAULT);
        }
    }
    Ok(())
}

// ───────────────────────────── list ─────────────────────────────

fn status_cell(s: &Status) -> String {
    match s {
        Status::Loaded => green("● loaded"),
        Status::Loading => yellow("◐ loading"),
        Status::External => cyan("● external"),
        // Not this recipe: some other server holds its port.
        Status::PortBusy => format!("{} {}", dim("○"), yellow("port taken")),
        Status::Stopped => dim("○ stopped"),
    }
}

fn status_tag(s: &Status) -> String {
    match s {
        Status::Loaded => green("loaded"),
        Status::External => cyan("external"),
        Status::Loading => yellow("loading"),
        Status::PortBusy => yellow("(port taken by another server)"),
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

/// Update status for the wrapped recipes among `entries`: stale cache
/// entries are re-checked first (bounded by a short timeout; never fails).
fn update_views(ctx: &Ctx, entries: &[&Found]) -> std::collections::BTreeMap<String, UpdateView> {
    let s = updates::Settings::from(&ctx.cfg);
    let recipes = || entries.iter().map(|f| &f.recipe);
    updates::refresh(&ctx.paths, &s, recipes(), false, s.timeout.min(updates::QUICK_TIMEOUT), false);
    updates::views(&ctx.paths, &s, recipes())
}

/// `⬆ update` / `⬆ newer` after a recipe name ("" otherwise).
fn update_tag(u: Option<&UpdateView>) -> String {
    match u.map(|u| u.state) {
        Some(updates::State::Behind) => yellow("⬆ update"),
        Some(updates::State::PinnedNewer) => cyan("⬆ newer"),
        _ => String::new(),
    }
}

fn update_json(u: Option<&UpdateView>) -> serde_json::Value {
    u.map(|u| json!(u)).unwrap_or(serde_json::Value::Null)
}

fn update_hint(ups: &std::collections::BTreeMap<String, UpdateView>) {
    let n = ups.values().filter(|u| u.available()).count();
    if n > 0 {
        println!(
            "{} {n} wrapped recipe(s) have upstream updates {}",
            cyan("⬆"),
            dim("· lmdeck outdated for details, lmdeck pull <name> to update")
        );
    }
}

fn deck_table(ctx: &Ctx, entries: &[&Found], loaded_only: bool, json_out: bool) -> Result<()> {
    deck_table_grouped(ctx, entries, loaded_only, json_out, true)
}

/// The recipe table. The human table groups recipes by model family
/// (`── Qwen ──` separators) unless `group` is off; JSON stays flat.
fn deck_table_grouped(ctx: &Ctx, entries: &[&Found], loaded_only: bool, json_out: bool, group: bool) -> Result<()> {
    let ups = update_views(ctx, entries);
    let mut cache: HashMap<u16, Probe> = HashMap::new();
    let mut t = Table::new(&["#", "", "name", "engine", "quant", "size", "dl", "port", "origin"]).right(&[0, 5, 7]);
    let mut rows = vec![];
    let mut display: Vec<(usize, usize, &'static str, Vec<String>)> = vec![];
    let node = ctx.cfg.node_name();
    let ports = deck::planned_ports(&ctx.catalog(), &ctx.paths, &ctx.cfg);
    let has_hosts = hosts::HostsFile::load(&ctx.paths).map(|f| !f.hosts.is_empty()).unwrap_or(false);
    let idx = has_hosts.then(|| lmdeck::weights::Index::fresh(&ctx.paths, &ctx.cfg, lmdeck::weights::INDEX_TTL));
    let live = has_hosts.then(|| hosts::read_live(&ctx.paths, Duration::from_secs(30))).flatten();
    let docker = ctx.cfg.engines.docker.clone().unwrap_or_else(|| "docker".into());
    for (i, f) in entries.iter().enumerate() {
        let r = &f.recipe;
        let port = deck::planned_port(&ports, f);
        let (st, running_on) = deck::seen_status(&ctx.paths, &docker, r, port, &mut cache, live.as_ref());
        let weights_on: Vec<String> = match &idx {
            Some(idx) => {
                let mut v: Vec<String> = idx.locate(r).into_iter().map(|l| l.host).collect();
                v.dedup();
                v
            }
            None => vec![],
        };
        let nodes: Vec<serde_json::Value> = if r.nodes.is_empty() {
            vec![]
        } else {
            lmdeck::cluster::views(&ctx.paths, r, &node)
                .into_iter()
                .map(|n| json!({"name": n.name, "role": n.role, "host": n.host, "served_as": n.served_as, "status": n.status}))
                .collect()
        };
        if loaded_only && !st.is_up() {
            continue;
        }
        let p = ctx.presence(r);
        rows.push(json!({
            "index": i + 1, "name": r.name, "origin": f.origin, "engine": r.engine.name.as_str(),
            "quant": r.quant_short(), "port": port, "status": status_word(&st), "family": r.family(),
            "downloaded": p.present(), "size_bytes": p.size, "model": r.model.repo.clone().or(r.model.path.clone()),
            "update": update_json(ups.get(&r.name)),
            "weights_on": weights_on, "nodes": nodes, "running_on": running_on,
            "other_platform": lmdeck::compat::other_platform(r, std::env::consts::OS, std::env::consts::ARCH),
        }));
        let glyph = match st {
            Status::Loaded | Status::External => green("●"),
            Status::Loading => yellow("◐"),
            Status::PortBusy | Status::Stopped => dim("○"),
        };

        // Recipes for another platform (MLX on a Spark) stay listed, greyed out.
        let elsewhere = lmdeck::compat::other_platform(r, std::env::consts::OS, std::env::consts::ARCH);
        let mut name = match &elsewhere {
            Some(why) => dim(&format!("{} ({why})", r.name)),
            None => bold(&r.name),
        };
        if st != Status::Stopped {
            name = format!("{name} {}", status_tag(&st));
        }
        if let Some(h) = &running_on {
            name = format!("{name} {}", dim(&format!("on {h}")));
        }
        let up = update_tag(ups.get(&r.name));
        if !up.is_empty() {
            name = format!("{name} {up}");
        }
        if !r.nodes.is_empty() {
            name = format!("{name} {}", dim(&format!("×{} nodes", r.nodes.len())));
        }
        if weights_on.iter().any(|h| h != hosts::LOCAL) {
            name = format!("{name} {}", dim(&format!("weights on: {}", weights_on.join(", "))));
        }
        display.push((
            i,
            r.family_rank(),
            r.family(),
            vec![
                (i + 1).to_string(),
                glyph,
                name,
                r.engine.name.as_str().into(),
                r.quant_short(),
                size_cell(r, &p),
                downloaded_cell(&p),
                port.to_string(),
                dim(&f.origin),
            ],
        ));
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    // Family-grouped table: rows keep their `#` (resolve order), the display
    // just sorts them under `── Family ──` separators.
    display.sort_by_key(|(i, rank, _, _)| (*rank, *i));
    let mut prev: Option<&'static str> = None;
    for (_, _, fam, cells) in &display {
        if group && prev != Some(fam) {
            prev = Some(fam);
            let mut sep = vec![String::new(); 9];
            sep[2] = cyan(&format!("── {fam}"));
            t.row(sep);
        }
        t.row(cells.clone());
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if t.is_empty() {
        println!("{}", dim(if loaded_only { "nothing is running" } else { "no recipes" }));
    } else {
        print!("{}", t.render());
        update_hint(&ups);
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
    deck_table_grouped(ctx, &entries, a.loaded, a.json, !a.flat)?;
    if !a.json {
        hints(ctx, &cat, a.all);
    }
    Ok(())
}

fn hints(ctx: &Ctx, cat: &Catalog, all: bool) {
    if !all && cat.deck().next().is_none() {
        println!(
            "{}",
            dim("deck is empty. Add recipes with `lmdeck recipe add <name|file|url>`; find more with `lmdeck recipe search`.")
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
    // Servers lmdeck didn't start here but that serve a deck recipe, and deck
    // recipes running on other hosts.
    let ports = deck::planned_ports(cat, &ctx.paths, &ctx.cfg);
    let mut cache = HashMap::new();
    let live = hosts::read_live(&ctx.paths, Duration::from_secs(30));
    let docker = ctx.cfg.engines.docker.clone().unwrap_or_else(|| "docker".into());
    for f in listed(cat) {
        if seen.contains(&f.recipe.name) || !f.recipe.nodes.is_empty() {
            continue;
        }
        seen.push(f.recipe.name.clone());
        let ext_port = deck::planned_port(&ports, f);
        let (st, on) = deck::seen_status(&ctx.paths, &docker, &f.recipe, ext_port, &mut cache, live.as_ref());
        if st.is_up() {
            let ep = deck::endpoint_on(&ctx.paths, on.as_deref(), ext_port);
            rows.push(json!({"name": f.recipe.name, "engine": f.recipe.engine.name.as_str(), "port": ext_port,
                "status": status_word(&st), "endpoint": ep, "running_on": on}));
            let name = match &on {
                Some(h) => format!("{} {}", bold(&f.recipe.name), dim(&format!("on {h}"))),
                None => format!("{} {}", bold(&f.recipe.name), dim("(not started by lmdeck)")),
            };
            t.row(vec![
                status_cell(&st),
                name,
                f.recipe.engine.name.as_str().into(),
                ext_port.to_string(),
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

/// Offers to install llama.cpp when a recipe needs it and the machine has no
/// `llama-server`. `--yes` accepts without a terminal; `--no-install` leaves
/// the failure to `can_launch`, whose message names the command.
fn engine_fix(r: &Recipe) -> Option<String> {
    match r.engine.name {
        Engine::LlamaCpp => Some("lmdeck engine install llama.cpp".into()),
        Engine::Vllm | Engine::Sglang | Engine::LmDeploy => {
            r.engine.image.as_ref().map(|image| format!("docker pull {image}"))
        }
        _ => None,
    }
}

fn use_preflight(ctx: &Ctx, r: &Recipe, a: &UseArgs, inv: &mut Inventory) -> Result<()> {
    if a.dry_run || a.no_install || inv.can_launch(r).is_ok() {
        return Ok(());
    }
    if r.engine.name != Engine::LlamaCpp || lmdeck::engine_install::current_asset().is_err() {
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal();
    if !a.yes && !interactive {
        return Ok(());
    }
    if !a.yes {
        println!();
        println!("{} {} needs llama.cpp, which is not installed.", yellow("!"), bold(&r.name));
        if !lmdeck::setup::ask_yes(&mut lmdeck::setup::Terminal, "  install a managed llama.cpp build (~19 MB)?", true)
        {
            return Ok(());
        }
    }
    let mut say = |line: &str| println!("  {line}");
    lmdeck::engine_install::install_llama_cpp(&ctx.paths, false, &mut say)?;
    *inv = Inventory::detect(&ctx.cfg);
    Ok(())
}

/// Offers to `docker pull` a recipe's image when it is not local. Docker would
/// pull on `run` anyway; asking first keeps multi-GB downloads explicit.
fn ensure_docker_image(ctx: &Ctx, r: &Recipe, a: &UseArgs) -> Result<()> {
    let Some(image) = r.engine.image.as_deref() else { return Ok(()) };
    if a.dry_run || a.no_install {
        return Ok(());
    }
    let docker = ctx.docker();
    if which(&docker).is_none() || docker_image_present(&docker, image) {
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal();
    if !a.yes && !interactive {
        return Ok(());
    }
    if !a.yes {
        println!();
        println!("{} image {} is not local.", yellow("!"), bold(image));
        if !lmdeck::setup::ask_yes(&mut lmdeck::setup::Terminal, "  pull it now?", true) {
            return Ok(());
        }
    }
    println!("{} pulling {}", cyan("↓"), bold(image));
    let status =
        Command::new(&docker).args(["pull", image]).status().with_context(|| format!("running {docker} pull"))?;
    if !status.success() {
        bail!("docker pull {image} failed");
    }
    Ok(())
}

fn docker_image_present(docker: &str, image: &str) -> bool {
    Command::new(docker)
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn use_recipe(ctx: &Ctx, a: &UseArgs) -> Result<()> {
    let cat = ctx.catalog();
    let found = resolve(&cat, &a.target)?;
    let r = &found.recipe;
    if !r.nodes.is_empty() {
        return use_cluster(ctx, r, a);
    }
    if r.nodes() > 1 {
        bail!("{} spans {} nodes; give it [[nodes]] entries to launch it across hosts", r.name, r.nodes());
    }
    // `--on local` (or this machine's name) runs it here, as a start without `--on` did
    // before it went where the weights are.
    let node = ctx.cfg.node_name();
    if let Some(on) = a.on.as_deref().filter(|h| !lmdeck::remote::is_local(h, &node)) {
        return use_on(ctx, r, on, a, None);
    }
    let pick = lmdeck::placement::Pick::of(&ctx.paths, &ctx.cfg, r);
    if let Some(p) = pick.as_ref().filter(|p| a.on.is_none() && p.host != hosts::LOCAL) {
        return use_on(ctx, r, &p.host, a, Some(&p.why));
    }
    // Another host's deck recipe runs there, unless its weights are here.
    if let Some(owner) = lmdeck::peerdeck::host_of(&found.origin).filter(|_| a.on.is_none()) {
        if !pick.as_ref().is_some_and(|p| p.why == lmdeck::placement::WEIGHTS_HERE) {
            let why = format!("it's in {owner}'s deck");
            return use_on(ctx, r, owner, a, Some(&why));
        }
    }
    let here = pick.map(|p| match a.on {
        Some(_) => format!("here, as --on says ({})", p.why),
        None => format!("here ({})", p.why),
    });
    let hw = Hardware::detect();
    let mut inv = Inventory::detect(&ctx.cfg);
    use_preflight(ctx, r, a, &mut inv)?;
    let p = ctx.presence(r);
    let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size))
        .with_fleet(r, || lmdeck::hosts::fleet(&ctx.paths, &hw.platform));
    let port = deck::planned_for(&cat, &ctx.paths, &ctx.cfg, r);
    let fabric = topology::fabric_for(&ctx.paths, &ctx.cfg, r);
    let peers = lmdeck::pick::peers(&ctx.paths, &ctx.cfg, r, hosts::LOCAL)?;
    let plan = engine::plan(
        r,
        &PlanCtx::new(&ctx.cfg, p.local)
            .with_port(Some(port))
            .with_fabric(fabric.as_ref().map(|f| f.kind.as_str().to_string()))
            .with_vars(lmdeck::vars::peer_vars(&peers.hosts)),
    )?;

    if a.dry_run {
        println!(
            "{} {} {}",
            bold("plan for"),
            cyan(&format!("{}/{}", found.origin, r.name)),
            dim(&format!("({})", r.engine.name))
        );
        if let Some(h) = &here {
            println!("  where: {h}");
        }
        print_report(&rep);
        if inv.can_launch(r).is_err() {
            if let Some(fix) = engine_fix(r) {
                println!("  fix:  {fix}");
            }
        }
        for pre in &plan.pre {
            println!("  pre:  {}", shlex::try_join(pre.iter().map(String::as_str)).unwrap_or_default());
        }
        if let (Some(src), Some(co)) = (&r.source, &plan.checkout) {
            for line in checkout::describe_sync(src, co, false) {
                println!("  git:  {line}");
            }
        }
        if let Some(cwd) = &plan.cwd {
            println!("  cwd:  {}", cwd.display());
        }
        for (k, v) in &plan.env {
            // Values only for wrapped recipes, whose env is how settings reach the scripts.
            if r.engine.name == Engine::Custom && !k.contains("TOKEN") && !k.contains("KEY") {
                println!("  env:  {k}={v}");
            } else {
                println!("  env:  {k}");
            }
        }
        if let Some(f) = &fabric {
            let node = ctx.cfg.node_name();
            let show = |h: &str| if h == hosts::LOCAL { node.clone() } else { h.to_string() };
            let order = if f.order.is_empty() { "set by LMDECK_FABRIC".to_string() } else { f.describe(show) };
            println!("  fabric: {} ({order})", f.kind.as_str());
            if f.kind == fabric::ShapeKind::Ring && f.order.len() > 1 {
                let workers: Vec<String> = f.order[1..].iter().map(|h| show(h)).collect();
                println!(
                    "  ranks: workers in cabled order: {} (Mia's scripts/local.sh: WORKER, WORKER2, WORKER3, ...)",
                    workers.join(", ")
                );
            }
        }
        if !peers.hosts.is_empty() {
            let list: Vec<String> =
                peers.hosts.iter().enumerate().map(|(i, (h, a))| format!("{}. {h} {a}", i + 1)).collect();
            println!("  peers: {}", list.join(", "));
        }
        for w in &peers.warnings {
            println!("  warning: {w}");
        }
        println!("  run:  {}", plan.command_line());
        if plan.detached {
            let how = match &plan.container {
                Some(c) => format!("container {c}"),
                None => format!("port {}", plan.port),
            };
            println!("  mode: detached (exits 0 once up; liveness from {how})");
        }
        if let Some(s) = &plan.stop {
            println!("  stop: {}", shlex::try_join(s.iter().map(String::as_str)).unwrap_or_else(|_| s.join(" ")));
        }
        if let Some(l) = &plan.logs {
            println!("  logs: {}", shlex::try_join(l.iter().map(String::as_str)).unwrap_or_else(|_| l.join(" ")));
        }
        println!("  url:  http://localhost:{}/v1", plan.port);
        return Ok(());
    }

    if let Some(h) = &here {
        println!("  {}", dim(&format!("where: {h}")));
    }
    let docker = ctx.cfg.engines.docker.clone().unwrap_or_else(|| "docker".into());
    match deck::local_status(&ctx.paths, Some(&docker), r, plan.port, &mut HashMap::new()) {
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
    ensure_docker_image(ctx, r, a)?;
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

    if let (Some(src), Some(co)) = (&r.source, &plan.checkout) {
        if !co.join(".git").exists() {
            println!("{} cloning {} {}", cyan("↓"), src.repo, dim(&format!("→ {}", co.display())));
        }
        if let Some(o) = checkout::ensure(&ctx.paths, r)? {
            println!("  {}", dim(&format!("checkout {}", o.describe())));
        }
    }
    println!("{} starting {} {}", cyan("▶"), bold(&r.name), dim(&format!("({}, port {})", r.engine.name, plan.port)));
    println!("  {}", dim(&plan.command_line()));
    let rec = runner::start(&ctx.paths, &r.name, r.engine.name.as_str(), &plan)?;
    println!("  {}", dim(&lmdeck::harness::sync_note(&ctx.paths, &ctx.cfg)));
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

fn stop(ctx: &Ctx, target: Option<&str>, all: bool, on: Option<&str>) -> Result<()> {
    let node = ctx.cfg.node_name();
    let deck = Deck::new(ctx.paths.clone(), ctx.cfg.clone());
    if let Some(host) = on.filter(|h| !lmdeck::remote::is_local(h, &node)) {
        let t = target.ok_or_else(|| anyhow!("say which recipe to stop on {host}"))?;
        let c = lmdeck::remote::Client::named(&ctx.paths, host)?;
        let m = c.action("stop", t, false)?;
        lmdeck::placement::Record::remove(&ctx.paths, t);
        println!("{} {host}: {m}", dim("○"));
        println!("  {}", dim(&lmdeck::harness::sync_note(&ctx.paths, &ctx.cfg)));
        return Ok(());
    }
    if let (Some(t), false) = (target, all) {
        if let Ok(f) = resolve(&ctx.catalog(), t) {
            if !f.recipe.nodes.is_empty() {
                println!("{} {}", dim("○"), deck.stop(t)?);
                return Ok(());
            }
            // Started elsewhere with --on: stop it there.
            if let Some(p) = lmdeck::placement::Record::load(&ctx.paths, &f.recipe.name) {
                if p.run_host != hosts::LOCAL && RunRecord::load(&ctx.paths, &f.recipe.name).is_none() {
                    let c = lmdeck::remote::Client::named(&ctx.paths, &p.run_host)?;
                    let m = c.action("stop", &p.served_as, false)?;
                    lmdeck::placement::Record::remove(&ctx.paths, &f.recipe.name);
                    println!("{} {}: {m}", dim("○"), p.run_host);
                    println!("  {}", dim(&lmdeck::harness::sync_note(&ctx.paths, &ctx.cfg)));
                    return Ok(());
                }
            }
        }
    }
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
        } else if all {
            println!("{} {n} was not started by lmdeck", dim("○"));
        } else {
            // Not started here by lmdeck: a server it recognises as this recipe (adopted)
            // or a run on another host is stopped where it is.
            match deck.stop(&n) {
                Ok(m) => println!("{} {}", dim("○"), m.lines().next().unwrap_or_default()),
                Err(e) => println!("{} {e:#}", dim("○")),
            }
        }
    }
    println!("  {}", dim(&lmdeck::harness::sync_note(&ctx.paths, &ctx.cfg)));
    Ok(())
}

fn logs(ctx: &Ctx, target: &str, lines: usize, on: Option<&str>, node: Option<&str>, follow: bool) -> Result<()> {
    let cat = ctx.catalog();
    let me = ctx.cfg.node_name();
    let found = resolve(&cat, target).ok();
    let mut on = on.map(str::to_string);
    let mut name = found.map(|f| f.recipe.name.clone()).unwrap_or_else(|| target.to_string());
    if let Some(f) = found.filter(|f| !f.recipe.nodes.is_empty()) {
        let r = &f.recipe;
        let n = match node {
            Some(want) => r.nodes.iter().find(|n| n.name == want).ok_or_else(|| {
                anyhow!(
                    "{} has no node '{want}' (nodes: {})",
                    r.name,
                    r.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>().join(", ")
                )
            })?,
            None => r.nodes.iter().find(|n| n.role == lmdeck::recipe::NodeRole::Head).expect("validated"),
        };
        name = lmdeck::cluster::node_name(r, n);
        let rec = lmdeck::cluster::Record::load(&ctx.paths, &r.name);
        let host = rec.as_ref().and_then(|x| x.host(&n.name)).unwrap_or(&n.host);
        if host == lmdeck::recipe::AUTO {
            bail!("{} hasn't started: node {} gets its machine when it starts (host = \"auto\")", r.name, n.name);
        }
        on = Some(host.to_string());
    } else if on.is_none() {
        // A run placed on another host (the weights home) logs there.
        if let Some((host, served_as)) = Deck::new(ctx.paths.clone(), ctx.cfg.clone()).remote_log(&name) {
            on = Some(host);
            name = served_as;
        }
    }
    if let Some(host) = on.filter(|h| !lmdeck::remote::is_local(h, &me)) {
        let c = lmdeck::remote::Client::named(&ctx.paths, &host)?;
        eprintln!("{}", dim(&format!("── logs from {host} ({name}) ──")));
        if !follow {
            for l in c.logs(&name, lines)? {
                println!("{l}");
            }
            return Ok(());
        }
        return follow_remote(&c, &name, lines);
    }
    let deck = Deck::new(ctx.paths.clone(), ctx.cfg.clone());
    let path = deck.log_for(&name);
    if !path.exists() {
        bail!("no log for {name} at {}", path.display());
    }
    if !follow {
        println!("{}", runner::tail(&path, lines));
        return Ok(());
    }
    let mut at: Option<(PathBuf, u64)> = None;
    loop {
        let path = deck.log_for(&name);
        let offset = at.as_ref().filter(|(p, _)| *p == path).map(|(_, o)| *o);
        let c = deck::log_since(&path, offset, lines);
        if c.reset || (offset.is_none() && at.is_some()) {
            eprintln!("{}", dim(&format!("── {} started over ──", path.display())));
        }
        print_lines(&c.lines)?;
        at = Some((path, c.offset));
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `lmdeck logs -f` of a run on another host: polls its `lmdeck serve` for
/// the lines written since the last poll, riding out a host that is slow to answer.
fn follow_remote(c: &lmdeck::remote::Client, name: &str, lines: usize) -> Result<()> {
    let (mut path, first) = c.logs_since(name, None, lines)?;
    print_lines(&first.lines)?;
    let mut offset = Some(first.offset);
    let mut failing = false;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        match c.logs_since(name, offset, lines) {
            Ok((p, _)) if p != path && offset.is_some() => {
                // A new log file there (the pull finished, or a new run): its tail, from the top.
                eprintln!("{}", dim(&format!("── {}: now {p} ──", c.name)));
                (path, offset) = (p, None);
            }
            Ok((p, chunk)) => {
                if failing {
                    eprintln!("{}", dim(&format!("── {} answers again ──", c.name)));
                    failing = false;
                }
                if chunk.reset {
                    eprintln!("{}", dim(&format!("── {}: the log started over ──", c.name)));
                }
                print_lines(&chunk.lines)?;
                (path, offset) = (p, Some(chunk.offset));
            }
            Err(e) if !failing => {
                eprintln!("{}", yellow(&format!("{e:#} (still trying)")));
                failing = true;
            }
            Err(_) => {}
        }
    }
}

/// Prints log lines; a closed pipe (`| head`) ends the follow quietly.
fn print_lines(lines: &[String]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for l in lines {
        if writeln!(out, "{l}").is_err() {
            std::process::exit(0);
        }
    }
    out.flush().ok();
    Ok(())
}

// ───────────────────────── remote / multi-node ─────────────────────────

fn host_label(h: &str, node: &str) -> String {
    if h == hosts::LOCAL {
        format!("{node} (local)")
    } else {
        h.to_string()
    }
}

/// `lmdeck use <recipe> --on <host>`, and a start without `--on` that goes to
/// the host holding the weights (`why` says so, under the plan's first line).
fn use_on(ctx: &Ctx, r: &Recipe, on: &str, a: &UseArgs, why: Option<&str>) -> Result<()> {
    let node = ctx.cfg.node_name();
    let plan = lmdeck::placement::plan(&ctx.paths, &ctx.cfg, r, on, a.allow_slow_link)?;
    let mut lines = plan.describe(&node);
    if let Some(w) = why {
        lines.insert(1, format!("why: {w} (--on local runs it here)"));
    }
    if a.dry_run {
        println!("{} {}", bold("plan for"), cyan(&r.name));
        for l in lines {
            println!("  {l}");
        }
        return Ok(());
    }
    for l in lines.iter().take(if why.is_some() { 5 } else { 4 }) {
        println!("  {}", dim(l));
    }
    let deck = Deck::new(ctx.paths.clone(), ctx.cfg.clone());
    for l in lmdeck::placement::execute(&deck, r, &plan, a.only, a.force)? {
        println!("{} {l}", cyan("▶"));
    }
    // A start here synced already; one on another host reaches this machine's
    // harnesses (and, on the hub, every host's) now, loading counts.
    if plan.run_host != hosts::LOCAL {
        println!("  {}", dim(&lmdeck::harness::sync_note(&ctx.paths, &ctx.cfg)));
    }
    if a.no_wait {
        return Ok(());
    }
    wait_on(&deck, &plan.run_host, &r.name, r.port(), r.engine.ready_timeout.unwrap_or(900), &node)
}

fn wait_on(deck: &Deck, host: &str, name: &str, port: u16, timeout: u64, node: &str) -> Result<()> {
    let (_, t) = lmdeck::ops::Target::of(deck, host)?;
    let t0 = std::time::Instant::now();
    let mut last = 0;
    loop {
        // A busy host (an engine still binding its port) can time out a read: keep waiting.
        let state = match t.state(deck, name, port) {
            Ok(s) => s,
            Err(e) if t0.elapsed().as_secs() < timeout => {
                if std::env::var_os("LMDECK_DEBUG").is_some() {
                    eprintln!("debug: {host}: {e:#}");
                }
                lmdeck::ops::NodeState::Up("loading".into())
            }
            Err(e) => return Err(e),
        };
        match state {
            lmdeck::ops::NodeState::Up(s) if s == "loaded" || s == "external" => {
                println!("{} {} is up on {} (port {port})", green("●"), bold(name), host_label(host, node));
                return Ok(());
            }
            lmdeck::ops::NodeState::Stopped if t0.elapsed() > Duration::from_secs(2) => {
                for l in t.logs(deck, name, 15) {
                    eprintln!("{l}");
                }
                bail!("{name} exited during startup on {}", host_label(host, node));
            }
            _ => {}
        }
        let el = t0.elapsed().as_secs();
        if el >= timeout {
            println!("{} {name} still loading after {timeout}s on {}", yellow("◐"), host_label(host, node));
            return Ok(());
        }
        if el / 10 > last {
            last = el / 10;
            eprintln!("  {}", dim(&format!("…waiting ({el}s)")));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `lmdeck use <multi-node recipe>`.
fn use_cluster(ctx: &Ctx, r: &Recipe, a: &UseArgs) -> Result<()> {
    let node = ctx.cfg.node_name();
    let deck = Deck::new(ctx.paths.clone(), ctx.cfg.clone());
    let opts = lmdeck::cluster::Opts { only: a.only, force: a.force, allow_slow_link: a.allow_slow_link };
    if a.on.is_some() {
        bail!("{} is a multi-node recipe; each [[nodes]] entry names its host (drop --on)", r.name);
    }
    let plan = lmdeck::cluster::plan(&deck, r, &opts)?;
    if a.dry_run {
        println!("{} {} {}", bold("plan for"), cyan(&r.name), dim("(multi-node)"));
        for l in plan.describe(&node) {
            println!("  {l}");
        }
        return Ok(());
    }
    println!("{} starting {} across {} nodes", cyan("▶"), bold(&r.name), plan.nodes.len());
    lmdeck::cluster::start(&deck, r, &opts, |l| println!("  {}", dim(l)))?;
    let head = plan.nodes.iter().find(|n| n.role == lmdeck::recipe::NodeRole::Head).expect("validated");
    if a.no_wait {
        println!("{} launched; check with `lmdeck list`", yellow("◐"));
        return Ok(());
    }
    wait_on(&deck, &head.host, &head.served_as, head.port, r.engine.ready_timeout.unwrap_or(900), &node)
}

/// `lmdeck weights`.
fn weights_cmd(ctx: &Ctx, refresh: bool, json: bool) -> Result<()> {
    let idx = if refresh {
        lmdeck::weights::Index::refresh(&ctx.paths, &ctx.cfg)
    } else {
        lmdeck::weights::Index::fresh(&ctx.paths, &ctx.cfg, lmdeck::weights::INDEX_TTL)
    };
    let node = ctx.cfg.node_name();
    let rows = lmdeck::weights::rows(&idx, &node);
    if json {
        let v: Vec<serde_json::Value> = rows
            .iter()
            .map(|(m, h, size, path, kind)| json!({"model": m, "host": h, "size": size, "path": path, "kind": kind}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"demo": idx.demo, "at": idx.at, "weights": v, "hosts": idx.hosts}))?
        );
        return Ok(());
    }
    if idx.demo {
        println!("{}", yellow("DEMO DATA (LMDECK_DEMO=1): simulated hosts"));
    }
    let mut t = Table::new(&["MODEL", "HOST", "SIZE", "KIND", "PATH"]);
    let mut last = String::new();
    for (m, h, size, path, kind) in &rows {
        let shown = if *m == last { dim("〃") } else { bold(m) };
        last = m.clone();
        t.row(vec![shown, h.clone(), human_bytes(*size), dim(kind), dim(path)]);
    }
    if rows.is_empty() {
        println!("{}", dim("no weights found on any host"));
    } else {
        print!("{}", t.render());
    }
    for (h, hw) in &idx.hosts {
        if !hw.online {
            println!(
                "{} {}: {} {}",
                yellow("!"),
                host_label(h, &node),
                hw.error.as_deref().unwrap_or("offline"),
                dim(if hw.inventory.is_some() { "(showing last known)" } else { "" })
            );
        }
    }
    Ok(())
}

// ───────────────────────────── pull ─────────────────────────────

fn pull(ctx: &Ctx, target: &str, dry_run: bool, latest: bool) -> Result<()> {
    let cat = ctx.catalog();
    let found = resolve(&cat, target)?;
    let r = &found.recipe;
    if latest && r.source.is_none() {
        bail!("--latest is for wrapped recipes (with a [source]); {} has none", r.name);
    }
    // A wrapped recipe: update its checkout. Its scripts fetch their own weights,
    // so there is nothing more to do unless it also names an HF repo lmdeck pulls.
    if let Some(src) = &r.source {
        let co = engine::checkout_dir(&ctx.paths.checkouts_dir(), &r.name);
        if dry_run {
            if latest {
                println!("# move ref to the head of the tracked branch, in {}", found.file);
            }
            for line in checkout::describe_sync(src, &co, true) {
                println!("{line}");
            }
        } else {
            println!("{} {} {}", cyan("↓"), src.repo, dim(&format!("→ {}", co.display())));
            let msg = deck::update_source(&ctx.paths, &ctx.cfg, found, latest)?.expect("has a source");
            println!("{} {} {msg}", green("✓"), bold(&r.name));
            if !latest {
                let s = updates::Settings::from(&ctx.cfg);
                if let Some(u) = updates::view(&ctx.paths, &s, r).filter(|u| u.available()) {
                    println!(
                        "  {} pinned to {}; upstream is newer ({}). {}",
                        cyan("⬆"),
                        src.git_ref.as_deref().map(updates::short).unwrap_or_default(),
                        u.delta(),
                        dim(&format!("lmdeck pull --latest {}", r.name))
                    );
                }
            }
        }
        if r.model.repo.is_none() || r.model.fetched_by_scripts() {
            return Ok(());
        }
    }
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
    let fleet = std::cell::OnceCell::new();
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
        let rep = compat::check(r, &hw, None, p.present().then_some(p.size))
            .with_fleet(r, || *fleet.get_or_init(|| lmdeck::hosts::fleet(&ctx.paths, &hw.platform)));
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

#[allow(clippy::too_many_arguments)]
fn browse(
    ctx: &Ctx,
    query: &str,
    tags: &[String],
    sort: &str,
    refresh: bool,
    offline: bool,
    fits: bool,
    json_out: bool,
) -> Result<()> {
    use lmdeck::browse::{self as b, Sort};
    let sort = Sort::parse(sort).ok_or_else(|| anyhow!("--sort must be name, sparks or speed"))?;
    let loaded = if offline {
        lmdeck::index::cached(&ctx.paths, &ctx.cfg.index)
    } else {
        lmdeck::index::load(&ctx.paths, &ctx.cfg.index, refresh)
    };
    let deck = Deck::new(ctx.paths.clone(), ctx.cfg.clone());
    let snap = deck.snapshot(false);
    let sparks = lmdeck::hosts::fleet(&ctx.paths, &snap.hardware.platform).online;
    let rows = b::rows(&loaded.index, Some(&snap), sparks);

    let shown: Vec<&b::Row> = b::filter(&rows, query, tags, sort)
        .into_iter()
        .filter(|r| !fits || r.fit != lmdeck::compat::Level::Fail)
        .collect();
    if json_out {
        let v = serde_json::json!({
            "url": loaded.url, "fetched_ms": loaded.fetched_ms, "from_cache": loaded.from_cache,
            "hint": loaded.hint, "schema": loaded.index.schema, "sort": sort.as_str(),
            "total": rows.len(), "entries": shown,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if let Some(h) = &loaded.hint {
        eprintln!("{} {h}", yellow("!"));
    }
    println!(
        "{} {} of {} {}",
        bold("recipe index"),
        shown.len(),
        rows.len(),
        dim(&format!("· {} · fetched {} · sort {}", loaded.url, loaded.age_text(), sort.as_str()))
    );
    for r in &shown {
        let e = &r.entry;
        let fit = match r.fit {
            lmdeck::compat::Level::Ok => green("✓ fits"),
            lmdeck::compat::Level::Warn => yellow("! tight"),
            lmdeck::compat::Level::Fail => red("✗ no fit"),
        };
        let state = match r.installed {
            "deck" => green("● in deck"),
            "builtin" => cyan("◆ built-in"),
            _ => dim("○ not installed"),
        };
        let speed = e.speed.as_ref().map(|s| format!("{:.1} tok/s", s.tok_s)).unwrap_or_else(|| "–".into());
        println!();
        let sparks = match e.sparks() {
            1 => "1 Spark".to_string(),
            n => format!("{n} Sparks"),
        };
        let name = if r.fit == lmdeck::compat::Level::Fail { dim(&e.name) } else { bold(&e.name) };
        println!("{name}  {state}  {fit}  {}", dim(&format!("{sparks} · {speed}")));

        if !e.description.is_empty() {
            println!("  {}", e.description);
        }
        let tags: Vec<String> = e.tags.pairs().into_iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("  {}", dim(&tags.join("  ")));
        if let Some(u) = &r.update {
            println!("  {}", yellow(u));
        }
        if r.fit != lmdeck::compat::Level::Ok {
            println!("  {}", dim(&r.fit_reason));
        }
        println!("  {}  {}", dim(&r.readme), cyan(&r.install));
    }
    if shown.is_empty() && !rows.is_empty() {
        println!("  nothing matches; try fewer tags or another query");
    }
    Ok(())
}

fn add_wrapper(ctx: &Ctx, repo: &str, name: Option<String>, git_ref: Option<String>, force: bool) -> Result<()> {
    let mut say = |l: &str| println!("{l}");
    let mut warn = |l: &str| eprintln!("{} {l}", yellow("!"));
    let mut reply = lmdeck::install::Reply { say: &mut say, warn: &mut warn };
    lmdeck::install::add(&ctx.paths, &ctx.cfg, repo, name, git_ref, force, &mut reply)?;
    Ok(())
}

fn recipe_add(ctx: &Ctx, reference: &str, name: Option<String>, force: bool) -> Result<()> {
    let added = lmdeck::install::add_recipe(&ctx.paths, &ctx.cfg, reference, name, force)?;
    println!("{} added {} to the deck {}", green("✓"), bold(&added.name), dim(&format!("({})", added.path.display())));
    Ok(())
}

fn recipe_list(ctx: &Ctx, source: Option<String>, json_out: bool) -> Result<()> {
    let cat = ctx.catalog();
    let entries: Vec<&Found> = cat.entries.iter().filter(|f| source.as_deref().is_none_or(|s| s == f.origin)).collect();
    let ups = update_views(ctx, &entries);
    if json_out {
        let rows: Vec<_> = entries
            .iter()
            .map(|f| {
                json!({"ref": format!("{}/{}", f.origin, f.recipe.name), "name": f.recipe.name, "origin": f.origin,
                "file": f.file, "engine": f.recipe.engine.name.as_str(), "quant": f.recipe.quant_short(),
                "model": f.recipe.model.repo.clone().or(f.recipe.model.path.clone()),
                "update": update_json(ups.get(&f.recipe.name))})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let mut t = Table::new(&["origin", "name", "engine", "quant", "model"]).max_width(4, 56);
    for f in &entries {
        let up = update_tag(ups.get(&f.recipe.name));
        t.row(vec![
            dim(&f.origin),
            if up.is_empty() { bold(&f.recipe.name) } else { format!("{} {up}", bold(&f.recipe.name)) },
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
        update_hint(&ups);
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
    if let Some(src) = &r.source {
        let co = engine::checkout_dir(&ctx.paths.checkouts_dir(), &r.name);
        let state = if co.join(".git").exists() { "checked out" } else { "not cloned yet (first start clones it)" };
        kv("source", format!("{}{}", src.repo, src.git_ref.as_ref().map(|v| format!("@{v}")).unwrap_or_default()));
        kv("checkout", format!("{} {}", co.display(), dim(state)));
        if let Some(u) = update_views(ctx, &[f]).remove(&r.name) {
            kv("upstream", upstream_text(&u, &r.name));
        }
    }
    kv("quant", r.quant_short());
    let p = ctx.presence(r);
    if r.model.is_empty() {
        kv("weights", dim("fetched by the recipe's own scripts").to_string());
    } else {
        kv(
            "weights",
            if p.present() {
                format!("{} {}", green("✓"), human_bytes(p.size))
            } else {
                format!("{} {}", dim("✗ not downloaded"), size_cell(r, &p))
            },
        );
    }
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
    if r.engine.detached {
        kv(
            "mode",
            format!("detached{}", r.engine.container.as_ref().map(|c| format!(" · container {c}")).unwrap_or_default()),
        );
    }
    if !r.engine.stop.is_empty() {
        kv("stop", r.engine.stop.join(" "));
    }
    if !r.tags.is_empty() {
        kv("tags", r.tags.join(", "));
    }
    let hw = Hardware::detect();
    let inv = Inventory::detect(&ctx.cfg);
    let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size))
        .with_fleet(r, || lmdeck::hosts::fleet(&ctx.paths, &hw.platform));
    println!("\n  {} {}", bold("on this machine"), dim(&format!("({})", hw.platform)));
    print_report(&rep);
    match engine::plan(r, &PlanCtx::new(&ctx.cfg, p.local)) {
        Ok(plan) => println!("\n  {}\n  {}", bold("launch"), plan.command_line()),
        Err(e) => println!("\n  {} {e:#}", yellow("launch:")),
    }
    Ok(())
}

/// One line about a wrapped recipe's upstream, for `recipe show` / `outdated`.
fn upstream_text(u: &UpdateView, name: &str) -> String {
    use updates::State as U;
    let follows = u.follows.as_deref().map(|b| format!(" ({b})")).unwrap_or_default();
    match u.state {
        U::Behind => format!(
            "{} {} {}",
            yellow(&format!("⬆ update available{follows}")),
            u.delta(),
            dim(&format!("· lmdeck pull {name}"))
        ),
        U::PinnedNewer => format!(
            "{} {} {}",
            cyan(&format!("⬆ pinned; upstream{follows} is newer")),
            u.delta(),
            dim(&format!("· lmdeck pull --latest {name}"))
        ),
        U::Current => format!(
            "{} {}",
            green(&format!("✓ up to date{follows}")),
            dim(&u.local.as_deref().map(updates::short).unwrap_or_default())
        ),
        U::NotCloned => dim("not cloned yet (cloned at the latest commit on first use)"),
        U::Unknown => dim(&match &u.error {
            Some(e) => format!("? unknown: {e}"),
            None => "? not checked yet".into(),
        }),
        U::Disabled => dim("update checks are off ([updates] check = false or LMDECK_NO_UPDATE_CHECK)"),
    }
}

// ───────────────────────────── outdated ─────────────────────────────

fn outdated(ctx: &Ctx, targets: &[String], fetch: bool, json_out: bool) -> Result<()> {
    let cat = ctx.catalog();
    let entries: Vec<&Found> = if targets.is_empty() {
        cat.entries.iter().filter(|f| f.recipe.source.is_some()).collect()
    } else {
        targets.iter().map(|t| resolve(&cat, t)).collect::<Result<Vec<_>>>()?
    };
    // One wrapped recipe per name (the deck shadows built-ins and sources).
    let mut seen = std::collections::HashSet::new();
    let entries: Vec<&Found> = entries.into_iter().filter(|f| seen.insert(f.recipe.name.clone())).collect();
    let s = updates::Settings::from(&ctx.cfg);
    if !s.enabled && !json_out {
        println!(
            "{}",
            dim("update checks are off ([updates] check = false or LMDECK_NO_UPDATE_CHECK); showing cached results")
        );
    }
    updates::refresh(&ctx.paths, &s, entries.iter().map(|f| &f.recipe), true, s.timeout, fetch);
    let ups = updates::views(&ctx.paths, &s, entries.iter().map(|f| &f.recipe));
    if json_out {
        let rows: Vec<_> = entries
            .iter()
            .filter_map(|f| {
                let u = ups.get(&f.recipe.name)?;
                Some(json!({"name": f.recipe.name, "ref": format!("{}/{}", f.origin, f.recipe.name),
                    "available": u.available(), "hint": u.hint(&f.recipe.name), "update": u}))
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    let mut t = Table::new(&["name", "status", "local", "remote", "behind", "follows", "repo"]).max_width(6, 56);
    for f in &entries {
        let Some(u) = ups.get(&f.recipe.name) else { continue };
        use updates::State as U;
        let status = match u.state {
            U::Behind => yellow("⬆ update"),
            U::PinnedNewer => cyan("⬆ newer (pinned)"),
            U::Current => green("✓ current"),
            U::NotCloned => dim("not cloned"),
            U::Unknown => dim("? unknown"),
            U::Disabled => dim("off"),
        };
        let sha = |c: &Option<String>| c.as_deref().map(updates::short).unwrap_or_else(|| dim("—"));
        t.row(vec![
            bold(&f.recipe.name),
            status,
            sha(&u.local),
            sha(&u.remote),
            u.behind.map(|n| n.to_string()).unwrap_or_else(|| dim(if u.available() { "?" } else { "—" })),
            u.follows.clone().unwrap_or_default(),
            u.repo.clone(),
        ]);
    }
    if t.is_empty() {
        println!("{}", dim("no wrapped recipes (recipes with a [source])"));
        return Ok(());
    }
    print!("{}", t.render());
    for f in &entries {
        let Some(u) = ups.get(&f.recipe.name) else { continue };
        if let Some(h) = u.hint(&f.recipe.name) {
            println!("  {} {}", cyan("→"), h);
        }
        if let (updates::State::Unknown, Some(e)) = (u.state, &u.error) {
            println!("  {} {}: {e}", dim("?"), f.recipe.name);
        }
    }
    if !fetch && ups.values().any(|u| u.available() && u.behind.is_none()) {
        println!("{}", dim("  (--fetch counts the commits behind)"));
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

fn engine_cmd(ctx: &Ctx, c: EngineCmd) -> Result<()> {
    match c {
        EngineCmd::Install { engine, force } => {
            let engine: Engine = engine.parse()?;
            match engine {
                Engine::LlamaCpp => {
                    let mut say = |line: &str| println!("  {line}");
                    let bin = lmdeck::engine_install::install_llama_cpp(&ctx.paths, force, &mut say)?;
                    let v = lmdeck::engine_install::version(&bin);
                    println!(
                        "{} {} {}",
                        green("✓"),
                        bold("llama.cpp"),
                        dim(&v.unwrap_or_else(|| bin.display().to_string()))
                    );
                    Ok(())
                }
                other => {
                    let how = match other {
                        Engine::Vllm | Engine::Sglang | Engine::LmDeploy => {
                            "use a Docker recipe, or install it in a Python environment and set engines.<name>"
                        }
                        Engine::TabbyApi => "clone TabbyAPI and set engines.tabbyapi_dir",
                        Engine::LmStudio => "install LM Studio and its `lms` CLI",
                        Engine::Mlx => "pip install mlx-lm, or set engines.mlx_server",
                        Engine::TensorFold => "run TensorFold's install.sh, or set engines.tensorfold",
                        Engine::Custom => "nothing to install",
                        Engine::LlamaCpp => unreachable!(),
                    };
                    bail!("{other} has no managed installer; {how}")
                }
            }
        }
    }
}

fn doctor(ctx: &Ctx, json_out: bool) -> Result<()> {
    let hw = Hardware::detect();
    let inv = Inventory::detect(&ctx.cfg);
    let cat = ctx.catalog();
    let fleet = std::cell::OnceCell::new();
    let checked: Vec<(&Found, Report)> = listed(&cat)
        .into_iter()
        .map(|f| {
            let p = ctx.presence(&f.recipe);
            let rep = compat::check(&f.recipe, &hw, Some(&inv), p.present().then_some(p.size))
                .with_fleet(&f.recipe, || *fleet.get_or_init(|| lmdeck::hosts::fleet(&ctx.paths, &hw.platform)));
            (f, rep)
        })
        .collect();
    let unsupported = unsupported_schemas(ctx, &cat);
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
        let schemas: Vec<_> = unsupported.iter().map(|(f, e)| json!({"file": f, "error": e})).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"version": env!("CARGO_PKG_VERSION"), "schema": lmdeck::schema::CURRENT, "hardware": hw, "inventory": inv,
                "paths": paths, "recipes": recipes, "recipe_errors": errors, "unsupported_schemas": schemas})
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
    if !inv.engine(Engine::LlamaCpp).is_some_and(|s| s.available) && lmdeck::engine_install::current_asset().is_ok() {
        println!("  {} install with `lmdeck engine install llama.cpp` (managed, ~19 MB)", dim("→"));
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

    let label = if cat.deck().next().is_some() { "deck" } else { "recipes" };
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
    let failed: Vec<_> = cat.errors.iter().filter(|(_, e)| !e.contains(lmdeck::schema::MARKER)).collect();
    if !failed.is_empty() {
        println!("\n{}", bold("recipe files that failed to load"));
        for (f, e) in failed {
            println!("  {} {f}: {}", yellow("!"), dim(e));
        }
    }
    println!("\n{} {}", bold("schemas"), dim(&format!("(this lmdeck reads schema {})", lmdeck::schema::CURRENT)));
    if unsupported.is_empty() {
        println!("  {} config, recipes and index all readable", green("✓"));
    }
    for (f, e) in &unsupported {
        println!("  {} {f}: {}", yellow("!"), e);
    }
    Ok(())
}

/// Files skipped because a newer lmdeck wrote them: the config, recipes, the index cache.
fn unsupported_schemas(ctx: &Ctx, cat: &Catalog) -> Vec<(String, String)> {
    let mut out = vec![];
    if let Some(w) = &ctx.cfg.skipped {
        out.push((ctx.paths.config_file().display().to_string(), w.clone()));
    }
    for (f, e) in &cat.errors {
        if e.contains(lmdeck::schema::MARKER) {
            let msg = e.find(lmdeck::schema::MARKER).map(|i| e[i..].to_string()).unwrap_or_else(|| e.clone());
            out.push((f.clone(), msg));
        }
    }
    // The recipe index: a refused fetch, or a cache written by a newer lmdeck.
    let idx = lmdeck::index::cached(&ctx.paths, &ctx.cfg.index);
    if let Some(h) = idx.hint.as_ref().filter(|h| h.contains(lmdeck::schema::MARKER)) {
        let msg = h.find(lmdeck::schema::MARKER).map(|i| h[i..].to_string()).unwrap_or_else(|| h.clone());
        out.push((format!("recipe index ({})", idx.url), msg));
    }
    out
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
            stop(ctx, Some(&name), false, None)
        } else {
            use_recipe(
                ctx,
                &UseArgs {
                    target: name,
                    only: false,
                    no_wait: false,
                    force: false,
                    dry_run: false,
                    yes: false,
                    no_install: false,
                    on: None,
                    allow_slow_link: false,
                },
            )
        };
        if let Err(e) = res {
            eprintln!("{} {e:#}", red("error:"));
        }
        println!();
        deck_table(ctx, &entries, false, false)?;
    }
}
