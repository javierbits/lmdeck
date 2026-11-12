//! `lmdeck serve`: a JSON API plus a single-page dashboard, both embedded in
//! the binary. Bound to 127.0.0.1 by default; anything else needs a token.
//!
//! API (all JSON unless noted):
//! - `GET  /api/health`
//! - `GET  /api/snapshot`              everything below in one document
//! - `GET  /api/recipes` · `/api/status` · `/api/doctor` · `/api/sources`
//! - `POST /api/{start,stop,switch,pull}/{recipe}`  body optional `{"force":true}` (`pull`: `{"latest":true}`)
//! - `GET  /api/logs/{recipe}?n=200[&offset=B]` its tail, or the whole lines after byte B (a follower's poll)
//! - `GET  /api/logs/{recipe}/stream`  Server-Sent Events, one `data:` per line
//! - `GET  /api/telemetry[?history=1&since=<unix ms>]`  this machine, live
//! - `GET  /api/usage`                  this machine's usage statistics (opt-in recording)
//! - `GET  /api/hosts`                 this machine plus every `hosts.toml` entry
//!
//! - `GET  /api/weights` · `/api/weights/files?repo=&revision=`  this machine's weights
//! - `GET  /api/weights/index[?refresh=1]`  every host's inventory (`lmdeck weights`)
//! - `GET  /api/browse?q=&tag=&sort=&fits=1&refresh=1&offline=1`  the recipe index
//! - `GET  /api/outdated?fetch=1`      every wrapped recipe's upstream state
//! - `GET  /api/recipe/{ref}`          one recipe's file and TOML
//! - `GET  /api/harness`               the harness mirror's state
//! - `GET  /api/fabric` · `/api/fabric/reach?addr=`  NICs / RDMA (read-only detection), reachability
//! - `GET  /api/topology`              hosts + links as this controller sees them
//! - `GET  /api/probe?port=`           does a TCP port answer on this machine
//! - `GET  /api/plan/{recipe}?on=<host>`  where weights come from and the NFS steps (dry run)
//! - `POST /api/start/{recipe}` body `{"on":"<host>","allow_slow_link":false}` runs it on another host;
//!   `{"auto":true}` runs it where its weights are (`crate::placement::Pick`), as the dashboard's Start does
//! - `POST /api/run` `{"recipe":"<toml>","only":false,"force":false}`  needs `[serve] accept_recipes`;
//!   `"peers":["<host>",…]` says the controller filled in the recipe's peers (they count as this machine's Sparks)
//! - `POST /api/weights/{export,volume,preflight}`  the NFS steps (see `placement`)
//! - `GET  /api/hosts/{host}/{recipes,status,logs/{recipe}}`, `POST /api/hosts/{host}/{start,stop,switch}/{recipe}`
//!   proxied to that host with the token from hosts.toml
//! - `POST /api/harness/lanes` `{"from":"<hub>","lanes":[…]}`  the hub's push: every model running in the
//!   cluster, stored and synced into this machine's harnesses (see `crate::hub`)
//! - `POST /api/add` · `/api/recipe/{add,remove/{name}}`  install from the index/repo, copy or remove a recipe
//! - `POST /api/source/{add,update,remove/{name}}`       recipe sources
//! - `POST /api/host/{add,sync,remove/{name}}`           hosts.toml, plus `lmdeck host sync`
//! - `POST /api/harness/{enable,disable,sync,hosts,export,hub}`  harness settings
//! - `POST /api/engine/install/{engine}`  managed engine install (llama.cpp)
//! - `POST /api/stop-all`              stop every server this machine started
//! - `POST /api/outdated` `{"fetch":true}`  re-check wrapped recipes now
//!
//! Auth: bound to loopback, no token is needed and the Host header must be
//! localhost (DNS rebinding). Bound anywhere else, a token is generated if
//! none is given and every `/api` route but `/api/health` needs it, reads
//! included (telemetry, logs and weights paths are not public). Whatever the
//! bind, a state-changing request from a non-loopback peer without a
//! configured token is refused (403).
//!
//! `{recipe}` is a name or `origin/name`. With a token, send
//! `Authorization: Bearer <token>` (or `?token=` for EventSource).
//! State-changing calls also need an `X-Lmdeck: 1` header, which a
//! cross-site form or `<img>` cannot add.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::config::Config;
use crate::deck::{tail_lines, Deck, Snapshot};
use crate::hosts::Sampler;
use crate::recipe::valid_name;
use crate::telemetry::{demo_mode, now_ms};

const INDEX_HTML: &str = include_str!("web/index.html");
const FAVICON: &str = include_str!("web/favicon.svg");

pub const DEFAULT_PORT: u16 = 7979;

pub struct WebOptions {
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
}

pub struct State {
    deck: std::sync::RwLock<Arc<Deck>>,
    pub token: Option<String>,
    /// Only accept these Host headers (DNS-rebinding guard) when bound to loopback.
    pub loopback: bool,
    cache: Mutex<Option<(Instant, Arc<Snapshot>)>>,
    json_cache: Mutex<std::collections::HashMap<String, (Instant, Value)>>,
    pub sampler: Sampler,
    /// The saved theme (config.toml `theme`), updated by `POST /api/theme`.
    theme: Mutex<Option<String>>,
}

impl State {
    pub fn new(deck: Deck, token: Option<String>, loopback: bool) -> State {
        let sampler = Sampler::spawn(deck.paths.clone(), true);
        let theme = Mutex::new(deck.cfg.theme.clone());
        State {
            deck: std::sync::RwLock::new(Arc::new(deck)),
            token,
            loopback,
            cache: Mutex::new(None),
            json_cache: Mutex::new(Default::default()),
            sampler,
            theme,
        }
    }

