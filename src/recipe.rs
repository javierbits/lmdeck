//! Recipes: a model source + quant + engine + hardware requirements.
//!
//! Recipes are TOML, and TOML only: a `.yaml`/`.yml` file is rejected with
//! an error that says so. A recipe either launches an engine lmdeck knows
//! (llama.cpp, vLLM, SGLang, LMDeploy, …) or wraps someone else's scripts on
//! the `custom` engine (`command` / `stop` / `detached`, plus a `[source]`
//! git checkout to run them from).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::paths::expand;

/// Inference engines lmdeck knows how to launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Engine {
    /// `llama-server` from llama.cpp (GGUF).
    LlamaCpp,
    /// `vllm serve`, natively or in Docker.
    Vllm,
    /// SGLang `launch_server`, natively or in Docker.
    Sglang,
    /// TabbyAPI with the exllamav3 backend (EXL3 / EXL2).
    TabbyApi,
    /// LM Studio via its `lms` CLI.
    LmStudio,
    /// Apple MLX via `mlx_lm.server`.
    Mlx,
    /// TensorFold (github.com/ashhart/TensorFold): native Zig server for Apple Metal and NVIDIA CUDA.
    TensorFold,
    /// LMDeploy `lmdeploy serve api_server` (TurboMind / PyTorch backends), natively or in Docker.
    LmDeploy,
    /// Anything else: `engine.command` is run verbatim.
    Custom,
}

impl Engine {
    pub const ALL: [Engine; 9] = [
        Engine::LlamaCpp,
        Engine::Vllm,
        Engine::Sglang,
        Engine::TabbyApi,
        Engine::LmStudio,
        Engine::Mlx,
        Engine::TensorFold,
        Engine::LmDeploy,
        Engine::Custom,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Engine::LlamaCpp => "llama.cpp",
            Engine::Vllm => "vllm",
            Engine::Sglang => "sglang",
            Engine::TabbyApi => "tabbyapi",
            Engine::LmStudio => "lmstudio",
            Engine::Mlx => "mlx",
            Engine::TensorFold => "tensorfold",
            Engine::LmDeploy => "lmdeploy",
            Engine::Custom => "custom",
        }
    }

    /// Port used when a recipe does not set one (each engine's own default).
    pub fn default_port(&self) -> u16 {
        match self {
            Engine::LlamaCpp | Engine::Mlx | Engine::TensorFold | Engine::Custom => 8080,
            Engine::Vllm => 8000,
            Engine::Sglang => 30000,
            Engine::TabbyApi => 5000,
            Engine::LmStudio => 1234,
            Engine::LmDeploy => 23333,
        }
    }

    /// Engines that need weights on local disk (cannot stream from a repo id).
    pub fn needs_local_weights(&self) -> bool {
        matches!(self, Engine::TabbyApi)
    }

    /// Engines that load from local disk or the Hugging Face cache but never
    /// download at startup, so `lmdeck pull` must run first.
    pub fn needs_downloaded_weights(&self) -> bool {
        matches!(self, Engine::TabbyApi | Engine::TensorFold)
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Engine {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "llama.cpp" | "llamacpp" | "llama-cpp" | "llama" | "llama-server" | "gguf" => Engine::LlamaCpp,
            "vllm" => Engine::Vllm,
            "sglang" => Engine::Sglang,
            "tabbyapi" | "tabby" | "exllamav3" | "exl3" => Engine::TabbyApi,
            "lmstudio" | "lm-studio" | "lms" => Engine::LmStudio,
            "mlx" | "mlx-lm" | "mlx_lm" => Engine::Mlx,
            "tensorfold" | "tensor-fold" | "tensorfold-native" | "tf" => Engine::TensorFold,
            "lmdeploy" | "lm-deploy" | "lmdeploy-serve" | "turbomind" => Engine::LmDeploy,
            "custom" => Engine::Custom,
            other => {
                bail!("unknown engine '{other}' (expected one of: {})", Engine::ALL.map(|e| e.as_str()).join(", "))
            }
        })
    }
}

impl TryFrom<String> for Engine {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Engine> for String {
    fn from(e: Engine) -> String {
        e.as_str().to_string()
    }
}

/// Where the weights come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    /// A Hugging Face model repo (`repo`, optional `revision` / `files`).
    #[default]
    Hf,
    /// Already on disk at `path` (relative to `models_dir`, or absolute).
    Local,
}

