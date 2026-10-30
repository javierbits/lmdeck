//! SSH to the other Sparks, over their ConnectX-7 addresses only.
//!
//! Everything the setup wizard does on another machine goes through [`Ssh`],
//! so tests can swap in a scripted runner. The real one shells out to
//! OpenSSH with `BatchMode=yes`: it never prompts, so a missing key shows up
//! as a failure the wizard can explain (and offer `ssh-copy-id` for) instead
//! of a password prompt in the middle of a scan. Nor can it type a
//! passphrase, so when ssh's own choice of keys is refused, [`Ssh::login`]
//! tries each key without one.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a remote command printed, and whether it exited 0.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(stdout: &str) -> Out {
        Out { ok: true, stdout: stdout.into(), stderr: String::new() }
    }

    pub fn fail(stderr: &str) -> Out {
        Out { ok: false, stdout: String::new(), stderr: stderr.into() }
    }
}

/// Why a key login failed, from ssh's stderr.
#[derive(Debug, Clone, PartialEq)]
pub enum Login {
    Ok,
    /// The server wants a password: this user's key isn't in its authorized_keys.
    Denied,
    /// The host key doesn't match known_hosts (the machine was reinstalled?).
    HostKey,
    /// Anything else (timeout, refused, no route): ssh's first stderr line.
    Failed(String),
}

impl Login {
    pub fn from_out(o: &Out) -> Login {
        if o.ok {
            return Login::Ok;
        }
        let e = o.stderr.to_lowercase();
        if e.contains("permission denied") {
            Login::Denied
        } else if e.contains("host key verification failed") || e.contains("remote host identification has changed") {
            Login::HostKey
        } else {
            let line = o.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("ssh failed");
            Login::Failed(line.to_string())
        }
    }
}

/// Runs things on other machines.
pub trait Ssh {
    /// Runs `cmd` on `addr` without prompting (BatchMode), feeding `stdin`.
    fn run(&mut self, addr: &str, cmd: &str, stdin: Option<&[u8]>) -> Out;
    /// The host key `addr` presents (no login needed): the same machine shows
    /// the same key on every cable, which is how links are grouped.
    fn host_key(&mut self, addr: &str) -> Option<String>;
    /// Copies the public half of `key` to `addr`. Interactive: ssh-copy-id
    /// asks for that machine's password on the terminal.
    fn copy_id(&mut self, addr: &str, key: &Path) -> bool;
    /// Lists this user's keys without a passphrase (the only kind BatchMode
    /// can use), with the ones ssh's config offers for `addr` first
    /// ([`usable_keys`]).
    fn keys(&mut self, addr: &str) -> Vec<PathBuf>;
    /// Makes an ed25519 key pair without a passphrase, never over an existing
    /// file, and returns its path.
    fn make_key(&mut self) -> Option<PathBuf>;
    /// Names `key` in every later [`Ssh::run`] on `addr`, or goes back to
    /// ssh's own choice with `None`.
    fn use_key(&mut self, addr: &str, key: Option<&Path>);

    /// Runs `cmd` like [`Ssh::run`]; when the login is refused, tries again
    /// with each of [`Ssh::keys`] and keeps the first that logs in for
    /// `addr`. Returns the first refusal when none does.
    fn login(&mut self, addr: &str, cmd: &str, stdin: Option<&[u8]>) -> Out {
        let out = self.run(addr, cmd, stdin);
        if Login::from_out(&out) != Login::Denied {
            return out;
        }
        for key in self.keys(addr) {
            self.use_key(addr, Some(&key));
            let again = self.run(addr, cmd, stdin);
            if again.ok {
                return again;
            }
        }
        self.use_key(addr, None);
        out
    }
}

/// OpenSSH on this machine.
#[derive(Debug, Default)]
pub struct System {
    /// The key [`Ssh::use_key`] named for each address; ssh picks its own for the rest.
    using: HashMap<String, PathBuf>,
}

/// Options every non-interactive call uses. `accept-new` records the host key
/// of a machine seen for the first time (a cable you plugged in) but still
/// refuses one whose key changed.
const OPTS: [&str; 8] =
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=accept-new", "-o", "LogLevel=ERROR"];

