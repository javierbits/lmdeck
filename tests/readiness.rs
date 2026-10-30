//! A server is ready when `GET /v1/models` answers 2xx. Anything else that
//! answers is up but not ready: llama-server says 503 while it loads, and a
//! server behind a key says 401 unless lmdeck sends it the key the server
//! keeps (TabbyAPI's `api_tokens.yml`, an `--api-key` flag).

use lmdeck::paths::Paths;
use lmdeck::runner::{probe, status, wait_ready, Probe, Ready, RunRecord, Status};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// An HTTP server on a free port answering every request with the status in
/// the returned cell (an OpenAI model list on a 2xx); with `key`, a request
/// that doesn't carry it gets 401, as a server behind an API key does.
fn stub(key: Option<&'static str>) -> (u16, Arc<AtomicU16>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let status = Arc::new(AtomicU16::new(503));
    let cell = status.clone();
    std::thread::spawn(move || {
        for mut sock in listener.incoming().flatten() {
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            let keyed = key.is_none_or(|k| {
                req.contains(&format!("x-api-key: {k}")) || req.contains(&format!("authorization: bearer {k}"))
            });
            let code = if keyed { cell.load(Ordering::SeqCst) } else { 401 };
            let body = match code {
                204 | 304 => "",
                200..=299 => r#"{"object":"list","data":[{"id":"stub"}]}"#,
                _ => r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#,
            };
            let head = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nConnection: close\r\n");
            let len = if body.is_empty() { String::new() } else { format!("Content-Length: {}\r\n", body.len()) };
            let _ = sock.write_all(format!("{head}{len}\r\n{body}").as_bytes());
        }
    });
    (port, status)
}

/// A run record for a server on `port` whose process (this test) is alive.
fn record(dir: &Path, engine: &str, port: u16, argv: &[&str]) -> RunRecord {
    RunRecord {
        name: "srv".into(),
        engine: engine.into(),
        pid: Some(std::process::id()),
        container: None,
        port,
        started_at: 0,
        log: dir.join("srv.log"),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        stop: None,
        oneshot: false,
        cwd: Some(dir.to_path_buf()),
        env: BTreeMap::new(),
        detached: false,
        exit_file: None,
    }
}

/// Paths rooted in `dir`, with `rec` saved as a running server.
fn saved(dir: &Path, rec: &RunRecord) -> Paths {
    let home = dir.display().to_string();
    let paths = Paths::resolve(|k| (k == "LMDECK_HOME").then(|| home.clone()), dir.to_path_buf());
    rec.save(&paths).unwrap();
    paths
}

/// What `wait_ready` says within `secs` seconds.
fn ready_within(rec: &RunRecord, secs: u64) -> Ready {
    wait_ready(rec, Duration::from_secs(secs), |_| {})
}

#[test]
fn a_server_answering_503_while_it_loads_is_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let (port, code) = stub(None);
    let rec = record(tmp.path(), "llama.cpp", port, &["llama-server"]);
    let paths = saved(tmp.path(), &rec);
    assert_eq!(probe(port, Duration::from_secs(2), None), Probe::NotReady(503));
    assert_eq!(ready_within(&rec, 2), Ready::Timeout, "a 503 is no readiness");
    assert_eq!(status(&paths, "srv", port), Status::Loading);
    // Loaded: the model list answers 200.
    code.store(200, Ordering::SeqCst);
    assert_eq!(probe(port, Duration::from_secs(2), None), Probe::Up(vec!["stub".into()]));
    assert_eq!(ready_within(&rec, 5), Ready::Ready);
    assert_eq!(status(&paths, "srv", port), Status::Loaded);
}

#[test]
fn any_2xx_is_ready_and_an_error_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let (port, code) = stub(None);
    let rec = record(tmp.path(), "custom", port, &["srv"]);
    code.store(204, Ordering::SeqCst);
    assert_eq!(ready_within(&rec, 5), Ready::Ready, "204 No Content");
    code.store(500, Ordering::SeqCst);
    assert_eq!(ready_within(&rec, 2), Ready::Timeout, "a 500 is a server that broke, not one that is ready");
    // Nothing listening and no process: died, not ready.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let gone = RunRecord { pid: None, ..record(tmp.path(), "custom", dead, &["srv"]) };
    assert_eq!(probe(dead, Duration::from_millis(500), None), Probe::Down);
    assert_eq!(ready_within(&gone, 2), Ready::Died);
}

#[test]
fn a_server_behind_its_own_key_is_probed_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    // TabbyAPI writes its key to api_tokens.yml in its working directory, auth on by default.
    let (port, code) = stub(Some("k3y-tabby"));
    code.store(200, Ordering::SeqCst);
    let tabby = record(tmp.path(), "tabbyapi", port, &["python3", "main.py"]);
    let paths = saved(tmp.path(), &tabby);
    assert_eq!(ready_within(&tabby, 2), Ready::Timeout, "401 without the key");
    std::fs::write(tmp.path().join("api_tokens.yml"), "api_key: k3y-tabby\nadmin_key: adm1n\n").unwrap();
    assert_eq!(ready_within(&tabby, 5), Ready::Ready);
    assert_eq!(status(&paths, "srv", port), Status::Loaded);
    // vLLM, SGLang, llama-server: the key the command line gives it.
    let (port, code) = stub(Some("k3y-vllm"));
    code.store(200, Ordering::SeqCst);
    let vllm = record(tmp.path(), "vllm", port, &["vllm", "serve", "m", "--api-key", "k3y-vllm"]);
    assert_eq!(ready_within(&vllm, 5), Ready::Ready);
    let other = record(tmp.path(), "vllm", port, &["vllm", "serve", "m", "--api-key=wrong"]);
    assert_eq!(ready_within(&other, 2), Ready::Timeout);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Ready exactly when the answer is a 2xx; any other answer is up, not ready.
    #[test]
    fn ready_exactly_on_a_2xx(code in 200u16..600) {
        static STUB: std::sync::OnceLock<(u16, Arc<AtomicU16>)> = std::sync::OnceLock::new();
        let (port, cell) = STUB.get_or_init(|| stub(None));
        cell.store(code, Ordering::SeqCst);
        let p = probe(*port, Duration::from_secs(2), None);
        prop_assert_eq!(matches!(p, Probe::Up(_)), (200..300).contains(&code), "{} -> {:?}", code, p);
        prop_assert!(matches!(p, Probe::Up(_)) || p == Probe::NotReady(code), "{} -> {:?}", code, p);
    }
}
