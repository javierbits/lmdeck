//! Setting up lmdeck on another Spark from the setup wizard, over SSH on its
//! ConnectX-7 address: check the key login (offer `ssh-copy-id` when it
//! fails), install lmdeck (copy this binary when the CPU matches, else
//! `cargo install` from GitHub), write a fresh token, start `lmdeck serve`
//! bound to the CX7 address, keep it running across reboots with a systemd
//! user service, then check it answers.
//!
//! The only secret typed here is the sudo password some machines need to
//! turn on "linger" (so a user service starts at boot without a login). It
//! is read without echo, sent to `sudo -S` on ssh's stdin, never printed,
//! logged or stored, and the buffer is zeroed after use. The token travels on
//! stdin too and lands in `~/.config/lmdeck/serve.env` (0600) there and in
//! this machine's hosts.toml (0600).

use crate::setup::{ask_yes, Peer, Prompt};
use crate::ssh::{Login, Ssh};
use crate::ui::{bold, dim, green, yellow};
use std::time::Duration;

/// Where `cargo install` builds lmdeck from when the binary can't be copied.
pub const REPO: &str = "https://github.com/javierbits/lmdeck";

/// What one login learns about the other machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub host: String,
    pub arch: String,
    pub os: String,
    pub user: String,
    pub home: String,
    /// Path of an installed lmdeck, and its version.
    pub lmdeck: Option<String>,
    pub version: Option<String>,
    pub cargo: bool,
    /// `systemctl --user` works in an ssh session.
    pub systemd_user: bool,
    pub linger: bool,
}

/// Runs the facts script (on stdin) through `sh`; the tag names it in tests.
pub const FACTS_CMD: &str = "sh -s -- lmdeck-facts";
const FACTS: &str = r#"echo "host=$(hostname)"
echo "arch=$(uname -m)"
echo "os=$(uname -s)"
echo "user=$(id -un)"
echo "home=$HOME"
b=$(command -v lmdeck 2>/dev/null || ls "$HOME/.local/bin/lmdeck" "$HOME/.cargo/bin/lmdeck" 2>/dev/null | head -n 1)
echo "lmdeck=$b"
[ -n "$b" ] && echo "version=$("$b" --version 2>/dev/null)"
{ command -v cargo || ls "$HOME/.cargo/bin/cargo"; } >/dev/null 2>&1 && echo "cargo=yes"
systemctl --user show-environment >/dev/null 2>&1 && echo "systemd=yes"
[ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" = yes ] && echo "linger=yes"
true
"#;

pub fn parse_facts(out: &str) -> Facts {
    let mut f = Facts::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "host" => f.host = v.split('.').next().unwrap_or(&v).to_string(),
            "arch" => f.arch = v,
            "os" => f.os = v,
            "user" => f.user = v,
            "home" => f.home = v,
            "lmdeck" => f.lmdeck = Some(v).filter(|s| !s.is_empty()),
            "version" => f.version = v.split_whitespace().last().map(str::to_string),
            "cargo" => f.cargo = v == "yes",
            "systemd" => f.systemd_user = v == "yes",
            "linger" => f.linger = v == "yes",
            _ => {}
        }
    }
    f
}

/// A peer that now runs `lmdeck serve`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ready {
    /// Its hostname.
    pub name: String,
    /// The CX7 address `lmdeck serve` is bound to.
    pub ip: String,
    pub token: String,
    /// It answered `/api/health` after starting.
    pub answering: bool,
}

