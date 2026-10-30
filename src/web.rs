//! `lmdeck serve`: a JSON API plus a single-page dashboard, both embedded in
//! the binary. Bound to 127.0.0.1 by default; anything else needs a token.
//!
//! API (all JSON unless noted):
//! - `GET  /api/health`
//! - `GET  /api/snapshot`              everything below in one document
//! - `GET  /api/recipes` · `/api/status` · `/api/doctor` · `/api/sources`
//! - `POST /api/{start,stop,switch,pull}/{recipe}`  body optional `{"force":true}` (`pull`: `{"latest":true}`)
//! - `GET  /api/logs/{recipe}?n=200`
//! - `GET  /api/logs/{recipe}/stream`  Server-Sent Events, one `data:` per line
//! - `GET  /api/telemetry[?history=1&since=<unix ms>]`  this machine, live
//! - `GET  /api/hosts`                 this machine plus every `hosts.toml` entry
//!
//! - `GET  /api/weights` · `/api/weights/files?repo=&revision=`  this machine's weights
//! - `GET  /api/fabric` · `/api/fabric/reach?addr=`  NICs / RDMA (read-only detection), reachability
//! - `GET  /api/topology`              hosts + links as this controller sees them
//! - `GET  /api/probe?port=`           does a TCP port answer on this machine
//! - `GET  /api/plan/{recipe}?on=<host>`  where weights come from and the NFS steps (dry run)
//! - `POST /api/start/{recipe}` body `{"on":"<host>","allow_slow_link":false}` runs it on another host
//! - `POST /api/run` `{"recipe":"<toml>","only":false,"force":false}`  needs `[serve] accept_recipes`
//! - `POST /api/weights/{export,volume,preflight}`  the NFS steps (see `placement`)
//! - `GET  /api/hosts/{host}/{recipes,status,logs/{recipe}}`, `POST /api/hosts/{host}/{start,stop,switch}/{recipe}`
//!   proxied to that host with the token from hosts.toml
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

