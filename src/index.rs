//! The recipe index: `/index.toml` in the lmdeck repo, a list of known
//! model repos (and, optionally, a native recipe for each) to browse and
//! install with `lmdeck add <name>`.
//!
//! - Fetched from `[index] url` (default: this repo's `main`), cached at
//!   `~/.cache/lmdeck/index.toml` with a TTL (`[index] ttl_hours`, 6), and
//!   read from the cache when offline.
//! - GitHub URLs go through the contents API with a token when one is around
//!   (`GITHUB_TOKEN`, `GH_TOKEN`, else `gh auth token`), so a private repo
//!   works; without one, the raw URL. Any other URL only gets a token when
//!   `[index] token_env` names one. Nothing ever prompts: `gh` runs with
//!   `GH_PROMPT_DISABLED` and a timeout, and every failure falls back to the
//!   cache with a hint.
//! - `schema = 1` like recipes; a newer index is not used (the cache stays).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::IndexConfig;
use crate::paths::Paths;
use crate::telemetry::now_ms;

/// The default index: `index.toml` on this repo's `main`.
pub const DEFAULT_URL: &str = "https://github.com/javierbits/lmdeck/blob/main/index.toml";

const MAX_BYTES: u64 = 1 << 20;

/// The whole file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    #[serde(default = "crate::schema::current")]
    pub schema: u32,
    #[serde(default, rename = "entry", skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,
}

/// One `[[entry]]`: a repo you can run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// What `lmdeck add <name>` takes and what the recipe is called.
    pub name: String,
    /// Git URL of the repo (its README is what the details link to).
    pub repo: String,
    /// A tag / branch / commit to pin; the repo's default branch if absent.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Tags,
    /// Weights on disk, GB, as the repo states them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_gb: Option<f64>,
    /// A decode speed the repo itself reports (never measured by lmdeck).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<Speed>,
    /// A native lmdeck recipe for it: a URL, or a path relative to the index.
    /// Without one, `add` wraps the repo's start.sh / stop.sh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
    /// How many DGX Sparks it runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparks: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Speed {
    pub tok_s: f64,
    /// Where the number comes from (e.g. "repo README: 1 stream, prose").
    #[serde(default)]
    pub note: String,
}

impl Tags {
    /// `(key, value)` pairs, for filters and chips.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![];
        for (k, x) in
            [("engine", &self.engine), ("model", &self.model), ("family", &self.family), ("quant", &self.quant)]
        {
            if let Some(x) = x {
                v.push((k, x.clone()));
            }
        }
        if let Some(n) = self.sparks {
            v.push(("sparks", n.to_string()));
        }
        v.extend(self.other.iter().map(|t| ("tag", t.clone())));
        v
    }
}

impl Entry {
    /// Does `filter` (`value` or `key=value`, case-insensitive) match a tag?
    pub fn has_tag(&self, filter: &str) -> bool {
        let f = filter.trim().to_lowercase();
        let (k, val) = match f.split_once(['=', ':']) {
            Some((k, v)) => (Some(k.trim().to_string()), v.trim().to_string()),
            None => (None, f.clone()),
        };
        self.tags.pairs().iter().any(|(tk, tv)| tv.to_lowercase() == val && k.as_deref().is_none_or(|k| k == *tk))
    }

    pub fn sparks(&self) -> u32 {
        self.tags.sparks.unwrap_or(1)
    }

    /// The repo's README on the web.
    pub fn readme_url(&self) -> String {
        let base = self.repo.trim_end_matches('/').trim_end_matches(".git");
        format!("{base}#readme")
    }

    pub fn search_text(&self) -> String {
        let tags: Vec<String> = self.tags.pairs().into_iter().map(|(_, v)| v).collect();
        format!("{} {} {} {}", self.name, self.description, self.repo, tags.join(" "))
    }
}

impl Index {
    /// Parses index TOML (a newer `schema` is a [`crate::schema::Unsupported`] error).
    pub fn parse(text: &str) -> Result<Index> {
        let v: toml::Value = toml::from_str(text)?;
        let mut v = crate::schema::check("index", v)?;
        if let Some(t) = v.as_table_mut() {
            t.remove("min_lmdeck");
        }
        let idx: Index = v.try_into()?;
        let mut seen = std::collections::HashSet::new();
        for e in &idx.entries {
            if !crate::recipe::valid_name(&e.name) {
                bail!("index entry '{}': names are [A-Za-z0-9._-]", e.name);
            }
            if !seen.insert(e.name.as_str()) {
                bail!("index entry '{}' appears twice", e.name);
            }
            if e.repo.trim().is_empty() {
                bail!("index entry '{}' has no repo", e.name);
            }
        }
        Ok(idx)
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.name == name)
            .or_else(|| self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name)))
    }
}

