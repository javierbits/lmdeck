//! Running a recipe on one host with its weights on another, without
//! copying them: the weights home exports its Hugging Face cache read-only
//! over NFS on the ConnectX-7 link, and the run host mounts it as a docker
//! NFS volume (no sudo there). This mirrors Mia's `WORKER_WEIGHTS=nfs`
//! (GLM-5.3-Flash-EXL3-2x-DGX-Sparks-TensorFold `scripts/nodes.sh`):
//!
//! 1. home: `/etc/exports` gets `<path> <run host CX7 addr>(ro,no_subtree_check)`
//!    and `sudo exportfs -ra`. lmdeck only checks; it runs these (with
//!    `sudo -n`) when `[weights] allow_export = true`, and otherwise stops
//!    and prints them.
//! 2. run host: `docker volume create --driver local --opt type=nfs …` (an
//!    existing volume is inspected and must point at the same export).
//! 3. preflight: a throwaway container checks `config.json` through the
//!    volume and the file list (`find -L … -printf '%P %s\n'`) must match
//!    the home's.
//! 4. start with the volume mounted at `/root/.cache/huggingface:ro`.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config::Config;
use crate::deck::Deck;
use crate::fabric::{Link, LinkKind};
use crate::hosts::{HostsFile, LOCAL};
use crate::ops::Target;
use crate::paths::Paths;
use crate::recipe::{Engine, NfsMount, Recipe};
use crate::runner::RunRecord;
use crate::weights::Index;

/// CX7 line rate when the port speed couldn't be read (labelled nominal).
pub const NOMINAL_CX7_GBPS: f64 = 200.0;

/// Mia's mount options, verbatim.
pub const NFS_OPTS: &str = "nfsvers=4.2,ro,nconnect=8,rsize=1048576,wsize=1048576,hard,timeo=600";

/// A placement that is (or was) running: what topology highlights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub recipe: String,
    pub run_host: String,
    pub home: String,
    /// The name it runs as on the run host.
    pub served_as: String,
    pub volume: String,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("placements")
}

impl Record {
    pub fn all(paths: &Paths) -> Vec<Record> {
        let Ok(rd) = fs::read_dir(dir(paths)) else { return vec![] };
        let mut v: Vec<Record> = rd
            .flatten()
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        v.sort_by(|a, b| a.recipe.cmp(&b.recipe));
        v
    }

