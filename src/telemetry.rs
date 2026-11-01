//! Live machine telemetry: memory, CPU load and temperature, GPU temperature,
//! power and utilization, and per-model memory and tokens/s.
//!
//! Every value comes from a real probe, or is `None`. Sources, in order:
//! - memory: `/proc/meminfo` (on GB10 this *is* GPU memory: it's unified and
//!   `nvidia-smi` reports `[N/A]`), `sysctl`/`vm_stat` on macOS
//! - CPU: `/proc/loadavg`, `/proc/stat` deltas, `sysctl vm.loadavg`
//! - temperatures: the `spbm` hwmon (DGX Spark, out-of-tree `spark_hwmon`
//!   driver), CPU hwmon chips, then `/sys/class/thermal`
//! - GPU: `nvidia-smi --query-gpu=…` (fields that read `[N/A]` stay `None`)
//! - system power: `spbm` `sys_total`/`dc_input` rails; on macOS only with
//!   `LMDECK_POWERMETRICS=1` and passwordless `sudo -n powermetrics`
//! - models: process-tree RSS from `ps`, GPU memory from
//!   `nvidia-smi --query-compute-apps`, tokens/s from the Prometheus
//!   `/metrics` of vLLM, SGLang and llama.cpp (`--metrics`)
//!
//! `LMDECK_DEMO=1` swaps all of this for [`demo`] data, flagged `demo: true`
//! everywhere it is shown. It exists for screenshots only.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::hardware::Hardware;
use crate::paths::Paths;
use crate::runner::{probe, Probe, RunRecord, Status};
use crate::util::run_with_timeout;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// How many samples a [`History`] keeps (6 minutes at the 2 s sample rate).
pub const HISTORY_LEN: usize = 180;
/// Sampling period used by `lmdeck serve` and the TUI.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// How often the sampler looks for recipes running outside lmdeck.
pub const ADOPT_EVERY: Duration = Duration::from_secs(10);

