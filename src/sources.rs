//! Recipe sources: GitHub repos, Hugging Face repos, and local directories.
//!
//! Fetching uses only public HTTPS endpoints:
//! * GitHub: `api.github.com/repos/{o}/{r}/git/trees/{ref}?recursive=1` to
//!   list, `raw.githubusercontent.com` to download.
//! * Hugging Face: `huggingface.co/api/{models|datasets}/{repo}/tree/{rev}?recursive=true`
//!   to list, `huggingface.co/{repo}/resolve/{rev}/{path}` to download.
//!
//! `GITHUB_TOKEN`/`GH_TOKEN` and `HF_TOKEN` are sent when set (higher rate
//! limits, private repos); they are never written anywhere.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::paths::expand;
use crate::recipe::{is_not_a_recipe, is_recipe_file, Recipe};
use crate::util::now_secs;

/// Files fetched per source, at most (keeps a mistaken `source add` cheap).
pub const MAX_FILES: usize = 500;
const MAX_FILE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HfKind {
    Model,
    Dataset,
}

/// A parsed source spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    GitHub { owner: String, repo: String, git_ref: Option<String>, subdir: Option<String> },
    Hf { kind: HfKind, repo: String, revision: Option<String>, subdir: Option<String> },
    Local(PathBuf),
}

fn split_ref(s: &str) -> (&str, Option<String>) {
    match s.rsplit_once('@') {
        Some((head, r)) if !r.is_empty() => (head, Some(r.to_string())),
        _ => (s, None),
    }
}

fn nonempty(parts: &[&str]) -> Option<String> {
    let joined = parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("/");
    (!joined.is_empty()).then_some(joined)
}

impl SourceSpec {
    /// Parses `github:owner/repo[/subdir][@ref]`, `hf:[datasets/]owner/repo[/subdir][@rev]`,
    /// GitHub / Hugging Face URLs (including `/tree/<ref>/<subdir>`), or a local path.
    pub fn parse(spec: &str) -> Result<SourceSpec> {
        let s = spec.trim().trim_end_matches('/');
        if let Some(rest) = s.strip_prefix("path:") {
            return Ok(SourceSpec::Local(expand(rest)));
        }
        if s.starts_with('/') || s.starts_with("~") || s.starts_with("./") || s.starts_with("../") {
            return Ok(SourceSpec::Local(expand(s)));
        }
        if let Some(rest) = s.strip_prefix("github:").or_else(|| s.strip_prefix("gh:")) {
            let (path, git_ref) = split_ref(rest);
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
                bail!("github source should look like github:owner/repo[/subdir][@ref]");
            }
            return Ok(SourceSpec::GitHub {
                owner: parts[0].into(),
                repo: parts[1].trim_end_matches(".git").into(),
                git_ref,
                subdir: nonempty(&parts[2..]),
            });
        }
        if let Some(rest) = s.strip_prefix("hf:").or_else(|| s.strip_prefix("huggingface:")) {
            let (path, revision) = split_ref(rest);
            let (kind, path) = match path.strip_prefix("datasets/") {
                Some(p) => (HfKind::Dataset, p),
                None => (HfKind::Model, path),
            };
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
                bail!("hf source should look like hf:[datasets/]owner/repo[/subdir][@revision]");
            }
            return Ok(SourceSpec::Hf {
                kind,
                repo: format!("{}/{}", parts[0], parts[1]),
                revision,
                subdir: nonempty(&parts[2..]),
            });
        }
        for (prefix, gh) in [
            ("https://github.com/", true),
            ("http://github.com/", true),
            ("https://huggingface.co/", false),
            ("https://hf.co/", false),
        ] {
            let Some(rest) = s.strip_prefix(prefix) else { continue };
            let mut parts: Vec<&str> = rest.split('/').collect();
            let kind = if !gh && parts.first() == Some(&"datasets") {
                parts.remove(0);
                HfKind::Dataset
            } else {
                HfKind::Model
            };
            if parts.len() < 2 {
                bail!("could not parse repository from {spec}");
            }
            let (owner, repo) = (parts[0], parts[1].trim_end_matches(".git"));
            let (r, sub) = if parts.get(2) == Some(&"tree") && parts.len() >= 4 {
                (Some(parts[3].to_string()), nonempty(&parts[4..]))
            } else {
                (None, None)
            };
            return Ok(if gh {
                SourceSpec::GitHub { owner: owner.into(), repo: repo.into(), git_ref: r, subdir: sub }
            } else {
                SourceSpec::Hf { kind, repo: format!("{owner}/{repo}"), revision: r, subdir: sub }
            });
        }
        if Path::new(s).is_dir() {
            return Ok(SourceSpec::Local(PathBuf::from(s)));
        }
        Err(anyhow!(
            "unrecognised source '{spec}': use github:owner/repo, hf:owner/repo, a GitHub/Hugging Face URL, or a local directory"
        ))
    }

    /// Canonical spec string (what is stored in config.toml).
    pub fn canonical(&self) -> String {
        let suffix = |sub: &Option<String>, r: &Option<String>| {
            let mut s = String::new();
            if let Some(sub) = sub {
                s.push('/');
                s.push_str(sub);
            }
            if let Some(r) = r {
                s.push('@');
                s.push_str(r);
            }
            s
        };
        match self {
            SourceSpec::GitHub { owner, repo, git_ref, subdir } => {
                format!("github:{owner}/{repo}{}", suffix(subdir, git_ref))
            }
            SourceSpec::Hf { kind, repo, revision, subdir } => {
                let ds = if *kind == HfKind::Dataset { "datasets/" } else { "" };
                format!("hf:{ds}{repo}{}", suffix(subdir, revision))
            }
            SourceSpec::Local(p) => format!("path:{}", p.display()),
        }
    }

    /// A short default name: the repo (or directory) name.
    pub fn default_name(&self) -> String {
        let raw = match self {
            SourceSpec::GitHub { repo, .. } => repo.clone(),
            SourceSpec::Hf { repo, .. } => repo.rsplit('/').next().unwrap_or(repo).to_string(),
            SourceSpec::Local(p) => {
                p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "local".into())
            }
        };
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect()
    }
}