/// The weights. Optional for `custom` recipes whose scripts fetch their own.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelSpec {
    #[serde(default)]
    pub source: ModelSource,
    /// Hugging Face repo id, e.g. `unsloth/Qwen3-4B-GGUF`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Branch, tag or commit (EXL3 repos keep each bpw on its own branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Files (or globs) to fetch from the repo. For GGUF the first entry is
    /// the file to load (first shard for split GGUFs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// Local path, relative to `models_dir` unless absolute / `~`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Approximate download size in GB (informational, used for memory estimates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_gb: Option<f64>,
    /// The host whose disk holds the weights (its `hosts.toml` name, or this
    /// machine's `[node] name`). Default: this machine if it has them, else
    /// the first host that does (`lmdeck weights`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Set by lmdeck when a recipe runs away from its weights: a Docker
    /// volume (an NFS mount of the home's HF cache, read-only) mounted in
    /// place of the local Hugging Face cache. Not for hand-written recipes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfs_volume: Option<NfsMount>,
}

/// How an NFS weights volume is mounted in the container.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NfsMount {
    /// Docker volume name.
    pub volume: String,
    /// Container path (`/root/.cache/huggingface`, the same on every node).
    pub mount_at: String,
}

impl ModelSpec {
    /// No repo and no path: the recipe names no weights of its own.
    pub fn is_empty(&self) -> bool {
        self.repo.as_deref().unwrap_or("").is_empty() && self.path.as_deref().unwrap_or("").is_empty()
    }
}