    pub fn load(paths: &Paths, recipe: &str) -> Option<Record> {
        fs::read_to_string(dir(paths).join(format!("{recipe}.json"))).ok().and_then(|t| serde_json::from_str(&t).ok())
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fs::create_dir_all(dir(paths))?;
        fs::write(dir(paths).join(format!("{}.json", self.recipe)), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn remove(paths: &Paths, recipe: &str) {
        let _ = fs::remove_file(dir(paths).join(format!("{recipe}.json")));
    }

    /// How long ago this placement was written (the start that sent it away).
    pub fn age(paths: &Paths, recipe: &str) -> Option<std::time::Duration> {
        fs::metadata(dir(paths).join(format!("{recipe}.json"))).ok()?.modified().ok()?.elapsed().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    pub bytes: u64,
    pub gbps: f64,
    /// True when `gbps` is the CX7 line rate, not a measured port speed.
    pub nominal: bool,
    pub secs: f64,
}

impl Estimate {
    pub fn new(bytes: u64, gbps: f64, nominal: bool) -> Estimate {
        Estimate { bytes, gbps, nominal, secs: bytes as f64 * 8.0 / (gbps * 1e9) }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} to stream {} at {} Gb/s{}",
            human_secs(self.secs),
            crate::util::human_bytes(self.bytes),
            self.gbps,
            if self.nominal { " (nominal CX7 rate; port speed unknown)" } else { "" }
        )
    }
}

/// `≈N s` / `≈N.N min`, or `under 1 s`.
pub fn human_secs(s: f64) -> String {
    if s < 1.0 {
        "under 1 s".into()
    } else if s < 90.0 {
        format!("≈{s:.0} s")
    } else {
        format!("≈{:.1} min", s / 60.0)
    }
}

/// The NFS half of a plan: every command, as text, in order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Nfs {
    pub home: String,
    pub volume: String,
    /// The home's address on the link (the NFS server).
    pub server: String,
    /// The run host's address on the link (the export's client).
    pub client: String,
    pub export_path: String,
    pub mount_at: String,
    pub repo: String,
    pub commit: Option<String>,
    /// Path of the snapshot inside the volume.
    pub snapshot_rel: String,
    pub export_line: String,
    pub export_cmds: Vec<String>,
    pub volume_cmd: String,
    pub preflight_cmds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub recipe: String,
    pub run_host: String,
    /// `local` (weights on the run host), `nfs`, or `none` (nothing mounted:
    /// no host has them, or `home` does and the recipe's own scripts fetch them).
    pub mode: String,
    pub home: Option<String>,
    pub link: Option<Link>,
    pub nfs: Option<Nfs>,
    pub estimate: Option<Estimate>,
    pub run_mem_gb: Option<f64>,
    pub need_gb: Option<f64>,
    /// The recipe's peers (`[hardware] hosts`), picked around the run host on
    /// this machine's fabric graph: each one's name and the CX7 address the
    /// run host reaches it on, in rank order. Empty when it names no peer.
    pub peers: Vec<(String, String)>,
    /// With peers and `command_by_fabric`: the CX7 fabric shape this machine
    /// sees, and the command it picks for the run host (shell-quoted).
    pub fabric: Option<(String, String)>,
    /// The env entries this machine fills for the run host (the peers, and
    /// `{fabric}` with them), as a local plan shows them: `KEY=value`, or
    /// `KEY` alone for a secret or an engine other than `custom`.
    pub env: Vec<String>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn describe(&self, node: &str) -> Vec<String> {
        let show = |h: &str| if h == LOCAL { format!("{node} (local)") } else { h.to_string() };
        let mut out = vec![format!("{} on {}", self.recipe, show(&self.run_host))];
        match (&self.mode[..], &self.home) {
            ("local", _) => out.push("weights: on the run host's disk".into()),
            ("nfs", Some(h)) => out.push(format!("weights: home {}, streamed read-only over NFS (no copy)", show(h))),
            ("none", Some(h)) => {
                out.push(format!("weights: on {}, not mounted (the recipe's own scripts fetch them)", show(h)))
            }
            _ => out.push("weights: not on any known host".into()),
        }
        if let Some(l) = &self.link {
            out.push(format!("link: {}", l.label()));
        }
        if let Some(e) = &self.estimate {
            out.push(format!("load: {}", e.describe()));
        }
        if let (Some(need), Some(have)) = (self.need_gb, self.run_mem_gb) {
            out.push(format!("fit: needs ~{need:.0} GB, {} has {have:.0} GB", show(&self.run_host)));
        }
        out.extend(self.env.iter().map(|e| format!("env:  {e}")));
        if !self.peers.is_empty() {
            let list: Vec<String> =
                self.peers.iter().enumerate().map(|(i, (h, a))| format!("{}. {h} {a}", i + 1)).collect();
            out.push(format!("peers: {}", list.join(", ")));
        }
        if let Some((shape, command)) = &self.fabric {
            out.push(format!("fabric: {shape} (as {node} sees it), so {} runs {command}", show(&self.run_host)));
        }
        if let Some(n) = &self.nfs {
            out.push(format!("1. on {} (check /etc/exports; root):", show(&n.home)));
            out.extend(n.export_cmds.iter().map(|c| format!("     {c}")));
            out.push(format!("2. on {} (no sudo):", show(&self.run_host)));
            out.push(format!("     {}", n.volume_cmd));
            out.push("3. preflight:".into());
            out.extend(n.preflight_cmds.iter().map(|c| format!("     {c}")));
            out.push(format!("4. start with -v {}:{}:ro -e HF_HUB_OFFLINE=1", n.volume, n.mount_at));
        }
        for w in &self.warnings {
            out.push(format!("warning: {w}"));
        }
        out
    }
}

pub fn volume_name(home: &str) -> String {
    let h: String = home.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
    format!("lmdeck-hf-{h}")
}

pub fn export_line(path: &str, client: &str) -> String {
    format!("{path} {client}(ro,no_subtree_check)")
}

pub fn export_cmds(path: &str, client: &str) -> Vec<String> {
    vec![format!("echo '{}' | sudo tee -a /etc/exports", export_line(path, client)), "sudo exportfs -ra".into()]
}

