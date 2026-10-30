//! The setup wizard logs in to other Sparks with a key it can actually use.
//!
//! lmdeck runs ssh in BatchMode, so a key with a passphrase is useless to it,
//! and ssh only offers the keys `~/.ssh/config` names for an address. On a
//! real cluster, faraday already had spark's cluster key, but spark's config
//! offered other keys for faraday's CX7 address: every login was refused, and
//! `ssh-copy-id` (which picks the newest `~/.ssh/id*.pub` and found it
//! installed) couldn't help. These tests pin down the fix: when ssh's own
//! choice is refused, try each passphrase-free key and keep the one that works.

use lmdeck::ssh::{usable_keys, Login, Out, Ssh};
use proptest::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const IP: &str = "10.100.134.2";

/// What sshd says when no offered key is in its authorized_keys.
fn denied() -> Out {
    Out::fail("jvr0x@10.100.134.2: Permission denied (publickey,password).")
}

/// Key paths from bare names.
fn paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(PathBuf::from).collect()
}

/// A Spark on the other end of the cable, as ssh on this machine sees it.
#[derive(Default)]
struct Peer {
    /// What ssh gets with its own choice of keys (config / agent); `None` is refused.
    plain: Option<Out>,
    /// This user's passphrase-free keys, as `keys` reports them.
    keys: Vec<PathBuf>,
    /// The keys in the peer's `~/.ssh/authorized_keys`.
    authorized: Vec<PathBuf>,
    /// The key lmdeck names per address (`use_key`).
    using: HashMap<String, PathBuf>,
    /// Every connection: the address and the key lmdeck named, if any.
    calls: Vec<(String, Option<PathBuf>)>,
}

impl Ssh for Peer {
    /// Answers like sshd: ssh's own choice, or the named key if it is authorized.
    fn run(&mut self, addr: &str, _cmd: &str, _stdin: Option<&[u8]>) -> Out {
        let key = self.using.get(addr).cloned();
        self.calls.push((addr.to_string(), key.clone()));
        match key {
            None => self.plain.clone().unwrap_or_else(denied),
            Some(k) if self.authorized.contains(&k) => Out::ok("faraday\n"),
            Some(_) => denied(),
        }
    }

    /// No host keys needed here.
    fn host_key(&mut self, _addr: &str) -> Option<String> {
        None
    }

    /// Authorizes `key` there, as ssh-copy-id would.
    fn copy_id(&mut self, _addr: &str, key: &Path) -> bool {
        self.authorized.push(key.to_path_buf());
        true
    }

    /// The scripted passphrase-free keys.
    fn keys(&mut self, _addr: &str) -> Vec<PathBuf> {
        self.keys.clone()
    }

    /// Never makes one here.
    fn make_key(&mut self) -> Option<PathBuf> {
        None
    }

    /// Names `key` for `addr` from now on, or goes back to ssh's own choice.
    fn use_key(&mut self, addr: &str, key: Option<&Path>) {
        match key {
            Some(k) => self.using.insert(addr.to_string(), k.to_path_buf()),
            None => self.using.remove(addr),
        };
    }
}

/// The key the next connection to `IP` would name.
fn kept(p: &mut Peer) -> Option<PathBuf> {
    p.run(IP, "true", None);
    p.calls.last().and_then(|(_, k)| k.clone())
}

#[test]
fn a_key_already_on_the_peer_is_found_and_kept() {
    // faraday: spark's cluster key is there, but ssh's config offers other keys.
    let mut p = Peer {
        keys: paths(&["spark0", "id_ed25519", "id_ed25519_cluster"]),
        authorized: paths(&["id_ed25519_cluster"]),
        ..Default::default()
    };
    let out = p.login(IP, "hostname", None);
    assert_eq!(Login::from_out(&out), Login::Ok);
    assert_eq!(out.stdout.trim(), "faraday");
    assert_eq!(kept(&mut p), Some(PathBuf::from("id_ed25519_cluster")), "later commands use the key that worked");
}