/// A git checkout lmdeck keeps in its cache (`<cache>/checkouts/<recipe>`)
/// so a `custom` recipe can run a repo's own scripts.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitSource {
    /// Anything `git clone` accepts: an https URL, `git@host:org/repo`, a local path.
    pub repo: String,
    /// Branch, tag or commit. Empty = the remote's default branch.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// For a commit or tag `ref`: the branch whose newer commits count as an
    /// update (default: the remote's default branch). Unused for branch refs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Quant {
    /// `gguf`, `exl3`, `exl2`, `nvfp4`, `mxfp4`, `fp8`, `awq`, `gptq`, `mlx`, `bf16`, …
    pub format: String,
    /// Bits per weight, when meaningful (EXL3 bpw, MLX bits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bits: Option<f64>,
    /// Free-form label, e.g. `Q4_K_M` or `4.0bpw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Quant {
    /// Short human label: `gguf Q4_K_M`, `exl3 4.0bpw`, `nvfp4`.
    pub fn short(&self) -> String {
        match (&self.label, self.bits) {
            (Some(l), _) => format!("{} {}", self.format, l),
            (None, Some(b)) => format!("{} {}b", self.format, b),
            _ => self.format.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineSpec {
    pub name: Engine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Context length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<u64>,
    /// Extra engine flags, appended last. Placeholders: `{name}`, `{port}`,
    /// `{ctx}`, `{model}`, `{models_dir}`, `{host}`, `{repo}`, `{revision}`,
    /// `{dir}` (the working directory), `{container}`, `{user}` / `{home}`
    /// (this machine's login name and home directory), what it has configured
    /// (`{hf_home}`, the `[engines]` binaries and their `_dir`s, see
    /// [`crate::vars`]), `{model_dir}` (where its weights are on it) and, with
    /// `hardware.hosts`, the peers' `{peer_addr}` / `{peer_host}`, … (README).
    /// A placeholder nothing fills fails the plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment for the server process (or container). Values take the
    /// same placeholders, e.g. `PORT = "{port}"` for wrapped scripts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Run in Docker with this image (vllm / sglang).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Docker entrypoint override, e.g. `["vllm", "serve"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    /// Extra docker `-v host:container[:ro]` mounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// Binary override for this recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// Working directory (TabbyAPI checkout for `tabbyapi`). With a
    /// `[source]` it defaults to the checkout, and a relative `dir` is
    /// resolved inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Full argv for `custom` engines (placeholders as in `args`). A relative
    /// program (`./start.sh`) is resolved against the working directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// `custom`: a different `command` per CX7 fabric shape, detected across
    /// this machine and hosts.toml at start (`LMDECK_FABRIC` sets it by hand).
    /// Keys: `ring`, `switch`, `mesh`, `cable`, `single`, `mixed`, `none`;
    /// `command` covers every shape left out. Mia's 4-Spark recipe runs
    /// `./start-switchless.sh` on a ring.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub command_by_fabric: BTreeMap<String, Vec<String>>,
    /// `custom`: argv that stops the server (`["./stop.sh"]`). Runs with the
    /// same working directory and environment as `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// `custom`: `command` exits 0 once the server is up (it backgrounds or
    /// `docker run -d`s it). A clean exit is not a crash; liveness comes from
    /// `container` when set, otherwise from probing `port`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// `custom`: Docker container the scripts run the server in. Used for
    /// liveness and as the `docker stop` fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// `custom` + `detached`: argv that streams the server's log once
    /// `command` has exited, e.g. `["docker", "logs", "-f", "my-container"]`.
    /// Its output goes to lmdeck's log file (the TUI and web log views).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// Seconds to wait for the server to answer `/v1/models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout: Option<u64>,
}

impl EngineSpec {
    pub fn new(name: Engine) -> Self {
        EngineSpec {
            name,
            port: None,
            ctx: None,
            args: vec![],
            env: BTreeMap::new(),
            image: None,
            entrypoint: vec![],
            volumes: vec![],
            bin: None,
            dir: None,
            command: vec![],
            command_by_fabric: BTreeMap::new(),
            stop: vec![],
            detached: false,
            container: None,
            logs: vec![],
            ready_timeout: None,
        }
    }

    /// Every string that takes placeholders: args, env values, volumes,
    /// command (each fabric's too), stop, container and logs.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        let by_fabric = self.command_by_fabric.values().flatten();
        let lists = [&self.args, &self.volumes, &self.command, &self.stop, &self.logs];
        lists.into_iter().flatten().chain(by_fabric).chain(self.env.values()).chain(&self.container).map(String::as_str)
    }
}

/// What the machine needs for this recipe to make sense.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HardwareReq {
    /// Minimum memory the model needs, in GB: unified memory on GB10 / Apple
    /// Silicon, total VRAM on discrete GPUs, RAM on CPU-only boxes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_memory_gb: Option<f64>,
    /// Allowed CPU architectures (`x86_64`, `aarch64`). Empty = any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arch: Vec<String>,
    /// Allowed operating systems (`linux`, `macos`). Empty = any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// Required accelerator family: `nvidia`, `apple`, `amd`, `cpu`. Empty = any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accelerator: Vec<String>,
    /// Minimum CUDA compute capability, e.g. `"10.0"` for NVFP4 (GB10 is 12.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_compute_capability: Option<String>,
    /// How many machines the recipe spans. Any count works: `[[nodes]]` names
    /// the host that runs each one. A value above 1 with no `[[nodes]]` is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<u32>,
    /// How many Sparks a wrapped recipe's own scripts use. They reach the
    /// other machines themselves (ssh, docker), so lmdeck starts it on this
    /// one only; the count is for the fit check ("needs 4 Sparks · 1 online").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<u32>,
}

impl HardwareReq {
    pub fn is_empty(&self) -> bool {
        *self == HardwareReq::default()
    }
}

/// A complete recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    /// File format version (`schema = 1`; missing means 1, see [`crate::schema`]).
    #[serde(default = "crate::schema::current")]
    pub schema: u32,
    /// Identifier (defaults to the file stem). `[A-Za-z0-9._-]` only.
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Optional for `custom` recipes.
    #[serde(default, skip_serializing_if = "ModelSpec::is_empty")]
    pub model: ModelSpec,
    /// A git checkout to run `custom` scripts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<GitSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<Quant>,
    pub engine: EngineSpec,
    #[serde(default, skip_serializing_if = "HardwareReq::is_empty")]
    pub hardware: HardwareReq,
    /// `[harness]`: whether agent-harness sync offers it as a chat model.
    #[serde(default, skip_serializing_if = "HarnessSpec::is_default")]
    pub harness: HarnessSpec,
    /// Multi-node: how the `[[nodes]]` are brought up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<ClusterSpec>,
    /// Multi-node: one entry per machine (exactly one `role = "head"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeSpec>,
}