pub fn volume_argv(docker: &str, volume: &str, server: &str, path: &str) -> Vec<String> {
    [
        docker,
        "volume",
        "create",
        "--driver",
        "local",
        "--opt",
        "type=nfs",
        "--opt",
        &format!("device=:{path}"),
        "--opt",
        &format!("o=addr={server},{NFS_OPTS}"),
        volume,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn preflight_argv(docker: &str, volume: &str, image: &str, rel: &str) -> (Vec<String>, Vec<String>) {
    let at = format!("/hf/{}", rel.trim_start_matches('/'));
    let v = format!("{volume}:/hf:ro");
    let s = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    (
        s(&[docker, "run", "--rm", "--entrypoint", "test", "-v", &v, image, "-f", &format!("{at}/config.json")]),
        s(&[
            docker,
            "run",
            "--rm",
            "--entrypoint",
            "find",
            "-v",
            &v,
            image,
            "-L",
            &at,
            "-type",
            "f",
            "-printf",
            "%P %s\\n",
        ]),
    )
}

pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where `/etc/exports` is read from (`LMDECK_EXPORTS_FILE` exists for tests).
fn exports_files() -> Vec<PathBuf> {
    if let Some(p) = std::env::var_os("LMDECK_EXPORTS_FILE") {
        return vec![PathBuf::from(p)];
    }
    let mut v = vec![PathBuf::from("/etc/exports")];
    if let Ok(rd) = fs::read_dir("/etc/exports.d") {
        v.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "exports")));
    }
    v
}

