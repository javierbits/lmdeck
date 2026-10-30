//! First-run setup: what lmdeck detected on this machine, the other Sparks of
//! the cluster, and which recipe sources to add. Runs once before the harness
//! prompt; `[setup] done` in config.toml keeps it from asking again
//! (`lmdeck host add` / `lmdeck source add` work any time).
//!
//! The other Sparks are found over the ConnectX-7 links only: the neighbours
//! on each CX7 port (a sweep of the subnet when the neighbour table is
//! silent), plus the subnets this machine routes through those ports, which
//! is how a ring reaches the Spark across from it. Every address that answers
//! on ssh or `lmdeck serve` is grouped into one machine by its SSH host key
//! and named by its real hostname, so two cables to the same Spark are one
//! host.

use crate::config::Config;
use crate::fabric::{self, NicKind};
use crate::hosts::HostsFile;
use crate::paths::Paths;
use crate::sources::SourceSpec;
use crate::ssh::{Login, Ssh};
use crate::util::run_with_timeout;
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::process::Command;
use std::time::Duration;

/// The port `lmdeck serve` listens on by default; the scan probes it.
pub const SERVE_PORT: u16 = 7979;
const SSH_PORT: u16 = 22;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
/// Addresses per probe thread: small, so a routed /24 sweeps in a few seconds.
const PROBE_CHUNK: usize = 8;

/// How an address was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// In the neighbour table of a CX7 port: a cable.
    Cable,
    /// Swept from a CX7 port's subnet (silent neighbour table).
    Sweep,
    /// In a subnet this machine routes through a CX7 port (across a ring).
    Routed,
}

/// One address the scan might be able to reach.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub ip: String,
    /// The local CX7 interface it is reached through.
    pub dev: String,
    pub via: Via,
}

/// What answered on one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub candidate: Candidate,
    /// An `lmdeck serve` answered `/api/health`.
    pub serving: bool,
    pub version: Option<String>,
    /// The hostname that `lmdeck serve` reported.
    pub host: Option<String>,
    /// Something listens on port 22.
    pub ssh: bool,
}

/// One machine on the CX7 fabric, however many cables lead to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    /// Its hostname, when it could be learned.
    pub name: Option<String>,
    /// Where the name came from: `hostname` (over ssh), `lmdeck`, `ssh config`, `dns`.
    pub named_by: &'static str,
    /// Every address it answered on, cables first.
    pub addrs: Vec<Candidate>,
    /// How a key login over ssh went.
    pub login: Login,
    /// The `lmdeck serve` version when one answered.
    pub serving: Option<String>,
}

impl Peer {
    /// The address lmdeck uses for it: where `lmdeck serve` answered, else a
    /// cable when there is one.
    pub fn ip(&self) -> &str {
        let first = self.addrs.first();
        let pick =
            if self.serving.is_some() { first } else { self.addrs.iter().find(|a| a.via != Via::Routed).or(first) };
        pick.map(|a| a.ip.as_str()).unwrap_or("")
    }

    fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.ip().to_string())
    }
}

/// What the wizard set up.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Names written to hosts.toml.
    pub hosts: Vec<String>,
    /// `(name, canonical spec)` of new sources; the caller fetches and saves them.
    pub sources: Vec<(String, String)>,
    /// Where weights should land; `None` when the answer did not change it.
    pub models_dir: Option<String>,
}

/// Where the wizard's answers come from: the terminal, or a script in tests.
pub trait Prompt {
    /// Prints `prompt` and reads one line (`None` on EOF).
    fn line(&mut self, prompt: &str) -> Option<String>;
    /// Like [`Prompt::line`], without echoing what is typed (a password).
    fn secret(&mut self, prompt: &str) -> Option<String> {
        self.line(prompt)
    }
}

/// Reads from stdin, printing the prompt first.
pub struct Terminal;

impl Prompt for Terminal {
    fn line(&mut self, prompt: &str) -> Option<String> {
        print!("{prompt}");
        std::io::stdout().flush().ok()?;
        let mut s = String::new();
        match std::io::stdin().lock().read_line(&mut s) {
            Ok(0) => None,
            Ok(_) => Some(s),
            Err(_) => None,
        }
    }

    fn secret(&mut self, prompt: &str) -> Option<String> {
        print!("{prompt}");
        std::io::stdout().flush().ok()?;
        let echo_off = NoEcho::new();
        let mut s = String::new();
        let read = std::io::stdin().lock().read_line(&mut s);
        drop(echo_off);
        println!();
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(s),
        }
    }
}

/// Turns terminal echo off until dropped.
struct NoEcho(Option<libc::termios>);

impl NoEcho {
    fn new() -> NoEcho {
        // SAFETY: tcgetattr/tcsetattr on stdin with a zeroed, then filled, termios.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return NoEcho(None);
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) != 0 {
                return NoEcho(None);
            }
            NoEcho(Some(saved))
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            }
        }
    }
}

/// Whether the first-run setup should run for this invocation.
pub fn should_run(cfg: &Config) -> bool {
    if cfg.setup.done || cfg.skipped.is_some() || std::env::var_os("LMDECK_NO_SYNC").is_some() {
        return false;
    }
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// How the wizard reaches the other machines; tests swap each part.
pub struct Net<'a> {
    pub ssh: &'a mut dyn Ssh,
    /// The port `lmdeck serve` uses on the peers.
    pub port: u16,
    /// Scans for the peers.
    pub find: &'a mut dyn FnMut(&mut dyn Ssh) -> Vec<Peer>,
    /// Is an `lmdeck serve` answering at `ip:port`?
    pub health: &'a mut dyn FnMut(&str, u16) -> bool,
}

/// Detects this machine's summary and scans for cluster peers, then prompts.
/// When hosts were added, shows the topology they make.
pub fn wizard(paths: &Paths, cfg: &Config) -> Result<Outcome> {
    let fab = fabric::detect();
    let mut ssh = crate::ssh::System::default();
    let mut find = |ssh: &mut dyn Ssh| scan(&fab, SERVE_PORT, ssh);
    let mut health = |ip: &str, port: u16| probe_one(ip, port).0;
    let mut net = Net { ssh: &mut ssh, port: SERVE_PORT, find: &mut find, health: &mut health };
    let out = run_wizard(paths, cfg, &mut Terminal, &fab, &mut net)?;
    if !out.hosts.is_empty() {
        println!("\n{}", crate::ui::bold("topology"));
        let views = crate::hosts::snapshot_once(paths, true);
        let mut t = crate::topology::build(paths, cfg, &views, false);
        // The cluster is the CX7 cabling; Tailscale links aren't part of it.
        t.links.retain(|l| l.link.kind != crate::fabric::LinkKind::Tailscale);
        for line in crate::topology::render_text(&t).lines() {
            println!("  {line}");
        }
    }
    Ok(out)
}

