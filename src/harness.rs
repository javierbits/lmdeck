//! Harness sync: point the agent CLIs on this machine (opencode, omp, grok,
//! pi, hermes) at the models lmdeck is serving, like lmswitch does.
//!
//! When a recipe starts (CLI `use`/`switch`, the TUI, or the web dashboard),
//! every *enabled* harness gets a provider entry — or a custom-provider lane —
//! per running server, each with its own `baseURL` (`http://<host>:<port>/v1`).
//! When a server stops, its entry goes away. Rules, mirroring lmswitch:
//!
//! - A harness is only written when it is installed (its config directory
//!   exists; hermes additionally must already have a `config.yaml` — lmdeck
//!   never creates one).
//! - lmdeck owns only what it wrote (the `lmdeck` provider, `[model.*]`
//!   sections); anything else in the file is preserved. The exception is
//!   opencode, where the `provider` table is replaced wholesale — per-model
//!   providers are the only shape that works there.
//! - Writes are atomic (tmp + rename) and skipped when nothing changed.
//! - The base URL host is `LMDECK_SYNC_HOST`, else the config `host` when it
//!   is a specific address, else `<hostname>.local` (so the URL also works
//!   from other machines on the LAN, as lmswitch's `SPARK_HOST` does).
//!
//! Which harnesses are enabled lives in `[harness]` in config.toml. Before the
//! first-run prompt has run (`asked`), every known harness is enabled and the
//! writers no-op for the ones that are not installed.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::catalog::Catalog;
use crate::config::Config;
use crate::hosts::{HostEntry, HostsFile, FETCH_TIMEOUT};
use crate::paths::{home_dir, Paths};
use crate::runner::{self, RunRecord, Status};
use crate::util::hostname;

/// One running server, as the harness configs describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    /// The served model id (the recipe name: llama.cpp gets `--alias`, vLLM
    /// and SGLang `--served-model-name`).
    pub name: String,
    /// Display name for pickers.
    pub display: String,
    /// Host part of the base URL.
    pub host: String,
    pub port: u16,
    /// Context window, defaulting to 65536 like the recipes' other defaults.
    pub ctx: u64,
    /// Accepts images (name heuristic: contains "vision" or a `vl` token).
    pub vision: bool,
    /// Served on another host (its grok display gets a ` [Host]` tag).
    pub remote: bool,
}

impl Lane {
    pub fn base_url(&self) -> String {
        format!("http://{}:{}/v1", self.host, self.port)
    }

    /// Short host label for picker names: `spark.local` → `Spark`.
    pub fn label(&self) -> String {
        host_label(&self.host)
    }
}

/// A harness lmdeck knows how to sync.
pub struct Harness {
    pub id: &'static str,
    pub label: &'static str,
    /// What presence means, for the wizard and `lmdeck harness list`.
    pub hint: &'static str,
}

pub const KNOWN: &[Harness] = &[
    Harness { id: "opencode", label: "OpenCode", hint: "~/.config/opencode" },
    Harness { id: "omp", label: "omp", hint: "~/.omp/agent" },
    Harness { id: "grok", label: "Grok", hint: "~/.grok" },
    Harness { id: "pi", label: "pi", hint: "~/.pi/agent" },
    Harness { id: "hermes", label: "Hermes", hint: "~/.hermes" },
];

pub fn known(id: &str) -> Option<&'static Harness> {
    KNOWN.iter().find(|h| h.id == id)
}

/// The harness's config file under `home`.
pub fn config_path(id: &str, home: &Path) -> Option<PathBuf> {
    match id {
        "opencode" => Some(home.join(".config/opencode/opencode.json")),
        "omp" => Some(home.join(".omp/agent/models.yml")),
        "grok" => Some(home.join(".grok/config.toml")),
        "pi" => Some(home.join(".pi/agent/models.json")),
        "hermes" => Some(home.join(".hermes/config.yaml")),
        _ => None,
    }
}

/// Whether the harness is installed (its config dir exists; hermes must
/// already have its `config.yaml`).
pub fn detected(id: &str, home: &Path) -> bool {
    let Some(path) = config_path(id, home) else { return false };
    if id == "hermes" {
        return path.is_file();
    }
    path.parent().is_some_and(|p| p.exists())
}

/// Enabled harness ids: the explicit `[harness] sync` list once the first-run
/// prompt has run, otherwise every known harness (writers no-op when the
/// harness is not installed).
pub fn enabled(cfg: &Config) -> Vec<&'static str> {
    if !cfg.harness.asked {
        return KNOWN.iter().map(|h| h.id).collect();
    }
    cfg.harness.sync.iter().filter_map(|s| known(s).map(|h| h.id)).collect()
}
/// The result of syncing one harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub id: &'static str,
    /// The file changed (or would have, had the harness been installed).
    pub changed: bool,
    pub installed: bool,
    /// The write failed (reported by [`sync_note`], never fatal).
    pub error: Option<String>,
}