// ---------------------------------------------------------------- fetching

/// One HTTP request, before it is sent (pure, so the auth rules are testable).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// `(owner, repo, ref, path)` of a GitHub file URL: `github.com/o/r/blob/<ref>/<path>`,
/// `raw.githubusercontent.com/o/r/<ref>/<path>` or `github:o/r/<path>@<ref>`.
pub fn github_file(url: &str) -> Option<(String, String, String, String)> {
    if let Some(rest) = url.strip_prefix("github:") {
        let (path, git_ref) = match rest.rsplit_once('@') {
            Some((p, r)) => (p, r.to_string()),
            None => (rest, "main".to_string()),
        };
        let mut it = path.splitn(3, '/');
        let (o, r, p) = (it.next()?, it.next()?, it.next()?);
        return Some((o.into(), r.into(), git_ref, p.into()));
    }
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    if let Some(r) = rest.strip_prefix("raw.githubusercontent.com/") {
        let mut it = r.splitn(4, '/');
        let (o, repo, git_ref, p) = (it.next()?, it.next()?, it.next()?, it.next()?);
        return Some((o.into(), repo.into(), git_ref.into(), p.into()));
    }
    if let Some(r) = rest.strip_prefix("github.com/") {
        let mut it = r.splitn(5, '/');
        let (o, repo, kind, git_ref, p) = (it.next()?, it.next()?, it.next()?, it.next()?, it.next()?);
        if kind == "blob" || kind == "raw" {
            return Some((o.into(), repo.into(), git_ref.into(), p.into()));
        }
    }
    None
}

fn github_api() -> String {
    std::env::var("LMDECK_GITHUB_API")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".into())
        .trim_end_matches('/')
        .to_string()
}

/// What to GET for `url`, with `token` if there is one. GitHub tokens only
/// ever go to GitHub (the contents API); `other_token` (from `[index]
/// token_env`) goes to whatever URL the config names.
pub fn request_for(url: &str, gh_token: Option<&str>, other_token: Option<&str>) -> Request {
    let ua = ("User-Agent".to_string(), concat!("lmdeck/", env!("CARGO_PKG_VERSION")).to_string());
    if let Some((o, r, git_ref, path)) = github_file(url) {
        if let Some(t) = gh_token {
            return Request {
                url: format!("{}/repos/{o}/{r}/contents/{path}?ref={git_ref}", github_api()),
                headers: vec![
                    ua,
                    ("Authorization".into(), format!("Bearer {t}")),
                    ("Accept".into(), "application/vnd.github.raw".into()),
                    ("X-GitHub-Api-Version".into(), "2022-11-28".into()),
                ],
            };
        }
        return Request {
            url: format!("https://raw.githubusercontent.com/{o}/{r}/{git_ref}/{path}"),
            headers: vec![ua],
        };
    }
    let mut headers = vec![ua];
    if let Some(t) = other_token {
        headers.push(("Authorization".into(), format!("Bearer {t}")));
    }
    Request { url: url.to_string(), headers }
}

/// A GitHub token from the environment, else `gh auth token` (no prompt, 3 s).
pub fn github_token() -> Option<String> {
    for k in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(v) = std::env::var(k).ok().filter(|v| !v.trim().is_empty()) {
            return Some(v.trim().to_string());
        }
    }
    if std::env::var("LMDECK_NO_GH").is_ok_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    let mut cmd = Command::new("gh");
    cmd.args(["auth", "token"]).env("GH_PROMPT_DISABLED", "1").env("GH_NO_UPDATE_NOTIFIER", "1");
    crate::util::run_with_timeout(&mut cmd, Duration::from_secs(3))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
}

fn is_local(url: &str) -> Option<PathBuf> {
    if let Some(p) = url.strip_prefix("file://") {
        return Some(PathBuf::from(p));
    }
    if url.contains("://") || url.starts_with("github:") {
        return None;
    }
    Some(crate::paths::expand(url))
}

