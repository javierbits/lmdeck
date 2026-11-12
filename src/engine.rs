//! Engine detection and launch-command construction.
//!
//! `plan()` is pure: given a recipe and a context it returns the exact argv
//! lmdeck would run, which is what `recipe show` and `--dry-run` print and
//! what the unit tests pin.

use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::{Config, EngineOverrides};
use crate::hardware::which;
use crate::paths::{expand, home_dir};
use crate::recipe::{Engine, Recipe};
use crate::util::run_with_timeout;

/// Whether one engine (or helper tool) is usable on this machine.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub name: String,
    pub available: bool,
    pub detail: String,
}

/// Everything `doctor` reports about installed engines and tools.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub engines: BTreeMap<String, ToolStatus>,
    pub tools: BTreeMap<String, ToolStatus>,
}

impl Inventory {
    pub fn engine(&self, e: Engine) -> Option<&ToolStatus> {
        self.engines.get(e.as_str())
    }

    pub fn tool_available(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.available)
    }

    /// `Ok(())` when the recipe's engine can be launched here.
    pub fn can_launch(&self, r: &Recipe) -> std::result::Result<(), String> {
        if r.engine.image.is_some() && matches!(r.engine.name, Engine::Vllm | Engine::Sglang | Engine::LmDeploy) {
            return if self.tool_available("docker") {
                Ok(())
            } else {
                Err(format!(
                    "{} runs in Docker ({}), but docker was not found",
                    r.engine.name,
                    r.engine.image.as_deref().unwrap_or("")
                ))
            };
        }
        if r.engine.name == Engine::Custom {
            let bin = r.engine.command.first().map(String::as_str).unwrap_or("");
            // `./start.sh` lives in the recipe's working dir / [source] checkout,
            // which may not be cloned yet; `start` checks it once it is.
            if Path::new(bin).is_relative() && bin.contains('/') {
                return Ok(());
            }
            return which(bin).map(|_| ()).ok_or_else(|| format!("custom command '{bin}' not found"));
        }
        match self.engine(r.engine.name) {
            Some(s) if s.available => Ok(()),
            Some(s) => {
                let fix = match r.engine.name {
                    Engine::LlamaCpp => "; install with `lmdeck engine install llama.cpp`",
                    Engine::Vllm | Engine::Sglang | Engine::LmDeploy if r.engine.image.is_some() => {
                        "; install Docker or pull the image"
                    }
                    _ => "",
                };
                Err(format!("{} not installed ({}){fix}", r.engine.name, s.detail))
            }
            None => Err(format!("{} not detected", r.engine.name)),
        }
    }

    /// Probes the machine. Cheap: PATH lookups plus one python `find_spec` call.
    pub fn detect(cfg: &Config) -> Inventory {
        let o = &cfg.engines;
        let python = o.python.clone().unwrap_or_else(|| "python3".into());
        let modules = python_modules(&python, &["vllm", "sglang", "exllamav3", "mlx_lm", "lmdeploy"]);
        let has_mod = |m: &str| modules.iter().any(|x| x == m);
        let found = |name: &str, p: Option<PathBuf>, missing: &str| ToolStatus {
            name: name.into(),
            available: p.is_some(),
            detail: p.map(|p| p.display().to_string()).unwrap_or_else(|| missing.into()),
        };
        let mut engines = BTreeMap::new();
        let mut tools = BTreeMap::new();

        let docker = found("docker", which(o.docker.as_deref().unwrap_or("docker")), "not on PATH");
        let docker_ok = docker.available;

        engines.insert(
            "llama.cpp".into(),
            found("llama.cpp", find_llama_server(o), "llama-server not on PATH (set engines.llama_server)"),
        );
        let vllm_bin = which(o.vllm.as_deref().unwrap_or("vllm"));
        engines.insert(
            "vllm".into(),
            match (vllm_bin, has_mod("vllm")) {
                (Some(p), _) => found("vllm", Some(p), ""),
                (None, true) => {
                    ToolStatus { name: "vllm".into(), available: true, detail: format!("python module ({python})") }
                }
                (None, false) => ToolStatus {
                    name: "vllm".into(),
                    available: false,
                    detail: if docker_ok {
                        "not installed natively; Docker recipes still work".into()
                    } else {
                        "not installed".into()
                    },
                },
            },
        );
        engines.insert(
            "sglang".into(),
            ToolStatus {
                name: "sglang".into(),
                available: has_mod("sglang"),
                detail: if has_mod("sglang") {
                    format!("python module ({python})")
                } else if docker_ok {
                    "not installed natively; Docker recipes still work".into()
                } else {
                    "not installed".into()
                },
            },
        );
        let lmdeploy_bin = which(o.lmdeploy.as_deref().unwrap_or("lmdeploy"));
        engines.insert(
            "lmdeploy".into(),
            match (lmdeploy_bin, has_mod("lmdeploy")) {
                (Some(p), _) => found("lmdeploy", Some(p), ""),
                (None, true) => ToolStatus {
                    name: "lmdeploy".into(),
                    available: true,
                    detail: format!("python module ({python}); run as `lmdeploy` needs its entry point on PATH"),
                },
                (None, false) => ToolStatus {
                    name: "lmdeploy".into(),
                    available: false,
                    detail: if docker_ok {
                        "not installed natively (pip install lmdeploy); Docker recipes still work".into()
                    } else {
                        "not installed (pip install lmdeploy)".into()
                    },
                },
            },
        );
        let tabby = find_tabbyapi(o);
        engines.insert(
            "tabbyapi".into(),
            ToolStatus {
                name: "tabbyapi".into(),
                available: tabby.is_some(),
                detail: match (&tabby, has_mod("exllamav3")) {
                    (Some(d), true) => format!("{} (exllamav3 importable)", d.display()),
                    (Some(d), false) => {
                        format!("{} (exllamav3 not in {python}; TabbyAPI's own venv may have it)", d.display())
                    }
                    (None, _) => "no TabbyAPI checkout found (set engines.tabbyapi_dir)".into(),
                },
            },
        );
        engines.insert("lmstudio".into(), found("lmstudio", find_lms(o), "lms CLI not found"));
        let mlx = which(o.mlx_server.as_deref().unwrap_or("mlx_lm.server"));
        engines.insert(
            "mlx".into(),
            match (mlx, has_mod("mlx_lm")) {
                (Some(p), _) => found("mlx", Some(p), ""),
                (None, true) => {
                    ToolStatus { name: "mlx".into(), available: true, detail: format!("python module ({python})") }
                }
                (None, false) => found("mlx", None, "mlx_lm not installed"),
            },
        );
        let tf = find_tensorfold(o);
        engines.insert(
            "tensorfold".into(),
            ToolStatus {
                name: "tensorfold".into(),
                available: tf.is_some(),
                detail: match &tf {
                    Some(p) => match tensorfold_version(p) {
                        Some(v) => format!("{} ({v})", p.display()),
                        None => p.display().to_string(),
                    },
                    None => {
                        "tensorfold / tensorfold-native not found (curl -fsSL https://tensorfold.dev/install.sh | sh, \
                             or brew install ashhart/tensorfold/tensorfold; or set engines.tensorfold)"
                            .into()
                    }
                },
            },
        );

        tools.insert("docker".into(), docker);
        let hf = which("hf").or_else(|| which("huggingface-cli"));
        tools.insert("hf".into(), found("hf", hf, "Hugging Face CLI not found (pip install -U huggingface_hub)"));
        tools.insert("llama-swap".into(), found("llama-swap", which("llama-swap"), "not on PATH"));
        tools.insert("nvidia-smi".into(), found("nvidia-smi", which("nvidia-smi"), "not on PATH"));
        tools.insert("python".into(), found("python", which(&python), "not on PATH"));
        Inventory { engines, tools }
    }
}

