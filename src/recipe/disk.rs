//! Where a recipe's weights are on this machine. Hugging Face weights, GGUFs
//! included, live in the Hugging Face cache
//! (`<hub>/models--org--name/snapshots/<commit>/…`, the commit `[model]
//! revision` names, else `refs/main`); `[model] path` and the models dir
//! (`<models_dir>/<repo>/<file>`, `<models_dir>/<repo>[-<revision>]`) hold
//! local-only ones.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

use super::{Engine, Recipe};
use crate::models::on_disk;

impl Recipe {
    /// The first of `[model] files`: the file the recipe loads (a split GGUF's
    /// first shard). TabbyAPI loads a whole directory, so its `files` only
    /// narrow the download.
    pub fn first_file(&self) -> Option<&str> {
        self.model.files.first().map(String::as_str).filter(|_| self.engine.name != Engine::TabbyApi)
    }

    /// [`Recipe::first_file`] when it is a GGUF: a recipe naming one wants
    /// that quant, not whatever else its repo holds.
    pub fn first_gguf(&self) -> Option<&str> {
        self.first_file().filter(|f| f.ends_with(".gguf"))
    }

    /// The repo's weights in the Hugging Face cache `hub`, when they are there:
    /// the first file in the snapshot `[model] revision` names (else
    /// `refs/main`), or that snapshot itself for a recipe without one.
    pub fn cached_weights(&self, hub: &Path) -> Option<PathBuf> {
        let repo = self.model.repo.as_deref()?;
        let (_, snap) = crate::weights::resolve_snapshot(hub, repo, self.model.revision.as_deref()).ok()?;
        let p = match self.first_file() {
            Some(f) => snap.join(f),
            None => snap,
        };
        on_disk(&p).then_some(p)
    }

    /// What the engine loads: llama.cpp and TabbyAPI take the Hugging Face
    /// cache's copy when it is there and the recipe has no `[model] path`;
    /// every engine falls back to [`Recipe::local_path`].
    pub fn weights_path(&self, models_dir: &Path, hub: &Path) -> Option<PathBuf> {
        let cache = self.model.path.is_none() && matches!(self.engine.name, Engine::LlamaCpp | Engine::TabbyApi);
        cache.then(|| self.cached_weights(hub)).flatten().or_else(|| self.local_path(models_dir))
    }

    /// `{model_dir}`: the directory holding the recipe's first file on this
    /// machine (its weights directory, for a recipe without one). `[model]
    /// path` when set; else the Hugging Face cache, then the models dir. Fails
    /// when the weights aren't on this machine.
    pub fn model_dir(&self, models_dir: &Path, hub: &Path) -> Result<PathBuf> {
        let first = self.first_file();
        let place = match (&self.model.path, self.model.repo.as_deref()) {
            // A directory `path` holds the repo's files, the way `lmdeck pull` put them there.
            (Some(_), _) => self.local_path(models_dir).map(|p| match first {
                Some(f) if p.is_dir() => p.join(f),
                _ => p,
            }),
            (None, Some(repo)) => self.cached_weights(hub).or_else(|| {
                Some(match first {
                    Some(f) => models_dir.join(repo).join(f),
                    None => models_dir.join(self.default_dir_name()?),
                })
            }),
            (None, None) => bail!("the recipe names no weights ([model] repo or path)"),
        };
        let Some(p) = place.filter(|p| on_disk(p)) else {
            let what = match (&self.model.path, &self.model.repo, first) {
                (Some(p), ..) => p.clone(),
                (None, Some(repo), Some(f)) => format!("{repo}/{f}"),
                (None, repo, _) => repo.clone().unwrap_or_default(),
            };
            bail!(
                "{what} isn't on this machine (looked in {} and {}); `lmdeck pull` downloads it",
                hub.display(),
                models_dir.display()
            );
        };
        Ok(match p.parent() {
            Some(dir) if !p.is_dir() => dir.to_path_buf(),
            _ => p,
        })
    }
}