/// GETs `url` (or reads a local path), with the auth rules above.
pub fn fetch_text(url: &str, cfg: &IndexConfig) -> Result<String> {
    if let Some(p) = is_local(url) {
        return fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()));
    }
    let gh = if github_file(url).is_some() { github_token() } else { None };
    let other = cfg.token_env.as_deref().and_then(|k| std::env::var(k).ok()).filter(|v| !v.trim().is_empty());
    let req = request_for(url, gh.as_deref(), other.as_deref());
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(cfg.timeout_secs.max(1)))
        .timeout(Duration::from_secs(cfg.timeout_secs.max(1) * 2))
        .build();
    let mut r = agent.get(&req.url);
    for (k, v) in &req.headers {
        r = r.set(k, v);
    }
    match r.call() {
        Ok(resp) => {
            use std::io::Read;
            let mut s = String::new();
            resp.into_reader().take(MAX_BYTES).read_to_string(&mut s)?;
            Ok(s)
        }
        Err(ureq::Error::Status(code, _)) => {
            let hint = match code {
                401 | 403 | 404 if github_file(url).is_some() && gh.is_none() => {
                    " (private repo? set GITHUB_TOKEN or run `gh auth login`)"
                }
                401 | 403 => " (token refused)",
                404 => " (not found)",
                _ => "",
            };
            Err(anyhow!("GET {} → HTTP {code}{hint}", req.url))
        }
        Err(e) => Err(anyhow!("GET {}: {e}", req.url)),
    }
}

/// `rel` (a recipe link in an entry) against the index URL.
pub fn resolve_link(index_url: &str, rel: &str) -> String {
    if rel.contains("://") || rel.starts_with('/') || rel.starts_with("github:") {
        return rel.to_string();
    }
    if let Some(p) = index_url.strip_prefix("github:") {
        let (path, git_ref) = p.rsplit_once('@').map(|(a, b)| (a, Some(b))).unwrap_or((p, None));
        let dir = path.rsplit_once('/').map(|x| x.0).unwrap_or(path);
        return format!("github:{dir}/{rel}{}", git_ref.map(|r| format!("@{r}")).unwrap_or_default());
    }
    match index_url.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{rel}"),
        None => rel.to_string(),
    }
}

// ---------------------------------------------------------------- cache

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Meta {
    url: String,
    fetched_ms: u64,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    error_ms: u64,
}

pub fn cache_file(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("index.toml")
}

fn meta_file(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("index.meta.json")
}

fn read_meta(paths: &Paths) -> Option<Meta> {
    serde_json::from_str(&fs::read_to_string(meta_file(paths)).ok()?).ok()
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// What a caller gets: an index (maybe empty) and how fresh it is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Loaded {
    pub index: Index,
    pub url: String,
    /// When the cached copy was fetched (unix ms), 0 if there is none.
    pub fetched_ms: u64,
    /// Read from the cache because fetching failed or wasn't attempted.
    pub from_cache: bool,
    /// Why the index may be old or missing, and what to do about it.
    pub hint: Option<String>,
}

impl Loaded {
    pub fn age_text(&self) -> String {
        if self.fetched_ms == 0 {
            return "never fetched".into();
        }
        crate::util::ago_ms(now_ms().saturating_sub(self.fetched_ms))
    }
}

