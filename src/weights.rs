//! Where model weights live, across every host in `hosts.toml`.
//!
//! - Each `lmdeck serve` answers `GET /api/weights` with its own
//!   [`Inventory`]: the Hugging Face cache and the models directory, with
//!   sizes and (for HF) the snapshot revisions on disk, plus the directory it
//!   would export to peers over NFS.
//! - `GET /api/weights/files?repo=&revision=` lists one snapshot's files
//!   with sizes (what `find -L <snapshot> -type f -printf '%P %s\n'` prints):
//!   the reference list for the NFS preflight on the run host.
//! - The controller merges them into [`Index`], cached at
//!   `<cache>/weights-index.json` and refreshed at most every
//!   [`INDEX_TTL`], so lists, cards and details can say "weights on: spark-1".
//! - A model has one weights *home*: `[model] home = "<host>"` when set, else
//!   this machine if it has the weights, else the first host that does.
//!   Running it elsewhere streams them from the home over NFS
//!   ([`crate::placement`]); nothing is ever copied.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::hosts::{HostsFile, LOCAL};
use crate::models::{self, hf_cache_dir};
use crate::paths::Paths;
use crate::recipe::Recipe;
use crate::util::now_secs;

/// How long a merged index is reused before hosts are asked again.
pub const INDEX_TTL: Duration = Duration::from_secs(60);

/// One model on one machine's disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Repo id (HF cache) or path relative to the models dir.
    pub id: String,
    /// `hf-cache` or `models-dir`.
    pub location: String,
    /// `hf`, `gguf` or `safetensors`.
    pub kind: String,
    pub path: String,
    pub size: u64,
    /// HF snapshots on disk: commit hashes, with the refs pointing at them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<Revision>,
    /// A cache repo's GGUF model files (relative to their snapshot) and their
    /// sizes, so a recipe matches only the quant it names. Older hosts send none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ggufs: Vec<(String, u64)>,
}