/// `[harness]`: how agent-harness sync (`crate::harness`) treats the recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSpec {
    /// `false`: the server isn't a chat model (a decision or classifier
    /// server), so harness sync leaves it out. Default `true`.
    #[serde(default = "HarnessSpec::chats")]
    pub chat: bool,
}

impl Default for HarnessSpec {
    /// A chat model, synced like any other.
    fn default() -> Self {
        HarnessSpec { chat: HarnessSpec::chats() }
    }
}

impl HarnessSpec {
    /// The `chat` default: recipes are chat models unless they say otherwise.
    fn chats() -> bool {
        true
    }

    /// Nothing set, so a written recipe leaves the table out.
    pub fn is_default(&self) -> bool {
        *self == HarnessSpec::default()
    }
}

mod disk;
mod nodes;
pub use nodes::{ClusterSpec, NodeRole, NodeSpec, StartOrder, AUTO};

/// Is this a recipe file name (`.toml`)?
pub fn is_recipe_file(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("toml"))
}

/// Is this a YAML file (no longer accepted as a recipe)?
pub fn is_yaml_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
}

/// The error for a `.yaml`/`.yml` recipe.
pub const YAML_UNSUPPORTED: &str =
    "YAML recipes are no longer supported: lmdeck reads TOML only (convert it to a .toml recipe; see the README's \"Recipe format\")";

/// Model families for the grouped views, in display order: the first rule
/// whose key appears (case-insensitive substring) in a recipe's display name,
/// name or model id wins; everything else is "Other". The list mirrors
/// lmswitch's `FAMILY_RULES` plus the other major labs.
pub const FAMILY_RULES: &[(&str, &[&str])] = &[
    ("Qwen", &["qwen", "qwopus"]),
    ("Gemma", &["diffusiongemma", "gemma", "gemini"]),
    ("DeepSeek", &["deepseek"]),
    ("Nemotron", &["nemotron"]),
    ("Kimi", &["kimi", "moonshot"]),
    ("GPT-OSS", &["gpt-oss", "gpt"]),
    ("GLM", &["glm"]),
    ("Nex", &["nex"]),
    ("Step", &["step"]),
    ("Ornith", &["ornith"]),
    ("MiMo", &["mimo"]),
    ("MiniMax", &["minimax"]),
    ("Llama", &["llama"]),
    ("Mistral", &["mistral", "mixtral", "ministral", "magistral", "voxtral", "pixtral", "codestral"]),
    ("Phi", &["phi"]),
    ("Command", &["command-r", "commandr"]),
    ("Yi", &["yi-"]),
    ("Falcon", &["falcon"]),
    ("SmolLM", &["smollm", "smol"]),
    ("OLMo", &["olmo"]),
    ("InternLM", &["internlm"]),
    ("Hunyuan", &["hunyuan"]),
    ("Granite", &["granite"]),
    ("Seed", &["seed-oss"]),
    ("Apertus", &["apertus"]),
    ("Ernie", &["ernie"]),
    ("Grok", &["grok"]),
    ("RWKV", &["rwkv"]),
    ("Apex", &["apex"]),
];

/// Sort rank of a family label (families keep their rule order; unknown → last).
pub fn family_rank_of(family: &str) -> usize {
    FAMILY_RULES.iter().position(|(l, _)| *l == family).unwrap_or(usize::MAX)
}

/// Returned (wrapped in anyhow) when a file parses but is not a recipe,
/// so source indexing can skip READMEs, CI configs, etc. quietly.
#[derive(Debug)]
pub struct NotARecipe;

impl fmt::Display for NotARecipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a recipe (no [engine] table)")
    }
}

impl std::error::Error for NotARecipe {}

pub fn is_not_a_recipe(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotARecipe>().is_some()
}

impl Recipe {
    /// Parses TOML recipe text. `stem` names the recipe when the text does not.
    pub fn parse(text: &str, stem: &str) -> Result<Recipe> {
        let value: toml::Value = toml::from_str(text)?;
        let table = value.as_table().ok_or(NotARecipe)?;
        if !table.get("engine").is_some_and(|v| v.is_table()) {
            return Err(NotARecipe.into());
        }
        let mut value = crate::schema::check("recipe", value)?;
        if let Some(t) = value.as_table_mut() {
            t.remove("min_lmdeck");
        }
        let mut recipe: Recipe = value.try_into()?;
        if recipe.name.trim().is_empty() {
            recipe.name = stem.to_string();
        }
        recipe.validate()?;
        Ok(recipe)
    }

