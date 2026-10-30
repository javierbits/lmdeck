//! Hardware detection for `lmdeck doctor` and recipe compatibility.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::util::run_with_timeout;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Free space in GB for the filesystem `dir` lands on. Walks up to the nearest
/// existing ancestor, so a models directory that has not been created yet
/// reports the filesystem it would be made on. `None` when nothing exists
/// (`/` always does) or the command is missing.
pub fn free_gb(dir: &Path) -> Option<f64> {
    let mut at = dir.to_path_buf();
    loop {
        if at.exists() {
            break;
        }
        if !at.pop() {
            return None;
        }
    }
    // `df -Pk` prints 1024-byte blocks; the 4th field is available.
    let out = run_with_timeout(Command::new("df").arg("-Pk").arg(&at), Duration::from_secs(4))?;
    let line = out.lines().nth(1)?;
    let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb as f64 * 1024.0 / GIB)
}

/// One GPU as reported by the driver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    pub vendor: String,
    pub name: String,
    /// Dedicated memory in GB, `None` when the GPU shares system memory.
    pub memory_gb: Option<f64>,
    pub compute_capability: Option<String>,
    pub driver: Option<String>,
}

/// What lmdeck knows about the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hardware {
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub cpu: String,
    pub cores: usize,
    pub total_memory_gb: f64,
    pub available_memory_gb: Option<f64>,
    pub gpus: Vec<Gpu>,
    /// GPU and CPU share one memory pool (GB10 / DGX Spark, Apple Silicon).
    pub unified_memory: bool,
    /// Friendly platform label, e.g. "NVIDIA GB10 (DGX Spark class)".
    pub platform: String,
}

impl Hardware {
    /// Accelerator family: `nvidia`, `apple`, `amd` or `cpu`.
    pub fn accelerator(&self) -> &str {
        match self.gpus.first() {
            Some(g) => g.vendor.as_str(),
            None => "cpu",
        }
    }

    /// Memory a model can actually be loaded into, in GB.
    pub fn model_memory_gb(&self) -> f64 {
        if self.unified_memory || self.gpus.is_empty() {
            return self.total_memory_gb;
        }
        let vram: f64 = self.gpus.iter().filter_map(|g| g.memory_gb).sum();
        if vram > 0.0 {
            vram
        } else {
            self.total_memory_gb
        }
    }

    /// Lowest compute capability across NVIDIA GPUs.
    pub fn compute_capability(&self) -> Option<String> {
        self.gpus.iter().filter_map(|g| g.compute_capability.clone()).min_by(|a, b| cmp_version(a, b))
    }

