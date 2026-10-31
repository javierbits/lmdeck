//! Hub-driven harness sync: one machine, the **hub** (`[harness] hub = true`,
//! the one whose hosts.toml lists every host), keeps every host's agent
//! harnesses in step with every model running anywhere. The other machines
//! run `lmdeck serve` and need no host list.
//!
//! - **Pushing.** After every start or stop the hub performs (the CLI, `--on`,
//!   the TUI, the web dashboard, `lmdeck harness sync`), it works out the
//!   cluster's lanes (its own servers and every host's, as [`crate::harness`]
//!   sees them), syncs its own harnesses with them and pushes them to every
//!   host in hosts.toml (`POST /api/harness/lanes`): in parallel, best effort,
//!   with a short timeout.
//! - **Polling.** Every `lmdeck serve` runs a [`Poller`]. On the hub it looks
//!   at every host every `LMDECK_HARNESS_SYNC_SECS` seconds (default 15, `0`
//!   turns it off) and pushes when the lanes changed, or to the hosts that
//!   haven't taken the last push. That catches models started or stopped on a
//!   host directly.
//! - **Remembering.** Every push starts with a look at every host, whoever
//!   makes it, folded into the hub's record (`<state>/harness-hub.json`,
//!   [`LastPush`]). A host whose snapshot can't be fetched keeps its
//!   last-known lanes through [`KEEP_MISSES`] failed looks in a row
//!   ([`Memory::after`]), counted across the CLI, the TUI, the web dashboard
//!   and the poller, and across restarts; a host taken out of hosts.toml is
//!   forgotten.
//! - **Receiving.** A serve stores the last push (`<state>/harness-lanes.json`,
//!   [`Pushed`]) and syncs with the pushed lanes plus its own running ones the
//!   hub didn't list (matched by name). Every later sync there merges them the
//!   same way, so a local start or stop never wipes the other hosts' models.
//!   The hub keeps its own view: it stores a push but never follows one.
//!
//! `LMDECK_NO_SYNC` turns all of it off; under `LMDECK_DEMO` nothing is pushed.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::config::Config;
use crate::harness::{Lane, Synced};
use crate::hosts::{HostEntry, HostsFile};
use crate::paths::Paths;
use crate::remote::Client;
use crate::util::now_secs;

/// Failed looks in a row a host's last-known lanes survive; the next one drops them.
pub const KEEP_MISSES: u32 = 2;
/// Seconds between the hub serve's looks at every host, unless
/// `LMDECK_HARNESS_SYNC_SECS` says otherwise.
pub const DEFAULT_SYNC_SECS: u64 = 15;
/// The most lanes one push may carry.
pub const MAX_LANES: usize = 256;
/// One push's budget: the host stores the lanes and syncs its harnesses before it answers.
const PUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// The harness-sync lock: one writer of the harness configs and of the hub's
/// state files at a time on this machine, across a serve's threads and a CLI
/// run beside it. Released on drop.
pub struct Exclusive {
    /// `<state>/harness.lock`, held with `flock` (closing it unlocks); `None`
    /// when it can't be opened or locked, and then only this process is held off.
    _file: Option<fs::File>,
    /// The in-process half: a serve's request threads and its poller.
    _threads: MutexGuard<'static, ()>,
}

impl Exclusive {
    /// Waits for the lock: this process's other threads first, then other
    /// processes through `flock` on `<state>/harness.lock`.
    pub fn take(paths: &Paths) -> Exclusive {
        static THREADS: Mutex<()> = Mutex::new(());
        let threads = THREADS.lock().unwrap_or_else(|e| e.into_inner());
        let file = fs::create_dir_all(&paths.state_dir)
            .and_then(|_| {
                fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(paths.state_dir.join("harness.lock"))
            })
            .ok()
            .filter(|f| loop {
                // SAFETY: flock on a descriptor this File owns; closing the File releases it.
                if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break true;
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break false;
                }
            });
        Exclusive { _file: file, _threads: threads }
    }

    /// Writes `v` as JSON over `path` (a tmp file, then a rename); holding the
    /// lock makes the tmp file this writer's alone.
    fn write_json(&self, path: &Path, v: &impl Serialize) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(v)?).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("updating {}", path.display()))
    }
}

/// What the hub last pushed here (`<state>/harness-lanes.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pushed {
    /// The hub's `[node] name`, as it gave it.
    pub from: String,
    /// When the push arrived (unix seconds).
    pub at: u64,
    /// Every model running in the cluster, as the hub sees it.
    pub lanes: Vec<Lane>,
    /// Every lane name any push carried: hermes rewrites or drops these
    /// entries instead of keeping them as hand-added ones.
    #[serde(default)]
    pub names: BTreeSet<String>,
}