/// The catalog's recipes running here without a run record (started by hand,
/// or by an older lmdeck), as [`crate::deck::local_status`] recognises them.
fn find_adopted(paths: &Paths) -> Vec<ModelTel> {
    let cfg = crate::config::Config::load(paths).unwrap_or_default();
    let cat = crate::catalog::Catalog::load(paths, &cfg);
    let ports = crate::deck::planned_ports(&cat, paths, &cfg);
    let mut cache = HashMap::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut out = vec![];
    for f in &cat.entries {
        let r = &f.recipe;
        if !r.nodes.is_empty() || !seen.insert(r.name.clone()) || RunRecord::load(paths, &r.name).is_some() {
            continue;
        }
        let port = crate::deck::planned_port(&ports, f);
        if crate::deck::local_status(paths, None, r, port, &mut cache) == Status::External {
            out.push(ModelTel {
                name: r.name.clone(),
                engine: r.engine.name.as_str().into(),
                port,
                status: Some("external".into()),
                ..Default::default()
            });
        }
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuTel {
    pub index: u32,
    pub name: String,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub util_pct: Option<f64>,
    pub mem_used_gb: Option<f64>,
    pub mem_total_gb: Option<f64>,
    pub sm_clock_mhz: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerRail {
    pub label: String,
    pub watts: f64,
}

/// One running recipe as seen from the machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelTel {
    pub name: String,
    pub engine: String,
    pub port: u16,
    pub pid: Option<u32>,
    /// Resident memory of the server's whole process tree.
    pub rss_gb: Option<f64>,
    /// `rss_gb` as a share of the machine's memory.
    pub mem_share_pct: Option<f64>,
    /// GPU memory from `nvidia-smi --query-compute-apps` (N/A on GB10).
    pub gpu_mem_gb: Option<f64>,
    /// Generated tokens per second over the last sample interval.
    pub tokens_per_s: Option<f64>,
    pub requests_running: Option<f64>,
    /// Where `tokens_per_s` came from, e.g. `vllm /metrics`.
    pub metrics: Option<String>,
    /// `loaded`, `loading`, or `external` (running, but not started by lmdeck).
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Telemetry {
    pub hostname: String,
    pub platform: String,
    pub os: String,
    pub arch: String,
    /// Unix time in milliseconds.
    pub at: u64,
    pub unified_memory: bool,
    pub mem_total_gb: f64,
    pub mem_used_gb: Option<f64>,
    pub mem_available_gb: Option<f64>,
    pub swap_used_gb: Option<f64>,
    /// macOS `kern.memorystatus_level` turned into "percent under pressure".
    pub mem_pressure_pct: Option<f64>,
    pub cpu_cores: usize,
    pub load: Option<[f64; 3]>,
    pub cpu_util_pct: Option<f64>,
    pub cpu_temp_c: Option<f64>,
    pub cpu_temp_source: Option<String>,
    pub system_power_w: Option<f64>,
    pub system_power_source: Option<String>,
    pub gpus: Vec<GpuTel>,
    pub rails: Vec<PowerRail>,
    pub models: Vec<ModelTel>,
    /// Which probes produced data, for the "sources" line.
    pub sources: Vec<String>,
    #[serde(default)]
    pub demo: bool,
}

impl Telemetry {
    pub fn mem_pct(&self) -> Option<f64> {
        let used = self.mem_used_gb?;
        (self.mem_total_gb > 0.0).then(|| 100.0 * used / self.mem_total_gb)
    }
    pub fn gpu_temp(&self) -> Option<f64> {
        self.gpus.iter().filter_map(|g| g.temp_c).reduce(f64::max)
    }
    pub fn gpu_power(&self) -> Option<f64> {
        let v: Vec<f64> = self.gpus.iter().filter_map(|g| g.power_w).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    pub fn gpu_util(&self) -> Option<f64> {
        let v: Vec<f64> = self.gpus.iter().filter_map(|g| g.util_pct).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    }
    /// Best "how hot is it" number: the GPU, else the CPU.
    pub fn temp(&self) -> Option<f64> {
        self.gpu_temp().or(self.cpu_temp_c)
    }
    /// Best "how much power" number: the whole system, else the GPU.
    pub fn power(&self) -> Option<f64> {
        self.system_power_w.or_else(|| self.gpu_power())
    }
    pub fn tokens_per_s(&self) -> Option<f64> {
        let v: Vec<f64> = self.models.iter().filter_map(|m| m.tokens_per_s).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    pub fn model(&self, name: &str) -> Option<&ModelTel> {
        self.models.iter().find(|m| m.name == name)
    }
}

/// The few numbers kept per sample for sparklines.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub at: u64,
    pub mem_pct: Option<f64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub gpu_util_pct: Option<f64>,
    pub cpu_util_pct: Option<f64>,
    pub tokens_per_s: Option<f64>,
}

impl From<&Telemetry> for Sample {
    fn from(t: &Telemetry) -> Sample {
        Sample {
            at: t.at,
            mem_pct: t.mem_pct(),
            temp_c: t.temp(),
            power_w: t.power(),
            gpu_util_pct: t.gpu_util(),
            cpu_util_pct: t.cpu_util_pct,
            tokens_per_s: t.tokens_per_s(),
        }
    }
}

/// Which series of a [`Sample`] to plot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Mem,
    Temp,
    Power,
    GpuUtil,
    CpuUtil,
    Tokens,
}

impl Metric {
    pub const ALL: [Metric; 6] =
        [Metric::Mem, Metric::Temp, Metric::Power, Metric::GpuUtil, Metric::CpuUtil, Metric::Tokens];
    pub fn get(self, s: &Sample) -> Option<f64> {
        match self {
            Metric::Mem => s.mem_pct,
            Metric::Temp => s.temp_c,
            Metric::Power => s.power_w,
            Metric::GpuUtil => s.gpu_util_pct,
            Metric::CpuUtil => s.cpu_util_pct,
            Metric::Tokens => s.tokens_per_s,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Metric::Mem => "mem",
            Metric::Temp => "temp",
            Metric::Power => "power",
            Metric::GpuUtil => "gpu",
            Metric::CpuUtil => "cpu",
            Metric::Tokens => "tok/s",
        }
    }
    pub fn unit(self) -> &'static str {
        match self {
            Metric::Mem | Metric::GpuUtil | Metric::CpuUtil => "%",
            Metric::Temp => "°C",
            Metric::Power => "W",
            Metric::Tokens => "",
        }
    }
}

/// A rolling window of samples.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub samples: VecDeque<Sample>,
}

impl History {
    pub fn push(&mut self, s: Sample) {
        if self.samples.back().is_some_and(|b| b.at >= s.at) {
            return; // same or older sample (a host answered twice)
        }
        self.samples.push_back(s);
        while self.samples.len() > HISTORY_LEN {
            self.samples.pop_front();
        }
    }
    /// Merges a remote host's own history (it may have more than we saw).
    pub fn merge(&mut self, other: &[Sample]) {
        let mut all: Vec<Sample> = self.samples.iter().cloned().chain(other.iter().cloned()).collect();
        all.sort_by_key(|s| s.at);
        all.dedup_by_key(|s| s.at);
        let skip = all.len().saturating_sub(HISTORY_LEN);
        self.samples = all.into_iter().skip(skip).collect();
    }
    pub fn series(&self, m: Metric) -> Vec<Option<f64>> {
        self.samples.iter().map(|s| m.get(s)).collect()
    }
    /// Latest value minus the value about `secs` seconds earlier.
    pub fn change(&self, m: Metric, secs: u64) -> Option<f64> {
        let last = self.samples.back()?;
        let now = m.get(last)?;
        let target = last.at.saturating_sub(secs * 1000);
        let then = self.samples.iter().rev().find(|s| s.at <= target).and_then(|s| m.get(s))?;
        Some(now - then)
    }
}

// ---------------------------------------------------------------- parsers

/// Treats nvidia-smi's `[N/A]`, `N/A`, `[Not Supported]` and blanks as missing.
fn smi_num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s.contains("N/A") || s.contains("Not Supported") || s.contains("Unknown") {
        return None;
    }
    s.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%' || c == ' ').trim().parse().ok()
}

/// The `--query-gpu` fields [`parse_gpu_query`] expects, in order.
pub const GPU_QUERY: &str =
    "index,name,temperature.gpu,power.draw,power.limit,utilization.gpu,memory.used,memory.total,clocks.sm";

/// Parses `nvidia-smi --query-gpu=GPU_QUERY --format=csv,noheader,nounits`.
pub fn parse_gpu_query(text: &str) -> Vec<GpuTel> {
    text.lines()
        .filter_map(|line| {
            let c: Vec<&str> = line.split(',').map(str::trim).collect();
            if c.len() < 9 || c[1].is_empty() {
                return None;
            }
            Some(GpuTel {
                index: c[0].parse().ok()?,
                name: c[1].to_string(),
                temp_c: smi_num(c[2]),
                power_w: smi_num(c[3]),
                power_limit_w: smi_num(c[4]),
                util_pct: smi_num(c[5]),
                mem_used_gb: smi_num(c[6]).map(|m| m / 1024.0),
                mem_total_gb: smi_num(c[7]).map(|m| m / 1024.0),
                sm_clock_mhz: smi_num(c[8]),
            })
        })
        .collect()
}

/// Parses `nvidia-smi --query-compute-apps=pid,used_memory --format=csv,noheader,nounits`
/// into pid → GiB. Rows whose memory is `[N/A]` (GB10) are dropped.
pub fn parse_compute_apps(text: &str) -> HashMap<u32, f64> {
    text.lines()
        .filter_map(|l| {
            let (pid, mem) = l.split_once(',')?;
            Some((pid.trim().parse().ok()?, smi_num(mem)? / 1024.0))
        })
        .collect()
}