/// The whole wizard against any [`Prompt`] and [`Net`].
pub fn run_wizard(
    paths: &Paths,
    cfg: &Config,
    p: &mut dyn Prompt,
    fab: &fabric::Fabric,
    net: &mut Net,
) -> Result<Outcome> {
    println!("{}", crate::ui::bold("lmdeck first-run setup"));
    let hw = crate::hardware::Hardware::detect();
    let inv = crate::engine::Inventory::detect(cfg);
    for line in machine_summary(&hw, fab, &inv) {
        println!("  {line}");
    }
    if ask_yes(p, "  does this look right?", true) {
        println!("{}", crate::ui::dim("  (details any time: `lmdeck doctor`)"));
    } else {
        println!("{}", crate::ui::dim("  check `lmdeck doctor`; continuing anyway."));
    }

    // Where the weights land, before the machines that will run them.
    let mut models_dir = cfg.models_dir.clone();
    println!("\n{}", crate::ui::bold("model weights"));
    println!("{}", crate::ui::dim("local-only weights, and recipes with a [model] path, live here."));
    println!("{}", crate::ui::dim("lmdeck pull puts Hugging Face weights (GGUFs too) in its cache: $HF_HOME/hub,"));
    println!("{}", crate::ui::dim("  else ~/.cache/huggingface/hub; set HF_HOME to move that one"));
    disk_check(&crate::paths::expand(&models_dir));
    loop {
        let hint = space_hint(&models_dir);
        let answer = p.line(&format!("  models dir [{}{}] > ", crate::ui::bold(&models_dir), hint));
        let Some(v) = answer else { break };
        let v = v.trim();
        if v.is_empty() {
            break;
        }
        let Some(v) = usable_dir(v) else {
            println!("  {} that is not a usable path", crate::ui::yellow("!"));
            continue;
        };
        println!(
            "  {} {} {}",
            crate::ui::green("✓"),
            crate::ui::bold(&v),
            crate::ui::dim(&hint_line(&crate::paths::expand(&v)))
        );
        disk_check(&crate::paths::expand(&v));
        models_dir = v;
        break;
    }

    // The other Sparks, over the CX7 links.
    println!("\n{}", crate::ui::bold("the cluster"));
    let mut hosts = HostsFile::load(paths).unwrap_or_default();
    let added = cluster_step(p, net, fab, &mut hosts);
    if added.is_empty() {
        println!("{}", crate::ui::dim("  no hosts added. `lmdeck host add <name> <url>` any time."));
    } else {
        hosts.save(paths)?;
        let listed: Vec<String> = added
            .iter()
            .filter_map(|n| hosts.get(n))
            .map(|h| format!("{} {}", h.name, crate::ui::dim(&h.url)))
            .collect();
        println!("  {} hosts.toml: {}", crate::ui::green("✓"), listed.join(", "));
    }

    // Recipe sources: collections of recipe files. No default; the built-ins are free.
    println!("\n{}", crate::ui::bold("recipe sources"));
    println!(
        "{}",
        crate::ui::dim(
            "repos full of lmdeck recipe files (.toml), searched alongside the built-ins (always included).\n\
             To run a model repo with its own start scripts, skip this and use `lmdeck add <url>` later."
        )
    );

    println!("{}", crate::ui::dim("  github:owner/repo · hf:owner/repo · an https URL · a local directory"));
    let mut sources: Vec<(String, String)> = vec![];
    loop {
        let answer = p.line(&format!("  add a source [{} to finish] > ", crate::ui::dim("empty")));
        let Some(spec) = answer else { break };
        let spec = spec.trim();
        if spec.is_empty() {
            break;
        }
        match SourceSpec::parse(spec) {
            Ok(parsed) => {
                let name = parsed.default_name();
                if cfg.source(&name).is_some() || sources.iter().any(|(n, _)| n == &name) {
                    println!("  {} source '{name}' is already configured", crate::ui::yellow("!"));
                } else if !crate::recipe::valid_name(&name)
                    || name == crate::catalog::DECK
                    || name == crate::catalog::BUILTIN
                {
                    println!("  {} invalid source name '{name}'", crate::ui::yellow("!"));
                } else {
                    println!(
                        "  {} {} {}",
                        crate::ui::green("✓"),
                        crate::ui::bold(&name),
                        crate::ui::dim(&parsed.canonical())
                    );
                    sources.push((name, parsed.canonical()));
                }
            }
            Err(e) => println!("  {} {e:#}", crate::ui::yellow("!")),
        }
    }
    if sources.is_empty() {
        println!("{}", crate::ui::dim("no sources added."));
    }

    Config::save_setup(paths, true)?;
    Ok(Outcome { hosts: added, sources, models_dir: Some(models_dir).filter(|m| m != &cfg.models_dir) })
}

/// Finds the peers, shows them, and asks about each. Returns the names added.
fn cluster_step(p: &mut dyn Prompt, net: &mut Net, fab: &fabric::Fabric, hosts: &mut HostsFile) -> Vec<String> {
    let port = net.port;
    let links_up = fab.nics.iter().any(is_fast);
    if links_up {
        println!("{}", crate::ui::dim("  looking for the other Sparks over the ConnectX-7 links (a few seconds)..."));
    } else {
        println!("{}", crate::ui::dim("  no ConnectX-7 link is up here, so there are no other Sparks to look for."));
    }
    let peers = (net.find)(&mut *net.ssh);
    if peers.is_empty() {
        if links_up {
            println!(
                "{}",
                crate::ui::dim(&format!("  nothing answered on the CX7 links (ssh on :22, lmdeck on :{port})."))
            );
        }
        return vec![];
    }
    show_peers(&peers, port);
    let mut added: Vec<String> = vec![];
    let (serving, silent): (Vec<&Peer>, Vec<&Peer>) = peers.iter().partition(|x| x.serving.is_some());
    for pr in serving {
        let url = format!("http://{}:{port}", pr.ip());
        let known = known_as(hosts, pr);
        if known.is_some() && known == pr.name {
            println!("  {} {} is already in hosts.toml", crate::ui::green("✓"), pr.label());
            continue;
        }
        let ver = pr.serving.as_deref().unwrap_or("?");
        if !ask_yes(p, &format!("  add {} (lmdeck {ver} at {url})?", crate::ui::bold(&pr.label())), true) {
            continue;
        }
        let Some(name) = ask_name(p, pr) else { continue };
        let token = p
            .line("    token (printed by `lmdeck serve` there) [none] > ")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if add_host(hosts, &name, &url, token, pr) && !added.contains(&name) {
            added.push(name);
        }
    }
    if !silent.is_empty() {
        let names: Vec<String> = silent.iter().map(|x| x.label()).collect();
        println!("\n  {} {}: lmdeck isn't running there.", crate::ui::yellow("!"), names.join(", "));
        for l in [
            "Every Spark runs `lmdeck serve`; this one starts, stops and measures the others through it.",
            "I can set that up over SSH on the CX7 link: install lmdeck, start `lmdeck serve` on the",
            "CX7 address with a new token, and add a service so it comes back after a reboot. At most",
            "it asks for your SSH password once (to copy your key) and the sudo password for the service.",
        ] {
            println!("    {}", crate::ui::dim(l));
        }
        for pr in silent {
            println!();
            let q = format!("  set up lmdeck on {} ({})?", crate::ui::bold(&pr.label()), pr.ip());
            if ask_yes(p, &q, true) {
                let Some(r) = crate::peers::bootstrap(p, &mut *net.ssh, pr, port, &mut *net.health) else { continue };
                if added.contains(&r.name) {
                    println!("    {}", crate::ui::dim(&format!("{} is already added: same machine", r.name)));
                    continue;
                }
                let known = Peer { name: Some(r.name.clone()), named_by: "hostname", ..pr.clone() };
                let url = format!("http://{}:{port}", r.ip);
                if let Some(name) = ask_name(p, &known) {
                    if add_host(hosts, &name, &url, Some(r.token), &known) && !added.contains(&name) {
                        added.push(name);
                    }
                }
                continue;
            }
            let q = format!("  add {} anyway? it shows as offline until `lmdeck serve` runs there", pr.label());
            if ask_yes(p, &q, false) {
                let url = format!("http://{}:{port}", pr.ip());
                if let Some(name) = ask_name(p, pr) {
                    if add_host(hosts, &name, &url, None, pr) && !added.contains(&name) {
                        added.push(name);
                    }
                }
            }
        }
    }
    added
}