impl Item {
    /// The size of `r`'s weights in this entry: its GGUF's when the entry lists
    /// them (`None` when that quant isn't among them), else the whole entry.
    pub fn holds(&self, r: &Recipe) -> Option<u64> {
        match r.first_gguf() {
            Some(f) if !self.ggufs.is_empty() => self.ggufs.iter().find(|(p, _)| p == f).map(|(_, s)| *s),
            _ => Some(self.size),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub commit: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// What one machine has, as its `/api/weights` reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    /// The machine's `[node] name` (or hostname).
    pub host: String,
    /// The HF hub cache (`…/hub`).
    pub hf_cache: String,
    /// What peers mount over NFS (`[weights] nfs_path`, else the HF home: the parent of `hub/`).
    pub export_path: String,
    /// Where that export goes in a container: `/root/.cache/huggingface`
    /// (or `…/huggingface/hub` when the export is the hub dir itself).
    pub mount_at: String,
    /// The hub dir relative to the export (`hub`, or empty).
    pub hub_rel: String,
    pub models_dir: String,
    pub items: Vec<Item>,
    #[serde(default)]
    pub demo: bool,
}

/// The export directory and where it sits in a container.
pub fn export_layout(cfg: &Config, hub: &Path) -> (PathBuf, &'static str, String) {
    if let Some(p) = cfg.weights.nfs_path.as_deref().filter(|p| !p.trim().is_empty()) {
        let p = crate::paths::expand(p);
        if p == hub {
            return (p, "/root/.cache/huggingface/hub", String::new());
        }
        let rel = hub.strip_prefix(&p).map(|r| r.display().to_string()).unwrap_or_else(|_| "hub".into());
        return (p, "/root/.cache/huggingface", rel);
    }
    match (hub.file_name().and_then(|n| n.to_str()), hub.parent()) {
        (Some("hub"), Some(parent)) => (parent.to_path_buf(), "/root/.cache/huggingface", "hub".into()),
        _ => (hub.to_path_buf(), "/root/.cache/huggingface/hub", String::new()),
    }
}

/// Scans this machine.
pub fn local(cfg: &Config) -> Inventory {
    let hub = hf_cache_dir();
    let (export, mount_at, hub_rel) = export_layout(cfg, &hub);
    let mut items: Vec<Item> = models::scan_hf_cache(&hub)
        .into_iter()
        .map(|d| Item {
            revisions: revisions(&d.path),
            id: d.id,
            location: d.location,
            kind: d.kind,
            path: d.path.display().to_string(),
            size: d.size,
            ggufs: d.ggufs,
        })
        .collect();
    items.extend(models::scan_models_dir(&cfg.models_dir()).into_iter().map(|d| Item {
        id: d.id,
        location: d.location,
        kind: d.kind,
        path: d.path.display().to_string(),
        size: d.size,
        revisions: vec![],
        ggufs: vec![],
    }));
    Inventory {
        host: cfg.node_name(),
        hf_cache: hub.display().to_string(),
        export_path: export.display().to_string(),
        mount_at: mount_at.into(),
        hub_rel,
        models_dir: cfg.models_dir().display().to_string(),
        items,
        demo: false,
    }
}

fn revisions(repo_dir: &Path) -> Vec<Revision> {
    let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(rd) = fs::read_dir(repo_dir.join("refs")) {
        for e in rd.flatten() {
            if let Ok(c) = fs::read_to_string(e.path()) {
                refs.entry(c.trim().to_string()).or_default().push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    let mut out: Vec<Revision> = fs::read_dir(repo_dir.join("snapshots"))
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let commit = e.file_name().to_string_lossy().into_owned();
                    Revision { refs: refs.remove(&commit).unwrap_or_default(), commit }
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| (a.refs.is_empty(), &a.commit).cmp(&(b.refs.is_empty(), &b.commit)));
    out
}

/// The snapshot a revision (branch, tag, commit or none) names on disk.
pub fn resolve_snapshot(hub: &Path, repo: &str, revision: Option<&str>) -> Result<(String, PathBuf)> {
    let dir = hub.join(format!("models--{}", repo.replace('/', "--")));
    let snaps = dir.join("snapshots");
    let rev = revision.unwrap_or("main");
    let commit = match fs::read_to_string(dir.join("refs").join(rev)) {
        Ok(c) => c.trim().to_string(),
        Err(_) if snaps.join(rev).is_dir() => rev.to_string(),
        Err(_) if revision.is_none() => {
            let only: Vec<String> = fs::read_dir(&snaps)
                .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            match only.as_slice() {
                [one] => one.clone(),
                [] => bail!("{repo} is not in {}", hub.display()),
                _ => bail!("{repo} has {} snapshots and no refs/main; set model.revision", only.len()),
            }
        }
        Err(_) => bail!("{repo}@{rev} is not in {}", hub.display()),
    };
    let p = snaps.join(&commit);
    if !p.is_dir() {
        bail!("{repo}: snapshot {commit} is missing from {}", hub.display());
    }
    Ok((commit, p))
}

/// Every file under a snapshot (symlinks followed) with its size, sorted:
/// the same list `find -L <dir> -type f -printf '%P %s\n' | sort` prints.
pub fn file_list(dir: &Path) -> Vec<(String, u64)> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, u64)>) {
        if depth > 8 || out.len() > 20_000 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = fs::metadata(&p) else { continue };
            if md.is_dir() {
                walk(root, &p, depth + 1, out);
            } else if md.is_file() {
                out.push((p.strip_prefix(root).unwrap_or(&p).display().to_string(), md.len()));
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, 0, &mut out);
    out.sort();
    out
}

/// Parses `find -printf '%P %s\n'` output.
pub fn parse_file_list(text: &str) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = text
        .lines()
        .filter_map(|l| {
            let (p, s) = l.trim_end().rsplit_once(' ')?;
            Some((p.to_string(), s.parse().ok()?))
        })
        .collect();
    v.sort();
    v
}

/// Differences between the home's list and what the run host sees.
pub fn compare_lists(want: &[(String, u64)], got: &[(String, u64)]) -> Vec<String> {
    let have: BTreeMap<&str, u64> = got.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let mut out = vec![];
    for (p, s) in want {
        match have.get(p.as_str()) {
            None => out.push(format!("missing {p}")),
            Some(g) if g != s => out.push(format!("{p}: {g} bytes, expected {s}")),
            _ => {}
        }
    }
    out
}

/// One host's entry in the merged index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostWeights {
    pub online: bool,
    pub error: Option<String>,
    /// Unix seconds of the inventory (last known when offline).
    pub at: u64,
    pub inventory: Option<Inventory>,
}