    pub fn deck(&self) -> Arc<Deck> {
        self.deck.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-reads config.toml after a route wrote it, so catalog, sources, hosts
    /// and harness settings take effect without restarting the server.
    fn reload(&self) -> Result<()> {
        let deck = self.deck();
        let cfg = crate::config::Config::load(&deck.paths)?;
        *self.deck.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(Deck::new(deck.paths.clone(), cfg));
        self.invalidate();
        Ok(())
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        let mut g = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, s)) = g.as_ref() {
            if at.elapsed() < Duration::from_millis(900) {
                return s.clone();
            }
        }
        let s = Arc::new(self.deck().snapshot(false));
        *g = Some((Instant::now(), s.clone()));
        s
    }

    /// A JSON reply reused for `ttl` (fabric and topology probes are slow).
    fn cached(&self, key: &str, ttl: Duration, f: impl FnOnce() -> Value) -> Value {
        if let Some((at, v)) = self.json_cache.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
            if at.elapsed() < ttl {
                return v.clone();
            }
        }
        let v = f();
        self.json_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), (Instant::now(), v.clone()));
        v
    }

    fn invalidate(&self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A random 128-bit hex token.
pub fn random_token() -> String {
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok();
    if !ok {
        // Fallback: time + pid mixed through a simple xorshift.
        let mut x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            ^ ((std::process::id() as u64) << 32);
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Binds the server; returns it with the address actually bound (port 0 picks one).
pub fn bind(host: &str, port: u16) -> Result<(Server, SocketAddr)> {
    let addr = format!(
        "{}:{port}",
        if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() }
    );
    let server = Server::http(&addr).map_err(|e| anyhow!("could not listen on {addr}: {e}"))?;
    let bound = server.server_addr().to_ip().ok_or_else(|| anyhow!("not an IP listener"))?;
    Ok((server, bound))
}

/// Serves requests until the process exits (one thread per request).
pub fn serve(server: Server, state: Arc<State>) {
    for req in server.incoming_requests() {
        let st = state.clone();
        std::thread::spawn(move || handle(&st, req));
    }
}

fn header(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn content_type(v: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], v.as_bytes()).expect("static header")
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

fn json_reply(req: Request, code: u16, v: &Value) {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    let _ = req.respond(
        Response::from_string(body)
            .with_status_code(code)
            .with_header(content_type("application/json; charset=utf-8"))
            .with_header(hdr("Cache-Control", "no-store")),
    );
}

fn error(req: Request, code: u16, msg: &str) {
    json_reply(req, code, &json!({"ok": false, "error": msg}));
}

/// `%xx` and `+` decoding for paths and query values.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| percent_decode(v))
}

/// Constant-time-ish string comparison for tokens.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize, Default)]
struct ActionBody {
    #[serde(default)]
    force: bool,
    /// `pull`: also move a pinned wrapped recipe to the upstream head.
    #[serde(default)]
    latest: bool,
    /// `start` / `switch`: run on this host instead (weights stay on their home).
    #[serde(default)]
    on: Option<String>,
    /// `start` / `switch` without `on`: run it where its weights are, as `lmdeck
    /// use` without `--on` does (the dashboard's buttons). Without it a start runs
    /// here: a controller starting a recipe on the host it picked must not move it.
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    allow_slow_link: bool,
}

fn handle(st: &State, mut req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = percent_decode(path);
    let query = query.to_string();

    if st.loopback {
        let host = header(&req, "Host").unwrap_or_default();
        let name = match host.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
            _ => host.clone(),
        };
        if !is_loopback(&name) {
            return error(req, 403, "unexpected Host header (lmdeck only answers on localhost)");
        }
    }

    match (req.method(), path.as_str()) {
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            let _ = req.respond(
                Response::from_string(INDEX_HTML)
                    .with_header(content_type("text/html; charset=utf-8"))
                    .with_header(hdr("Cache-Control", "no-store"))
                    .with_header(hdr(
                        "Content-Security-Policy",
                        "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; \
                         img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'",
                    ))
                    .with_header(hdr("X-Content-Type-Options", "nosniff")),
            );
            return;
        }
        (Method::Get, "/favicon.svg") | (Method::Get, "/favicon.ico") => {
            let _ = req.respond(Response::from_string(FAVICON).with_header(content_type("image/svg+xml")));
            return;
        }
        _ => {}
    }

    let Some(api) = path.strip_prefix("/api/") else {
        return error(req, 404, "not found");
    };
    if api == "health" {
        // The hostname lets the setup wizard on another Spark name this one.
        return json_reply(
            req,
            200,
            &json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "host": crate::util::hostname()}),
        );
    }
    if let Some(tok) = &st.token {
        let given = header(&req, "Authorization")
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
            .or_else(|| query_param(&query, "token"));
        if !given.is_some_and(|g| same(&g, tok)) {
            return error(req, 401, "missing or wrong token");
        }
    }

    let method = req.method().clone();
    match method {
        Method::Get => match api {
            "snapshot" => json_reply(req, 200, &json!(*st.snapshot())),
            "recipes" => json_reply(req, 200, &json!(st.snapshot().recipes)),
            // This machine's own deck, for the other hosts' cluster-wide deck.
            "deck" => json_reply(req, 200, &json!(crate::peerdeck::local(&st.deck().paths))),
            "status" if query_param(&query, "name").is_some() => {
                // One recipe, without building a whole snapshot (controllers poll this while waiting).
                let name = query_param(&query, "name").unwrap_or_default();
                let cat = st.deck().catalog();
                let ports = crate::deck::planned_ports(&cat, &st.deck().paths, &st.deck().cfg);
                let rows: Vec<Value> = cat
                    .entries
                    .iter()
                    .filter(|f| f.recipe.name == name)
                    .take(1)
                    .map(|f| {
                        let port = crate::deck::planned_port(&ports, f);
                        let s = crate::runner::status(&st.deck().paths, &f.recipe.name, port);
                        json!({"name": f.recipe.name, "reference": format!("{}/{}", f.origin, f.recipe.name),
                           "status": crate::deck::status_word(&s), "port": port})
                    })
                    .collect();
                json_reply(req, 200, &json!(rows));
            }
            "status" => {
                let s = st.snapshot();
                let rows: Vec<Value> = s
                    .recipes
                    .iter()
                    .filter(|r| r.listed || r.is_up())
                    .map(|r| {
                        json!({"name": r.name, "reference": r.reference, "status": r.status,
                               "port": r.port, "pulling": r.pulling, "endpoint": r.endpoint})
                    })
                    .collect();
                json_reply(req, 200, &json!(rows))
            }
            "doctor" => {
                let s = st.snapshot();
                let deck = st.deck();
                let paths = json!({
                    "config": deck.paths.config_file(), "recipes": deck.paths.recipes_dir(),
                    "cache": deck.paths.cache_dir, "state": deck.paths.state_dir,
                    "models_dir": deck.cfg.models_dir(), "hf_cache": crate::models::hf_cache_dir(),
                });
                let ups = crate::updates::Settings::from(&deck.cfg);
                json_reply(
                    req,
                    200,
                    &json!({"version": s.version, "schema": crate::schema::CURRENT,
                            "hardware": s.hardware, "inventory": s.inventory,
                            "recipe_errors": s.recipe_errors, "paths": paths,
                            "updates": {"enabled": ups.enabled, "ttl_s": ups.ttl.as_secs(),
                                        "timeout_s": ups.timeout.as_secs()},
                            "stats_recording": deck.cfg.stats.record,
                            "accept_recipes": deck.cfg.serve.accept_recipes}),
                )
            }
            "sources" => json_reply(req, 200, &json!(st.snapshot().sources)),
            "themes" => {
                let saved = st.theme.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let current = crate::theme::resolve(None, saved.as_deref()).map_or(crate::theme::DEFAULT, |p| p.slug);
                json_reply(req, 200, &themes_json(current))
            }
            "telemetry" => {
                st.sampler.wait_first(Duration::from_secs(8));
                let history = query_param(&query, "history").is_some_and(|v| v == "1" || v == "true");
                let since = query_param(&query, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
                json_reply(req, 200, &json!(st.sampler.reply(history, since)))
            }
            "usage" => {
                let usage = crate::usage::view(&st.deck().paths, st.deck().cfg.stats.record);
                json_reply(req, 200, &json!(usage))
            }
            "browse" => browse_reply(st, req, &query),
            "outdated" => outdated_reply(st, req, &query),
            "harness" => harness_reply(st, req),
            "hosts" => {
                st.sampler.wait_first(Duration::from_secs(8));
                json_reply(req, 200, &json!({"at": now_ms(), "demo": demo_mode(), "hosts": st.sampler.hosts()}))
            }
            _ => {
                let Some(req) = get_more(st, req, api, &query) else { return };
                if let Some(rest) = api.strip_prefix("logs/") {
                    if let Some(target) = rest.strip_suffix("/stream") {
                        return stream_logs(st, req, target);
                    }
                    let name = match st.deck().name_of(rest) {
                        Ok(n) => n,
                        Err(e) => return error(req, 404, &format!("{e:#}")),
                    };
                    let n = query_param(&query, "n").and_then(|v| v.parse().ok()).unwrap_or(200usize).min(5000);
                    let path = st.deck().log_for(&name);
                    // `offset` (from the last reply) makes this a follower's poll: only the lines since.
                    let offset = query_param(&query, "offset").and_then(|v| v.parse().ok());
                    let c = crate::deck::log_since(&path, offset, n);
                    let reply =
                        json!({"name": name, "path": path, "lines": c.lines, "offset": c.offset, "reset": c.reset});
                    return json_reply(req, 200, &reply);
                }
                error(req, 404, "not found")
            }
        },
        Method::Post => {
            if header(&req, "X-Lmdeck").as_deref() != Some("1") {
                return error(req, 403, "state-changing requests need the X-Lmdeck: 1 header");
            }
            let peer_local = req.remote_addr().is_none_or(|a| a.ip().is_loopback());
            if st.token.is_none() && !peer_local {
                return error(req, 403, "refusing an unauthenticated change from another machine; serve with a token");
            }
            let mut body = String::new();
            let _ = req.as_reader().take(256 * 1024).read_to_string(&mut body);
            if api == "theme" {
                return set_theme(st, req, &body);
            }
            let Some(req) = post_more(st, req, api, &body) else { return };
            let Some((verb, target)) = api.split_once('/') else { return error(req, 404, "not found") };
            let opts: ActionBody = if body.trim().is_empty() {
                ActionBody::default()
            } else {
                match serde_json::from_str(&body) {
                    Ok(o) => o,
                    Err(e) => return error(req, 400, &format!("bad JSON body: {e}")),
                }
            };
            let node = st.deck().cfg.node_name();
            let elsewhere = opts.on.as_deref().filter(|h| !crate::remote::is_local(h, &node));
            let result = match (verb, elsewhere) {
                ("start" | "switch", Some(host)) => place(st, target, host, verb == "switch", &opts),
                ("start" | "switch", None) if opts.auto && opts.on.is_none() => {
                    crate::placement::start_auto(&st.deck(), target, verb == "switch", opts.force)
                }
                _ => match verb {
                    "start" => st.deck().start(target, false, opts.force),
                    "switch" => st.deck().start(target, true, opts.force),
                    "stop" => st.deck().stop(target),
                    "pull" => st.deck().pull(target, opts.latest),
                    _ => return error(req, 404, "unknown action (start, stop, switch, pull)"),
                },
            };
            st.invalidate();
            match result {
                Ok(msg) => json_reply(req, 200, &json!({"ok": true, "message": msg})),
                Err(e) => error(req, 409, &format!("{e:#}")),
            }
        }
        _ => error(req, 405, "method not allowed"),
    }
}

