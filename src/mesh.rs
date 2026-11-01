//! Keeps every host's hosts.toml listing the whole cluster, so a start, a
//! stop or `list` works from any machine, not only the one that ran setup.
//! `lmdeck host sync` reads each peer's token over ssh when hosts.toml lacks
//! it, then pushes each peer the others (and this machine) through
//! `lmdeck host import`, which only adds or updates: run it as often as you
//! like.

use anyhow::{bail, Result};
use std::path::Path;

use crate::hosts::{HostEntry, HostsFile};
use crate::paths::Paths;
use crate::ssh::Ssh;

/// Prints a host's `lmdeck serve` token (setup keeps it in serve.env).
pub const TOKEN_CMD: &str = r#"sed -n 's/^LMDECK_TOKEN=//p' "$HOME/.config/lmdeck/serve.env" | head -n 1"#;

/// Runs lmdeck on a peer over ssh, where a non-login shell's PATH often lacks
/// `~/.local/bin` (setup's install) and `~/.cargo/bin` (`cargo install`).
pub fn remote_lmdeck(args: &str) -> String {
    format!(
        r#"b=$(command -v lmdeck 2>/dev/null || ls "$HOME/.local/bin/lmdeck" "$HOME/.cargo/bin/lmdeck" 2>/dev/null | head -n 1); [ -n "$b" ] || {{ echo "lmdeck isn't installed there (not in PATH, ~/.local/bin or ~/.cargo/bin)" >&2; exit 127; }}; "$b" {args}"#
    )
}

/// `--host` and `--port` of a `lmdeck serve` systemd unit's ExecStart.
pub fn parse_exec(unit: &str) -> Option<(String, u16)> {
    let exec = unit.lines().find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let words: Vec<&str> = exec.split_whitespace().collect();
    let arg = |flag: &str| words.windows(2).find(|w| w[0] == flag).map(|w| w[1].to_string());
    let host = arg("--host")?;
    let port = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(crate::setup::SERVE_PORT);
    Some((host, port))
}

/// The value of `LMDECK_TOKEN=` in a serve.env.
pub fn parse_token(env: &str) -> Option<String> {
    env.lines()
        .find_map(|l| l.trim().strip_prefix("LMDECK_TOKEN="))
        .map(|t| t.trim().trim_matches('"').to_string())
        .filter(|t| !t.is_empty())
}

/// This machine as the others should list it: the address its `lmdeck serve`
/// service listens on and its token. `None` without that service.
pub fn own_entry(paths: &Paths, home: &Path, name: &str) -> Option<HostEntry> {
    let unit = std::fs::read_to_string(home.join(".config/systemd/user/lmdeck-serve.service")).ok()?;
    let (host, port) = parse_exec(&unit)?;
    if host == "127.0.0.1" || host == "localhost" || host == "0.0.0.0" {
        return None;
    }
    let token = std::fs::read_to_string(paths.config_dir.join("serve.env")).ok().and_then(|e| parse_token(&e));
    Some(HostEntry { name: name.into(), url: format!("http://{host}:{port}"), token, ..Default::default() })
}

/// Where to ssh to reach a host: its serve address, then its name.
fn addrs(e: &HostEntry) -> Vec<String> {
    let mut v = vec![crate::deck::url_host(&e.url).to_string()];
    if !v.contains(&e.name) {
        v.push(e.name.clone());
    }
    v
}

/// What a sync did, one line per host.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub lines: Vec<String>,
    pub failed: Vec<String>,
}

