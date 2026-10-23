//! Recipe ↔ hardware compatibility checks.

use serde::Serialize;

use crate::engine::Inventory;
use crate::hardware::{cmp_version, normalize_arch, normalize_os, Hardware};
use crate::recipe::{Engine, Recipe};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub level: Level,
    pub what: String,
    pub detail: String,
}

/// The outcome of checking one recipe against one machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn level(&self) -> Level {
        self.checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok)
    }

    pub fn compatible(&self) -> bool {
        self.level() != Level::Fail
    }

    /// One-line reasons for failures (and warnings when `warnings`).
    pub fn reasons(&self, warnings: bool) -> Vec<String> {
        self.checks
            .iter()
            .filter(|c| c.level == Level::Fail || (warnings && c.level == Level::Warn))
            .map(|c| format!("{}: {}", c.what, c.detail))
            .collect()
    }
}

/// Memory the recipe needs, in GB: explicit `min_memory_gb`, else the
/// weights size plus 20% headroom (KV cache, activations).
pub fn required_memory_gb(r: &Recipe, on_disk_bytes: Option<u64>) -> Option<f64> {
    if let Some(m) = r.hardware.min_memory_gb {
        return Some(m);
    }
    let gb = on_disk_bytes.filter(|b| *b > 0).map(|b| b as f64 / 1_073_741_824.0).or(r.model.size_gb)?;
    Some(gb * 1.2)
}

/// vLLM/SGLang reserve a fixed fraction of (unified) memory up front.
pub fn reserved_fraction(r: &Recipe) -> Option<f64> {
    let mut it = r.engine.args.iter();
    while let Some(a) = it.next() {
        for flag in ["--gpu-memory-utilization", "--mem-fraction-static"] {
            if a == flag {
                return it.next().and_then(|v| v.parse().ok());
            }
            if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
                return v.parse().ok();
            }
        }
    }
    None
}

/// Checks `r` against `hw` (and, when given, the installed engines).
pub fn check(r: &Recipe, hw: &Hardware, inv: Option<&Inventory>, on_disk_bytes: Option<u64>) -> Report {
    let mut checks = vec![];
    let mut push = |level, what: &str, detail: String| checks.push(Check { level, what: what.into(), detail });
    let req = &r.hardware;

    if !req.os.is_empty() {
        let ok = req.os.iter().any(|o| normalize_os(o) == hw.os);
        push(if ok { Level::Ok } else { Level::Fail }, "os", format!("needs {}, this is {}", req.os.join("/"), hw.os));
    }
    if !req.arch.is_empty() {
        let ok = req.arch.iter().any(|a| normalize_arch(a) == normalize_arch(&hw.arch));
        push(
            if ok { Level::Ok } else { Level::Fail },
            "arch",
            format!("needs {}, this is {}", req.arch.join("/"), hw.arch),
        );
    }
    if !req.accelerator.is_empty() {
        let have = hw.accelerator();
        let ok = req.accelerator.iter().any(|a| a.eq_ignore_ascii_case(have));
        push(
            if ok { Level::Ok } else { Level::Fail },
            "accelerator",
            format!("needs {}, this has {}", req.accelerator.join("/"), have),
        );
    }
    if let Some(min) = &req.min_compute_capability {
        match hw.compute_capability() {
            Some(cc) => {
                let ok = cmp_version(&cc, min).is_ge();
                push(if ok { Level::Ok } else { Level::Fail }, "compute", format!("needs sm {min}+, GPU is {cc}"));
            }
            None => push(Level::Fail, "compute", format!("needs an NVIDIA GPU with compute capability {min}+")),
        }
    }
    let pool = hw.model_memory_gb();
    let kind = if hw.unified_memory {
        "unified"
    } else if hw.gpus.iter().any(|g| g.memory_gb.is_some()) {
        "VRAM"
    } else {
        "RAM"
    };
    match required_memory_gb(r, on_disk_bytes) {
        Some(need) if pool > 0.0 => {
            let level = if need > pool {
                Level::Fail
            } else if need > pool * 0.9 {
                Level::Warn
            } else {
                Level::Ok
            };
            push(level, "memory", format!("needs ~{need:.0} GB, {pool:.0} GB {kind}"));
        }
        Some(need) => push(Level::Warn, "memory", format!("needs ~{need:.0} GB, could not measure this machine")),
        None => push(Level::Warn, "memory", "no size or min_memory_gb in recipe; cannot estimate".into()),
    }
    if let Some(frac) = reserved_fraction(r) {
        if hw.unified_memory && frac > 0.9 {
            push(
                Level::Warn,
                "reserve",
                format!("engine reserves {:.0}% of unified memory; leave room for the OS", frac * 100.0),
            );
        }
    }
    if !r.nodes.is_empty() {
        let mut hosts: Vec<&str> = r.nodes.iter().map(|n| n.host.as_str()).collect();
        hosts.dedup();
        push(
            Level::Ok,
            "nodes",
            format!("{} nodes on {}, started in order (each host runs its own share)", r.nodes.len(), hosts.join(", ")),
        );
    } else if r.nodes() > 1 {
        push(Level::Fail, "nodes", format!("spans {} machines but has no [[nodes]] saying what runs where", r.nodes()));
    }
    if r.engine.name == Engine::TensorFold {
        for (level, what, detail) in tensorfold_checks(r, hw) {
            push(level, what, detail);
        }
    }
    if !r.nodes.is_empty() {
        // Only this machine's nodes can be checked here; remote nodes are checked by their host.
        for n in r.nodes.iter().filter(|n| n.host == crate::hosts::LOCAL) {
            let bin = n.command.first().map(String::as_str).unwrap_or("");
            if !bin.is_empty() && !bin.contains('/') && crate::hardware::which(bin).is_none() {
                push(Level::Warn, "engine", format!("node {}: command '{bin}' not found", n.name));
            }
        }
    } else if let Some(inv) = inv {
        match inv.can_launch(r) {
            Ok(()) => push(Level::Ok, "engine", format!("{} available", r.engine.name)),
            Err(e) => push(Level::Warn, "engine", e),
        }
    }
    Report { checks }
}