fn python_modules(python: &str, mods: &[&str]) -> Vec<String> {
    let code = format!("import importlib.util as u\nprint(' '.join(m for m in {:?} if u.find_spec(m)))", mods);
    run_with_timeout(Command::new(python).args(["-c", &code]), Duration::from_secs(10))
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

pub(crate) fn find_llama_server(o: &EngineOverrides) -> Option<PathBuf> {
    if let Some(b) = &o.llama_server {
        return which(b);
    }
    which("llama-server")
        .or_else(|| {
            let h = home_dir();
            [h.join("llama.cpp/build/bin/llama-server"), h.join("utils/llama.cpp/build/bin/llama-server")]
                .into_iter()
                .find(|p| which(&p.to_string_lossy()).is_some())
        })
        .or_else(crate::engine_install::managed_llama_server)
}

pub(crate) fn find_tabbyapi(o: &EngineOverrides) -> Option<PathBuf> {
    let h = home_dir();
    let candidates: Vec<PathBuf> = match &o.tabbyapi_dir {
        Some(d) => vec![expand(d)],
        None => vec![h.join("tabbyAPI"), h.join("tabbyapi"), h.join("utils/tabbyAPI")],
    };
    candidates.into_iter().find(|d| d.join("main.py").is_file())
}

pub(crate) fn find_lms(o: &EngineOverrides) -> Option<PathBuf> {
    if let Some(b) = &o.lms {
        return which(b);
    }
    which("lms").or_else(|| {
        let h = home_dir();
        [h.join(".lmstudio/bin/lms"), h.join(".cache/lm-studio/bin/lms")]
            .into_iter()
            .find(|p| which(&p.to_string_lossy()).is_some())
    })
}

/// TensorFold installs as `tensorfold` (Homebrew, or a symlink in `~/.tensorfold/bin` from
/// install.sh); the Linux release archive ships the same binary as `tensorfold-native`.
pub(crate) fn find_tensorfold(o: &EngineOverrides) -> Option<PathBuf> {
    if let Some(b) = &o.tensorfold {
        return which(&expand(b).to_string_lossy());
    }
    which("tensorfold").or_else(|| which("tensorfold-native")).or_else(|| {
        let h = home_dir();
        [h.join(".tensorfold/bin/tensorfold")].into_iter().find(|p| which(&p.to_string_lossy()).is_some())
    })
}

/// `tensorfold-native 1.1.2` → `1.1.2`. `--version` needs no model or GPU driver.
fn tensorfold_version(bin: &Path) -> Option<String> {
    let out = run_with_timeout(Command::new(bin).arg("--version"), Duration::from_secs(5))?;
    parse_tensorfold_version(&out)
}

fn parse_tensorfold_version(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    let (name, v) = line.rsplit_once(' ')?;
    (name.starts_with("tensorfold") && v.chars().next().is_some_and(|c| c.is_ascii_digit())).then(|| v.to_string())
}

/// Inputs to [`plan`].
#[derive(Debug, Clone)]
pub struct PlanCtx {
    pub models_dir: PathBuf,
    pub host: String,
    pub overrides: EngineOverrides,
    pub home: PathBuf,
    /// Whether the recipe's local weights are already on disk.
    pub local_present: bool,
    /// Resolved binaries (from detection) keyed by engine; fall back to names.
    pub bins: BTreeMap<String, String>,
    /// Where `[source]` checkouts live (`<cache>/checkouts`).
    pub checkouts_dir: PathBuf,
    /// The port to plan on, when the caller pre-computed one (`[serve]
    /// port_start` counting); `None` uses the recipe's own default.
    pub port: Option<u16>,
    /// The CX7 fabric shape (`ring`, `switch`, ...) when the caller detected
    /// one: picks `engine.command_by_fabric` and fills `{fabric}`.
    pub fabric: Option<String>,
    /// More placeholder values the caller planned (a wrapped recipe's peers).
    pub vars: BTreeMap<String, String>,
}

impl PlanCtx {
    pub fn new(cfg: &Config, local_present: bool) -> Self {
        let mut bins = BTreeMap::new();
        if let Some(p) = find_llama_server(&cfg.engines) {
            bins.insert("llama.cpp".into(), p.display().to_string());
        }
        if let Some(p) = find_lms(&cfg.engines) {
            bins.insert("lmstudio".into(), p.display().to_string());
        }
        if let Some(p) = find_tensorfold(&cfg.engines) {
            bins.insert("tensorfold".into(), p.display().to_string());
        }
        if let Some(p) = find_tabbyapi(&cfg.engines) {
            bins.insert("tabbyapi_dir".into(), p.display().to_string());
        }
        PlanCtx {
            models_dir: cfg.models_dir(),
            host: cfg.host.clone(),
            overrides: cfg.engines.clone(),
            home: home_dir(),
            local_present,
            bins,
            checkouts_dir: crate::paths::Paths::from_env().checkouts_dir(),
            port: None,
            fabric: None,
            vars: BTreeMap::new(),
        }
    }

    /// [`PlanCtx`] that also fills `vars` (see [`crate::pick::peers`]).
    pub fn with_vars(mut self, vars: BTreeMap<String, String>) -> Self {
        self.vars = vars;
        self
    }

    /// [`PlanCtx`] planning on a pre-computed port (see `deck::planned_ports`).
    pub fn with_port(mut self, port: Option<u16>) -> Self {
        self.port = port;
        self
    }

    pub fn with_fabric(mut self, fabric: Option<String>) -> Self {
        self.fabric = fabric;
        self
    }
}

/// What lmdeck will execute to bring a recipe up.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LaunchPlan {
    /// Commands run to completion first (e.g. `lms server start`).
    pub pre: Vec<Vec<String>>,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// Docker container name, for Docker-backed recipes.
    pub container: Option<String>,
    /// `argv` exits once the model is loaded (LM Studio) instead of serving.
    pub oneshot: bool,
    /// Command that unloads a oneshot model, or a wrapped recipe's `stop`.
    pub stop: Option<Vec<String>>,
    pub port: u16,
    /// `argv` exits 0 once the server is up (wrapped `custom` recipes).
    pub detached: bool,
    /// Streams the server log after a detached `argv` exits.
    pub logs: Option<Vec<String>>,
    /// The `[source]` checkout this plan runs from, if any.
    pub checkout: Option<PathBuf>,
}

