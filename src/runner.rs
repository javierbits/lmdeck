//! Starting, probing and stopping model servers.
//!
//! A *detached* recipe (a wrapped `start.sh` that `docker run -d`s the server
//! and exits 0) runs under a small `sh` wrapper that writes the start
//! command's exit code to `<run>/<name>.exit` and then, if the recipe has
//! `engine.logs`, `exec`s that log follower so its output lands in the same
//! log file. Liveness after a clean exit comes from the container (when
//! `engine.container` is set) or from probing the port.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::engine::{resolve_program, LaunchPlan};
use crate::paths::Paths;
use crate::util::now_secs;

/// What lmdeck remembers about a server it started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub name: String,
    pub engine: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub container: Option<String>,
    pub port: u16,
    pub started_at: u64,
    pub log: PathBuf,
    pub argv: Vec<String>,
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    #[serde(default)]
    pub oneshot: bool,
    /// Working directory and environment of the start command; `stop` runs with both.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The start command exits 0 once the server is up.
    #[serde(default)]
    pub detached: bool,
    /// Where the detached start command's exit code is written.
    #[serde(default)]
    pub exit_file: Option<PathBuf>,
}

impl RunRecord {
    fn path(paths: &Paths, name: &str) -> PathBuf {
        paths.run_dir().join(format!("{name}.json"))
    }

    pub fn load(paths: &Paths, name: &str) -> Option<RunRecord> {
        let text = fs::read_to_string(Self::path(paths, name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fs::create_dir_all(paths.run_dir())?;
        fs::write(Self::path(paths, &self.name), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn remove(paths: &Paths, name: &str) {
        let _ = fs::remove_file(Self::path(paths, name));
        let _ = fs::remove_file(exit_file(paths, name));
    }

    pub fn all(paths: &Paths) -> Vec<RunRecord> {
        let mut out: Vec<RunRecord> = fs::read_dir(paths.run_dir())
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| serde_json::from_str(&fs::read_to_string(e.path()).ok()?).ok())
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a: &RunRecord, b| a.name.cmp(&b.name));
        out
    }

    /// The process (or oneshot load, or detached server) is still alive.
    pub fn alive(&self) -> bool {
        match self.pid {
            Some(pid) if pid_alive(pid) => true,
            Some(_) if self.detached => self.detached_alive(),
            Some(_) => false,
            None => self.oneshot,
        }
    }

    /// Exit code of a detached start command, once it has exited.
    pub fn start_exit(&self) -> Option<i32> {
        fs::read_to_string(self.exit_file.as_ref()?).ok()?.trim().parse().ok()
    }

    /// A detached server whose start command has exited: its container is
    /// running, or (without a container) the start succeeded and the port answers.
    fn detached_alive(&self) -> bool {
        match &self.container {
            Some(c) => container_running("docker", c),
            None => {
                self.start_exit() == Some(0) && matches!(probe(self.port, Duration::from_millis(800)), Probe::Up(_))
            }
        }
    }
}

fn exit_file(paths: &Paths, name: &str) -> PathBuf {
    paths.run_dir().join(format!("{name}.exit"))
}

/// `docker inspect` says the container exists and is running.
pub fn container_running(docker_bin: &str, name: &str) -> bool {
    Command::new(docker_bin)
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
}

/// Runs the start command, records its exit code, then hands over to the log
/// follower (if any) in the same process. Arguments: exit file, follower
/// command line (shell-quoted, may be empty), start argv…
const DETACHED_WRAPPER: &str = r##"f="$1"; l="$2"; shift 2
"$@"; rc=$?
echo "$rc" > "$f"
if [ "$rc" -ne 0 ]; then echo "# lmdeck: start command exited $rc"; exit "$rc"; fi
if [ -z "$l" ]; then echo "# lmdeck: start command exited 0 (detached)"; exit 0; fi
echo "# lmdeck: start command exited 0; following: $l"
eval "exec $l""##;

pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    let mut st: libc::c_int = 0;
    // SAFETY: waitpid with WNOHANG only reaps `pid` if it is our own exited
    // child (a zombie would otherwise look alive to kill(0)); for any other
    // process it fails with ECHILD and has no effect.
    if unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) } == pid {
        return false;
    }
    // SAFETY: kill(2) with signal 0 performs only an existence/permission check.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Outcome of probing `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Nothing is listening.
    Down,
    /// Something answered; the served model ids when the response was readable.
    Up(Vec<String>),
}

