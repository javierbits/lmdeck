use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use lmdeck::catalog::{self, Catalog, BUILTIN, DECK};
use lmdeck::checkout;
use lmdeck::compat::{self, Level, Report};
use lmdeck::config::{Config, SourceEntry};
use lmdeck::deck::{self, listed, memory_need_gb, resolve, status_word, Deck};
use lmdeck::engine::{self, Inventory, PlanCtx};
use lmdeck::hardware::{which, Hardware};
use lmdeck::hosts::{self, HostView, HostsFile};
use lmdeck::models::{self, hf_cache_dir, Presence};
use lmdeck::paths::{home_dir, Paths};
use lmdeck::recipe::{is_recipe_file, is_yaml_file, valid_name, Engine, Recipe, YAML_UNSUPPORTED};
use lmdeck::runner::{self, Probe, Ready, RunRecord, Status};
use lmdeck::sources::{self, Found, SourceSpec};
use lmdeck::ui::{self, bold, cyan, dim, green, red, yellow, Table};
use lmdeck::updates::{self, UpdateView};
use lmdeck::util::{human_bytes, human_duration, human_gb, now_secs};
use lmdeck::wrap;

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
    /// Wrap a repo that ships start.sh / stop.sh as a recipe in the deck.
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
    /// Colour themes for the TUI and web dashboard (Omarchy's, plus omp).
    #[command(subcommand)]
    Theme(ThemeCmd),
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
        Some(Cmd::Logs { target, lines, on, node }) => logs(&ctx, &target, lines, on.as_deref(), node.as_deref()),
        Some(Cmd::Weights { refresh, json }) => weights_cmd(&ctx, refresh, json),
        Some(Cmd::Recipe(c)) => recipe_cmd(&ctx, c),
        Some(Cmd::Source(c)) => source_cmd(ctx, c),
        Some(Cmd::Host(c)) => host_cmd(&ctx, c),
        Some(Cmd::Doctor { json }) => doctor(&ctx, json),
        Some(Cmd::Init { models_dir, force }) => init(ctx, models_dir, force),
    }
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
                        dim("on that machine run `lmdeck serve --host <its tailscale/LAN IP> --token <token>`")
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
    let ups = update_views(ctx, entries);
    let mut cache: HashMap<u16, Probe> = HashMap::new();
    let mut t = Table::new(&["#", "", "name", "engine", "quant", "size", "dl", "port", "origin"])
        .right(&[0, 5, 7])
        .max_width(2, 48);
    let mut rows = vec![];
    let node = ctx.cfg.node_name();
    let has_hosts = hosts::HostsFile::load(&ctx.paths).map(|f| !f.hosts.is_empty()).unwrap_or(false);
    let idx = has_hosts.then(|| lmdeck::weights::Index::fresh(&ctx.paths, &ctx.cfg, lmdeck::weights::INDEX_TTL));
    for (i, f) in entries.iter().enumerate() {
        let r = &f.recipe;
        let st = runner::status_cached(&ctx.paths, &r.name, r.port(), &mut cache);
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
            "quant": r.quant_short(), "port": r.port(), "status": status_word(&st),
            "downloaded": p.present(), "size_bytes": p.size, "model": r.model.repo.clone().or(r.model.path.clone()),
            "update": update_json(ups.get(&r.name)),
            "weights_on": weights_on, "nodes": nodes,
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
    if !r.nodes.is_empty() {
        return use_cluster(ctx, r, a);
    }
    if r.nodes() > 1 {
        bail!("{} spans {} nodes; give it [[nodes]] entries to launch it across hosts", r.name, r.nodes());
    }
    if let Some(on) = &a.on {
        return use_on(ctx, r, on, a);
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
        } else {
            println!("{} {n} was not started by lmdeck", dim("○"));
        }
    }
    Ok(())
}