fn resolve_recipe(st: &State, target: &str) -> Result<crate::recipe::Recipe> {
    let cat = st.deck().catalog();
    Ok(crate::deck::resolve(&cat, target)?.recipe.clone())
}

/// `start --on`: plan and run on another host, weights from their home.
fn place(st: &State, target: &str, host: &str, only: bool, o: &ActionBody) -> Result<String> {
    crate::placement::start_on(&st.deck(), target, host, only, o.force, o.allow_slow_link)
}

fn client_for(st: &State, host: &str) -> Result<crate::remote::Client> {
    crate::remote::Client::named(&st.deck().paths, host)
}

fn flag(query: &str, key: &str) -> bool {
    query_param(query, key).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

fn query_params(query: &str, key: &str) -> Vec<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
        .collect()
}

/// Hosts in hosts.toml, minus this machine (`local` / its node name).
fn configured_hosts(deck: &Deck) -> Vec<String> {
    let node = deck.cfg.node_name();
    crate::hosts::HostsFile::load(&deck.paths)
        .map(|f| f.hosts.iter().filter(|e| !crate::remote::is_local(&e.name, &node)).map(|e| e.name.clone()).collect())
        .unwrap_or_default()
}

/// `lmdeck browse`: the index joined with this machine, filtered and sorted.
fn browse_reply(st: &State, req: Request, query: &str) {
    use crate::browse::{self, Sort};
    let sort = Sort::parse(query_param(query, "sort").as_deref().unwrap_or("name")).unwrap_or_default();
    let deck = st.deck();
    let loaded = if flag(query, "offline") {
        crate::index::cached(&deck.paths, &deck.cfg.index)
    } else {
        crate::index::load(&deck.paths, &deck.cfg.index, flag(query, "refresh"))
    };
    let snap = st.snapshot();
    let sparks = crate::hosts::fleet(&deck.paths, &snap.hardware.platform).online;
    let rows = browse::rows(&loaded.index, Some(&snap), sparks);
    let tags: Vec<String> = query_params(query, "tag")
        .into_iter()
        .flat_map(|v| v.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect::<Vec<_>>())
        .collect();
    let q = query_param(query, "q").unwrap_or_default();
    let fits = flag(query, "fits");
    let shown: Vec<&browse::Row> = browse::filter(&rows, &q, &tags, sort)
        .into_iter()
        .filter(|r| !fits || r.fit != crate::compat::Level::Fail)
        .collect();
    let tag_values: Vec<Value> = browse::tag_values(&loaded.index)
        .into_iter()
        .map(|(key, value, count)| json!({"key": key, "value": value, "count": count}))
        .collect();
    json_reply(
        req,
        200,
        &json!({
            "url": loaded.url, "fetched_ms": loaded.fetched_ms, "from_cache": loaded.from_cache,
            "hint": loaded.hint, "age": loaded.age_text(), "sort": sort.as_str(),
            "total": rows.len(), "shown": shown.len(), "tags": tag_values, "entries": shown,
        }),
    );
}

