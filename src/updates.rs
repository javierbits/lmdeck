//! "Update available" for wrapped recipes: does the `[source]` repo have
//! commits the checkout doesn't?
//!
//! The check is one `git ls-remote` per repo (no fetch), bounded by a
//! timeout, and its result is cached in `<cache>/update-check.json` for
//! `[updates] ttl_hours` (6 h by default). Front-ends only ever read the
//! cache; [`refresh`] (run by `lmdeck outdated`, on a background thread in
//! the TUI and `serve`, or for stale entries by the CLI listings) does the
//! network part. Failures are recorded as `unknown` and never surface as
//! errors.
//!
//! What counts as an update depends on `[source] ref`:
//! - no ref, or a branch: the remote branch head differs from the checkout's
//!   `HEAD` → [`State::Behind`]; `lmdeck pull` moves the checkout.
//! - a tag or commit (a pin; what `lmdeck add` writes): the head of
//!   `[source] branch` (default: the remote's default branch) differs from
//!   the pinned commit → [`State::PinnedNewer`]. A plain `pull` keeps the
//!   pin; `pull --latest` rewrites the deck recipe's `ref` to the new head.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::engine::checkout_dir;
use crate::paths::Paths;
use crate::recipe::{GitSource, Recipe};
use crate::util::now_secs;

/// Failed checks are retried sooner than successful ones are refreshed.
const ERROR_TTL_SECS: u64 = 15 * 60;
/// The CLI listings use a shorter limit for their implicit checks.
pub const QUICK_TIMEOUT: Duration = Duration::from_secs(3);

/// Where a wrapped recipe stands against its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The checkout is at the remote head it follows.
    Current,
    /// The followed branch has moved on: `lmdeck pull` updates.
    Behind,
    /// Pinned to a commit/tag and the tracked branch has newer commits:
    /// `lmdeck pull --latest` moves the pin.
    PinnedNewer,
    /// No checkout yet (it is cloned on first use, at the latest commit).
    NotCloned,
    /// Not checked yet, or the remote could not be reached.
    Unknown,
    /// Checks are turned off.
    Disabled,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Current => "up to date",
            State::Behind => "update available",
            State::PinnedNewer => "newer upstream (pinned)",
            State::NotCloned => "not cloned",
            State::Unknown => "unknown",
            State::Disabled => "checks off",
        }
    }
}

/// One wrapped recipe's update status, as every front-end shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateView {
    pub state: State,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// The ref is a tag or commit.
    pub pinned: bool,
    /// The branch compared against (`main`), when known.
    pub follows: Option<String>,
    /// Checkout `HEAD`.
    pub local: Option<String>,
    /// The followed branch's head at the last check.
    pub remote: Option<String>,
    /// Commits between `local` and `remote`, when the objects are local.
    pub behind: Option<u32>,
    pub checked_at: Option<u64>,
    /// Why the state is `unknown`.
    pub error: Option<String>,
}

impl UpdateView {
    /// An update is waiting (either kind).
    pub fn available(&self) -> bool {
        matches!(self.state, State::Behind | State::PinnedNewer)
    }

    /// The command that takes it.
    pub fn hint(&self, name: &str) -> Option<String> {
        match self.state {
            State::Behind => Some(format!("lmdeck pull {name}")),
            State::PinnedNewer => Some(format!("lmdeck pull --latest {name}")),
            _ => None,
        }
    }

    /// `0c8fd6a → 1a2b3c4 (3 commits)`.
    pub fn delta(&self) -> String {
        let s = |c: &Option<String>| c.as_deref().map(short).unwrap_or_else(|| "?".into());
        let n = match self.behind {
            Some(1) => " (1 commit)".to_string(),
            Some(n) => format!(" ({n} commits)"),
            None => String::new(),
        };
        format!("{} → {}{n}", s(&self.local), s(&self.remote))
    }
}

pub fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Effective settings: config `[updates]` plus `LMDECK_NO_UPDATE_CHECK`.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub enabled: bool,
    pub ttl: Duration,
    pub timeout: Duration,
}