/// Installs and starts lmdeck on `pr`. `health` checks `ip:port` for a
/// running `lmdeck serve` (the wizard's probe; tests stub it). Prints each
/// step; `None` when a step failed or was declined (with what to do next).
pub fn bootstrap(
    p: &mut dyn Prompt,
    ssh: &mut dyn Ssh,
    pr: &Peer,
    port: u16,
    health: &mut dyn FnMut(&str, u16) -> bool,
) -> Option<Ready> {
    let ip = pr.ip().to_string();
    let label = pr.name.clone().unwrap_or_else(|| ip.clone());

    // 1. A key login over the CX7 address.
    let f = login(p, ssh, &ip, &label)?;
    let name = f.host.clone();
    println!("    {} ssh {}@{name} ({ip}) works · {} {}", green("✓"), f.user, f.os, f.arch);

    // 2. lmdeck itself.
    let local = env!("CARGO_PKG_VERSION");
    let bin = match (&f.lmdeck, &f.version) {
        (Some(b), Some(v)) if v == local => {
            println!("    {} lmdeck {v} is already installed at {b}", green("✓"));
            b.clone()
        }
        _ => install(ssh, &ip, &f)?,
    };

    // 3. A token, on stdin, into a 0600 file there.
    let token = crate::web::random_token();
    let env = format!("LMDECK_TOKEN={token}\n");
    let out = ssh.run(&ip, TOKEN_CMD, Some(env.as_bytes()));
    if !out.ok {
        println!("    {} couldn't write the token on {name}: {}", yellow("!"), first_line(&out.stderr));
        return None;
    }
    println!("    {} new token saved there (~/.config/lmdeck/serve.env) and here (hosts.toml)", green("✓"));

    // 4. Start it, as a service when possible.
    let exec = format!("{bin} serve --host {ip} --port {port}");
    let mut started = false;
    if f.systemd_user {
        if ask_yes(p, &format!("    keep it running across reboots (a systemd user service on {name})?"), true) {
            let unit = unit_file(&exec, &f.home);
            let out = ssh.run(&ip, SERVICE_CMD, Some(unit.as_bytes()));
            if out.ok {
                println!("    {} lmdeck-serve.service enabled and started", green("✓"));
                started = true;
                if !f.linger {
                    linger(p, ssh, &ip, &name, &f.user);
                }
            } else {
                println!("    {} the service didn't start: {}", yellow("!"), first_line(&out.stderr));
            }
        }
    } else {
        println!("    {}", dim(&format!("no systemd user session on {name}: starting it in the background instead")));
    }
    if !started {
        let out = ssh.run(&ip, &background_cmd(&exec), None);
        if !out.ok {
            println!("    {} couldn't start lmdeck serve: {}", yellow("!"), first_line(&out.stderr));
            return None;
        }
        println!("    {} lmdeck serve started (until {name} reboots)", green("✓"));
    }

    // 5. Does it answer?
    let mut answering = false;
    for i in 0..20 {
        if health(&ip, port) {
            answering = true;
            break;
        }
        if i < 19 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    if answering {
        println!("    {} lmdeck answers at {}", green("✓"), bold(&format!("http://{ip}:{port}")));
    } else {
        println!(
            "    {} nothing answers at {ip}:{port} yet. On {name}: `systemctl --user status lmdeck-serve` (or ~/.local/state/lmdeck/serve.log); a firewall may block {port}",
            yellow("!")
        );
    }
    Some(Ready { name, ip, token, answering })
}

/// Logs in with the key; when the key is refused, offers to make one and
/// copy it with `ssh-copy-id`, then tries once more.
fn login(p: &mut dyn Prompt, ssh: &mut dyn Ssh, ip: &str, label: &str) -> Option<Facts> {
    for attempt in 0..2 {
        let out = ssh.run(ip, FACTS_CMD, Some(FACTS.as_bytes()));
        match Login::from_out(&out) {
            Login::Ok => return Some(parse_facts(&out.stdout)),
            Login::Denied if attempt == 0 => {
                println!("    {} {label} ({ip}) doesn't accept this machine's SSH key yet.", yellow("!"));
                println!(
                    "    {}",
                    dim("lmdeck logs in with keys only (BatchMode), so it never stops to ask for a password.")
                );
                if !ssh.has_key(false) {
                    if !ask_yes(p, "    you have no SSH key here; make one (ed25519, no passphrase)?", true) {
                        println!("    {}", dim(&format!("later: ssh-keygen -t ed25519, then ssh-copy-id {ip}")));
                        return None;
                    }
                    if !ssh.has_key(true) {
                        println!("    {} ssh-keygen failed", yellow("!"));
                        return None;
                    }
                }
                let q = format!("    copy your key there with ssh-copy-id? it asks for your password on {label} once");
                if !ask_yes(p, &q, true) {
                    println!("    {}", dim(&format!("later: ssh-copy-id {ip}, then `lmdeck setup`")));
                    return None;
                }
                if !ssh.copy_id(ip) {
                    println!("    {} ssh-copy-id didn't finish; try it by hand: ssh-copy-id {ip}", yellow("!"));
                    return None;
                }
            }
            Login::Denied => {
                println!(
                    "    {} {label} ({ip}) still refuses the key; check ~/.ssh/authorized_keys there",
                    yellow("!")
                );
                return None;
            }
            Login::HostKey => {
                println!(
                    "    {} {label} ({ip}) shows a different SSH host key than ~/.ssh/known_hosts has. If it was reinstalled: ssh-keygen -R {ip}, then `lmdeck setup`",
                    yellow("!")
                );
                return None;
            }
            Login::Failed(m) => {
                println!("    {} ssh to {label} ({ip}) failed: {m}", yellow("!"));
                return None;
            }
        }
    }
    None
}

/// Copies this binary when the other machine has the same OS and CPU, else
/// builds it there with cargo. Returns the installed path.
fn install(ssh: &mut dyn Ssh, ip: &str, f: &Facts) -> Option<String> {
    let same = f.os.eq_ignore_ascii_case(std::env::consts::OS) && f.arch == std::env::consts::ARCH;
    if same {
        match std::env::current_exe().and_then(std::fs::read) {
            Ok(bytes) => {
                println!(
                    "    {}",
                    dim(&format!(
                        "copying this lmdeck ({:.0} MB, {}) to {}:~/.local/bin/lmdeck",
                        bytes.len() as f64 / 1e6,
                        f.arch,
                        f.host
                    ))
                );
                let out = ssh.run(ip, COPY_CMD, Some(&bytes));
                if out.ok {
                    println!("    {} installed {}", green("✓"), out.stdout.trim());
                    return Some(format!("{}/.local/bin/lmdeck", f.home));
                }
                println!("    {} copy failed: {}", yellow("!"), first_line(&out.stderr));
            }
            Err(e) => println!("    {} can't read this lmdeck binary: {e}", yellow("!")),
        }
    }
    if f.cargo {
        println!("    {}", dim(&format!("building lmdeck on {} with cargo install (a few minutes)...", f.host)));
        let out = ssh.run(ip, &cargo_cmd(), None);
        if out.ok {
            println!("    {} built and installed with cargo", green("✓"));
            return Some(format!("{}/.cargo/bin/lmdeck", f.home));
        }
        let last = out.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        println!("    {} cargo install failed: {last}", yellow("!"));
        return None;
    }
    println!(
        "    {} {} is {} {} and has no cargo. Install Rust there (https://rustup.rs), then `lmdeck setup` builds it with `cargo install --git {REPO}`",
        yellow("!"),
        f.host,
        f.os,
        f.arch
    );
    None
}

/// Turns on linger so the user service starts at boot. Tries without sudo
/// first; else asks for the sudo password once, without echo.
fn linger(p: &mut dyn Prompt, ssh: &mut dyn Ssh, ip: &str, name: &str, user: &str) {
    if ssh.run(ip, "loginctl enable-linger", None).ok && linger_on(ssh, ip) {
        println!("    {} linger on: it starts at boot, before anyone logs in", green("✓"));
        return;
    }
    if !user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) || user.is_empty() {
        return;
    }
    println!(
        "    {}",
        dim(&format!("to start at boot without a login, {name} needs `loginctl enable-linger`, which needs sudo."))
    );
    let Some(pw) = p.secret(&format!("    sudo password for {user}@{name} (not shown, not saved; empty skips) > "))
    else {
        return;
    };
    let mut input = pw.into_bytes();
    while input.last().is_some_and(|b| matches!(b, b'\n' | b'\r')) {
        input.pop();
    }
    if input.is_empty() {
        println!("    {}", dim(&format!("skipped. later, on {name}: sudo loginctl enable-linger {user}")));
        return;
    }
    input.push(b'\n');
    let out = ssh.run(ip, &format!("sudo -S -p '' loginctl enable-linger {user}"), Some(&input));
    input.fill(0);
    if out.ok && linger_on(ssh, ip) {
        println!("    {} linger on: it starts at boot", green("✓"));
    } else {
        println!(
            "    {} sudo didn't work; it runs now but not after a reboot. Later, on {name}: sudo loginctl enable-linger {user}",
            yellow("!")
        );
    }
}

