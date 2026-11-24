//! Asks GitHub for the latest lmdeck release and compares it with this build.
//!
//! The answer is cached under the cache dir for a few hours: a star, a slow
//! network is not worth re-querying every time the deck opens. Errors are
//! silenced — being offline must never break the UI.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::paths::Paths;
use crate::util::now_secs;

/// Latest-release answer is reused for this long.
pub const TTL: Duration = Duration::from_secs(6 * 3600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct CacheFile {
    #[serde(default)]
    latest: Option<String>,
    #[serde(default)]
    checked_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Release {
    tag_name: String,
}

fn cache_file(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("self-update.json")
}

fn load_cache(paths: &Paths) -> CacheFile {
    fs::read_to_string(cache_file(paths)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn store_cache(paths: &Paths, latest: &str) {
    let cache = CacheFile { latest: Some(latest.to_string()), checked_at: now_secs() };
    let _ = fs::write(cache_file(paths), serde_json::to_string_pretty(&cache).unwrap_or_default());
}

fn releases_url() -> Result<String> {
    let repo = env!("CARGO_PKG_REPOSITORY");
    let path = repo.strip_prefix("https://github.com/").unwrap_or(repo);
    if !path.contains('/') {
        anyhow::bail!("unknown repository {repo}");
    }
    Ok(format!("https://api.github.com/repos/{path}/releases/latest"))
}

fn fetch_latest() -> Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("lmdeck/", env!("CARGO_PKG_VERSION")))
        .build();
    let body = agent.get(&releases_url()?).call().map(|r| r.into_string().unwrap_or_default()).unwrap_or_default();
    let r: Release = serde_json::from_str(&body)?;
    Ok(r.tag_name.trim_start_matches('v').to_string())
}

/// `major.minor.patch`, ignoring pre-release/build suffixes; `None` on garbage.
fn base(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v').split('-').next().unwrap_or("");
    let mut parts = v.split('.');
    let mut p = |_i: usize| -> Option<u64> { parts.next().and_then(|s| s.parse().ok()) };
    Some((p(0)?, p(1).unwrap_or(0), p(2).unwrap_or(0)))
}

/// Whether the release version `latest` is newer than this build's `current`.
pub fn newer_than(latest: &str, current: &str) -> bool {
    match (base(latest), base(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// The latest release, when it is newer than `current`. Reads the cache when
/// it is fresh (`fetch` only refreshes a stale one); failures come back as
/// `None`. Never blocks more than the fetch timeout.
pub fn latest_if_newer(paths: &Paths, current: &str, fetch: bool) -> Option<String> {
    let cache = load_cache(paths);
    let cache_fresh = cache.checked_at > 0 && now_secs().saturating_sub(cache.checked_at) < TTL.as_secs();
    let latest = if cache_fresh {
        cache.latest?
    } else {
        if !fetch {
            return None;
        }
        let l = fetch_latest().ok()?;
        store_cache(paths, &l);
        l
    };
    if newer_than(&latest, current) {
        Some(latest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cache_is_served_without_fetching() {
        // A fresh cache answers with no network: pretend the release exists.
        let d = tempfile::tempdir().unwrap();
        let paths =
            Paths { config_dir: d.path().join("c"), cache_dir: d.path().join("k"), state_dir: d.path().join("s") };
        fs::create_dir_all(&paths.cache_dir).unwrap();
        fs::write(cache_file(&paths), format!(r#"{{"latest":"99.0.0","checked_at":{}}}"#, now_secs())).unwrap();
        assert_eq!(latest_if_newer(&paths, "0.1.5", true), Some("99.0.0".to_string()));
        // A stale cache is not served without fetching.
        fs::write(cache_file(&paths), r#"{"latest":"99.0.0","checked_at":0}"#).unwrap();
        assert_eq!(latest_if_newer(&paths, "0.1.5", false), None);
    }

    #[test]
    fn base_version_ignores_suffixes() {
        assert_eq!(base("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(base("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(base("1.2"), Some((1, 2, 0)));
        assert_eq!(base("bogus"), None);
    }

    #[test]
    fn newer_than_compares_numerically() {
        assert!(newer_than("0.2.0", "0.1.5"));
        assert!(newer_than("0.1.6", "0.1.5"));
        assert!(!newer_than("0.1.5", "0.1.5"));
        assert!(!newer_than("0.1.4", "0.1.5"));
        assert!(!newer_than("garbage", "0.1.5"));
        assert!(!newer_than("0.1.6", "garbage"));
    }
}