impl LaunchPlan {
    /// Shell-quoted command line, for display.
    pub fn command_line(&self) -> String {
        shlex::try_join(self.argv.iter().map(String::as_str)).unwrap_or_else(|_| self.argv.join(" "))
    }
}

fn has_flag(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| names.iter().any(|n| a == n || a.starts_with(&format!("{n}="))))
}

fn substitute(s: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

pub fn container_name(recipe: &str) -> String {
    format!("lmdeck-{recipe}")
}

/// `./start.sh` → `<dir>/start.sh`, so the program is found the same way on
/// every platform (posix_spawn on macOS resolves relative programs against
/// the parent's directory, not the child's).
pub fn resolve_program(argv: &mut [String], cwd: Option<&Path>) {
    if let (Some(first), Some(dir)) = (argv.first_mut(), cwd) {
        if first.contains('/') && Path::new(first.as_str()).is_relative() {
            *first = dir.join(first.trim_start_matches("./")).display().to_string();
        }
    }
}

/// Where a recipe's `[source]` is checked out.
pub fn checkout_dir(checkouts_dir: &Path, recipe: &str) -> PathBuf {
    checkouts_dir.join(recipe)
}

/// Builds the launch plan for `r`.
pub fn plan(r: &Recipe, ctx: &PlanCtx) -> Result<LaunchPlan> {
    if r.nodes() > 1 {
        bail!(
            "'{}' spans {} nodes: each [[nodes]] entry gets its own plan (`lmdeck use {} --dry-run`)",
            r.name,
            r.nodes(),
            r.name
        );
    }
    let port = ctx.port.unwrap_or_else(|| r.port());
    let local = r.weights_path(&ctx.models_dir, &crate::vars::hf_hub(&ctx.home));
    let use_local =
        local.is_some() && (ctx.local_present || r.engine.name.needs_local_weights() || r.model.repo.is_none());
    let repo = r.model.repo.clone().unwrap_or_default();
    let revision = r.model.revision.clone().unwrap_or_default();
    // The model argument handed to repo-aware engines.
    let model_arg =
        if use_local { local.as_ref().map(|p| p.display().to_string()).unwrap_or_default() } else { repo.clone() };
    let mut vars: BTreeMap<&str, String> = BTreeMap::new();
    vars.insert("name", r.name.clone());
    vars.insert("port", port.to_string());
    vars.insert("ctx", r.engine.ctx.map(|c| c.to_string()).unwrap_or_default());
    vars.insert("model", model_arg.clone());
    vars.insert("models_dir", ctx.models_dir.display().to_string());
    vars.insert("host", ctx.host.clone());
    vars.insert("repo", repo.clone());
    vars.insert("revision", revision.clone());
    vars.insert("fabric", ctx.fabric.clone().unwrap_or_default());
    vars.extend(crate::vars::machine_vars(r, &ctx.models_dir, &ctx.overrides, &ctx.home)?);
    vars.extend(ctx.vars.iter().map(|(k, v)| (k.as_str(), v.clone())));
    // The working directory: a [source] checkout (a relative `dir` resolves
    // inside it), else `dir` as written.
    let checkout = r.source.as_ref().map(|_| checkout_dir(&ctx.checkouts_dir, &r.name));
    let work_dir: Option<PathBuf> = match (&checkout, r.engine.dir.as_deref()) {
        (Some(co), Some(d)) if !expand(d).is_absolute() => Some(co.join(d)),
        (Some(co), None) => Some(co.clone()),
        (_, Some(d)) => Some(expand(d)),
        (None, None) => None,
    };
    if r.engine.name == Engine::Custom {
        vars.insert("dir", work_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default());
        vars.insert("container", r.engine.container.clone().unwrap_or_default());
    }
    let args: Vec<String> = r.engine.args.iter().map(|a| substitute(a, &vars)).collect();
    let env: BTreeMap<String, String> = r.engine.env.iter().map(|(k, v)| (k.clone(), substitute(v, &vars))).collect();
    let ctx_len = r.engine.ctx;
    let docker_bin = ctx.overrides.docker.clone().unwrap_or_else(|| "docker".into());
    let mut plan = LaunchPlan {
        pre: vec![],
        argv: vec![],
        cwd: None,
        env: BTreeMap::new(),
        container: None,
        oneshot: false,
        stop: None,
        port,
        detached: false,
        logs: None,
        checkout,
    };
    let host = ctx.host.clone();
    let bin_or = |engine: Engine, cfg: &Option<String>, default: &str| -> String {
        r.engine
            .bin
            .clone()
            .or_else(|| cfg.clone())
            .or_else(|| ctx.bins.get(engine.as_str()).cloned())
            .unwrap_or_else(|| default.to_string())
    };

    match r.engine.name {
        Engine::LlamaCpp => {
            let mut a = vec![bin_or(Engine::LlamaCpp, &ctx.overrides.llama_server, "llama-server")];
            if use_local {
                a.extend(["--model".into(), model_arg.clone()]);
            } else if let (false, Some(file)) = (repo.is_empty(), r.model.files.first()) {
                // llama.cpp downloads into its own cache when the file is not local yet.
                a.extend(["--hf-repo".into(), repo.clone(), "--hf-file".into(), file.clone()]);
            } else {
                bail!("llama.cpp recipe '{}' has neither a local GGUF nor an hf repo + file", r.name);
            }
            if !has_flag(&args, &["--alias", "-a"]) {
                a.extend(["--alias".into(), r.name.clone()]);
            }
            if !has_flag(&args, &["--host"]) {
                a.extend(["--host".into(), host.clone()]);
            }
            if !has_flag(&args, &["--port"]) {
                a.extend(["--port".into(), port.to_string()]);
            }
            if let Some(c) = ctx_len.filter(|_| !has_flag(&args, &["--ctx-size", "-c"])) {
                a.extend(["--ctx-size".into(), c.to_string()]);
            }
            if !has_flag(&args, &["--n-gpu-layers", "-ngl", "--gpu-layers"]) {
                a.extend(["--n-gpu-layers".into(), "99".into()]);
            }
            // Prometheus /metrics, so telemetry can show real tokens/s.
            if !has_flag(&args, &["--metrics"]) {
                a.push("--metrics".into());
            }
            a.extend(args);
            plan.argv = a;
            plan.env = env;
        }
        Engine::Vllm | Engine::Sglang | Engine::LmDeploy => {
            let engine = r.engine.name;
            let vllm = engine == Engine::Vllm;
            let lmdeploy = engine == Engine::LmDeploy;
            // Flag names per engine: (model-name, host, port, context).
            let (name_flag, host_flag, port_flag, ctx_flag) = match engine {
                Engine::Vllm => ("--served-model-name", "--host", "--port", "--max-model-len"),
                Engine::Sglang => ("--served-model-name", "--host", "--port", "--context-length"),
                _ => ("--model-name", "--server-name", "--server-port", "--session-len"),
            };
            let mut serve_args: Vec<String> = vec![];
            if engine == Engine::Sglang {
                serve_args.extend(["--model-path".into(), model_arg.clone()]);
            } else {
                serve_args.push(model_arg.clone());
            }
            if !has_flag(&args, &[name_flag]) {
                serve_args.extend([name_flag.into(), r.name.clone()]);
            }
            if !has_flag(&args, &[host_flag]) {
                serve_args.extend([host_flag.into(), host.clone()]);
            }
            if !has_flag(&args, &[port_flag]) {
                serve_args.extend([port_flag.into(), port.to_string()]);
            }
            if let Some(c) = ctx_len.filter(|_| !has_flag(&args, &[ctx_flag])) {
                serve_args.extend([ctx_flag.into(), c.to_string()]);
            }
            if !use_local && !revision.is_empty() && !has_flag(&args, &["--revision"]) {
                serve_args.extend(["--revision".into(), revision.clone()]);
            }
            serve_args.extend(args);

            if let Some(image) = &r.engine.image {
                let name = container_name(&r.name);
                let hf_home = crate::vars::hf_home(&ctx.home);
                let mut a: Vec<String> = vec![
                    docker_bin,
                    "run".into(),
                    "--rm".into(),
                    "--name".into(),
                    name.clone(),
                    "--gpus".into(),
                    "all".into(),
                    "--network".into(),
                    "host".into(),
                    "--ipc".into(),
                    "host".into(),
                ];
                match &r.model.nfs_volume {
                    // Weights streamed read-only from their home over NFS (placement.rs).
                    Some(nfs) => a.extend([
                        "-v".into(),
                        format!("{}:{}:ro", nfs.volume, nfs.mount_at),
                        "-e".into(),
                        "HF_HUB_OFFLINE=1".into(),
                    ]),
                    None => a.extend(["-v".into(), format!("{}:/root/.cache/huggingface", hf_home.display())]),
                }
                if use_local {
                    a.extend(["-v".into(), format!("{model_arg}:{model_arg}:ro")]);
                }
                for v in &r.engine.volumes {
                    let v = substitute(v, &vars);
                    let expanded = match v.split_once(':') {
                        Some((h, rest)) => format!("{}:{rest}", expand(h).display()),
                        None => v,
                    };
                    a.extend(["-v".into(), expanded]);
                }
                for (k, v) in &env {
                    a.extend(["-e".into(), format!("{k}={v}")]);
                }
                // Forward the token by name only: its value never lands in argv or logs.
                if std::env::var_os("HF_TOKEN").is_some() && !env.contains_key("HF_TOKEN") {
                    a.extend(["-e".into(), "HF_TOKEN".into()]);
                }
                let ep = r.engine.entrypoint.clone();
                if ep.is_empty() && lmdeploy {
                    // openmmlab/lmdeploy images have no serving entrypoint.
                    a.push(image.clone());
                    a.extend(["lmdeploy".into(), "serve".into(), "api_server".into()]);
                } else if ep.is_empty() && !vllm {
                    a.push(image.clone());
                    a.extend(["python3".into(), "-m".into(), "sglang.launch_server".into()]);
                } else if ep.is_empty() {
                    // The vllm/vllm-openai image's ENTRYPOINT is already `vllm serve`.
                    a.push(image.clone());
                } else {
                    a.extend(["--entrypoint".into(), ep[0].clone(), image.clone()]);
                    a.extend(ep[1..].iter().cloned());
                }
                a.extend(serve_args);
                plan.argv = a;
                plan.container = Some(name);
            } else if lmdeploy {
                let mut a = vec![
                    bin_or(Engine::LmDeploy, &ctx.overrides.lmdeploy, "lmdeploy"),
                    "serve".into(),
                    "api_server".into(),
                ];
                a.extend(serve_args);
                plan.argv = a;
                plan.env = env;
            } else if vllm {
                let mut a = vec![bin_or(Engine::Vllm, &ctx.overrides.vllm, "vllm"), "serve".into()];
                a.extend(serve_args);
                plan.argv = a;
                plan.env = env;
            } else {
                let py =
                    r.engine.bin.clone().or_else(|| ctx.overrides.python.clone()).unwrap_or_else(|| "python3".into());
                let mut a = vec![py, "-m".into(), "sglang.launch_server".into()];
                a.extend(serve_args);
                plan.argv = a;
                plan.env = env;
            }
        }
        Engine::TabbyApi => {
            let Some(dir) = local.filter(|_| use_local) else {
                bail!("tabbyapi needs the weights on disk");
            };
            let tabby_dir = r
                .engine
                .dir
                .as_deref()
                .map(expand)
                .or_else(|| ctx.overrides.tabbyapi_dir.as_deref().map(expand))
                .or_else(|| ctx.bins.get("tabbyapi_dir").map(PathBuf::from))
                .unwrap_or_else(|| ctx.home.join("tabbyAPI"));
            let venv_py = tabby_dir.join("venv/bin/python");
            let py = r
                .engine
                .bin
                .clone()
                .or_else(|| venv_py.is_file().then(|| venv_py.display().to_string()))
                .or_else(|| ctx.overrides.python.clone())
                .unwrap_or_else(|| "python3".into());
            let parent = dir.parent().unwrap_or(Path::new("/")).display().to_string();
            let base = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let mut a = vec![py, "main.py".into()];
            if !has_flag(&args, &["--host"]) {
                a.extend(["--host".into(), host.clone()]);
            }
            if !has_flag(&args, &["--port"]) {
                a.extend(["--port".into(), port.to_string()]);
            }
            a.extend(["--model-dir".into(), parent, "--model-name".into(), base]);
            if !has_flag(&args, &["--backend"]) {
                a.extend(["--backend".into(), "exllamav3".into()]);
            }
            if let Some(c) = ctx_len.filter(|_| !has_flag(&args, &["--max-seq-len"])) {
                a.extend(["--max-seq-len".into(), c.to_string()]);
            }
            a.extend(args);
            plan.argv = a;
            plan.cwd = Some(tabby_dir);
            plan.env = env;
        }
        Engine::LmStudio => {
            let lms = bin_or(Engine::LmStudio, &ctx.overrides.lms, "lms");
            let key = if use_local { model_arg.clone() } else { repo.clone() };
            plan.pre = vec![vec![lms.clone(), "server".into(), "start".into(), "--port".into(), port.to_string()]];
            let mut a = vec![lms.clone(), "load".into(), key, "--identifier".into(), r.name.clone(), "-y".into()];
            if let Some(c) = ctx_len.filter(|_| !has_flag(&args, &["--context-length"])) {
                a.extend(["--context-length".into(), c.to_string()]);
            }
            a.extend(args);
            plan.argv = a;
            plan.oneshot = true;
            plan.stop = Some(vec![lms, "unload".into(), r.name.clone()]);
            plan.env = env;
        }
        Engine::Mlx => {
            let mut a =
                vec![bin_or(Engine::Mlx, &ctx.overrides.mlx_server, "mlx_lm.server"), "--model".into(), model_arg];
            if !has_flag(&args, &["--host"]) {
                a.extend(["--host".into(), host.clone()]);
            }
            if !has_flag(&args, &["--port"]) {
                a.extend(["--port".into(), port.to_string()]);
            }
            a.extend(args);
            plan.argv = a;
            plan.env = env;
        }
        Engine::TensorFold => {
            // `tensorfold serve` takes a repo id already in the HF cache (`tensorfold pull`
            // put it there, at the recipe's revision) or a checkpoint directory. It never
            // downloads, so there is no revision flag here.
            if model_arg.is_empty() {
                bail!("tensorfold recipe '{}' has neither an hf repo nor a local checkpoint path", r.name);
            }
            let mut a =
                vec![bin_or(Engine::TensorFold, &ctx.overrides.tensorfold, "tensorfold"), "serve".into(), model_arg];
            if !has_flag(&args, &["--name"]) {
                a.extend(["--name".into(), r.name.clone()]);
            }
            if !has_flag(&args, &["--host"]) {
                a.extend(["--host".into(), host.clone()]);
            }
            if !has_flag(&args, &["--port"]) {
                a.extend(["--port".into(), port.to_string()]);
            }
            if let Some(c) = ctx_len.filter(|_| !has_flag(&args, &["--context"])) {
                a.extend(["--context".into(), c.to_string()]);
            }
            a.extend(args);
            plan.argv = a;
            plan.env = env;
        }
        Engine::Custom => {
            // `args` are appended only when the recipe gives some.
            // A per-fabric command (a switchless ring's own start script) wins.
            let command =
                ctx.fabric.as_ref().and_then(|f| r.engine.command_by_fabric.get(f)).unwrap_or(&r.engine.command);
            let mut a: Vec<String> = command.iter().map(|c| substitute(c, &vars)).collect();
            a.extend(args);
            plan.argv = a;
            plan.cwd = work_dir;
            plan.env = env;
            plan.detached = r.engine.detached;
            plan.container = r.engine.container.clone();
            if !r.engine.stop.is_empty() {
                plan.stop = Some(r.engine.stop.iter().map(|c| substitute(c, &vars)).collect());
            }
            if !r.engine.logs.is_empty() {
                plan.logs = Some(r.engine.logs.iter().map(|c| substitute(c, &vars)).collect());
            }
        }
    }
    crate::vars::check_plan(&r.name, &plan)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(local_present: bool) -> PlanCtx {
        PlanCtx {
            models_dir: PathBuf::from("/m"),
            host: "0.0.0.0".into(),
            overrides: EngineOverrides::default(),
            home: PathBuf::from("/home/j"),
            local_present,
            bins: BTreeMap::new(),
            checkouts_dir: PathBuf::from("/c/checkouts"),
            port: None,
            fabric: None,
            vars: BTreeMap::new(),
        }
    }

    #[test]
    fn command_follows_the_fabric() {
        let r = recipe(
            "[engine]\nname = \"custom\"\ncommand = [\"./start.sh\", \"{fabric}\"]\ndetached = true\n\
             [engine.command_by_fabric]\nring = [\"./start-switchless.sh\"]\n",
        );
        let with = |f: Option<&str>| PlanCtx { fabric: f.map(String::from), ..ctx(false) };
        assert_eq!(plan(&r, &with(Some("ring"))).unwrap().argv, ["./start-switchless.sh"]);
        assert_eq!(plan(&r, &with(Some("switch"))).unwrap().argv, ["./start.sh", "switch"]);
        assert_eq!(plan(&r, &with(None)).unwrap().argv, ["./start.sh", ""]);
        let bad = Recipe::parse(
            "[engine]\nname = \"custom\"\ncommand = [\"x\"]\n[engine.command_by_fabric]\nloop = [\"y\"]\n",
            "t",
        );
        assert!(format!("{:#}", bad.unwrap_err()).contains("unknown fabric 'loop'"));
        let bad = Recipe::parse(
            "[engine]\nname = \"vllm\"\n[model]\nrepo = \"a/b\"\n[engine.command_by_fabric]\nring = [\"y\"]\n",
            "t",
        );
        assert!(bad.is_err(), "command_by_fabric is for custom engines");
    }

    fn recipe(t: &str) -> Recipe {
        Recipe::parse(t, "test").unwrap()
    }

    const GGUF: &str = "[model]\nrepo = \"unsloth/Qwen3-4B-GGUF\"\nfiles = [\"Qwen3-4B-Q4_K_M.gguf\"]\n[engine]\nname = \"llama.cpp\"\nport = 8085\nctx = 32768\nargs = [\"-fa\", \"on\", \"--mmproj\", \"{models_dir}/x.gguf\"]\n";

    #[test]
    fn llama_cpp_local_and_remote() {
        let r = recipe(GGUF);
        let p = plan(&r, &ctx(true)).unwrap();
        assert_eq!(
            p.argv,
            [
                "llama-server",
                "--model",
                "/m/unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf",
                "--alias",
                "test",
                "--host",
                "0.0.0.0",
                "--port",
                "8085",
                "--ctx-size",
                "32768",
                "--n-gpu-layers",
                "99",
                "--metrics",
                "-fa",
                "on",
                "--mmproj",
                "/m/x.gguf"
            ]
        );
        let p = plan(&r, &ctx(false)).unwrap();
        assert_eq!(&p.argv[1..5], ["--hf-repo", "unsloth/Qwen3-4B-GGUF", "--hf-file", "Qwen3-4B-Q4_K_M.gguf"]);
        assert!(!p.oneshot && p.container.is_none());
    }

    #[test]
    fn user_args_suppress_defaults() {
        let r = recipe("[model]\nsource=\"local\"\npath=\"a.gguf\"\n[engine]\nname=\"llama.cpp\"\nargs=[\"-ngl\",\"20\",\"--port=9\"]\n");
        let p = plan(&r, &ctx(true)).unwrap();
        assert!(!p.argv.contains(&"--n-gpu-layers".to_string()));
        assert!(!p.argv.contains(&"--port".to_string()));
        assert!(p.argv.contains(&"--port=9".to_string()));
    }

    #[test]
    fn vllm_native_uses_repo_and_revision() {
        let r = recipe("[model]\nrepo=\"nvidia/Qwen3.6-35B-A3B-NVFP4\"\nrevision=\"abc\"\n[engine]\nname=\"vllm\"\nctx=65536\nargs=[\"--gpu-memory-utilization\",\"0.6\"]\n");
        let p = plan(&r, &ctx(false)).unwrap();
        assert_eq!(
            p.argv,
            [
                "vllm",
                "serve",
                "nvidia/Qwen3.6-35B-A3B-NVFP4",
                "--served-model-name",
                "test",
                "--host",
                "0.0.0.0",
                "--port",
                "8000",
                "--max-model-len",
                "65536",
                "--revision",
                "abc",
                "--gpu-memory-utilization",
                "0.6"
            ]
        );
    }

    #[test]
    fn vllm_docker() {
        let r = recipe("[model]\nsource=\"local\"\npath=\"nvidia/q\"\n[engine]\nname=\"vllm\"\nport=8114\nimage=\"vllm/vllm-openai:latest\"\nvolumes=[\"/a:/b:ro\"]\n[engine.env]\nFOO=\"1\"\n");
        let p = plan(&r, &ctx(true)).unwrap();
        let line = p.command_line();
        assert!(line.starts_with("docker run --rm --name lmdeck-test --gpus all --network host --ipc host"), "{line}");
        assert!(line.contains("-v /m/nvidia/q:/m/nvidia/q:ro"));
        assert!(line.contains("-v /a:/b:ro -e 'FOO=1'"), "{line}");
        assert!(line.contains("vllm/vllm-openai:latest /m/nvidia/q --served-model-name test"));
        assert_eq!(p.container.as_deref(), Some("lmdeck-test"));
        assert!(p.env.is_empty(), "docker env goes through -e");
    }

    #[test]
    fn docker_entrypoint_override() {
        let r =
            recipe("[model]\nrepo=\"a/b\"\n[engine]\nname=\"vllm\"\nimage=\"img\"\nentrypoint=[\"vllm\",\"serve\"]\n");
        let line = plan(&r, &ctx(false)).unwrap().command_line();
        assert!(line.contains("--entrypoint vllm img serve a/b"), "{line}");
    }

    #[test]
    fn sglang_native_and_docker() {
        let r = recipe("[model]\nrepo=\"a/b\"\n[engine]\nname=\"sglang\"\nctx=4096\n");
        let p = plan(&r, &ctx(false)).unwrap();
        assert_eq!(&p.argv[..5], ["python3", "-m", "sglang.launch_server", "--model-path", "a/b"]);
        assert!(p.command_line().contains("--context-length 4096"));
        let r = recipe("[model]\nrepo=\"a/b\"\n[engine]\nname=\"sglang\"\nimage=\"lmsysorg/sglang:latest\"\n");
        let line = plan(&r, &ctx(false)).unwrap().command_line();
        assert!(line.contains("lmsysorg/sglang:latest python3 -m sglang.launch_server --model-path a/b"), "{line}");
    }

    #[test]
    fn tabbyapi_exl3() {
        let r = recipe("[model]\nrepo=\"turboderp/Qwen3-30B-A3B-exl3\"\nrevision=\"4.0bpw\"\n[engine]\nname=\"tabbyapi\"\nctx=32768\nargs=[\"--cache-mode\",\"Q8\"]\n");
        let p = plan(&r, &ctx(true)).unwrap();
        assert_eq!(p.cwd, Some(PathBuf::from("/home/j/tabbyAPI")));
        assert_eq!(
            p.argv,
            [
                "python3",
                "main.py",
                "--host",
                "0.0.0.0",
                "--port",
                "5000",
                "--model-dir",
                "/m/turboderp",
                "--model-name",
                "Qwen3-30B-A3B-exl3-4.0bpw",
                "--backend",
                "exllamav3",
                "--max-seq-len",
                "32768",
                "--cache-mode",
                "Q8"
            ]
        );
    }

    #[test]
    fn lmstudio_is_oneshot_with_unload() {
        let r = recipe("[model]\nrepo=\"qwen/qwen3-4b\"\n[engine]\nname=\"lmstudio\"\nctx=8192\n");
        let p = plan(&r, &ctx(false)).unwrap();
        assert!(p.oneshot);
        assert_eq!(p.pre[0], ["lms", "server", "start", "--port", "1234"]);
        assert_eq!(p.argv, ["lms", "load", "qwen/qwen3-4b", "--identifier", "test", "-y", "--context-length", "8192"]);
        assert_eq!(p.stop.unwrap(), ["lms", "unload", "test"]);
    }

    #[test]
    fn mlx_and_custom_placeholders() {
        let r = recipe("[model]\nrepo=\"mlx-community/Qwen3-4B-4bit\"\n[engine]\nname=\"mlx\"\n");
        assert_eq!(
            plan(&r, &ctx(false)).unwrap().argv,
            ["mlx_lm.server", "--model", "mlx-community/Qwen3-4B-4bit", "--host", "0.0.0.0", "--port", "8080"]
        );
        let r = recipe("[model]\nrepo=\"a/b\"\n[engine]\nname=\"custom\"\nport=9001\ncommand=[\"srv\",\"--m\",\"{model}\",\"-p\",\"{port}\",\"{name}\"]\n");
        assert_eq!(plan(&r, &ctx(false)).unwrap().argv, ["srv", "--m", "a/b", "-p", "9001", "test"]);
    }

    #[test]
    fn tensorfold_serves_cached_repo_or_local_dir() {
        let r = recipe("[model]\nrepo=\"mlx-community/Qwen3.5-2B-MLX-4bit\"\nrevision=\"93760be\"\n[engine]\nname=\"tensorfold\"\nctx=8192\nargs=[\"--no-thinking\"]\n");
        let p = plan(&r, &ctx(false)).unwrap();
        assert_eq!(
            p.argv,
            [
                "tensorfold",
                "serve",
                "mlx-community/Qwen3.5-2B-MLX-4bit",
                "--name",
                "test",
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--context",
                "8192",
                "--no-thinking"
            ]
        );
        assert!(!p.oneshot && p.container.is_none() && p.stop.is_none());
        assert!(!p.argv.iter().any(|a| a.contains("93760be")), "serve takes no revision");

        let r = recipe("[model]\nsource=\"local\"\npath=\"~/models/nemotron-living\"\n[engine]\nname=\"tf\"\nport=8091\nbin=\"/opt/tf/bin/tensorfold-native\"\nargs=[\"--name\",\"local-model\",\"--host=127.0.0.1\"]\n");
        assert_eq!(r.engine.name, Engine::TensorFold);
        let p = plan(&r, &ctx(true)).unwrap();
        assert_eq!(p.argv[0], "/opt/tf/bin/tensorfold-native");
        assert_eq!(p.argv[1], "serve");
        assert!(p.argv[2].ends_with("models/nemotron-living"), "{:?}", p.argv);
        assert_eq!(&p.argv[3..], ["--port", "8091", "--name", "local-model", "--host=127.0.0.1"]);
    }

    #[test]
    fn tensorfold_version_parsing() {
        assert_eq!(parse_tensorfold_version("tensorfold-native 1.1.2\n").as_deref(), Some("1.1.2"));
        assert_eq!(parse_tensorfold_version("tensorfold 1.0.8").as_deref(), Some("1.0.8"));
        assert_eq!(parse_tensorfold_version("usage: something"), None);
        assert_eq!(parse_tensorfold_version(""), None);
    }

    #[test]
    fn lmdeploy_native_and_docker() {
        let r = recipe("[model]\nrepo=\"org/Model-AWQ\"\nrevision=\"r1\"\n[engine]\nname=\"lmdeploy\"\nctx=32768\nargs=[\"--model-format\",\"awq\"]\n");
        let p = plan(&r, &ctx(false)).unwrap();
        assert_eq!(
            p.argv,
            [
                "lmdeploy",
                "serve",
                "api_server",
                "org/Model-AWQ",
                "--model-name",
                "test",
                "--server-name",
                "0.0.0.0",
                "--server-port",
                "23333",
                "--session-len",
                "32768",
                "--revision",
                "r1",
                "--model-format",
                "awq"
            ]
        );
        let r = recipe("[model]\nrepo=\"a/b\"\n[engine]\nname=\"lmdeploy\"\nimage=\"openmmlab/lmdeploy:latest\"\nargs=[\"--server-port\",\"9000\"]\n");
        let p = plan(&r, &ctx(false)).unwrap();
        let line = p.command_line();
        assert!(line.starts_with("docker run --rm --name lmdeck-test"), "{line}");
        assert!(
            line.contains("openmmlab/lmdeploy:latest lmdeploy serve api_server a/b --model-name test --server-name 0.0.0.0 --server-port 9000"),
            "{line}"
        );
        assert_eq!(p.container.as_deref(), Some("lmdeck-test"));
    }

    #[test]
    fn custom_wrapper_runs_from_checkout() {
        let r = recipe("[source]\nrepo=\"https://example.com/srv.git\"\n[engine]\nname=\"custom\"\ncommand=[\"./start.sh\"]\nstop=[\"./stop.sh\",\"{container}\"]\ndetached=true\nport=8888\ncontainer=\"srv\"\nlogs=[\"docker\",\"logs\",\"-f\",\"{container}\"]\nenv={PORT=\"{port}\",WHERE=\"{dir}\"}\n");
        let p = plan(&r, &ctx(false)).unwrap();
        assert_eq!(p.argv, ["./start.sh"], "no args appended");
        assert_eq!(p.cwd, Some(PathBuf::from("/c/checkouts/test")));
        assert_eq!(p.checkout, Some(PathBuf::from("/c/checkouts/test")));
        assert_eq!(p.stop.as_deref().unwrap(), ["./stop.sh", "srv"]);
        assert_eq!(p.logs.as_deref().unwrap(), ["docker", "logs", "-f", "srv"]);
        assert_eq!(p.env["PORT"], "8888");
        assert_eq!(p.env["WHERE"], "/c/checkouts/test");
        assert!(p.detached && !p.oneshot);
        assert_eq!(p.container.as_deref(), Some("srv"));

        let r = recipe("[source]\nrepo=\"x\"\n[engine]\nname=\"custom\"\ndir=\"server\"\ncommand=[\"./run\"]\n");
        assert_eq!(plan(&r, &ctx(false)).unwrap().cwd, Some(PathBuf::from("/c/checkouts/test/server")));
        let r = recipe("[source]\nrepo=\"x\"\n[engine]\nname=\"custom\"\ndir=\"/opt/s\"\ncommand=[\"./run\"]\n");
        assert_eq!(plan(&r, &ctx(false)).unwrap().cwd, Some(PathBuf::from("/opt/s")));

        let mut argv = vec!["./start.sh".to_string(), "x".into()];
        resolve_program(&mut argv, Some(Path::new("/c/co")));
        assert_eq!(argv, ["/c/co/start.sh", "x"]);
        let mut argv = vec!["docker".to_string()];
        resolve_program(&mut argv, Some(Path::new("/c/co")));
        assert_eq!(argv, ["docker"]);
    }

    #[test]
    fn multi_node_refused() {
        let r = recipe("[model]\nrepo=\"a/b\"\n[engine]\nname=\"vllm\"\n[hardware]\nnodes=2\n");
        assert!(plan(&r, &ctx(false)).unwrap_err().to_string().contains("spans 2 nodes"));
    }
}