#[test]
fn when_ssh_already_logs_in_no_key_is_tried() {
    let mut p = Peer { plain: Some(Out::ok("gigabyte\n")), keys: paths(&["id_ed25519"]), ..Default::default() };
    assert_eq!(Login::from_out(&p.login(IP, "hostname", None)), Login::Ok);
    assert_eq!(p.calls, vec![(IP.to_string(), None)], "one connection, with ssh's own choice");
}

#[test]
fn refused_with_every_key_stays_denied_and_names_none() {
    let mut p = Peer { keys: paths(&["id_ed25519", "spark0"]), ..Default::default() };
    assert_eq!(Login::from_out(&p.login(IP, "hostname", None)), Login::Denied);
    assert_eq!(p.calls.len(), 3, "ssh's own choice, then each key once");
    assert_eq!(kept(&mut p), None, "a refused key doesn't stay in use");
}

#[test]
fn only_a_refused_login_is_retried_with_keys() {
    let fails = [
        Out::fail("Host key verification failed."),
        Out::fail("ssh: connect to host 10.100.134.2 port 22: Connection timed out"),
    ];
    for plain in fails {
        let mut p = Peer {
            plain: Some(plain.clone()),
            keys: paths(&["id_ed25519"]),
            authorized: paths(&["id_ed25519"]),
            ..Default::default()
        };
        assert_eq!(p.login(IP, "hostname", None), plain);
        assert_eq!(p.calls.len(), 1, "a key can't fix {:?}", plain.stderr);
    }
}

/// Makes an ed25519 key pair at `dir/name`, with `passphrase` ("" for none).
fn keygen(dir: &Path, name: &str, passphrase: &str) -> PathBuf {
    let path = dir.join(name);
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", passphrase, "-f"])
        .arg(&path)
        .status()
        .expect("ssh-keygen runs")
        .success();
    assert!(made, "ssh-keygen made {name}");
    path
}

#[test]
fn usable_keys_skip_passphrases_and_keys_without_a_pub_and_put_offered_ones_first() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let default = keygen(d, "id_ed25519", "");
    let cluster = keygen(d, "id_ed25519_cluster", "");
    let personal = keygen(d, "id_ed25519_personal", "secret"); // BatchMode can't type it
    let spark0 = keygen(d, "spark0", ""); // any name with a .pub is a key
    let lone = keygen(d, "id_ecdsa_lone", "");
    std::fs::remove_file(d.join("id_ecdsa_lone.pub")).unwrap(); // nothing for ssh-copy-id to send
    for other in ["config", "known_hosts", "authorized_keys"] {
        std::fs::write(d.join(other), "").unwrap();
    }
    // ssh's config offers these for the address, in this order.
    let offered = [personal, spark0.clone(), d.join("missing"), lone];
    assert_eq!(usable_keys(d, &offered), vec![spark0, default, cluster]);
}

#[test]
fn usable_keys_of_an_empty_or_missing_dir_are_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(usable_keys(dir.path(), &[]).is_empty());
    assert!(usable_keys(&dir.path().join("nope"), &[dir.path().join("nope/id_ed25519")]).is_empty());
}

proptest! {
    /// Login works exactly when ssh's own choice or one of the keys is
    /// accepted, costs at most one connection per key, and keeps that key.
    #[test]
    fn login_tries_each_key_once_and_keeps_the_one_that_works(
        n in 0usize..6,
        accepted in prop::option::of(0usize..6),
        plain_ok in any::<bool>(),
    ) {
        let keys: Vec<PathBuf> = (0..n).map(|i| PathBuf::from(format!("k{i}"))).collect();
        let authorized: Vec<PathBuf> = accepted.filter(|&a| a < n).map(|a| keys[a].clone()).into_iter().collect();
        let mut p = Peer {
            plain: plain_ok.then(|| Out::ok("faraday\n")),
            keys,
            authorized: authorized.clone(),
            ..Default::default()
        };
        let ok = Login::from_out(&p.login(IP, "hostname", None)) == Login::Ok;
        prop_assert_eq!(ok, plain_ok || !authorized.is_empty());
        prop_assert!(p.calls.len() <= 1 + n);
        let want = if plain_ok { None } else { authorized.first().cloned() };
        prop_assert_eq!(kept(&mut p), want);
    }
}
