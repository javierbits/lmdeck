//! Recipes: a model source + quant + engine + hardware requirements.
//!
//! Native recipes are TOML (YAML is accepted too, same schema). lmswitch's
//! per-model YAML (`runtime: llama|vllm|...`, as published in
//! `jvr0x/ai-models`) is detected and converted on load, so every existing
//! lmswitch recipe works as an lmdeck recipe.

use anyhow::{anyhow, bail, Context, Result};
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
    /// Anything else: `engine.command` is run verbatim.
    Custom,
}

impl Engine {
    pub const ALL: [Engine; 8] = [
        Engine::LlamaCpp,
        Engine::Vllm,
        Engine::Sglang,
        Engine::TabbyApi,
        Engine::LmStudio,
        Engine::Mlx,
        Engine::TensorFold,
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
    /// `{ctx}`, `{model}`, `{models_dir}`, `{host}`, `{repo}`, `{revision}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment for the server process (or container).
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
    /// Working directory (TabbyAPI checkout for `tabbyapi`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Full argv for `custom` engines (placeholders as in `args`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
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
            ready_timeout: None,
        }
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
    /// Number of machines the recipe spans (lmdeck v0.1 runs single-node only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<u32>,
}

impl HardwareReq {
    pub fn is_empty(&self) -> bool {
        *self == HardwareReq::default()
    }
}

/// A complete recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recipe {
    /// Identifier (defaults to the file stem). `[A-Za-z0-9._-]` only.
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub model: ModelSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<Quant>,
    pub engine: EngineSpec,
    #[serde(default, skip_serializing_if = "HardwareReq::is_empty")]
    pub hardware: HardwareReq,
}

/// File formats a recipe can be read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Yaml,
}

impl Format {
    pub fn from_path(p: &Path) -> Option<Format> {
        match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "toml" => Some(Format::Toml),
            "yaml" | "yml" => Some(Format::Yaml),
            _ => None,
        }
    }
}

/// Returned (wrapped in anyhow) when a file parses but is not a recipe,
/// so source indexing can skip READMEs, CI configs, etc. quietly.
#[derive(Debug)]
pub struct NotARecipe;

impl fmt::Display for NotARecipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a recipe (no [engine]/[model] tables and no lmswitch `runtime`/`model` keys)")
    }
}

impl std::error::Error for NotARecipe {}

pub fn is_not_a_recipe(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotARecipe>().is_some()
}

impl Recipe {
    /// Parses recipe text. `stem` names the recipe when the text does not.
    pub fn parse(text: &str, format: Format, stem: &str) -> Result<Recipe> {
        let mut recipe = match format {
            Format::Toml => {
                let value: toml::Value = toml::from_str(text)?;
                let table = value.as_table().ok_or(NotARecipe)?;
                if !(table.get("engine").is_some_and(|v| v.is_table())
                    && table.get("model").is_some_and(|v| v.is_table()))
                {
                    return Err(NotARecipe.into());
                }
                value.try_into::<Recipe>()?
            }
            Format::Yaml => {
                let value: serde_yaml::Value = match serde_yaml::from_str(text) {
                    // PyYAML (and so lmswitch) lets a later duplicate key win.
                    Err(e) if e.to_string().contains("duplicate entry") => {
                        serde_yaml::from_str(&dedupe_top_level_keys(text))?
                    }
                    other => other?,
                };
                let map = value.as_mapping().ok_or(NotARecipe)?;
                let has_table = |k: &str| map.get(k).is_some_and(|v| v.is_mapping());
                if has_table("engine") && has_table("model") {
                    serde_yaml::from_value::<Recipe>(value)?
                } else if map.get("runtime").is_some_and(|v| v.is_string())
                    || map.get("model").is_some_and(|v| v.is_string())
                {
                    from_lmswitch(map, stem, text)?
                } else {
                    return Err(NotARecipe.into());
                }
            }
        };
        if recipe.name.trim().is_empty() {
            recipe.name = stem.to_string();
        }
        recipe.validate()?;
        Ok(recipe)
    }

    /// Reads a recipe file (`.toml`, `.yaml`, `.yml`).
    pub fn load(path: &Path) -> Result<Recipe> {
        let format = Format::from_path(path)
            .ok_or_else(|| anyhow!("{}: recipes must be .toml, .yaml or .yml", path.display()))?;
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("recipe");
        Recipe::parse(&text, format, stem).with_context(|| format!("loading {}", path.display()))
    }