/// The scan result, one machine per block: name, state, then its addresses.
fn show_peers(peers: &[Peer], port: u16) {
    println!("  found {} {} on the CX7 links:", peers.len(), if peers.len() == 1 { "machine" } else { "machines" });
    let w = peers.iter().map(|x| x.label().chars().count()).max().unwrap_or(0);
    for pr in peers {
        let lm = match &pr.serving {
            Some(v) => crate::ui::green(&format!("lmdeck {v} on :{port}")),
            None => crate::ui::yellow("no lmdeck"),
        };
        let login = match &pr.login {
            Login::Ok => crate::ui::green("ssh ok"),
            Login::Denied => crate::ui::yellow("ssh: key not accepted"),
            Login::HostKey => crate::ui::yellow("ssh: host key changed"),
            Login::Failed(m) => crate::ui::yellow(&format!("ssh: {m}")),
        };
        let named = match (pr.name.is_some(), pr.named_by) {
            (false, _) => crate::ui::dim(" (hostname unknown until ssh works)"),
            (true, "hostname") => String::new(),
            (true, by) => crate::ui::dim(&format!(" (name from {by})")),
        };
        let label = format!("{:<w$}", pr.label());
        println!("    {}  {lm} · {login}{named}", crate::ui::bold(&label));
        // Cables first; a ring neighbour's far-side addresses are just a count.
        let cabled = pr.addrs.iter().any(|a| a.via != Via::Routed);
        let mut addrs: Vec<String> = pr
            .addrs
            .iter()
            .filter(|a| !cabled || a.via != Via::Routed)
            .map(|a| match a.via {
                Via::Routed => format!("{} (routed via {})", a.ip, a.dev),
                _ => format!("{} ({})", a.ip, a.dev),
            })
            .collect();
        let more = pr.addrs.len() - addrs.len();
        if more > 0 {
            addrs.push(format!("+{more} routed"));
        }
        println!("    {:<w$}  {}", "", crate::ui::dim(&addrs.join(", ")));
    }
}

/// The hosts.toml name already pointing at one of the peer's addresses.
fn known_as(hosts: &HostsFile, pr: &Peer) -> Option<String> {
    hosts.hosts.iter().find(|h| pr.addrs.iter().any(|a| url_host(&h.url) == a.ip)).map(|h| h.name.clone())
}

/// `http://10.0.0.2:7979/x` → `10.0.0.2`.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let hostport = rest.split('/').next().unwrap_or(rest);
    hostport.rsplit_once(':').map(|(h, _)| h).unwrap_or(hostport)
}

/// Adds the peer under `name`, replacing any entry that points at the same
/// machine under another name (a second cable, or an earlier made-up name).
fn add_host(hosts: &mut HostsFile, name: &str, url: &str, token: Option<String>, pr: &Peer) -> bool {
    let stale: Vec<String> = hosts
        .hosts
        .iter()
        .filter(|h| h.name != name && pr.addrs.iter().any(|a| url_host(&h.url) == a.ip))
        .map(|h| h.name.clone())
        .collect();
    match hosts.add(name, url, token) {
        Ok(_) => {
            for s in &stale {
                hosts.remove(s);
                println!("    {}", crate::ui::dim(&format!("replaces '{s}', the same machine")));
            }
            true
        }
        Err(e) => {
            println!("  {} {e:#}", crate::ui::yellow("!"));
            false
        }
    }
}

/// The name to save a peer under: its hostname by default, never a made-up
/// one. With no hostname known, an empty answer skips the machine.
fn ask_name(p: &mut dyn Prompt, pr: &Peer) -> Option<String> {
    loop {
        let prompt = match &pr.name {
            Some(n) => format!("    name for {} [{n}] > ", pr.ip()),
            None => format!("    name for {} (hostname unknown; empty skips) > ", pr.ip()),
        };
        let answer = p.line(&prompt);
        let eof = answer.is_none();
        let typed = answer.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let name = typed.or_else(|| pr.name.clone())?;

        if crate::recipe::valid_name(&name) && name != crate::hosts::LOCAL {
            return Some(name);
        }
        println!("  {} invalid host name '{name}' (letters, digits, . _ -)", crate::ui::yellow("!"));
        if eof {
            return None;
        }
    }
}

/// ` · 3.6 TB free`, or empty when the path cannot be measured.
fn space_hint(dir: &str) -> String {
    free_space_note(&crate::paths::expand(dir)).map(|s| format!(" · {s} free")).unwrap_or_default()
}

/// Warns when the filesystem under `path` is nearly full.
fn disk_check(path: &std::path::Path) {
    if let Some(w) = crate::hardware::disk_gb(path).and_then(|(free, total)| crate::hardware::disk_warning(free, total))
    {
        println!("  {} {w}", crate::ui::yellow("!"));
    }
}

/// The free-space sentence for a confirmed directory.
fn hint_line(path: &std::path::Path) -> String {
    free_space_note(path)
        .map(|s| format!("{s} free at {}", path.display()))
        .unwrap_or_else(|| path.display().to_string())
}

/// A path we can actually use: expands to something absolute. An unset
/// variable expands to nothing, which is the only way this fails.
fn usable_dir(v: &str) -> Option<String> {
    let p = crate::paths::expand(v);
    let s = p.to_string_lossy().trim().to_string();
    (!s.is_empty() && p.is_absolute()).then(|| v.trim().to_string())
}

fn free_space_note(path: &std::path::Path) -> Option<String> {
    crate::hardware::free_gb(path).map(crate::hardware::human_gb)
}

pub(crate) fn ask_yes(p: &mut dyn Prompt, prompt: &str, default: bool) -> bool {
    let hint = if default { "Y/n" } else { "y/N" };
    match p.line(&format!("{prompt} [{hint}] > ")) {
        None => default,
        Some(s) => match s.trim().to_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        },
    }
}