    /// Detects the local machine. Never fails: unknowns are left empty.
    pub fn detect() -> Hardware {
        let os = normalize_os(std::env::consts::OS);
        let arch = std::env::consts::ARCH.to_string();
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let hostname = cmd_out("hostname", &[]).unwrap_or_default();
        let (total, avail, cpu) = if os == "macos" {
            let total = cmd_out("sysctl", &["-n", "hw.memsize"])
                .and_then(|s| s.parse::<f64>().ok())
                .map(|b| b / GIB)
                .unwrap_or(0.0);
            let avail = cmd_out("vm_stat", &[]).and_then(|s| parse_vm_stat(&s));
            let cpu = cmd_out("sysctl", &["-n", "machdep.cpu.brand_string"]).unwrap_or_default();
            (total, avail, cpu)
        } else {
            let mem = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| parse_meminfo(&s));
            let cpu = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| parse_cpuinfo(&s)).unwrap_or_default();
            (mem.map(|m| m.0).unwrap_or(0.0), mem.map(|m| m.1), cpu)
        };
        let mut gpus = vec![];
        if let Some(out) = run_with_timeout(
            Command::new("nvidia-smi")
                .args(["--query-gpu=name,memory.total,compute_cap,driver_version", "--format=csv,noheader,nounits"]),
            Duration::from_secs(8),
        ) {
            gpus = parse_nvidia_smi(&out);
        }
        if gpus.is_empty() && os == "macos" && arch == "aarch64" {
            gpus.push(Gpu {
                vendor: "apple".into(),
                name: if cpu.is_empty() { "Apple Silicon".into() } else { cpu.clone() },
                memory_gb: None,
                compute_capability: None,
                driver: None,
            });
        }
        if gpus.is_empty() && Path::new("/dev/kfd").exists() {
            gpus.push(Gpu {
                vendor: "amd".into(),
                name: "AMD GPU (ROCm)".into(),
                memory_gb: None,
                compute_capability: None,
                driver: None,
            });
        }
        Hardware::assemble(os, arch, hostname, cpu, cores, total, avail, gpus)
    }

    /// Derives the unified-memory flag and platform label.
    #[allow(clippy::too_many_arguments)]
    pub fn assemble(
        os: String,
        arch: String,
        hostname: String,
        cpu: String,
        cores: usize,
        total_memory_gb: f64,
        available_memory_gb: Option<f64>,
        gpus: Vec<Gpu>,
    ) -> Hardware {
        let gb10 = gpus.iter().any(|g| g.name.to_uppercase().contains("GB10"));
        let apple = gpus.iter().any(|g| g.vendor == "apple");
        // An NVIDIA GPU that reports no dedicated memory shares system RAM.
        let shared_nv = gpus.iter().any(|g| g.vendor == "nvidia" && g.memory_gb.is_none());
        let unified = gb10 || apple || shared_nv;
        let platform = if gb10 {
            "NVIDIA GB10 (DGX Spark class)".to_string()
        } else if apple {
            "Apple Silicon".to_string()
        } else if let Some(g) = gpus.first() {
            if gpus.len() > 1 {
                format!("{}x {}", gpus.len(), g.name)
            } else {
                g.name.clone()
            }
        } else {
            "CPU only".to_string()
        };
        Hardware {
            os,
            arch,
            hostname,
            cpu,
            cores,
            total_memory_gb,
            available_memory_gb,
            gpus,
            unified_memory: unified,
            platform,
        }
    }
}

pub fn normalize_os(os: &str) -> String {
    match os {
        "darwin" | "macos" | "osx" | "mac" => "macos".into(),
        other => other.to_lowercase(),
    }
}

pub fn normalize_arch(arch: &str) -> String {
    match arch.to_lowercase().as_str() {
        "arm64" | "aarch64" => "aarch64".into(),
        "amd64" | "x86_64" | "x64" => "x86_64".into(),
        other => other.into(),
    }
}

fn cmd_out(bin: &str, args: &[&str]) -> Option<String> {
    run_with_timeout(Command::new(bin).args(args), Duration::from_secs(5))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `(total_gb, available_gb)` from `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> Option<(f64, f64)> {
    let field = |key: &str| -> Option<f64> {
        text.lines()
            .find(|l| l.starts_with(key) && l[key.len()..].starts_with(':'))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<f64>().ok())
            .map(|kb| kb * 1024.0 / GIB)
    };
    let total = field("MemTotal")?;
    let avail = field("MemAvailable").or_else(|| field("MemFree")).unwrap_or(0.0);
    Some((total, avail))
}

/// Free + inactive + speculative pages from macOS `vm_stat`, in GB.
pub fn parse_vm_stat(text: &str) -> Option<f64> {
    let page = text
        .lines()
        .next()
        .and_then(|l| l.split("page size of ").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(16384.0);
    let pages = |key: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().trim_end_matches('.').parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let total = pages("Pages free") + pages("Pages inactive") + pages("Pages speculative");
    (total > 0.0).then_some(total * page / GIB)
}

pub fn parse_cpuinfo(text: &str) -> Option<String> {
    text.lines()
        .find(|l| l.starts_with("model name") || l.starts_with("Model"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
}

/// Parses `nvidia-smi --query-gpu=name,memory.total,compute_cap,driver_version
/// --format=csv,noheader,nounits`. GB10 reports memory as `[N/A]`.
pub fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    let na = |s: &str| {
        let s = s.trim();
        if s.is_empty() || s.contains("N/A") || s.contains("Not Supported") {
            None
        } else {
            Some(s.to_string())
        }
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.split(',').collect();
            let name = na(cols.first()?)?;
            Some(Gpu {
                vendor: "nvidia".into(),
                name,
                memory_gb: cols.get(1).and_then(|m| na(m)).and_then(|m| m.parse::<f64>().ok()).map(|mib| mib / 1024.0),
                compute_capability: cols.get(2).and_then(|c| na(c)),
                driver: cols.get(3).and_then(|d| na(d)),
            })
        })
        .collect()
}

