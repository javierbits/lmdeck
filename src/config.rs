//! `~/.config/lmdeck/config.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::paths::{expand, Paths};

/// User configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// File format version (`schema = 1`; see [`crate::schema`]).
    #[serde(default = "crate::schema::current")]
    pub schema: u32,
    /// Set when config.toml was skipped (a newer schema): defaults are in use.
    #[serde(skip)]
    pub skipped: Option<String>,
    /// Where model weights live. Recipe `model.path` values are relative to it.
    #[serde(default = "default_models_dir")]
    pub models_dir: String,
    /// Interface servers bind to.
    #[serde(default = "default_host")]
    pub host: String,
    /// Colour theme for the TUI and the web dashboard (`lmdeck theme list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Optional per-engine binary / directory overrides.
    #[serde(default)]
    pub engines: EngineOverrides,
    /// Recipe repositories.
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
    /// Checking wrapped recipes' `[source]` repos for new commits.
    #[serde(default, skip_serializing_if = "UpdateChecks::is_default")]
    pub updates: UpdateChecks,
    /// `[node]`: this machine's name and fast-link address as peers see them.
    #[serde(default, skip_serializing_if = "NodeConfig::is_default")]
    pub node: NodeConfig,
    /// `[serve]`: what other machines may ask this one's `lmdeck serve` to do.
    #[serde(default, skip_serializing_if = "ServeConfig::is_default")]
    pub serve: ServeConfig,
    /// `[weights]`: sharing this machine's weights with peers over NFS.
    #[serde(default, skip_serializing_if = "WeightsConfig::is_default")]
    pub weights: WeightsConfig,
}

/// `[node]` in config.toml (all optional; detection fills the rest).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeConfig {
    /// This machine's name in topologies and `weights on:` badges (default: the hostname).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl NodeConfig {
    pub fn is_default(&self) -> bool {
        *self == NodeConfig::default()
    }
}

/// `[serve]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServeConfig {
    /// Accept whole recipes from a controller (`POST /api/run`). A recipe is a
    /// command line, so this is off by default; starting recipes this machine
    /// already has by name never needs it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_recipes: bool,
    /// Name of an environment variable holding the API token (so a fixed
    /// token survives restarts without living in this file). `--token` /
    /// `LMDECK_TOKEN` win over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
}

impl ServeConfig {
    pub fn is_default(&self) -> bool {
        *self == ServeConfig::default()
    }
}

/// `[weights]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WeightsConfig {
    /// Let lmdeck add the read-only `/etc/exports` line and run `exportfs -ra`
    /// itself (with `sudo -n`, so a password prompt fails instead of hanging).
    /// Off by default: lmdeck prints the two commands for you to run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_export: bool,
    /// The directory exported to peers (default: the Hugging Face home, the
    /// parent of `hub/`, which is what containers mount at /root/.cache/huggingface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nfs_path: Option<String>,
}

impl WeightsConfig {
    pub fn is_default(&self) -> bool {
        *self == WeightsConfig::default()
    }
}

/// `[updates]`: how lmdeck looks for new commits upstream of wrapped recipes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateChecks {
    /// `false` turns the checks off (also `LMDECK_NO_UPDATE_CHECK=1`).
    #[serde(default = "yes")]
    pub check: bool,
    /// How long a result is reused before the remote is asked again.
    #[serde(default = "default_ttl_hours")]
    pub ttl_hours: f64,
    /// Per-repo limit for one `git ls-remote`.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn yes() -> bool {
    true
}

fn default_ttl_hours() -> f64 {
    6.0
}

fn default_timeout_secs() -> u64 {
    5
}

impl Default for UpdateChecks {
    fn default() -> Self {
        UpdateChecks { check: true, ttl_hours: default_ttl_hours(), timeout_secs: default_timeout_secs() }
    }
}

impl UpdateChecks {
    pub fn is_default(&self) -> bool {
        *self == UpdateChecks::default()
    }
}

/// Explicit locations for engines that are not on `$PATH`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EngineOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llama_server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vllm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// A TabbyAPI checkout (the directory holding `main.py`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tabbyapi_dir: Option<String>,
    /// The `lmdeploy` CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmdeploy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lms: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mlx_server: Option<String>,
    /// TensorFold binary (`tensorfold`, or the release archive's `tensorfold-native`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensorfold: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<String>,
    /// LM Studio's server port (default 1234).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmstudio_port: Option<u16>,
}