pub fn url(cfg: &IndexConfig) -> String {
    std::env::var("LMDECK_INDEX_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| cfg.url.clone())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// The cached index, without touching the network. The cache belongs to the
/// URL it was fetched from: a copy fetched from another URL (an old default,
/// a changed `[index] url`) is never handed out for this one.
pub fn cached(paths: &Paths, cfg: &IndexConfig) -> Loaded {
    let url = url(cfg);
    let meta = read_meta(paths).unwrap_or_default();
    let other_url = (meta.url != url).then(|| meta.url.clone()).filter(|u| !u.is_empty() && meta.fetched_ms > 0);
    let ours = meta.url == url && meta.fetched_ms > 0;
    let text = if ours { fs::read_to_string(cache_file(paths)).ok() } else { None };
    let meta = if meta.url == url { meta } else { Meta::default() };
    let mut out = Loaded { url: url.clone(), fetched_ms: meta.fetched_ms, from_cache: true, ..Default::default() };
    match text.as_deref().map(Index::parse) {
        Some(Ok(i)) => out.index = i,
        Some(Err(e)) => out.hint = Some(format!("cached index unreadable: {e:#}")),
        None => {
            out.fetched_ms = 0;
            out.hint = Some(match (&meta.last_error, &other_url) {
                (Some(e), _) => format!("no index yet: {e}"),
                (None, Some(o)) => {
                    format!("the cached index is from {o}, not {url}; `lmdeck browse --refresh` fetches it")
                }
                (None, None) => "no index cached yet (`lmdeck browse` fetches it)".into(),
            });
        }
    }
    if out.hint.is_none() {
        if let Some(e) = meta.last_error.as_ref().filter(|_| meta.error_ms > meta.fetched_ms) {
            out.hint = Some(format!("using the cached index from {}: {e}", out.age_text()));
        }
    }
    out
}

/// Is the cache older than the TTL (or for another URL)?
pub fn stale(paths: &Paths, cfg: &IndexConfig) -> bool {
    let Some(m) = read_meta(paths) else { return true };
    if m.url != url(cfg) || !cache_file(paths).exists() {
        return true;
    }
    let ttl_ms = (cfg.ttl_hours.max(0.0) * 3_600_000.0) as u64;
    let last_try = m.fetched_ms.max(m.error_ms);
    now_ms().saturating_sub(last_try) >= ttl_ms.min(if m.error_ms > m.fetched_ms { 15 * 60_000 } else { u64::MAX })
}

/// Fetches the index if the cache is stale (or `force`), writes the cache,
/// and falls back to the cache on any failure. Never prompts.
pub fn load(paths: &Paths, cfg: &IndexConfig, force: bool) -> Loaded {
    if !force && !stale(paths, cfg) {
        return cached(paths, cfg);
    }
    let url = url(cfg);
    let res = fetch_text(&url, cfg).and_then(|t| Index::parse(&t).map(|i| (t, i)).context("the fetched index"));
    let mut meta = read_meta(paths).filter(|m| m.url == url).unwrap_or_default();
    match res {
        Ok((text, index)) => {
            let now = now_ms();
            let ok = write_atomic(&cache_file(paths), &text).is_ok();
            meta = Meta { url: url.clone(), fetched_ms: now, last_error: None, error_ms: 0 };
            let _ = serde_json::to_string(&meta).map(|m| write_atomic(&meta_file(paths), &m));
            let hint = (!ok).then(|| format!("couldn't write {}", cache_file(paths).display()));
            Loaded { index, url, fetched_ms: now, from_cache: false, hint }
        }
        Err(e) => {
            meta.last_error = Some(format!("{e:#}"));
            meta.error_ms = now_ms();
            meta.url = url.clone();

            let _ = serde_json::to_string(&meta).map(|m| write_atomic(&meta_file(paths), &m));
            let mut c = cached(paths, cfg);
            let base = if c.fetched_ms > 0 {
                format!("couldn't refresh the index ({e:#}); using the cached copy from {}", c.age_text())
            } else {
                format!("couldn't fetch the index: {e:#}")
            };
            c.hint = Some(base);
            c
        }
    }
}

/// Refreshes the cache in a background thread if it's stale (TUI / serve).
pub fn spawn_refresh(paths: Paths, cfg: IndexConfig) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if !stale(&paths, &cfg) || RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let _ = load(&paths, &cfg, false);
        RUNNING.store(false, Ordering::SeqCst);
    });
}

