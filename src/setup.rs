//! First-run setup: what lmdeck detected on this machine, the other machines
//! of the cluster (ConnectX-7 / RDMA neighbours, then a sweep of those
//! subnets, plus Tailscale peers), and which recipe sources to add. Runs once
//! before the harness prompt; `[setup] done` in config.toml keeps it from
//! asking again (`lmdeck host add` / `lmdeck source add` work any time).

use crate::config::Config;
use crate::fabric::{self, NicKind};
use crate::hosts::HostsFile;
use crate::paths::Paths;
use crate::sources::SourceSpec;
use crate::util::run_with_timeout;
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::process::Command;
use std::time::Duration;

/// The port `lmdeck serve` listens on by default; the scan probes it.
pub const SERVE_PORT: u16 = 7979;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_millis(1500);
const PROBE_CHUNK: usize = 64;

/// One machine the scan might be able to reach.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub ip: String,
    /// From Tailscale, when the peer has a hostname there.
    pub name: Option<String>,
    /// Where the candidate came from: `cx7 neighbour`, `cx7 sweep`, `tailscale`.
    pub via: &'static str,
}

/// What answered on one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub candidate: Candidate,
    /// An `lmdeck serve` answered `/api/health`.
    pub serving: bool,
    pub version: Option<String>,
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
}

/// Whether the first-run setup should run for this invocation.
pub fn should_run(cfg: &Config) -> bool {
    if cfg.setup.done || cfg.skipped.is_some() || std::env::var_os("LMDECK_NO_SYNC").is_some() {
        return false;
    }
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Detects this machine's summary and scans for cluster peers, then prompts.
pub fn wizard(paths: &Paths, cfg: &Config) -> Result<Outcome> {
    let fab = fabric::detect();
    let cands = gather_candidates(&fab);
    run_wizard(paths, cfg, &mut Terminal, SERVE_PORT, &fab, cands)
}

/// The whole wizard against any [`Prompt`]. `port` is a parameter so tests can
/// point the probe at a stub server.
pub fn run_wizard(
    paths: &Paths,
    cfg: &Config,
    p: &mut dyn Prompt,
    port: u16,
    fab: &fabric::Fabric,
    cands: Vec<Candidate>,
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
    println!("{}", crate::ui::dim("lmdeck pull downloads here; recipes read `<models_dir>/<repo>/<file>`."));
    println!("{}", crate::ui::dim("engines that keep their own hub cache (tensorfold, mlx, lmstudio) ignore this:"));
    println!("{}", crate::ui::dim("  set HF_HOME too, for those"));
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
        models_dir = v;
        break;
    }

    // The other machines: probe every candidate, then ask about each.
    println!("\n{}", crate::ui::bold("the cluster"));
    let found = probe_all(&cands, port);
    let (serving, silent): (Vec<&Found>, Vec<&Found>) = found.iter().partition(|f| f.serving);
    let mut hosts = HostsFile::load(paths).unwrap_or_default();
    let mut added = vec![];
    for f in &serving {
        let ver = f.version.as_deref().unwrap_or("?");
        let q = format!(
            "  lmdeck {ver} answers at {}. add it?",
            crate::ui::bold(&format!("http://{}:{port}", f.candidate.ip))
        );
        if !ask_yes(p, &q, true) {
            continue;
        }
        if let Some(name) = ask_host(p, &mut hosts, &f.candidate, port) {
            added.push(name);
        }
    }
    if !silent.is_empty() {
        let ips: Vec<String> = silent.iter().map(|f| f.candidate.ip.clone()).collect();
        println!(
            "{}",
            crate::ui::dim(&format!(
                "  nothing answered on :{port} at {}; those machines need `lmdeck serve`",
                ips.join(", ")
            ))
        );
        if ask_yes(p, "  add them anyway?", false) {
            for f in &silent {
                if let Some(name) = ask_host(p, &mut hosts, &f.candidate, port) {
                    added.push(name);
                }
            }
        }
    }
    if added.is_empty() {
        println!("{}", crate::ui::dim("no hosts added. `lmdeck host add <name> <url>` any time."));
    } else {
        hosts.save(paths)?;
        println!("{} hosts.toml: {}", crate::ui::green("✓"), added.join(", "));
    }

    // Recipe sources: repos to install from. No default; the built-ins are free.
    println!("\n{}", crate::ui::bold("recipe sources"));
    println!("{}", crate::ui::dim("repos to `lmdeck add <name>` from; the built-in recipes are always included."));
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

/// One add-a-host loop: name (with a default), optional token. Retries on a
/// rejected name. Returns the name when the host was added.
fn ask_host(p: &mut dyn Prompt, hosts: &mut HostsFile, c: &Candidate, port: u16) -> Option<String> {
    let default = c
        .name
        .clone()
        .or_else(|| reverse_name(&c.ip))
        .unwrap_or_else(|| format!("spark-{}", c.ip.rsplit('.').next().unwrap_or("0")));
    let url = format!("http://{}:{port}", c.ip);
    loop {
        let name = p
            .line(&format!("    name [{default}] > "))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default.clone());
        let token = p
            .line("    token (printed by `lmdeck serve` there) [none] > ")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match hosts.add(&name, &url, token) {
            Ok(_) => return Some(name),
            Err(e) => println!("  {} {e:#}", crate::ui::yellow("!")),
        }
    }
}