impl Pushed {
    /// Returns the file the last push lives in.
    fn path(paths: &Paths) -> PathBuf {
        paths.state_dir.join("harness-lanes.json")
    }

    /// Reads the last push, if one arrived and its file still parses.
    pub fn load(paths: &Paths) -> Option<Pushed> {
        serde_json::from_str(&fs::read_to_string(Self::path(paths)).ok()?).ok()
    }

    /// Merges this machine's own lanes into the pushed ones: every pushed
    /// lane, then each own lane whose name the hub didn't push. Every name a
    /// push ever carried counts as lmdeck's (for hermes).
    pub fn merge(self, (own, mut managed): (Vec<Lane>, BTreeSet<String>)) -> (Vec<Lane>, BTreeSet<String>) {
        let mut lanes = self.lanes;
        let extra: Vec<Lane> = own.into_iter().filter(|o| !lanes.iter().any(|l| l.name == o.name)).collect();
        lanes.extend(extra);
        managed.extend(self.names);
        (lanes, managed)
    }
}

/// The hub's record (`<state>/harness-hub.json`): what it pushed last, to
/// whom, and what every host served at its last looks. Every push the hub
/// makes reads and updates it (the CLI, the TUI, the web dashboard and the
/// serve's poller): it pushes again when the lanes change, or to a host that
/// hasn't taken them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastPush {
    /// The lanes pushed.
    pub lanes: Vec<Lane>,
    /// The hosts that took them.
    pub took: BTreeSet<String>,
    /// What every host served at the last looks, and its failed looks in a row.
    #[serde(default)]
    pub hosts: Memory,
    /// Counts the pushes: each one starts a new round, and a push whose round
    /// is over by the time it lands claims no host ([`LastPush::claim`]).
    #[serde(default)]
    pub round: u64,
}

impl LastPush {
    /// Returns the file the record lives in.
    fn path(paths: &Paths) -> PathBuf {
        paths.state_dir.join("harness-hub.json")
    }

    /// Reads the record, if there is one and it still parses.
    pub fn load(paths: &Paths) -> Option<LastPush> {
        serde_json::from_str(&fs::read_to_string(Self::path(paths)).ok()?).ok()
    }

    /// Saves the record; `held` makes this the only writer.
    pub fn save(&self, paths: &Paths, held: &Exclusive) -> Result<()> {
        held.write_json(&Self::path(paths), self)
    }

    /// Records that the hosts in `took` took the push of round `round`. A
    /// push whose round is over claims nothing and voids every claim, so the
    /// next look pushes to every host again.
    pub fn claim(&mut self, round: u64, took: &[String]) {
        if self.round == round {
            self.took.extend(took.iter().cloned());
            return;
        }
        // Reason: another push overlapped this one, and either can land last on a host,
        // so neither may say what that host holds. Ending the round voids the other
        // push's claims as well, whichever of the two claims first.
        self.took.clear();
        self.round = self.round.wrapping_add(1);
    }
}

impl Lane {
    /// Refuses a lane no harness config should get. grok's config is written
    /// as text, so a name, host or display that could break out of its
    /// quotes or out of the base URL is refused, and so are port 0 and a
    /// context window no config format holds.
    pub fn check(&self) -> Result<(), String> {
        let host_ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':');
        if !crate::recipe::valid_name(&self.name) {
            return Err("its name isn't a recipe name".into());
        }
        if self.host.is_empty() || self.host.len() > 253 || !self.host.chars().all(host_ok) {
            return Err("its host isn't a host name or address".into());
        }
        if self.display.len() > 256 || self.display.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
            return Err("its display name has quotes, backslashes or control characters".into());
        }
        if self.port == 0 {
            return Err("its port is 0".into());
        }
        if self.ctx > u64::from(u32::MAX) {
            return Err("its context window is out of range".into());
        }
        Ok(())
    }
}

/// A push as it arrives at `POST /api/harness/lanes`.
#[derive(Debug, Clone, Deserialize)]
pub struct Push {
    /// The hub's `[node] name`.
    pub from: String,
    /// Every model running in the cluster.
    pub lanes: Vec<Lane>,
}