/// Every wrapped recipe's upstream state, refreshing the cache first.
fn outdated_rows(deck: &Deck, fetch: bool) -> Vec<Value> {
    let cat = deck.catalog();
    let mut seen = std::collections::HashSet::new();
    let entries: Vec<&crate::sources::Found> =
        cat.entries.iter().filter(|f| f.recipe.source.is_some() && seen.insert(f.recipe.name.clone())).collect();
    let s = crate::updates::Settings::from(&deck.cfg);
    crate::updates::refresh(&deck.paths, &s, entries.iter().map(|f| &f.recipe), true, s.timeout, fetch);
    let ups = crate::updates::views(&deck.paths, &s, entries.iter().map(|f| &f.recipe));
    entries
        .iter()
        .filter_map(|f| {
            let u = ups.get(&f.recipe.name)?;
            Some(json!({"name": f.recipe.name, "reference": format!("{}/{}", f.origin, f.recipe.name),
                        "available": u.available(), "hint": u.hint(&f.recipe.name), "update": u}))
        })
        .collect()
}

fn outdated_reply(st: &State, req: Request, query: &str) {
    let deck = st.deck();
    let fetch = flag(query, "fetch");
    let rows = outdated_rows(&deck, fetch);
    let available = rows.iter().filter(|r| r["available"] == true).count();
    st.invalidate();
    json_reply(
        req,
        200,
        &json!({"at": now_ms(), "fetch": fetch, "total": rows.len(), "available": available, "rows": rows}),
    );
}

/// The harness mirror's state: what is enabled, what is installed, where the
/// config lives, and this machine's hub/export settings.
fn harness_reply(st: &State, req: Request) {
    let deck = st.deck();
    let home = crate::paths::home_dir();
    let enabled = crate::harness::enabled(&deck.cfg);
    let rows: Vec<Value> = crate::harness::KNOWN
        .iter()
        .map(|h| {
            json!({
                "id": h.id, "label": h.label, "hint": h.hint,
                "enabled": enabled.contains(&h.id),
                "detected": crate::harness::detected(h.id, &home),
                "config": crate::harness::config_path(h.id, &home).map(|p| p.display().to_string()),
            })
        })
        .collect();
    let pushed = crate::hub::Pushed::load(&deck.paths).map(|p| {
        json!({
            "from": p.from, "at": p.at, "lanes": p.lanes.len(),
        })
    });
    let v = json!({
        "harnesses": rows, "hosts": deck.cfg.harness.hosts, "known_hosts": configured_hosts(&deck),
        "export": deck.cfg.harness.opencode_export, "hub": deck.cfg.harness.hub,
        "asked": deck.cfg.harness.asked, "pushed": pushed,
    });
    json_reply(req, 200, &v);
}

/// Read routes added for remote control, weights and topology. Returns the
/// request when the route isn't one of these.
fn get_more(st: &State, req: Request, api: &str, query: &str) -> Option<Request> {
    let q = |k: &str| query_param(query, k);
    match api {
        "weights" => {
            let cfg = st.deck().cfg.clone();
            json_reply(req, 200, &st.cached("weights", Duration::from_secs(60), || json!(crate::weights::local(&cfg))))
        }
        "weights/index" => {
            let deck = st.deck();
            let idx = if flag(query, "refresh") {
                crate::weights::Index::refresh(&deck.paths, &deck.cfg)
            } else {
                crate::weights::Index::load(&deck.paths)
            };
            let node = deck.cfg.node_name();
            let rows: Vec<Value> = crate::weights::rows(&idx, &node)
                .into_iter()
                .map(
                    |(m, h, size, path, kind)| json!({"model": m, "host": h, "size": size, "path": path, "kind": kind}),
                )
                .collect();
            json_reply(req, 200, &json!({"at": idx.at, "demo": idx.demo, "rows": rows, "hosts": idx.hosts}))
        }
        "weights/files" => {
            let Some(repo) = q("repo") else { return refused(req, 400, "repo= is required") };
            match crate::weights::local_files(&repo, q("revision").as_deref()) {
                Ok((commit, files)) => json_reply(req, 200, &json!({"commit": commit, "files": files})),
                Err(e) => error(req, 404, &format!("{e:#}")),
            }
        }
        "fabric" => {
            json_reply(req, 200, &st.cached("fabric", Duration::from_secs(30), || json!(crate::fabric::detect())))
        }
        "fabric/reach" => {
            let Some(addr) = q("addr").filter(|a| a.parse::<IpAddr>().is_ok()) else {
                return refused(req, 400, "addr= must be an IP address");
            };
            let (ok, rtt) = crate::fabric::reach(&addr);
            json_reply(req, 200, &json!({"ok": ok, "rtt_ms": rtt}))
        }
        "topology" => {
            st.sampler.wait_first(Duration::from_secs(8));
            let verify = q("verify").is_none_or(|v| v != "0");
            let v = st.cached(&format!("topology-{verify}"), Duration::from_secs(20), || {
                json!(crate::topology::build(&st.deck().paths, &st.deck().cfg, &st.sampler.hosts(), verify))
            });
            json_reply(req, 200, &v)
        }
        "probe" => match q("port").and_then(|p| p.parse::<u16>().ok()) {
            Some(p) => json_reply(req, 200, &json!({"port": p, "open": crate::ops::port_open(p)})),
            None => error(req, 400, "port= is required"),
        },
        _ => {
            if let Some(target) = api.strip_prefix("recipe/") {
                let deck = st.deck();
                let cat = deck.catalog();
                let f = match cat.resolve(target) {
                    Ok(f) => f,
                    Err(e) => return refused(req, 404, &format!("{e:#}")),
                };
                let toml = match f.recipe.to_toml() {
                    Ok(t) => t,
                    Err(e) => return refused(req, 500, &format!("{e:#}")),
                };
                let v = json!({"reference": format!("{}/{}", f.origin, f.recipe.name), "name": f.recipe.name,
                               "origin": f.origin, "file": f.file, "toml": toml,
                               "display_name": f.recipe.display(), "description": f.recipe.description});
                json_reply(req, 200, &v);
                return None;
            }
            if let Some(target) = api.strip_prefix("plan/") {
                let r = match resolve_recipe(st, target) {
                    Ok(r) => r,
                    Err(e) => return refused(req, 404, &format!("{e:#}")),
                };
                let on = q("on").unwrap_or_else(|| crate::hosts::LOCAL.to_string());
                let slow = q("allow_slow_link").is_some_and(|v| v == "1" || v == "true");
                let node = st.deck().cfg.node_name();
                let v = match crate::placement::plan(&st.deck().paths, &st.deck().cfg, &r, &on, slow) {
                    Ok(p) => json!({"ok": true, "plan": p, "lines": p.describe(&node)}),
                    Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
                };
                json_reply(req, 200, &v);
                return None;
            }
            if let Some(rest) = api.strip_prefix("hosts/") {
                let Some((host, what)) = rest.split_once('/') else { return Some(req) };
                let c = match client_for(st, host) {
                    Ok(c) => c,
                    Err(e) => return refused(req, 404, &format!("{e:#}")),
                };
                let path = match what {
                    "recipes" | "status" | "snapshot" => what.to_string(),
                    w if w.starts_with("logs/") => {
                        let at = q("offset").map(|o| format!("&offset={o}")).unwrap_or_default();
                        format!("{}?n={}{at}", crate::remote::encode(w), q("n").unwrap_or_else(|| "200".to_string()))
                    }
                    _ => return refused(req, 404, "not found"),
                };
                match c.get::<Value>(&path) {
                    Ok(v) => json_reply(req, 200, &v),
                    Err(e) => error(req, 502, &format!("{e:#}")),
                }
                return None;
            }
            return Some(req);
        }
    }
    None
}