/// `/proc/meminfo` fields in GiB: (total, available, swap used).
pub fn parse_meminfo_full(text: &str) -> Option<(f64, f64, Option<f64>)> {
    let kb = |key: &str| -> Option<f64> {
        text.lines()
            .find(|l| l.split(':').next() == Some(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<f64>().ok())
            .map(|k| k * 1024.0 / GIB)
    };
    let total = kb("MemTotal")?;
    let avail = kb("MemAvailable").or_else(|| kb("MemFree"))?;
    let swap = match (kb("SwapTotal"), kb("SwapFree")) {
        (Some(t), Some(f)) if t > 0.0 => Some(t - f),
        _ => None,
    };
    Some((total, avail, swap))
}

/// `/proc/loadavg` or macOS `sysctl -n vm.loadavg` (`{ 1.0 2.0 3.0 }`).
pub fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let v: Vec<f64> = text
        .split_whitespace()
        .filter(|w| *w != "{" && *w != "}")
        .take(3)
        .map(|w| w.parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    (v.len() == 3).then(|| [v[0], v[1], v[2]])
}

/// `(idle, total)` jiffies from the first line of `/proc/stat`.
pub fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let idle = v[3] + v.get(4).copied().unwrap_or(0); // idle + iowait
    Some((idle, v.iter().sum()))
}

/// CPU busy percentage between two `/proc/stat` readings.
pub fn cpu_util(prev: (u64, u64), now: (u64, u64)) -> Option<f64> {
    let total = now.1.checked_sub(prev.1)?;
    let idle = now.0.checked_sub(prev.0)?;
    (total > 0).then(|| (100.0 * (total - idle.min(total)) as f64 / total as f64).clamp(0.0, 100.0))
}

/// `ps -A -o pid=,ppid=,rss=` → (pid, ppid, rss KiB).
pub fn parse_ps(text: &str) -> Vec<(u32, u32, u64)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// A process and all its descendants.
pub fn process_tree(procs: &[(u32, u32, u64)], root: u32) -> Vec<u32> {
    let mut out = vec![];
    if procs.iter().any(|p| p.0 == root) {
        out.push(root);
    }
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        for p in procs {
            if p.1 == parent && p.0 != parent && !out.contains(&p.0) {
                out.push(p.0);
            }
        }
        i += 1;
    }
    out
}

/// Sum of the RSS of a process tree, in GiB.
pub fn tree_rss_gb(procs: &[(u32, u32, u64)], root: u32) -> Option<f64> {
    let tree = process_tree(procs, root);
    if tree.is_empty() {
        return None;
    }
    let kb: u64 = procs.iter().filter(|p| tree.contains(&p.0)).map(|p| p.2).sum();
    Some(kb as f64 * 1024.0 / GIB)
}

/// Prometheus counters/gauges by metric name, summed over label sets.
pub fn parse_prometheus(text: &str) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, rest) = match line.find('{') {
            Some(i) => {
                let close = line[i..].find('}').map(|j| i + j + 1).unwrap_or(line.len());
                (&line[..i], &line[close..])
            }
            None => match line.split_once(char::is_whitespace) {
                Some((n, r)) => (n, r),
                None => continue,
            },
        };
        let Some(v) = rest.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) else { continue };
        if v.is_finite() {
            *out.entry(name.to_string()).or_default() += v;
        }
    }
    out
}

/// What an engine's `/metrics` says, normalised.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineMetrics {
    /// Monotonic generated-tokens counter, when the engine has one.
    pub tokens_total: Option<f64>,
    /// An instantaneous throughput gauge, used when there is no counter.
    pub tokens_per_s: Option<f64>,
    pub requests_running: Option<f64>,
    pub source: &'static str,
}

pub fn engine_metrics(engine: &str, m: &HashMap<String, f64>) -> Option<EngineMetrics> {
    let get = |k: &str| m.get(k).copied();
    let em = match engine {
        "vllm" => EngineMetrics {
            tokens_total: get("vllm:generation_tokens_total"),
            tokens_per_s: None,
            requests_running: get("vllm:num_requests_running"),
            source: "vllm /metrics",
        },
        "sglang" => EngineMetrics {
            tokens_total: get("sglang:generation_tokens_total"),
            tokens_per_s: get("sglang:gen_throughput"),
            requests_running: get("sglang:num_running_reqs"),
            source: "sglang /metrics",
        },
        "llama.cpp" => EngineMetrics {
            tokens_total: get("llamacpp:tokens_predicted_total"),
            tokens_per_s: None,
            requests_running: get("llamacpp:requests_processing"),
            source: "llama.cpp /metrics",
        },
        _ => return None,
    };
    (em.tokens_total.is_some() || em.tokens_per_s.is_some() || em.requests_running.is_some()).then_some(em)
}

/// One hwmon/thermal temperature reading.
#[derive(Debug, Clone, PartialEq)]
pub struct TempReading {
    pub chip: String,
    pub label: String,
    pub celsius: f64,
}

/// Picks the CPU temperature: spbm CPU clusters, then CPU hwmon chips, then
/// thermal zones (DGX Spark exposes several `acpitz` zones).
pub fn pick_cpu_temp(r: &[TempReading]) -> Option<(f64, String)> {
    let max_of = |f: &dyn Fn(&TempReading) -> bool| r.iter().filter(|t| f(t)).map(|t| t.celsius).reduce(f64::max);
    let sane = |t: &TempReading| t.celsius > 0.0 && t.celsius < 150.0;
    if let Some(c) = max_of(&|t| sane(t) && t.chip == "spbm" && t.label.starts_with("cpu_")) {
        return Some((c, "spbm".into()));
    }
    for chip in ["k10temp", "coretemp", "zenpower", "cpu_thermal", "soc_thermal"] {
        if let Some(c) = max_of(&|t| sane(t) && t.chip == chip) {
            return Some((c, format!("hwmon {chip}")));
        }
    }
    if let Some(c) = max_of(&|t| sane(t) && t.chip.starts_with("zone:") && t.chip.contains("cpu")) {
        return Some((c, "thermal zones".into()));
    }
    max_of(&|t| sane(t) && t.chip.starts_with("zone:")).map(|c| (c, "thermal zones (max)".into()))
}

