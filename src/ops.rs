//! One verb set for "this machine" and "a host's `lmdeck serve`", so
//! placement and multi-node code never branch on where a node is.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use crate::deck::{status_word, Deck};
use crate::hosts::LOCAL;
use crate::recipe::Recipe;
use crate::remote::{encode, is_local, Client};

pub enum Target {
    Local,
    Remote(Client),
}

/// What a node is doing, as far as its machine can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    /// `loaded`, `loading`, `external` or `port-busy`.
    Up(String),
    Stopped,
}

#[derive(Deserialize)]
struct Row {
    name: String,
    status: String,
}

impl Target {
    /// `local`, the `[node] name` or a hosts.toml name. Returns the host key too.
    pub fn of(deck: &Deck, host: &str) -> Result<(String, Target)> {
        if is_local(host, &deck.cfg.node_name()) {
            return Ok((LOCAL.into(), Target::Local));
        }
        Ok((host.to_string(), Target::Remote(Client::named(&deck.paths, host)?)))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Target::Local)
    }

    /// Starts a recipe the target already knows by name.
    pub fn start_named(&self, deck: &Deck, name: &str, only: bool, force: bool) -> Result<String> {
        match self {
            Target::Local => deck.start(name, only, force),
            Target::Remote(c) => c.action(if only { "switch" } else { "start" }, name, force),
        }
    }

    /// Starts `r` as given (uploaded to a remote host).
    pub fn start_recipe(&self, deck: &Deck, r: &Recipe, only: bool, force: bool) -> Result<String> {
        match self {
            Target::Local => deck.start_recipe(r, only, force),
            Target::Remote(c) => c.run(&toml::to_string(r)?, only, force),
        }
    }

    pub fn stop(&self, deck: &Deck, name: &str) -> Result<String> {
        match self {
            Target::Local => deck.stop(name),
            Target::Remote(c) => c.action("stop", name, false),
        }
    }

    /// Whether the target has a recipe called `name` in its catalog.
    pub fn knows(&self, deck: &Deck, name: &str) -> Result<bool> {
        match self {
            Target::Local => Ok(crate::deck::resolve(&deck.catalog(), name).is_ok()),
            Target::Remote(c) => {
                let v: Vec<Value> = c.get("recipes")?;
                Ok(v.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(name)))
            }
        }
    }

    pub fn state(&self, deck: &Deck, name: &str, port: u16) -> Result<NodeState> {
        let word = match self {
            Target::Local => status_word(&crate::runner::status(&deck.paths, name, port)).to_string(),
            Target::Remote(c) => {
                let rows: Vec<Row> = c.get(&format!("status?name={}", encode(name)))?;
                rows.into_iter().find(|r| r.name == name).map(|r| r.status).unwrap_or_else(|| "stopped".into())
            }
        };
        Ok(if word == "stopped" { NodeState::Stopped } else { NodeState::Up(word) })
    }

    /// Whether a TCP port answers on the target's loopback.
    pub fn port_open(&self, port: u16) -> Result<bool> {
        match self {
            Target::Local => Ok(port_open(port)),
            Target::Remote(c) => {
                let v: Value = c.get(&format!("probe?port={port}"))?;
                Ok(v.get("open").and_then(Value::as_bool).unwrap_or(false))
            }
        }
    }

    pub fn logs(&self, deck: &Deck, name: &str, n: usize) -> Vec<String> {
        match self {
            Target::Local => crate::deck::tail_lines(&deck.log_for(name), n),
            Target::Remote(c) => c.logs(name, n).unwrap_or_default(),
        }
    }

    /// Ensures this host exports its weights read-only to `client`.
    pub fn ensure_export(&self, deck: &Deck, client: &str) -> Result<String> {
        match self {
            Target::Local => {
                let inv = crate::weights::local(&deck.cfg);
                crate::placement::ensure_export(&deck.cfg, &inv.export_path, client)
            }
            Target::Remote(c) => msg(c.post("weights/export", &json!({"client": client}))?),
        }
    }

    pub fn ensure_volume(&self, deck: &Deck, volume: &str, server: &str, path: &str) -> Result<String> {
        match self {
            Target::Local => crate::placement::ensure_volume(&deck.docker_bin(), volume, server, path),
            Target::Remote(c) => {
                msg(c.post("weights/volume", &json!({"volume": volume, "server": server, "path": path}))?)
            }
        }
    }

    /// File list of `<volume>/<rel>` as a container sees it.
    pub fn preflight(&self, deck: &Deck, volume: &str, image: &str, rel: &str) -> Result<Vec<(String, u64)>> {
        match self {
            Target::Local => crate::placement::preflight(&deck.docker_bin(), volume, image, rel),
            Target::Remote(c) => {
                let v = c.post("weights/preflight", &json!({"volume": volume, "image": image, "path": rel}))?;
                let files = v.get("files").and_then(Value::as_array).context("preflight: no file list")?;
                Ok(files.iter().filter_map(|f| Some((f.get(0)?.as_str()?.to_string(), f.get(1)?.as_u64()?))).collect())
            }
        }
    }

    /// A snapshot's file list on this host's disk: (commit, files).
    pub fn snapshot_files(&self, repo: &str, revision: Option<&str>) -> Result<(String, Vec<(String, u64)>)> {
        match self {
            Target::Local => crate::weights::local_files(repo, revision),
            Target::Remote(c) => {
                let mut q = format!("weights/files?repo={}", encode(repo));
                if let Some(r) = revision {
                    q.push_str(&format!("&revision={}", encode(r)));
                }
                #[derive(Deserialize)]
                struct Files {
                    commit: String,
                    files: Vec<(String, u64)>,
                }
                let f: Files = c.get(&q)?;
                Ok((f.commit, f.files))
            }
        }
    }
}

fn msg(v: Value) -> Result<String> {
    if v.get("ok").and_then(Value::as_bool) == Some(false) {
        bail!("{}", v.get("error").and_then(Value::as_str).unwrap_or("failed"));
    }
    Ok(v.get("message").and_then(Value::as_str).unwrap_or("ok").to_string())
}

pub fn port_open(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}