/// Every host's weights, keyed by host (`local` for this machine).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub at: u64,
    pub hosts: BTreeMap<String, HostWeights>,
    #[serde(default)]
    pub demo: bool,
}

/// Where a recipe's weights are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Loc {
    /// Host key (`local` or a hosts.toml name).
    pub host: String,
    pub path: String,
    pub size: u64,
    pub kind: String,
}

fn index_path(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("weights-index.json")
}

impl Index {
    pub fn load(paths: &Paths) -> Index {
        fs::read_to_string(index_path(paths)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self, paths: &Paths) {
        let p = index_path(paths);
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        if let Ok(body) = serde_json::to_string(self) {
            let tmp = p.with_extension("json.tmp");
            if fs::write(&tmp, body).is_ok() {
                let _ = fs::rename(&tmp, &p);
            }
        }
    }

    /// Asks every host (in parallel) and this machine; failed hosts keep
    /// their last known inventory, marked offline.
    pub fn refresh(paths: &Paths, cfg: &Config) -> Index {
        if crate::telemetry::demo_mode() {
            return demo(paths, cfg);
        }
        let prev = Index::load(paths);
        let entries = HostsFile::load(paths).map(|f| f.hosts).unwrap_or_default();
        let replies: Vec<Result<Inventory>> = std::thread::scope(|s| {
            let jobs: Vec<_> = entries
                .iter()
                .map(|e| s.spawn(move || crate::remote::Client::new(e).get::<Inventory>("weights")))
                .collect();
            jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| bail!("panicked"))).collect()
        });
        let now = now_secs();
        let mut idx = Index { at: now, hosts: BTreeMap::new(), demo: false };
        idx.hosts.insert(LOCAL.into(), HostWeights { online: true, error: None, at: now, inventory: Some(local(cfg)) });
        for (e, r) in entries.iter().zip(replies) {
            let hw = match r {
                Ok(inv) => HostWeights { online: true, error: None, at: now, inventory: Some(inv) },
                Err(err) => {
                    let old = prev.hosts.get(&e.name).cloned().unwrap_or_default();
                    HostWeights { online: false, error: Some(format!("{err:#}")), ..old }
                }
            };
            idx.hosts.insert(e.name.clone(), hw);
        }
        idx.save(paths);
        idx
    }

    /// The cached index, refreshed when older than `max_age`.
    pub fn fresh(paths: &Paths, cfg: &Config, max_age: Duration) -> Index {
        let idx = Index::load(paths);
        if idx.at > 0
            && now_secs().saturating_sub(idx.at) < max_age.as_secs()
            && idx.demo == crate::telemetry::demo_mode()
        {
            return idx;
        }
        Index::refresh(paths, cfg)
    }

    pub fn inventory(&self, host: &str) -> Option<&Inventory> {
        self.hosts.get(host).and_then(|h| h.inventory.as_ref())
    }

    /// Every host holding `r`'s weights, `local` first.
    pub fn locate(&self, r: &Recipe) -> Vec<Loc> {
        let mut out = vec![];
        for (host, hw) in &self.hosts {
            let Some(inv) = &hw.inventory else { continue };
            if let Some(it) = find(inv, r) {
                let size = it.holds(r).unwrap_or(it.size);
                out.push(Loc { host: host.clone(), path: it.path.clone(), size, kind: it.kind.clone() });
            }
        }
        out.sort_by_key(|l| l.host != LOCAL);
        out
    }

    /// The weights home: `[model] home`, else this machine when it has them, else the first host that does.
    pub fn home(&self, r: &Recipe, node_name: &str) -> Option<Loc> {
        let locs = self.locate(r);
        if let Some(h) = r.model.home.as_deref() {
            let key = if crate::remote::is_local(h, node_name) { LOCAL } else { h };
            return locs.into_iter().find(|l| l.host == key);
        }
        locs.into_iter().next()
    }
}

/// The inventory item that holds a recipe's weights.
pub fn find<'a>(inv: &'a Inventory, r: &Recipe) -> Option<&'a Item> {
    if r.model.path.is_none() {
        if let Some(repo) = &r.model.repo {
            let cached = |i: &&Item| i.location == "hf-cache" && i.id == *repo && i.holds(r).is_some();
            if let Some(it) = inv.items.iter().find(cached) {
                return Some(it);
            }
        }
    }
    let local = r.local_path(Path::new(&inv.models_dir))?;
    let rel = local.strip_prefix(&inv.models_dir).ok()?.display().to_string();
    inv.items.iter().find(|i| {
        i.location == "models-dir"
            && (i.id == rel || rel.starts_with(&format!("{}/", i.id)) || i.id.starts_with(&format!("{rel}/")))
    })
}