/// Picks whole-system power from spbm rails.
pub fn pick_system_power(rails: &[PowerRail]) -> Option<(f64, String)> {
    ["sys_total", "dc_input"]
        .iter()
        .find_map(|l| rails.iter().find(|r| r.label == *l).map(|r| (r.watts, format!("spbm {l}"))))
}

/// Parses `powermetrics -n 1 --samplers cpu_power,gpu_power` output (mW).
pub fn parse_powermetrics(text: &str) -> (Option<f64>, Option<f64>) {
    let mw = |key: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 1000.0)
    };
    (mw("Combined Power"), mw("GPU Power"))
}

/// macOS `kern.memorystatus_level` (percent free) → percent under pressure.
pub fn parse_memorystatus_level(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|v| (0.0..=100.0).contains(v)).map(|free| 100.0 - free)
}

// ---------------------------------------------------------------- sysfs

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_num(p: &Path) -> Option<f64> {
    read_trim(p)?.parse().ok()
}

/// Every hwmon temp channel and thermal zone, plus spbm power rails.
fn read_sensors(root: &Path) -> (Vec<TempReading>, Vec<PowerRail>) {
    let mut temps = vec![];
    let mut rails = vec![];
    if let Ok(dir) = fs::read_dir(root.join("class/hwmon")) {
        for d in dir.flatten() {
            let p = d.path();
            let chip = read_trim(&p.join("name")).unwrap_or_default();
            let Ok(files) = fs::read_dir(&p) else { continue };
            let mut names: Vec<String> =
                files.flatten().map(|f| f.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            for f in &names {
                if let Some(idx) = f.strip_prefix("temp").and_then(|s| s.strip_suffix("_input")) {
                    if let Some(v) = read_num(&p.join(f)) {
                        let label =
                            read_trim(&p.join(format!("temp{idx}_label"))).unwrap_or_else(|| format!("temp{idx}"));
                        temps.push(TempReading { chip: chip.clone(), label, celsius: v / 1000.0 });
                    }
                }
                if chip == "spbm" {
                    if let Some(idx) = f.strip_prefix("power").and_then(|s| s.strip_suffix("_input")) {
                        if let Some(v) = read_num(&p.join(f)) {
                            let label = read_trim(&p.join(format!("power{idx}_label")))
                                .unwrap_or_else(|| format!("power{idx}"));
                            rails.push(PowerRail { label, watts: v / 1_000_000.0 });
                        }
                    }
                }
            }
        }
    }
    if let Ok(dir) = fs::read_dir(root.join("class/thermal")) {
        for d in dir.flatten() {
            let p = d.path();
            if !d.file_name().to_string_lossy().starts_with("thermal_zone") {
                continue;
            }
            if let (Some(t), Some(v)) = (read_trim(&p.join("type")), read_num(&p.join("temp"))) {
                temps.push(TempReading { chip: format!("zone:{t}"), label: t, celsius: v / 1000.0 });
            }
        }
    }
    (temps, rails)
}

// ---------------------------------------------------------------- collector

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Whether `LMDECK_DEMO` asks for mock telemetry.
pub fn demo_mode() -> bool {
    std::env::var("LMDECK_DEMO").is_ok_and(|v| !v.is_empty() && v != "0" && v != "false")
}

/// Collects [`Telemetry`]; keeps the previous readings needed for rates.
pub struct Collector {
    prev_cpu: Option<(u64, u64)>,
    prev_tokens: HashMap<String, (f64, Instant)>,
    nvidia_smi: Option<bool>,
    /// The last [`find_adopted`] result and when it ran.
    adopted: Option<(Instant, Vec<ModelTel>)>,
    hw: Option<Hardware>,
    pub sys_root: std::path::PathBuf,
    pub demo: bool,
}

impl Default for Collector {
    fn default() -> Self {
        Collector::new()
    }
}

impl Collector {
    pub fn new() -> Collector {
        Collector {
            prev_cpu: None,
            prev_tokens: HashMap::new(),
            nvidia_smi: None,
            adopted: None,
            hw: None,
            sys_root: "/sys".into(),
            demo: demo_mode(),
        }
    }

    pub fn collect(&mut self, paths: &Paths) -> Telemetry {
        if self.demo {
            let at = now_ms();
            let mut t = demo::local(at as f64 / 1000.0, at);
            // Label the synthetic model with whatever is really running, so
            // the deck and hosts views tell the same (demo) story.
            if let (Some(r), Some(m)) = (RunRecord::all(paths).into_iter().find(|r| r.alive()), t.models.first_mut()) {
                m.name = r.name;
                m.engine = r.engine;
                m.port = r.port;
                m.pid = r.pid;
                m.metrics = Some("demo".into());
            }
            return t;
        }
        // Static facts (platform, hostname, cores) are detected once.
        let hw = self.hw.get_or_insert_with(Hardware::detect).clone();
        let mut t = Telemetry {
            hostname: hw.hostname.clone(),
            platform: hw.platform.clone(),
            os: hw.os.clone(),
            arch: hw.arch.clone(),
            at: now_ms(),
            unified_memory: hw.unified_memory,
            mem_total_gb: hw.total_memory_gb,
            cpu_cores: hw.cores,
            ..Default::default()
        };
        if hw.os == "macos" {
            self.collect_macos(&mut t, &hw);
        } else {
            self.collect_linux(&mut t);
        }
        self.collect_gpus(&mut t);
        self.collect_models(&mut t, paths);
        t
    }

    fn collect_linux(&mut self, t: &mut Telemetry) {
        if let Some((total, avail, swap)) =
            fs::read_to_string("/proc/meminfo").ok().and_then(|s| parse_meminfo_full(&s))
        {
            t.mem_total_gb = total;
            t.mem_available_gb = Some(avail);
            t.mem_used_gb = Some((total - avail).max(0.0));
            t.swap_used_gb = swap;
            t.sources.push("/proc/meminfo".into());
        }
        t.load = fs::read_to_string("/proc/loadavg").ok().and_then(|s| parse_loadavg(&s));
        if let Some(now) = fs::read_to_string("/proc/stat").ok().and_then(|s| parse_proc_stat(&s)) {
            t.cpu_util_pct = self.prev_cpu.and_then(|p| cpu_util(p, now));
            self.prev_cpu = Some(now);
        }
        let (temps, rails) = read_sensors(&self.sys_root);
        if let Some((c, src)) = pick_cpu_temp(&temps) {
            t.cpu_temp_c = Some(c);
            t.cpu_temp_source = Some(src);
        }
        if let Some((w, src)) = pick_system_power(&rails) {
            t.system_power_w = Some(w);
            t.system_power_source = Some(src);
        }
        if !rails.is_empty() {
            t.sources.push("spbm hwmon".into());
        }
        t.rails = rails;
    }

    fn collect_macos(&mut self, t: &mut Telemetry, hw: &Hardware) {
        let avail = run_with_timeout(&mut Command::new("vm_stat"), Duration::from_secs(2))
            .and_then(|s| crate::hardware::parse_vm_stat(&s))
            .or(hw.available_memory_gb);
        if let Some(avail) = avail {
            t.mem_available_gb = Some(avail);
            t.mem_used_gb = Some((t.mem_total_gb - avail).max(0.0));
            t.sources.push("vm_stat".into());
        }
        let sysctl = |k: &str| run_with_timeout(Command::new("sysctl").args(["-n", k]), Duration::from_secs(2));
        t.load = sysctl("vm.loadavg").and_then(|s| parse_loadavg(&s));
        t.mem_pressure_pct = sysctl("kern.memorystatus_level").and_then(|s| parse_memorystatus_level(&s));
        // powermetrics needs root: only with an explicit opt-in, and never prompt.
        if std::env::var("LMDECK_POWERMETRICS").is_ok_and(|v| v == "1") {
            if let Some(out) = run_with_timeout(
                Command::new("sudo").args([
                    "-n",
                    "powermetrics",
                    "-n",
                    "1",
                    "-i",
                    "500",
                    "--samplers",
                    "cpu_power,gpu_power",
                ]),
                Duration::from_secs(4),
            ) {
                let (combined, gpu) = parse_powermetrics(&out);
                if let Some(w) = combined {
                    t.system_power_w = Some(w);
                    t.system_power_source = Some("powermetrics CPU+GPU+ANE".into());
                }
                if let Some(w) = gpu {
                    t.gpus.push(GpuTel { name: hw.accelerator().to_string(), power_w: Some(w), ..Default::default() });
                }
                t.sources.push("powermetrics".into());
            }
        }
    }

    fn collect_gpus(&mut self, t: &mut Telemetry) {
        if self.nvidia_smi == Some(false) {
            return;
        }
        let out = run_with_timeout(
            Command::new("nvidia-smi").args([&format!("--query-gpu={GPU_QUERY}"), "--format=csv,noheader,nounits"]),
            Duration::from_secs(4),
        );
        if self.nvidia_smi.is_none() {
            self.nvidia_smi = Some(out.is_some());
        }
        if let Some(out) = out {
            t.gpus = parse_gpu_query(&out);
            t.sources.push("nvidia-smi".into());
        }
    }

    /// Deck recipes running here that lmdeck didn't start, re-checked every
    /// [`ADOPT_EVERY`] (a port probe or `docker inspect` per recipe).
    fn adopted(&mut self, paths: &Paths, skip: &[String]) -> Vec<ModelTel> {
        if self.adopted.as_ref().is_none_or(|(at, _)| at.elapsed() >= ADOPT_EVERY) {
            self.adopted = Some((Instant::now(), find_adopted(paths)));
        }
        let found = self.adopted.as_ref().map(|(_, v)| v.clone()).unwrap_or_default();
        found.into_iter().filter(|m| !skip.contains(&m.name)).collect()
    }

    fn collect_models(&mut self, t: &mut Telemetry, paths: &Paths) {
        let recs: Vec<RunRecord> = RunRecord::all(paths).into_iter().filter(|r| r.alive()).collect();
        let names: Vec<String> = recs.iter().map(|r| r.name.clone()).collect();
        let adopted = self.adopted(paths, &names);
        if recs.is_empty() {
            self.prev_tokens.clear();
            t.models.extend(adopted);
            return;
        }
        let procs = run_with_timeout(Command::new("ps").args(["-A", "-o", "pid=,ppid=,rss="]), Duration::from_secs(3))
            .map(|s| parse_ps(&s))
            .unwrap_or_default();
        let gpu_mem = if self.nvidia_smi == Some(true) {
            run_with_timeout(
                Command::new("nvidia-smi")
                    .args(["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"]),
                Duration::from_secs(4),
            )
            .map(|s| parse_compute_apps(&s))
            .unwrap_or_default()
        } else {
            HashMap::new()
        };
        for r in recs {
            let mut m = ModelTel {
                name: r.name.clone(),
                engine: r.engine.clone(),
                port: r.port,
                pid: r.pid,
                ..Default::default()
            };
            if let Some(pid) = r.pid {
                m.rss_gb = tree_rss_gb(&procs, pid);
                m.mem_share_pct = m.rss_gb.filter(|_| t.mem_total_gb > 0.0).map(|g| 100.0 * g / t.mem_total_gb);
                let tree = process_tree(&procs, pid);
                let g: f64 = tree.iter().filter_map(|p| gpu_mem.get(p)).sum();
                m.gpu_mem_gb = (g > 0.0).then_some(g);
            }
            self.engine_rate(&mut m);
            let ready = probe(r.port, Duration::from_millis(500), r.api_key().as_deref());
            m.status = Some(if matches!(ready, Probe::Up(_)) { "loaded" } else { "loading" }.into());
            t.models.push(m);
        }
        t.models.extend(adopted);
        let names: Vec<String> = t.models.iter().map(|m| m.name.clone()).collect();
        self.prev_tokens.retain(|k, _| names.contains(k));
    }

    fn engine_rate(&mut self, m: &mut ModelTel) {
        if !matches!(m.engine.as_str(), "vllm" | "sglang" | "llama.cpp") {
            return;
        }
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_millis(700)).build();
        let Ok(resp) = agent.get(&format!("http://127.0.0.1:{}/metrics", m.port)).call() else { return };
        let Ok(body) = resp.into_string() else { return };
        let Some(em) = engine_metrics(&m.engine, &parse_prometheus(&body)) else { return };
        m.metrics = Some(em.source.to_string());
        m.requests_running = em.requests_running;
        let now = Instant::now();
        if let Some(total) = em.tokens_total {
            if let Some((prev, at)) = self.prev_tokens.get(&m.name) {
                let dt = now.duration_since(*at).as_secs_f64();
                if total >= *prev && dt > 0.2 {
                    m.tokens_per_s = Some((total - prev) / dt);
                }
            }
            self.prev_tokens.insert(m.name.clone(), (total, now));
        } else {
            m.tokens_per_s = em.tokens_per_s;
        }
    }
}