/// Is this repo-relative path a candidate recipe file? Only `.toml`: YAML
/// files (lmswitch recipes) are not recipes any more.
pub fn is_candidate(path: &str, subdir: Option<&str>) -> bool {
    if let Some(sub) = subdir {
        let sub = sub.trim_matches('/');
        if !(path.starts_with(&format!("{sub}/"))) {
            return false;
        }
    }
    if path.split('/').any(|seg| seg.starts_with('.')) {
        return false;
    }
    is_recipe_file(Path::new(path))
}

/// Written next to the mirrored files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceIndex {
    pub spec: String,
    pub fetched_at: u64,
    pub files: Vec<String>,
}

pub const INDEX_FILE: &str = ".lmdeck-source.json";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("lmdeck/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn env_token(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

fn get(agent: &ureq::Agent, url: &str, token: &Option<String>, gh: bool) -> Result<ureq::Response> {
    let mut req = agent.get(url);
    if let Some(t) = token {
        req = req.set("Authorization", &if gh { format!("token {t}") } else { format!("Bearer {t}") });
    }
    if gh && url.contains("api.github.com") {
        req = req.set("Accept", "application/vnd.github+json");
    }
    match req.call() {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(code, resp)) => {
            let hint = match code {
                401 | 403 if gh => " (rate-limited or private: set GITHUB_TOKEN)",
                401 | 403 => " (private or gated: set HF_TOKEN)",
                404 => " (not found)",
                _ => "",
            };
            let body: String = resp.into_string().unwrap_or_default().chars().take(200).collect();
            bail!("GET {url} → HTTP {code}{hint} {body}")
        }
        Err(e) => Err(anyhow!("GET {url}: {e}")),
    }
}

fn read_limited(resp: ureq::Response) -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    resp.into_reader().take(MAX_FILE_BYTES).read_to_string(&mut buf)?;
    Ok(buf)
}