/// Probes an OpenAI-compatible server on localhost. Any HTTP response counts
/// as up (TabbyAPI answers 401 without a key).
pub fn probe(port: u16, timeout: Duration) -> Probe {
    let url = format!("http://127.0.0.1:{port}/v1/models");
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    match agent.get(&url).call() {
        Ok(resp) => {
            let ids = resp
                .into_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned())
                .map(|arr| arr.iter().filter_map(|m| m.get("id")?.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Probe::Up(ids)
        }
        Err(ureq::Error::Status(_, _)) => Probe::Up(vec![]),
        Err(e) => {
            if std::env::var_os("LMDECK_DEBUG").is_some() {
                eprintln!("lmdeck: probe {url}: {e}");
            }
            Probe::Down
        }
    }
}

/// Live state of one recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Started by lmdeck and answering.
    Loaded,
    /// Started by lmdeck, process alive, not answering yet.
    Loading,
    /// Not started by lmdeck, but the port serves this recipe's name.
    External,
    /// The port is taken by something else.
    PortBusy,
    Stopped,
}

impl Status {
    pub fn is_up(&self) -> bool {
        matches!(self, Status::Loaded | Status::Loading | Status::External)
    }
}

/// Determines a recipe's status (cleaning up stale run records).
pub fn status(paths: &Paths, name: &str, port: u16) -> Status {
    status_cached(paths, name, port, &mut HashMap::new())
}

/// Like [`status`], probing each port at most once per `cache`.
pub fn status_cached(paths: &Paths, name: &str, port: u16, cache: &mut HashMap<u16, Probe>) -> Status {
    let mut probe_port = |p: u16| cache.entry(p).or_insert_with(|| probe(p, Duration::from_millis(800))).clone();
    if let Some(rec) = RunRecord::load(paths, name) {
        if rec.alive() {
            return match probe_port(rec.port) {
                Probe::Up(ids) if !rec.oneshot || ids.iter().any(|i| i == name) => Status::Loaded,
                _ if rec.oneshot => {
                    RunRecord::remove(paths, name);
                    Status::Stopped
                }
                _ => Status::Loading,
            };
        }
        RunRecord::remove(paths, name);
    }
    match probe_port(port) {
        Probe::Down => Status::Stopped,
        Probe::Up(ids) if ids.iter().any(|i| i == name) => Status::External,
        Probe::Up(_) => Status::PortBusy,
    }
}