impl Ssh for System {
    fn run(&mut self, addr: &str, cmd: &str, stdin: Option<&[u8]>) -> Out {
        let mut ssh = Command::new("ssh");
        ssh.args(OPTS);
        if let Some(key) = self.using.get(addr) {
            // Reason: IdentitiesOnly leaves the agent's other keys out, so they
            // can't use up the server's MaxAuthTries before this one is offered.
            ssh.arg("-i").arg(key).args(["-o", "IdentitiesOnly=yes"]);
        }
        let child = ssh
            .arg(addr)
            .arg(cmd)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return Out::fail(&format!("can't run ssh: {e}")),
        };
        if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(data);
        }
        match child.wait_with_output() {
            Ok(o) => Out {
                ok: o.status.success(),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            },
            Err(e) => Out::fail(&e.to_string()),
        }
    }

    fn host_key(&mut self, addr: &str) -> Option<String> {
        let out = crate::util::run_with_timeout(
            Command::new("ssh-keyscan").args(["-T", "3", "-t", "ed25519,ecdsa,rsa", addr]),
            std::time::Duration::from_secs(6),
        )?;
        parse_keyscan(&out)
    }

    fn copy_id(&mut self, addr: &str, key: &Path) -> bool {
        Command::new("ssh-copy-id")
            .arg("-i")
            .arg(public_half(key))
            .args(["-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=10", addr])
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Reads the keys ssh's config offers for `addr` from `ssh -G`, then
    /// keeps those and the rest of `~/.ssh` that [`usable_keys`] accepts.
    fn keys(&mut self, addr: &str) -> Vec<PathBuf> {
        let home = crate::paths::home_dir();
        let config =
            crate::util::run_with_timeout(Command::new("ssh").args(["-G", addr]), std::time::Duration::from_secs(5))
                .unwrap_or_default();
        usable_keys(&home.join(".ssh"), &identity_files(&config, &home))
    }

    /// Runs ssh-keygen at `~/.ssh/id_ed25519`, or `~/.ssh/id_ed25519_lmdeck`
    /// when that is taken.
    fn make_key(&mut self) -> Option<PathBuf> {
        let dir = crate::paths::home_dir().join(".ssh");
        // Reason: an existing key may be in use elsewhere (and ssh-keygen would
        // stop to ask), so the new one takes the first name that is free.
        let key = ["id_ed25519", "id_ed25519_lmdeck"]
            .iter()
            .map(|n| dir.join(n))
            .find(|k| !k.exists() && !public_half(k).exists())?;
        let _ = std::fs::create_dir_all(&dir);
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", ""])
            .arg("-f")
            .arg(&key)
            .status()
            .is_ok_and(|s| s.success())
            .then_some(key)
    }

    /// Stores `key` for `addr`, which `run` then passes as `-i` with
    /// `IdentitiesOnly=yes`.
    fn use_key(&mut self, addr: &str, key: Option<&Path>) {
        match key {
            Some(k) => self.using.insert(addr.to_string(), k.to_path_buf()),
            None => self.using.remove(addr),
        };
    }
}

/// Lists the `identityfile` values of `ssh -G` output in order, with a
/// leading `~/` expanded to `home`. Values with `%` tokens are skipped.
fn identity_files(config: &str, home: &Path) -> Vec<PathBuf> {
    config
        .lines()
        .filter_map(|l| l.strip_prefix("identityfile "))
        .filter(|v| !v.contains('%'))
        .map(|v| match v.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(v),
        })
        .collect()
}

/// Returns `key` with `.pub` appended: its public half.
fn public_half(key: &Path) -> PathBuf {
    let mut p = key.as_os_str().to_owned();
    p.push(".pub");
    p.into()
}

/// Returns the keys ssh can log in with in BatchMode: `offered` in its
/// order, then the files in `dir` by name, each kept when it exists, has a
/// `.pub` next to it and no passphrase. Duplicates are dropped; paths come
/// back as given, or as `dir` joined with the name.
pub fn usable_keys(dir: &Path, offered: &[PathBuf]) -> Vec<PathBuf> {
    let mut names: Vec<_> =
        std::fs::read_dir(dir).map(|d| d.filter_map(Result::ok).map(|e| e.file_name()).collect()).unwrap_or_default();
    names.sort();
    let mut keys: Vec<PathBuf> = vec![];
    // Reason: the keys ssh's config offers come first, so the one the wizard
    // copies is the one plain `ssh` uses too.
    for key in offered.iter().cloned().chain(names.iter().map(|n| dir.join(n))) {
        if keys.contains(&key) || !key.is_file() || !public_half(&key).is_file() {
            continue;
        }
        // Reason: BatchMode can't type a passphrase, so a key that has one
        // never logs in; `-P ""` makes ssh-keygen fail on it instead of asking.
        let open = crate::util::run_with_timeout(
            Command::new("ssh-keygen").args(["-y", "-P", ""]).arg("-f").arg(&key),
            std::time::Duration::from_secs(5),
        );
        if open.is_some() {
            keys.push(key);
        }
    }
    keys
}