/// Syncs every enabled harness with the currently running servers. Best
/// effort: per-harness write errors are reported, never fatal.
/// `LMDECK_NO_SYNC` disables the whole thing.
pub fn sync(paths: &Paths, cfg: &Config) -> Vec<Synced> {
    if std::env::var_os("LMDECK_NO_SYNC").is_some() {
        return vec![];
    }
    let (lanes, managed) = lane_sources(paths, cfg);
    let home = home_dir();
    enabled(cfg)
        .into_iter()
        .map(|id| {
            let path = config_path(id, &home).expect("enabled ids are known");
            let installed = detected(id, &home);
            let write = match id {
                "opencode" => write_opencode(&path, &lanes),
                "omp" => write_omp(&path, &lanes),
                "grok" => write_grok(&path, &lanes),
                "pi" => write_pi(&path, &lanes),
                "hermes" => write_hermes(&path, &lanes, &managed),
                _ => Ok(false),
            };
            match write {
                Ok(changed) => Synced { id, changed, installed, error: None },
                Err(e) => Synced { id, changed: false, installed, error: Some(format!("{e:#}")) },
            }
        })
        .collect()
}

/// One-line summary for a start/stop message: ` (synced opencode, omp)`,
/// or ` (harness sync failed: omp: …)` when a write went wrong.
pub fn sync_note(paths: &Paths, cfg: &Config) -> String {
    let done = sync(paths, cfg);
    let failed: Vec<String> = done.iter().filter_map(|s| s.error.as_ref().map(|e| format!("{}: {e}", s.id))).collect();
    if !failed.is_empty() {
        return format!(" (harness sync failed: {})", failed.join("; "));
    }
    let changed: Vec<&str> = done.iter().filter(|s| s.changed).map(|s| s.id).collect();
    if changed.is_empty() {
        String::new()
    } else {
        format!(" (synced {})", changed.join(", "))
    }
}

/// The base URL host: `LMDECK_SYNC_HOST`, else the config `host` when it is a
/// specific address, else `<hostname>.local`.
pub fn serve_host(cfg: &Config) -> String {
    let env = std::env::var("LMDECK_SYNC_HOST").ok().filter(|v| !v.trim().is_empty());
    serve_host_with(cfg.host.trim(), env, &hostname())
}

fn serve_host_with(bind: &str, sync_env: Option<String>, hostname: &str) -> String {
    if let Some(v) = sync_env {
        return v;
    }
    if !bind.is_empty() && bind != "0.0.0.0" && bind != "::" {
        return bind.to_string();
    }
    format!("{hostname}.local")
}

/// The running lanes plus the set of recipe names lmdeck manages (for hermes'
/// custom-provider ownership rule). Local lanes first, then every configured
/// host's running models (same-name remote lanes are skipped: a local lane
/// and a remote one share the model id, and the harnesses key on it).
fn lane_sources(paths: &Paths, cfg: &Config) -> (Vec<Lane>, BTreeSet<String>) {
    let host = serve_host(cfg);
    let cat = Catalog::load(paths, cfg);
    let mut managed: BTreeSet<String> = cat.entries.iter().map(|f| f.recipe.name.clone()).collect();
    let mut probe_cache = HashMap::new();
    let mut lanes = vec![];
    for rec in RunRecord::all(paths) {
        if rec.port == 0 {
            continue;
        }
        if !matches!(
            runner::status_cached(paths, &rec.name, rec.port, &mut probe_cache),
            Status::Loaded | Status::Loading
        ) {
            continue;
        }
        let recipe = cat.entries.iter().find(|f| f.recipe.name == rec.name).map(|f| &f.recipe);
        // Every running lane is managed, catalog or not (its hermes entry is
        // rewritten, never preserved as foreign).
        managed.insert(rec.name.clone());
        let display = recipe.and_then(|r| r.display_name.clone()).unwrap_or_else(|| rec.name.clone());
        lanes.push(Lane {
            vision: looks_vision(&rec.name) || looks_vision(&display),
            ctx: recipe.and_then(|r| r.engine.ctx).unwrap_or(65_536),
            name: rec.name.clone(),
            display,
            host: host.clone(),
            port: rec.port,
            remote: false,
        });
    }
    let wanted = cfg.harness.hosts.as_deref();
    for mut lane in remote_lanes(paths, wanted, &cfg.node_name()) {
        if lanes.iter().any(|l| l.name == lane.name) {
            continue;
        }
        managed.insert(lane.name.clone());
        lane.vision = looks_vision(&lane.name) || looks_vision(&lane.display);
        lanes.push(lane);
    }
    (lanes, managed)
}

/// The running models of the configured hosts, fetched in parallel
/// (best-effort: an unreachable host just contributes nothing). Each lane's
/// host is the address part of the host's `hosts.toml` URL, so the written
/// base URLs resolve wherever the harnesses run.
fn remote_lanes(paths: &Paths, hosts: Option<&[String]>, node_name: &str) -> Vec<Lane> {
    let Ok(file) = HostsFile::load(paths) else { return vec![] };
    let targets: Vec<&HostEntry> = file
        .hosts
        .iter()
        .filter(|e| !crate::remote::is_local(&e.name, node_name))
        .filter(|e| hosts.is_none_or(|h| h.iter().any(|n| n == &e.name)))
        .collect();
    if targets.is_empty() {
        return vec![];
    }
    std::thread::scope(|s| {
        let jobs: Vec<_> = targets.iter().map(|e| s.spawn(move || lanes_from(e))).collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
    })
}