/// `LMDECK_DEMO=1`: this machine's real inventory plus demo hosts holding
/// the deck's own recipes (sizes from each recipe's `size_gb`). Labelled demo.
fn demo(paths: &Paths, cfg: &Config) -> Index {
    let now = now_secs();
    let mut idx = Index { at: now, hosts: BTreeMap::new(), demo: true };
    let mut me = local(cfg);
    me.demo = true;
    let cat = crate::catalog::Catalog::load(paths, cfg);
    let recipes: Vec<&Recipe> = crate::deck::listed(&cat).iter().map(|f| &f.recipe).collect();
    let hosts = ["spark2", "spark3"];
    let mut per: BTreeMap<&str, Vec<Item>> = BTreeMap::new();
    for (i, r) in recipes.iter().enumerate() {
        let Some(repo) = &r.model.repo else { continue };
        let size = (r.model.size_gb.unwrap_or(8.0) * 1e9) as u64;
        let item = |host: &str| Item {
            id: repo.clone(),
            location: "hf-cache".into(),
            kind: "hf".into(),
            path: format!("/home/demo/.cache/huggingface/hub/models--{}", repo.replace('/', "--")),
            size,
            revisions: vec![Revision { commit: format!("demo{host}{i:04}"), refs: vec!["main".into()] }],
            ggufs: vec![],
        };
        if i % 3 != 2 {
            per.entry(hosts[i % 2]).or_default().push(item(hosts[i % 2]));
        }
        if i % 4 == 0 {
            per.entry(hosts[(i + 1) % 2]).or_default().push(item(hosts[(i + 1) % 2]));
        }
    }
    idx.hosts.insert(LOCAL.into(), HostWeights { online: true, error: None, at: now, inventory: Some(me) });
    for h in hosts {
        let inv = Inventory {
            host: h.into(),
            hf_cache: "/home/demo/.cache/huggingface/hub".into(),
            export_path: "/home/demo/.cache/huggingface".into(),
            mount_at: "/root/.cache/huggingface".into(),
            hub_rel: "hub".into(),
            models_dir: "/home/demo/models".into(),
            items: per.remove(h).unwrap_or_default(),
            demo: true,
        };
        idx.hosts.insert(h.into(), HostWeights { online: true, error: None, at: now, inventory: Some(inv) });
    }
    idx.hosts.insert(
        "mac".into(),
        HostWeights { online: false, error: Some("timed out".into()), at: now.saturating_sub(185), inventory: None },
    );
    idx.save(paths);
    idx
}

/// `lmdeck weights` rows: model → host, size, path.
pub fn rows(idx: &Index, node_name: &str) -> Vec<(String, String, u64, String, String)> {
    let mut out = vec![];
    for (host, hw) in &idx.hosts {
        let shown = if host == LOCAL { format!("{node_name} (local)") } else { host.clone() };
        for it in hw.inventory.iter().flat_map(|i| &i.items) {
            out.push((it.id.clone(), shown.clone(), it.size, it.path.clone(), it.kind.clone()));
        }
    }
    out.sort_by(|a, b| (a.0.to_lowercase(), &a.1).cmp(&(b.0.to_lowercase(), &b.1)));
    out
}