/// The first key in `ssh-keyscan` output as `type base64`, the part that is
/// the same whichever address the machine was reached on. Keys are sorted so
/// the choice doesn't depend on the order the server sent them.
pub fn parse_keyscan(out: &str) -> Option<String> {
    let mut keys: Vec<String> = out
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let _host = f.next()?;
            Some(format!("{} {}", f.next()?, f.next()?))
        })
        .collect();
    keys.sort();
    keys.into_iter().next()
}

/// The first alias in `~/.ssh/config` whose `Host` line lists `ip`, skipping
/// patterns and addresses (`Host ember ember.local 10.100.132.2` → `ember`).
pub fn config_alias(config: &str, ip: &str) -> Option<String> {
    config.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        if !f.next()?.eq_ignore_ascii_case("host") {
            return None;
        }
        let names: Vec<&str> = f.collect();
        if !names.contains(&ip) {
            return None;
        }
        names
            .iter()
            .find(|n| n.parse::<std::net::IpAddr>().is_err() && !n.contains(['*', '?', '!']))
            .map(|n| n.split('.').next().unwrap_or(n).to_string())
    })
}

/// [`config_alias`] against this user's `~/.ssh/config`.
pub fn alias_for(ip: &str) -> Option<String> {
    let text = std::fs::read_to_string(crate::paths::home_dir().join(".ssh/config")).ok()?;
    config_alias(&text, ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_failures_are_told_apart() {
        assert_eq!(Login::from_out(&Out::ok("ember\n")), Login::Ok);
        assert_eq!(
            Login::from_out(&Out::fail("jvr0x@10.0.0.2: Permission denied (publickey,password).")),
            Login::Denied
        );
        assert_eq!(Login::from_out(&Out::fail("Host key verification failed.\r\n")), Login::HostKey);
        assert_eq!(
            Login::from_out(&Out::fail("\nssh: connect to host 10.0.0.2 port 22: Connection refused\n")),
            Login::Failed("ssh: connect to host 10.0.0.2 port 22: Connection refused".into())
        );
    }

    #[test]
    fn keyscan_picks_a_stable_key() {
        let a =
            "# 10.0.0.2:22 SSH-2.0-OpenSSH_9.6\n10.0.0.2 ssh-ed25519 AAAAkey1\n10.0.0.2 ecdsa-sha2-nistp256 AAAAkey2\n";
        let b = "10.0.1.2 ecdsa-sha2-nistp256 AAAAkey2\n10.0.1.2 ssh-ed25519 AAAAkey1\n";
        assert_eq!(parse_keyscan(a), parse_keyscan(b));
        assert_eq!(parse_keyscan(a).as_deref(), Some("ecdsa-sha2-nistp256 AAAAkey2"));
        assert_eq!(parse_keyscan("# only a comment\n"), None);
    }

    #[test]
    fn ssh_config_alias_skips_patterns_and_addresses() {
        let cfg = "Host *\n  ForwardAgent no\nHost 10.100.132.2 ember Ember ember.local 10.100.133.2\n  HostName 10.100.132.2\nHost gigabyte.lan 10.100.128.1\n";
        assert_eq!(config_alias(cfg, "10.100.133.2").as_deref(), Some("ember"));
        assert_eq!(config_alias(cfg, "10.100.128.1").as_deref(), Some("gigabyte"));
        assert_eq!(config_alias(cfg, "10.100.136.1"), None);
        assert_eq!(config_alias("Host 10.0.0.9\n", "10.0.0.9"), None);
    }

    #[test]
    fn identity_files_keep_ssh_g_order_and_skip_tokens() {
        let g = "user jvr0x\nidentitiesonly yes\nidentityfile ~/.ssh/id_ed25519_cluster\nidentityfile /etc/ssh/spark0\nidentityfile %d/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519\n";
        let home = Path::new("/home/jvr0x");
        assert_eq!(
            identity_files(g, home),
            vec![home.join(".ssh/id_ed25519_cluster"), PathBuf::from("/etc/ssh/spark0"), home.join(".ssh/id_ed25519")]
        );
        assert!(identity_files("", home).is_empty());
    }
}