impl Push {
    /// Reads a push body, refusing it whole when a lane fails
    /// [`Lane::check`], two lanes share a name, or it carries more than
    /// [`MAX_LANES`].
    pub fn parse(body: &Value) -> Result<Push> {
        let push = Push::deserialize(body).context("want {\"from\": \"<hub>\", \"lanes\": [...]}")?;
        if push.from.len() > 128 || push.from.chars().any(char::is_control) {
            bail!("the hub's name is too long or has control characters");
        }
        if push.lanes.len() > MAX_LANES {
            bail!("{} lanes is more than {MAX_LANES}", push.lanes.len());
        }
        let mut names = BTreeSet::new();
        for l in &push.lanes {
            l.check().map_err(|e| anyhow!("lane {:?}: {e}", l.name))?;
            if !names.insert(l.name.as_str()) {
                bail!("lane {:?} comes twice", l.name);
            }
        }
        Ok(push)
    }

    /// Stores the push and syncs this machine's harnesses with it, both under
    /// the sync lock, so a local sync never reads a push half applied.
    /// Returns a one-line summary. Writes nothing when `LMDECK_NO_SYNC` is set.
    pub fn apply(self, paths: &Paths, cfg: &Config) -> Result<String> {
        if std::env::var_os("LMDECK_NO_SYNC").is_some() {
            return Ok("harness sync is off here (LMDECK_NO_SYNC)".into());
        }
        let held = Exclusive::take(paths);
        let mut names = Pushed::load(paths).map(|p| p.names).unwrap_or_default();
        names.extend(self.lanes.iter().map(|l| l.name.clone()));
        let summary = format!("{} lane(s) from {}", self.lanes.len(), self.from);
        // Reason: seen from here every pushed model runs on some host, the hub's own
        // included; grok tags remote lanes with that host.
        let lanes = self.lanes.into_iter().map(|l| Lane { remote: true, ..l }).collect();
        let pushed = Pushed { from: self.from, at: now_secs(), lanes, names };
        held.write_json(&Pushed::path(paths), &pushed)?;
        let (lanes, managed) = crate::harness::wanted(paths, cfg);
        let synced = crate::harness::write(cfg, &lanes, &managed, &held);
        Ok(format!("{summary}{}", Report { synced, ..Default::default() }.note()))
    }
}

/// What the hub knows of one host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    /// The host's lanes as last fetched (none once they are dropped).
    pub lanes: Vec<Lane>,
    /// Failed fetches in a row since the host last answered.
    pub misses: u32,
}

/// The hub's memory, kept in its record ([`LastPush`]): what every host
/// served at the hub's last looks, in hosts.toml order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Memory {
    /// Each host's name and what is known of it.
    pub hosts: Vec<(String, Seen)>,
}

impl Memory {
    /// Returns the memory after one more look, given what each host answered
    /// (`None`: its snapshot couldn't be fetched). A host that answered has
    /// exactly what it said; one that didn't keeps its last-known lanes
    /// through [`KEEP_MISSES`] failed looks in a row and loses them on the
    /// next; a host no longer looked at is forgotten.
    pub fn after(mut self, polled: Vec<(String, Option<Vec<Lane>>)>) -> Memory {
        let hosts = polled
            .into_iter()
            .map(|(name, answer)| {
                let before = self.hosts.iter().position(|(n, _)| *n == name).map(|i| self.hosts.swap_remove(i).1);
                let seen = match answer {
                    Some(lanes) => Seen { lanes, misses: 0 },
                    None => {
                        let before = before.unwrap_or_default();
                        let misses = before.misses.saturating_add(1);
                        // Reason: a blip (a slow or restarting serve) must not take a host's
                        // models out of every harness; a host gone for longer must.
                        let lanes = if misses <= KEEP_MISSES { before.lanes } else { vec![] };
                        Seen { lanes, misses }
                    }
                };
                (name, seen)
            })
            .collect();
        Memory { hosts }
    }

