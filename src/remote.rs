//! Talking to another machine's `lmdeck serve`: the controller side of
//! remote start / stop / switch / logs, uploaded recipes, weights and fabric
//! queries. Every call carries the host's token (see
//! [`HostEntry::resolved_token`]) and mutating calls the `X-Lmdeck: 1` header
//! the server demands.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::time::Duration;

use crate::deck::LogChunk;
use crate::hosts::{HostEntry, HostsFile, LOCAL};
use crate::paths::Paths;

/// Reads (snapshot, status, logs, weights, fabric).
pub const READ_TIMEOUT: Duration = Duration::from_secs(6);
/// Mutations: a start returns once the server process is spawned, but a
/// switch stops others first and the NFS preflight runs a container.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct Client {
    pub name: String,
    pub url: String,
    token: Option<String>,
    token_env: Option<String>,
}

impl Client {
    pub fn new(e: &HostEntry) -> Client {
        Client {
            name: e.name.clone(),
            url: e.url.clone(),
            token: e.resolved_token(),
            token_env: e.token_env.clone().filter(|v| !v.trim().is_empty()),
        }
    }

    /// A host by name from `hosts.toml`.
    pub fn named(paths: &Paths, name: &str) -> Result<Client> {
        let file = HostsFile::load(paths)?;
        let e = file.get(name).ok_or_else(|| {
            let known: Vec<&str> = file.hosts.iter().map(|h| h.name.as_str()).collect();
            anyhow!(
                "no host '{name}' in hosts.toml (known: {}); add it with `lmdeck host add {name} http://…:7979 --token …`",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            )
        })?;
        Ok(Client::new(e))
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(3)).timeout(timeout).build()
    }

    fn auth(&self, req: ureq::Request) -> ureq::Request {
        match &self.token {
            Some(t) => req.set("Authorization", &format!("Bearer {t}")),
            None => req,
        }
    }

    /// `GET /api/<path>` as JSON.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.get_timeout(path, READ_TIMEOUT)
    }

    pub fn get_timeout<T: DeserializeOwned>(&self, path: &str, timeout: Duration) -> Result<T> {
        let url = format!("{}/api/{path}", self.url);
        let resp = self.auth(Self::agent(timeout).get(&url)).call().map_err(|e| self.describe(e))?;
        resp.into_json().with_context(|| format!("{}: bad reply from {url}", self.name))
    }

    /// `POST /api/<path>` with a JSON body; returns the reply (an `{"ok":false,"error":…}`
    /// reply becomes an error carrying the host's message).
    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.post_timeout(path, body, WRITE_TIMEOUT)
    }

    /// Sends [`Client::post`] with its own timeout (a harness push must not
    /// hold a start or stop up for long).
    pub fn post_timeout(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let url = format!("{}/api/{path}", self.url);
        let req = self.auth(Self::agent(timeout).post(&url)).set("X-Lmdeck", "1");
        let resp = req.send_json(body).map_err(|e| self.describe(e))?;
        resp.into_json().with_context(|| format!("{}: bad reply from {url}", self.name))
    }

    /// `start` / `stop` / `switch` / `pull` for a recipe the host knows.
    pub fn action(&self, verb: &str, recipe: &str, force: bool) -> Result<String> {
        let v = self.post(&format!("{verb}/{}", encode(recipe)), &json!({"force": force}))?;
        Ok(v.get("message").and_then(Value::as_str).unwrap_or("ok").to_string())
    }

    /// Uploads a recipe (TOML) and starts it (`POST /api/run`; the host must
    /// have `[serve] accept_recipes = true`). `picked` names the peers this
    /// machine filled in for the host, which takes them as its own Sparks.
    pub fn run(&self, recipe_toml: &str, picked: Option<&[String]>, only: bool, force: bool) -> Result<String> {
        let v = self.post("run", &json!({"recipe": recipe_toml, "peers": picked, "only": only, "force": force}))?;
        Ok(v.get("message").and_then(Value::as_str).unwrap_or("ok").to_string())
    }

    pub fn logs(&self, recipe: &str, n: usize) -> Result<Vec<String>> {
        let v: Value = self.get(&format!("logs/{}?n={n}", encode(recipe)))?;
        Ok(v.get("lines")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
            .unwrap_or_default())
    }

    /// [`Client::logs`] for a follower: the last `n` lines with no `offset`,
    /// else the whole lines written since. Also returns the path the host
    /// reads (a new path means a new log: start over without an offset).
    pub fn logs_since(&self, recipe: &str, offset: Option<u64>, n: usize) -> Result<(String, LogChunk)> {
        let at = offset.map(|o| format!("&offset={o}")).unwrap_or_default();
        let v: Value = self.get(&format!("logs/{}?n={n}{at}", encode(recipe)))?;
        if v.get("offset").is_none() {
            bail!("{}: its lmdeck can't follow a log yet; update lmdeck there", self.name);
        }
        let path = v.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        let chunk = serde_json::from_value(v).with_context(|| format!("{}: bad log reply", self.name))?;
        Ok((path, chunk))
    }

    fn describe(&self, e: ureq::Error) -> anyhow::Error {
        let who = &self.name;
        match e {
            ureq::Error::Status(code, resp) => {
                let msg = resp
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                match code {
                    401 if self.token.is_none() => match &self.token_env {
                        Some(v) => {
                            anyhow!("{who}: the host wants a token; hosts.toml reads it from ${v}, which is not set")
                        }
                        None => anyhow!(
                            "{who}: the host wants a token: set token or token_env in hosts.toml, or {}",
                            crate::hosts::token_env_name(who)
                        ),
                    },
                    401 => anyhow!("{who}: the host rejected the token ({msg})"),
                    _ if msg.is_empty() => anyhow!("{who}: HTTP {code}"),
                    _ => anyhow!("{who}: {msg}"),
                }
            }
            ureq::Error::Transport(t) => {
                anyhow!("{who} ({}): {}", self.url, t.to_string().lines().next().unwrap_or(""))
            }
        }
    }
}

/// Path-segment encoding for recipe references (`origin/name` keeps its `/`).
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Whether `host` names this machine (`local`, empty, or `[node] name`).
pub fn is_local(host: &str, node_name: &str) -> bool {
    host.is_empty() || host == LOCAL || host == node_name
}

/// Refuses an obviously wrong target early, with the list of hosts.
pub fn check_host(paths: &Paths, host: &str, node_name: &str) -> Result<()> {
    if is_local(host, node_name) {
        return Ok(());
    }
    Client::named(paths, host).map(|_| ()).or_else(|e| bail!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_references() {
        assert_eq!(encode("deck/qwen3 4b"), "deck/qwen3%204b");
        assert_eq!(encode("a?b#c"), "a%3Fb%23c");
        assert!(is_local("", "spark-1") && is_local("local", "spark-1") && is_local("spark-1", "spark-1"));
        assert!(!is_local("spark-2", "spark-1"));
    }
}