/// A named recipe repository, e.g. `{ name = "mine", spec = "github:me/recipes" }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceEntry {
    pub name: String,
    pub spec: String,
}

fn default_models_dir() -> String {
    "~/models".into()
}

fn default_host() -> String {
    "0.0.0.0".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema: crate::schema::CURRENT,
            skipped: None,
            models_dir: default_models_dir(),
            host: default_host(),
            theme: None,
            engines: EngineOverrides::default(),
            // No default source: the lmswitch community repo (jvr0x/ai-models)
            // is YAML, which lmdeck no longer reads.
            sources: vec![],
            updates: UpdateChecks::default(),
            node: NodeConfig::default(),
            serve: ServeConfig::default(),
            weights: WeightsConfig::default(),
        }
    }
}

impl Config {
    /// Loads the config, or the defaults when no file exists yet.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        if !file.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        match Self::parse(&text) {
            Err(e) if crate::schema::is_unsupported(&e) => {
                // A config from a newer lmdeck: don't guess at it, run on defaults.
                Ok(Config { skipped: Some(format!("{}: {e:#}", file.display())), ..Config::default() })
            }
            r => r.with_context(|| format!("parsing {}", file.display())),
        }
    }

    /// Parses config TOML (a newer `schema` is an [`crate::schema::Unsupported`] error).
    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        let v = crate::schema::check("config", v)?;
        Ok(v.try_into()?)
    }

    /// Writes the config (creating the config dir).
    pub fn save(&self, paths: &Paths) -> Result<PathBuf> {
        fs::create_dir_all(&paths.config_dir)?;
        let file = paths.config_file();
        let body =
            format!("# lmdeck configuration — https://github.com/jvr0x/lmdeck\n{}", toml::to_string_pretty(self)?);
        fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
        Ok(file)
    }

    /// The expanded models directory. `LMDECK_MODELS_DIR` overrides the file.
    pub fn models_dir(&self) -> PathBuf {
        match std::env::var("LMDECK_MODELS_DIR") {
            Ok(v) if !v.trim().is_empty() => expand(&v),
            _ => expand(&self.models_dir),
        }
    }

    /// Saves `theme = "<slug>"` (or removes it, for `None`) without touching
    /// the rest of the file: comments and other settings stay as they are.
    pub fn save_theme(paths: &Paths, theme: Option<&str>) -> Result<PathBuf> {
        let file = paths.config_file();
        if !file.exists() {
            let cfg = Config { theme: theme.map(str::to_string), ..Config::default() };
            return cfg.save(paths);
        }
        let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let out = set_top_level(&text, "theme", theme.map(|t| toml::Value::String(t.into()).to_string()).as_deref());
        Self::parse(&out).context("updated config would not parse")?;
        let tmp = file.with_extension("toml.tmp");
        fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &file)?;
        Ok(file)
    }

    /// This machine's name as other hosts see it: `[node] name`, else the hostname.
    pub fn node_name(&self) -> String {
        self.node.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(crate::util::hostname)
    }

    pub fn source(&self, name: &str) -> Option<&SourceEntry> {
        self.sources.iter().find(|s| s.name == name)
    }
}