    /// Returns what is known of the host called `name`.
    pub fn get(&self, name: &str) -> Option<&Seen> {
        self.hosts.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// Returns every remembered lane, host by host.
    pub fn lanes(&self) -> Vec<Lane> {
        self.hosts.iter().flat_map(|(_, s)| s.lanes.iter().cloned()).collect()
    }
}

/// What a sync after a start or stop did.
#[derive(Debug, Default)]
pub struct Report {
    /// Each harness on this machine.
    pub synced: Vec<Synced>,
    /// The hosts that took the push (the hub only).
    pub took: Vec<String>,
    /// Why each host that missed the push missed it (each message names its host).
    pub missed: Vec<String>,
}

impl Report {
    /// Returns the summary start and stop messages end with:
    /// ` (synced opencode, omp; pushed to spark2, spark3)`, ` (harness sync
    /// failed: omp: ...)`, or nothing when nothing happened.
    pub fn note(&self) -> String {
        let failed: Vec<String> =
            self.synced.iter().filter_map(|s| s.error.as_ref().map(|e| format!("{}: {e}", s.id))).collect();
        let changed: Vec<&str> = self.synced.iter().filter(|s| s.changed).map(|s| s.id).collect();
        let mut parts = vec![];
        if !failed.is_empty() {
            parts.push(format!("harness sync failed: {}", failed.join("; ")));
        } else if !changed.is_empty() {
            parts.push(format!("synced {}", changed.join(", ")));
        }
        if !self.took.is_empty() {
            parts.push(format!("pushed to {}", self.took.join(", ")));
        }
        parts.extend(self.missed.iter().map(|m| format!("push failed: {m}")));
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join("; "))
        }
    }
}

/// This machine as the hub (or not), for one config.
pub struct Hub<'a> {
    /// This machine's directories.
    paths: &'a Paths,
    /// Its config: `[harness] hub` says whether it is the hub.
    cfg: &'a Config,
}

impl<'a> Hub<'a> {
    /// Returns this machine's hub side for `cfg`.
    pub fn new(paths: &'a Paths, cfg: &'a Config) -> Hub<'a> {
        Hub { paths, cfg }
    }

    /// Returns whether this machine pushes: it is the hub, harness sync is
    /// on, and it isn't showing demo data (which must never reach real hosts).
    pub fn active(&self) -> bool {
        self.cfg.harness.hub && std::env::var_os("LMDECK_NO_SYNC").is_none() && !crate::telemetry::demo_mode()
    }

    /// Syncs this machine's harnesses after a start or stop. The hub first
    /// works out the cluster's lanes, writes its own harnesses with them,
    /// then pushes them to every host in hosts.toml ([`Hub::look`]).
    pub fn sync(&self) -> Report {
        if !self.active() {
            return Report { synced: crate::harness::sync(self.paths, self.cfg), ..Default::default() };
        }
        self.look(true)
    }

    /// Takes one look as the hub: asks every host for its running models,
    /// folds the answers into the record's memory ([`Memory::after`]) and
    /// works out the cluster's lanes from it. When they changed, or `fresh`
    /// is set, it writes this machine's harnesses with them and pushes them
    /// to every host; otherwise only to the hosts that haven't taken them.
    /// The sync lock covers reading and writing the record and the
    /// harnesses, never a fetch or a push.
    fn look(&self, fresh: bool) -> Report {
        let polled = self.fetch();
        let targets = self.targets();
        let held = Exclusive::take(self.paths);
        let mut last = LastPush::load(self.paths).unwrap_or_default();
        last.hosts = std::mem::take(&mut last.hosts).after(polled);
        let (lanes, managed) = crate::harness::lane_sources_with(self.paths, self.cfg, last.hosts.lanes());
        let synced = if fresh || last.lanes != lanes {
            let synced = crate::harness::write(self.cfg, &lanes, &managed, &held);
            (last.lanes, last.took) = (lanes, BTreeSet::new());
            synced
        } else {
            vec![]
        };
        // Reason: a host taken out of hosts.toml leaves the record too, so it gets a push
        // again if it comes back.
        last.took.retain(|h| targets.iter().any(|e| e.name == *h));
        let to: Vec<HostEntry> = targets.into_iter().filter(|e| !last.took.contains(&e.name)).collect();
        if !to.is_empty() {
            last.round = last.round.wrapping_add(1);
        }
        // Reason: saved even when nothing is pushed, or a host's misses would never
        // add up while the lanes it keeps look unchanged.
        last.save(self.paths, &held).unwrap_or_else(|e| Self::debug(&format!("{e:#}")));
        drop(held);
        if to.is_empty() {
            return Report { synced, ..Default::default() };
        }
        let (took, missed) = self.push(&last.lanes, &to);
        let held = Exclusive::take(self.paths);
        if let Some(mut now) = LastPush::load(self.paths) {
            now.claim(last.round, &took);
            now.save(self.paths, &held).unwrap_or_else(|e| Self::debug(&format!("{e:#}")));
        }
        Report { synced, took, missed }
    }

    /// Asks every included host for its running models, in parallel; `None`
    /// for a host that didn't answer.
    fn fetch(&self) -> Vec<(String, Option<Vec<Lane>>)> {
        // Reason: a snapshot right after a start can take longer than the 1.5 s a local sync
        // waits, and a host taken for empty would vanish from every host's harnesses.
        let node = self.cfg.node_name();
        crate::harness::remote_fetch(self.paths, self.cfg.harness.hosts.as_deref(), &node, crate::remote::READ_TIMEOUT)
    }

    /// Returns the hosts a push goes to: every hosts.toml entry but this machine.
    fn targets(&self) -> Vec<HostEntry> {
        let node = self.cfg.node_name();
        HostsFile::load(self.paths)
            .map(|f| f.hosts.into_iter().filter(|e| !crate::remote::is_local(&e.name, &node)).collect())
            .unwrap_or_default()
    }

    /// Pushes `lanes` to each of `to` in parallel, waiting for every answer.
    /// Returns the hosts that took them and why the others didn't.
    fn push(&self, lanes: &[Lane], to: &[HostEntry]) -> (Vec<String>, Vec<String>) {
        // Reason: one lane a host would refuse (a display name grok can't hold) must not
        // cost that host every other lane.
        let lanes: Vec<&Lane> = lanes.iter().filter(|l| l.check().is_ok()).collect();
        let body = json!({"from": self.cfg.node_name(), "lanes": lanes});
        let body = &body;
        let answers: Vec<(String, Result<Value>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = to
                .iter()
                .map(|e| (e, s.spawn(move || Client::new(e).post_timeout("harness/lanes", body, PUSH_TIMEOUT))))
                .collect();
            jobs.into_iter()
                .map(|(e, j)| (e.name.clone(), j.join().unwrap_or_else(|_| Err(anyhow!("{}: push panicked", e.name)))))
                .collect()
        });
        let (mut took, mut missed) = (vec![], vec![]);
        for (name, answer) in answers {
            match answer {
                Ok(_) => took.push(name),
                Err(e) => missed.push(format!("{e:#}")),
            }
        }
        (took, missed)
    }

    /// Prints `msg` on stderr when `LMDECK_DEBUG` is set (the serve's poller
    /// has no one else to tell, and a host that is down would flood the
    /// journal).
    fn debug(msg: &str) {
        if std::env::var_os("LMDECK_DEBUG").is_some() {
            eprintln!("debug: harness hub: {msg}");
        }
    }
}

