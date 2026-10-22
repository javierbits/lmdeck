//! Small shared helpers.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs a command, returning its stdout when it exits 0 within `timeout`.
/// Missing binaries, failures and timeouts all yield `None`.
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output().ok()?;
                return status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Bytes → `2.3G` style (binary units, like lmswitch).
pub fn human_bytes(n: u64) -> String {
    if n == 0 {
        return "-".into();
    }
    let gib = n as f64 / 1024f64.powi(3);
    if gib >= 1000.0 {
        format!("{:.1}T", gib / 1024.0)
    } else if gib >= 0.1 {
        format!("{gib:.1}G")
    } else {
        format!("{:.0}M", n as f64 / 1024f64.powi(2))
    }
}

/// GB (decimal-ish float) → `24.0G`.
pub fn human_gb(gb: f64) -> String {
    if gb <= 0.0 {
        "-".into()
    } else if gb >= 1000.0 {
        format!("{:.1}T", gb / 1024.0)
    } else {
        format!("{gb:.1}G")
    }
}

/// Total size of a file, or of every regular file under a directory.
/// Symlinked files are followed (the Hugging Face cache links snapshots to
/// blobs); symlinked directories are not. Bounded depth and entry count, so a
/// careless `path = "/"` stays cheap.
pub fn path_size(p: &Path) -> u64 {
    const MAX_ENTRIES: usize = 50_000;
    fn walk(p: &Path, depth: usize, total: &mut u64, seen: &mut usize) {
        if depth > 8 || *seen > MAX_ENTRIES {
            return;
        }
        let Ok(rd) = fs::read_dir(p) else { return };
        for e in rd.flatten() {
            *seen += 1;
            if *seen > MAX_ENTRIES {
                return;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                walk(&e.path(), depth + 1, total, seen);
            } else if ft.is_file() {
                *total += e.metadata().map(|m| m.len()).unwrap_or(0);
            } else if ft.is_symlink() {
                if let Ok(m) = fs::metadata(e.path()) {
                    if m.is_file() {
                        *total += m.len();
                    }
                }
            }
        }
    }
    match fs::metadata(p) {
        Ok(m) if m.is_file() => m.len(),
        Ok(m) if m.is_dir() => {
            let mut total = 0;
            walk(p, 0, &mut total, &mut 0);
            total
        }
        _ => 0,
    }
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `3h12m` style uptime.
pub fn human_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_bytes(0), "-");
        assert_eq!(human_bytes(2_497_281_312), "2.3G");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5M");
        assert_eq!(human_bytes(2 * 1024u64.pow(4)), "2.0T");
        assert_eq!(human_gb(119.7), "119.7G");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(human_duration(5), "5s");
        assert_eq!(human_duration(125), "2m05s");
        assert_eq!(human_duration(3 * 3600 + 120), "3h02m");
        assert_eq!(human_duration(2 * 86400 + 3600), "2d1h");
    }

    #[test]
    fn runs_commands_with_timeout() {
        assert_eq!(run_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(5)).as_deref(), Some("hi\n"));
        assert!(run_with_timeout(&mut Command::new("false"), Duration::from_secs(5)).is_none());
        assert!(run_with_timeout(&mut Command::new("definitely-not-a-binary-xyz"), Duration::from_secs(1)).is_none());
        assert!(run_with_timeout(Command::new("sleep").arg("5"), Duration::from_millis(200)).is_none());
    }

    #[test]
    fn sizes_directories() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a"), vec![0u8; 10]).unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        fs::write(d.path().join("sub/b"), vec![0u8; 5]).unwrap();
        assert_eq!(path_size(d.path()), 15);
        assert_eq!(path_size(&d.path().join("missing")), 0);
    }
}
