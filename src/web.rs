//! `lmdeck serve`: a JSON API plus a single-page dashboard, both embedded in
//! the binary. Bound to 127.0.0.1 by default; anything else needs a token.
//!
//! API (all JSON unless noted):
//! - `GET  /api/health`
//! - `GET  /api/snapshot`              everything below in one document
//! - `GET  /api/recipes` · `/api/status` · `/api/doctor` · `/api/sources`
//! - `POST /api/{start,stop,switch,pull}/{recipe}`  body optional `{"force":true}`
//! - `GET  /api/logs/{recipe}?n=200`
//! - `GET  /api/logs/{recipe}/stream`  Server-Sent Events, one `data:` per line
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
}

impl State {
    pub fn new(deck: Deck, token: Option<String>, loopback: bool) -> State {
        State { deck: Arc::new(deck), token, loopback, cache: Mutex::new(None) }
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
            _ => {
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
            let Some((verb, target)) = api.split_once('/') else { return error(req, 404, "not found") };
            let mut body = String::new();
            let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
            let opts: ActionBody = if body.trim().is_empty() {
                ActionBody::default()
            } else {
                match serde_json::from_str(&body) {
                    Ok(o) => o,
                    Err(e) => return error(req, 400, &format!("bad JSON body: {e}")),
                }
            };
            let result = match verb {
                "start" => st.deck.start(target, false, opts.force),
                "switch" => st.deck.start(target, true, opts.force),
                "stop" => st.deck.stop(target),
                "pull" => st.deck.pull(target),
                _ => return error(req, 404, "unknown action (start, stop, switch, pull)"),
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
