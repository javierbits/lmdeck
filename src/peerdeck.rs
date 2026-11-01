//! The cluster-wide deck: every hosts.toml peer's own deck recipes, fetched
//! from its `GET /api/deck` and cached, so `list`, the TUI and the dashboard
//! show (and start) a recipe kept on any machine. A peer recipe's origin is
//! `@<host>`; a name in this machine's deck, or an earlier peer's, wins.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::hosts::HostsFile;
use crate::paths::Paths;
use crate::util::now_secs;

/// How long a fetched peer deck is shown without asking again.
pub const TTL: Duration = Duration::from_secs(120);
/// Per-peer timeout for one fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// One deck recipe as a peer serves it: its name and its file's TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shared {
    pub name: String,
    pub toml: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerDecks {
    /// Unix seconds of the last fetch.
    pub at: u64,
    /// host → its deck, in hosts.toml order of fetching (a host that didn't
    /// answer keeps what it last served).
    pub hosts: BTreeMap<String, Vec<Shared>>,
}

/// The origin of a recipe from `host`'s deck.
pub fn origin(host: &str) -> String {
    format!("@{host}")
}

/// The host a peer origin (`@ember`) names.
pub fn host_of(origin: &str) -> Option<&str> {
    origin.strip_prefix('@').filter(|h| !h.is_empty())
}

fn path(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("peer-decks.json")
}

/// This machine's deck as `/api/deck` serves it.
pub fn local(paths: &Paths) -> Vec<Shared> {
    crate::catalog::deck(paths)
        .0
        .into_iter()
        .filter_map(|f| Some(Shared { name: f.recipe.name, toml: fs::read_to_string(&f.file).ok()? }))
        .collect()
}

impl PeerDecks {
    pub fn load(paths: &Paths) -> PeerDecks {
        fs::read_to_string(path(paths)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self, paths: &Paths) {
        let p = path(paths);
        let _ = fs::create_dir_all(&paths.cache_dir);
        if let Ok(body) = serde_json::to_string(self) {
            let tmp = p.with_extension("json.tmp");
            if fs::write(&tmp, body).is_ok() {
                let _ = fs::rename(&tmp, &p);
            }
        }
    }

    /// Asks every hosts.toml peer for its deck, in parallel, and caches the
    /// answers. A peer that doesn't answer keeps its last deck; one removed
    /// from hosts.toml is dropped.
    pub fn refresh(paths: &Paths) -> PeerDecks {
        let prev = PeerDecks::load(paths);
        let entries = HostsFile::load(paths).map(|f| f.hosts).unwrap_or_default();
        let replies: Vec<(String, Option<Vec<Shared>>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = entries
                .iter()
                .map(|e| {
                    s.spawn(move || {
                        let c = crate::remote::Client::new(e);
                        (e.name.clone(), c.get_timeout::<Vec<Shared>>("deck", FETCH_TIMEOUT).ok())
                    })
                })
                .collect();
            jobs.into_iter().filter_map(|j| j.join().ok()).collect()
        });
        let mut out = PeerDecks { at: now_secs(), hosts: BTreeMap::new() };
        for (host, deck) in replies {
            if let Some(d) = deck.or_else(|| prev.hosts.get(&host).cloned()) {
                out.hosts.insert(host, d);
            }
        }
        out.save(paths);
        out
    }

    /// The cache when younger than `max_age`, else a refresh. Without hosts
    /// (or in hermetic tests) nothing is asked.
    pub fn fresh(paths: &Paths, max_age: Duration) -> PeerDecks {
        let cached = PeerDecks::load(paths);
        if now_secs().saturating_sub(cached.at) < max_age.as_secs() || std::env::var_os("LMDECK_NO_PROBE").is_some() {
            return cached;
        }
        if HostsFile::load(paths).map(|f| f.hosts.is_empty()).unwrap_or(true) {
            return cached;
        }
        PeerDecks::refresh(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_name_their_host() {
        assert_eq!(origin("ember"), "@ember");
        assert_eq!(host_of("@ember"), Some("ember"));
        assert_eq!(host_of("deck"), None);
        assert_eq!(host_of("@"), None);
    }
}