/// Mock telemetry for `LMDECK_DEMO=1`. Plausible DGX Spark / Mac numbers
/// from smooth functions of time; every value carries `demo: true`.
pub mod demo {
    use super::*;

    fn wave(t: f64, period: f64, phase: f64) -> f64 {
        ((t / period + phase) * std::f64::consts::TAU).sin()
    }

    /// A GB10 box running a model. `seed` varies the shape per host.
    pub fn spark(name: &str, seed: f64, t: f64, at: u64, busy: bool) -> Telemetry {
        let load = if busy { 0.55 + 0.4 * wave(t, 90.0, seed).max(-0.6) } else { 0.05 + 0.03 * wave(t, 40.0, seed) };
        let gpu_w = 12.0 + 150.0 * load + 6.0 * wave(t, 7.0, seed);
        let models = if busy {
            vec![ModelTel {
                name: ["qwen3.6-35b-a3b-nvfp4-spark", "gpt-oss-120b-mxfp4", "qwen3-30b-a3b-exl3"]
                    [(seed * 3.0) as usize % 3]
                    .into(),
                engine: "vllm".into(),
                port: 8000,
                pid: Some(4100 + (seed * 100.0) as u32),
                rss_gb: Some(58.0 + 20.0 * seed),
                mem_share_pct: Some((58.0 + 20.0 * seed) / 1.2),
                gpu_mem_gb: None,
                tokens_per_s: Some((38.0 + 30.0 * load + 3.0 * wave(t, 5.0, seed)).max(0.0)),
                requests_running: Some((load * 6.0).round()),
                metrics: Some("vllm /metrics".into()),
                status: Some("loaded".into()),
            }]
        } else {
            vec![]
        };
        let mem_used = if busy { 72.0 + 20.0 * seed + 2.0 * wave(t, 60.0, seed) } else { 9.0 + wave(t, 50.0, seed) };
        Telemetry {
            hostname: name.into(),
            platform: "NVIDIA GB10 (DGX Spark class)".into(),
            os: "linux".into(),
            arch: "aarch64".into(),
            at,
            unified_memory: true,
            mem_total_gb: 119.7,
            mem_used_gb: Some(mem_used),
            mem_available_gb: Some(119.7 - mem_used),
            swap_used_gb: Some(0.0),
            mem_pressure_pct: None,
            cpu_cores: 20,
            load: Some([1.0 + 6.0 * load, 1.0 + 5.0 * load, 0.8 + 4.0 * load]),
            cpu_util_pct: Some((8.0 + 30.0 * load + 4.0 * wave(t, 11.0, seed)).clamp(0.0, 100.0)),
            cpu_temp_c: Some(42.0 + 25.0 * load + 1.5 * wave(t, 13.0, seed)),
            cpu_temp_source: Some("spbm".into()),
            system_power_w: Some(gpu_w + 22.0 + 10.0 * load),
            system_power_source: Some("spbm sys_total".into()),
            gpus: vec![GpuTel {
                index: 0,
                name: "NVIDIA GB10".into(),
                temp_c: Some(44.0 + 30.0 * load + 1.5 * wave(t, 9.0, seed)),
                power_w: Some(gpu_w),
                power_limit_w: None,
                util_pct: Some((100.0 * load + 5.0 * wave(t, 3.0, seed)).clamp(0.0, 100.0)),
                mem_used_gb: None,
                mem_total_gb: None,
                sm_clock_mhz: Some(1300.0 + 1100.0 * load),
            }],
            rails: vec![],
            models,
            sources: vec!["demo".into()],
            demo: true,
        }
    }