/// `error` for the `Option<Request>` routers: the request is answered.
fn refused(req: Request, code: u16, msg: &str) -> Option<Request> {
    error(req, code, msg);
    None
}

/// A success for the `Option<Request>` routers: the request is answered.
fn accepted(req: Request, msg: &str) -> Option<Request> {
    json_reply(req, 200, &json!({"ok": true, "message": msg}));
    None
}

/// State-changing routes for remote control and weights. Returns the
/// request when the route isn't one of these.
fn post_more(st: &State, req: Request, api: &str, body: &str) -> Option<Request> {
    let parse = |b: &str| -> Result<Value, String> {
        if b.trim().is_empty() {
            Ok(json!({}))
        } else {
            serde_json::from_str(b).map_err(|e| format!("bad JSON body: {e}"))
        }
    };
    let v = match parse(body) {
        Ok(v) => v,
        Err(e) => return refused(req, 400, &e),
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let accept = st.deck().cfg.serve.accept_recipes;
    let refuse =
        "this host does not accept work from a controller; set [serve] accept_recipes = true in its config.toml";
    let result: Result<Value> = match api {
        "add" => (|| {
            let deck = st.deck();
            let repo = s("repo").ok_or_else(|| anyhow!("repo is required"))?;
            let (mut lines, mut warns) = (vec![], vec![]);
            let added = {
                let mut say = |l: &str| lines.push(crate::ui::strip_ansi(l));
                let mut warn = |l: &str| warns.push(crate::ui::strip_ansi(l));
                let mut reply = crate::install::Reply { say: &mut say, warn: &mut warn };
                crate::install::add(&deck.paths, &deck.cfg, &repo, s("name"), s("ref"), b("force"), &mut reply)?
            };
            st.reload()?;
            Ok(json!({"ok": true, "name": added.name, "path": added.path, "lines": lines, "warnings": warns,
                      "message": format!("added {} to the deck", added.name)}))
        })(),
        "recipe/add" => (|| {
            let deck = st.deck();
            let reference = s("reference").ok_or_else(|| anyhow!("reference is required"))?;
            let added = crate::install::add_recipe(&deck.paths, &deck.cfg, &reference, s("name"), b("force"))?;
            st.reload()?;
            Ok(json!({"ok": true, "name": added.name, "path": added.path,
                      "message": format!("added {} to the deck", added.name)}))
        })(),
        "source/add" => (|| {
            let deck = st.deck();
            let spec = s("spec").ok_or_else(|| anyhow!("spec is required"))?;
            let parsed = crate::sources::SourceSpec::parse(&spec)?;
            let name = s("name").unwrap_or_else(|| parsed.default_name());
            if !valid_name(&name) || name == crate::catalog::DECK || name == crate::catalog::BUILTIN {
                return Err(anyhow!("invalid source name '{name}'"));
            }
            if deck.cfg.sources.iter().any(|x| x.name == name) {
                return Err(anyhow!("a source named '{name}' already exists"));
            }
            let dir = deck.paths.sources_cache_dir().join(&name);
            let mut found = 0;
            if v.get("fetch").and_then(Value::as_bool).unwrap_or(true) {
                let idx = crate::sources::sync(&parsed, &dir)?;
                found = crate::sources::load_dir(&name, &dir).0.len();
                if idx.files.is_empty() {
                    found = 0;
                }
            }
            let mut cfg = deck.cfg.clone();
            cfg.sources.push(crate::config::SourceEntry { name: name.clone(), spec: parsed.canonical() });
            cfg.save(&deck.paths)?;
            st.reload()?;
            Ok(json!({"ok": true, "name": name, "spec": parsed.canonical(), "recipes": found,
                      "message": format!("added source {name} ({found} recipes)")}))
        })(),
        "source/update" => (|| {
            let deck = st.deck();
            let targets: Vec<crate::config::SourceEntry> = match s("name") {
                Some(n) => vec![deck
                    .cfg
                    .sources
                    .iter()
                    .find(|x| x.name == n)
                    .cloned()
                    .ok_or_else(|| anyhow!("no source named '{n}'"))?],
                None => deck.cfg.sources.clone(),
            };
            let mut results = vec![];
            let mut errors = vec![];
            for src in targets {
                let dir = deck.paths.sources_cache_dir().join(&src.name);
                match crate::sources::SourceSpec::parse(&src.spec).and_then(|p| crate::sources::sync(&p, &dir)) {
                    Ok(idx) => {
                        let n = crate::sources::load_dir(&src.name, &dir).0.len();
                        results.push(json!({"name": src.name, "recipes": n, "files": idx.files.len()}));
                    }
                    Err(e) => errors.push(format!("{}: {e:#}", src.name)),
                }
            }
            st.invalidate();
            Ok(json!({"ok": errors.is_empty(), "results": results, "errors": errors,
                      "message": match errors.len() { 0 => format!("{} source(s) updated", results.len()),
                          n => format!("{n} source(s) failed") }}))
        })(),
        "host/add" => (|| {
            let deck = st.deck();
            let name = s("name").ok_or_else(|| anyhow!("name is required"))?;
            let url = s("url").ok_or_else(|| anyhow!("url is required"))?;
            let mut file = crate::hosts::HostsFile::load(&deck.paths)?;
            let entry = file.add(&name, &url, s("token"))?;
            if let Some(slot) = file.hosts.iter_mut().find(|h| h.name == entry.name) {
                if let Some(env) = s("token_env") {
                    slot.token_env = Some(env);
                }
                if let Some(addr) = s("link_addr") {
                    if addr.parse::<std::net::IpAddr>().is_err() {
                        return Err(anyhow!("link_addr must be an IP address"));
                    }
                    slot.link_addr = Some(addr);
                }
            }
            let entry = file.hosts.iter().find(|h| h.name == name).cloned().unwrap_or(entry);
            file.save(&deck.paths)?;
            let mut reachable = None;
            if v.get("check").and_then(Value::as_bool).unwrap_or(true) {
                reachable = Some(match crate::hosts::fetch(&entry, u64::MAX, crate::hosts::FETCH_TIMEOUT) {
                    Ok((r, ms)) => {
                        let t = r.telemetry.unwrap_or_default();
                        json!({"ok": true, "ms": ms, "hostname": t.hostname, "platform": t.platform})
                    }
                    Err(e) => json!({"ok": false, "error": e}),
                });
            }
            Ok(json!({"ok": true, "name": entry.name, "url": entry.url, "reachable": reachable,
                      "message": format!("added host {}", entry.name)}))
        })(),
        "host/sync" => (|| {
            let deck = st.deck();
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            let own = crate::mesh::own_entry(&deck.paths, &home, &deck.cfg.node_name());
            let rep = crate::mesh::sync(&deck.paths, &mut crate::ssh::System::default(), own, b("dry_run"))?;
            Ok(json!({"ok": rep.failed.is_empty(), "lines": rep.lines, "failed": rep.failed,
                      "message": if rep.failed.is_empty() { format!("{} host(s) updated", rep.lines.len()) }
                          else { format!("{} host(s) not updated", rep.failed.len()) }}))
        })(),
        "harness/sync" => {
            let deck = st.deck();
            let rep = crate::hub::Hub::new(&deck.paths, &deck.cfg).sync();
            let synced: Vec<Value> = rep
                .synced
                .iter()
                .map(|x| json!({"id": x.id, "changed": x.changed, "installed": x.installed, "error": x.error}))
                .collect();
            Ok(json!({"ok": !rep.synced.iter().any(|x| x.error.is_some()), "synced": synced,
                      "took": rep.took, "missed": rep.missed, "message": rep.note()}))
        }
        "harness/enable" | "harness/disable" => (|| {
            let deck = st.deck();
            let name = s("name").ok_or_else(|| anyhow!("name is required"))?;
            let h = crate::harness::known(&name).ok_or_else(|| anyhow!("unknown harness '{name}'"))?;
            let mut hc = deck.cfg.harness.clone();
            hc.asked = true;
            if api == "harness/enable" {
                if !hc.sync.iter().any(|x| x == h.id) {
                    hc.sync.push(h.id.to_string());
                }
            } else {
                hc.sync.retain(|x| x != h.id);
            }
            Config::save_harness(&deck.paths, &hc)?;
            st.reload()?;
            Ok(json!({"ok": true, "id": h.id, "enabled": api == "harness/enable",
                      "message": format!("{} {}", h.label, if api == "harness/enable" { "on" } else { "off" })}))
        })(),
        "harness/hosts" => (|| {
            let deck = st.deck();
            let known = configured_hosts(&deck);
            let set: Option<Vec<String>> = match v.get("hosts") {
                None | Some(Value::Null) => None,
                Some(Value::Array(a)) => {
                    let list: Vec<String> = a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                    let unknown: Vec<&String> = list.iter().filter(|h| !known.contains(h)).collect();
                    if !unknown.is_empty() {
                        return Err(anyhow!(
                            "no host {} in hosts.toml (known: {})",
                            unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                            known.join(", ")
                        ));
                    }
                    Some(list)
                }
                Some(_) => return Err(anyhow!("hosts must be an array of names or null")),
            };
            let mut hc = deck.cfg.harness.clone();
            hc.hosts = set.clone();
            hc.asked = true;
            Config::save_harness(&deck.paths, &hc)?;
            st.reload()?;
            let what = match &set {
                None => "all configured hosts".to_string(),
                Some(h) if h.is_empty() => "none (local models only)".to_string(),
                Some(h) => h.join(", "),
            };
            Ok(json!({"ok": true, "hosts": set, "message": format!("sync hosts: {what}")}))
        })(),
        "harness/export" | "harness/hub" => (|| {
            let deck = st.deck();
            let on = b("on");
            let mut hc = deck.cfg.harness.clone();
            if api == "harness/export" {
                hc.opencode_export = on;
            } else {
                hc.hub = on;
            }
            Config::save_harness(&deck.paths, &hc)?;
            st.reload()?;
            let what = if api == "harness/export" { "opencode-export" } else { "harness hub" };
            Ok(json!({"ok": true, "on": on, "message": format!("{what} {}", if on { "on" } else { "off" })}))
        })(),
        "outdated" => {
            let deck = st.deck();
            let rows = outdated_rows(&deck, b("fetch"));
            Ok(json!({"ok": true, "fetch": b("fetch"), "rows": rows,
                      "message": format!("checked {} wrapped recipe(s)", rows.len())}))
        }
        "stop-all" => {
            let deck = st.deck();
            let mut stopped = vec![];
            for r in crate::runner::RunRecord::all(&deck.paths) {
                if deck.stop(&r.name).is_ok() {
                    stopped.push(r.name);
                }
            }
            Ok(json!({"ok": true, "stopped": stopped,
                      "message": match stopped.len() {
                          0 => "nothing to stop".to_string(),
                          1 => format!("stopped {}", stopped[0]),
                          n => format!("stopped {n} servers") }}))
        }
        "run" => {
            if !accept {
                return refused(req, 403, refuse);
            }
            (|| {
                let text = s("recipe").ok_or_else(|| anyhow!("recipe (TOML) is required"))?;
                let r = crate::recipe::Recipe::parse(&text, "remote")?;
                if !r.nodes.is_empty() {
                    return Err(anyhow!("upload each node's recipe, not the multi-node one"));
                }
                // Reason: a controller that filled in the recipe's peers names them, and they
                // stand in for this machine's own hosts.toml, which it need not have.
                let picked: Option<Vec<String>> = serde_json::from_value(v.get("peers").cloned().unwrap_or_default())?;
                crate::catalog::save_remote(&st.deck().paths, &r)?;
                let m = st.deck().start_recipe(&r, picked.as_deref(), b("only"), b("force"))?;
                Ok(json!({"ok": true, "message": m}))
            })()
        }
        "weights/export" => (|| {
            let client = s("client").ok_or_else(|| anyhow!("client is required"))?;
            let inv = crate::weights::local(&st.deck().cfg);
            let m = crate::placement::ensure_export(&st.deck().cfg, &inv.export_path, &client)?;
            Ok(json!({"ok": true, "message": m, "path": inv.export_path}))
        })(),
        "weights/volume" => {
            if !accept {
                return refused(req, 403, refuse);
            }
            (|| {
                let need = |k: &str| s(k).ok_or_else(|| anyhow!("{k} is required"));
                let (vol, server, path) = (need("volume")?, need("server")?, need("path")?);
                if server.parse::<IpAddr>().is_err() {
                    return Err(anyhow!("server must be an IP address"));
                }
                let m = crate::placement::ensure_volume(&st.deck().docker_bin(), &vol, &server, &path)?;
                Ok(json!({"ok": true, "message": m}))
            })()
        }
        "weights/preflight" => {
            if !accept {
                return refused(req, 403, refuse);
            }
            (|| {
                let need = |k: &str| s(k).ok_or_else(|| anyhow!("{k} is required"));
                let files = crate::placement::preflight(
                    &st.deck().docker_bin(),
                    &need("volume")?,
                    &need("image")?,
                    &need("path")?,
                )?;
                Ok(json!({"ok": true, "files": files}))
            })()
        }
        "harness/lanes" => match crate::hub::Push::parse(&v) {
            Ok(push) => push.apply(&st.deck().paths, &st.deck().cfg).map(|m| json!({"ok": true, "message": m})),
            Err(e) => return refused(req, 400, &format!("{e:#}")),
        },
        _ => {
            if let Some(name) = api.strip_prefix("recipe/remove/") {
                let deck = st.deck();
                let name = percent_decode(name);
                return match (|| -> Result<String> {
                    if !crate::catalog::remove_from_deck(&deck.paths, &name)? {
                        anyhow::bail!("no deck recipe named '{name}'");
                    }
                    st.reload()?;
                    Ok(format!("removed {name}"))
                })() {
                    Ok(msg) => accepted(req, &msg),
                    Err(e) => {
                        error(req, 409, &format!("{e:#}"));
                        None
                    }
                };
            }
            if let Some(name) = api.strip_prefix("source/remove/") {
                let deck = st.deck();
                let name = percent_decode(name);
                return match (|| -> Result<String> {
                    let mut cfg = deck.cfg.clone();
                    let before = cfg.sources.len();
                    cfg.sources.retain(|s| s.name != name);
                    if cfg.sources.len() == before {
                        anyhow::bail!("no source named '{name}'");
                    }
                    cfg.save(&deck.paths)?;
                    let _ = std::fs::remove_dir_all(deck.paths.sources_cache_dir().join(&name));
                    st.reload()?;
                    Ok(format!("removed source {name}"))
                })() {
                    Ok(msg) => accepted(req, &msg),
                    Err(e) => {
                        error(req, 409, &format!("{e:#}"));
                        None
                    }
                };
            }
            if let Some(name) = api.strip_prefix("host/remove/") {
                let deck = st.deck();
                let name = percent_decode(name);
                return match (|| -> Result<String> {
                    let mut file = crate::hosts::HostsFile::load(&deck.paths)?;
                    if !file.remove(&name) {
                        anyhow::bail!("no host named '{name}'");
                    }
                    file.save(&deck.paths)?;
                    Ok(format!("removed host {name}"))
                })() {
                    Ok(msg) => accepted(req, &msg),
                    Err(e) => {
                        error(req, 409, &format!("{e:#}"));
                        None
                    }
                };
            }
            if let Some(engine) = api.strip_prefix("engine/install/") {
                let deck = st.deck();
                return match (|| -> Result<Value> {
                    let engine: crate::recipe::Engine = engine.parse()?;
                    let mut lines = vec![];
                    let bin = match engine {
                        crate::recipe::Engine::LlamaCpp => {
                            let mut say = |l: &str| lines.push(crate::ui::strip_ansi(l));
                            crate::engine_install::install_llama_cpp(&deck.paths, b("force"), &mut say)?
                        }
                        other => anyhow::bail!("{other} has no managed installer"),
                    };
                    let v = crate::engine_install::version(&bin);
                    // A fresh Deck re-detects the inventory (the old one caches it for a minute).
                    st.reload()?;
                    Ok(json!({"ok": true, "engine": "llama.cpp", "binary": bin,
                        "version": v, "lines": lines,
                        "message": format!("installed llama.cpp {}", v.clone().unwrap_or_default())}))
                })() {
                    Ok(v) => {
                        json_reply(req, 200, &v);
                        None
                    }
                    Err(e) => {
                        error(req, 409, &format!("{e:#}"));
                        None
                    }
                };
            }
            let Some(rest) = api.strip_prefix("hosts/") else { return Some(req) };
            let mut it = rest.splitn(3, '/');
            let (Some(host), Some(verb), Some(target)) = (it.next(), it.next(), it.next()) else {
                return refused(req, 404, "want /api/hosts/<host>/<start|stop|switch>/<recipe>");
            };
            if !matches!(verb, "start" | "stop" | "switch") {
                return refused(req, 404, "unknown action (start, stop, switch)");
            }
            // The model reaches this machine's harnesses now (on the hub, every host's).
            client_for(st, host).and_then(|c| c.action(verb, target, b("force"))).map(|m| {
                json!({"ok": true, "message": format!("{m}{}", crate::harness::sync_note(&st.deck().paths, &st.deck().cfg))})
            })
        }
    };
    st.invalidate();
    match result {
        Ok(v) => json_reply(req, 200, &v),
        Err(e) => error(req, 409, &format!("{e:#}")),
    }
    None
}

/// Every theme for the dashboard's picker: CSS variables plus swatches.
pub fn themes_json(current: &str) -> Value {
    let themes: Vec<Value> = crate::theme::all()
        .iter()
        .map(|p| {
            let vars: serde_json::Map<String, Value> =
                p.css_vars().into_iter().map(|(k, v)| (k.to_string(), Value::String(v))).collect();
            json!({"slug": p.slug, "name": p.name, "light": p.light, "source": p.source(),
                   "swatch": p.swatch().map(|c| format!("#{c:06x}")), "vars": vars})
        })
        .collect();
    json!({"current": current, "default": crate::theme::DEFAULT, "themes": themes})
}

fn set_theme(st: &State, req: Request, body: &str) {
    #[derive(Deserialize)]
    struct Body {
        name: String,
    }
    let b: Body = match serde_json::from_str(body) {
        Ok(b) => b,
        Err(e) => return error(req, 400, &format!("bad JSON body (want {{\"name\": \"<theme>\"}}): {e}")),
    };
    let Some(p) = crate::theme::by_name(&b.name) else {
        return error(req, 404, &format!("{:#}", crate::theme::unknown(&b.name)));
    };
    match crate::config::Config::save_theme(&st.deck().paths, Some(p.slug)) {
        Ok(file) => {
            *st.theme.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.slug.to_string());
            json_reply(req, 200, &json!({"ok": true, "theme": p.slug, "saved_to": file}))
        }
        Err(e) => error(req, 500, &format!("couldn't save the theme: {e:#}")),
    }
}

