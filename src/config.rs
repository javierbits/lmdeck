//! `~/.config/lmdeck/config.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::paths::{expand, Paths};

/// The recipe repository every fresh config starts with: the community
/// recipes that shipped with lmswitch (lmswitch-format YAML, which lmdeck
/// reads natively).
pub const DEFAULT_SOURCE_NAME: &str = "ai-models";
pub const DEFAULT_SOURCE_SPEC: &str = "github:jvr0x/ai-models";

/// User configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Where model weights live. Recipe `model.path` values are relative to it.
    #[serde(default = "default_models_dir")]
    pub models_dir: String,
    /// Interface servers bind to.
    #[serde(default = "default_host")]
    pub host: String,
    /// Optional per-engine binary / directory overrides.
    #[serde(default)]
    pub engines: EngineOverrides,
    /// Recipe repositories.
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lms: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mlx_server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<String>,
    /// LM Studio's server port (default 1234).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmstudio_port: Option<u16>,
}

/// A named recipe repository, e.g. `{ name = "ai-models", spec = "github:jvr0x/ai-models" }`.
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
            models_dir: default_models_dir(),
            host: default_host(),
            engines: EngineOverrides::default(),
            sources: vec![SourceEntry { name: DEFAULT_SOURCE_NAME.into(), spec: DEFAULT_SOURCE_SPEC.into() }],
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
        Self::parse(&text).with_context(|| format!("parsing {}", file.display()))
    }

    /// Parses config TOML.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
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

    pub fn source(&self, name: &str) -> Option<&SourceEntry> {
        self.sources.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_ai_models_source() {
        let c = Config::default();
        assert_eq!(c.models_dir, "~/models");
        assert_eq!(c.sources[0].spec, DEFAULT_SOURCE_SPEC);
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
}
