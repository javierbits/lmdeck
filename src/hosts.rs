//! Other machines running `lmdeck serve`, aggregated next to this one.
//!
//! Design, kept deliberately small:
//! - `hosts.toml` in the config dir lists `[[host]] name / url / token`.
//!   It is written with mode 0600 because it may hold tokens.
//! - Every [`SAMPLE_EVERY`] the local [`Sampler`] collects this machine's
//!   telemetry and fetches `GET <url>/api/telemetry?history=1&since=…` from
//!   each host in parallel, with a short timeout. Remotes only ever answer
//!   with their *own* telemetry, so hosts that list each other can't loop.
//! - A host that fails keeps its last telemetry and history, marked offline
//!   with the error, so the UIs can grey it out.
//! - Aggregation is read-only: starting/stopping models on another host is
//!   done on that host (its own dashboard or CLI).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::paths::Paths;
use crate::telemetry::{demo, now_ms, Collector, History, Sample, Telemetry, SAMPLE_EVERY};

/// Per-request timeout when polling a host.
pub const FETCH_TIMEOUT: Duration = Duration::from_millis(1500);
/// The reserved name of this machine in the aggregate.
pub const LOCAL: &str = "local";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostEntry {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostsFile {
    #[serde(default, rename = "host")]
    pub hosts: Vec<HostEntry>,
}

pub fn hosts_path(paths: &Paths) -> PathBuf {
    paths.config_dir.join("hosts.toml")
}

/// `http(s)://host[:port]` with no trailing slash. No scheme is guessed.
pub fn normalize_url(url: &str) -> Result<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u.strip_prefix("http://").or_else(|| u.strip_prefix("https://"));
    match rest {
        Some(r) if !r.is_empty() && !r.contains(char::is_whitespace) => Ok(u.to_string()),
        _ => bail!("host URL must look like http://spark1:7979 (got '{url}')"),
    }
}

impl HostsFile {
    pub fn load(paths: &Paths) -> Result<HostsFile> {
        let p = hosts_path(paths);
        match fs::read_to_string(&p) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HostsFile::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<PathBuf> {
        fs::create_dir_all(&paths.config_dir)?;
        let p = hosts_path(paths);
        let body =
            format!("# lmdeck hosts — written by `lmdeck host add`; may contain tokens\n{}", toml::to_string(self)?);
        let tmp = p.with_extension("toml.tmp");
        {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            use std::io::Write;
            opts.open(&tmp)?.write_all(body.as_bytes())?;
        }
        #[cfg(unix)]
        fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        fs::rename(&tmp, &p).with_context(|| format!("writing {}", p.display()))?;
        Ok(p)
    }

    /// Adds or replaces a host.
    pub fn add(&mut self, name: &str, url: &str, token: Option<String>) -> Result<HostEntry> {
        if !crate::recipe::valid_name(name) || name == LOCAL {
            bail!("invalid host name '{name}' (letters, digits, . _ -; '{LOCAL}' is reserved)");
        }
        let e = HostEntry { name: name.to_string(), url: normalize_url(url)?, token: token.filter(|t| !t.is_empty()) };
        self.hosts.retain(|h| h.name != name);
        self.hosts.push(e.clone());
        Ok(e)
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let n = self.hosts.len();
        self.hosts.retain(|h| h.name != name);
        self.hosts.len() != n
    }
}

/// One host as the UIs show it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostView {
    pub name: String,
    pub url: Option<String>,
    pub local: bool,
    pub online: bool,
    pub error: Option<String>,
    pub latency_ms: Option<u64>,
    /// Unix ms of the last successful sample.
    pub last_seen: Option<u64>,
    /// Latest telemetry; stale (last known) when `online` is false.
    pub telemetry: Option<Telemetry>,
    pub history: Vec<Sample>,
}