    pub fn mac(t: f64, at: u64) -> Telemetry {
        let mem_used = 41.0 + 3.0 * wave(t, 70.0, 0.3);
        Telemetry {
            hostname: "mac-studio".into(),
            platform: "Apple M3 Ultra".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            at,
            unified_memory: true,
            mem_total_gb: 96.0,
            mem_used_gb: Some(mem_used),
            mem_available_gb: Some(96.0 - mem_used),
            mem_pressure_pct: Some(22.0 + 4.0 * wave(t, 70.0, 0.3)),
            cpu_cores: 28,
            load: Some([3.1, 2.8, 2.5]),
            gpus: vec![GpuTel {
                index: 0,
                name: "Apple M3 Ultra".into(),
                temp_c: None,
                power_w: Some(45.0 + 10.0 * wave(t, 40.0, 0.2)),
                mem_used_gb: Some(mem_used * 0.4),
                mem_total_gb: None, // unified
                ..Default::default()
            }],
            models: vec![ModelTel {
                name: "qwen3-30b-a3b-mlx".into(),
                engine: "mlx".into(),
                port: 8082,
                pid: Some(7311),
                rss_gb: Some(18.4),
                mem_share_pct: Some(19.2),
                ..Default::default()
            }],
            sources: vec!["demo".into()],
            demo: true,
            ..Default::default()
        }
    }