/// One host's running models, from its `/api/snapshot`.
fn lanes_from(e: &HostEntry) -> Vec<Lane> {
    let Ok(v) = crate::remote::Client::new(e).get_timeout::<serde_json::Value>("snapshot", FETCH_TIMEOUT) else {
        return vec![];
    };
    let Some(rows) = v.get("recipes").and_then(|r| r.as_array()) else { return vec![] };
    let Some(host) = url_host(&e.url) else { return vec![] };
    rows.iter()
        .filter(|r| r.get("status").and_then(|s| s.as_str()).is_some_and(|s| s == "loaded" || s == "loading"))
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?.to_string();
            let display = r.get("display_name").and_then(|d| d.as_str()).unwrap_or(&name).to_string();
            Some(Lane {
                vision: false,
                remote: true,
                name,
                display,
                host: host.clone(),
                port: r.get("port")?.as_u64()? as u16,
                ctx: r.get("ctx").and_then(|c| c.as_u64()).unwrap_or(65_536),
            })
        })
        .collect()
}

/// The host part of a serve URL: `http://spark2.lan:7979` → `spark2.lan`.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split('/').next()?;
    if let Some(rest) = rest.strip_prefix('[') {
        return rest.split_once(']').map(|(h, _)| h.to_string());
    }
    match rest.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.is_empty() => Some(h.to_string()),
        _ => Some(rest.to_string()),
    }
}

/// Name heuristic from lmswitch: "vision" anywhere, or a standalone `vl`
/// token (`qwen3-vl-8b` but not `vllm`).
fn looks_vision(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("vision") || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| t == "vl")
}

/// `spark.local` → `Spark`, `dual` → `Dual` (Python `str.capitalize`).
fn host_label(host: &str) -> String {
    let word = host.split('.').next().unwrap_or("?");
    let mut chars = word.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => "?".into(),
    }
}

/// Writes `payload` over `path` atomically; `Ok(false)` when unchanged.
fn write_atomic(path: &Path, payload: &str) -> Result<bool> {
    if fs::read_to_string(path).is_ok_and(|t| t == payload) {
        return Ok(false);
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, payload).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("updating {}", path.display()))?;
    Ok(true)
}

fn harness_installed(path: &Path) -> bool {
    path.parent().is_some_and(|p| p.exists())
}

// ───────────────────────────── opencode ─────────────────────────────
// `~/.config/opencode/opencode.json`: one provider per running model (each on
// its own port). lmdeck owns the `provider` table; everything else (schema,
// plugin, permissions, commands) is left alone.

fn write_opencode(path: &Path, lanes: &[Lane]) -> Result<bool> {
    if !harness_installed(path) {
        return Ok(false);
    }
    let mut cfg: serde_json::Value =
        fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    if !cfg.is_object() {
        cfg = json!({});
    }
    let mut providers = serde_json::Map::new();
    for l in lanes {
        let mut model = json!({ "name": l.display, "limit": { "context": l.ctx, "output": l.ctx.min(8192) } });
        if l.vision {
            // opencode gates image attachments on this flag.
            model["attachment"] = json!(true);
        }
        providers.insert(
            l.name.clone(),
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": format!("{} ({})", l.display, l.label()),
                "options": { "baseURL": l.base_url() },
                "models": { (l.name.clone()): model },
            }),
        );
    }
    cfg["provider"] = serde_json::Value::Object(providers);
    write_atomic(path, &format!("{}\n", serde_json::to_string_pretty(&cfg)?))
}

// ───────────────────────────── omp ─────────────────────────────
// `~/.omp/agent/models.yml`: all lanes live under one `lmdeck` provider, each
// model carrying its own `baseUrl` (the provider's is a deliberately dead
// sentinel so it can never absorb a lane that lost its own URL). Other
// providers (ollama, a hosted proxy, …) are preserved.

fn write_omp(path: &Path, lanes: &[Lane]) -> Result<bool> {
    if !harness_installed(path) {
        return Ok(false);
    }
    let mut cfg: serde_yaml::Value = fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_yaml::from_str(&t).ok())
        .unwrap_or_else(|| serde_yaml::Value::Mapping(Default::default()));
    if !cfg.is_mapping() {
        cfg = serde_yaml::Value::Mapping(Default::default());
    }
    let drop_providers = {
        let providers = yaml_table(cfg.as_mapping_mut().expect("checked mapping"), "providers");
        providers.remove(serde_yaml::Value::String("lmdeck".into()));
        if !lanes.is_empty() {
            let models: Vec<serde_yaml::Value> = lanes
                .iter()
                .map(|l| {
                    let mut m = serde_yaml::Mapping::new();
                    m.insert("id".into(), l.name.clone().into());
                    m.insert("name".into(), format!("{} ({})", l.display, l.label()).into());
                    m.insert("baseUrl".into(), l.base_url().into());
                    m.insert("contextWindow".into(), l.ctx.into());
                    // omp's own discovery caps local models at 32768 output tokens.
                    m.insert("maxTokens".into(), l.ctx.min(32_768).into());
                    if l.vision {
                        m.insert("input".into(), vec!["text", "image"].into());
                        m.insert("imageInputDecoder".into(), "stb".into());
                    }
                    serde_yaml::Value::Mapping(m)
                })
                .collect();
            let mut p = serde_yaml::Mapping::new();
            p.insert("baseUrl".into(), "http://127.0.0.1:1/v1".into());
            p.insert("auth".into(), "none".into());
            p.insert("api".into(), "openai-completions".into());
            p.insert("models".into(), models.into());
            providers.insert("lmdeck".into(), p.into());
        }
        providers.is_empty()
    };
    if drop_providers {
        cfg.as_mapping_mut().expect("checked mapping").remove(serde_yaml::Value::String("providers".into()));
    }
    write_atomic(path, &serde_yaml::to_string(&cfg)?)
}