/// Starts a server according to `plan`, recording it under the state dir.
pub fn start(paths: &Paths, name: &str, engine: &str, plan: &LaunchPlan) -> Result<RunRecord> {
    if plan.argv.is_empty() {
        bail!("empty launch command");
    }
    fs::create_dir_all(paths.log_dir())?;
    let log_path = paths.log_dir().join(format!("{name}.log"));
    let mut log = File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    writeln!(log, "# lmdeck {} — started {}", env!("CARGO_PKG_VERSION"), now_secs())?;
    writeln!(log, "# {}", plan.command_line())?;
    if let Some(cwd) = &plan.cwd {
        writeln!(log, "# in {}", cwd.display())?;
        if !cwd.is_dir() {
            bail!("working directory {} does not exist", cwd.display());
        }
    }

    let build = |argv: &[String], log: &File| -> Result<Command> {
        let mut argv = argv.to_vec();
        resolve_program(&mut argv, plan.cwd.as_deref());
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).envs(&plan.env).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log.try_clone()?);
        if let Some(cwd) = &plan.cwd {
            cmd.current_dir(cwd);
        }
        Ok(cmd)
    };

    for pre in &plan.pre {
        let status = build(pre, &log)?.status().with_context(|| format!("running {}", pre[0]))?;
        if !status.success() {
            bail!("`{}` failed ({status}); see {}", pre.join(" "), log_path.display());
        }
    }

    let mut rec = RunRecord {
        name: name.into(),
        engine: engine.into(),
        pid: None,
        container: plan.container.clone(),
        port: plan.port,
        started_at: now_secs(),
        log: log_path.clone(),
        argv: plan.argv.clone(),
        stop: plan.stop.clone(),
        oneshot: plan.oneshot,
        cwd: plan.cwd.clone(),
        env: plan.env.clone(),
        detached: plan.detached,
        exit_file: None,
    };
    if plan.oneshot {
        let status = build(&plan.argv, &log)?.status().with_context(|| format!("running {}", plan.argv[0]))?;
        if !status.success() {
            bail!("`{}` failed ({status}); see {}", plan.command_line(), log_path.display());
        }
    } else {
        let mut cmd = if plan.detached {
            let ef = exit_file(paths, name);
            // The start command may exit before the run record is saved.
            if let Some(d) = ef.parent() {
                fs::create_dir_all(d)?;
            }
            let _ = fs::remove_file(&ef);
            rec.exit_file = Some(ef.clone());
            let follower = plan
                .logs
                .as_ref()
                .map(|l| shlex::try_join(l.iter().map(String::as_str)).unwrap_or_else(|_| l.join(" ")))
                .unwrap_or_default();
            let mut start = plan.argv.clone();
            resolve_program(&mut start, plan.cwd.as_deref());
            let mut argv: Vec<String> = vec![
                "sh".into(),
                "-c".into(),
                DETACHED_WRAPPER.into(),
                "lmdeck-start".into(),
                ef.display().to_string(),
                follower,
            ];
            argv.extend(start);
            build(&argv, &log)?
        } else {
            build(&plan.argv, &log)?
        };
        // Own process group: survives the terminal and lets stop() signal the whole tree.
        cmd.process_group(0);
        let child = cmd.spawn().with_context(|| format!("launching {}", plan.argv[0]))?;
        rec.pid = Some(child.id());
        // The child is intentionally not waited on; it outlives lmdeck.
        std::mem::forget(child);
    }
    rec.save(paths)?;
    Ok(rec)
}

/// How a readiness wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    Ready,
    Died,
    Timeout,
}