/// ` · 3.6 TB free`, or empty when the path cannot be measured.
fn space_hint(dir: &str) -> String {
    free_space_note(&crate::paths::expand(dir)).map(|s| format!(" · {s}")).unwrap_or_default()
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
    let gb = crate::hardware::free_gb(path)?;
    Some(if gb >= 1000.0 { format!("{:.1} TB", gb / 1000.0) } else { format!("{gb:.0} GB") })
}

fn ask_yes(p: &mut dyn Prompt, prompt: &str, default: bool) -> bool {
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
    let shown = fab.nics.iter().filter(|n| matches!(n.kind, NicKind::Cx7 | NicKind::Rdma | NicKind::Tailscale));
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

/// Machines worth probing: neighbours on the ConnectX-7 / RDMA ports (the
/// Spark-to-Spark links), every address in those subnets when the neighbour
/// table is silent, then `ts` (Tailscale peers as `(ip, hostname)`).
pub fn gather_candidates_with(fab: &fabric::Fabric, ts: Vec<(String, String)>) -> Vec<Candidate> {
    candidates_from(fab, ts, &neighbours_json())
}

/// [`gather_candidates_with`] with the machine's real Tailscale peers.
pub fn gather_candidates(fab: &fabric::Fabric) -> Vec<Candidate> {
    gather_candidates_with(fab, tailscale_peers())
}

/// Pure core of the scan: `neighbours_json` is the stdout of `ip -j neighbor`.
pub fn candidates_from(fab: &fabric::Fabric, ts: Vec<(String, String)>, neighbours_json: &str) -> Vec<Candidate> {
    let fast: Vec<&fabric::Nic> =
        fab.nics.iter().filter(|n| matches!(n.kind, NicKind::Cx7 | NicKind::Rdma) && n.state == "up").collect();
    let local: Vec<&str> = fab.nics.iter().flat_map(|n| n.ipv4.iter().map(|a| a.ip.as_str())).collect();
    let mut out: Vec<Candidate> = vec![];
    if !fast.is_empty() {
        let neighbours = parse_neighbours(neighbours_json, &fast, &local);
        for (ip, _) in &neighbours {
            out.push(Candidate { ip: ip.clone(), name: None, via: "cx7 neighbour" });
        }
        for nic in &fast {
            if neighbours.iter().any(|(_, dev)| dev == &nic.name) {
                continue;
            }
            for a in &nic.ipv4 {
                for ip in sweep(&a.ip, a.prefix) {
                    if local.contains(&ip.as_str()) {
                        continue;
                    }
                    out.push(Candidate { ip, name: None, via: "cx7 sweep" });
                }
            }
        }
    }
    for (ip, name) in ts {
        match out.iter_mut().find(|c| c.ip == ip) {
            Some(c) => c.name = c.name.take().or(Some(name)),
            None => out.push(Candidate { ip, name: Some(name), via: "tailscale" }),
        }
    }
    let mut seen: Vec<String> = vec![];
    out.retain(|c| {
        if seen.contains(&c.ip) {
            false
        } else {
            seen.push(c.ip.clone());
            true
        }
    });
    out
}

/// IPv4 neighbours (`ip -j neighbor`) on the given interfaces, minus this
/// machine's own addresses.
fn parse_neighbours(json: &str, fast: &[&fabric::Nic], local: &[&str]) -> Vec<(String, String)> {
    let Ok(arr) = serde_json::from_str::<Vec<Value>>(json) else { return vec![] };
    let names: Vec<&str> = fast.iter().map(|n| n.name.as_str()).collect();
    arr.iter()
        .filter_map(|it| {
            let ip = it.get("dst").and_then(Value::as_str)?.parse::<Ipv4Addr>().ok()?;
            if ip.is_loopback() || ip.is_multicast() || local.contains(&ip.to_string().as_str()) {
                return None;
            }
            let dev = it.get("dev").and_then(Value::as_str)?;
            names.contains(&dev).then(|| (ip.to_string(), dev.to_string()))
        })
        .collect()
}

/// Stdout of `ip -j neighbor`, empty when the command is missing.
fn neighbours_json() -> String {
    run_with_timeout(Command::new("ip").args(["-j", "neighbor"]), Duration::from_secs(4)).unwrap_or_default()
}

/// The other addresses in the same subnet (never the network, broadcast or
/// this machine). Subnets larger than /24 are skipped: too many blind probes.
fn sweep(ip: &str, prefix: u8) -> Vec<String> {
    let Ok(ip) = ip.parse::<Ipv4Addr>() else { return vec![] };
    if !(24..=31).contains(&prefix) {
        return vec![];
    }
    let bits = u32::from(ip);
    if prefix == 31 {
        // RFC 3021: both addresses are usable.
        return vec![Ipv4Addr::from(bits ^ 1).to_string()];
    }
    let mask = u32::MAX << (32 - u16::from(prefix));
    let net = bits & mask;
    let count = 1u64 << (32 - u16::from(prefix));
    (1..count - 1).map(|i| Ipv4Addr::from(net + i as u32)).filter(|a| *a != ip).map(|a| a.to_string()).collect()
}

/// Tailscale peers as `(ip, hostname)`; empty without tailscale.
fn tailscale_peers() -> Vec<(String, String)> {
    let Some(out) = run_with_timeout(Command::new("tailscale").args(["status", "--json"]), Duration::from_secs(4))
    else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(&out) else { return vec![] };
    let peers = match v.get("Peer").or_else(|| v.get("Peers")) {
        Some(Value::Object(m)) => m.values().cloned().collect::<Vec<_>>(),
        Some(Value::Array(a)) => a.clone(),
        _ => return vec![],
    };
    peers
        .iter()
        .filter_map(|p| {
            let ip = p.get("TailscaleIPs").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str)?;
            let name = p.get("HostName").and_then(Value::as_str).filter(|n| !n.is_empty())?;
            Some((ip.to_string(), name.to_string()))
        })
        .collect()
}