/// Sets (or, with `None`, removes) a top-level `key = value` line in TOML
/// text. Top-level keys must come before the first `[table]`, so a new line
/// goes just above it.
pub fn set_top_level(text: &str, key: &str, value: Option<&str>) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = lines.iter().position(|l| l.trim_start().starts_with('['));
    let top = header.unwrap_or(lines.len());
    let is_key = |l: &str| {
        let l = l.trim_start();
        l.strip_prefix(key).is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let new = value.map(|v| format!("{key} = {v}"));
    match (lines[..top].iter().position(|l| is_key(l)), new) {
        (Some(i), Some(n)) => lines[i] = n,
        (Some(i), None) => {
            lines.remove(i);
        }
        (None, Some(n)) => {
            // Above the first table, after any blank lines that precede it.
            let mut at = top;
            while at > 0 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, n);
            if header.is_some() && at == top {
                lines.insert(at + 1, String::new());
            }
        }
        (None, None) => {}
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_no_sources() {
        let c = Config::default();
        assert_eq!(c.models_dir, "~/models");
        assert!(c.sources.is_empty());
    }

    #[test]
    fn parses_partial_config_and_roundtrips() {
        let c = Config::parse("models_dir = \"/data/models\"\n[engines]\ntabbyapi_dir = \"~/tabbyAPI\"\n").unwrap();
        assert_eq!(c.models_dir, "/data/models");
        assert_eq!(c.host, "0.0.0.0");
        assert!(c.sources.is_empty());
        assert_eq!(c.engines.tabbyapi_dir.as_deref(), Some("~/tabbyAPI"));
        let back = Config::parse(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Config::parse("models_dir = [").is_err());
    }

    #[test]
    fn schema_missing_current_and_newer() {
        assert_eq!(Config::parse("models_dir = \"/m\"\n").unwrap().schema, 1);
        assert!(toml::to_string(&Config::default()).unwrap().starts_with("schema = 1\n"));
        let d = tempfile::tempdir().unwrap();
        let paths = Paths { config_dir: d.path().into(), cache_dir: d.path().join("c"), state_dir: d.path().join("s") };
        fs::write(paths.config_file(), "schema = 3\nmin_lmdeck = \"2.0.0\"\nmodels_dir = \"/x\"\n").unwrap();
        let c = Config::load(&paths).unwrap();
        assert_eq!(c.models_dir, "~/models", "a newer config is skipped, defaults used");
        assert!(c.skipped.as_deref().unwrap().contains("needs lmdeck >= 2.0.0 / schema 3"), "{:?}", c.skipped);
        // …and never overwritten.
        assert!(Config::save_theme(&paths, Some("nord")).is_err());
        assert!(fs::read_to_string(paths.config_file()).unwrap().starts_with("schema = 3"));
    }

    #[test]
    fn theme_edits_keep_the_rest_of_the_file() {
        let text = "# my notes\nmodels_dir = \"/data\" # keep\n\n[engines]\n# tabby\ntabbyapi_dir = \"~/t\"\n";
        let a = set_top_level(text, "theme", Some("\"nord\""));
        assert_eq!(
            a,
            "# my notes\nmodels_dir = \"/data\" # keep\ntheme = \"nord\"\n\n[engines]\n# tabby\ntabbyapi_dir = \"~/t\"\n"
        );
        assert_eq!(Config::parse(&a).unwrap().theme.as_deref(), Some("nord"));
        let b = set_top_level(&a, "theme", Some("\"rose-pine\""));
        assert_eq!(b.matches("theme =").count(), 1);
        assert_eq!(Config::parse(&b).unwrap().theme.as_deref(), Some("rose-pine"));
        let c = set_top_level(&b, "theme", None);
        assert_eq!(c, text);
        // A `theme` key inside a table is not the top-level one.
        let t = set_top_level("[x]\ntheme = 1\n", "theme", Some("\"omp\""));
        assert_eq!(t, "theme = \"omp\"\n\n[x]\ntheme = 1\n");
        assert_eq!(set_top_level("", "theme", Some("\"omp\"")), "theme = \"omp\"\n");
    }

    #[test]
    fn save_theme_persists_and_creates_the_file() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        let paths = Paths { config_dir: dir.join("cfg"), cache_dir: dir.join("cache"), state_dir: dir.join("state") };
        Config::save_theme(&paths, Some("kanagawa")).unwrap();
        assert_eq!(Config::load(&paths).unwrap().theme.as_deref(), Some("kanagawa"));
        assert!(Config::load(&paths).unwrap().sources.is_empty());
        fs::write(paths.config_file(), "# hand-written\nmodels_dir = \"/m\"\n").unwrap();
        Config::save_theme(&paths, Some("white")).unwrap();
        let text = fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.starts_with("# hand-written\n"), "{text}");
        assert_eq!(Config::load(&paths).unwrap().theme.as_deref(), Some("white"));
        Config::save_theme(&paths, None).unwrap();
        assert_eq!(Config::load(&paths).unwrap().theme, None);
    }
}