/// Gets `root[key]` as a mapping, creating it when missing or of another type.
fn yaml_table<'a>(root: &'a mut serde_yaml::Mapping, key: &str) -> &'a mut serde_yaml::Mapping {
    if !root.get(key).is_some_and(|v| v.is_mapping()) {
        root.insert(key.into(), serde_yaml::Value::Mapping(Default::default()));
    }
    root.get_mut(key).and_then(|v| v.as_mapping_mut()).expect("just inserted")
}

// ───────────────────────────── grok ─────────────────────────────
// `~/.grok/config.toml`: `[model.<key>]` sections per lane, rewritten
// textually so everything else in the file (comments included) survives.
// With nothing serving, the file is left as-is so the picker keeps its
// entries across a restart (mirrors lmswitch).

fn write_grok(path: &Path, lanes: &[Lane]) -> Result<bool> {
    if !harness_installed(path) || lanes.is_empty() {
        return Ok(false);
    }
    let mut sections: Vec<String> = vec![];
    for l in lanes {
        let key: String = l.name.chars().map(|c| if c == '.' || c == '_' { '-' } else { c }).collect();
        // Remote lanes get a host tag, like lmswitch's cluster models.
        let display = if l.remote { format!("{} [{}]", l.display, l.label()) } else { l.display.clone() };
        sections.push(format!(
            "[model.{key}]\nmodel = \"{}\"\nbase_url = \"{}\"\nname = \"{}\"\ncontext_window = {}",
            l.name,
            l.base_url(),
            display,
            l.ctx
        ));
    }
    let existing = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = existing.lines().collect();

    // Split into sections: drop `[model.*]`, keep every other section (and
    // any leading content) with their lines.
    let mut kept: Vec<Vec<String>> = vec![];
    let mut i = 0;
    while i < lines.len() {
        let starts_header = lines[i].trim_start().starts_with('[');
        if starts_header && lines[i].trim_start().starts_with("[model.") {
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with('[') {
                i += 1;
            }
            continue;
        }
        let start = i;
        i += 1;
        while i < lines.len() && !lines[i].trim_start().starts_with('[') {
            i += 1;
        }
        kept.push(lines[start..i].iter().map(|s| s.to_string()).collect());
    }
    if kept.is_empty() {
        kept.push(vec![]);
    }

    // Repair a dangling `[models] default`: keep the user's choice while it
    // serves, else fall back to the first live lane.
    let serving: Vec<&str> = lanes.iter().map(|l| l.name.as_str()).collect();
    for sec in &mut kept {
        if sec.first().map(|l| l.trim() == "[models]").unwrap_or(false) {
            for line in sec.iter_mut().skip(1) {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("default").and_then(|r| r.trim().strip_prefix('=')) {
                    let val = rest.trim().trim_matches('"');
                    if !serving.contains(&val) {
                        *line = format!("default = \"{}\"", serving[0]);
                    }
                }
            }
        }
    }

    let mut out: Vec<String> = vec![];
    for mut sec in kept {
        while sec.last().is_some_and(|l| l.trim().is_empty()) {
            sec.pop();
        }
        out.extend(sec);
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        out.push(String::new());
    }
    for sec in &sections {
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        out.push(String::new());
        out.extend(sec.lines().map(str::to_string));
    }
    let mut payload = out.join("\n");
    if !payload.ends_with('\n') {
        payload.push('\n');
    }
    write_atomic(path, &payload)
}

// ───────────────────────────── pi ─────────────────────────────
// `~/.pi/agent/models.json`: same shape as omp's — one `lmdeck` provider with
// a dead sentinel baseUrl (pi requires one) and per-model `baseUrl` overrides.

fn write_pi(path: &Path, lanes: &[Lane]) -> Result<bool> {
    if !harness_installed(path) {
        return Ok(false);
    }
    let mut cfg: serde_json::Value =
        fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
    if !cfg.is_object() {
        cfg = json!({});
    }
    if !cfg.get("providers").is_some_and(|v| v.is_object()) {
        cfg["providers"] = json!({});
    }
    let models: Vec<serde_json::Value> = lanes
        .iter()
        .map(|l| {
            let mut m = json!({
                "id": l.name,
                "name": format!("{} ({})", l.display, l.label()),
                "baseUrl": l.base_url(),
                "contextWindow": l.ctx,
                "maxTokens": l.ctx.min(32_768),
            });
            if l.vision {
                m["input"] = json!(["text", "image"]);
            }
            m
        })
        .collect();
    if models.is_empty() {
        cfg["providers"].as_object_mut().expect("checked object").remove("lmdeck");
    } else {
        cfg["providers"]["lmdeck"] = json!({
            "baseUrl": "http://127.0.0.1:1/v1",
            "api": "openai-completions",
            "apiKey": "none",
            "models": models,
        });
    }
    if cfg["providers"].as_object().is_some_and(|p| p.is_empty()) {
        cfg.as_object_mut().expect("checked object").remove("providers");
    }
    write_atomic(path, &format!("{}\n", serde_json::to_string_pretty(&cfg)?))
}