/// TensorFold's published scope: Apple-silicon Macs (Metal) and Linux with an NVIDIA
/// GPU (CUDA, x86_64 or aarch64). It reads MLX, NVFP4, FP8, BF16 and EXL3/EXLR8
/// checkpoints of its native families; it has no GGUF, EXL2, AWQ or GPTQ path.
fn tensorfold_checks(r: &Recipe, hw: &Hardware) -> Vec<(Level, &'static str, String)> {
    let mut out = vec![];
    let arch = normalize_arch(&hw.arch);
    let metal = hw.os == "macos" && arch == "aarch64";
    let cuda = hw.os == "linux" && hw.accelerator().eq_ignore_ascii_case("nvidia");
    if !metal && !cuda {
        out.push((
            Level::Fail,
            "tensorfold",
            format!(
                "runs on Apple-silicon macOS or Linux with an NVIDIA GPU; this is {} {} ({})",
                hw.os,
                arch,
                hw.accelerator()
            ),
        ));
    }
    if let Some(q) = &r.quant {
        let f = q.format.to_ascii_lowercase();
        if ["gguf", "exl2", "awq", "gptq"].contains(&f.as_str()) {
            out.push((Level::Fail, "quant", format!("TensorFold does not load {f} checkpoints")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{parse_nvidia_smi, Hardware};
    use crate::recipe::Recipe;

    fn spark() -> Hardware {
        Hardware::assemble(
            "linux".into(),
            "aarch64".into(),
            "spark".into(),
            "".into(),
            20,
            119.7,
            Some(110.0),
            parse_nvidia_smi("NVIDIA GB10, [N/A], 12.1, 580\n"),
        )
    }

    fn mac() -> Hardware {
        let gpu = crate::hardware::Gpu {
            vendor: "apple".into(),
            name: "Apple M4".into(),
            memory_gb: None,
            compute_capability: None,
            driver: None,
        };
        Hardware::assemble("macos".into(), "aarch64".into(), "mac".into(), "".into(), 10, 16.0, Some(8.0), vec![gpu])
    }

    fn r(hw: &str) -> Recipe {
        Recipe::parse(&format!("[model]\nrepo=\"a/b\"\n[engine]\nname=\"vllm\"\n[hardware]\n{hw}"), "t").unwrap()
    }

    #[test]
    fn nvfp4_on_spark_ok_on_mac_fails() {
        let rec = r("min_memory_gb = 40\naccelerator=[\"nvidia\"]\nmin_compute_capability=\"10.0\"\narch=[\"aarch64\",\"x86_64\"]\n");
        let rep = check(&rec, &spark(), None, None);
        assert!(rep.compatible(), "{:?}", rep);
        assert_eq!(rep.level(), Level::Ok);
        let rep = check(&rec, &mac(), None, None);
        assert!(!rep.compatible());
        let reasons = rep.reasons(false).join(" | ");
        assert!(
            reasons.contains("accelerator") && reasons.contains("compute") && reasons.contains("memory"),
            "{reasons}"
        );
    }

    #[test]
    fn os_and_arch_mismatch() {
        let rec = r("os=[\"macos\"]\narch=[\"arm64\"]\n");
        let rep = check(&rec, &spark(), None, None);
        assert_eq!(rep.reasons(false), vec!["os: needs macos, this is linux"]);
        assert!(check(&rec, &mac(), None, None).compatible());
    }

    #[test]
    fn memory_estimate_from_size_and_tight_fit_warns() {
        let rec = r("");
        assert_eq!(required_memory_gb(&rec, None), None);
        assert_eq!(check(&rec, &spark(), None, None).level(), Level::Warn);
        const GIB: u64 = 1 << 30;
        let need = required_memory_gb(&rec, Some(100 * GIB)).unwrap();
        assert!((need - 120.0).abs() < 1e-9);
        assert_eq!(check(&rec, &spark(), None, Some(100 * GIB)).level(), Level::Fail);
        assert_eq!(check(&rec, &spark(), None, Some(92 * GIB)).level(), Level::Warn);
        assert_eq!(check(&rec, &spark(), None, Some(20 * GIB)).level(), Level::Ok);
    }

    #[test]
    fn tensorfold_platform_and_format() {
        let tf = |extra: &str| {
            Recipe::parse(
                &format!("[model]\nrepo=\"a/b\"\n{extra}[engine]\nname=\"tensorfold\"\n[hardware]\nmin_memory_gb=4\n"),
                "t",
            )
            .unwrap()
        };
        let mlx = tf("[quant]\nformat=\"mlx\"\nbits=4\n");
        assert_eq!(check(&mlx, &spark(), None, None).level(), Level::Ok);
        assert_eq!(check(&mlx, &mac(), None, None).level(), Level::Ok);
        let cpu_box =
            Hardware::assemble("linux".into(), "x86_64".into(), "box".into(), "".into(), 8, 64.0, Some(32.0), vec![]);
        let rep = check(&mlx, &cpu_box, None, None);
        assert!(!rep.compatible());
        assert!(rep.reasons(false)[0].starts_with("tensorfold: runs on Apple-silicon macOS"), "{:?}", rep);
        let gguf = tf("[quant]\nformat=\"gguf\"\n");
        assert_eq!(
            check(&gguf, &spark(), None, None).reasons(false),
            vec!["quant: TensorFold does not load gguf checkpoints"]
        );
    }

    #[test]
    fn multi_node_and_reserve() {
        let rec = Recipe::parse(
            "[model]\nrepo=\"a/b\"\n[engine]\nname=\"sglang\"\nargs=[\"--mem-fraction-static\",\"0.95\"]\n[hardware]\nnodes=2\nmin_memory_gb=10\n",
            "t",
        )
        .unwrap();
        assert_eq!(reserved_fraction(&rec), Some(0.95));
        let rep = check(&rec, &spark(), None, None);
        assert!(!rep.compatible());
        assert!(rep.checks.iter().any(|c| c.what == "reserve" && c.level == Level::Warn));

        // With [[nodes]] saying where each part runs, the span itself is fine.
        let duo = Recipe::parse(
            "[model]\nrepo=\"a/b\"\n[engine]\nname=\"custom\"\n[hardware]\nnodes=2\nmin_memory_gb=10\n\
             [[nodes]]\nname=\"head\"\nrole=\"head\"\nhost=\"local\"\ncommand=[\"sh\"]\n\
             [[nodes]]\nname=\"w1\"\nrole=\"worker\"\nhost=\"b\"\ncommand=[\"sh\"]\n",
            "t",
        )
        .unwrap();
        let rep = check(&duo, &spark(), None, None);
        assert!(rep.compatible(), "{:?}", rep.checks);
        assert!(rep.checks.iter().any(|c| c.what == "nodes" && c.detail.contains("local, b")));
    }
}