/// Compares dotted versions numerically (`"8.9" < "12.1"`).
pub fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect() };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let o = a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0));
        if o.is_ne() {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

/// Finds an executable on `$PATH` (or checks an explicit path).
pub fn which(bin: &str) -> Option<PathBuf> {
    let expanded = crate::paths::expand(bin);
    if bin.contains('/') {
        return is_executable(&expanded).then_some(expanded);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gb10_nvidia_smi() {
        let out = "NVIDIA GB10, [N/A], 12.1, 580.95.05\n";
        let gpus = parse_nvidia_smi(out);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].memory_gb, None);
        assert_eq!(gpus[0].compute_capability.as_deref(), Some("12.1"));
        let hw = Hardware::assemble(
            "linux".into(),
            "aarch64".into(),
            "spark".into(),
            "".into(),
            20,
            119.7,
            Some(100.0),
            gpus,
        );
        assert!(hw.unified_memory);
        assert_eq!(hw.platform, "NVIDIA GB10 (DGX Spark class)");
        assert_eq!(hw.model_memory_gb(), 119.7);
        assert_eq!(hw.accelerator(), "nvidia");
    }

    #[test]
    fn discrete_gpus_sum_vram() {
        let out = "NVIDIA GeForce RTX 4090, 24564, 8.9, 550.1\nNVIDIA GeForce RTX 4090, 24564, 8.9, 550.1\n";
        let hw = Hardware::assemble(
            "linux".into(),
            "x86_64".into(),
            "h".into(),
            "".into(),
            32,
            64.0,
            None,
            parse_nvidia_smi(out),
        );
        assert!(!hw.unified_memory);
        assert!((hw.model_memory_gb() - 47.98).abs() < 0.01);
        assert_eq!(hw.platform, "2x NVIDIA GeForce RTX 4090");
        assert_eq!(hw.compute_capability().as_deref(), Some("8.9"));
    }

    #[test]
    fn cpu_only_uses_ram() {
        let hw = Hardware::assemble("linux".into(), "x86_64".into(), "h".into(), "".into(), 8, 16.0, None, vec![]);
        assert_eq!(hw.accelerator(), "cpu");
        assert_eq!(hw.model_memory_gb(), 16.0);
        assert_eq!(hw.platform, "CPU only");
    }

    #[test]
    fn parses_meminfo_and_vm_stat() {
        let m = "MemTotal:       125515524 kB\nMemFree:        1000 kB\nMemAvailable:   104857600 kB\n";
        let (t, a) = parse_meminfo(m).unwrap();
        assert!((t - 119.7).abs() < 0.1);
        assert!((a - 100.0).abs() < 0.01);
        let v = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               65536.\nPages active:  1.\nPages inactive:                           65536.\nPages speculative:                        0.\n";
        assert!((parse_vm_stat(v).unwrap() - 2.0).abs() < 0.001);
    }

    #[test]
    fn version_ordering() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_version("8.9", "12.1"), Less);
        assert_eq!(cmp_version("12.1", "12.1"), Equal);
        assert_eq!(cmp_version("10", "9.0"), Greater);
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_arch("arm64"), "aarch64");
        assert_eq!(normalize_arch("amd64"), "x86_64");
        assert_eq!(normalize_os("darwin"), "macos");
    }
}