/// Fills in missing tokens over ssh (saving hosts.toml), then pushes every
/// host the rest of the cluster. `own` is this machine's entry
/// ([`own_entry`]); `dry_run` reads tokens but writes nowhere.
pub fn sync(paths: &Paths, ssh: &mut dyn Ssh, own: Option<HostEntry>, dry_run: bool) -> Result<Report> {
    let mut file = HostsFile::load(paths)?;
    if file.hosts.is_empty() {
        bail!("hosts.toml lists no hosts; add them with `lmdeck setup` or `lmdeck host add`");
    }
    let mut rep = Report::default();
    let mut fetched = false;
    for e in file.hosts.iter_mut().filter(|e| e.resolved_token().is_none()) {
        for a in addrs(e) {
            let out = ssh.run(&a, TOKEN_CMD, None);
            if let Some(t) = out.ok.then(|| out.stdout.trim().to_string()).filter(|t| !t.is_empty()) {
                e.token = Some(t);
                fetched = true;
                rep.lines.push(format!("{}: read its token over ssh", e.name));
                break;
            }
        }
    }
    if fetched && !dry_run {
        file.save(paths)?;
    }
    for e in &file.hosts {
        let mut hosts: Vec<HostEntry> = file
            .hosts
            .iter()
            .filter(|o| o.name != e.name)
            .map(|o| HostEntry {
                name: o.name.clone(),
                url: o.url.clone(),
                token: o.resolved_token(),
                ..Default::default()
            })
            .collect();
        hosts.extend(own.clone().filter(|o| o.name != e.name));
        let names: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
        if dry_run {
            rep.lines.push(format!("{}: would list {}", e.name, names.join(", ")));
            continue;
        }
        let body = toml::to_string(&HostsFile { hosts, links: vec![] })?;
        let cmd = remote_lmdeck(&format!("host import --skip {}", shlex::try_quote(&e.name)?));
        let mut last = String::new();
        let mut done = false;
        for a in addrs(e) {
            let out = ssh.run(&a, &cmd, Some(body.as_bytes()));
            if out.ok {
                let said = out.stdout.trim();
                rep.lines.push(format!("{}: {}", e.name, if said.is_empty() { "up to date" } else { said }));
                done = true;
                break;
            }
            last = out.stderr.trim().lines().last().unwrap_or("ssh failed").to_string();
        }
        if !done {
            rep.failed.push(format!("{}: {last}", e.name));
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::Out;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Answers by (addr, command prefix); records every call.
    #[derive(Default)]
    struct Fake {
        replies: HashMap<(String, &'static str), Out>,
        calls: Vec<(String, String, Option<String>)>,
    }

    impl Ssh for Fake {
        fn run(&mut self, addr: &str, cmd: &str, stdin: Option<&[u8]>) -> Out {
            self.calls.push((addr.into(), cmd.into(), stdin.map(|s| String::from_utf8_lossy(s).into_owned())));
            let kind = if cmd == TOKEN_CMD { "token" } else { "import" };
            self.replies.get(&(addr.to_string(), kind)).cloned().unwrap_or_else(|| Out::fail("ssh: connect refused"))
        }
        fn host_key(&mut self, _: &str) -> Option<String> {
            None
        }
        fn copy_id(&mut self, _: &str, _: &Path) -> bool {
            false
        }
        fn keys(&mut self, _: &str) -> Vec<PathBuf> {
            vec![]
        }
        fn make_key(&mut self) -> Option<PathBuf> {
            None
        }
        fn use_key(&mut self, _: &str, _: Option<&Path>) {}
    }

    fn tmp() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let p = Paths { config_dir: d.path().join("c"), cache_dir: d.path().join("k"), state_dir: d.path().join("s") };
        (d, p)
    }

    #[test]
    fn parses_the_serve_unit_and_env() {
        let unit =
            "[Service]\nExecStart=/home/u/.cargo/bin/lmdeck serve --host 10.100.132.1 --port 7979\nRestart=always\n";
        assert_eq!(parse_exec(unit), Some(("10.100.132.1".into(), 7979)));
        assert_eq!(parse_exec("ExecStart=/x/lmdeck serve --host 10.0.0.1\n"), Some(("10.0.0.1".into(), 7979)));
        assert_eq!(parse_exec("ExecStart=/x/lmdeck serve\n"), None);
        assert_eq!(parse_token("# x\nLMDECK_TOKEN=abc\n"), Some("abc".into()));
        assert_eq!(parse_token("LMDECK_TOKEN=\n"), None);
    }

    #[test]
    fn remote_lmdeck_looks_beyond_path() {
        let c = remote_lmdeck("host import");
        assert!(c.contains("command -v lmdeck") && c.contains(".local/bin/lmdeck") && c.contains(".cargo/bin/lmdeck"));
        assert!(c.ends_with("\"$b\" host import"), "{c}");
    }

    #[test]
    fn sync_fetches_missing_tokens_and_pushes_everyone_the_rest() {
        let (_d, paths) = tmp();
        let mut f = HostsFile::default();
        f.add("ember", "http://10.0.0.2:7979", Some("t-ember".into())).unwrap();
        f.add("faraday", "http://10.0.0.3:7979", None).unwrap();
        f.save(&paths).unwrap();
        let mut ssh = Fake::default();
        ssh.replies.insert(("10.0.0.3".into(), "token"), Out::ok("t-faraday\n"));
        ssh.replies.insert(("10.0.0.2".into(), "import"), Out::ok("added faraday, spark\n"));
        // faraday's serve address refuses ssh; its name works.
        ssh.replies.insert(("faraday".into(), "import"), Out::ok(""));
        let own = Some(HostEntry {
            name: "spark".into(),
            url: "http://10.0.0.1:7979".into(),
            token: Some("t-spark".into()),
            ..Default::default()
        });
        let rep = sync(&paths, &mut ssh, own, false).unwrap();
        assert_eq!(
            rep.lines,
            vec!["faraday: read its token over ssh", "ember: added faraday, spark", "faraday: up to date"]
        );
        assert!(rep.failed.is_empty());
        assert_eq!(HostsFile::load(&paths).unwrap().get("faraday").unwrap().token.as_deref(), Some("t-faraday"));
        let pushed =
            |addr: &str| ssh.calls.iter().find(|(a, c, _)| a == addr && c.contains("host import")).unwrap().clone();
        let (_, cmd, body) = pushed("10.0.0.2");
        assert!(cmd.ends_with("host import --skip ember"), "{cmd}");
        let got: HostsFile = toml::from_str(&body.unwrap()).unwrap();
        let names: Vec<(&str, Option<&str>)> =
            got.hosts.iter().map(|h| (h.name.as_str(), h.token.as_deref())).collect();
        assert_eq!(names, vec![("faraday", Some("t-faraday")), ("spark", Some("t-spark"))]);
        let (_, _, body) = pushed("faraday");
        let got: HostsFile = toml::from_str(&body.unwrap()).unwrap();
        assert_eq!(got.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), vec!["ember", "spark"]);
    }

    #[test]
    fn sync_reports_hosts_it_cannot_reach_and_dry_run_writes_nothing() {
        let (_d, paths) = tmp();
        let mut f = HostsFile::default();
        f.add("ember", "http://10.0.0.2:7979", Some("t".into())).unwrap();
        f.save(&paths).unwrap();
        let mut ssh = Fake::default();
        let rep = sync(&paths, &mut ssh, None, false).unwrap();
        assert_eq!(rep.failed, vec!["ember: ssh: connect refused"]);
        let mut ssh = Fake::default();
        let rep = sync(&paths, &mut ssh, None, true).unwrap();
        assert_eq!(rep.lines, vec!["ember: would list "]);
        assert!(ssh.calls.is_empty());
        assert!(sync(&tmp().1, &mut Fake::default(), None, false).is_err(), "no hosts");
    }
}