/// Reverse DNS for a name default (`getent hosts`).
fn reverse_name(ip: &str) -> Option<String> {
    let out = run_with_timeout(Command::new("getent").args(["hosts", ip]), Duration::from_secs(2))?;
    out.split_whitespace().nth(1).map(str::to_string)
}

/// Is there an `lmdeck serve` on this address? `(serving, version)`.
pub fn probe_one(ip: &str, port: u16) -> (bool, Option<String>) {
    let Ok(addr) = format!("{ip}:{port}").parse::<std::net::SocketAddr>() else {
        return (false, None);
    };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else {
        return (false, None);
    };
    let _ = s.set_read_timeout(Some(READ_TIMEOUT));
    let _ = s.set_write_timeout(Some(READ_TIMEOUT));
    let req =
        format!("GET /api/health HTTP/1.1\r\nHost: {ip}\r\nConnection: close\r\nUser-Agent: lmdeck-setup\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return (false, None);
    }
    let mut buf = Vec::new();
    let _ = s.take(64 * 1024).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    match serde_json::from_str::<Value>(body.trim()) {
        Ok(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => {
            (true, v.get("version").and_then(Value::as_str).map(str::to_string))
        }
        _ => (false, None),
    }
}

/// Probes every candidate, a chunk at a time.
pub fn probe_all(cands: &[Candidate], port: u16) -> Vec<Found> {
    std::thread::scope(|s| {
        let handles: Vec<_> = cands
            .chunks(PROBE_CHUNK)
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .map(|c| {
                            let (serving, version) = probe_one(&c.ip, port);
                            Found { candidate: c.clone(), serving, version }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::Nic;
    use std::collections::VecDeque;

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
    }

    #[test]
    fn neighbours_filter_keeps_fast_interfaces_and_drops_ours() {
        let nics = [nic("cx", NicKind::Cx7, "up", &[("10.0.0.1", 31)])];
        let fast: Vec<&fabric::Nic> = nics.iter().collect();
        let json = r#"[
            {"dst": "10.0.0.2", "dev": "cx", "lladdr": "aa"},
            {"dst": "192.168.1.4", "dev": "eno1"},
            {"dst": "10.0.0.1", "dev": "cx"},
            {"dst": "not-an-ip", "dev": "cx"},
            {"dev": "cx"}
        ]"#;
        let got = parse_neighbours(json, &fast, &["10.0.0.1"]);
        assert_eq!(got, vec![("10.0.0.2".to_string(), "cx".to_string())]);
    }

    #[test]
    fn candidates_cx7_then_tailscale_no_duplicates() {
        let fab = fabric::Fabric {
            nics: vec![
                nic("lo", NicKind::Loopback, "unknown", &[("127.0.0.1", 8)]),
                nic("eth0", NicKind::Ethernet, "up", &[("192.168.1.50", 24)]),
                nic("ts0", NicKind::Tailscale, "unknown", &[("100.64.0.1", 32)]),
                nic("cx", NicKind::Cx7, "up", &[("10.0.0.1", 31)]),
            ],
            ..Default::default()
        };
        let nb = r#"[{"dst": "10.0.0.2", "dev": "cx"}]"#;
        // The neighbour is found, so no sweep; own and non-fast addresses never appear.
        let got = candidates_from(&fab, vec![("100.64.0.9".into(), "peer1".into())], nb);
        assert_eq!(
            got,
            vec![
                Candidate { ip: "10.0.0.2".into(), name: None, via: "cx7 neighbour" },
                Candidate { ip: "100.64.0.9".into(), name: Some("peer1".into()), via: "tailscale" },
            ]
        );
        // Silent neighbour table: the /31 peer address is swept, and a
        // tailscale peer on the same address merges into it with a name.
        let got =
            candidates_from(&fab, vec![("10.0.0.0".into(), "spark2".into())], r#"[{"dst": "10.0.0.1", "dev": "cx"}]"#);
        assert_eq!(got, vec![Candidate { ip: "10.0.0.0".into(), name: Some("spark2".into()), via: "cx7 sweep" },]);
        // No fast NICs: nothing but the passed peers.
        let mut fab2 = fab.clone();
        fab2.nics.retain(|n| n.kind != NicKind::Cx7);
        assert_eq!(candidates_from(&fab2, vec![], "").len(), 0);
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
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true,\"version\":\"9.9.9\"}\n",
        );
        let (serving, version) = probe_one("127.0.0.1", good.port);
        assert!(serving);
        assert_eq!(version.as_deref(), Some("9.9.9"));

        let junk = Stub::start("HTTP/1.1 200 OK\r\n\r\n<html>not lmdeck</html>");
        assert!(!probe_one("127.0.0.1", junk.port).0);

        // Nothing listening on a likely-free port.
        assert!(!probe_one("127.0.0.1", 1).0);
    }

    #[test]
    fn probe_all_walks_every_candidate() {
        let good = Stub::start("HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}");
        let cands = vec![
            Candidate { ip: "127.0.0.1".into(), name: None, via: "cx7 neighbour" },
            Candidate { ip: "10.255.255.1".into(), name: None, via: "cx7 sweep" },
        ];
        let found = probe_all(&cands, good.port);
        assert_eq!(found.len(), 2);
        assert!(found[0].serving);
        assert!(!found[1].serving);
    }

    /// Scripted answers for the wizard.
    struct Script(VecDeque<String>);

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

    #[test]
    fn wizard_adds_serving_host_and_source_then_marks_done() {
        let (_d, paths) = tmp("wizard");
        let good = Stub::start("HTTP/1.1 200 OK\r\n\r\n{\"ok\":true,\"version\":\"1.2.3\"}");
        let cands = vec![
            Candidate { ip: "127.0.0.1".into(), name: Some("spark2".into()), via: "cx7 neighbour" },
            Candidate { ip: "10.255.255.1".into(), name: None, via: "tailscale" },
        ];
        let mut p = Script(
            [
                "",                       // machine looks right
                "/tmp/lmdeck-weights",    // models dir
                "",                       // add 127.0.0.1 (default name spark2)
                "",                       // name: default
                "tok123",                 // token
                "n",                      // 10.255.255.1: nothing answered; don't add
                "github:someone/recipes", // source
                "",                       // done with sources
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
        let fab = fabric::Fabric::default();
        let cfg = Config::default();
        let out = run_wizard(&paths, &cfg, &mut p, good.port, &fab, cands).unwrap();
        assert_eq!(out.hosts, vec!["spark2"]);
        assert_eq!(out.models_dir.as_deref(), Some("/tmp/lmdeck-weights"));
        assert_eq!(out.sources, vec![("recipes".to_string(), "github:someone/recipes".to_string())]);
        let hosts = HostsFile::load(&paths).unwrap();
        let e = hosts.get("spark2").unwrap();
        assert_eq!(e.url, format!("http://127.0.0.1:{}", good.port));
        assert_eq!(e.token.as_deref(), Some("tok123"));
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.contains("[setup]\ndone = true"), "{text}");
    }

    #[test]
    fn wizard_skips_silent_hosts_and_bad_sources() {
        let (_d, paths) = tmp("wizard2");
        let dead = Stub::start("HTTP/1.1 200 OK\r\n\r\n{\"ok\":false}"); // answers, but not lmdeck
        let cands = vec![Candidate { ip: "127.0.0.1".into(), name: None, via: "cx7 sweep" }];
        let mut p = Script(
            [
                "n",                  // machine doesn't look right: still continues
                "",                   // models dir: keep the default
                "n",                  // don't add the silent host
                "not a spec at all!", // rejected
                "github:o/r",         // accepted
                "",                   // finish
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
        let fab = fabric::Fabric::default();
        let out = run_wizard(&paths, &Config::default(), &mut p, dead.port, &fab, cands).unwrap();
        assert!(out.hosts.is_empty());
        assert_eq!(out.models_dir, None, "the default answer changes nothing");
        assert_eq!(out.sources.len(), 1);
        assert!(HostsFile::load(&paths).unwrap().hosts.is_empty());
    }

    #[test]
    fn wizard_takes_a_models_dir_and_keeps_the_default_on_garbage() {
        let (_d, paths) = tmp("wizard3");
        let cands: Vec<Candidate> = vec![];
        let mut p = Script(
            [
                "",                  // machine looks right
                "$NOT_SET_ANYWHERE", // expands to nothing: rejected
                "/mnt/nvme/models",  // accepted
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
        let fab = fabric::Fabric::default();
        let out = run_wizard(&paths, &Config::default(), &mut p, 1, &fab, cands).unwrap();
        assert_eq!(out.models_dir.as_deref(), Some("/mnt/nvme/models"));
        assert_eq!(out.hosts.len(), 0);

        // EOF after the confirm keeps the configured default.
        let (_d2, paths2) = tmp("wizard4");
        let mut p2 = Script([String::from("")].into_iter().collect());
        let out = run_wizard(&paths2, &Config::default(), &mut p2, 1, &fab, vec![]).unwrap();
        assert_eq!(out.models_dir, None);
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