/// The machine half of the wizard's first screen.
fn machine_summary(
    hw: &crate::hardware::Hardware,
    fab: &fabric::Fabric,
    inv: &crate::engine::Inventory,
) -> Vec<String> {
    let mut v = vec![];
    v.push(format!("host       {} ({}, {})", hw.hostname, hw.os, hw.arch));
    v.push(format!("platform   {}", if hw.platform.is_empty() { "unknown" } else { hw.platform.as_str() }));
    v.push(format!("cpu        {} · {} cores", if hw.cpu.is_empty() { "unknown" } else { hw.cpu.as_str() }, hw.cores));
    v.push(format!(
        "memory     {:.0} GB total{}{}",
        hw.total_memory_gb,
        hw.available_memory_gb.map(|a| format!(", {:.0} available", a)).unwrap_or_default(),
        if hw.unified_memory { " (unified with the GPU)" } else { "" }
    ));
    if hw.gpus.is_empty() {
        v.push("gpu        none detected".into());
    }
    for g in &hw.gpus {
        v.push(format!("gpu        {}{}", g.name, g.memory_gb.map(|m| format!(" · {m:.0} GB")).unwrap_or_default()));
    }
    let engines: Vec<String> = inv.engines.iter().filter(|(_, st)| st.available).map(|(n, _)| n.clone()).collect();
    v.push(format!("engines    {}", if engines.is_empty() { "none detected".into() } else { engines.join(" · ") }));
    let shown = fab.nics.iter().filter(|n| matches!(n.kind, NicKind::Cx7 | NicKind::Rdma));
    for n in shown {
        let ips = n.ipv4.iter().map(|a| format!("{}/{}", a.ip, a.prefix)).collect::<Vec<_>>().join(", ");
        let rdma = n.rdma.as_deref().map(|r| format!(" · rdma {r}")).unwrap_or_default();
        let kind = match n.kind {
            NicKind::Cx7 => " · cx7",
            NicKind::Rdma => " · rdma",
            _ => "",
        };
        v.push(format!("net        {} {} {}{}{}", n.name, n.state, ips, rdma, kind));
    }
    v
}

/// A ConnectX-7 / RDMA port that is up with an address: a Spark-to-Spark link.
fn is_fast(n: &fabric::Nic) -> bool {
    matches!(n.kind, NicKind::Cx7 | NicKind::Rdma) && n.state == "up" && !n.ipv4.is_empty()
}

/// Scans the CX7 fabric: candidate addresses, what answers on them, and
/// which answers are the same machine.
pub fn scan(fab: &fabric::Fabric, port: u16, ssh: &mut dyn Ssh) -> Vec<Peer> {
    let cands = candidates_from(fab, &ip_json(&["-j", "neighbor"]), &ip_json(&["-j", "-4", "route"]));
    let answers = probe_all(&cands, port);
    identify(&answers, ssh, &fallback_name)
}

/// Addresses worth probing, all through a CX7 port: neighbours (cables), a
/// sweep of a port's subnet when its neighbour table is silent, and every
/// address in the subnets routed through a CX7 port (the far side of a ring).
/// `neighbours_json` / `routes_json` are `ip -j neighbor` / `ip -j -4 route`.
pub fn candidates_from(fab: &fabric::Fabric, neighbours_json: &str, routes_json: &str) -> Vec<Candidate> {
    let fast: Vec<&fabric::Nic> = fab.nics.iter().filter(|n| is_fast(n)).collect();
    if fast.is_empty() {
        return vec![];
    }
    let local: Vec<&str> = fab.nics.iter().flat_map(|n| n.ipv4.iter().map(|a| a.ip.as_str())).collect();
    let mut out: Vec<Candidate> = vec![];
    let neighbours = parse_neighbours(neighbours_json, &fast, &local);
    for (ip, dev) in &neighbours {
        out.push(Candidate { ip: ip.clone(), dev: dev.clone(), via: Via::Cable });
    }
    for nic in &fast {
        if neighbours.iter().any(|(_, dev)| dev == &nic.name) {
            continue;
        }
        for a in &nic.ipv4 {
            for ip in sweep(&a.ip, a.prefix) {
                out.push(Candidate { ip, dev: nic.name.clone(), via: Via::Sweep });
            }
        }
    }
    for (net, prefix, dev) in parse_routes(routes_json, &fast) {
        for ip in hosts_in(&net, prefix) {
            out.push(Candidate { ip, dev: dev.clone(), via: Via::Routed });
        }
    }
    let mut seen: Vec<String> = vec![];
    out.retain(|c| {
        if local.contains(&c.ip.as_str()) || seen.contains(&c.ip) {
            false
        } else {
            seen.push(c.ip.clone());
            true
        }
    });
    out
}

/// IPv4 neighbours (`ip -j neighbor`) with a link-layer address on the given
/// interfaces, minus this machine's own addresses.
fn parse_neighbours(json: &str, fast: &[&fabric::Nic], local: &[&str]) -> Vec<(String, String)> {
    let Ok(arr) = serde_json::from_str::<Vec<Value>>(json) else { return vec![] };
    let names: Vec<&str> = fast.iter().map(|n| n.name.as_str()).collect();
    arr.iter()
        .filter_map(|it| {
            let ip = it.get("dst").and_then(Value::as_str)?.parse::<Ipv4Addr>().ok()?;
            if ip.is_loopback() || ip.is_multicast() || local.contains(&ip.to_string().as_str()) {
                return None;
            }
            it.get("lladdr").and_then(Value::as_str)?;
            let dev = it.get("dev").and_then(Value::as_str)?;
            names.contains(&dev).then(|| (ip.to_string(), dev.to_string()))
        })
        .collect()
}

/// Subnets reached through a gateway on a CX7 port (`ip -j -4 route`), as
/// `(network, prefix, device)`. The default route never counts.
fn parse_routes(json: &str, fast: &[&fabric::Nic]) -> Vec<(String, u8, String)> {
    let Ok(arr) = serde_json::from_str::<Vec<Value>>(json) else { return vec![] };
    let names: Vec<&str> = fast.iter().map(|n| n.name.as_str()).collect();
    arr.iter()
        .filter_map(|r| {
            r.get("gateway").and_then(Value::as_str)?;
            let dev = r.get("dev").and_then(Value::as_str)?;
            if !names.contains(&dev) {
                return None;
            }
            let (net, prefix) = r.get("dst").and_then(Value::as_str)?.split_once('/')?;
            net.parse::<Ipv4Addr>().ok()?;
            Some((net.to_string(), prefix.parse().ok()?, dev.to_string()))
        })
        .collect()
}

/// Stdout of `ip <args>`, empty when the command is missing.
fn ip_json(args: &[&str]) -> String {
    run_with_timeout(Command::new("ip").args(args), Duration::from_secs(4)).unwrap_or_default()
}

/// The usable addresses of the subnet `ip/prefix` belongs to (not the
/// network or broadcast address). Subnets larger than /24 are skipped: too
/// many blind probes.
fn hosts_in(ip: &str, prefix: u8) -> Vec<String> {
    let Ok(ip) = ip.parse::<Ipv4Addr>() else { return vec![] };
    if !(24..=31).contains(&prefix) {
        return vec![];
    }
    let mask = u32::MAX << (32 - u16::from(prefix));
    let net = u32::from(ip) & mask;
    if prefix == 31 {
        // RFC 3021: both addresses are usable.
        return vec![Ipv4Addr::from(net).to_string(), Ipv4Addr::from(net + 1).to_string()];
    }
    let count = 1u64 << (32 - u16::from(prefix));
    (1..count - 1).map(|i| Ipv4Addr::from(net + i as u32).to_string()).collect()
}

/// The other addresses in `ip`'s subnet.
fn sweep(ip: &str, prefix: u8) -> Vec<String> {
    hosts_in(ip, prefix).into_iter().filter(|a| a != ip).collect()
}

/// A name for a machine ssh couldn't log in to: an alias in ~/.ssh/config
/// that lists the address, else reverse DNS.
fn fallback_name(ip: &str) -> Option<(String, &'static str)> {
    crate::ssh::alias_for(ip).map(|n| (n, "ssh config")).or_else(|| reverse_name(ip).map(|n| (n, "dns")))
}