/// Whether an exports(5) text exports `path` to `client` (exact address,
/// `*`, or a CIDR containing it).
pub fn exports_has(text: &str, path: &str, client: &str) -> bool {
    let ip: Option<std::net::Ipv4Addr> = client.parse().ok();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_whitespace();
        let Some(p) = parts.next() else { continue };
        if p.trim_matches('"').trim_end_matches('/') != path.trim_end_matches('/') {
            continue;
        }
        for spec in parts {
            let who = spec.split('(').next().unwrap_or("");
            if who == client || who == "*" {
                return true;
            }
            if let (Some(ip), Some((net, bits))) = (ip, who.split_once('/')) {
                if let (Ok(net), Ok(bits)) = (net.parse::<std::net::Ipv4Addr>(), bits.parse::<u32>()) {
                    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits.min(32)) };
                    if u32::from(ip) & mask == u32::from(net) & mask {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Home side: is `path` exported to `client`? If not, run the root steps
/// only when `[weights] allow_export` says so; otherwise fail with them.
pub fn ensure_export(cfg: &Config, path: &str, client: &str) -> Result<String> {
    if client.parse::<std::net::IpAddr>().is_err() {
        bail!("export client must be an IP address, got '{client}'");
    }
    let text: String = exports_files().iter().filter_map(|p| fs::read_to_string(p).ok()).collect::<Vec<_>>().join("\n");
    if exports_has(&text, path, client) {
        return Ok(format!("{path} is already exported to {client}"));
    }
    let cmds = export_cmds(path, client);
    if !cfg.weights.allow_export {
        bail!(
            "{path} is not exported to {client}. Run this once on the weights home (or set [weights] allow_export = true so lmdeck runs it with sudo -n):\n  {}",
            cmds.join("\n  ")
        );
    }
    let line = export_line(path, client);
    let mut tee = Command::new("sudo")
        .args(["-n", "tee", "-a", "/etc/exports"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("sudo")?;
    use std::io::Write;
    tee.stdin.take().context("sudo stdin")?.write_all(format!("{line}\n").as_bytes())?;
    if !tee.wait()?.success() {
        bail!("`sudo -n tee -a /etc/exports` failed (passwordless sudo needed); run by hand:\n  {}", cmds.join("\n  "));
    }
    let st = Command::new("sudo").args(["-n", "exportfs", "-ra"]).status().context("sudo exportfs")?;
    if !st.success() {
        bail!("`sudo -n exportfs -ra` failed");
    }
    Ok(format!("added `{line}` to /etc/exports and ran exportfs -ra"))
}

fn docker_out(argv: &[String]) -> Result<(bool, String, String)> {
    let o = Command::new(&argv[0]).args(&argv[1..]).output().with_context(|| format!("running {}", argv[0]))?;
    Ok((
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    ))
}

/// Run-host side: create the NFS volume, or check an existing one points at
/// the same export.
pub fn ensure_volume(docker: &str, volume: &str, server: &str, path: &str) -> Result<String> {
    if volume.is_empty() || !volume.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        bail!("bad volume name '{volume}'");
    }
    let (ok, out, _) = docker_out(&[
        docker.into(),
        "volume".into(),
        "inspect".into(),
        "--format".into(),
        "{{json .Options}}".into(),
        volume.into(),
    ])?;
    if ok {
        let opts: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_default();
        let device = opts.get("device").and_then(|v| v.as_str()).unwrap_or("");
        let o = opts.get("o").and_then(|v| v.as_str()).unwrap_or("");
        let addr_ok = o.split(',').any(|kv| kv == format!("addr={server}"));
        if device != format!(":{path}") || !addr_ok {
            bail!(
                "docker volume {volume} exists but points at {device} (o={o}), not {server}:{path}; remove it with `docker volume rm {volume}` and retry"
            );
        }
        return Ok(format!("docker volume {volume} already mounts {server}:{path}"));
    }
    let argv = volume_argv(docker, volume, server, path);
    let (ok, _, err) = docker_out(&argv)?;
    if !ok {
        bail!("{} failed: {}", shell_join(&argv), err.trim());
    }
    Ok(format!("created docker volume {volume} → {server}:{path} (ro)"))
}

/// Run-host side: what a container sees through the volume.
pub fn preflight(docker: &str, volume: &str, image: &str, rel: &str) -> Result<Vec<(String, u64)>> {
    if rel.split('/').any(|s| s == "..") {
        bail!("bad snapshot path");
    }
    let (test, find) = preflight_argv(docker, volume, image, rel);
    let (ok, _, err) = docker_out(&test)?;
    if !ok {
        bail!(
            "preflight: config.json is not visible through volume {volume} ({}); check the export and the NFS server on the home",
            err.trim().lines().last().unwrap_or("test -f failed")
        );
    }
    let (ok, out, err) = docker_out(&find)?;
    if !ok {
        bail!("preflight file list failed: {}", err.trim());
    }
    Ok(crate::weights::parse_file_list(&out))
}

/// The fast-link reachability rule: CX7 (or another direct link) or nothing,
/// unless the caller accepts a slow link.
pub fn require_link<'a>(
    link: Option<&'a Link>,
    a: &str,
    b: &str,
    allow_slow: bool,
) -> Result<(&'a Link, Option<String>)> {
    match link {
        Some(l) if l.kind.is_direct() => Ok((l, None)),
        Some(l) if allow_slow => Ok((
            l,
            Some(format!(
                "only a {} link between {a} and {b}: weights will stream at LAN speed (no ConnectX-7 link found)",
                l.kind.as_str()
            )),
        )),
        Some(l) => bail!(
            "no ConnectX-7 link between {a} and {b} (only {}): refusing to stream weights over it. \
             Cable the CX7 ports, set link_addr / a [[link]] in hosts.toml, or pass --allow-slow-link",
            l.label()
        ),
        None => bail!(
            "no link found between {a} and {b} (is `lmdeck serve` up on both, and do they share a subnet?); \
             set link_addr or a [[link]] in hosts.toml"
        ),
    }
}

/// Picks where the weights come from for `r` on `run_host`, and the peers
/// around it when the recipe's scripts drive several Sparks.
pub fn plan(paths: &Paths, cfg: &Config, r: &Recipe, run_host: &str, allow_slow: bool) -> Result<Plan> {
    let node = cfg.node_name();
    let run_key = if crate::remote::is_local(run_host, &node) { LOCAL.to_string() } else { run_host.to_string() };
    crate::remote::check_host(paths, &run_key, &node)?;
    // Reason: the run host may hold no hosts.toml, so this machine picks its peers on its
    // own graph, and too few fail here, before the run host is asked to start anything.
    let peers = crate::pick::peers(paths, cfg, r, &run_key)?;
    let shape = if peers.hosts.is_empty() { None } else { crate::topology::fabric_for(paths, cfg, r) };
    let fabric = shape.map(|s| {
        let argv = r.engine.command_by_fabric.get(s.kind.as_str()).unwrap_or(&r.engine.command);
        let line = shlex::try_join(argv.iter().map(String::as_str)).unwrap_or_else(|_| argv.join(" "));
        (s.kind.as_str().to_string(), line)
    });
    let idx = Index::fresh(paths, cfg, crate::weights::INDEX_TTL);
    let locs = idx.locate(r);
    let mut p = Plan {
        recipe: r.name.clone(),
        run_host: run_key.clone(),
        mode: "none".into(),
        home: None,
        link: None,
        nfs: None,
        estimate: None,
        run_mem_gb: None,
        need_gb: r.hardware.min_memory_gb.or(r.model.size_gb.map(|g| g * 1.2)),
        peers: peers.hosts,
        fabric,
        env: vec![],
        warnings: peers.warnings,
    };
    // Only the env filled here: `{port}`, `{hf_home}` and the like are the run host's.
    let shown = |k: &str| r.engine.name == Engine::Custom && !k.contains("TOKEN") && !k.contains("KEY");
    let placed = placed_recipe(r, &p);
    p.env = placed
        .engine
        .env
        .iter()
        .filter(|(k, v)| r.engine.env.get(*k) != Some(*v))
        .map(|(k, v)| if shown(k) { format!("{k}={v}") } else { k.clone() })
        .collect();
    p.run_mem_gb = run_memory(paths, &run_key);
    if let (Some(need), Some(have)) = (p.need_gb, p.run_mem_gb) {
        if need > have {
            p.warnings.push(format!("needs ~{need:.0} GB but {run_key} has {have:.0} GB"));
        }
    }
    if locs.iter().any(|l| l.host == run_key) {
        p.mode = "local".into();
        p.home = Some(run_key);
        return Ok(p);
    }
    let Some(home) = idx.home(r, &node) else {
        p.warnings.push("no host has these weights; the run host will download them if the engine does".into());
        return Ok(p);
    };
    if r.model.fetched_by_scripts() {
        // Reason: `pull = false` says the recipe's own scripts fetch its weights where it
        // runs, as they did before it named them: lmdeck mounts nothing and only says so.
        let name = |h: &str| if h == LOCAL { node.clone() } else { h.to_string() };
        p.warnings.push(format!(
            "{} doesn't hold the weights ({} has them, {}): the recipe's own scripts fetch them there",
            name(&run_key),
            name(&home.host),
            crate::util::human_bytes(home.size)
        ));
        p.home = Some(home.host);
        return Ok(p);
    }
    if home.kind != "hf" || r.model.repo.is_none() {
        bail!(
            "{}'s weights on {} are in the models dir ({}); NFS placement streams the Hugging Face cache only. Mount it by hand or run on {}",
            r.name,
            home.host,
            home.path,
            home.host
        );
    }
    let docker = r.engine.image.is_some() && matches!(r.engine.name.as_str(), "vllm" | "sglang" | "lmdeploy");
    if !docker {
        bail!(
            "{} isn't a docker recipe, so lmdeck can't mount the weights for it on {run_key}; run it on {} (where the weights are) or mount {} there by hand",
            r.name,
            home.host,
            home.path
        );
    }
    let inv = idx.inventory(&home.host).context("weights home inventory")?.clone();
    let graph = crate::topology::graph_for(paths, cfg, &[home.host.clone(), run_key.clone()]);
    let best = crate::fabric::path_as_link(&graph, &home.host, &run_key);
    let (link, warn) = require_link(best.as_ref(), &home.host, &run_key, allow_slow)?;
    let link = link.clone();
    p.warnings.extend(warn);
    let (h_end, r_end) = link.sides(&home.host).ok_or_else(|| anyhow!("link sides"))?;
    let (server, client) = (h_end.ip.clone(), r_end.ip.clone());
    let repo = r.model.repo.clone().unwrap_or_default();
    let item = crate::weights::find(&inv, r);
    let commit = item.and_then(|it| {
        let want = r.model.revision.as_deref().unwrap_or("main");
        it.revisions
            .iter()
            .find(|v| v.commit == want || v.refs.iter().any(|x| x == want))
            .or(it.revisions.first())
            .map(|v| v.commit.clone())
    });
    let volume = volume_name(&if home.host == LOCAL { node.clone() } else { home.host.clone() });
    let hub = if inv.hub_rel.is_empty() { String::new() } else { format!("{}/", inv.hub_rel) };
    let snapshot_rel = format!(
        "{hub}models--{}/snapshots/{}",
        repo.replace('/', "--"),
        commit.clone().unwrap_or_else(|| "<rev>".into())
    );
    let image = r.engine.image.clone().unwrap_or_default();
    let d = &cfg.engines.docker.clone().unwrap_or_else(|| "docker".into());
    let (t, f) = preflight_argv(d, &volume, &image, &snapshot_rel);
    let gbps = link.gbps().filter(|g| *g > 0.0);
    p.estimate = match (gbps, link.kind) {
        (Some(g), _) => Some(Estimate::new(home.size, g, false)),
        (None, LinkKind::Cx7) => Some(Estimate::new(home.size, NOMINAL_CX7_GBPS, true)),
        _ => None,
    };
    p.nfs = Some(Nfs {
        home: home.host.clone(),
        volume: volume.clone(),
        server: server.clone(),
        client: client.clone(),
        export_path: inv.export_path.clone(),
        mount_at: inv.mount_at.clone(),
        repo,
        commit,
        snapshot_rel,
        export_line: export_line(&inv.export_path, &client),
        export_cmds: export_cmds(&inv.export_path, &client),
        volume_cmd: shell_join(&volume_argv(d, &volume, &server, &inv.export_path)),
        preflight_cmds: vec![shell_join(&t), shell_join(&f)],
    });
    p.mode = "nfs".into();
    p.home = Some(home.host);
    p.link = Some(link);
    Ok(p)
}

fn run_memory(paths: &Paths, key: &str) -> Option<f64> {
    if key == LOCAL {
        return Some(crate::hardware::Hardware::detect().total_memory_gb).filter(|g| *g > 0.0);
    }
    let c = crate::remote::Client::named(paths, key).ok()?;
    let v: serde_json::Value = c.get("telemetry").ok()?;
    v.pointer("/telemetry/mem_total_gb").and_then(|m| m.as_f64()).filter(|g| *g > 0.0)
}

/// The recipe as the run host should start it: the peers this machine picked
/// filled in (with them, the command for this machine's view of the fabric),
/// and the NFS volume mounted.
pub fn placed_recipe(r: &Recipe, plan: &Plan) -> Recipe {
    let mut out = r.clone();
    let mut vars = crate::vars::peer_vars(&plan.peers);
    if let Some((shape, _)) = &plan.fabric {
        // Reason: a run host without hosts.toml sees itself alone (`single`) and would
        // run the wrong launcher; the shape and its command travel picked instead.
        if let Some(argv) = out.engine.command_by_fabric.remove(shape) {
            out.engine.command = argv;
        }
        out.engine.command_by_fabric.clear();
        vars.insert("fabric".into(), shape.clone());
    }
    out.engine.fill(&vars);
    if let Some(n) = &plan.nfs {
        out.model.nfs_volume = Some(NfsMount { volume: n.volume.clone(), mount_at: n.mount_at.clone() });
        out.model.home = None;
    }
    out
}

/// The NFS steps for one client, in order. Returns progress lines.
pub fn prepare_nfs(deck: &Deck, n: &Nfs, run: &Target, image: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let (_, home) = Target::of(deck, &n.home)?;
    out.push(home.ensure_export(deck, &n.client)?);
    out.push(run.ensure_volume(deck, &n.volume, &n.server, &n.export_path)?);
    let (commit, want) = home.snapshot_files(&n.repo, n.commit.as_deref())?;
    let rel = match &n.commit {
        Some(c) if *c == commit => n.snapshot_rel.clone(),
        _ => n.snapshot_rel.rsplit_once('/').map(|(a, _)| format!("{a}/{commit}")).unwrap_or_default(),
    };
    let got = run.preflight(deck, &n.volume, image, &rel)?;
    let diff = crate::weights::compare_lists(&want, &got);
    if !diff.is_empty() {
        bail!(
            "preflight: the volume's file list differs from the home's ({} problem(s)): {}",
            diff.len(),
            diff.iter().take(5).cloned().collect::<Vec<_>>().join("; ")
        );
    }
    out.push(format!("preflight ok: {} files match through {}", got.len(), n.volume));
    Ok(out)
}

/// Starts `r` on `plan.run_host`. Returns progress lines.
pub fn execute(deck: &Deck, r: &Recipe, plan: &Plan, only: bool, force: bool) -> Result<Vec<String>> {
    let (_, run) = Target::of(deck, &plan.run_host)?;
    let mut out = vec![];
    // The peers filled in, by name: the run host takes them as its Sparks.
    let names: Vec<String> = plan.peers.iter().map(|(h, _)| h.clone()).collect();
    let picked = (!names.is_empty()).then_some(names.as_slice());
    if let Some(n) = &plan.nfs {
        out.extend(prepare_nfs(deck, n, &run, r.engine.image.as_deref().unwrap_or_default())?);
        let placed = placed_recipe(r, plan);
        out.push(run.start_recipe(deck, &placed, picked, only, force)?);
        Record {
            recipe: r.name.clone(),
            run_host: plan.run_host.clone(),
            home: n.home.clone(),
            served_as: r.name.clone(),
            volume: n.volume.clone(),
        }
        .save(&deck.paths)?;
        return Ok(out);
    }
    if run.is_local() {
        Record::remove(&deck.paths, &r.name);
        out.push(deck.start_recipe(r, None, only, force)?);
        return Ok(out);
    }
    // Reason: with peers the recipe goes resolved even when the run host knows the name
    // (a built-in on both): its own copy would pick them from its own hosts.toml.
    if picked.is_some() {
        out.push(run.start_recipe(deck, &placed_recipe(r, plan), picked, only, force)?);
    } else if run.knows(deck, &r.name)? {
        out.push(run.start_named(deck, &r.name, only, force)?);
    } else {
        out.push(run.start_recipe(deck, r, None, only, force)?);
    }
    // Remembered so `stop`, `logs` and the views find it on that host.
    Record {
        recipe: r.name.clone(),
        run_host: plan.run_host.clone(),
        // Reason: topology draws home → run host as an NFS stream; a copy elsewhere
        // that nothing mounts (`pull = false`) isn't one.
        home: plan.home.clone().filter(|h| *h == plan.run_host).unwrap_or_default(),
        served_as: r.name.clone(),
        volume: String::new(),
    }
    .save(&deck.paths)?;
    Ok(out)
}

/// Start a recipe (by reference) on `host`: plan (weights home, link, fit) and run.
/// Shared by the TUI run-on picker, the Hosts tab and the web dashboard.
pub fn start_on(deck: &Deck, target: &str, host: &str, only: bool, force: bool, allow_slow: bool) -> Result<String> {
    let cat = deck.catalog();
    let r = crate::deck::resolve(&cat, target)?.recipe.clone();
    if !r.nodes.is_empty() {
        bail!("{} is a multi-node recipe; its [[nodes]] say where each part runs", r.name);
    }
    let p = plan(&deck.paths, &deck.cfg, &r, host, allow_slow)?;
    let lines = execute(deck, &r, &p, only, force)?;
    let started = lines.last().cloned().unwrap_or_else(|| format!("started {} on {host}", r.name));
    // A start here synced already; one on another host reaches this machine's
    // harnesses (and, on the hub, every host's) now.
    let note = if p.run_host == LOCAL { String::new() } else { crate::harness::sync_note(&deck.paths, &deck.cfg) };
    Ok(format!("{started}{note}"))
}

/// Where a start that names no host runs a recipe, and why: on the host that
/// holds its weights when this machine doesn't, else here (`lmdeck use`
/// without `--on`, and the TUI's and the dashboard's Start and Switch).
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    /// `local`, or the hosts.toml name of the host it runs on.
    pub host: String,
    /// Why it runs there, as the plan and the start output say it (`weights on a, not here`).
    pub why: String,
}

impl Pick {
    /// Picks for `r` from the weights index (asked again when older than
    /// [`crate::weights::INDEX_TTL`]). `None` when there is nothing to pick:
    /// this machine knows no other host, the recipe names no weights or has
    /// `[[nodes]]`, or the index is demo data.
    pub fn of(paths: &Paths, cfg: &Config, r: &Recipe) -> Option<Pick> {
        // Reason: demo weights are made up, and a start must never follow them to a real host.
        if crate::telemetry::demo_mode() || r.model.is_empty() || !r.nodes.is_empty() {
            return None;
        }
        let hosts = HostsFile::load(paths).ok().filter(|f| !f.hosts.is_empty())?;
        // Reason: a model already running here stays, so its start says so instead of
        // loading a second copy on the weights home.
        if RunRecord::load(paths, &r.name).is_some() {
            return Some(Pick::here("it runs here already"));
        }
        let idx = Index::fresh(paths, cfg, crate::weights::INDEX_TTL);
        Some(Pick::from_index(&idx, &hosts, r, &cfg.node_name()))
    }

    /// Picks for `r` from `idx`: here when this machine holds the weights; the
    /// weights home ([`Index::home`]) when `hosts` lists it; else here.
    pub fn from_index(idx: &Index, hosts: &HostsFile, r: &Recipe, node: &str) -> Pick {
        let locs = idx.locate(r);
        if locs.iter().any(|l| l.host == LOCAL) {
            return Pick::here("weights here");
        }
        match (idx.home(r, node), r.model.home.as_deref()) {
            (Some(h), _) if hosts.get(&h.host).is_some() => {
                let quiet = idx.hosts.get(&h.host).is_some_and(|w| !w.online);
                let note = if quiet { format!("; {} didn't answer just now", h.host) } else { String::new() };
                Pick { why: format!("weights on {}, not here{note}", h.host), host: h.host }
            }
            // Reason: a cached index can still name a host just taken out of hosts.toml.
            (Some(h), _) => Pick::here(&format!("weights on {}, which hosts.toml doesn't list", h.host)),
            (None, Some(home)) if !locs.is_empty() => {
                Pick::here(&format!("[model] home {home} doesn't hold the weights"))
            }
            (None, _) => Pick::here("no known host has the weights"),
        }
    }

    /// This machine, for `why`.
    fn here(why: &str) -> Pick {
        Pick { host: LOCAL.into(), why: why.into() }
    }
}

/// Starts a recipe (by reference) where a start that names no host runs it
/// ([`Pick`]): through [`start_on`] on the host holding its weights, else
/// here. The TUI's and the dashboard's Start and Switch.
pub fn start_auto(deck: &Deck, target: &str, only: bool, force: bool) -> Result<String> {
    let cat = deck.catalog();
    let r = &crate::deck::resolve(&cat, target)?.recipe;
    match Pick::of(&deck.paths, &deck.cfg, r).filter(|p| p.host != LOCAL) {
        Some(p) => Ok(format!("on {} ({}): {}", p.host, p.why, start_on(deck, target, &p.host, only, force, false)?)),
        None => deck.start(target, only, force),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_times_read_naturally() {
        assert_eq!(human_secs(0.07), "under 1 s");
        assert_eq!(human_secs(12.4), "≈12 s");
        assert_eq!(human_secs(300.0), "≈5.0 min");
    }

    #[test]
    fn exports_matching() {
        let t = "# comment\n/home/a/.cache/huggingface 192.168.100.11(ro,no_subtree_check)\n/srv 10.0.0.0/24(ro)\n/pub *(ro)\n";
        assert!(exports_has(t, "/home/a/.cache/huggingface", "192.168.100.11"));
        assert!(exports_has(t, "/home/a/.cache/huggingface/", "192.168.100.11"));
        assert!(!exports_has(t, "/home/a/.cache/huggingface", "192.168.100.12"));
        assert!(exports_has(t, "/srv", "10.0.0.7") && !exports_has(t, "/srv", "10.0.1.7"));
        assert!(exports_has(t, "/pub", "1.2.3.4"));
    }

    #[test]
    fn commands_mirror_mia() {
        let v = shell_join(&volume_argv("docker", "lmdeck-hf-spark1", "192.168.100.10", "/home/a/.cache/huggingface"));
        assert_eq!(
            v,
            "docker volume create --driver local --opt type=nfs --opt device=:/home/a/.cache/huggingface --opt \
             o=addr=192.168.100.10,nfsvers=4.2,ro,nconnect=8,rsize=1048576,wsize=1048576,hard,timeo=600 lmdeck-hf-spark1"
        );
        assert_eq!(export_line("/x", "10.0.0.2"), "/x 10.0.0.2(ro,no_subtree_check)");
        let (t, f) = preflight_argv("docker", "v", "img", "hub/models--o--n/snapshots/abc");
        assert_eq!(
            shell_join(&t),
            "docker run --rm --entrypoint test -v v:/hf:ro img -f /hf/hub/models--o--n/snapshots/abc/config.json"
        );
        assert!(shell_join(&f).ends_with("-L /hf/hub/models--o--n/snapshots/abc -type f -printf '%P %s\\n'"));
        let e = Estimate::new(100_000_000_000, 200.0, true);
        assert!((e.secs - 4.0).abs() < 1e-9 && e.describe().contains("nominal"));
    }
}