    /// Reads a `.toml` recipe file. YAML files get a clear "not supported" error.
    pub fn load(path: &Path) -> Result<Recipe> {
        if is_yaml_file(path) {
            bail!("{}: {YAML_UNSUPPORTED}", path.display());
        }
        if !is_recipe_file(path) {
            bail!("{}: recipes must be .toml files", path.display());
        }
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("recipe");
        Recipe::parse(&text, stem).with_context(|| format!("loading {}", path.display()))
    }

    /// Checks invariants the rest of lmdeck relies on.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.name) {
            bail!("invalid recipe name '{}': use letters, digits, '.', '_' or '-'", self.name);
        }
        let custom = self.engine.name == Engine::Custom;
        match self.model.source {
            ModelSource::Hf if !custom && self.model.repo.as_deref().unwrap_or("").is_empty() => {
                bail!("model.source = \"hf\" needs model.repo")
            }
            ModelSource::Local if self.model.path.as_deref().unwrap_or("").is_empty() => {
                bail!("model.source = \"local\" needs model.path")
            }
            _ => {}
        }
        if let Some(repo) = &self.model.repo {
            if repo.split('/').count() != 2 || repo.split('/').any(|p| p.is_empty()) {
                bail!("model.repo '{repo}' should look like 'org/name'");
            }
        }
        if custom && self.engine.command.is_empty() && self.nodes.is_empty() {
            bail!("engine \"custom\" needs engine.command");
        }
        if !custom {
            for (field, set) in [
                ("stop", !self.engine.stop.is_empty()),
                ("command_by_fabric", !self.engine.command_by_fabric.is_empty()),
                ("detached", self.engine.detached),
                ("container", self.engine.container.is_some()),
                ("logs", !self.engine.logs.is_empty()),
                ("[source]", self.source.is_some()),
            ] {
                if set {
                    bail!("{field} is only for engine \"custom\" (wrapped scripts)");
                }
            }
        }
        for (k, argv) in &self.engine.command_by_fabric {
            if crate::fabric::ShapeKind::parse(k).is_none_or(|s| s.as_str() != k) {
                bail!(
                    "engine.command_by_fabric: unknown fabric '{k}' (expected one of: {})",
                    crate::fabric::ShapeKind::NAMES.join(", ")
                );
            }
            if argv.is_empty() {
                bail!("engine.command_by_fabric.{k} is empty");
            }
        }
        if !self.engine.logs.is_empty() && !self.engine.detached {
            bail!("engine.logs is for detached recipes; a foreground command's output is logged already");
        }
        if let Some(src) = &self.source {
            if src.repo.trim().is_empty() {
                bail!("[source] needs repo");
            }
        }
        if self.engine.name == Engine::LlamaCpp
            && self.model.path.is_none()
            && self.model.files.first().is_none_or(|f| !f.ends_with(".gguf"))
        {
            bail!("llama.cpp recipes need model.files = [\"<file>.gguf\", …] or model.path");
        }
        if self.hardware.nodes == Some(0) {
            bail!("hardware.nodes must be at least 1");
        }
        if self.hardware.hosts == Some(0) {
            bail!("hardware.hosts must be at least 1");
        }
        if !self.nodes.is_empty() {
            let heads = self.nodes.iter().filter(|n| n.role == NodeRole::Head).count();
            if heads != 1 {
                bail!("[[nodes]] needs exactly one role = \"head\" (found {heads})");
            }
            for (i, n) in self.nodes.iter().enumerate() {
                if !valid_name(&n.name) {
                    bail!("[[nodes]] #{}: invalid name '{}'", i + 1, n.name);
                }
                if self.nodes[..i].iter().any(|m| m.name == n.name) {
                    bail!("[[nodes]]: two nodes are named '{}'", n.name);
                }
                if n.host.trim().is_empty() {
                    bail!("[[nodes]] {}: needs host", n.name);
                }
                if n.host == AUTO && n.role == NodeRole::Head {
                    bail!("[[nodes]] {}: the head needs a host (\"local\" or a hosts.toml name); \"auto\" places workers around it", n.name);
                }
                if n.command.is_empty() {
                    bail!("[[nodes]] {}: needs command", n.name);
                }
                if !n.logs.is_empty() && !n.detached {
                    bail!("[[nodes]] {}: logs is for detached nodes", n.name);
                }
            }
            if let Some(want) = self.hardware.nodes {
                if want as usize != self.nodes.len() {
                    bail!("hardware.nodes = {want} but there are {} [[nodes]]", self.nodes.len());
                }
            }
        } else if self.cluster.is_some() {
            bail!("[cluster] needs [[nodes]]");
        }
        Ok(())
    }

    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The model family this recipe belongs to (for the grouped views): the
    /// first [`FAMILY_RULES`] entry whose key appears in the display name,
    /// recipe name or model id.
    pub fn family(&self) -> &'static str {
        let hay = format!(
            "{} {} {}",
            self.display(),
            self.name,
            [self.model.repo.as_deref(), self.model.path.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ")
        )
        .to_lowercase();
        for (label, keys) in FAMILY_RULES {
            if keys.iter().any(|k| hay.contains(k)) {
                return label;
            }
        }
        "Other"
    }

    /// Sort rank of [`Recipe::family`] (families keep their rule order).
    pub fn family_rank(&self) -> usize {
        family_rank_of(self.family())
    }

    pub fn port(&self) -> u16 {
        self.engine.port.unwrap_or_else(|| self.engine.name.default_port())
    }

    /// How many machines it needs: its `[[nodes]]`, `hardware.nodes`, or the
    /// Sparks a wrapped recipe's scripts use (`hardware.hosts`).
    pub fn machines(&self) -> u32 {
        self.nodes().max(self.hardware.hosts.unwrap_or(1))
    }

    pub fn nodes(&self) -> u32 {
        if self.nodes.is_empty() {
            self.hardware.nodes.unwrap_or(1)
        } else {
            self.nodes.len() as u32
        }
    }

    pub fn quant_short(&self) -> String {
        self.quant.as_ref().map(|q| q.short()).unwrap_or_else(|| "-".into())
    }

    /// Where the weights live (or will live) outside the Hugging Face cache:
    /// `[model] path`, or llama.cpp's and TabbyAPI's place under the models dir.
    /// `None`: the engine is handed the repo id (vLLM, SGLang, MLX).
    pub fn local_path(&self, models_dir: &Path) -> Option<PathBuf> {
        if let Some(p) = &self.model.path {
            let expanded = expand(p);
            return Some(if expanded.is_absolute() { expanded } else { models_dir.join(p) });
        }
        let repo = self.model.repo.as_deref()?;
        match self.engine.name {
            Engine::LlamaCpp => Some(models_dir.join(repo).join(self.model.files.first()?)),
            Engine::TabbyApi => Some(models_dir.join(self.default_dir_name()?)),
            _ => None,
        }
    }

    /// `org/name` or `org/name-<revision>` for non-default revisions.
    fn default_dir_name(&self) -> Option<String> {
        let repo = self.model.repo.as_deref()?;
        Some(match self.model.revision.as_deref() {
            None | Some("main") | Some("") => repo.to_string(),
            Some(rev) => format!("{repo}-{rev}"),
        })
    }

    /// Serialises to the native TOML format.
    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Lower-cased haystack used by `recipe search`.
    pub fn search_text(&self) -> String {
        let mut s = vec![
            self.name.clone(),
            self.display().to_string(),
            self.description.clone().unwrap_or_default(),
            self.engine.name.to_string(),
            self.model.repo.clone().unwrap_or_default(),
            self.source.as_ref().map(|s| s.repo.clone()).unwrap_or_default(),
            self.quant_short(),
        ];
        s.extend(self.tags.iter().cloned());
        s.join(" ").to_lowercase()
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NATIVE: &str = r#"
name = "qwen3-4b-q4km"
display_name = "Qwen3-4B Q4_K_M"
tags = ["qwen"]

[model]
source = "hf"
repo = "unsloth/Qwen3-4B-GGUF"
files = ["Qwen3-4B-Q4_K_M.gguf"]
size_gb = 2.5

[quant]
format = "gguf"
label = "Q4_K_M"

[engine]
name = "llama.cpp"
port = 8085
ctx = 32768
args = ["-fa", "on"]

[hardware]
min_memory_gb = 4
"#;
    #[test]
    fn families_group_the_major_labs() {
        let fam = |toml: &str| Recipe::parse(toml, "t").unwrap().family();
        let with_name = |name: &str| {
            fam(&format!("[model]\nrepo = \"org/{name}\"\nfiles = [\"a.gguf\"]\n[engine]\nname = \"llama.cpp\"\n"))
        };
        assert_eq!(with_name("Qwen3-4B-GGUF"), "Qwen");
        assert_eq!(with_name("GLM-5.3"), "GLM");
        assert_eq!(with_name("DeepSeek-V3"), "DeepSeek");
        assert_eq!(with_name("gpt-oss-20b"), "GPT-OSS");
        assert_eq!(with_name("Llama-3.1-8B"), "Llama");
        assert_eq!(with_name("Mixtral-8x7B"), "Mistral");
        assert_eq!(with_name("Kimi-K2"), "Kimi");
        assert_eq!(with_name("ornith-1.5-122b"), "Ornith");
        assert_eq!(with_name("mystery-model"), "Other");
        // the display name counts when the id carries nothing
        let mut r =
            Recipe::parse("[model]\nrepo = \"org/zzz\"\nfiles = [\"a.gguf\"]\n[engine]\nname = \"llama.cpp\"\n", "t")
                .unwrap();
        r.display_name = Some("GLM-5.3-Flash".into());
        assert_eq!(r.family(), "GLM");
        assert!(r.family_rank() < family_rank_of("Other"));
    }

    #[test]
    fn parses_native_toml() {
        let r = Recipe::parse(NATIVE, "ignored").unwrap();
        assert_eq!(r.name, "qwen3-4b-q4km");
        assert_eq!(r.engine.name, Engine::LlamaCpp);
        assert_eq!(r.port(), 8085);
        assert_eq!(r.quant_short(), "gguf Q4_K_M");
        assert_eq!(r.hardware.min_memory_gb, Some(4.0));
        assert_eq!(
            r.local_path(Path::new("/m")).unwrap(),
            PathBuf::from("/m/unsloth/Qwen3-4B-GGUF/Qwen3-4B-Q4_K_M.gguf")
        );
    }

    #[test]
    fn toml_roundtrip_is_lossless() {
        let r = Recipe::parse(NATIVE, "x").unwrap();
        let again = Recipe::parse(&r.to_toml().unwrap(), "x").unwrap();
        assert_eq!(r, again);
    }

    #[test]
    fn stem_names_the_recipe() {
        let t = "[model]\nrepo = \"mlx-community/Qwen3-4B-4bit\"\n[engine]\nname = \"mlx\"\n";
        let r = Recipe::parse(t, "qwen-mlx").unwrap();
        assert_eq!(r.name, "qwen-mlx");
        assert_eq!(r.engine.name, Engine::Mlx);
        assert_eq!(r.port(), 8080);
        assert!(r.local_path(Path::new("/m")).is_none());
    }

    #[test]
    fn validation_errors() {
        let bad_repo = "[model]\nrepo = \"noslash\"\n[engine]\nname = \"vllm\"\n";
        assert!(Recipe::parse(bad_repo, "a").is_err());
        let no_gguf = "[model]\nrepo = \"a/b\"\n[engine]\nname = \"llama.cpp\"\n";
        assert!(Recipe::parse(no_gguf, "a").is_err());
        let custom = "[model]\nsource = \"local\"\npath = \"x\"\n[engine]\nname = \"custom\"\n";
        assert!(Recipe::parse(custom, "a").is_err());
        let bad_engine = "[model]\nrepo = \"a/b\"\n[engine]\nname = \"ollama2\"\n";
        assert!(Recipe::parse(bad_engine, "a").is_err());
        assert!(Recipe::parse("[model]\nrepo=\"a/b\"\n[engine]\nname=\"vllm\"\n", "bad name").is_err());
    }

    #[test]
    fn non_recipes_are_flagged() {
        let e = Recipe::parse("title = \"x\"", "a").unwrap_err();
        assert!(is_not_a_recipe(&e));
    }

    #[test]
    fn engine_aliases() {
        assert_eq!("llama".parse::<Engine>().unwrap(), Engine::LlamaCpp);
        assert_eq!("exllamav3".parse::<Engine>().unwrap(), Engine::TabbyApi);
        assert_eq!("LMS".parse::<Engine>().unwrap(), Engine::LmStudio);
        assert!("nope".parse::<Engine>().is_err());
    }

    const WRAPPER: &str = r#"
[source]
repo = "https://github.com/example/server"
ref = "main"

[engine]
name = "custom"
command = ["./start.sh"]
stop = ["./stop.sh"]
detached = true
container = "my-server"
logs = ["docker", "logs", "-f", "my-server"]
port = 8888
env = { PORT = "{port}" }
"#;

    #[test]
    fn wrapper_recipe_needs_no_model() {
        let r = Recipe::parse(WRAPPER, "wrapped").unwrap();
        assert_eq!(r.name, "wrapped");
        assert_eq!(r.engine.name, Engine::Custom);
        assert!(r.model.is_empty());
        let src = r.source.as_ref().unwrap();
        assert_eq!((src.repo.as_str(), src.git_ref.as_deref()), ("https://github.com/example/server", Some("main")));
        assert_eq!(r.engine.stop, ["./stop.sh"]);
        assert!(r.engine.detached);
        assert_eq!(r.engine.container.as_deref(), Some("my-server"));
        assert_eq!(r.engine.logs, ["docker", "logs", "-f", "my-server"]);
        assert_eq!(r.engine.env["PORT"], "{port}");
        assert_eq!(r.port(), 8888);
        // Round-trips without growing an empty [model] table.
        let toml = r.to_toml().unwrap();
        assert!(!toml.contains("[model]") && toml.contains("ref = \"main\""), "{toml}");
        assert_eq!(Recipe::parse(&toml, "wrapped").unwrap(), r);
        // The smallest wrapper.
        let tiny = "[engine]\nname = \"custom\"\ncommand = [\"./start.sh\"]\n";
        assert!(Recipe::parse(tiny, "tiny").is_ok());
    }

    #[test]
    fn wrapper_fields_are_custom_only() {
        let t = "[model]\nrepo = \"a/b\"\n[engine]\nname = \"vllm\"\ndetached = true\n";
        assert!(Recipe::parse(t, "a").unwrap_err().to_string().contains("only for engine"));
        let t = "[model]\nrepo = \"a/b\"\n[source]\nrepo = \"x\"\n[engine]\nname = \"vllm\"\n";
        assert!(Recipe::parse(t, "a").is_err());
        let t = "[engine]\nname = \"custom\"\ncommand = [\"x\"]\nlogs = [\"tail\"]\n";
        assert!(Recipe::parse(t, "a").unwrap_err().to_string().contains("detached"));
        // Non-custom engines still need a model.
        assert!(Recipe::parse("[engine]\nname = \"vllm\"\n", "a").is_err());
    }

    #[test]
    fn yaml_recipes_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["old.yaml", "old.yml"] {
            let p = dir.path().join(f);
            fs::write(&p, "runtime: llama\nmodel: a/b/c.gguf\n").unwrap();
            let e = format!("{:#}", Recipe::load(&p).unwrap_err());
            assert!(e.contains("YAML recipes are no longer supported") && e.contains("TOML"), "{e}");
        }
        assert!(is_recipe_file(Path::new("x.TOML")) && !is_recipe_file(Path::new("x.yaml")));
    }

    #[test]
    fn lmdeploy_engine() {
        assert_eq!("lmdeploy".parse::<Engine>().unwrap(), Engine::LmDeploy);
        assert_eq!("TurboMind".parse::<Engine>().unwrap(), Engine::LmDeploy);
        assert_eq!(Engine::LmDeploy.default_port(), 23333);
        assert!(Engine::ALL.contains(&Engine::LmDeploy));
    }

    #[test]
    fn tabby_default_dir_includes_revision() {
        let t =
            "[model]\nrepo = \"turboderp/Qwen3-30B-A3B-exl3\"\nrevision = \"4.0bpw\"\n[engine]\nname = \"tabbyapi\"\n";
        let r = Recipe::parse(t, "q").unwrap();
        assert_eq!(r.local_path(Path::new("/m")).unwrap(), PathBuf::from("/m/turboderp/Qwen3-30B-A3B-exl3-4.0bpw"));
    }
}