    /// The local machine in demo mode.
    pub fn local(t: f64, at: u64) -> Telemetry {
        let mut tel = spark("spark-01.lab", 0.1, t, at, true);
        tel.hostname = "spark-01.lab".into();
        tel
    }

    /// Fills a history with the last `n` samples of a generator.
    pub fn history(n: usize, at: u64, f: impl Fn(f64, u64) -> Telemetry) -> History {
        let mut h = History::default();
        for i in (0..n).rev() {
            let back = i as u64 * SAMPLE_EVERY.as_millis() as u64;
            let ts = at.saturating_sub(back);
            h.push(Sample::from(&f(ts as f64 / 1000.0, ts)));
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from a DGX Spark (GB10, driver 580): memory and limits are [N/A].
    const GB10: &str = "0, NVIDIA GB10, 58, 14.21, [N/A], 2, [N/A], [N/A], 2405\n";
    const TWO_4090: &str = "0, NVIDIA GeForce RTX 4090, 61, 287.50, 450.00, 97, 21034, 24564, 2730\n\
                            1, NVIDIA GeForce RTX 4090, 44, [Not Supported], 450.00, 0, 3, 24564, 210\n";

    #[test]
    fn gpu_query_handles_na_fields() {
        let g = parse_gpu_query(GB10);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "NVIDIA GB10");
        assert_eq!(g[0].temp_c, Some(58.0));
        assert_eq!(g[0].power_w, Some(14.21));
        assert_eq!(g[0].power_limit_w, None);
        assert_eq!(g[0].util_pct, Some(2.0));
        assert_eq!(g[0].mem_used_gb, None);
        assert_eq!(g[0].mem_total_gb, None);
        assert_eq!(g[0].sm_clock_mhz, Some(2405.0));

        let g = parse_gpu_query(TWO_4090);
        assert_eq!(g.len(), 2);
        assert!((g[0].mem_used_gb.unwrap() - 20.54).abs() < 0.01);
        assert!((g[0].mem_total_gb.unwrap() - 23.99).abs() < 0.01);
        assert_eq!(g[1].power_w, None);
        assert_eq!(g[1].index, 1);
        assert!(parse_gpu_query("garbage\n\n").is_empty());
    }

    #[test]
    fn compute_apps_drop_na() {
        let m = parse_compute_apps("4242, 20480\n4343, [N/A]\n");
        assert_eq!(m.get(&4242), Some(&20.0));
        assert!(!m.contains_key(&4343));
    }

    #[test]
    fn meminfo_load_and_stat() {
        let mi = "MemTotal:       125513348 kB\nMemFree:         1000000 kB\nMemAvailable:   104857600 kB\nSwapTotal:       16777216 kB\nSwapFree:        16252928 kB\n";
        let (total, avail, swap) = parse_meminfo_full(mi).unwrap();
        assert!((total - 119.7).abs() < 0.05, "{total}");
        assert!((avail - 100.0).abs() < 0.01);
        assert!((swap.unwrap() - 0.5).abs() < 0.01);
        assert_eq!(parse_meminfo_full("MemTotal: 1 kB\n"), None, "no available field");

        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/1234 5678\n"), Some([0.52, 0.58, 0.59]));
        assert_eq!(parse_loadavg("{ 1.84 2.01 2.20 }\n"), Some([1.84, 2.01, 2.20]));
        assert_eq!(parse_loadavg("nope"), None);

        let a = parse_proc_stat("cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 2 3 4\n").unwrap();
        let b = parse_proc_stat("cpu  200 0 200 1300 100 0 0 0 0 0\n").unwrap();
        assert_eq!(a, (800, 1000));
        assert_eq!(cpu_util(a, b), Some(25.0));
        assert_eq!(cpu_util(b, a), None, "counter went backwards");
    }

    #[test]
    fn process_tree_rss() {
        let ps = parse_ps("    1     0   1000\n  100     1  2048\n  101   100 1048576\n  102   101 1048576\n  200     1   999\nbad line\n");
        assert_eq!(process_tree(&ps, 100), vec![100, 101, 102]);
        let gb = tree_rss_gb(&ps, 100).unwrap();
        assert!((gb - 2.002).abs() < 0.001, "{gb}");
        assert_eq!(tree_rss_gb(&ps, 999), None);
    }

    #[test]
    fn prometheus_and_engines() {
        let vllm = "# HELP vllm:generation_tokens_total Number of generation tokens processed.\n\
                    # TYPE vllm:generation_tokens_total counter\n\
                    vllm:generation_tokens_total{engine=\"0\",model_name=\"qwen\"} 1500.0\n\
                    vllm:generation_tokens_total{engine=\"1\",model_name=\"qwen\"} 500.0\n\
                    vllm:num_requests_running{engine=\"0\",model_name=\"qwen\"} 3.0\n\
                    vllm:prompt_tokens_total 1e3\n";
        let m = parse_prometheus(vllm);
        assert_eq!(m.get("vllm:generation_tokens_total"), Some(&2000.0));
        assert_eq!(m.get("vllm:prompt_tokens_total"), Some(&1000.0));
        let e = engine_metrics("vllm", &m).unwrap();
        assert_eq!(e.tokens_total, Some(2000.0));
        assert_eq!(e.requests_running, Some(3.0));

        let llama = "llamacpp:tokens_predicted_total 812\nllamacpp:requests_processing 1\n";
        let e = engine_metrics("llama.cpp", &parse_prometheus(llama)).unwrap();
        assert_eq!(e.tokens_total, Some(812.0));
        assert_eq!(engine_metrics("mlx", &parse_prometheus(llama)), None);
        assert_eq!(engine_metrics("vllm", &parse_prometheus("other 1\n")), None);
    }

    #[test]
    fn temperatures_and_power() {
        let r = |chip: &str, label: &str, c: f64| TempReading { chip: chip.into(), label: label.into(), celsius: c };
        let spark = vec![
            r("zone:acpitz", "acpitz", 47.0),
            r("spbm", "tj_max", 60.0),
            r("spbm", "cpu_p_clu0", 51.0),
            r("spbm", "cpu_e_clu1", 49.0),
            r("spbm", "gpu", 55.0),
        ];
        assert_eq!(pick_cpu_temp(&spark), Some((51.0, "spbm".into())));
        let plain_spark =
            vec![r("zone:acpitz", "acpitz", 47.0), r("zone:acpitz", "acpitz", 52.5), r("nvme", "Composite", 70.0)];
        assert_eq!(pick_cpu_temp(&plain_spark), Some((52.5, "thermal zones (max)".into())));
        let amd = vec![r("k10temp", "Tctl", 63.1), r("zone:x86_pkg_temp", "x86_pkg_temp", 99.0)];
        assert_eq!(pick_cpu_temp(&amd).unwrap().0, 63.1);
        assert_eq!(pick_cpu_temp(&[r("zone:bogus", "b", -273.0)]), None);

        let rails =
            vec![PowerRail { label: "gpu".into(), watts: 5.0 }, PowerRail { label: "sys_total".into(), watts: 26.4 }];
        assert_eq!(pick_system_power(&rails), Some((26.4, "spbm sys_total".into())));
        assert_eq!(pick_system_power(&rails[..1]), None);

        let pm = "**** Processor usage ****\nCPU Power: 1234 mW\nGPU Power: 56 mW\nANE Power: 0 mW\nCombined Power (CPU + GPU + ANE): 1290 mW\n";
        assert_eq!(parse_powermetrics(pm), (Some(1.29), Some(0.056)));
        assert_eq!(parse_memorystatus_level("63\n"), Some(37.0));
        assert_eq!(parse_memorystatus_level("x"), None);
    }

    #[test]
    fn sensors_from_a_fake_sysfs() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path().join("class/hwmon/hwmon3");
        fs::create_dir_all(&h).unwrap();
        fs::write(h.join("name"), "spbm\n").unwrap();
        fs::write(h.join("power1_input"), "26400000\n").unwrap();
        fs::write(h.join("power1_label"), "sys_total\n").unwrap();
        fs::write(h.join("temp2_input"), "51000\n").unwrap();
        fs::write(h.join("temp2_label"), "cpu_p_clu0\n").unwrap();
        let z = d.path().join("class/thermal/thermal_zone0");
        fs::create_dir_all(&z).unwrap();
        fs::write(z.join("type"), "acpitz\n").unwrap();
        fs::write(z.join("temp"), "47000\n").unwrap();
        let (temps, rails) = read_sensors(d.path());
        assert_eq!(rails, vec![PowerRail { label: "sys_total".into(), watts: 26.4 }]);
        assert_eq!(pick_cpu_temp(&temps), Some((51.0, "spbm".into())));
        assert_eq!(temps.len(), 2);
    }

