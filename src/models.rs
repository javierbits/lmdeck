//! What is on disk: the Hugging Face cache and the models directory.

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::{expand, home_dir};
use crate::recipe::Recipe;
use crate::util::path_size;

/// A model found on disk.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Downloaded {
    /// Repo id (HF cache) or path relative to the models dir.
    pub id: String,
    /// `hf-cache` or `models-dir`.
    pub location: String,
    pub path: PathBuf,
    pub size: u64,
    /// `gguf`, `safetensors`, or `hf` for cache snapshots.
    pub kind: String,
}

/// The Hugging Face hub cache: `$HF_HUB_CACHE`, `$HF_HOME/hub`, or `~/.cache/huggingface/hub`.
pub fn hf_cache_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("HF_HUB_CACHE").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    if let Some(v) = std::env::var_os("HF_HOME").filter(|v| !v.is_empty()) {
        return expand(&v.to_string_lossy()).join("hub");
    }
    home_dir().join(".cache/huggingface/hub")
}

/// `models--org--name` → `org/name`.
pub fn repo_from_cache_dir(dir_name: &str) -> Option<String> {
    let rest = dir_name.strip_prefix("models--")?;
    let (org, name) = rest.split_once("--")?;
    Some(format!("{org}/{name}"))
}

/// Size of a repo's cache entry if it has any snapshot files.
pub fn hf_cached_size(cache: &Path, repo: &str) -> Option<u64> {
    let dir = cache.join(format!("models--{}", repo.replace('/', "--")));
    let snaps = dir.join("snapshots");
    let has_files = fs::read_dir(&snaps)
        .ok()?
        .flatten()
        .any(|s| fs::read_dir(s.path()).map(|mut it| it.next().is_some()).unwrap_or(false));
    has_files.then(|| path_size(&dir.join("blobs")))
}

pub fn scan_hf_cache(cache: &Path) -> Vec<Downloaded> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(cache) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(repo) = repo_from_cache_dir(&name) {
            if let Some(size) = hf_cached_size(cache, &repo) {
                out.push(Downloaded { id: repo, location: "hf-cache".into(), path: e.path(), size, kind: "hf".into() });
            }
        }
    }
    out.sort_by_key(|a| a.id.to_lowercase());
    out
}

/// `foo-00001-of-00003.gguf` → `Some((1, 3))`.
pub fn gguf_shard(file: &str) -> Option<(u32, u32)> {
    let stem = file.strip_suffix(".gguf")?;
    let (head, total) = stem.rsplit_once("-of-")?;
    let idx = head.rsplit('-').next()?;
    if idx.len() == 5 && total.len() == 5 {
        Some((idx.parse().ok()?, total.parse().ok()?))
    } else {
        None
    }
}

/// Size of a GGUF, summing every shard of a split model.
pub fn gguf_size(path: &Path) -> u64 {
    let Some(file) = path.file_name().and_then(|f| f.to_str()) else { return 0 };
    match (gguf_shard(file), path.parent()) {
        (Some((_, total)), Some(dir)) => {
            let suffix = format!("-of-{total:05}.gguf");
            let head = file.rsplit_once("-of-").map(|(h, _)| h).unwrap_or("");
            let prefix = format!("{}-", head.rsplit_once('-').map(|(h, _)| h).unwrap_or(head));
            fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            n.starts_with(&prefix) && n.ends_with(&suffix)
                        })
                        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                        .sum()
                })
                .unwrap_or(0)
        }
        _ => fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    }
}

/// GGUF files and safetensors model directories under `root` (depth ≤ 4).
pub fn scan_models_dir(root: &Path) -> Vec<Downloaded> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<Downloaded>) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        let has_weights = entries.iter().any(|e| e.file_name().to_string_lossy().ends_with(".safetensors"));
        let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        if has_weights && depth > 0 {
            out.push(Downloaded {
                id: rel(dir),
                location: "models-dir".into(),
                path: dir.to_path_buf(),
                size: path_size(dir),
                kind: "safetensors".into(),
            });
            return;
        }
        for e in entries {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                walk(root, &p, depth + 1, out);
            } else if name.ends_with(".gguf") && !name.to_lowercase().starts_with("mmproj") {
                if gguf_shard(&name).is_some_and(|(i, _)| i != 1) {
                    continue;
                }
                out.push(Downloaded {
                    id: rel(&p),
                    location: "models-dir".into(),
                    path: p.clone(),
                    size: gguf_size(&p),
                    kind: "gguf".into(),
                });
            }
        }
    }
    let mut out = vec![];
    walk(root, root, 0, &mut out);
    out
}