/// The native recipe an entry links to, fetched and parsed.
pub fn fetch_recipe(cfg: &IndexConfig, e: &Entry) -> Result<Option<crate::recipe::Recipe>> {
    let Some(link) = &e.recipe else { return Ok(None) };
    let u = resolve_link(&url(cfg), link);
    let text = fetch_text(&u, cfg).with_context(|| format!("fetching {}'s recipe", e.name))?;
    let mut r = crate::recipe::Recipe::parse(&text, &e.name).with_context(|| format!("{}'s recipe ({u})", e.name))?;
    r.name = e.name.clone();
    Ok(Some(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema = 1
[[entry]]
name = "a-one"
repo = "https://github.com/o/a"
ref = "v1"
description = "first"
tags = { engine = "vllm", family = "qwen", sparks = 2, quant = "nvfp4" }
speed = { tok_s = 40.0, note = "README" }
[[entry]]
name = "b-two"
repo = "https://github.com/o/b"
tags = { engine = "tensorfold", quant = "exl3" }
"#;

    #[test]
    fn parses_and_validates() {
        let i = Index::parse(SAMPLE).unwrap();
        assert_eq!(i.entries.len(), 2);
        assert_eq!(i.find("A-ONE").unwrap().git_ref.as_deref(), Some("v1"));
        assert_eq!(i.entries[1].sparks(), 1);
        assert!(
            i.entries[0].has_tag("sparks=2") && i.entries[0].has_tag("NVFP4") && i.entries[0].has_tag("engine:vllm")
        );
        assert!(!i.entries[0].has_tag("family=vllm"));
        assert!(Index::parse("[[entry]]\nname = \"x y\"\nrepo = \"r\"\n").is_err());
        assert!(Index::parse("[[entry]]\nname = \"x\"\nrepo = \"r\"\n[[entry]]\nname = \"x\"\nrepo = \"r\"\n").is_err());
        assert_eq!(Index::parse("").unwrap().schema, 1, "missing schema means 1");
        let e = Index::parse("schema = 5\nmin_lmdeck = \"1.0.0\"\n").unwrap_err();
        assert!(crate::schema::is_unsupported(&e) && format!("{e:#}").contains("needs lmdeck >= 1.0.0 / schema 5"));
    }

    #[test]
    fn github_urls_and_auth_rules() {
        let want = Some(("javierbits".into(), "lmdeck".into(), "main".into(), "index.toml".into()));
        assert_eq!(github_file(DEFAULT_URL), want);
        assert_eq!(github_file("https://raw.githubusercontent.com/javierbits/lmdeck/main/index.toml"), want);
        assert_eq!(github_file("github:javierbits/lmdeck/index.toml@main"), want);
        assert_eq!(github_file("https://example.com/index.toml"), None);
        // With a token: contents API, Bearer, raw media type.
        let r = request_for(DEFAULT_URL, Some("T0K"), Some("OTHER"));
        assert!(r.url.ends_with("/repos/javierbits/lmdeck/contents/index.toml?ref=main"), "{}", r.url);
        assert!(r.headers.contains(&("Authorization".into(), "Bearer T0K".into())));
        assert!(r.headers.contains(&("Accept".into(), "application/vnd.github.raw".into())));
        // Without: the raw URL, no auth.
        let r = request_for(DEFAULT_URL, None, Some("OTHER"));
        assert_eq!(r.url, "https://raw.githubusercontent.com/javierbits/lmdeck/main/index.toml");
        assert!(!r.headers.iter().any(|(k, _)| k == "Authorization"));
        // A GitHub token never goes to another host; token_env's does.
        let r = request_for("https://example.com/i.toml", Some("T0K"), None);
        assert!(!r.headers.iter().any(|(k, _)| k == "Authorization"));
        let r = request_for("https://example.com/i.toml", Some("T0K"), Some("OTHER"));
        assert!(r.headers.contains(&("Authorization".into(), "Bearer OTHER".into())));
    }

    #[test]
    fn recipe_links_resolve_against_the_index() {
        assert_eq!(
            resolve_link(DEFAULT_URL, "recipes/x.toml"),
            "https://github.com/javierbits/lmdeck/blob/main/recipes/x.toml"
        );
        assert_eq!(resolve_link("github:o/r/index.toml@v2", "recipes/x.toml"), "github:o/r/recipes/x.toml@v2");
        assert_eq!(resolve_link("/srv/idx/index.toml", "x.toml"), "/srv/idx/x.toml");
        assert_eq!(resolve_link(DEFAULT_URL, "https://h/x.toml"), "https://h/x.toml");
    }

    #[test]
    fn the_cache_belongs_to_its_url() {
        if std::env::var("LMDECK_INDEX_URL").is_ok() {
            return; // the env override would pick the URL for both configs
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(
            |k| (k == "LMDECK_HOME").then(|| dir.path().join("h").display().to_string()),
            dir.path().into(),
        );
        let a = dir.path().join("a.toml");
        fs::write(&a, SAMPLE).unwrap();
        let cfg_a = IndexConfig { url: Some(format!("file://{}", a.display())), ..Default::default() };
        let loaded = load(&paths, &cfg_a, true);
        assert_eq!(loaded.index.entries.len(), 2, "{:?}", loaded.hint);
        assert_eq!(cached(&paths, &cfg_a).index.entries.len(), 2);
        // Another URL never gets A's copy, even when fetching it fails.
        let cfg_b = IndexConfig { url: Some("file:///nonexistent/b.toml".into()), ..Default::default() };
        let c = cached(&paths, &cfg_b);
        assert!(c.index.entries.is_empty() && c.fetched_ms == 0);
        assert!(c.hint.as_deref().unwrap_or("").contains("a.toml"), "{:?}", c.hint);
        let l = load(&paths, &cfg_b, false);
        assert!(l.index.entries.is_empty(), "B must not reuse A's cache");
        assert!(l.hint.as_deref().unwrap_or("").starts_with("couldn't fetch the index"), "{:?}", l.hint);
    }

    #[test]
    fn the_repo_index_is_valid_and_real() {
        let text = include_str!("../index.toml");
        let i = Index::parse(text).unwrap();
        assert!(text.lines().any(|l| l.trim() == "schema = 1"));
        assert!(!i.entries.is_empty());
        for e in &i.entries {
            assert!(e.repo.starts_with("https://github.com/"), "{}", e.repo);
            assert!(!e.description.is_empty(), "{}", e.name);
            if let Some(link) = &e.recipe {
                let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(link);
                assert!(p.exists(), "{} links to missing {link}", e.name);
                crate::recipe::Recipe::load(&p).unwrap();
            }
        }
    }
}