/// What `/api/telemetry` returns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryReply {
    pub telemetry: Option<Telemetry>,
    #[serde(default)]
    pub history: Vec<Sample>,
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(401, _) => "unauthorized: wrong or missing token".into(),
        ureq::Error::Status(403, _) => "forbidden: the host only answers on localhost (serve --host …)".into(),
        ureq::Error::Status(404, _) => "no /api/telemetry (lmdeck too old?)".into(),
        ureq::Error::Status(c, _) => format!("HTTP {c}"),
        ureq::Error::Transport(t) => {
            let s = t.to_string();
            if s.contains("timed out") || s.contains("Timeout") {
                "timed out".into()
            } else if s.contains("onnection refused") {
                "connection refused".into()
            } else {
                s.lines().next().unwrap_or("unreachable").to_string()
            }
        }
    }
}

/// Fetches one host's telemetry plus history newer than `since`.
pub fn fetch(entry: &HostEntry, since: u64, timeout: Duration) -> Result<(TelemetryReply, u64), String> {
    let agent = ureq::AgentBuilder::new().timeout_connect(timeout).timeout(timeout).build();
    let url = format!("{}/api/telemetry?history=1&since={since}", entry.url);
    let mut req = agent.get(&url);
    if let Some(t) = &entry.token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let t0 = Instant::now();
    let resp = req.call().map_err(describe)?;
    let reply: TelemetryReply = resp.into_json().map_err(|e| format!("bad reply: {e}"))?;
    if reply.telemetry.is_none() {
        return Err("host has no telemetry yet".into());
    }
    Ok((reply, t0.elapsed().as_millis() as u64))
}

/// Polls the configured hosts and remembers what it last saw.
#[derive(Default)]
pub struct Aggregator {
    known: HashMap<String, (HostView, History)>,
    pub timeout: Option<Duration>,
}

impl Aggregator {
    /// One round: the local view first, then every entry (fetched in parallel).
    pub fn refresh(&mut self, entries: &[HostEntry], local: &Telemetry, local_hist: &History) -> Vec<HostView> {
        let timeout = self.timeout.unwrap_or(FETCH_TIMEOUT);
        let since: Vec<u64> = entries
            .iter()
            .map(|e| self.known.get(&e.name).and_then(|k| k.1.samples.back()).map(|s| s.at).unwrap_or(0))
            .collect();
        let results: Vec<Result<(TelemetryReply, u64), String>> = std::thread::scope(|s| {
            let jobs: Vec<_> =
                entries.iter().zip(&since).map(|(e, since)| s.spawn(move || fetch(e, *since, timeout))).collect();
            jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err("fetch panicked".into()))).collect()
        });
        self.known.retain(|k, _| entries.iter().any(|e| &e.name == k));
        let mut out = vec![HostView {
            name: LOCAL.into(),
            url: None,
            local: true,
            online: true,
            error: None,
            latency_ms: Some(0),
            last_seen: Some(local.at),
            telemetry: Some(local.clone()),
            history: local_hist.samples.iter().cloned().collect(),
        }];
        for (e, r) in entries.iter().zip(results) {
            let (view, hist) = self.known.entry(e.name.clone()).or_default();
            view.name = e.name.clone();
            view.url = Some(e.url.clone());
            match r {
                Ok((reply, ms)) => {
                    hist.merge(&reply.history);
                    if let Some(t) = &reply.telemetry {
                        hist.push(Sample::from(t));
                        view.last_seen = Some(t.at);
                    }
                    view.online = true;
                    view.error = None;
                    view.latency_ms = Some(ms);
                    view.telemetry = reply.telemetry;
                }
                Err(err) => {
                    view.online = false;
                    view.error = Some(err);
                    view.latency_ms = None;
                }
            }
            view.history = hist.samples.iter().cloned().collect();
            out.push(view.clone());
        }
        out
    }
}