impl Settings {
    pub fn from(cfg: &Config) -> Settings {
        let off = std::env::var("LMDECK_NO_UPDATE_CHECK").is_ok_and(|v| !v.is_empty() && v != "0");
        Settings {
            enabled: cfg.updates.check && !off,
            ttl: Duration::from_secs_f64((cfg.updates.ttl_hours.max(0.0)) * 3600.0),
            timeout: Duration::from_secs(cfg.updates.timeout_secs.clamp(1, 120)),
        }
    }
}

/// One cached check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub repo: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub checked_at: u64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub follows: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    /// `HEAD` when `behind` was counted.
    #[serde(default)]
    pub local: Option<String>,
    #[serde(default)]
    pub behind: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Entry {
    fn matches(&self, src: &GitSource) -> bool {
        self.repo == src.repo && self.git_ref == norm(&src.git_ref) && self.branch == norm(&src.branch)
    }

    fn fresh(&self, s: &Settings) -> bool {
        let ttl = if self.error.is_some() { s.ttl.as_secs().min(ERROR_TTL_SECS) } else { s.ttl.as_secs() };
        now_secs().saturating_sub(self.checked_at) < ttl
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    recipes: BTreeMap<String, Entry>,
}

static CACHE_LOCK: Mutex<()> = Mutex::new(());

pub fn cache_file(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("update-check.json")
}

fn load_cache(paths: &Paths) -> CacheFile {
    fs::read_to_string(cache_file(paths)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Merges `entries` into the cache file (write to a temp file, then rename).
fn store(paths: &Paths, entries: Vec<(String, Entry)>) {
    let _g = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut c = load_cache(paths);
    for (k, v) in entries {
        c.recipes.insert(k, v);
    }
    let file = cache_file(paths);
    let _ = fs::create_dir_all(&paths.cache_dir);
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    if let Ok(text) = serde_json::to_string_pretty(&c) {
        if fs::write(&tmp, text).is_ok() && fs::rename(&tmp, &file).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

fn norm(v: &Option<String>) -> Option<String> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn is_hex_sha(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// The checkout's `HEAD` commit (read from `.git/HEAD` when detached, so the
/// dashboards don't spawn git every refresh).
pub fn local_head(dir: &Path) -> Option<String> {
    let head = fs::read_to_string(dir.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    if head.len() >= 40 && is_hex_sha(head) {
        return Some(head.to_string());
    }
    git_out(Some(dir), &["rev-parse", "HEAD"], Duration::from_secs(5)).ok()
}

/// Runs git with a deadline; kills its process group when the time is up.
fn git_out(dir: Option<&Path>, args: &[&str], timeout: Duration) -> Result<String> {
    let mut c = Command::new("git");
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    let secs = timeout.as_secs().max(1).to_string();
    c.args(["-c", "http.lowSpeedLimit=1", "-c", &format!("http.lowSpeedTime={secs}")])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ADVICE", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        c.env("GIT_SSH_COMMAND", format!("ssh -o BatchMode=yes -o ConnectTimeout={secs}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c.spawn().context("running git (is it installed?)")?;
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        let msg = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        bail!(
            "git {} failed{}",
            args.first().copied().unwrap_or(""),
            if msg.is_empty() { String::new() } else { format!(": {msg}") }
        );
    }
    Ok(stdout.trim().to_string())
}

/// What the remote says, from one `git ls-remote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub pinned: bool,
    pub follows: Option<String>,
    pub head: String,
}

/// Asks the remote for the head `src` follows (no fetch).
pub fn ls_remote(src: &GitSource, timeout: Duration) -> Result<Remote> {
    let r = norm(&src.git_ref);
    let track = norm(&src.branch);
    let mut args: Vec<String> = vec!["ls-remote".into(), "--symref".into(), src.repo.clone(), "HEAD".into()];
    if let Some(r) = &r {
        args.push(format!("refs/heads/{r}"));
        args.push(format!("refs/tags/{r}"));
    }
    if let Some(b) = &track {
        args.push(format!("refs/heads/{b}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git_out(None, &argv, timeout)?;
    let mut refs: BTreeMap<String, String> = BTreeMap::new();
    let mut default_branch = None;
    for line in out.lines() {
        let Some((a, b)) = line.split_once('\t') else { continue };
        if let Some(target) = a.strip_prefix("ref: ") {
            if b == "HEAD" {
                default_branch = target.strip_prefix("refs/heads/").map(str::to_string);
            }
            continue;
        }
        refs.insert(b.to_string(), a.to_string());
    }
    let head_of = |branch: Option<&str>| -> Result<(Option<String>, String)> {
        match branch {
            Some(b) => refs
                .get(&format!("refs/heads/{b}"))
                .map(|h| (Some(b.to_string()), h.clone()))
                .ok_or_else(|| anyhow!("branch '{b}' not found upstream")),
            None => refs
                .get("HEAD")
                .map(|h| (default_branch.clone(), h.clone()))
                .ok_or_else(|| anyhow!("the remote has no HEAD")),
        }
    };
    match &r {
        None => {
            let (follows, head) = head_of(None)?;
            Ok(Remote { pinned: false, follows, head })
        }
        Some(r) if refs.contains_key(&format!("refs/heads/{r}")) => {
            let (follows, head) = head_of(Some(r))?;
            Ok(Remote { pinned: false, follows, head })
        }
        Some(r) if refs.contains_key(&format!("refs/tags/{r}")) || is_hex_sha(r) => {
            let (follows, head) = head_of(track.as_deref())?;
            Ok(Remote { pinned: true, follows, head })
        }
        Some(r) => bail!("ref '{r}' not found upstream (not a branch, tag or commit)"),
    }
}

/// Commits in `local..remote`, if `remote` is already in the checkout's objects.
fn count_behind(dir: &Path, local: &str, remote: &str) -> Option<u32> {
    let t = Duration::from_secs(5);
    git_out(Some(dir), &["cat-file", "-e", &format!("{remote}^{{commit}}")], t).ok()?;
    git_out(Some(dir), &["rev-list", "--count", &format!("{local}..{remote}")], t).ok()?.parse().ok()
}

fn wrapped(r: &Recipe) -> Option<&GitSource> {
    r.source.as_ref()
}

/// Checks one recipe against its remote now.
pub fn check_one(paths: &Paths, r: &Recipe, timeout: Duration, fetch: bool) -> Option<Entry> {
    let src = wrapped(r)?;
    let mut e = Entry {
        repo: src.repo.clone(),
        git_ref: norm(&src.git_ref),
        branch: norm(&src.branch),
        checked_at: now_secs(),
        ..Entry::default()
    };
    match ls_remote(src, timeout) {
        Ok(rem) => {
            let dir = checkout_dir(&paths.checkouts_dir(), &r.name);
            let local = local_head(&dir);
            if let Some(l) = local.as_deref().filter(|l| *l != rem.head) {
                if fetch {
                    let _ = git_out(Some(&dir), &["fetch", "--quiet", "--tags", "origin"], timeout * 6);
                }
                e.behind = count_behind(&dir, l, &rem.head);
            } else if local.is_some() {
                e.behind = Some(0);
            }
            e.local = local;
            e.pinned = rem.pinned;
            e.follows = rem.follows;
            e.remote = Some(rem.head);
        }
        Err(err) => e.error = Some(format!("{err:#}")),
    }
    Some(e)
}

/// The status of `r` from the cache alone (no network). `None` for native recipes.
pub fn view(paths: &Paths, s: &Settings, r: &Recipe) -> Option<UpdateView> {
    let cache = load_cache(paths);
    view_with(paths, s, r, cache.recipes.get(&r.name))
}

/// Every wrapped recipe's status from the cache, keyed by recipe name.
pub fn views<'a>(
    paths: &Paths,
    s: &Settings,
    recipes: impl IntoIterator<Item = &'a Recipe>,
) -> BTreeMap<String, UpdateView> {
    let cache = load_cache(paths);
    recipes
        .into_iter()
        .filter_map(|r| view_with(paths, s, r, cache.recipes.get(&r.name)).map(|v| (r.name.clone(), v)))
        .collect()
}

fn view_with(paths: &Paths, s: &Settings, r: &Recipe, entry: Option<&Entry>) -> Option<UpdateView> {
    let src = wrapped(r)?;
    let dir = checkout_dir(&paths.checkouts_dir(), &r.name);
    let checkout = if dir.join(".git").exists() { local_head(&dir) } else { None };
    // Reason: a recipe placed on another Spark has no checkout here, but a
    // commit pin is what that host cloned and runs; it is still worth comparing.
    let pin = norm(&src.git_ref).filter(|r| is_hex_sha(r));
    let local = checkout.clone().or_else(|| pin.clone());
    let entry = entry.filter(|e| e.matches(src));
    let mut v = UpdateView {
        state: State::Unknown,
        repo: src.repo.clone(),
        git_ref: norm(&src.git_ref),
        pinned: entry.map(|e| e.pinned).unwrap_or_else(|| norm(&src.git_ref).is_some_and(|r| is_hex_sha(&r))),
        follows: entry.and_then(|e| e.follows.clone()),
        local: local.clone(),
        remote: entry.and_then(|e| e.remote.clone()),
        behind: None,
        checked_at: entry.map(|e| e.checked_at),
        error: entry.and_then(|e| e.error.clone()),
    };
    v.state = if !s.enabled {
        State::Disabled
    } else if let (Some(l), Some(e), Some(rem)) = (local.as_deref(), entry, v.remote.as_deref()) {
        if l == rem {
            v.behind = Some(0);
            State::Current
        } else {
            v.behind = e.behind.filter(|_| e.local.as_deref() == Some(l));
            if e.pinned {
                State::PinnedNewer
            } else {
                State::Behind
            }
        }
    } else if checkout.is_none() && pin.is_none() {
        State::NotCloned
    } else {
        State::Unknown
    };
    Some(v)
}

/// Re-checks the cloned wrapped recipes in `recipes` whose cache entry is
/// missing, stale or for another repo/ref (all of them with `force`), in parallel.
/// Never fails: unreachable remotes are recorded as errors (`unknown`).
pub fn refresh<'a>(
    paths: &Paths,
    s: &Settings,
    recipes: impl IntoIterator<Item = &'a Recipe>,
    force: bool,
    timeout: Duration,
    fetch: bool,
) -> usize {
    if !s.enabled {
        return 0;
    }
    let cache = load_cache(paths);
    let due: Vec<&Recipe> = recipes
        .into_iter()
        // Without a checkout there is nothing to compare, except a commit pin:
        // that is what a run placed on another Spark cloned there.
        .filter(|r| {
            checkout_dir(&paths.checkouts_dir(), &r.name).join(".git").exists()
                || wrapped(r).and_then(|s| norm(&s.git_ref)).is_some_and(|r| is_hex_sha(&r))
        })
        .filter(|r| match (wrapped(r), cache.recipes.get(&r.name)) {
            (None, _) => false,
            (Some(_), _) if force => true,
            (Some(src), Some(e)) => !(e.matches(src) && e.fresh(s)),
            (Some(_), None) => true,
        })
        .collect();
    if due.is_empty() {
        return 0;
    }
    let results: Vec<(String, Entry)> = std::thread::scope(|sc| {
        let handles: Vec<_> = due
            .iter()
            .map(|r| sc.spawn(move || check_one(paths, r, timeout, fetch).map(|e| (r.name.clone(), e))))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });
    let n = results.len();
    store(paths, results);
    n
}

/// Forgets `name`'s cached result (after a pull or a ref change) and checks it again.
pub fn after_pull(paths: &Paths, cfg: &Config, r: &Recipe) {
    let s = Settings::from(cfg);
    if s.enabled && r.source.is_some() {
        refresh(paths, &s, [r], true, s.timeout, false);
    }
}

/// Starts a thread that keeps the cache fresh for every wrapped recipe in
/// the catalog (checks only stale entries; wakes every few minutes).
pub fn spawn_background(paths: Paths, cfg: Config) {
    let s = Settings::from(&cfg);
    if !s.enabled {
        return;
    }
    std::thread::spawn(move || loop {
        let cat = crate::catalog::Catalog::load(&paths, &cfg);
        refresh(&paths, &s, cat.entries.iter().map(|f| &f.recipe), false, s.timeout, false);
        std::thread::sleep(Duration::from_secs(300).min(s.ttl.max(Duration::from_secs(30))));
    });
}

/// `pull --latest` for a pinned deck recipe: asks the remote for the tracked
/// branch head and rewrites `ref` in the recipe file. Returns `(old, new)`
/// refs, or `None` when the recipe is not pinned (a plain pull already moves it).
pub fn bump_pin(file: &Path, r: &Recipe, timeout: Duration) -> Result<Option<(String, String)>> {
    let Some(src) = wrapped(r) else { bail!("{} has no [source] to update", r.name) };
    let rem = ls_remote(src, timeout).with_context(|| format!("asking {} for its latest commit", src.repo))?;
    if !rem.pinned {
        return Ok(None);
    }
    let old = norm(&src.git_ref).unwrap_or_default();
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let out = set_source_ref(&text, &rem.head)
        .ok_or_else(|| anyhow!("no `ref = …` line under [source] in {}", file.display()))?;
    let parsed = Recipe::parse(&out, &r.name).context("the updated recipe would not parse")?;
    if norm(&parsed.source.and_then(|s| s.git_ref)).as_deref() != Some(rem.head.as_str()) {
        bail!("could not rewrite the ref in {}", file.display());
    }
    let tmp = file.with_extension("toml.tmp");
    fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, file)?;
    Ok(Some((old, rem.head)))
}

/// Replaces the value of `ref` in the `[source]` table, keeping comments and
/// layout. `None` when there is no such line.
pub fn set_source_ref(text: &str, new_ref: &str) -> Option<String> {
    let mut in_source = false;
    let mut done = false;
    let mut lines: Vec<String> = vec![];
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            in_source = t.trim_end().starts_with("[source]");
        }
        if in_source && !done {
            if let Some(rest) = t.strip_prefix("ref") {
                if rest.trim_start().starts_with('=') {
                    let indent = &line[..line.len() - t.len()];
                    lines.push(format!("{indent}ref = {}", toml::Value::String(new_ref.into())));
                    done = true;
                    continue;
                }
            }
        }
        lines.push(line.to_string());
    }
    if !done {
        return None;
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::tests::{fixture_repo, sh_git};

    fn paths(root: &Path) -> Paths {
        Paths { config_dir: root.join("cfg"), cache_dir: root.join("cache"), state_dir: root.join("state") }
    }

    fn wrapper(name: &str, repo: &Path, git_ref: Option<&str>) -> Recipe {
        let r = git_ref.map(|r| format!("ref = \"{r}\"\n")).unwrap_or_default();
        let text = format!(
            "[source]\nrepo = \"{}\"\n{r}[engine]\nname = \"custom\"\ncommand = [\"./start.sh\"]\n",
            repo.display()
        );
        Recipe::parse(&text, name).unwrap()
    }

    fn on() -> Settings {
        Settings { enabled: true, ttl: Duration::from_secs(3600), timeout: Duration::from_secs(10) }
    }

    fn commit(repo: &Path, body: &str) -> String {
        fs::write(repo.join("start.sh"), body).unwrap();
        sh_git(repo, &["commit", "--quiet", "-am", body]);
        git_out(Some(repo), &["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn branch_recipe_shows_then_clears_an_update() {
        let d = tempfile::tempdir().unwrap();
        let repo = fixture_repo(d.path(), &[("start.sh", "v1\n")]);
        let p = paths(d.path());
        let s = on();
        let r = wrapper("w", &repo, Some("main"));
        assert_eq!(view(&p, &s, &r).unwrap().state, State::NotCloned);
        crate::checkout::ensure(&p, &r).unwrap();
        // Cloned but never checked.
        assert_eq!(view(&p, &s, &r).unwrap().state, State::Unknown);
        assert_eq!(refresh(&p, &s, [&r], false, s.timeout, false), 1);
        assert_eq!(view(&p, &s, &r).unwrap().state, State::Current);
        // Fresh cache: no second network call.
        assert_eq!(refresh(&p, &s, [&r], false, s.timeout, false), 0);

        let tip = commit(&repo, "v2\n");
        commit(&repo, "v3\n");
        let tip = git_out(Some(&repo), &["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap_or(tip);
        refresh(&p, &s, [&r], true, s.timeout, false);
        let v = view(&p, &s, &r).unwrap();
        assert_eq!(v.state, State::Behind, "{v:?}");
        assert!(v.available());
        assert_eq!(v.remote.as_deref(), Some(tip.as_str()));
        assert_eq!(v.follows.as_deref(), Some("main"));
        assert_eq!(v.behind, None, "objects not fetched yet");
        assert_eq!(v.hint("w").as_deref(), Some("lmdeck pull w"));
        // --fetch counts the commits without moving the checkout.
        refresh(&p, &s, [&r], true, s.timeout, true);
        assert_eq!(view(&p, &s, &r).unwrap().behind, Some(2));
        assert_eq!(fs::read_to_string(checkout_dir(&p.checkouts_dir(), "w").join("start.sh")).unwrap(), "v1\n");

        crate::checkout::update(&p, &r).unwrap();
        // The cached remote now equals HEAD: cleared even before a re-check.
        assert_eq!(view(&p, &s, &r).unwrap().state, State::Current);
        // Native recipes have no status; disabled checks say so.
        let native =
            Recipe::parse("[engine]\nname = \"llama.cpp\"\n[model]\nsource = \"local\"\npath = \"x.gguf\"\n", "n")
                .unwrap();
        assert!(view(&p, &s, &native).is_none());
        let off = Settings { enabled: false, ..s };
        assert_eq!(view(&p, &off, &r).unwrap().state, State::Disabled);
        assert_eq!(refresh(&p, &off, [&r], true, s.timeout, false), 0);
    }

    #[test]
    fn pinned_recipe_reports_newer_and_bump_moves_the_pin() {
        let d = tempfile::tempdir().unwrap();
        let repo = fixture_repo(d.path(), &[("start.sh", "v1\n")]);
        let first = git_out(Some(&repo), &["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
        let p = paths(d.path());
        let s = on();
        let r = wrapper("pin", &repo, Some(&first));
        crate::checkout::ensure(&p, &r).unwrap();
        refresh(&p, &s, [&r], false, s.timeout, false);
        assert_eq!(view(&p, &s, &r).unwrap().state, State::Current);
        let tip = commit(&repo, "v2\n");
        refresh(&p, &s, [&r], true, s.timeout, false);
        let v = view(&p, &s, &r).unwrap();
        assert_eq!(v.state, State::PinnedNewer, "{v:?}");
        assert!(v.pinned);
        // A plain update keeps the pin.
        crate::checkout::update(&p, &r).unwrap();
        assert_eq!(view(&p, &s, &r).unwrap().state, State::PinnedNewer);

        let file = d.path().join("pin.toml");
        let text = format!(
            "# mine\n[source]\nrepo = \"{}\"\n# pinned\nref = \"{first}\" # keep?\n\n[engine]\nname = \"custom\"\ncommand = [\"./start.sh\"]\n",
            repo.display()
        );
        fs::write(&file, &text).unwrap();
        let (old, new) = bump_pin(&file, &r, s.timeout).unwrap().unwrap();
        assert_eq!((old.as_str(), new.as_str()), (first.as_str(), tip.as_str()));
        let r2 = Recipe::parse(&fs::read_to_string(&file).unwrap(), "pin").unwrap();
        assert!(fs::read_to_string(&file).unwrap().starts_with("# mine\n[source]\n"));
        assert_eq!(r2.source.as_ref().unwrap().git_ref.as_deref(), Some(tip.as_str()));
        crate::checkout::update(&p, &r2).unwrap();
        after_pull(&p, &Config::default(), &r2);
        assert_eq!(view(&p, &s, &r2).unwrap().state, State::Current);
        // A branch recipe is not "bumped".
        let b = wrapper("pin", &repo, Some("main"));
        assert!(bump_pin(&file, &b, s.timeout).unwrap().is_none());
    }

    /// A recipe placed on another Spark has no checkout on the controller;
    /// its commit pin still tells whether upstream has moved on.
    #[test]
    fn a_pinned_recipe_without_a_checkout_still_checks_upstream() {
        let d = tempfile::tempdir().unwrap();
        let repo = fixture_repo(d.path(), &[("start.sh", "v1\n")]);
        let first = git_out(Some(&repo), &["rev-parse", "HEAD"], Duration::from_secs(5)).unwrap();
        let p = paths(d.path());
        let s = on();
        let r = wrapper("placed", &repo, Some(&first));
        assert_eq!(view(&p, &s, &r).unwrap().state, State::Unknown, "not checked yet");
        assert_eq!(refresh(&p, &s, [&r], false, s.timeout, false), 1);
        let v = view(&p, &s, &r).unwrap();
        assert_eq!(v.state, State::Current, "{v:?}");
        assert_eq!(v.local.as_deref(), Some(first.as_str()), "the pin is what the run host cloned");

        let tip = commit(&repo, "v2\n");
        refresh(&p, &s, [&r], true, s.timeout, false);
        let v = view(&p, &s, &r).unwrap();
        assert_eq!(v.state, State::PinnedNewer, "{v:?}");
        assert_eq!(v.remote.as_deref(), Some(tip.as_str()));
        assert_eq!(v.hint("placed").as_deref(), Some("lmdeck pull --latest placed"));

        // A branch ref has no commit to compare until it is cloned.
        let b = wrapper("branch", &repo, Some("main"));
        assert_eq!(view(&p, &s, &b).unwrap().state, State::NotCloned);
    }

    #[test]
    fn unreachable_remote_is_unknown_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let repo = fixture_repo(d.path(), &[("start.sh", "v1\n")]);
        let p = paths(d.path());
        let s = on();
        let r = wrapper("gone", &repo, Some("main"));
        crate::checkout::ensure(&p, &r).unwrap();
        fs::remove_dir_all(&repo).unwrap();
        assert_eq!(refresh(&p, &s, [&r], false, s.timeout, false), 1);
        let v = view(&p, &s, &r).unwrap();
        assert_eq!(v.state, State::Unknown);
        assert!(v.error.is_some());
        assert!(!v.available());
        // A changed ref invalidates the cached result.
        let other = wrapper("gone", &d.path().join("elsewhere"), Some("main"));
        assert_eq!(view(&p, &s, &other).unwrap().checked_at, None);
        // A ref that is nowhere upstream is an error, not a pin.
        let repo2 = fixture_repo(&d.path().join("b"), &[("start.sh", "x\n")]);
        let bad = GitSource { repo: repo2.display().to_string(), git_ref: Some("nope-branch".into()), branch: None };
        assert!(ls_remote(&bad, s.timeout).is_err());
    }

    #[test]
    fn tags_follow_the_default_or_named_branch() {
        let d = tempfile::tempdir().unwrap();
        let repo = fixture_repo(d.path(), &[("start.sh", "v1\n")]);
        sh_git(&repo, &["tag", "v1"]);
        sh_git(&repo, &["branch", "dev"]);
        let tip = commit(&repo, "v2\n");
        let src = GitSource { repo: repo.display().to_string(), git_ref: Some("v1".into()), branch: None };
        let rem = ls_remote(&src, Duration::from_secs(10)).unwrap();
        assert!(rem.pinned);
        assert_eq!(rem.follows.as_deref(), Some("main"));
        assert_eq!(rem.head, tip);
        let dev = GitSource { branch: Some("dev".into()), ..src };
        let rem = ls_remote(&dev, Duration::from_secs(10)).unwrap();
        assert_eq!(rem.follows.as_deref(), Some("dev"));
        assert_ne!(rem.head, tip);
    }

    #[test]
    fn rewrites_only_the_source_ref() {
        let t = "[model]\nref = \"x\"\n[source]\nrepo = \"r\"\n  ref = \"old\"\n[engine]\nref = 1\n";
        assert_eq!(
            set_source_ref(t, "new").unwrap(),
            "[model]\nref = \"x\"\n[source]\nrepo = \"r\"\n  ref = \"new\"\n[engine]\nref = 1\n"
        );
        assert!(set_source_ref("[source]\nrepo = \"r\"\n", "n").is_none());
        assert!(set_source_ref("[source]\nreference = 1\n", "n").is_none());
    }
}