fn linger_on(ssh: &mut dyn Ssh, ip: &str) -> bool {
    let out = ssh.run(ip, "loginctl show-user \"$(id -un)\" -p Linger --value", None);
    out.ok && out.stdout.trim() == "yes"
}

const TOKEN_CMD: &str =
    "umask 077 && mkdir -p \"$HOME/.config/lmdeck\" && cat > \"$HOME/.config/lmdeck/serve.env\" && chmod 600 \"$HOME/.config/lmdeck/serve.env\"";

const COPY_CMD: &str = "mkdir -p \"$HOME/.local/bin\" && cat > \"$HOME/.local/bin/lmdeck.new\" && chmod 755 \"$HOME/.local/bin/lmdeck.new\" && mv -f \"$HOME/.local/bin/lmdeck.new\" \"$HOME/.local/bin/lmdeck\" && \"$HOME/.local/bin/lmdeck\" --version";

const SERVICE_CMD: &str = "mkdir -p \"$HOME/.config/systemd/user\" && cat > \"$HOME/.config/systemd/user/lmdeck-serve.service\" && systemctl --user daemon-reload && systemctl --user enable lmdeck-serve.service >/dev/null 2>&1 && systemctl --user restart lmdeck-serve.service";

fn cargo_cmd() -> String {
    format!(
        ". \"$HOME/.cargo/env\" 2>/dev/null; mkdir -p \"$HOME/.cache\"; cargo install --locked --git {REPO} lmdeck > \"$HOME/.cache/lmdeck-install.log\" 2>&1 || {{ tail -n 5 \"$HOME/.cache/lmdeck-install.log\" >&2; exit 1; }}"
    )
}