fn encode_path(p: &str) -> String {
    p.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Lists candidate files and returns `(relative path, contents)` pairs.
fn fetch_files(spec: &SourceSpec) -> Result<Vec<(String, String)>> {
    match spec {
        SourceSpec::Local(dir) => {
            if !dir.is_dir() {
                bail!("{} is not a directory", dir.display());
            }
            let mut out = vec![];
            fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, String)>) -> Result<()> {
                if depth > 6 || out.len() >= MAX_FILES {
                    return Ok(());
                }
                let mut entries: Vec<_> = fs::read_dir(dir)?.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for e in entries {
                    let p = e.path();
                    let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                    if p.is_dir() {
                        if !e.file_name().to_string_lossy().starts_with('.') {
                            walk(root, &p, depth + 1, out)?;
                        }
                    } else if is_candidate(&rel, None)
                        && e.metadata().map(|m| m.len() <= MAX_FILE_BYTES).unwrap_or(false)
                        && out.len() < MAX_FILES
                    {
                        out.push((rel, fs::read_to_string(&p)?));
                    }
                }
                Ok(())
            }
            walk(dir, dir, 0, &mut out)?;
            Ok(out)
        }
        SourceSpec::GitHub { owner, repo, git_ref, subdir } => {
            let a = agent();
            let token = env_token(&["GITHUB_TOKEN", "GH_TOKEN"]);
            let git_ref = match git_ref {
                Some(r) => r.clone(),
                None => {
                    let meta: serde_json::Value =
                        get(&a, &format!("https://api.github.com/repos/{owner}/{repo}"), &token, true)?.into_json()?;
                    meta["default_branch"].as_str().unwrap_or("main").to_string()
                }
            };
            let tree: serde_json::Value = get(
                &a,
                &format!("https://api.github.com/repos/{owner}/{repo}/git/trees/{}?recursive=1", encode_path(&git_ref)),
                &token,
                true,
            )?
            .into_json()?;
            let paths: Vec<String> = tree["tree"]
                .as_array()
                .ok_or_else(|| anyhow!("unexpected GitHub tree response"))?
                .iter()
                .filter(|e| e["type"] == "blob" && e["size"].as_u64().unwrap_or(0) <= MAX_FILE_BYTES)
                .filter_map(|e| e["path"].as_str().map(String::from))
                .filter(|p| is_candidate(p, subdir.as_deref()))
                .take(MAX_FILES)
                .collect();
            paths
                .into_iter()
                .map(|p| {
                    let url = format!(
                        "https://raw.githubusercontent.com/{owner}/{repo}/{}/{}",
                        encode_path(&git_ref),
                        encode_path(&p)
                    );
                    Ok((p, read_limited(get(&a, &url, &token, true)?)?))
                })
                .collect()
        }
        SourceSpec::Hf { kind, repo, revision, subdir } => {
            let a = agent();
            let token = env_token(&["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"]);
            let rev = revision.clone().unwrap_or_else(|| "main".into());
            let (api_kind, url_prefix) = match kind {
                HfKind::Model => ("models", String::new()),
                HfKind::Dataset => ("datasets", "datasets/".to_string()),
            };
            let tree: serde_json::Value = get(
                &a,
                &format!("https://huggingface.co/api/{api_kind}/{repo}/tree/{}?recursive=true", encode_path(&rev)),
                &token,
                false,
            )?
            .into_json()?;
            let paths: Vec<String> = tree
                .as_array()
                .ok_or_else(|| anyhow!("unexpected Hugging Face tree response"))?
                .iter()
                .filter(|e| e["type"] == "file" && e["size"].as_u64().unwrap_or(0) <= MAX_FILE_BYTES)
                .filter_map(|e| e["path"].as_str().map(String::from))
                .filter(|p| is_candidate(p, subdir.as_deref()))
                .take(MAX_FILES)
                .collect();
            paths
                .into_iter()
                .map(|p| {
                    let url = format!(
                        "https://huggingface.co/{url_prefix}{repo}/resolve/{}/{}",
                        encode_path(&rev),
                        encode_path(&p)
                    );
                    Ok((p, read_limited(get(&a, &url, &token, false)?)?))
                })
                .collect()
        }
    }
}