/// Reverse DNS (`getent hosts`), short name.
fn reverse_name(ip: &str) -> Option<String> {
    let out = run_with_timeout(Command::new("getent").args(["hosts", ip]), Duration::from_secs(2))?;
    let name = out.split_whitespace().nth(1)?;
    Some(name.split('.').next().unwrap_or(name).to_string())
}

/// Groups the answers into machines: by SSH host key (same machine, same key
/// on every cable), then by hostname, which one key login per machine reads.
/// `fallback` names a machine ssh couldn't log in to.
pub fn identify(
    answers: &[Answer],
    ssh: &mut dyn Ssh,
    fallback: &dyn Fn(&str) -> Option<(String, &'static str)>,
) -> Vec<Peer> {
    let mut groups: Vec<(Option<String>, Vec<&Answer>)> = vec![];
    for a in answers.iter().filter(|a| a.serving || a.ssh) {
        let key = if a.ssh { ssh.host_key(&a.candidate.ip) } else { None };
        match groups.iter_mut().find(|(k, _)| key.is_some() && k == &key) {
            Some((_, members)) => members.push(a),
            None => groups.push((key, vec![a])),
        }
    }
    let mut peers: Vec<Peer> = vec![];
    for (_, members) in groups {
        let mut login = Login::Failed("nothing listens on :22".into());
        let mut name: Option<String> = None;
        let mut named_by = "";
        for a in members.iter().filter(|a| a.ssh) {
            let out = ssh.login(&a.candidate.ip, "hostname", None);
            login = Login::from_out(&out);
            if login == Login::Ok {
                name = out.stdout.lines().next().map(short_name).filter(|s| !s.is_empty());
                named_by = "hostname";
                break;
            }
            if !matches!(login, Login::Failed(_)) {
                break;
            }
        }
        if name.is_none() {
            if let Some(h) = members.iter().find_map(|a| a.host.clone()) {
                name = Some(short_name(&h));
                named_by = "lmdeck";
            }
        }
        if name.is_none() {
            if let Some((n, by)) = members.iter().find_map(|a| fallback(&a.candidate.ip)) {
                name = Some(n);
                named_by = by;
            }
        }
        let mut addrs: Vec<Candidate> = members.iter().map(|a| a.candidate.clone()).collect();
        let serving = members.iter().find(|a| a.serving).map(|a| a.version.clone().unwrap_or_else(|| "?".into()));
        // The address lmdeck answered on goes first: that's where hosts.toml points.
        if let Some(at) = members.iter().find(|a| a.serving).map(|a| a.candidate.ip.clone()) {
            addrs.sort_by_key(|a| a.ip != at);
        }
        peers.push(Peer { name, named_by, addrs, login, serving });
    }
    // Machines whose keys couldn't be read but report the same hostname.
    let real = |p: &Peer| matches!(p.named_by, "hostname" | "lmdeck");
    let mut merged: Vec<Peer> = vec![];
    for pr in peers {
        let same = merged.iter_mut().find(|m| {
            real(m)
                && real(&pr)
                && m.name.as_deref().map(str::to_lowercase) == pr.name.as_deref().map(str::to_lowercase)
        });
        match same {
            Some(m) => {
                if m.serving.is_none() && pr.serving.is_some() {
                    // Where lmdeck answered goes first.
                    let mine = std::mem::take(&mut m.addrs);
                    m.addrs = pr.addrs;
                    m.addrs.extend(mine);
                    m.serving = pr.serving;
                } else {
                    m.addrs.extend(pr.addrs);
                }
                if pr.login == Login::Ok {
                    m.login = Login::Ok;
                    m.named_by = "hostname";
                }
            }
            None => merged.push(pr),
        }
    }
    for m in &mut merged {
        if m.serving.is_none() {
            m.addrs.sort_by_key(|a| a.via == Via::Routed);
        }
    }
    merged.sort_by_key(|m| m.addrs.iter().all(|a| a.via == Via::Routed));
    merged
}

fn short_name(s: &str) -> String {
    let s = s.trim();
    s.split('.').next().unwrap_or(s).to_string()
}

/// Is there an `lmdeck serve` on this address? `(serving, version, hostname)`.
pub fn probe_one(ip: &str, port: u16) -> (bool, Option<String>, Option<String>) {
    let Ok(addr) = format!("{ip}:{port}").parse::<std::net::SocketAddr>() else {
        return (false, None, None);
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else {
        return (false, None, None);
    };
    let _ = s.set_read_timeout(Some(READ_TIMEOUT));
    let _ = s.set_write_timeout(Some(READ_TIMEOUT));
    let req =
        format!("GET /api/health HTTP/1.1\r\nHost: {ip}\r\nConnection: close\r\nUser-Agent: lmdeck-setup\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return (false, None, None);
    }
    let mut buf = Vec::new();
    let _ = s.take(64 * 1024).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    match serde_json::from_str::<Value>(body.trim()) {
        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
            let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
            (true, s("version"), s("host"))
        }
        _ => (false, None, None),
    }
}

/// Whether something accepts a TCP connection on `ip:port`.
fn tcp_open(ip: &str, port: u16) -> bool {
    format!("{ip}:{port}")
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|a| TcpStream::connect_timeout(&a, CONNECT_TIMEOUT).is_ok())
}

/// Probes every candidate for `lmdeck serve` on `port` and ssh on :22, a few
/// addresses per thread.
pub fn probe_all(cands: &[Candidate], port: u16) -> Vec<Answer> {
    probe_all_with(cands, port, SSH_PORT)
}