/// `lmdeck serve` detached from the ssh session, token from serve.env.
fn background_cmd(exec: &str) -> String {
    format!(
        "mkdir -p \"$HOME/.local/state/lmdeck\" && set -a && . \"$HOME/.config/lmdeck/serve.env\" && set +a && nohup setsid {exec} > \"$HOME/.local/state/lmdeck/serve.log\" 2>&1 < /dev/null &"
    )
}

/// The user unit: the token comes from serve.env, never the command line.
/// `Restart=always` also covers a boot where the CX7 address isn't up yet.
pub fn unit_file(exec: &str, home: &str) -> String {
    format!(
        "[Unit]\nDescription=lmdeck serve (ConnectX-7)\nAfter=network.target\n\n[Service]\nEnvironmentFile=%h/.config/lmdeck/serve.env\nEnvironment=PATH={home}/.local/bin:{home}/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\nExecStart={exec}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
    )
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no output").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::tests::{MockSsh, Script};
    use crate::setup::{Candidate, Via};
    use crate::ssh::Out;

    const IP: &str = "10.100.136.1";

    fn peer() -> Peer {
        Peer {
            name: None,
            named_by: "",
            addrs: vec![Candidate { ip: IP.into(), dev: "cx".into(), via: Via::Routed }],
            login: Login::Denied,
            serving: None,
        }
    }

    fn facts(arch: &str, extra: &str) -> String {
        format!("host=faraday.lan\narch={arch}\nos=Linux\nuser=jvr0x\nhome=/home/jvr0x\nlmdeck=\n{extra}")
    }

    #[test]
    fn facts_parse() {
        let f = parse_facts(
            "host=ember.local\narch=aarch64\nos=Linux\nuser=me\nhome=/home/me\nlmdeck=/home/me/.cargo/bin/lmdeck\nversion=lmdeck 0.1.0\ncargo=yes\nsystemd=yes\n",
        );
        assert_eq!(f.host, "ember");
        assert_eq!(f.lmdeck.as_deref(), Some("/home/me/.cargo/bin/lmdeck"));
        assert_eq!(f.version.as_deref(), Some("0.1.0"));
        assert!(f.cargo && f.systemd_user && !f.linger);
        assert_eq!(parse_facts("lmdeck=\n").lmdeck, None);
    }

    #[test]
    fn copies_the_key_installs_starts_a_service_and_keeps_the_password_out_of_sight() {
        let mut ssh = MockSsh { key_exists: true, copy_ok: true, ..Default::default() };
        ssh.reply(IP, FACTS_CMD, Out::fail("jvr0x@10.100.136.1: Permission denied (publickey,password)."));
        ssh.reply(IP, FACTS_CMD, Out::ok(&facts(std::env::consts::ARCH, "systemd=yes\n")));
        ssh.reply(IP, "lmdeck.new", Out::ok("lmdeck 0.1.0\n"));
        ssh.reply(IP, "serve.env", Out::ok(""));
        ssh.reply(IP, "lmdeck-serve.service", Out::ok(""));
        ssh.reply(IP, "sudo -S", Out::ok(""));
        ssh.reply(IP, "loginctl enable-linger", Out::fail("Interactive authentication required."));
        ssh.reply(IP, "show-user", Out::ok("yes\n"));
        let mut p = Script::new(&["", "", "hunter2"]); // copy the key, service, sudo password
        let mut asked = vec![];
        let r = bootstrap(&mut p, &mut ssh, &peer(), 7979, &mut |ip, port| {
            asked.push(format!("{ip}:{port}"));
            true
        })
        .expect("ready");
        assert_eq!(r.name, "faraday");
        assert_eq!(r.ip, IP);
        assert!(r.answering);
        assert_eq!(asked, vec![format!("{IP}:7979")]);
        assert_eq!(ssh.copied, vec![IP.to_string()]);
        let unit = ssh.calls.iter().find(|(_, c, _)| c.contains("lmdeck-serve.service")).unwrap().2.clone().unwrap();
        assert!(
            unit.contains(&format!("ExecStart=/home/jvr0x/.local/bin/lmdeck serve --host {IP} --port 7979")),
            "{unit}"
        );
        assert!(unit.contains("EnvironmentFile=%h/.config/lmdeck/serve.env"));
        assert!(!unit.contains(&r.token), "the token stays out of the unit");
        // The password only ever goes to sudo's stdin.
        let sudo = ssh.calls.iter().find(|(_, c, _)| c.contains("sudo -S")).unwrap();
        assert_eq!(sudo.2.as_deref(), Some("hunter2\n"));
        assert!(!sudo.1.contains("hunter2"));
        // (The binary copy is skipped: in tests that binary is this test, which holds the literal.)
        let elsewhere = ssh
            .calls
            .iter()
            .filter(|(_, c, _)| !c.contains("sudo -S") && !c.contains("lmdeck.new"))
            .any(|(_, c, s)| c.contains("hunter2") || s.as_deref().unwrap_or("").contains("hunter2"));
        assert!(!elsewhere);

        // Every call went to the CX7 address.
        assert!(ssh.calls.iter().all(|(a, _, _)| a == IP));
    }

    #[test]
    fn another_cpu_builds_with_cargo_and_no_systemd_starts_in_the_background() {
        let other = if std::env::consts::ARCH == "aarch64" { "x86_64" } else { "aarch64" };
        let mut ssh = MockSsh::default();
        ssh.reply(IP, FACTS_CMD, Out::ok(&facts(other, "cargo=yes\nlinger=yes\n")));
        ssh.reply(IP, "cargo install", Out::ok(""));
        ssh.reply(IP, "serve.env", Out::ok(""));
        ssh.reply(IP, "nohup setsid", Out::ok(""));
        let mut p = Script::new(&[]);
        let r = bootstrap(&mut p, &mut ssh, &peer(), 7979, &mut |_, _| true).expect("ready");
        assert_eq!(r.name, "faraday");
        let cargo = ssh.calls.iter().find(|(_, c, _)| c.contains("cargo install")).unwrap();
        assert!(cargo.1.contains("--git https://github.com/javierbits/lmdeck"));
        assert!(!ssh.calls.iter().any(|(_, c, _)| c.contains("lmdeck.new")), "no binary copy across CPUs");
        let bg = ssh.calls.iter().find(|(_, c, _)| c.contains("nohup")).unwrap();
        assert!(bg.1.contains(&format!("/home/jvr0x/.cargo/bin/lmdeck serve --host {IP} --port 7979")));
        assert!(!bg.1.contains(&r.token));
    }

    #[test]
    fn no_cargo_and_another_cpu_stops_with_a_hint() {
        let other = if std::env::consts::ARCH == "aarch64" { "x86_64" } else { "aarch64" };
        let mut ssh = MockSsh::default();
        ssh.reply(IP, FACTS_CMD, Out::ok(&facts(other, "")));
        assert_eq!(bootstrap(&mut Script::new(&[]), &mut ssh, &peer(), 7979, &mut |_, _| true), None);
        assert!(!ssh.calls.iter().any(|(_, c, _)| c.contains("serve")));
    }

    #[test]
    fn a_changed_host_key_or_a_declined_copy_stops_before_anything_runs() {
        let mut ssh = MockSsh::default();
        ssh.reply(IP, FACTS_CMD, Out::fail("Host key verification failed."));
        assert_eq!(bootstrap(&mut Script::new(&[]), &mut ssh, &peer(), 7979, &mut |_, _| true), None);
        assert!(ssh.copied.is_empty());

        let mut ssh = MockSsh { key_exists: true, ..Default::default() };
        ssh.reply(IP, FACTS_CMD, Out::fail("Permission denied (publickey)."));
        assert_eq!(bootstrap(&mut Script::new(&["n"]), &mut ssh, &peer(), 7979, &mut |_, _| true), None);
        assert!(ssh.copied.is_empty());
        assert_eq!(ssh.calls.len(), 1);
    }

    #[test]
    fn an_installed_lmdeck_of_this_version_is_reused() {
        let mut ssh = MockSsh::default();
        let f = format!(
            "host=ember\narch={}\nos=Linux\nuser=jvr0x\nhome=/home/jvr0x\nlmdeck=/home/jvr0x/.cargo/bin/lmdeck\nversion=lmdeck {}\nsystemd=yes\nlinger=yes\n",
            std::env::consts::ARCH,
            env!("CARGO_PKG_VERSION")
        );
        ssh.reply(IP, FACTS_CMD, Out::ok(&f));
        ssh.reply(IP, "serve.env", Out::ok(""));
        ssh.reply(IP, "lmdeck-serve.service", Out::ok(""));
        let r = bootstrap(&mut Script::new(&[""]), &mut ssh, &peer(), 7979, &mut |_, _| true).unwrap();
        assert_eq!(r.name, "ember");
        assert!(!ssh.calls.iter().any(|(_, c, _)| c.contains("lmdeck.new") || c.contains("cargo install")));
        assert!(!ssh.calls.iter().any(|(_, c, _)| c.contains("enable-linger")), "linger is already on");
    }
}