/// Synthetic hosts for `LMDECK_DEMO=1`: three Sparks and an offline Mac.
pub fn demo_hosts(local: &Telemetry, local_hist: &History, at: u64) -> Vec<HostView> {
    let n = local_hist.samples.len().max(1);
    let host = |name: &str, url: &str, seed: f64, busy: bool, ms: u64| {
        let f = |t: f64, at: u64| demo::spark(name, seed, t, at, busy);
        HostView {
            name: name.into(),
            url: Some(url.into()),
            online: true,
            latency_ms: Some(ms),
            last_seen: Some(at),
            telemetry: Some(f(at as f64 / 1000.0, at)),
            history: demo::history(n.max(150), at, f).samples.into(),
            ..Default::default()
        }
    };
    let mut local_view = HostView {
        name: LOCAL.into(),
        local: true,
        online: true,
        latency_ms: Some(0),
        last_seen: Some(local.at),
        telemetry: Some(local.clone()),
        history: local_hist.samples.iter().cloned().collect(),
        ..Default::default()
    };
    if local_view.history.len() < 150 {
        let back = demo::history(150, at, |t, a| demo::spark("spark1", 0.1, t + 400.0, a, true));
        local_view.history = back.samples.into();
    }
    let seen = at.saturating_sub(185_000);
    let mac_hist = demo::history(60, seen, demo::mac);
    vec![
        local_view,
        host("spark2", "http://spark2:7979", 0.45, true, 4),
        host("spark3", "http://spark3:7979", 0.8, false, 6),
        HostView {
            name: "mac".into(),
            url: Some("http://mac:7979".into()),
            online: false,
            error: Some("timed out".into()),
            last_seen: Some(seen),
            telemetry: Some(demo::mac(seen as f64 / 1000.0, seen)),
            history: mac_hist.samples.into(),
            ..Default::default()
        },
    ]
}

/// Live state shared by the sampler thread and its readers.
#[derive(Default)]
pub struct Live {
    pub telemetry: Option<Telemetry>,
    pub history: History,
    pub hosts: Vec<HostView>,
}

/// Background sampling: local telemetry, its history, and the host aggregate.
pub struct Sampler {
    pub live: Arc<Mutex<Live>>,
}

impl Sampler {
    /// Starts the sampler thread. `with_hosts` also polls `hosts.toml` entries.
    pub fn spawn(paths: Paths, with_hosts: bool) -> Sampler {
        let live = Arc::new(Mutex::new(Live::default()));
        let shared = live.clone();
        std::thread::Builder::new()
            .name("lmdeck-telemetry".into())
            .spawn(move || {
                let mut collector = Collector::new();
                let mut agg = Aggregator::default();
                let mut history = History::default();
                loop {
                    let started = Instant::now();
                    let t = collector.collect(&paths);
                    history.push(Sample::from(&t));
                    let hosts = if collector.demo {
                        demo_hosts(&t, &history, now_ms())
                    } else if with_hosts {
                        let entries = HostsFile::load(&paths).map(|f| f.hosts).unwrap_or_default();
                        agg.refresh(&entries, &t, &history)
                    } else {
                        agg.refresh(&[], &t, &history)
                    };
                    {
                        let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                        g.telemetry = Some(t);
                        g.history = history.clone();
                        g.hosts = hosts;
                    }
                    std::thread::sleep(SAMPLE_EVERY.saturating_sub(started.elapsed()));
                }
            })
            .expect("spawn telemetry thread");
        Sampler { live }
    }