/// The serve's hub loop: whether this serve has looked yet. What every host
/// served lives in the hub's record ([`LastPush`]), so a restarted serve
/// starts from it.
#[derive(Debug, Default)]
pub struct Poller {
    /// Set once this serve has looked as the hub; until then a look pushes
    /// to every host, whatever they had.
    started: bool,
}

impl Poller {
    /// Starts the poller thread of `lmdeck serve` and returns its interval;
    /// none when `LMDECK_HARNESS_SYNC_SECS` is 0, harness sync is off, or in
    /// demo mode. The thread reads config.toml before every look, so
    /// `lmdeck harness hub on|off` reaches a running serve.
    pub fn spawn(paths: Paths) -> Option<Duration> {
        let every = Self::interval()?;
        if std::env::var_os("LMDECK_NO_SYNC").is_some() || crate::telemetry::demo_mode() {
            return None;
        }
        let mut poller = Poller::default();
        std::thread::Builder::new()
            .name("lmdeck-hub".into())
            .spawn(move || loop {
                if let Ok(cfg) = Config::load(&paths) {
                    poller.tick(&paths, &cfg);
                }
                std::thread::sleep(every);
            })
            .ok()?;
        Some(every)
    }

    /// Returns the time between looks: `LMDECK_HARNESS_SYNC_SECS` seconds
    /// (default [`DEFAULT_SYNC_SECS`]; a value that isn't a number counts as
    /// unset), or `None` when it is 0.
    pub fn interval() -> Option<Duration> {
        let secs = std::env::var("LMDECK_HARNESS_SYNC_SECS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_SYNC_SECS);
        (secs > 0).then(|| Duration::from_secs(secs))
    }

    /// Takes one look as the hub ([`Hub::look`]); the first one this serve
    /// takes pushes to every host. A machine that isn't the hub only forgets
    /// that it looked.
    fn tick(&mut self, paths: &Paths, cfg: &Config) {
        let hub = Hub::new(paths, cfg);
        if !hub.active() {
            *self = Poller::default();
            return;
        }
        let fresh = !std::mem::replace(&mut self.started, true);
        for m in &hub.look(fresh).missed {
            Hub::debug(m);
        }
    }
}