use crate::deck::{tail_lines, Deck, Snapshot};
use crate::hosts::Sampler;
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
    pub deck: Arc<Deck>,
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
            deck: Arc::new(deck),
            token,
            loopback,
            cache: Mutex::new(None),
            json_cache: Mutex::new(Default::default()),
            sampler,
            theme,
        }
    }

    fn snapshot(&self) -> Arc<Snapshot> {
        let mut g = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, s)) = g.as_ref() {
            if at.elapsed() < Duration::from_millis(900) {
                return s.clone();
            }
        }
        let s = Arc::new(self.deck.snapshot(false));
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
        return json_reply(req, 200, &json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}));
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
            "status" if query_param(&query, "name").is_some() => {
                // One recipe, without building a whole snapshot (controllers poll this while waiting).
                let name = query_param(&query, "name").unwrap_or_default();
                let cat = st.deck.catalog();
                let ports = crate::deck::planned_ports(&cat, &st.deck.paths, &st.deck.cfg);
                let rows: Vec<Value> = cat
                    .entries
                    .iter()
                    .filter(|f| f.recipe.name == name)
                    .take(1)
                    .map(|f| {
                        let port = crate::deck::planned_port(&ports, f);
                        let s = crate::runner::status(&st.deck.paths, &f.recipe.name, port);
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
                json_reply(
                    req,
                    200,
                    &json!({"version": s.version, "hardware": s.hardware, "inventory": s.inventory,
                            "recipe_errors": s.recipe_errors}),
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
                    let name = match st.deck.name_of(rest) {
                        Ok(n) => n,
                        Err(e) => return error(req, 404, &format!("{e:#}")),
                    };
                    let n = query_param(&query, "n").and_then(|v| v.parse().ok()).unwrap_or(200usize).min(5000);
                    let path = st.deck.log_for(&name);
                    let lines = tail_lines(&path, n);
                    return json_reply(req, 200, &json!({"name": name, "path": path, "lines": lines}));
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
            let node = st.deck.cfg.node_name();
            let elsewhere = opts.on.as_deref().filter(|h| !crate::remote::is_local(h, &node));
            let result = match (verb, elsewhere) {
                ("start" | "switch", Some(host)) => place(st, target, host, verb == "switch", &opts),
                _ => match verb {
                    "start" => st.deck.start(target, false, opts.force),
                    "switch" => st.deck.start(target, true, opts.force),
                    "stop" => st.deck.stop(target),
                    "pull" => st.deck.pull(target, opts.latest),
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
    let cat = st.deck.catalog();
    Ok(crate::deck::resolve(&cat, target)?.recipe.clone())
}

/// `start --on`: plan and run on another host, weights from their home.
fn place(st: &State, target: &str, host: &str, only: bool, o: &ActionBody) -> Result<String> {
    crate::placement::start_on(&st.deck, target, host, only, o.force, o.allow_slow_link)
}

fn client_for(st: &State, host: &str) -> Result<crate::remote::Client> {
    crate::remote::Client::named(&st.deck.paths, host)
}

/// Read routes added for remote control, weights and topology. Returns the
/// request when the route isn't one of these.
fn get_more(st: &State, req: Request, api: &str, query: &str) -> Option<Request> {
    let q = |k: &str| query_param(query, k);
    match api {
        "weights" => {
            let cfg = st.deck.cfg.clone();
            json_reply(req, 200, &st.cached("weights", Duration::from_secs(60), || json!(crate::weights::local(&cfg))))
        }
        "weights/index" => {
            let idx = crate::weights::Index::load(&st.deck.paths);
            let node = st.deck.cfg.node_name();
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
                json!(crate::topology::build(&st.deck.paths, &st.deck.cfg, &st.sampler.hosts(), verify))
            });
            json_reply(req, 200, &v)
        }
        "probe" => match q("port").and_then(|p| p.parse::<u16>().ok()) {
            Some(p) => json_reply(req, 200, &json!({"port": p, "open": crate::ops::port_open(p)})),
            None => error(req, 400, "port= is required"),
        },
        _ => {
            if let Some(target) = api.strip_prefix("plan/") {
                let r = match resolve_recipe(st, target) {
                    Ok(r) => r,
                    Err(e) => return refused(req, 404, &format!("{e:#}")),
                };
                let on = q("on").unwrap_or_else(|| crate::hosts::LOCAL.to_string());
                let slow = q("allow_slow_link").is_some_and(|v| v == "1" || v == "true");
                let node = st.deck.cfg.node_name();
                let v = match crate::placement::plan(&st.deck.paths, &st.deck.cfg, &r, &on, slow) {
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
                        format!("{}?n={}", crate::remote::encode(w), q("n").unwrap_or_else(|| "200".to_string()))
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
    let accept = st.deck.cfg.serve.accept_recipes;
    let refuse =
        "this host does not accept work from a controller; set [serve] accept_recipes = true in its config.toml";
    let result: Result<Value> = match api {
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
                crate::catalog::save_remote(&st.deck.paths, &r)?;
                let m = st.deck.start_recipe(&r, b("only"), b("force"))?;
                Ok(json!({"ok": true, "message": m}))
            })()
        }
        "weights/export" => (|| {
            let client = s("client").ok_or_else(|| anyhow!("client is required"))?;
            let inv = crate::weights::local(&st.deck.cfg);
            let m = crate::placement::ensure_export(&st.deck.cfg, &inv.export_path, &client)?;
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
                let m = crate::placement::ensure_volume(&st.deck.docker_bin(), &vol, &server, &path)?;
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
                    &st.deck.docker_bin(),
                    &need("volume")?,
                    &need("image")?,
                    &need("path")?,
                )?;
                Ok(json!({"ok": true, "files": files}))
            })()
        }
        _ => {
            let Some(rest) = api.strip_prefix("hosts/") else { return Some(req) };
            let mut it = rest.splitn(3, '/');
            let (Some(host), Some(verb), Some(target)) = (it.next(), it.next(), it.next()) else {
                return refused(req, 404, "want /api/hosts/<host>/<start|stop|switch>/<recipe>");
            };
            if !matches!(verb, "start" | "stop" | "switch") {
                return refused(req, 404, "unknown action (start, stop, switch)");
            }
            client_for(st, host)
                .and_then(|c| c.action(verb, target, b("force")))
                .map(|m| json!({"ok": true, "message": m}))
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
    match crate::config::Config::save_theme(&st.deck.paths, Some(p.slug)) {
        Ok(file) => {
            *st.theme.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.slug.to_string());
            json_reply(req, 200, &json!({"ok": true, "theme": p.slug, "saved_to": file}))
        }
        Err(e) => error(req, 500, &format!("couldn't save the theme: {e:#}")),
    }
}

fn stream_logs(st: &State, req: Request, target: &str) {
    let name = match st.deck.name_of(target) {
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
    let mut stream = LogStream::new(st.deck.clone(), name);
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
    if let Some(t) = &token {
        println!("  open: {url}#token={t}");
        println!("  api:  curl -H 'Authorization: Bearer {t}' {url}api/status");
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