    /// Checks invariants the rest of lmdeck relies on.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.name) {
            bail!("invalid recipe name '{}': use letters, digits, '.', '_' or '-'", self.name);
        }
        match self.model.source {
            ModelSource::Hf if self.model.repo.as_deref().unwrap_or("").is_empty() => {
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
        if self.engine.name == Engine::Custom && self.engine.command.is_empty() {
            bail!("engine \"custom\" needs engine.command");
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
        Ok(())
    }

    pub fn display(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    pub fn port(&self) -> u16 {
        self.engine.port.unwrap_or_else(|| self.engine.name.default_port())
    }

    pub fn nodes(&self) -> u32 {
        self.hardware.nodes.unwrap_or(1)
    }

    pub fn quant_short(&self) -> String {
        self.quant.as_ref().map(|q| q.short()).unwrap_or_else(|| "-".into())
    }

    /// Where the weights live (or will live) on disk, if the engine loads a
    /// local path. `None` means the engine is handed the repo id and uses the
    /// Hugging Face cache itself (vLLM, SGLang, MLX).
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
            self.quant_short(),
        ];
        s.extend(self.tags.iter().cloned());
        s.join(" ").to_lowercase()
    }
}

/// Drops all but the last occurrence of each top-level YAML key (PyYAML
/// semantics for duplicate keys). Only keys at column 0 are considered.
pub fn dedupe_top_level_keys(text: &str) -> String {
    let key_of = |line: &str| -> Option<String> {
        let first = line.chars().next()?;
        if !(first.is_ascii_alphanumeric() || first == '_' || first == '"' || first == '\'') {
            return None;
        }
        let (k, _) = line.split_once(':')?;
        Some(k.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
    };
    let mut blocks: Vec<(Option<String>, Vec<&str>)> = vec![(None, vec![])];
    for line in text.lines() {
        match key_of(line) {
            Some(k) => blocks.push((Some(k), vec![line])),
            None => blocks.last_mut().expect("non-empty").1.push(line),
        }
    }
    let mut out = String::new();
    for (i, (key, lines)) in blocks.iter().enumerate() {
        let superseded = key.as_ref().is_some_and(|k| blocks[i + 1..].iter().any(|(k2, _)| k2.as_ref() == Some(k)));
        if !superseded {
            for l in lines {
                out.push_str(l);
                out.push('\n');
            }
        }
    }
    out
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Guesses the quant format/label from a repo id, path or file name.
pub fn infer_quant(text: &str) -> Option<Quant> {
    let low = text.to_lowercase();
    let bits = bpw(&low);
    let gguf_label = gguf_label(text);
    let format = if low.ends_with(".gguf") || low.contains("gguf") {
        "gguf"
    } else if low.contains("exl3") {
        "exl3"
    } else if low.contains("exl2") {
        "exl2"
    } else if low.contains("nvfp4") {
        "nvfp4"
    } else if low.contains("mxfp4") {
        "mxfp4"
    } else if low.contains("fp8") {
        "fp8"
    } else if low.contains("awq") {
        "awq"
    } else if low.contains("gptq") {
        "gptq"
    } else if low.contains("mlx") {
        "mlx"
    } else if low.contains("bf16") {
        "bf16"
    } else {
        return None;
    };
    let label = if format == "gguf" { gguf_label } else { bits.map(|b| format!("{b}bpw")) };
    Some(Quant { format: format.into(), bits, label })
}

fn bpw(low: &str) -> Option<f64> {
    let idx = low.find("bpw")?;
    let head = &low[..idx];
    let start =
        head.char_indices().rev().take_while(|(_, c)| c.is_ascii_digit() || *c == '.').last().map(|(i, _)| i)?;
    head[start..].parse().ok()
}

fn gguf_label(text: &str) -> Option<String> {
    let file = text.rsplit('/').next().unwrap_or(text);
    let stem = file.strip_suffix(".gguf").unwrap_or(file);
    stem.split(['-', '.'])
        .rev()
        .find(|t| {
            let u = t.to_ascii_uppercase();
            let quanty = (u.starts_with('Q') || u.starts_with("IQ"))
                && u.trim_start_matches('I').chars().nth(1).is_some_and(|c| c.is_ascii_digit());
            quanty || matches!(u.as_str(), "BF16" | "F16" | "F32" | "MXFP4")
        })
        .map(|t| t.to_ascii_uppercase())
}

// ---------------------------------------------------------------------------
// lmswitch YAML compatibility
// ---------------------------------------------------------------------------

type YMap = serde_yaml::Mapping;

fn y_str(map: &YMap, key: &str) -> Option<String> {
    match map.get(key)? {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn y_u64(map: &YMap, key: &str) -> Option<u64> {
    match map.get(key)? {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn y_bool(map: &YMap, key: &str) -> Option<bool> {
    match map.get(key)? {
        serde_yaml::Value::Bool(b) => Some(*b),
        serde_yaml::Value::String(s) => match s.to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `extra_args`-style value: a list (one token each) or a shell-split string.
fn y_args(map: &YMap, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| match v {
                serde_yaml::Value::String(s) => Some(s.clone()),
                serde_yaml::Value::Number(n) => Some(n.to_string()),
                serde_yaml::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .collect(),
        Some(serde_yaml::Value::String(s)) => shlex::split(s).unwrap_or_default(),
        _ => vec![],
    }
}

fn y_env(map: &YMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    match map.get("env") {
        Some(serde_yaml::Value::Mapping(m)) => {
            for (k, v) in m {
                if let (Some(k), Some(v)) = (k.as_str(), yaml_scalar(v)) {
                    out.insert(k.to_string(), v);
                }
            }
        }
        Some(serde_yaml::Value::Sequence(seq)) => {
            for item in seq {
                if let Some((k, v)) = item.as_str().and_then(|s| s.split_once('=')) {
                    out.insert(k.to_string(), v.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

fn yaml_scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// First comment line of the file, as a description.
fn leading_comment(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .take_while(|l| l.is_empty() || l.starts_with('#'))
        .find_map(|l| l.strip_prefix('#').map(str::trim).filter(|l| !l.is_empty()))?;
    let mut s: String = line.chars().take(140).collect();
    if line.chars().count() > 140 {
        s.push('…');
    }
    Some(s)
}

/// Converts an lmswitch model YAML into a [`Recipe`].
pub fn from_lmswitch(map: &YMap, stem: &str, text: &str) -> Result<Recipe> {
    let runtime = y_str(map, "runtime").unwrap_or_else(|| "llama".into());
    let (engine, nodes) = match runtime.as_str() {
        "llama" => (Engine::LlamaCpp, 1),
        "llama-dual" => (Engine::LlamaCpp, 2),
        "vllm" => (Engine::Vllm, 1),
        "vllm-dual" | "vllm-dual-ray" => (Engine::Vllm, 2),
        "sglang" => (Engine::Sglang, 1),
        "sglang-dual" => (Engine::Sglang, 2),
        "exllamav3" | "tabbyapi" => (Engine::TabbyApi, 1),
        "exllamav3-dual" => (Engine::TabbyApi, 2),
        other => bail!("unsupported lmswitch runtime '{other}'"),
    };
    let rel = y_str(map, "model").unwrap_or_default();
    if rel.is_empty() {
        bail!("lmswitch recipe has no `model:` path");
    }
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();

    let mut model = ModelSpec { path: Some(rel.clone()), ..Default::default() };
    // lmswitch's convention mirrors Hugging Face: <org>/<repo>[/<file>].
    if parts.len() >= 2 && !rel.starts_with('/') && !rel.starts_with('~') {
        model.source = ModelSource::Hf;
        model.repo = Some(format!("{}/{}", parts[0], parts[1]));
        if engine == Engine::LlamaCpp && parts.len() > 2 {
            model.files.push(parts[2..].join("/"));
            if let Some(mm) = y_str(map, "mmproj") {
                if let Some(rest) = mm.strip_prefix(&format!("{}/{}/", parts[0], parts[1])) {
                    model.files.push(rest.to_string());
                }
            }
        }
    } else {
        model.source = ModelSource::Local;
    }

    let mut spec = EngineSpec::new(engine);
    spec.port = y_u64(map, "port").and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0);
    spec.ctx = y_u64(map, "ctx");
    spec.ready_timeout = y_u64(map, "ready_timeout");
    spec.env = y_env(map);
    spec.volumes = y_args(map, "extra_mounts");
    let mut args: Vec<String> = vec![];
    let flag = |args: &mut Vec<String>, k: &str, f: &str| {
        if let Some(v) = y_str(map, k) {
            args.push(f.to_string());
            args.push(v);
        }
    };
    match engine {
        Engine::LlamaCpp => {
            spec.bin = y_str(map, "llama_bin");
            flag(&mut args, "gpu_layers", "--n-gpu-layers");
            flag(&mut args, "threads", "--threads");
            flag(&mut args, "batch", "--batch-size");
            flag(&mut args, "ubatch", "--ubatch-size");
            flag(&mut args, "alias", "--alias");
            if let Some(mm) = y_str(map, "mmproj") {
                args.push("--mmproj".into());
                args.push(format!("{{models_dir}}/{mm}"));
            }
            // lmswitch passes `-fit off` unless told otherwise (GB10 cudaMemGetInfo abort).
            let fit = match map.get("fit") {
                Some(serde_yaml::Value::Bool(false)) => Some("off".to_string()),
                Some(_) => y_str(map, "fit"),
                None => Some("off".to_string()),
            };
            if let Some(fit) = fit.filter(|f| !matches!(f.as_str(), "" | "none" | "skip")) {
                args.push("-fit".into());
                args.push(fit);
            }
        }
        Engine::Vllm => {
            spec.image = Some(y_str(map, "image").unwrap_or_else(|| "vllm/vllm-openai:cu130-nightly".into()));
            if let Some(ep) = map.get("entrypoint") {
                spec.entrypoint = match ep {
                    serde_yaml::Value::String(s) => shlex::split(s).unwrap_or_default(),
                    _ => y_args(map, "entrypoint"),
                };
            }
            let default_util = if nodes > 1 { "0.80" } else { "0.15" };
            args.push(format!(
                "--gpu-memory-utilization={}",
                y_str(map, "gpu_memory_utilization").unwrap_or_else(|| default_util.into())
            ));
            args.push(format!("--max-num-seqs={}", y_str(map, "max_num_seqs").unwrap_or_else(|| "32".into())));
            if y_bool(map, "enforce_eager").unwrap_or(true) {
                args.push("--enforce-eager".into());
            }
            vllm_like_flags(map, &mut args);
            for (k, f) in [
                ("limit_mm_per_prompt", "--limit-mm-per-prompt"),
                ("attention_backend", "--attention-backend"),
                ("load_format", "--load-format"),
            ] {
                if let Some(v) = y_str(map, k) {
                    args.push(format!("{f}={v}"));
                }
            }
            if !y_bool(map, "no_chat_template").unwrap_or(false) {
                if let Some(v) = y_str(map, "chat_template") {
                    args.push(format!("--chat-template={v}"));
                }
            }
        }
        Engine::Sglang => {
            spec.image = Some(y_str(map, "image").unwrap_or_else(|| "lmsysorg/sglang:latest".into()));
            flag(&mut args, "mem_fraction_static", "--mem-fraction-static");
            flag(&mut args, "attention_backend", "--attention-backend");
            flag(&mut args, "kv_cache_dtype", "--kv-cache-dtype");
            flag(&mut args, "chunked_prefill_size", "--chunked-prefill-size");
            flag(&mut args, "max_running_requests", "--max-running-requests");
            vllm_like_flags(map, &mut args);
        }
        Engine::TabbyApi => {
            spec.dir = y_str(map, "exllamav3_repo");
            if let Some(q) = y_str(map, "cache_quant") {
                args.push("--cache-mode".into());
                args.push(if q.chars().all(|c| c.is_ascii_digit()) { format!("Q{q}") } else { q });
            }
        }
        _ => {}
    }
    args.extend(y_args(map, "extra_args"));
    spec.args = args;

    let mut tags = vec!["lmswitch".to_string(), runtime.clone()];
    if nodes > 1 {
        tags.push("multi-node".into());
    }
    let quant = infer_quant(&rel).or_else(|| infer_quant(stem));
    if let Some(q) = &quant {
        tags.push(q.format.clone());
    }
    Ok(Recipe {
        name: stem.to_string(),
        display_name: y_str(map, "display_name"),
        description: leading_comment(text),
        tags,
        model,
        quant,
        engine: spec,
        hardware: HardwareReq { nodes: (nodes > 1).then_some(nodes), ..Default::default() },
    })
}

fn vllm_like_flags(map: &YMap, args: &mut Vec<String>) {
    if let Some(p) = y_str(map, "tool_call_parser") {
        args.push("--enable-auto-tool-choice".into());
        args.push("--tool-call-parser".into());
        args.push(p);
    }
    if let Some(p) = y_str(map, "reasoning_parser") {
        args.push(format!("--reasoning-parser={p}"));
    }
    if y_bool(map, "trust_remote_code").unwrap_or(false) {
        args.push("--trust-remote-code".into());
    }
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
    fn duplicate_yaml_keys_last_wins() {
        let text = "runtime: llama\nmodel: \"a/b/c.gguf\"\nruntime: \"vllm\"\nextra_args:\n  - --x\n";
        let r = Recipe::parse(text, Format::Yaml, "dup").unwrap();
        assert_eq!(r.engine.name, Engine::Vllm);
        assert!(r.engine.args.contains(&"--x".to_string()));
    }

    #[test]
    fn parses_native_toml() {
        let r = Recipe::parse(NATIVE, Format::Toml, "ignored").unwrap();
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
        let r = Recipe::parse(NATIVE, Format::Toml, "x").unwrap();
        let again = Recipe::parse(&r.to_toml().unwrap(), Format::Toml, "x").unwrap();
        assert_eq!(r, again);
    }

    #[test]
    fn native_yaml_and_stem_name() {
        let y = "model:\n  repo: mlx-community/Qwen3-4B-4bit\nengine:\n  name: mlx\n";
        let r = Recipe::parse(y, Format::Yaml, "qwen-mlx").unwrap();
        assert_eq!(r.name, "qwen-mlx");
        assert_eq!(r.engine.name, Engine::Mlx);
        assert_eq!(r.port(), 8080);
        assert!(r.local_path(Path::new("/m")).is_none());
    }

    #[test]
    fn validation_errors() {
        let bad_repo = "[model]\nrepo = \"noslash\"\n[engine]\nname = \"vllm\"\n";
        assert!(Recipe::parse(bad_repo, Format::Toml, "a").is_err());
        let no_gguf = "[model]\nrepo = \"a/b\"\n[engine]\nname = \"llama.cpp\"\n";
        assert!(Recipe::parse(no_gguf, Format::Toml, "a").is_err());
        let custom = "[model]\nsource = \"local\"\npath = \"x\"\n[engine]\nname = \"custom\"\n";
        assert!(Recipe::parse(custom, Format::Toml, "a").is_err());
        let bad_engine = "[model]\nrepo = \"a/b\"\n[engine]\nname = \"ollama2\"\n";
        assert!(Recipe::parse(bad_engine, Format::Toml, "a").is_err());
        assert!(Recipe::parse("[model]\nrepo=\"a/b\"\n[engine]\nname=\"vllm\"\n", Format::Toml, "bad name").is_err());
    }

    #[test]
    fn non_recipes_are_flagged() {
        let e = Recipe::parse("title = \"x\"", Format::Toml, "a").unwrap_err();
        assert!(is_not_a_recipe(&e));
        let e = Recipe::parse("name: ci\non: [push]\n", Format::Yaml, "a").unwrap_err();
        assert!(is_not_a_recipe(&e));
    }

    #[test]
    fn engine_aliases() {
        assert_eq!("llama".parse::<Engine>().unwrap(), Engine::LlamaCpp);
        assert_eq!("exllamav3".parse::<Engine>().unwrap(), Engine::TabbyApi);
        assert_eq!("LMS".parse::<Engine>().unwrap(), Engine::LmStudio);
        assert!("nope".parse::<Engine>().is_err());
    }

    #[test]
    fn converts_lmswitch_llama_yaml() {
        let y = r#"# Qwen3.6 35B MTP on the Spark
runtime: llama
model: "unsloth/Qwen3.6-35B-A3B-MTP-GGUF/Qwen3.6-35B-A3B-UD-Q8_K_XL.gguf"
port: 8089
ctx: 262144
display_name: "Qwen3.6-35B-A3B Q8 MTP"
mmproj: "unsloth/Qwen3.6-35B-A3B-MTP-GGUF/mmproj-BF16.gguf"
batch: 2048
extra_args: ["-fa", "on", "--jinja"]
"#;
        let r = Recipe::parse(y, Format::Yaml, "qwen3.6-35b-q8-mtp").unwrap();
        assert_eq!(r.name, "qwen3.6-35b-q8-mtp");
        assert_eq!(r.engine.name, Engine::LlamaCpp);
        assert_eq!(r.model.repo.as_deref(), Some("unsloth/Qwen3.6-35B-A3B-MTP-GGUF"));
        assert_eq!(r.model.files, vec!["Qwen3.6-35B-A3B-UD-Q8_K_XL.gguf", "mmproj-BF16.gguf"]);
        assert_eq!(r.port(), 8089);
        assert_eq!(r.engine.ctx, Some(262144));
        assert_eq!(r.quant_short(), "gguf Q8_K_XL");
        assert_eq!(r.description.as_deref(), Some("Qwen3.6 35B MTP on the Spark"));
        let a = &r.engine.args;
        assert!(a.windows(2).any(|w| w == ["--batch-size", "2048"]));
        assert!(a.windows(2).any(|w| w == ["-fit", "off"]));
        assert!(a.contains(&"{models_dir}/unsloth/Qwen3.6-35B-A3B-MTP-GGUF/mmproj-BF16.gguf".to_string()));
        assert_eq!(a[a.len() - 3..], ["-fa", "on", "--jinja"]);
    }

    #[test]
    fn converts_lmswitch_vllm_and_dual() {
        let y = "runtime: \"vllm\"\nmodel: nvidia/qwen3.6-35b-a3b-nvfp4\nport: 8114\ngpu_memory_utilization: 0.55\ntool_call_parser: qwen3_coder\nenforce_eager: false\nenv:\n  A: 1\n";
        let r = Recipe::parse(y, Format::Yaml, "q").unwrap();
        assert_eq!(r.engine.name, Engine::Vllm);
        assert_eq!(r.engine.image.as_deref(), Some("vllm/vllm-openai:cu130-nightly"));
        assert!(r.engine.args.contains(&"--gpu-memory-utilization=0.55".to_string()));
        assert!(!r.engine.args.contains(&"--enforce-eager".to_string()));
        assert_eq!(r.engine.env.get("A").map(String::as_str), Some("1"));
        assert_eq!(r.quant.as_ref().unwrap().format, "nvfp4");
        assert_eq!(r.nodes(), 1);

        let d = "runtime: vllm-dual\nmodel: org/big\n";
        let r = Recipe::parse(d, Format::Yaml, "big").unwrap();
        assert_eq!(r.nodes(), 2);
        assert!(r.tags.contains(&"multi-node".to_string()));

        assert!(Recipe::parse("runtime: banana\nmodel: a/b\n", Format::Yaml, "x").is_err());
    }

    #[test]
    fn infers_quants() {
        assert_eq!(infer_quant("Qwen3-4B-Q4_K_M.gguf").unwrap().short(), "gguf Q4_K_M");
        assert_eq!(infer_quant("x/IQ1_M.gguf").unwrap().label.as_deref(), Some("IQ1_M"));
        let q = infer_quant("turboderp/Qwen3-30B-A3B-exl3-4.0bpw").unwrap();
        assert_eq!((q.format.as_str(), q.bits), ("exl3", Some(4.0)));
        assert_eq!(infer_quant("nvidia/Foo-NVFP4").unwrap().format, "nvfp4");
        assert!(infer_quant("Qwen/Qwen3-4B").is_none());
    }

    #[test]
    fn tabby_default_dir_includes_revision() {
        let t =
            "[model]\nrepo = \"turboderp/Qwen3-30B-A3B-exl3\"\nrevision = \"4.0bpw\"\n[engine]\nname = \"tabbyapi\"\n";
        let r = Recipe::parse(t, Format::Toml, "q").unwrap();
        assert_eq!(r.local_path(Path::new("/m")).unwrap(), PathBuf::from("/m/turboderp/Qwen3-30B-A3B-exl3-4.0bpw"));
    }
}
