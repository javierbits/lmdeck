//! Managed inference-engine installs. Today this is llama.cpp: a pinned
//! release archive downloaded into lmdeck's cache, checksum-verified, and
//! unpacked next to its shared libraries. Nothing is installed system-wide
//! and no `sudo` is involved.
//!
//! The release tag and digests are pinned so the same archive is fetched on
//! every machine. Bump [`LLAMA_CPP_TAG`] and the four digests together.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{copy, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::paths::Paths;

/// Pinned llama.cpp release. The assets are built by the upstream release
/// workflow and published on GitHub.
pub const LLAMA_CPP_TAG: &str = "b12318";

/// One release archive for this platform.
pub struct Asset {
    pub os: &'static str,
    pub arch: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

const ASSETS: &[Asset] = &[
    Asset {
        os: "linux",
        arch: "x86_64",
        file: "llama-b12318-bin-ubuntu-x64.tar.gz",
        sha256: "092ee3cb21574ff3a451557f7dd45a815a5c8fd1bc62923b48c916d04c5e4443",
        size: 23_311_520,
    },
    Asset {
        os: "linux",
        arch: "aarch64",
        file: "llama-b12318-bin-ubuntu-arm64.tar.gz",
        sha256: "38b74378619839dcced1982363a3e74f33978eaaed54f0a4a68fd9043cc85a68",
        size: 18_952_173,
    },
    Asset {
        os: "macos",
        arch: "aarch64",
        file: "llama-b12318-bin-macos-arm64.tar.gz",
        sha256: "c8de54b88616af7f76911608905ab4fd2e2343aefcdf997b4823f73ea7341f53",
        size: 16_896_103,
    },
    Asset {
        os: "macos",
        arch: "x86_64",
        file: "llama-b12318-bin-macos-x64.tar.gz",
        sha256: "b318e1c1882a0c975a6bfa2212ae5ed01588e59d1d99b94b078abaebbe221b03",
        size: 12_639_522,
    },
];

/// The archive for a platform, or `None` when lmdeck has no managed build.
pub fn asset_for(os: &str, arch: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.os == os && a.arch == arch)
}

pub fn current_asset() -> Result<&'static Asset> {
    asset_for(std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
        format!(
            "no managed llama.cpp build for {} {}; set engines.llama_server to an existing llama-server",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

pub fn managed_dir(paths: &Paths) -> PathBuf {
    paths.cache_dir.join("engines").join("llama.cpp").join(LLAMA_CPP_TAG)
}

pub fn managed_binary(paths: &Paths) -> PathBuf {
    managed_dir(paths).join("llama-server")
}

/// A managed llama.cpp install, for engine detection (`find_llama_server`).
pub fn managed_llama_server() -> Option<PathBuf> {
    let paths = Paths::from_env();
    let bin = managed_binary(&paths);
    bin.is_file().then_some(bin)
}

fn release_url(asset: &Asset) -> String {
    format!("https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_CPP_TAG}/{}", asset.file)
}

/// Downloads, verifies and unpacks llama.cpp into the lmdeck cache. Returns
/// the installed `llama-server` path.
pub fn install_llama_cpp(paths: &Paths, force: bool, say: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    let bin = managed_binary(paths);
    if bin.is_file() && !force {
        say(&format!("llama.cpp {LLAMA_CPP_TAG} is already installed at {}", bin.display()));
        return Ok(bin);
    }

    let asset = current_asset()?;
    let dir = managed_dir(paths);
    let downloads = paths.cache_dir.join("engines").join("downloads");
    fs::create_dir_all(&downloads)?;
    let archive = downloads.join(asset.file);

    if !(archive.is_file() && sha256_file(&archive).is_ok_and(|h| h == asset.sha256)) {
        say(&format!("downloading llama.cpp {LLAMA_CPP_TAG} ({} MB)…", asset.size / 1_000_000));
        download(&release_url(asset), &archive)?;
    }
    let got = sha256_file(&archive)?;
    if got != asset.sha256 {
        let _ = fs::remove_file(&archive);
        bail!("llama.cpp archive checksum mismatch (expected {}, got {})", asset.sha256, got);
    }
    say("checksum ok");

    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dir.with_file_name(format!(".unpack-{}", std::process::id()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    unpack(&archive, &tmp)?;
    let root = install_root(&tmp)?;
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::rename(&root, &dir).with_context(|| format!("moving {} to {}", root.display(), dir.display()))?;
    let _ = fs::remove_dir_all(&tmp);

    let installed = managed_binary(paths);
    if !installed.is_file() {
        bail!("llama.cpp archive did not contain llama-server");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&installed)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&installed, perms)?;
    }
    say(&format!("installed {}", installed.display()));
    Ok(installed)
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(300))
        .user_agent(concat!("lmdeck/", env!("CARGO_PKG_VERSION")))
        .build();
    let resp = agent.get(url).call().with_context(|| format!("downloading {url}"))?;
    let tmp = dest.with_extension("part");
    let mut out = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    copy(&mut resp.into_reader(), &mut out).with_context(|| format!("writing {}", tmp.display()))?;
    out.sync_all()?;
    fs::rename(&tmp, dest).with_context(|| format!("moving {} to {}", tmp.display(), dest.display()))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(dest).with_context(|| format!("unpacking {}", archive.display()))?;
    Ok(())
}

/// The directory to move into place: the archive's single top-level directory
/// when it has one, else the directory holding `llama-server`.
fn install_root(tmp: &Path) -> Result<PathBuf> {
    let dirs: Vec<PathBuf> = fs::read_dir(tmp)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    if dirs.len() == 1 {
        return Ok(dirs[0].clone());
    }
    find_binary(tmp, "llama-server")
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .with_context(|| format!("{} does not contain llama-server", tmp.display()))
}

fn find_binary(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_binary(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == OsStr::new(name)) {
            return Some(path);
        }
    }
    None
}

/// The managed binary answers `--version`; used by `engine install` to prove
/// the unpacked libraries load.
pub fn version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    text.lines().next().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_archive_for_each_release_target() {
        assert_eq!(asset_for("linux", "x86_64").unwrap().file, "llama-b12318-bin-ubuntu-x64.tar.gz");
        assert_eq!(asset_for("linux", "aarch64").unwrap().file, "llama-b12318-bin-ubuntu-arm64.tar.gz");
        assert_eq!(asset_for("macos", "aarch64").unwrap().file, "llama-b12318-bin-macos-arm64.tar.gz");
        assert_eq!(asset_for("macos", "x86_64").unwrap().file, "llama-b12318-bin-macos-x64.tar.gz");
        assert!(asset_for("windows", "x86_64").is_none());
    }

    #[test]
    fn unpacks_the_release_layout_and_finds_the_server() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("llama.tar.gz");
        {
            let f = File::create(&archive).unwrap();
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let body = b"#!/bin/sh\necho llama-server\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "llama-b12318/llama-server", &body[..]).unwrap();
            tar.finish().unwrap();
        }
        let tmp = d.path().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        unpack(&archive, &tmp).unwrap();
        let root = install_root(&tmp).unwrap();
        assert_eq!(root.file_name().unwrap(), "llama-b12318");
        assert!(root.join("llama-server").is_file());
    }
}