// ───────────────────────────── hermes ─────────────────────────────
// `~/.hermes/config.yaml`: a single active `model:` block plus per-lane
// `custom_providers` entries. The active model is sticky — only switched when
// the configured one is no longer being served. Requires an existing config.

fn write_hermes(path: &Path, lanes: &[Lane], managed: &BTreeSet<String>) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut cfg: serde_yaml::Value = fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_yaml::from_str(&t).ok())
        .unwrap_or_else(|| serde_yaml::Value::Mapping(Default::default()));
    if !cfg.is_mapping() {
        cfg = serde_yaml::Value::Mapping(Default::default());
    }

    let cur_default = cfg.get("model").and_then(|m| m.get("default")).and_then(|v| v.as_str()).map(str::to_string);
    let primary = lanes
        .iter()
        .find(|l| Some(&l.name) == cur_default.as_ref())
        .or_else(|| lanes.iter().find(|l| !l.vision))
        .or_else(|| lanes.first());
    if let Some(p) = primary {
        let model = yaml_table(cfg.as_mapping_mut().expect("checked mapping"), "model");
        model.insert("default".into(), p.name.clone().into());
        model.insert("provider".into(), "custom".into());
        model.insert("base_url".into(), p.base_url().into());
        model.insert("api_key".into(), "none".into());
        model.insert("context_length".into(), p.ctx.into());
    }

    let cur_vision = cfg
        .get("auxiliary")
        .and_then(|a| a.get("vision"))
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let vision_model = cur_vision
        .as_deref()
        .and_then(|c| lanes.iter().find(|l| l.name == c && l.vision))
        .or_else(|| lanes.iter().find(|l| l.vision));
    if let Some(v) = vision_model {
        let aux = yaml_table(cfg.as_mapping_mut().expect("checked mapping"), "auxiliary");
        let vision = yaml_table(aux, "vision");
        vision.insert("provider".into(), "custom".into());
        vision.insert("model".into(), v.name.clone().into());
        vision.insert("base_url".into(), v.base_url().into());
        vision.insert("api_key".into(), "none".into());
        vision.insert("context_length".into(), v.ctx.into());
    }

    // custom_providers: lmdeck-managed names are every recipe in the catalog
    // plus every running lane; anything else the user added by hand is kept.
    let desired: Vec<serde_yaml::Value> = lanes
        .iter()
        .map(|l| {
            let mut models = serde_yaml::Mapping::new();
            models.insert(l.name.clone().into(), serde_yaml::Value::Mapping(Default::default()));
            let mut e = serde_yaml::Mapping::new();
            e.insert("name".into(), l.name.clone().into());
            e.insert("base_url".into(), l.base_url().into());
            e.insert("api_key".into(), "none".into());
            e.insert("model".into(), l.name.clone().into());
            e.insert("context_length".into(), l.ctx.into());
            e.insert("models".into(), models.into());
            // Live /v1/models discovery would probe a possibly-stopped lane on
            // every open of the picker; we already know each lane's model.
            e.insert("discover_models".into(), false.into());
            serde_yaml::Value::Mapping(e)
        })
        .collect();
    let existing = cfg.get("custom_providers").and_then(|v| v.as_sequence()).cloned().unwrap_or_default();
    let foreign: Vec<serde_yaml::Value> = existing
        .into_iter()
        .filter(|e| e.get("name").and_then(|n| n.as_str()).map(|n| !managed.contains(n)).unwrap_or(true))
        .collect();
    // Desired lanes replace any same-name entry (a stale managed set must
    // never duplicate a lane); foreign entries survive untouched.
    let mut all = foreign;
    for d in desired {
        let name = d.get("name").and_then(|n| n.as_str()).map(str::to_string);
        if let Some(n) = name {
            all.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(n.as_str()));
        }
        all.push(d);
    }
    if all.is_empty() {
        if let Some(m) = cfg.as_mapping_mut() {
            m.remove(serde_yaml::Value::String("custom_providers".into()));
        }
    } else if let Some(m) = cfg.as_mapping_mut() {
        m.insert("custom_providers".into(), all.into());
    }
    write_atomic(path, &serde_yaml::to_string(&cfg)?)
}

// ───────────────────────────── first-run prompt ─────────────────────────────