/// Fetches a source into `dest` (atomically replacing the previous mirror).
pub fn sync(spec: &SourceSpec, dest: &Path) -> Result<SourceIndex> {
    let files = fetch_files(spec).with_context(|| format!("fetching {}", spec.canonical()))?;
    let parent = dest.parent().ok_or_else(|| anyhow!("bad cache path"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dest.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let mut names = vec![];
    for (rel, body) in &files {
        if rel.split('/').any(|s| s == ".." || s.is_empty()) {
            continue;
        }
        let out = tmp.join(rel);
        if let Some(p) = out.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&out, body)?;
        names.push(rel.clone());
    }
    let index = SourceIndex { spec: spec.canonical(), fetched_at: now_secs(), files: names };
    fs::write(tmp.join(INDEX_FILE), serde_json::to_string_pretty(&index)?)?;
    let _ = fs::remove_dir_all(dest);
    fs::rename(&tmp, dest)?;
    Ok(index)
}

pub fn read_index(dir: &Path) -> Option<SourceIndex> {
    serde_json::from_str(&fs::read_to_string(dir.join(INDEX_FILE)).ok()?).ok()
}

/// A recipe plus where it came from.
#[derive(Debug, Clone)]
pub struct Found {
    pub recipe: Recipe,
    /// `deck`, `builtin`, or a source name.
    pub origin: String,
    /// File path relative to the source root (or absolute for the deck).
    pub file: String,
}

/// Loads every recipe in a mirrored source directory. Returns recipes plus
/// `(file, error)` for files that look like recipes but failed to load.
pub fn load_dir(origin: &str, dir: &Path) -> (Vec<Found>, Vec<(String, String)>) {
    let mut found = vec![];
    let mut errors = vec![];
    let files = match read_index(dir) {
        Some(idx) => idx.files,
        None => return (found, errors),
    };
    for rel in files {
        let path = dir.join(&rel);
        let Ok(text) = fs::read_to_string(&path) else { continue };
        if !is_recipe_file(&path) {
            continue; // a mirror fetched before YAML support was dropped
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("recipe");
        match Recipe::parse(&text, stem) {
            Ok(recipe) => found.push(Found { recipe, origin: origin.into(), file: rel }),
            Err(e) if is_not_a_recipe(&e) => {}
            Err(e) => errors.push((rel, format!("{e:#}"))),
        }
    }
    (found, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        assert_eq!(
            SourceSpec::parse("github:jvr0x/ai-models").unwrap(),
            SourceSpec::GitHub { owner: "jvr0x".into(), repo: "ai-models".into(), git_ref: None, subdir: None }
        );
        assert_eq!(
            SourceSpec::parse("github:o/r/recipes/spark@v1").unwrap(),
            SourceSpec::GitHub {
                owner: "o".into(),
                repo: "r".into(),
                git_ref: Some("v1".into()),
                subdir: Some("recipes/spark".into())
            }
        );
        assert_eq!(
            SourceSpec::parse("https://github.com/o/r/tree/dev/recipes").unwrap().canonical(),
            "github:o/r/recipes@dev"
        );
        assert_eq!(SourceSpec::parse("https://github.com/o/r.git").unwrap().canonical(), "github:o/r");
        assert_eq!(
            SourceSpec::parse("hf:datasets/o/r@rev").unwrap(),
            SourceSpec::Hf { kind: HfKind::Dataset, repo: "o/r".into(), revision: Some("rev".into()), subdir: None }
        );
        assert_eq!(SourceSpec::parse("https://huggingface.co/o/r/tree/main/x").unwrap().canonical(), "hf:o/r/x@main");
        assert_eq!(SourceSpec::parse("https://huggingface.co/datasets/o/r").unwrap().canonical(), "hf:datasets/o/r");
        assert!(matches!(SourceSpec::parse("/tmp").unwrap(), SourceSpec::Local(_)));
        assert!(SourceSpec::parse("github:onlyowner").is_err());
        assert!(SourceSpec::parse("ftp://nope").is_err());
    }

    #[test]
    fn canonical_roundtrips() {
        for s in ["github:a/b", "github:a/b/sub@x", "hf:a/b", "hf:datasets/a/b/s@r", "path:/tmp/x"] {
            assert_eq!(SourceSpec::parse(s).unwrap().canonical(), s);
        }
        assert_eq!(SourceSpec::parse("hf:org/My Recipes").unwrap().default_name(), "My-Recipes");
    }

    #[test]
    fn candidate_filter() {
        assert!(is_candidate("qwen.toml", None));
        assert!(!is_candidate("qwen.yaml", None) && !is_candidate("qwen.yml", None));
        assert!(is_candidate("recipes/a.toml", Some("recipes")));
        assert!(!is_candidate("other/a.toml", Some("recipes")));
        assert!(!is_candidate(".github/workflows/ci.yml", None));
        assert!(!is_candidate("README.md", None));
    }

    #[test]
    fn syncs_local_dir_and_loads() {
        let src = tempfile::tempdir().unwrap();
        fs::write(
            src.path().join("gpt.toml"),
            "[model]\nrepo = \"openai/gpt-oss-20b-GGUF\"\nfiles = [\"gpt-oss-20b-Q4.gguf\"]\n[engine]\nname = \"llama.cpp\"\nport = 8086\n",
        )
        .unwrap();
        fs::create_dir_all(src.path().join("nested")).unwrap();
        fs::write(src.path().join("nested/x.toml"), "[model]\nrepo=\"a/b\"\n[engine]\nname=\"vllm\"\n").unwrap();
        fs::write(src.path().join("broken.toml"), "[model]\nrepo=\"bad\"\n[engine]\nname=\"vllm\"\n").unwrap();
        fs::write(src.path().join("README.md"), "# hi").unwrap();
        fs::write(src.path().join("notes.toml"), "title = \"not a recipe\"\n").unwrap();
        fs::write(src.path().join("old.yaml"), "runtime: llama\nmodel: a/b/c.gguf\n").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let dest = cache.path().join("sources/local");
        let idx = sync(&SourceSpec::Local(src.path().into()), &dest).unwrap();
        assert_eq!(idx.files.len(), 4);
        let (found, errors) = load_dir("local", &dest);
        let mut names: Vec<_> = found.iter().map(|f| f.recipe.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["gpt", "x"]);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "broken.toml");
        // Re-sync replaces the mirror.
        fs::remove_file(src.path().join("gpt.toml")).unwrap();
        sync(&SourceSpec::Local(src.path().into()), &dest).unwrap();
        assert_eq!(load_dir("local", &dest).0.len(), 1);
    }
}