    #[test]
    fn history_window_changes_and_merge() {
        let mut h = History::default();
        for i in 0..(HISTORY_LEN as u64 + 20) {
            h.push(Sample { at: i * 2000, temp_c: Some(40.0 + i as f64 * 0.1), ..Default::default() });
        }
        assert_eq!(h.samples.len(), HISTORY_LEN);
        h.push(Sample { at: 0, ..Default::default() });
        assert_eq!(h.samples.len(), HISTORY_LEN, "older samples are ignored");
        let d = h.change(Metric::Temp, 60).unwrap();
        assert!((d - 3.0).abs() < 1e-9, "{d}");
        assert_eq!(h.change(Metric::Power, 60), None);

        let mut a = History::default();
        a.push(Sample { at: 10, ..Default::default() });
        a.merge(&[Sample { at: 5, ..Default::default() }, Sample { at: 10, ..Default::default() }]);
        assert_eq!(a.samples.iter().map(|s| s.at).collect::<Vec<_>>(), vec![5, 10]);
    }

    #[test]
    fn derived_numbers() {
        let t = Telemetry {
            mem_total_gb: 100.0,
            mem_used_gb: Some(25.0),
            cpu_temp_c: Some(50.0),
            gpus: vec![
                GpuTel { temp_c: Some(60.0), power_w: Some(100.0), util_pct: Some(50.0), ..Default::default() },
                GpuTel { temp_c: Some(70.0), power_w: None, util_pct: Some(100.0), ..Default::default() },
            ],
            models: vec![
                ModelTel { tokens_per_s: Some(10.0), ..Default::default() },
                ModelTel { tokens_per_s: Some(5.0), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(t.mem_pct(), Some(25.0));
        assert_eq!(t.temp(), Some(70.0));
        assert_eq!(t.power(), Some(100.0));
        assert_eq!(t.gpu_util(), Some(75.0));
        assert_eq!(t.tokens_per_s(), Some(15.0));
        let bare = Telemetry::default();
        assert_eq!((bare.mem_pct(), bare.temp(), bare.power(), bare.tokens_per_s()), (None, None, None, None));
    }

    #[test]
    fn demo_is_flagged() {
        let t = demo::local(10.0, 1000);
        assert!(t.demo);
        let h = demo::history(30, 100_000, |s, at| demo::spark("x", 0.5, s, at, true));
        assert_eq!(h.samples.len(), 30);
        assert!(h.samples.iter().all(|s| s.power_w.is_some()));
    }
}