/// Polls until the server answers, its process exits, or `timeout` passes.
pub fn wait_ready(rec: &RunRecord, timeout: Duration, mut tick: impl FnMut(Duration)) -> Ready {
    let start = Instant::now();
    let mut last_tick = Duration::ZERO;
    loop {
        if rec.detached {
            if !rec.pid.is_some_and(pid_alive) {
                // The start command has exited (and any log follower with it).
                let container_up = rec.container.as_deref().map(|c| container_running("docker", c));
                match (rec.start_exit(), container_up) {
                    (_, Some(true)) => {}
                    (Some(0), None) => {} // exited cleanly: keep probing the port
                    _ => return Ready::Died,
                }
            }
        } else if !rec.alive() {
            return Ready::Died;
        }
        if let Probe::Up(_) = probe(rec.port, Duration::from_secs(2)) {
            return Ready::Ready;
        }
        let el = start.elapsed();
        if el >= timeout {
            return Ready::Timeout;
        }
        if el - last_tick >= Duration::from_secs(10) {
            last_tick = el;
            tick(el);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Last `n` lines of a log file.
pub fn tail(path: &Path, n: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Stops a server lmdeck started. Returns `false` if there was nothing to stop.
pub fn stop(paths: &Paths, name: &str, docker_bin: &str) -> Result<bool> {
    let Some(rec) = RunRecord::load(paths, name) else { return Ok(false) };
    // 1. The recipe's own stop command, with the start command's cwd and env.
    if let Some(cmd) = rec.stop.as_ref().filter(|c| !c.is_empty()) {
        let mut argv = cmd.clone();
        resolve_program(&mut argv, rec.cwd.as_deref());
        let out = || -> Stdio {
            fs::OpenOptions::new().append(true).open(&rec.log).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
        };
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]).envs(&rec.env).stdin(Stdio::null()).stdout(out()).stderr(out());
        if let Some(cwd) = rec.cwd.as_ref().filter(|d| d.is_dir()) {
            c.current_dir(cwd);
        }
        let _ = c.status();
    }
    // 2. `docker stop` (for a wrapped recipe, only if its stop left the container running).
    if let Some(c) = &rec.container {
        if !rec.detached || container_running(docker_bin, c) {
            let _ = Command::new(docker_bin)
                .args(["stop", "-t", "20", c])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    // 3. The process group (a still-running start script, or the log follower).
    if let Some(pid) = rec.pid.filter(|p| pid_alive(*p)) {
        terminate(pid, Duration::from_secs(20));
    }
    RunRecord::remove(paths, name);
    Ok(true)
}

/// SIGTERM the process group, escalating to SIGKILL after `grace`.
pub fn terminate(pid: u32, grace: Duration) {
    let Ok(p) = i32::try_from(pid) else { return };
    // SAFETY: plain signal delivery; -p addresses the process group we created.
    unsafe {
        if libc::kill(-p, libc::SIGTERM) != 0 {
            libc::kill(p, libc::SIGTERM);
        }
    }
    let start = Instant::now();
    while start.elapsed() < grace {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // SAFETY: as above.
    unsafe {
        if libc::kill(-p, libc::SIGKILL) != 0 {
            libc::kill(p, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn tmp_paths() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let p = Paths { config_dir: d.path().join("c"), cache_dir: d.path().join("k"), state_dir: d.path().join("s") };
        (d, p)
    }

    fn plan(argv: &[&str], port: u16) -> LaunchPlan {
        LaunchPlan {
            pre: vec![],
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: BTreeMap::from([("LMDECK_TEST".to_string(), "1".to_string())]),
            container: None,
            oneshot: false,
            stop: None,
            port,
            detached: false,
            logs: None,
            checkout: None,
        }
    }

    fn free_port() -> u16 {
        // Never hand out the same port twice in one test process: parallel tests
        // each start a server on "their" port.
        static USED: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
        let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let p = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            if !used.contains(&p) {
                used.push(p);
                return p;
            }
        }
    }

    #[test]
    fn start_status_stop_lifecycle() {
        let (_d, paths) = tmp_paths();
        let port = free_port();
        let rec = start(&paths, "sleeper", "custom", &plan(&["sleep", "30"], port)).unwrap();
        assert!(rec.alive());
        assert_eq!(RunRecord::all(&paths).len(), 1);
        assert_eq!(status(&paths, "sleeper", port), Status::Loading);
        assert!(stop(&paths, "sleeper", "docker").unwrap());
        assert!(!pid_alive(rec.pid.unwrap()));
        assert_eq!(status(&paths, "sleeper", port), Status::Stopped);
        assert!(!stop(&paths, "sleeper", "docker").unwrap());
    }

    #[test]
    fn dead_process_is_detected() {
        let (_d, paths) = tmp_paths();
        let rec = start(&paths, "crash", "custom", &plan(&["sh", "-c", "echo boom; exit 3"], free_port())).unwrap();
        assert_eq!(wait_ready(&rec, Duration::from_secs(10), |_| {}), Ready::Died);
        assert!(tail(&rec.log, 5).contains("boom"));
        assert_eq!(status(&paths, "crash", rec.port), Status::Stopped);
        assert!(RunRecord::load(&paths, "crash").is_none(), "stale record cleaned");
    }

    /// A wrapped "start.sh" that backgrounds a stub HTTP server, waits for it,
    /// and exits 0; "stop.sh" kills it via a pid file. Both run in the
    /// recipe's cwd with its env (PORT).
    #[test]
    fn detached_start_ready_and_stop_script() {
        let (d, paths) = tmp_paths();
        let work = d.path().join("checkout");
        fs::create_dir_all(&work).unwrap();
        let start_sh = "#!/bin/sh\n\
            echo \"starting on $PORT in $(pwd)\"\n\
            python3 -m http.server \"$PORT\" --bind 127.0.0.1 >server.log 2>&1 &\n\
            echo $! > server.pid\n\
            for i in $(seq 1 100); do\n\
              python3 -c \"import urllib.request,sys; urllib.request.urlopen('http://127.0.0.1:$PORT/')\" 2>/dev/null && exit 0\n\
              sleep 0.1\n\
            done\n\
            exit 1\n";
        let stop_sh = "#!/bin/sh\necho \"stopping $PORT\"\nkill $(cat server.pid)\nrm -f server.pid\n";
        for (f, body) in [("start.sh", start_sh), ("stop.sh", stop_sh)] {
            fs::write(work.join(f), body).unwrap();
            let mut perm = fs::metadata(work.join(f)).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
            fs::set_permissions(work.join(f), perm).unwrap();
        }
        let port = free_port();
        let mut p = plan(&["./start.sh"], port);
        p.cwd = Some(work.clone());
        p.env = BTreeMap::from([("PORT".to_string(), port.to_string())]);
        p.detached = true;
        p.stop = Some(vec!["./stop.sh".into()]);
        let rec = start(&paths, "wrapped", "custom", &p).unwrap();
        assert_eq!(wait_ready(&rec, Duration::from_secs(30), |_| {}), Ready::Ready);
        // The start script exits 0; that is not a crash.
        let t0 = Instant::now();
        while pid_alive(rec.pid.unwrap()) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(rec.start_exit(), Some(0));
        assert!(rec.alive(), "alive through the port probe");
        assert_eq!(status(&paths, "wrapped", port), Status::Loaded);
        let saved = RunRecord::load(&paths, "wrapped").unwrap();
        assert_eq!(saved.cwd.as_deref(), Some(work.as_path()));
        assert_eq!(saved.env["PORT"], port.to_string());
        assert!(stop(&paths, "wrapped", "docker").unwrap());
        assert!(!work.join("server.pid").exists(), "stop.sh ran in the checkout");
        let t0 = Instant::now();
        while probe(port, Duration::from_millis(300)) != Probe::Down && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(status(&paths, "wrapped", port), Status::Stopped);
        let log = tail(&rec.log, 20);
        assert!(log.contains(&format!("starting on {port}")), "{log}");
        assert!(log.contains("exited 0 (detached)") && log.contains(&format!("stopping {port}")), "{log}");
    }

    #[test]
    fn detached_failure_and_log_follower() {
        let (_d, paths) = tmp_paths();
        let mut p = plan(&["sh", "-c", "echo nope; exit 4"], free_port());
        p.detached = true;
        let rec = start(&paths, "bad", "custom", &p).unwrap();
        assert_eq!(wait_ready(&rec, Duration::from_secs(10), |_| {}), Ready::Died);
        assert_eq!(rec.start_exit(), Some(4), "{:?} {}", rec.exit_file, tail(&rec.log, 20));
        assert!(!rec.alive());

        // After a clean exit the follower keeps the pid alive and writes to the log.
        let mut p = plan(&["true"], free_port());
        p.detached = true;
        p.logs = Some(vec!["sh".into(), "-c".into(), "echo following the server; sleep 30".into()]);
        let rec = start(&paths, "follow", "custom", &p).unwrap();
        let t0 = Instant::now();
        while !tail(&rec.log, 5).contains("following the server") && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(tail(&rec.log, 5).contains("following the server"));
        assert_eq!(rec.start_exit(), Some(0));
        assert!(pid_alive(rec.pid.unwrap()));
        assert!(stop(&paths, "follow", "docker").unwrap());
        assert!(!pid_alive(rec.pid.unwrap()));
    }

    #[test]
    fn probe_down_on_closed_port() {
        assert_eq!(probe(free_port(), Duration::from_millis(300)), Probe::Down);
    }

    #[test]
    fn failing_launch_errors() {
        let (_d, paths) = tmp_paths();
        assert!(start(&paths, "x", "custom", &plan(&["/definitely/not/here"], 1)).is_err());
        let mut p = plan(&["true"], 1);
        p.pre = vec![vec!["false".into()]];
        assert!(start(&paths, "y", "custom", &p).is_err());
    }
}