fn probe_all_with(cands: &[Candidate], port: u16, ssh_port: u16) -> Vec<Answer> {
    std::thread::scope(|s| {
        let handles: Vec<_> = cands
            .chunks(PROBE_CHUNK)
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .map(|c| {
                            let (serving, version, host) = probe_one(&c.ip, port);
                            let ssh = tcp_open(&c.ip, ssh_port);
                            Answer { candidate: c.clone(), serving, version, host, ssh }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fabric::Nic;
    use crate::ssh::Out;
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};

    fn nic(name: &str, kind: NicKind, state: &str, ipv4: &[(&str, u8)]) -> Nic {
        Nic {
            name: name.into(),
            kind,
            mac: None,
            state: state.into(),
            mtu: None,
            speed_mbps: None,
            driver: None,
            ipv4: ipv4.iter().map(|(ip, p)| fabric::Addr { ip: ip.to_string(), prefix: *p }).collect(),
            ipv6: vec![],
            rdma: None,
            gid_index: None,
        }
    }

    fn cand(ip: &str, dev: &str, via: Via) -> Candidate {
        Candidate { ip: ip.into(), dev: dev.into(), via }
    }

    /// A scripted ssh: host keys and command replies per address; records calls.
    #[derive(Default)]
    pub(crate) struct MockSsh {
        pub keys: HashMap<String, String>,
        /// `(address, command substring)` → reply; the first match wins.
        pub replies: Vec<(String, String, Out)>,
        pub calls: Vec<(String, String, Option<String>)>,
        /// The key each of `calls` named (`use_key`), if any.
        pub named: Vec<Option<PathBuf>>,
        /// Every `copy_id`: the address and the key.
        pub copied: Vec<(String, PathBuf)>,
        pub copy_ok: bool,
        /// This user's passphrase-free keys, as `keys` reports them.
        pub usable: Vec<PathBuf>,
        /// The keys the peers accept when named; a copy that works adds one.
        pub authorized: Vec<PathBuf>,
        /// The key named per address (`use_key`).
        pub using: HashMap<String, PathBuf>,
    }

    impl MockSsh {
        pub fn reply(&mut self, addr: &str, cmd: &str, out: Out) {
            self.replies.push((addr.into(), cmd.into(), out));
        }
    }

    impl Ssh for MockSsh {
        /// The first matching reply; it is used up when a later one also
        /// matches. A named key that isn't authorized is refused instead.
        fn run(&mut self, addr: &str, cmd: &str, stdin: Option<&[u8]>) -> Out {
            self.calls.push((addr.into(), cmd.into(), stdin.map(|s| String::from_utf8_lossy(s).into_owned())));
            let key = self.using.get(addr).cloned();
            self.named.push(key.clone());
            if key.is_some_and(|k| !self.authorized.contains(&k)) {
                return Out::fail(&format!("jvr0x@{addr}: Permission denied (publickey)."));
            }
            let hits: Vec<usize> = (0..self.replies.len())
                .filter(|&i| self.replies[i].0 == addr && cmd.contains(self.replies[i].1.as_str()))
                .collect();
            match hits.as_slice() {
                [] => Out::fail(&format!("ssh: connect to host {addr} port 22: Connection timed out")),
                [only] => self.replies[*only].2.clone(),
                [first, ..] => self.replies.remove(*first).2,
            }
        }
        fn host_key(&mut self, addr: &str) -> Option<String> {
            self.keys.get(addr).cloned()
        }
        /// Records the copy, and authorizes the key when it works.
        fn copy_id(&mut self, addr: &str, key: &Path) -> bool {
            self.copied.push((addr.into(), key.to_path_buf()));
            if self.copy_ok {
                self.authorized.push(key.to_path_buf());
            }
            self.copy_ok
        }
        /// Returns the scripted usable keys.
        fn keys(&mut self, _addr: &str) -> Vec<PathBuf> {
            self.usable.clone()
        }
        /// Makes `id_ed25519`, as ssh-keygen would.
        fn make_key(&mut self) -> Option<PathBuf> {
            Some(PathBuf::from("id_ed25519"))
        }
        /// Names `key` for `addr` from now on, or goes back to ssh's own choice.
        fn use_key(&mut self, addr: &str, key: Option<&Path>) {
            match key {
                Some(k) => self.using.insert(addr.into(), k.to_path_buf()),
                None => self.using.remove(addr),
            };
        }
    }

    #[test]
    fn sweep_lists_the_subnet_but_not_us() {
        assert_eq!(sweep("10.0.0.5", 24).len(), 253);
        assert!(!sweep("10.0.0.5", 24).contains(&"10.0.0.5".to_string()));
        assert!(!sweep("10.0.0.5", 24).contains(&"10.0.0.0".to_string()));
        assert!(!sweep("10.0.0.5", 24).contains(&"10.0.0.255".to_string()));
        assert_eq!(sweep("10.0.0.1", 31), vec!["10.0.0.0".to_string()]);
        assert_eq!(sweep("10.0.0.1", 30), vec!["10.0.0.2".to_string()]);
        assert!(sweep("10.0.0.1", 32).is_empty());
        assert!(sweep("10.0.0.1", 16).is_empty(), "too large to sweep");
        assert_eq!(hosts_in("10.100.136.0", 24).len(), 254);
    }

    #[test]
    fn neighbours_need_a_link_address_on_a_fast_port() {
        let nics = [nic("cx", NicKind::Cx7, "up", &[("10.0.0.1", 31)])];
        let fast: Vec<&fabric::Nic> = nics.iter().collect();
        let json = r#"[
            {"dst": "10.0.0.2", "dev": "cx", "lladdr": "aa"},
            {"dst": "10.0.0.3", "dev": "cx", "state": ["FAILED"]},
            {"dst": "192.168.1.4", "dev": "eno1", "lladdr": "bb"},
            {"dst": "10.0.0.1", "dev": "cx", "lladdr": "cc"},
            {"dst": "not-an-ip", "dev": "cx", "lladdr": "dd"},
            {"dev": "cx"}
        ]"#;
        let got = parse_neighbours(json, &fast, &["10.0.0.1"]);
        assert_eq!(got, vec![("10.0.0.2".to_string(), "cx".to_string())]);
    }

    /// The folded ring as seen from one Spark: two cables (two subnets each)
    /// to its ring neighbours, and the far Spark's subnets routed through them.
    fn ring_fabric() -> fabric::Fabric {
        fabric::Fabric {
            nics: vec![
                nic("lo", NicKind::Loopback, "unknown", &[("127.0.0.1", 8)]),
                nic("wl0", NicKind::Ethernet, "up", &[("192.168.1.102", 24)]),
                nic("tailscale0", NicKind::Tailscale, "unknown", &[("100.102.79.91", 32)]),
                nic("cxa", NicKind::Cx7, "up", &[("10.100.132.1", 24)]),
                nic("cxb", NicKind::Cx7, "up", &[("10.100.133.1", 24)]),
                nic("cxc", NicKind::Cx7, "up", &[("10.100.128.2", 24)]),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn candidates_are_cx7_only_cables_then_routed_subnets() {
        let fab = ring_fabric();
        let nb = r#"[{"dst": "10.100.132.2", "dev": "cxa", "lladdr": "aa"},
                     {"dst": "10.100.133.2", "dev": "cxb", "lladdr": "ab"},
                     {"dst": "192.168.1.1", "dev": "wl0", "lladdr": "ff"}]"#;
        let routes = r#"[{"dst": "default", "gateway": "192.168.1.1", "dev": "wl0"},
                         {"dst": "10.100.136.0/24", "gateway": "10.100.132.2", "dev": "cxa"},
                         {"dst": "10.100.200.0/24", "gateway": "192.168.1.1", "dev": "wl0"},
                         {"dst": "10.100.132.0/24", "dev": "cxa", "scope": "link"}]"#;
        let got = candidates_from(&fab, nb, routes);
        assert_eq!(got[0], cand("10.100.132.2", "cxa", Via::Cable));
        assert_eq!(got[1], cand("10.100.133.2", "cxb", Via::Cable));
        // cxc's neighbour table is silent: its /24 is swept.
        assert_eq!(got.iter().filter(|c| c.via == Via::Sweep && c.dev == "cxc").count(), 253);
        // The far Spark's subnet, routed through a cable.
        let routed: Vec<&Candidate> = got.iter().filter(|c| c.via == Via::Routed).collect();
        assert_eq!(routed.len(), 254);
        assert!(routed.iter().all(|c| c.ip.starts_with("10.100.136.") && c.dev == "cxa"));
        // Nothing from Wi-Fi, Tailscale or the LAN, and never ourselves.
        assert!(got.iter().all(|c| c.ip.starts_with("10.100.1")), "only CX7 subnets");
        assert!(!got.iter().any(|c| c.ip == "10.100.132.1" || c.ip == "10.100.128.2"));
        // No CX7 link up: nothing at all.
        let mut fab2 = fab.clone();
        fab2.nics.retain(|n| n.kind != NicKind::Cx7);
        assert!(candidates_from(&fab2, nb, routes).is_empty());
    }

    fn ans(ip: &str, via: Via, ssh: bool, serving: Option<(&str, &str)>) -> Answer {
        Answer {
            candidate: cand(ip, "cx", via),
            serving: serving.is_some(),
            version: serving.map(|(v, _)| v.to_string()),
            host: serving.map(|(_, h)| h.to_string()),
            ssh,
        }
    }

    #[test]
    fn identify_groups_cables_by_host_key_and_names_by_hostname() {
        let mut ssh = MockSsh::default();
        for (ip, key) in [
            ("10.100.132.2", "K-ember"),
            ("10.100.133.2", "K-ember"),
            ("10.100.128.1", "K-giga"),
            ("10.100.136.1", "K-far"),
            ("10.100.134.2", "K-far"),
        ] {
            ssh.keys.insert(ip.into(), key.into());
        }
        ssh.reply("10.100.132.2", "hostname", Out::ok("ember\n"));
        ssh.reply("10.100.128.1", "hostname", Out::ok("gigabyte.lan\n"));
        ssh.reply("10.100.136.1", "hostname", Out::fail("jvr0x@10.100.136.1: Permission denied (publickey,password)."));
        let answers = vec![
            ans("10.100.132.2", Via::Cable, true, None),
            ans("10.100.133.2", Via::Cable, true, None),
            ans("10.100.128.1", Via::Cable, true, None),
            // gigabyte's lmdeck answers on its second cable, but its key couldn't be read there.
            ans("10.100.129.1", Via::Cable, false, Some(("0.1.0", "gigabyte"))),
            ans("10.100.136.1", Via::Routed, true, None),
            ans("10.100.134.2", Via::Routed, true, None),
            ans("10.100.140.9", Via::Routed, false, None), // nothing listens: dropped
        ];
        let peers = identify(&answers, &mut ssh, &|_| None);
        assert_eq!(peers.len(), 3, "{peers:#?}");
        assert_eq!(peers[0].name.as_deref(), Some("ember"));
        assert_eq!(peers[0].login, Login::Ok);
        assert_eq!(peers[0].addrs.len(), 2);
        assert_eq!(peers[0].ip(), "10.100.132.2");
        assert_eq!(peers[1].name.as_deref(), Some("gigabyte"), "the FQDN is shortened");
        assert_eq!(peers[1].serving.as_deref(), Some("0.1.0"));
        assert_eq!(peers[1].ip(), "10.100.129.1", "hosts.toml points where lmdeck answered");
        assert_eq!(peers[1].addrs.len(), 2);
        assert_eq!(peers[2].name, None, "no made-up name when ssh can't log in");
        assert_eq!(peers[2].login, Login::Denied);
        assert_eq!(peers[2].addrs.len(), 2);
        // One login per machine, not per cable.
        assert_eq!(ssh.calls.iter().filter(|(_, c, _)| c == "hostname").count(), 3);

        // Without a login, an ssh config alias names it, and says so.
        let mut ssh2 = MockSsh::default();
        let peers =
            identify(&answers[4..5], &mut ssh2, &|ip| (ip == "10.100.136.1").then(|| ("faraday".into(), "ssh config")));
        assert_eq!(peers[0].name.as_deref(), Some("faraday"));
        assert_eq!(peers[0].named_by, "ssh config");
    }

    /// A one-shot HTTP server for probe tests.
    struct Stub {
        port: u16,
    }

    impl Stub {
        fn start(respond: &'static str) -> Stub {
            use std::net::TcpListener;
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            std::thread::spawn(move || {
                if let Ok((mut s, _)) = l.accept() {
                    let mut buf = [0u8; 1024];
                    let _ = std::io::Read::read(&mut s, &mut buf);
                    let _ = s.write_all(respond.as_bytes());
                }
            });
            Stub { port }
        }
    }

    #[test]
    fn probe_fingerprints_lmdeck_health() {
        let good = Stub::start(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true,\"version\":\"9.9.9\",\"host\":\"ember\"}\n",
        );
        let (serving, version, host) = probe_one("127.0.0.1", good.port);
        assert!(serving);
        assert_eq!(version.as_deref(), Some("9.9.9"));
        assert_eq!(host.as_deref(), Some("ember"));

        let junk = Stub::start("HTTP/1.1 200 OK\r\n\r\n<html>not lmdeck</html>");
        assert!(!probe_one("127.0.0.1", junk.port).0);

        // Nothing listening on a likely-free port.
        assert!(!probe_one("127.0.0.1", 1).0);
    }

    #[test]
    fn probe_all_walks_every_candidate() {
        let good = Stub::start("HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}");
        let ssh = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ssh_port = ssh.local_addr().unwrap().port();
        let cands = vec![cand("127.0.0.1", "cx", Via::Cable), cand("10.255.255.1", "cx", Via::Sweep)];
        let found = probe_all_with(&cands, good.port, ssh_port);
        assert_eq!(found.len(), 2);
        assert!(found[0].serving && found[0].ssh);
        assert!(!found[1].serving && !found[1].ssh);
    }

    /// Scripted answers for the wizard.
    pub(crate) struct Script(pub VecDeque<String>);

    impl Script {
        pub fn new(lines: &[&str]) -> Script {
            Script(lines.iter().map(|s| s.to_string()).collect())
        }
    }

    impl Prompt for Script {
        fn line(&mut self, _prompt: &str) -> Option<String> {
            self.0.pop_front()
        }
    }

    fn tmp(tag: &str) -> (tempfile::TempDir, Paths) {
        let d = tempfile::TempDir::new().unwrap();
        let root = d.path().to_path_buf();
        (
            d,
            Paths {
                config_dir: root.join(tag).join("config"),
                cache_dir: root.join(tag).join("cache"),
                state_dir: root.join(tag).join("state"),
            },
        )
    }

    fn peer(name: Option<&str>, ips: &[&str], serving: Option<&str>, login: Login) -> Peer {
        Peer {
            name: name.map(str::to_string),
            named_by: if name.is_some() { "hostname" } else { "" },
            addrs: ips.iter().map(|ip| cand(ip, "cx", Via::Cable)).collect(),
            login,
            serving: serving.map(str::to_string),
        }
    }

    fn wiz(paths: &Paths, p: &mut Script, ssh: &mut MockSsh, peers: Vec<Peer>) -> Outcome {
        let fab = fabric::Fabric::default();
        let mut find = move |_: &mut dyn Ssh| peers.clone();
        let mut health = |_: &str, _: u16| true;
        let mut net = Net { ssh, port: 7979, find: &mut find, health: &mut health };
        run_wizard(paths, &Config::default(), p, &fab, &mut net).unwrap()
    }

    #[test]
    fn wizard_adds_a_serving_host_by_hostname_and_marks_done() {
        let (_d, paths) = tmp("wizard");
        let peers = vec![
            peer(Some("ember"), &["10.100.132.2", "10.100.133.2"], Some("1.2.3"), Login::Ok),
            peer(Some("gigabyte"), &["10.100.128.1"], None, Login::Ok),
        ];
        let mut p = Script::new(&[
            "",                       // machine looks right
            "/tmp/lmdeck-weights",    // models dir
            "",                       // add ember
            "",                       // name: the hostname
            "tok123",                 // token
            "n",                      // gigabyte has no lmdeck: don't set it up
            "n",                      // nor add it anyway
            "github:someone/recipes", // source
            "",                       // done with sources
        ]);
        let out = wiz(&paths, &mut p, &mut MockSsh::default(), peers);
        assert_eq!(out.hosts, vec!["ember"]);
        assert_eq!(out.models_dir.as_deref(), Some("/tmp/lmdeck-weights"));
        assert_eq!(out.sources, vec![("recipes".to_string(), "github:someone/recipes".to_string())]);
        let hosts = HostsFile::load(&paths).unwrap();
        assert_eq!(hosts.hosts.len(), 1, "two cables, one host");
        let e = hosts.get("ember").unwrap();
        assert_eq!(e.url, "http://10.100.132.2:7979");
        assert_eq!(e.token.as_deref(), Some("tok123"));
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.contains("[setup]\ndone = true"), "{text}");
    }

    #[test]
    fn wizard_replaces_made_up_names_and_skips_known_hosts() {
        let (_d, paths) = tmp("wizard-dup");
        let mut hosts = HostsFile::default();
        hosts.add("spark-2", "http://10.100.132.2:7979", None).unwrap();
        hosts.add("spark-1", "http://10.100.133.2:7979", None).unwrap();
        hosts.add("gigabyte", "http://10.100.128.1:7979", Some("t".into())).unwrap();
        hosts.save(&paths).unwrap();
        let peers = vec![
            peer(Some("ember"), &["10.100.132.2", "10.100.133.2"], Some("1.2.3"), Login::Ok),
            peer(Some("gigabyte"), &["10.100.128.1"], Some("1.2.3"), Login::Ok),
        ];
        let mut p = Script::new(&["", "", "", "", "tok", ""]);
        let out = wiz(&paths, &mut p, &mut MockSsh::default(), peers);
        assert_eq!(out.hosts, vec!["ember"], "gigabyte is already there under its own name");
        let hosts = HostsFile::load(&paths).unwrap();
        let mut names: Vec<&str> = hosts.hosts.iter().map(|h| h.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["ember", "gigabyte"]);
    }

    #[test]
    fn wizard_never_invents_a_name() {
        let (_d, paths) = tmp("wizard-noname");
        let peers = vec![peer(None, &["10.100.136.1"], None, Login::Denied)];
        // Not set up, added anyway; the name prompt has no default, so empty skips it.
        let mut p = Script::new(&["", "", "n", "y", "", ""]);
        let out = wiz(&paths, &mut p, &mut MockSsh::default(), peers.clone());
        assert!(out.hosts.is_empty());
        assert!(HostsFile::load(&paths).unwrap().hosts.is_empty());
        // Typing one works.
        let (_d2, paths2) = tmp("wizard-noname2");
        let mut p = Script::new(&["", "", "n", "y", "faraday", ""]);
        let out = wiz(&paths2, &mut p, &mut MockSsh::default(), peers);
        assert_eq!(out.hosts, vec!["faraday"]);
    }

    #[test]
    fn wizard_sets_up_a_silent_peer_and_adds_it_by_its_hostname() {
        let (_d, paths) = tmp("wizard-boot");
        let ip = "10.100.136.1";
        let mut ssh = MockSsh { usable: vec!["id_ed25519".into()], copy_ok: true, ..Default::default() };
        ssh.reply(ip, crate::peers::FACTS_CMD, Out::fail("Permission denied (publickey,password)."));
        let facts = format!(
            "host=faraday\narch={}\nos={}\nuser=jvr0x\nhome=/home/jvr0x\nlmdeck=\nsystemd=yes\nlinger=yes\n",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
        ssh.reply(ip, crate::peers::FACTS_CMD, Out::ok(&facts));
        ssh.reply(ip, "lmdeck.new", Out::ok("lmdeck 0.1.0\n"));
        ssh.reply(ip, "serve.env", Out::ok(""));
        let peers = vec![peer(None, &[ip], None, Login::Denied)];
        let mut p = Script::new(&[
            "", "", // machine, models dir
            "", // set up lmdeck on 10.100.136.1: yes
            "", // ssh-copy-id: yes
            "", // service: yes
            "", // name: the hostname it reported
            "", // no sources
        ]);
        let out = wiz(&paths, &mut p, &mut ssh, peers);
        assert_eq!(ssh.copied, vec![(ip.to_string(), PathBuf::from("id_ed25519"))]);
        assert_eq!(out.hosts, vec!["faraday"]);
        let hosts = HostsFile::load(&paths).unwrap();
        let e = hosts.get("faraday").unwrap();
        assert_eq!(e.url, format!("http://{ip}:7979"));
        let tok = e.token.clone().unwrap();
        assert_eq!(tok.len(), 32);
        let sent = ssh.calls.iter().find(|(_, c, _)| c.contains("serve.env")).and_then(|(_, _, s)| s.clone()).unwrap();
        assert_eq!(sent, format!("LMDECK_TOKEN={tok}\n"), "the token goes over stdin");
        assert!(!ssh.calls.iter().any(|(_, c, _)| c.contains(&tok)), "never on a command line");
    }

    #[test]
    fn wizard_skips_silent_hosts_and_bad_sources() {
        let (_d, paths) = tmp("wizard2");
        let peers = vec![peer(Some("ember"), &["10.100.132.2"], None, Login::Ok)];
        let mut p = Script::new(&[
            "n",                  // machine doesn't look right: still continues
            "",                   // models dir: keep the default
            "n",                  // don't set up the silent host
            "n",                  // nor add it
            "not a spec at all!", // rejected
            "github:o/r",         // accepted
            "",                   // finish
        ]);
        let out = wiz(&paths, &mut p, &mut MockSsh::default(), peers);
        assert!(out.hosts.is_empty());
        assert_eq!(out.models_dir, None, "the default answer changes nothing");
        assert_eq!(out.sources.len(), 1);
        assert!(HostsFile::load(&paths).unwrap().hosts.is_empty());
    }

    #[test]
    fn wizard_takes_a_models_dir_and_keeps_the_default_on_garbage() {
        let (_d, paths) = tmp("wizard3");
        let mut p = Script::new(&[
            "",                  // machine looks right
            "$NOT_SET_ANYWHERE", // expands to nothing: rejected
            "/mnt/nvme/models",  // accepted
        ]);
        let out = wiz(&paths, &mut p, &mut MockSsh::default(), vec![]);
        assert_eq!(out.models_dir.as_deref(), Some("/mnt/nvme/models"));
        assert_eq!(out.hosts.len(), 0);

        // EOF after the confirm keeps the configured default.
        let (_d2, paths2) = tmp("wizard4");
        let mut p2 = Script::new(&[""]);
        let out = wiz(&paths2, &mut p2, &mut MockSsh::default(), vec![]);
        assert_eq!(out.models_dir, None);
    }

    #[test]
    fn the_summary_shows_cx7_ports_but_not_tailscale() {
        let hw = crate::hardware::Hardware::detect();
        let inv = crate::engine::Inventory::detect(&Config::default());

        let lines = machine_summary(&hw, &ring_fabric(), &inv);
        assert!(lines.iter().any(|l| l.contains("cxa")));
        assert!(!lines.iter().any(|l| l.contains("tailscale") || l.contains("100.102.")), "{lines:?}");
    }

    #[test]
    fn free_space_reports_the_filesystem_above_a_missing_dir() {
        let d = tempfile::TempDir::new().unwrap();
        let missing = d.path().join("a/b/c");
        assert!(!missing.exists());
        let gb = crate::hardware::free_gb(&missing).expect("df walks up to an existing ancestor");
        assert!(gb > 0.0, "{gb}");
    }
}