fn stream_logs(st: &State, req: Request, target: &str) {
    let name = match st.deck().name_of(target) {
        Ok(n) => n,
        Err(e) => return error(req, 404, &format!("{e:#}")),
    };
    // tiny_http buffers chunked bodies in 8 KiB chunks, so write the SSE
    // response by hand and flush after every event.
    let mut w = req.into_writer();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\n\
                X-Accel-Buffering: no\r\nConnection: close\r\n\r\n";
    if w.write_all(head.as_bytes()).and_then(|_| w.flush()).is_err() {
        return;
    }
    let mut stream = LogStream::new(st.deck().clone(), name);
    while let Some(chunk) = stream.next_chunk() {
        if w.write_all(&chunk).and_then(|_| w.flush()).is_err() {
            break; // client went away
        }
    }
}

/// Follows a recipe's log as SSE events. The caller stops when the client goes
/// away (a write fails); the stream itself ends after [`LogStream::MAX_AGE`].
pub struct LogStream {
    deck: Arc<Deck>,
    name: String,
    path: Option<PathBuf>,
    pos: u64,
    partial: Vec<u8>,
    out: Vec<u8>,
    last_write: Instant,
    born: Instant,
}

impl LogStream {
    const MAX_AGE: Duration = Duration::from_secs(6 * 3600);