fn logs(ctx: &Ctx, target: &str, lines: usize, on: Option<&str>, node: Option<&str>) -> Result<()> {
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
        on = Some(n.host.clone());
    } else if on.is_none() {
        if let Some(p) = lmdeck::placement::Record::load(&ctx.paths, &name).filter(|p| p.run_host != hosts::LOCAL) {
            if !ctx.paths.log_dir().join(format!("{name}.log")).exists() {
                on = Some(p.run_host);
            }
        }
    }
    if let Some(host) = on.filter(|h| !lmdeck::remote::is_local(h, &me)) {
        let c = lmdeck::remote::Client::named(&ctx.paths, &host)?;
        for l in c.logs(&name, lines)? {
            println!("{l}");
        }
        return Ok(());
    }
    let path = ctx.paths.log_dir().join(format!("{name}.log"));
    if !path.exists() {
        bail!("no log for {name} at {}", path.display());
    }
    println!("{}", runner::tail(&path, lines));
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

/// `lmdeck use <recipe> --on <host>`.
fn use_on(ctx: &Ctx, r: &Recipe, on: &str, a: &UseArgs) -> Result<()> {
    let node = ctx.cfg.node_name();
    let plan = lmdeck::placement::plan(&ctx.paths, &ctx.cfg, r, on, a.allow_slow_link)?;
    if a.dry_run {
        println!("{} {}", bold("plan for"), cyan(&r.name));
        for l in plan.describe(&node) {
            println!("  {l}");
        }
        return Ok(());
    }
    for l in plan.describe(&node).iter().take(4) {
        println!("  {}", dim(l));
    }
    let deck = Deck::new(ctx.paths.clone(), ctx.cfg.clone());
    for l in lmdeck::placement::execute(&deck, r, &plan, a.only, a.force)? {
        println!("{} {l}", cyan("▶"));
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
    // so there is nothing more to do unless it also names an HF repo.
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
        if r.model.repo.is_none() {
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

/// `lmdeck add <repo>`: clone it, read its scripts, write a wrapper recipe.
/// A bare name (no `/`, `:` and not a directory) is looked up in the index.
fn is_index_name(arg: &str) -> bool {
    let a = arg.trim();
    !a.is_empty() && !a.contains(['/', ':', '\\']) && !a.starts_with('~') && !lmdeck::paths::expand(a).is_dir()
}

fn add_from_index(ctx: &Ctx, arg: &str, name: Option<String>, git_ref: Option<String>, force: bool) -> Result<()> {
    let loaded = lmdeck::index::load(&ctx.paths, &ctx.cfg.index, false);
    if let Some(h) = &loaded.hint {
        eprintln!("{} {h}", yellow("!"));
    }
    let Some(e) = loaded.index.find(arg) else {
        let mut near: Vec<(i64, &str)> = loaded
            .index
            .entries
            .iter()
            .filter_map(|e| lmdeck::tui::app::fuzzy_score(arg, &e.name).map(|s| (s, e.name.as_str())))
            .collect();
        near.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let near: Vec<&str> = near.into_iter().take(3).map(|x| x.1).collect();
        let more = if near.is_empty() {
            format!("{} entries in the index; see `lmdeck browse`", loaded.index.entries.len())
        } else {
            format!("did you mean: {}?", near.join(", "))
        };
        bail!("'{arg}' is not in the recipe index ({more}); for a repo pass owner/repo or a URL");
    };
    let e = e.clone();
    println!("{} {} {}", cyan("◆"), bold(&e.name), dim(&format!("from the index ({})", loaded.url)));
    if e.recipe.is_some() && git_ref.is_none() {
        let mut r = lmdeck::index::fetch_recipe(&ctx.cfg.index, &e)?.expect("entry has a recipe link");
        if let Some(n) = name {
            if !valid_name(&n) {
                bail!("invalid name '{n}'");
            }
            r.name = n;
        }
        let path = lmdeck::catalog::add_to_deck(&ctx.paths, &r, force)?;
        println!("{} added {} to the deck {}", green("✓"), bold(&r.name), dim(&format!("({})", path.display())));
        println!("  try: lmdeck use {} --dry-run", r.name);
        return Ok(());
    }
    add_wrapper(ctx, &e.repo, Some(name.unwrap_or(e.name.clone())), git_ref.or(e.git_ref.clone()), force)
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
    let nhosts = HostsFile::load(&ctx.paths).map(|f| f.hosts.len()).unwrap_or(0);
    let rows = b::rows(&loaded.index, Some(&snap), b::machines(nhosts));
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
        println!("{}  {state}  {fit}  {}", bold(&e.name), dim(&format!("{} Spark(s) · {speed}", e.sparks())));
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
    if is_index_name(repo) {
        return add_from_index(ctx, repo, name, git_ref, force);
    }
    let (url, url_ref, default_name) = wrap::parse_repo_arg(repo)?;
    let name = name.unwrap_or(default_name);
    if !valid_name(&name) {
        bail!("invalid name '{name}': use letters, digits, '.', '_' or '-' (or pass --name)");
    }
    let deck_file = ctx.paths.recipes_dir().join(format!("{name}.toml"));
    if deck_file.exists() && !force {
        bail!("{name} is already in the deck ({}); use --force to overwrite", deck_file.display());
    }
    let wanted_ref = git_ref.or(url_ref);
    let co = engine::checkout_dir(&ctx.paths.checkouts_dir(), &name);
    if co.join(".git").exists() {
        let origin = checkout::origin_url(&co).unwrap_or_default();
        if origin != url {
            if !force {
                bail!("{} already holds a checkout of {origin}; use --force to replace it", co.display());
            }
            std::fs::remove_dir_all(&co).with_context(|| format!("removing {}", co.display()))?;
        }
    }
    println!("{} {} {}", cyan("↓"), url, dim(&format!("→ {}", co.display())));
    let src = lmdeck::recipe::GitSource { repo: url.clone(), git_ref: wanted_ref.clone(), branch: None };
    let outcome = checkout::sync(&src, &co, true)?;
    let det = wrap::detect(&co)?;
    let branch = checkout::default_branch(&co);
    // Pin the commit unless a ref was asked for.
    let pinned = wanted_ref.clone().unwrap_or_else(|| outcome.commit().to_string());
    let display = url.rsplit(['/', ':']).next().unwrap_or(&name).trim_end_matches(".git").to_string();
    let pin = wrap::SourcePin {
        repo: &url,
        git_ref: &pinned,
        branch: if wanted_ref.is_none() { branch.as_deref() } else { None },
    };
    let text = wrap::recipe_toml(&name, &display, &pin, &det);
    Recipe::parse(&text, &name).context("the generated recipe does not parse (please report this)")?;
    std::fs::create_dir_all(ctx.paths.recipes_dir())?;
    std::fs::write(&deck_file, &text).with_context(|| format!("writing {}", deck_file.display()))?;
    println!("{} added {} to the deck {}", green("✓"), bold(&name), dim(&format!("({})", deck_file.display())));
    let found = |label: &str, v: Option<String>| match v {
        Some(v) => println!("  {label:<10} {v}"),
        None => println!("  {label:<10} {}", yellow("not found (TODO in the recipe)")),
    };
    found("port", det.port.map(|p| format!("{p} (from {})", det.port_var.clone().unwrap_or_default())));
    if det.uses_docker || det.container.is_some() {
        found("container", det.container.clone());
    }
    println!("  {:<10} {}", "detached", if det.detached { "yes" } else { "no (TODO: check)" });
    let todos = text.matches("TODO").count();
    if todos > 0 {
        println!("{} {todos} TODO(s) to review in {}", yellow("!"), deck_file.display());
    }
    println!("  try: lmdeck use {name} --dry-run");
    Ok(())
}

fn recipe_add(ctx: &Ctx, reference: &str, name: Option<String>, force: bool) -> Result<()> {
    let mut recipe = if catalog::is_file_ref(reference) {
        Recipe::load(Path::new(reference))?
    } else if reference.starts_with("https://") || reference.starts_with("http://") {
        let path_part = reference.split(['?', '#']).next().unwrap_or(reference);
        if is_yaml_file(Path::new(path_part)) {
            bail!("{YAML_UNSUPPORTED}");
        }
        if !is_recipe_file(Path::new(path_part)) {
            bail!("URL must end in .toml (to wrap a repo's start.sh / stop.sh, use `lmdeck add <repo>`)");
        }
        let stem = Path::new(path_part).file_stem().and_then(|s| s.to_str()).unwrap_or("recipe").to_string();
        Recipe::parse(&fetch_url(reference)?, &stem)?
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
        let state = if co.join(".git").exists() { "checked out" } else { "not cloned yet" };
        kv(
            "source",
            format!("{}{} {}", src.repo, src.git_ref.as_ref().map(|v| format!("@{v}")).unwrap_or_default(), dim(state)),
        );
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
    let rep = compat::check(r, &hw, Some(&inv), p.present().then_some(p.size));
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
