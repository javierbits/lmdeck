//! Placeholders: the `{name}` slots in recipe commands, args, env, stop and
//! logs that lmdeck fills from what it knows about the machines.
//!
//! A placeholder is `{` + a lowercase name (letters, digits, `_`) + `}`.
//! Anything else in braces is left alone: a shell's `${VAR}`, JSON such as
//! `{"method":"mtp"}`, `{ANY_CAPS}`.

use std::collections::BTreeMap;
use std::time::Duration;

/// The placeholder names in `s`, in order.
pub fn names(s: &str) -> Vec<&str> {
    s.match_indices('{')
        .filter(|(i, _)| !s[..*i].ends_with('$'))
        .filter_map(|(i, _)| s[i + 1..].split_once('}').map(|(name, _)| name))
        .filter(|n| {
            n.starts_with(|c: char| c.is_ascii_lowercase())
                && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect()
}

/// Which peer a placeholder names: `peer_addr` / `peer_host` the first,
/// `peer2_addr` / `peer2_host` the second, and so on (`None` for any other name).
pub fn peer_index(name: &str) -> Option<usize> {
    let (n, field) = name.strip_prefix("peer")?.split_once('_')?;
    if !matches!(field, "addr" | "host") {
        return None;
    }
    match n {
        "" => Some(1),
        n => n.parse().ok().filter(|i| *i >= 2),
    }
}

/// The first placeholder in `texts` naming a peer past the first `count`
/// (with `count` 0: the first peer placeholder at all).
pub fn peer_past<'a>(texts: impl IntoIterator<Item = &'a str>, count: usize) -> Option<&'a str> {
    texts.into_iter().flat_map(names).find(|n| peer_index(n).is_some_and(|i| i > count))
}

/// `{peer_host}` / `{peer_addr}`, `{peer2_host}` / `{peer2_addr}`, … for
/// `peers` (name, address), in order.
pub fn peer_vars(peers: &[(String, String)]) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for (i, (host, addr)) in peers.iter().enumerate() {
        let n = if i == 0 { String::new() } else { (i + 1).to_string() };
        vars.insert(format!("peer{n}_host"), host.clone());
        vars.insert(format!("peer{n}_addr"), addr.clone());
    }
    vars
}

/// This machine's login name: `$USER`, else `$LOGNAME`, else `id -un`.
pub fn login() -> Option<String> {
    ["USER", "LOGNAME"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())).or_else(|| {
        let out = crate::util::run_with_timeout(std::process::Command::new("id").arg("-un"), Duration::from_secs(2));
        out.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    })
}