    pub fn new(deck: Arc<Deck>, name: String) -> LogStream {
        LogStream {
            deck,
            name,
            path: None,
            pos: 0,
            partial: vec![],
            out: vec![],
            last_write: Instant::now(),
            born: Instant::now(),
        }
    }

    fn event(&mut self, kind: Option<&str>, data: &str) {
        if let Some(k) = kind {
            self.out.extend_from_slice(format!("event: {k}\n").as_bytes());
        }
        for line in data.split('\n') {
            self.out.extend_from_slice(b"data: ");
            self.out.extend_from_slice(line.as_bytes());
            self.out.push(b'\n');
        }
        self.out.push(b'\n');
    }

    fn poll(&mut self) {
        let path = self.deck.log_for(&self.name);
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if self.path.as_ref() != Some(&path) || len < self.pos {
            // New (or truncated) file: announce it and replay its tail.
            let first = self.path.is_none();
            self.path = Some(path.clone());
            self.partial.clear();
            self.event(Some("reset"), &json!({"path": path, "first": first}).to_string());
            for l in tail_lines(&path, 200) {
                self.event(None, &l);
            }
            self.pos = len;
            return;
        }
        if len > self.pos {
            if let Ok(mut f) = std::fs::File::open(&path) {
                if f.seek(SeekFrom::Start(self.pos)).is_ok() {
                    let mut buf = vec![];
                    let _ = f.take(1 << 20).read_to_end(&mut buf);
                    self.pos += buf.len() as u64;
                    self.partial.extend_from_slice(&buf);
                }
            }
            while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.partial.drain(..=i).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                let text = text.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or("").to_string();
                self.event(None, &crate::ui::strip_ansi(&text));
            }
        }
    }
}

