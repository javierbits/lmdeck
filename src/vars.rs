//! Placeholders: the `{name}` slots in recipe commands, args, env, stop and
//! logs that lmdeck fills from what it knows about the machines.
//!
//! A placeholder is `{` + a lowercase name (letters, digits, `_`) + `}`.
//! Anything else in braces is left alone: a shell's `${VAR}`, JSON such as
//! `{"method":"mtp"}`, `{ANY_CAPS}`. A placeholder lmdeck can't fill fails
//! the plan instead of reaching the command as text.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::EngineOverrides;
use crate::hardware::which;

/// The placeholders the engine plan fills for every recipe on its machine.
pub const ENGINE: [&str; 11] =
    ["name", "port", "ctx", "model", "models_dir", "host", "repo", "revision", "fabric", "dir", "container"];

/// What the machine itself has: `{home}`, `{user}`, `{hf_home}` and the
/// TabbyAPI checkout (`{tabbyapi_dir}`), see [`machine`].
pub const MACHINE: [&str; 4] = ["home", "user", "hf_home", "tabbyapi_dir"];

/// The `[engines]` binaries a command can name, by their config keys; each
/// also has a `_dir` form, the directory holding it.
pub const BINS: [&str; 8] = ["llama_server", "vllm", "python", "lmdeploy", "lms", "mlx_server", "tensorfold", "docker"];

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

/// The Hugging Face home lmdeck mounts into containers: `$HF_HOME`, else
/// `~/.cache/huggingface` under `home`.
pub fn hf_home(home: &Path) -> PathBuf {
    let set = std::env::var("HF_HOME").ok().filter(|v| !v.trim().is_empty());
    set.map(|v| crate::paths::expand(&v)).unwrap_or_else(|| home.join(".cache/huggingface"))
}

/// Whether the machine a command runs on fills `name`: the engine plan's own
/// placeholders ([`ENGINE`]) and the machine's ([`machine`]).
pub fn on_machine(name: &str) -> bool {
    ENGINE.contains(&name)
        || MACHINE.contains(&name)
        || BINS.iter().any(|b| name.strip_suffix("_dir").unwrap_or(name) == *b)
}

/// A machine placeholder as this machine (home dir `home`, `[engines]` `o`)
/// has it: `Some(Ok(value))`, `Some(Err(why))` when it lacks it, `None` when
/// `name` isn't one. The binaries resolve as lmdeck's own engine detection
/// does: the `[engines]` path, else `PATH` and the usual install places.
pub fn machine(name: &str, o: &EngineOverrides, home: &Path) -> Option<std::result::Result<String, String>> {
    let (bin, dir) = name.strip_suffix("_dir").filter(|b| BINS.contains(b)).map_or((name, false), |b| (b, true));
    let path = match bin {
        "home" => Some(home.to_path_buf()),
        "user" => return Some(login().ok_or_else(|| "this machine's login name is unknown ($USER is unset)".into())),
        "hf_home" => Some(hf_home(home)),
        "tabbyapi_dir" => crate::engine::find_tabbyapi(o),
        "llama_server" => crate::engine::find_llama_server(o),
        "lms" => crate::engine::find_lms(o),
        "tensorfold" => crate::engine::find_tensorfold(o),
        "vllm" => which(o.vllm.as_deref().unwrap_or("vllm")),
        "python" => which(o.python.as_deref().unwrap_or("python3")),
        "lmdeploy" => which(o.lmdeploy.as_deref().unwrap_or("lmdeploy")),
        "mlx_server" => which(o.mlx_server.as_deref().unwrap_or("mlx_lm.server")),
        "docker" => which(o.docker.as_deref().unwrap_or("docker")),
        _ => return None,
    };
    let path = if dir { path.and_then(|p| p.parent().map(Path::to_path_buf)) } else { path };
    Some(
        path.map(|p| p.display().to_string()).ok_or_else(|| {
            format!("this machine has no {bin}: set [engines] {bin} in config.toml (or put it on PATH)")
        }),
    )
}

/// The machine placeholders `texts` use, as this machine has them; fails on
/// the first one it lacks, saying what to set.
pub fn machine_vars<'a>(
    recipe: &str,
    texts: impl IntoIterator<Item = &'a str>,
    o: &EngineOverrides,
    home: &Path,
) -> Result<Vec<(&'a str, String)>> {
    let mut out = vec![];
    for name in texts.into_iter().flat_map(names) {
        match machine(name, o, home) {
            Some(Ok(v)) => out.push((name, v)),
            Some(Err(why)) => bail!("{recipe}: {{{name}}} can't be filled: {why}"),
            None => {}
        }
    }
    Ok(out)
}

/// Fails on the first placeholder in `texts` (each with where it sits) that
/// `known` doesn't take, naming it and where it is.
pub fn check<'a>(
    recipe: &str,
    texts: impl IntoIterator<Item = (String, &'a str)>,
    known: impl Fn(&str) -> bool,
) -> Result<()> {
    for (at, t) in texts {
        if let Some(x) = names(t).into_iter().find(|n| !known(n)) {
            bail!("{recipe}: {at} uses {{{x}}}, which lmdeck doesn't fill there (the placeholders are in the README's \"Recipe format\")");
        }
    }
    Ok(())
}

/// Fails when a launch plan still has a placeholder in its command, env,
/// stop or logs. Peers are let through: the start planning fills them
/// (`crate::pick::peers`), and a listing doesn't ask the fabric.
pub fn check_plan(recipe: &str, plan: &crate::engine::LaunchPlan) -> Result<()> {
    let cmd = plan.pre.iter().flatten().chain(&plan.argv).map(|s| ("the command".to_string(), s.as_str()));
    let env = plan.env.iter().map(|(k, v)| (format!("env {k}"), v.as_str()));
    let stop = plan.stop.iter().flatten().map(|s| ("stop".to_string(), s.as_str()));
    let logs = plan.logs.iter().flatten().map(|s| ("logs".to_string(), s.as_str()));
    check(recipe, cmd.chain(env).chain(stop).chain(logs), |n| peer_index(n).is_some())
}