/// Whether a recipe's weights are present, and their size on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    /// The recipe's own local path exists.
    pub local: bool,
    /// The repo is in the Hugging Face cache.
    pub cached: bool,
    pub size: u64,
}

impl Presence {
    pub fn present(&self) -> bool {
        self.local || self.cached
    }
}

pub fn presence(r: &Recipe, models_dir: &Path, hf_cache: &Path) -> Presence {
    if let Some(p) = r.local_path(models_dir) {
        let local = if p.extension().is_some_and(|e| e == "gguf") {
            p.is_file()
        } else {
            p.is_dir() && fs::read_dir(&p).map(|mut d| d.next().is_some()).unwrap_or(false)
        };
        if local {
            let size = if p.is_file() { gguf_size(&p) } else { path_size(&p) };
            return Presence { local: true, cached: false, size };
        }
    }
    if let Some(repo) = &r.model.repo {
        if let Some(size) = hf_cached_size(hf_cache, repo) {
            return Presence { local: false, cached: true, size };
        }
    }
    Presence { local: false, cached: false, size: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_parsing() {
        assert_eq!(gguf_shard("Q-UD-IQ1_M-00001-of-00003.gguf"), Some((1, 3)));
        assert_eq!(gguf_shard("model.gguf"), None);
        assert_eq!(repo_from_cache_dir("models--unsloth--Qwen3-4B-GGUF").as_deref(), Some("unsloth/Qwen3-4B-GGUF"));
        assert_eq!(repo_from_cache_dir("datasets--a--b"), None);
    }

    #[test]
    fn scans_models_dir_and_sums_shards() {
        let d = tempfile::tempdir().unwrap();
        let g = d.path().join("unsloth/Big-GGUF");
        fs::create_dir_all(&g).unwrap();
        fs::write(g.join("Big-Q4-00001-of-00002.gguf"), vec![0u8; 10]).unwrap();
        fs::write(g.join("Big-Q4-00002-of-00002.gguf"), vec![0u8; 7]).unwrap();
        fs::write(g.join("mmproj-F16.gguf"), vec![0u8; 3]).unwrap();
        let s = d.path().join("nvidia/q-nvfp4");
        fs::create_dir_all(&s).unwrap();
        fs::write(s.join("model.safetensors"), vec![0u8; 4]).unwrap();
        fs::write(s.join("config.json"), b"{}").unwrap();
        let found = scan_models_dir(d.path());
        assert_eq!(found.len(), 2, "{found:?}");
        let gg = found.iter().find(|f| f.kind == "gguf").unwrap();
        assert_eq!(gg.id, "unsloth/Big-GGUF/Big-Q4-00001-of-00002.gguf");
        assert_eq!(gg.size, 17);
        let st = found.iter().find(|f| f.kind == "safetensors").unwrap();
        assert_eq!((st.id.as_str(), st.size), ("nvidia/q-nvfp4", 6));
    }

    #[test]
    fn hf_cache_scan_and_presence() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("models--mlx-community--Qwen3-4B-4bit");
        fs::create_dir_all(repo.join("snapshots/abc")).unwrap();
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::write(repo.join("blobs/x"), vec![0u8; 42]).unwrap();
        fs::write(repo.join("snapshots/abc/model.safetensors"), b"link").unwrap();
        let found = scan_hf_cache(d.path());
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].id.as_str(), found[0].size), ("mlx-community/Qwen3-4B-4bit", 42));

        let r = Recipe::parse("[model]\nrepo=\"mlx-community/Qwen3-4B-4bit\"\n[engine]\nname=\"mlx\"\n", "m").unwrap();
        let p = presence(&r, Path::new("/nonexistent"), d.path());
        assert!(p.cached && !p.local && p.present());
        let r =
            Recipe::parse("[model]\nrepo=\"a/b\"\nfiles=[\"x.gguf\"]\n[engine]\nname=\"llama.cpp\"\n", "m").unwrap();
        assert!(!presence(&r, d.path(), d.path()).present());
        fs::create_dir_all(d.path().join("a/b")).unwrap();
        fs::write(d.path().join("a/b/x.gguf"), vec![0u8; 5]).unwrap();
        let p = presence(&r, d.path(), d.path());
        assert!(p.local);
        assert_eq!(p.size, 5);
    }
}