/// Reads a snapshot's file list from this machine (for `/api/weights/files`).
pub fn local_files(repo: &str, revision: Option<&str>) -> Result<(String, Vec<(String, u64)>)> {
    let (commit, dir) = resolve_snapshot(&hf_cache_dir(), repo, revision).context("weights/files")?;
    Ok((commit, file_list(&dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cache(hub: &Path, repo: &str, commit: &str, files: &[(&str, usize)]) {
        let d = hub.join(format!("models--{}", repo.replace('/', "--")));
        fs::create_dir_all(d.join("refs")).unwrap();
        fs::write(d.join("refs/main"), commit).unwrap();
        let snap = d.join("snapshots").join(commit);
        fs::create_dir_all(d.join("blobs")).unwrap();
        for (i, (f, n)) in files.iter().enumerate() {
            let blob = d.join("blobs").join(format!("b{i}"));
            fs::write(&blob, vec![0u8; *n]).unwrap();
            let p = snap.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&blob, &p).unwrap();
            #[cfg(not(unix))]
            fs::copy(&blob, &p).unwrap();
        }
    }

    #[test]
    fn snapshots_files_and_preflight_compare() {
        let d = tempfile::tempdir().unwrap();
        let hub = d.path().join("hub");
        fake_cache(&hub, "o/n", "abc123", &[("config.json", 10), ("model.safetensors", 100), ("sub/tok.json", 5)]);
        let (c, dir) = resolve_snapshot(&hub, "o/n", None).unwrap();
        assert_eq!(c, "abc123");
        assert_eq!(resolve_snapshot(&hub, "o/n", Some("abc123")).unwrap().0, "abc123");
        assert!(resolve_snapshot(&hub, "o/n", Some("v2")).is_err());
        assert!(resolve_snapshot(&hub, "o/missing", None).is_err());
        let list = file_list(&dir);
        assert_eq!(
            list,
            vec![("config.json".into(), 10), ("model.safetensors".into(), 100), ("sub/tok.json".into(), 5)]
        );
        let got = parse_file_list("sub/tok.json 5\nconfig.json 10\nmodel.safetensors 99\n");
        assert_eq!(compare_lists(&list, &got), vec!["model.safetensors: 99 bytes, expected 100".to_string()]);
        assert_eq!(compare_lists(&list, &got[..1]).len(), 3 - 1 + 1 - 1);
        assert!(compare_lists(&list, &list).is_empty());
        let revs = revisions(&hub.join("models--o--n"));
        assert_eq!(revs, vec![Revision { commit: "abc123".into(), refs: vec!["main".into()] }]);
    }

    #[test]
    fn export_layout_mirrors_the_hf_home() {
        let cfg = Config::default();
        let (p, at, rel) = export_layout(&cfg, Path::new("/home/u/.cache/huggingface/hub"));
        assert_eq!(
            (p, at, rel.as_str()),
            (PathBuf::from("/home/u/.cache/huggingface"), "/root/.cache/huggingface", "hub")
        );
        let (p, at, rel) = export_layout(&cfg, Path::new("/data/hf-models"));
        assert_eq!((p, at, rel.as_str()), (PathBuf::from("/data/hf-models"), "/root/.cache/huggingface/hub", ""));
    }

    #[test]
    fn locate_and_home() {
        let inv = |host: &str, ids: &[&str]| Inventory {
            host: host.into(),
            models_dir: "/m".into(),
            items: ids
                .iter()
                .map(|id| Item {
                    id: id.to_string(),
                    location: "hf-cache".into(),
                    kind: "hf".into(),
                    path: format!("/hf/{id}"),
                    size: 7,
                    revisions: vec![],
                    ggufs: vec![],
                })
                .collect(),
            ..Default::default()
        };
        let mut idx = Index::default();
        for (h, ids) in [("local", vec!["a/b"]), ("spark2", vec!["a/b", "c/d"]), ("spark3", vec!["c/d"])] {
            idx.hosts
                .insert(h.into(), HostWeights { online: true, inventory: Some(inv(h, &ids)), ..Default::default() });
        }
        let mut r = Recipe::parse("[model]\nrepo = \"c/d\"\n[engine]\nname = \"vllm\"\n", "x").unwrap();
        let hosts: Vec<String> = idx.locate(&r).into_iter().map(|l| l.host).collect();
        assert_eq!(hosts, vec!["spark2", "spark3"]);
        assert_eq!(idx.home(&r, "spark1").unwrap().host, "spark2");
        r.model.home = Some("spark3".into());
        assert_eq!(idx.home(&r, "spark1").unwrap().host, "spark3");
        r.model.repo = Some("a/b".into());
        r.model.home = None;
        assert_eq!(idx.home(&r, "spark1").unwrap().host, "local", "this machine first");
        r.model.home = Some("spark1".into());
        assert_eq!(idx.home(&r, "spark1").unwrap().host, "local", "home names this machine");
        let rows = rows(&idx, "spark1");
        assert_eq!(rows[0].0, "a/b");
        assert_eq!(rows[0].1, "spark1 (local)");
    }
}