    /// Waits up to `timeout` for the first sample.
    pub fn wait_first(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        loop {
            if self.live.lock().map(|g| g.telemetry.is_some()).unwrap_or(false) {
                return true;
            }
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn reply(&self, history: bool, since: u64) -> TelemetryReply {
        let g = self.live.lock().unwrap_or_else(|e| e.into_inner());
        TelemetryReply {
            telemetry: g.telemetry.clone(),
            history: if history {
                g.history.samples.iter().filter(|s| s.at > since).cloned().collect()
            } else {
                vec![]
            },
        }
    }

    pub fn hosts(&self) -> Vec<HostView> {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).hosts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn paths(dir: &std::path::Path) -> Paths {
        Paths { config_dir: dir.join("cfg"), cache_dir: dir.join("cache"), state_dir: dir.join("state") }
    }

    #[test]
    fn hosts_file_roundtrip_and_validation() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let mut f = HostsFile::load(&p).unwrap();
        assert!(f.hosts.is_empty());
        f.add("spark1", "http://spark1:7979/", Some("s3cret".into())).unwrap();
        f.add("mac", "https://mac.tail1234.ts.net:7979", None).unwrap();
        f.add("spark1", "http://100.64.0.11:7979", Some("t2".into())).unwrap(); // replaces
        assert!(f.add("local", "http://x:1", None).is_err());
        assert!(f.add("bad name", "http://x:1", None).is_err());
        assert!(f.add("nourl", "spark1:7979", None).is_err(), "no scheme is guessed");
        let file = f.save(&p).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let back = HostsFile::load(&p).unwrap();
        assert_eq!(back.hosts.len(), 2);
        assert_eq!(
            back.hosts[1],
            HostEntry { name: "spark1".into(), url: "http://100.64.0.11:7979".into(), token: Some("t2".into()) }
        );
        assert_eq!(back.hosts[0].token, None);
        let mut back = back;
        assert!(back.remove("mac"));
        assert!(!back.remove("mac"));
    }

    /// A one-shot HTTP server answering every request with `status` + `body`.
    fn fake_host(status: &str, body: String, requests: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let status = status.to_string();
        let h = std::thread::spawn(move || {
            let mut seen = vec![];
            for _ in 0..requests {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
            seen
        });
        (url, h)
    }

    fn closed_port() -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        url
    }

    #[test]
    fn aggregates_online_offline_and_unauthorized_hosts() {
        let remote = Telemetry {
            hostname: "spark2".into(),
            at: 5_000,
            mem_total_gb: 119.7,
            mem_used_gb: Some(60.0),
            ..Default::default()
        };
        let hist = vec![Sample { at: 1_000, mem_pct: Some(10.0), ..Default::default() }];
        let body = serde_json::to_string(&TelemetryReply { telemetry: Some(remote.clone()), history: hist }).unwrap();
        let (ok_url, ok_srv) = fake_host("200 OK", body.clone(), 2);
        let (auth_url, _auth) = fake_host("401 Unauthorized", "{\"ok\":false}".into(), 1);
        let entries = vec![
            HostEntry { name: "spark2".into(), url: ok_url, token: Some("tok".into()) },
            HostEntry { name: "spark3".into(), url: closed_port(), token: None },
            HostEntry { name: "mac".into(), url: auth_url, token: None },
        ];
        let local = Telemetry { hostname: "spark1".into(), at: 4_000, ..Default::default() };
        let mut agg = Aggregator { timeout: Some(Duration::from_millis(800)), ..Default::default() };
        let v = agg.refresh(&entries, &local, &History::default());
        assert_eq!(v.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["local", "spark2", "spark3", "mac"]);
        assert!(v[0].local && v[0].online);
        assert!(v[1].online, "{:?}", v[1].error);
        assert_eq!(v[1].telemetry.as_ref().unwrap().hostname, "spark2");
        assert_eq!(v[1].history.iter().map(|s| s.at).collect::<Vec<_>>(), vec![1_000, 5_000]);
        assert!(!v[2].online);
        assert_eq!(v[2].error.as_deref(), Some("connection refused"));
        assert!(v[2].telemetry.is_none());
        assert!(!v[3].online);
        assert!(v[3].error.as_deref().unwrap().contains("token"), "{:?}", v[3].error);

        // Second round: the host asks only for newer history; then it goes away
        // and keeps its last telemetry, greyed out by the UIs.
        let v = agg.refresh(&entries[..1], &local, &History::default());
        assert!(v[1].online);
        let reqs = ok_srv.join().unwrap();
        assert!(reqs[0].contains("since=0") && reqs[0].contains("Authorization: Bearer tok"), "{}", reqs[0]);
        assert!(reqs[1].contains("since=5000"), "{}", reqs[1]);
        let v = agg.refresh(&entries[..1], &local, &History::default());
        assert!(!v[1].online);
        assert_eq!(v[1].telemetry.as_ref().map(|t| t.at), Some(5_000), "last known telemetry is kept");
        assert_eq!(v[1].last_seen, Some(5_000));
        assert_eq!(v.len(), 2, "removed hosts are dropped");
    }

    #[test]
    fn demo_hosts_are_all_flagged() {
        let local = demo::local(1.0, 1_000_000);
        let v = demo_hosts(&local, &History::default(), 1_000_000);
        assert_eq!(v.len(), 4);
        assert!(v.iter().all(|h| h.telemetry.as_ref().is_some_and(|t| t.demo)));
        assert!(v.iter().any(|h| !h.online));
        assert!(v.iter().all(|h| h.history.len() >= 60));
    }
}