/// Whether the interactive first-run prompt should run for this invocation.
pub fn should_ask(cfg: &Config) -> bool {
    if cfg.harness.asked || cfg.skipped.is_some() || std::env::var_os("LMDECK_NO_SYNC").is_some() {
        return false;
    }
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choice {
    /// Harness ids to sync.
    pub harnesses: Vec<String>,
    /// Hosts whose running models are included (`None` = every configured
    /// host, as lmswitch did with its cluster).
    pub hosts: Option<Vec<String>>,
}

/// The first-run prompt: one y/N per known harness (defaulting to yes for the
/// detected ones), then one per configured host for including its running
/// models. Returns the choice, or `None` when skipped (non-interactive,
/// `LMDECK_NO_SYNC`, or a config from a newer lmdeck).
pub fn wizard(cfg: &Config) -> Option<Choice> {
    use std::io::Write as _;
    if !should_ask(cfg) {
        return None;
    }
    let home = home_dir();
    println!("{}", crate::ui::bold("sync lmdeck with your agent harnesses?"));
    println!(
        "{}",
        crate::ui::dim(
            "when a model is served, its config gets a provider entry pointing at it; on stop it goes away."
        )
    );
    let mut harnesses = vec![];
    for h in KNOWN {
        let on = detected(h.id, &home);
        let mark = if on { crate::ui::green("●") } else { crate::ui::dim("○") };
        let extra = if on { "detected" } else { "not detected" };
        print!(
            "  {mark} {} {} {} [{}] > ",
            crate::ui::bold(h.label),
            crate::ui::dim(h.hint),
            extra,
            if on { "Y/n" } else { "y/N" }
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            break;
        }
        let yes = match line.trim().to_lowercase().as_str() {
            "" => on,
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => on,
        };
        if yes {
            harnesses.push(h.id.to_string());
        }
    }
    if harnesses.is_empty() {
        println!("{}", crate::ui::dim("no harnesses selected — `lmdeck harness enable <name>` later."));
    } else {
        println!("{} {}", crate::ui::green("✓"), harnesses.join(", "));
    }

    // Models running on the other machines: include each host's lanes?
    let node = cfg.node_name();
    let remote: Vec<String> = HostsFile::load(&Paths::from_env())
        .map(|f| f.hosts.iter().filter(|e| !crate::remote::is_local(&e.name, &node)).map(|e| e.name.clone()).collect())
        .unwrap_or_default();
    let mut hosts = None;
    if !remote.is_empty() {
        println!("{}", crate::ui::bold("models running on other hosts can be synced too:"));
        let mut chosen = vec![];
        for name in &remote {
            print!("  include models running on {} [Y/n] > ", crate::ui::bold(name));
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                break;
            }
            let yes = match line.trim().to_lowercase().as_str() {
                "" => true,
                "y" | "yes" => true,
                "n" | "no" => false,
                _ => true,
            };
            if yes {
                chosen.push(name.clone());
            }
        }
        if chosen.is_empty() {
            println!("{}", crate::ui::dim("local models only — `lmdeck harness hosts all` later."));
        } else {
            hosts = Some(chosen);
        }
    }
    Some(Choice { harnesses, hosts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("lmdeck-harness-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn lane(name: &str, port: u16, vision: bool) -> Lane {
        Lane {
            name: name.into(),
            display: name.into(),
            host: "spark.local".into(),
            port,
            ctx: 65_536,
            vision,
            remote: false,
        }
    }

    #[test]
    fn url_host_extracts_the_address() {
        assert_eq!(url_host("http://spark2.lan:7979").as_deref(), Some("spark2.lan"));
        assert_eq!(url_host("http://10.0.0.5:7979/").as_deref(), Some("10.0.0.5"));
        assert_eq!(url_host("http://[::1]:7979").as_deref(), Some("::1"));
        assert_eq!(url_host("spark.lan").as_deref(), Some("spark.lan"));
    }

    fn lane_ctx(name: &str, port: u16, ctx: u64) -> Lane {
        Lane { ctx, ..lane(name, port, false) }
    }

    #[test]
    fn lanes_come_from_alive_run_records() {
        let root = tempdir("lanes");
        let paths =
            Paths::resolve(|k| if k == "LMDECK_HOME" { Some(root.display().to_string()) } else { None }, root.clone());
        fs::create_dir_all(paths.run_dir()).unwrap();
        let rec = serde_json::json!({
            "name": "test-model", "engine": "llama.cpp", "pid": std::process::id(),
            "port": 45_971, "started_at": 0, "log": "/tmp/x.log", "argv": ["x"], "oneshot": false,
        });
        fs::write(paths.run_dir().join("test-model.json"), serde_json::to_string(&rec).unwrap()).unwrap();
        let (lanes, managed) = lane_sources(&paths, &Config::default());
        assert_eq!(lanes.len(), 1, "{lanes:?}");
        assert_eq!(lanes[0].name, "test-model");
        assert_eq!(lanes[0].port, 45_971);
        assert_eq!(lanes[0].host, format!("{}.local", hostname()));
        assert!(managed.contains("test-model"));
        // a dead pid is not a lane
        let rec = serde_json::json!({
            "name": "dead", "engine": "llama.cpp", "pid": 2_147_000_000u32,
            "port": 45_972, "started_at": 0, "log": "/tmp/x.log", "argv": ["x"], "oneshot": false,
        });
        fs::write(paths.run_dir().join("dead.json"), serde_json::to_string(&rec).unwrap()).unwrap();
        let (lanes, _) = lane_sources(&paths, &Config::default());
        assert!(lanes.iter().all(|l| l.name != "dead"), "{lanes:?}");
    }

    #[test]
    fn opencode_creates_providers_and_preserves_the_rest() {
        let dir = tempdir("opencode");
        let path = dir.join("opencode.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, r#"{"$schema":"https://opencode.ai/config.json","theme":"omarchy"}"#).unwrap();
        let lanes = vec![lane("glm5.3-flash", 8890, false), lane("qwen3-vl-8b", 8109, true)];
        assert!(write_opencode(&path, &lanes).unwrap());
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "omarchy");
        let p = &v["provider"];
        assert_eq!(p["glm5.3-flash"]["options"]["baseURL"], "http://spark.local:8890/v1");
        assert_eq!(p["glm5.3-flash"]["models"]["glm5.3-flash"]["limit"]["context"], 65_536);
        assert_eq!(p["glm5.3-flash"]["models"]["glm5.3-flash"]["limit"]["output"], 8192);
        assert_eq!(p["qwen3-vl-8b"]["models"]["qwen3-vl-8b"]["attachment"], true);
        assert!(p["glm5.3-flash"]["models"]["glm5.3-flash"].get("attachment").is_none());
        // idempotent
        assert!(!write_opencode(&path, &lanes).unwrap());
    }

    #[test]
    fn opencode_missing_dir_is_a_noop() {
        let path = PathBuf::from("/nonexistent-lmdeck-test/opencode.json");
        assert!(!write_opencode(&path, &[lane("x", 1, false)]).unwrap());
    }

    #[test]
    fn omp_preserves_foreign_providers_and_drops_empty_lmdeck() {
        let dir = tempdir("omp");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.yml");
        fs::write(
            &path,
            "providers:\n  ollama:\n    baseUrl: http://localhost:11434/v1\n    api: openai-completions\n",
        )
        .unwrap();
        let lanes = vec![lane_ctx("glm5.3-flash", 8890, 1_048_576)];
        assert!(write_omp(&path, &lanes).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("ollama"));
        assert!(text.contains("lmdeck"));
        assert!(text.contains("http://spark.local:8890/v1"));
        assert!(text.contains("contextWindow: 1048576"));
        assert!(text.contains("maxTokens: 32768"));
        // no lanes: the lmdeck provider goes away, ollama stays
        assert!(write_omp(&path, &[]).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("lmdeck"));
        assert!(text.contains("ollama"));
        assert!(!write_omp(&path, &[]).unwrap());
    }

    #[test]
    fn omp_vision_lane_declares_image_input() {
        let dir = tempdir("omp-vision");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.yml");
        write_omp(&path, &[lane("qwen3-vl-8b", 8109, true)]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("input:"));
        assert!(text.contains("- image"));
        assert!(text.contains("imageInputDecoder: stb"));
    }

    #[test]
    fn grok_replaces_model_sections_and_keeps_the_rest() {
        let dir = tempdir("grok");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[cli]\ninstaller = \"internal\"\n\n[models]\ndefault = \"old-model\"\n\n[privacy]\nacked = true\n\n[model.old-model]\nmodel = \"old-model\"\nbase_url = \"http://x:1/v1\"\nname = \"Old\"\ncontext_window = 4096\n",
        )
        .unwrap();
        let lanes = vec![lane_ctx("glm5.3.flash", 8890, 262_144)];
        assert!(write_grok(&path, &lanes).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[cli]"));
        assert!(text.contains("[privacy]"));
        assert!(!text.contains("[model.old-model]"));
        assert!(text.contains("[model.glm5-3-flash]"));
        assert!(text.contains("base_url = \"http://spark.local:8890/v1\""));
        assert!(text.contains("context_window = 262144"));
        assert!(text.contains("default = \"glm5.3.flash\""));
        // idempotent
        assert!(!write_grok(&path, &lanes).unwrap());
        // nothing serving: file untouched
        let before = fs::read_to_string(&path).unwrap();
        assert!(!write_grok(&path, &[]).unwrap());
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn pi_uses_the_lmdeck_provider_with_per_model_urls() {
        let dir = tempdir("pi");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        let lanes = vec![lane_ctx("glm5.3-flash", 8890, 1_048_576)];
        assert!(write_pi(&path, &lanes).unwrap());
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let p = &v["providers"]["lmdeck"];
        assert_eq!(p["api"], "openai-completions");
        assert_eq!(p["baseUrl"], "http://127.0.0.1:1/v1");
        assert_eq!(p["models"][0]["id"], "glm5.3-flash");
        assert_eq!(p["models"][0]["baseUrl"], "http://spark.local:8890/v1");
        assert_eq!(p["models"][0]["contextWindow"], 1_048_576);
        assert!(!write_pi(&path, &lanes).unwrap());
        assert!(write_pi(&path, &[]).unwrap());
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["providers"].get("lmdeck").is_none());
    }

    #[test]
    fn hermes_is_sticky_and_preserves_foreign_providers() {
        let dir = tempdir("hermes");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        fs::write(
            &path,
            "model:\n  default: old\n  provider: custom\n  base_url: http://x:1/v1\n  api_key: none\n  context_length: 4096\nauxiliary:\n  vision:\n    model: old\n    base_url: http://x:1/v1\ncustom_providers:\n- name: hand-added\n  base_url: http://10.0.0.5:8080/v1\n  api_key: none\n  model: hand-added\n  context_length: 8192\n  models:\n    hand-added: {}\n  discover_models: false\n",
        )
        .unwrap();
        // hand-added is foreign until it appears in the catalog
        let mut managed = BTreeSet::from(["glm5.3-flash".to_string()]);
        let lanes = vec![lane_ctx("glm5.3-flash", 8890, 1_048_576), lane("qwen3-vl-8b", 8109, true)];
        assert!(write_hermes(&path, &lanes, &managed).unwrap());
        let v: serde_yaml::Value = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // sticky: the current default stays (it is still served)
        assert_eq!(v["model"]["default"], "glm5.3-flash");
        assert_eq!(v["model"]["base_url"], "http://spark.local:8890/v1");
        // the vision aux is sticky too, and picks the vision lane
        assert_eq!(v["auxiliary"]["vision"]["model"], "qwen3-vl-8b");
        // foreign custom provider preserved, managed lanes present
        let cps = v["custom_providers"].as_sequence().unwrap();
        assert_eq!(cps.len(), 3);
        assert!(cps.iter().any(|e| e["name"] == "hand-added"));
        assert!(cps.iter().all(|e| e["discover_models"] == false));
        // when the default stops serving, it moves to a live lane
        let lanes2 = vec![lane_ctx("qwen3-vl-8b", 8109, 65_536)];
        assert!(write_hermes(&path, &lanes2, &managed).unwrap());
        let v: serde_yaml::Value = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["model"]["default"], "qwen3-vl-8b");
        let cps = v["custom_providers"].as_sequence().unwrap();
        assert_eq!(cps.len(), 2); // the foreign entry survives
                                  // once hand-added is a managed recipe name, its entry is replaced in kind
        managed.insert("hand-added".to_string());
        assert!(write_hermes(&path, &lanes2, &managed).unwrap());
        let v: serde_yaml::Value = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let cps = v["custom_providers"].as_sequence().unwrap();
        assert_eq!(cps.len(), 1);
        assert!(!write_hermes(&path, &lanes2, &managed).unwrap());
        // no config file: never created
        assert!(!write_hermes(&dir.join("absent.yaml"), &lanes2, &managed).unwrap());
    }

    #[test]
    fn vision_heuristic_and_host_label() {
        assert!(looks_vision("qwen3-vl-8b"));
        assert!(looks_vision("Vision-Llama"));
        assert!(!looks_vision("vllm-server"));
        assert!(!looks_vision("glm5.3-flash"));
        assert_eq!(host_label("spark.local"), "Spark");
        assert_eq!(host_label("dual"), "Dual");
        assert_eq!(host_label(""), "?");
    }

    #[test]
    fn serve_host_prefers_env_then_bind_then_mdns() {
        assert_eq!(serve_host_with("0.0.0.0", Some("spark.lan".into()), "spark"), "spark.lan");
        assert_eq!(serve_host_with("0.0.0.0", None, "spark"), "spark.local");
        assert_eq!(serve_host_with("127.0.0.1", None, "spark"), "127.0.0.1");
        assert_eq!(serve_host_with("::", None, "spark"), "spark.local");
    }

    #[test]
    fn enabled_follows_the_asked_flag() {
        let mut cfg = Config::default();
        assert_eq!(enabled(&cfg), vec!["opencode", "omp", "grok", "pi", "hermes"]);
        cfg.harness = crate::config::HarnessConfig { sync: vec!["omp".into()], asked: true, hosts: None };
        assert_eq!(enabled(&cfg), vec!["omp"]);
        cfg.harness = crate::config::HarnessConfig { sync: vec![], asked: true, hosts: None };
        assert!(enabled(&cfg).is_empty());
    }

    /// A one-shot HTTP server answering `/api/snapshot` with the given rows.
    fn stub_snapshot(rows: serde_json::Value) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = serde_json::json!({ "recipes": rows }).to_string();
        let handle = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        (port, handle)
    }

    #[test]
    fn remote_lanes_come_from_configured_hosts() {
        let root = tempdir("remote");
        let home = root.display().to_string();
        let paths = Paths::resolve(|k| if k == "LMDECK_HOME" { Some(home.clone()) } else { None }, root);
        fs::create_dir_all(paths.config_dir.clone()).unwrap();
        let (port, handle) = stub_snapshot(serde_json::json!([
            {"name": "glm5.3-flash", "display_name": "GLM-5.3-Flash", "status": "loaded", "port": 8890, "ctx": 1048576},
            {"name": "stopped-one", "display_name": "stopped", "status": "stopped", "port": 8080}
        ]));
        fs::write(
            paths.config_dir.join("hosts.toml"),
            format!("[[host]]\nname = \"spark2\"\nurl = \"http://127.0.0.1:{port}\"\n"),
        )
        .unwrap();
        let lanes = remote_lanes(&paths, None, "this-box");
        assert_eq!(lanes.len(), 1, "{lanes:?}");
        assert_eq!(lanes[0].name, "glm5.3-flash");
        assert_eq!(lanes[0].host, "127.0.0.1");
        assert_eq!(lanes[0].port, 8890);
        assert_eq!(lanes[0].ctx, 1_048_576);
        assert!(lanes[0].remote);
        handle.join().unwrap();
        // a host filter naming nothing configured yields no lanes
        assert!(remote_lanes(&paths, Some(&["other".into()]), "this-box").is_empty());
    }

    #[test]
    fn grok_tags_remote_lanes_with_the_host() {
        let dir = tempdir("grok-remote");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut lane = lane_ctx("glm5.3-flash", 8890, 1_048_576);
        lane.host = "spark2.lan".into();
        lane.remote = true;
        assert!(write_grok(&path, &[lane]).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("name = \"glm5.3-flash [Spark2]\""), "{text}");
        assert!(text.contains("base_url = \"http://spark2.lan:8890/v1\""));
    }

    #[test]
    fn same_name_remote_lane_is_skipped() {
        let mut local = lane("alpha", 8080, false);
        local.host = "spark.local".into();
        let mut remote = lane("alpha", 9999, false);
        remote.host = "spark2.lan".into();
        remote.remote = true;
        let mut lanes = vec![local];
        if !lanes.iter().any(|l| l.name == remote.name) {
            lanes.push(remote);
        }
        assert_eq!(lanes.len(), 1);
        assert!(!lanes[0].remote);
    }
}