impl LogStream {
    /// Blocks until there is something to send: log lines, a reset, or a
    /// keep-alive comment every 15 s. `None` once the stream is too old.
    pub fn next_chunk(&mut self) -> Option<Vec<u8>> {
        loop {
            if !self.out.is_empty() {
                self.last_write = Instant::now();
                return Some(std::mem::take(&mut self.out));
            }
            if self.born.elapsed() > Self::MAX_AGE {
                return None;
            }
            self.poll();
            if self.out.is_empty() {
                if self.last_write.elapsed() > Duration::from_secs(15) {
                    self.out.extend_from_slice(b": ping\n\n");
                } else {
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }
}

/// `lmdeck serve`.
pub fn run(deck: Deck, opts: WebOptions) -> Result<()> {
    let loopback = is_loopback(&opts.host);
    let mut token = opts.token.filter(|t| !t.is_empty());
    let generated = !loopback && token.is_none();
    if generated {
        token = Some(random_token());
    }
    let (server, addr) = bind(&opts.host, opts.port)?;
    let shown_host = if loopback { "127.0.0.1".to_string() } else { opts.host.clone() };
    let url = format!(
        "http://{}:{}/",
        if shown_host.contains(':') { format!("[{shown_host}]") } else { shown_host },
        addr.port()
    );
    println!("◆ lmdeck dashboard on {url}");
    match &token {
        // Only a generated token is shown: a supplied one stays out of logs
        // and the journal (a service on a peer runs with LMDECK_TOKEN).
        Some(t) if generated => {
            println!("  open: {url}#token={t}");
            println!("  api:  curl -H 'Authorization: Bearer {t}' {url}api/status");
        }
        Some(_) => println!("  token required (the one passed with --token / LMDECK_TOKEN)"),
        None => {}
    }

    if !loopback {
        eprintln!(
            "warning: listening on {} exposes start/stop controls to your network{}",
            opts.host,
            if generated { "; a one-time token was generated (pass --token to pick one)" } else { "" }
        );
    }
    println!("  ctrl-c to stop (servers it started keep running)");
    crate::updates::spawn_background(deck.paths.clone(), deck.cfg.clone());
    if let Some(every) = crate::hub::Poller::spawn(deck.paths.clone()) {
        if deck.cfg.harness.hub {
            println!(
                "  harness hub: every host's models reach every host's harnesses (looks every {} s)",
                every.as_secs()
            );
        }
    }
    let state = Arc::new(State::new(deck, token, loopback));
    serve(server, state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_and_queries() {
        assert_eq!(percent_decode("deck%2Fqwen3-4b"), "deck/qwen3-4b");
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        assert_eq!(percent_decode("bad%zz%"), "bad%zz%");
        assert_eq!(query_param("n=5&token=abc%21", "token").as_deref(), Some("abc!"));
        assert_eq!(query_param("n=5", "token"), None);
    }

    #[test]
    fn topology_page_is_not_capped_at_three_hosts() {
        assert!(INDEX_HTML.contains("function ringR"));
        assert!(!INDEX_HTML.contains("nHost <= 3"), "canvas height must not special-case three hosts");
        assert!(!INDEX_HTML.contains("ellipse/triangle"));
    }

    #[test]
    fn loopback_and_tokens() {
        for h in ["localhost", "127.0.0.1", "::1", "[::1]", "127.0.0.2"] {
            assert!(is_loopback(h), "{h}");
        }
        for h in ["0.0.0.0", "192.168.1.5", "example.com", "::"] {
            assert!(!is_loopback(h), "{h}");
        }
        let t = random_token();
        assert_eq!(t.len(), 32);
        assert_ne!(t, random_token());
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
