//! Pure parsers for probe output, kept separate from the probes so they can be tested with
//! captured fixtures from machines we do not have.

use crate::{DetectionSource, GpuDevice, GpuVendor, MIB};

/// Returns `(MemTotal, MemAvailable)` in bytes from `/proc/meminfo` content.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kib: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            Some(kib * 1024)
        })
    };
    (field("MemTotal"), field("MemAvailable"))
}

/// Parses `nvidia-smi --query-gpu=name,memory.total,memory.used,driver_version
/// --format=csv,noheader,nounits` output (memory in MiB).
pub fn parse_nvidia_smi_csv(text: &str) -> Vec<GpuDevice> {
    text.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split(',').map(str::trim).collect();
            if cols.len() < 4 || cols[0].is_empty() {
                return None;
            }
            let mib = |s: &str| s.parse::<u64>().ok().map(|v| v * MIB);
            Some(GpuDevice {
                vendor: GpuVendor::Nvidia,
                name: cols[0].to_owned(),
                vram_total_bytes: mib(cols[1]),
                vram_used_bytes: mib(cols[2]),
                unified_memory: false,
                driver_version: Some(cols[3].to_owned()).filter(|s| !s.is_empty()),
                source: DetectionSource::NvidiaSmi,
            })
        })
        .collect()
}

/// Extracts the instruction-set flags llama.cpp cares about from `/proc/cpuinfo`.
pub fn parse_proc_cpuinfo_flags(text: &str) -> (Option<String>, Vec<String>) {
    const RELEVANT: &[&str] = &[
        "sse4_2",
        "avx",
        "avx2",
        "fma",
        "f16c",
        "avx512f",
        "avx512bw",
        "avx512vl",
        "avx512_vnni",
        "avx512_bf16",
        "avx_vnni",
        "amx_tile",
        "amx_int8",
        "amx_bf16",
        "asimd",
        "sve",
        "i8mm",
    ];
    let model = text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "model name").then(|| v.trim().to_owned())
    });
    let flags = text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            matches!(k.trim(), "flags" | "Features").then_some(v)
        })
        .unwrap_or("");
    let mut features: Vec<String> = flags
        .split_whitespace()
        .filter(|f| RELEVANT.contains(f))
        .map(str::to_owned)
        .collect();
    features.sort();
    features.dedup();
    (model, features)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GIB;

    #[test]
    fn meminfo_reports_bytes() {
        let text = "MemTotal:       32768000 kB\nMemFree:  100 kB\nMemAvailable:   16384000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            (Some(32768000 * 1024), Some(16384000 * 1024))
        );
    }

    #[test]
    fn meminfo_tolerates_missing_fields() {
        assert_eq!(parse_meminfo("garbage"), (None, None));
    }

    #[test]
    fn nvidia_smi_multi_gpu() {
        let text =
            "NVIDIA GeForce RTX 3070, 8192, 814, 595.99.02\nNVIDIA RTX A6000, 49140, [N/A], 595.99.02\n";
        let gpus = parse_nvidia_smi_csv(text);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].vram_total_bytes, Some(8 * GIB));
        assert_eq!(gpus[1].vram_used_bytes, None);
        assert_eq!(gpus[1].driver_version.as_deref(), Some("595.99.02"));
    }

    #[test]
    fn cpu_flags_are_filtered() {
        let text = "model name\t: AMD Ryzen 7 5800X\nflags\t\t: fpu sse4_2 avx avx2 fma f16c bogus\n";
        let (model, feats) = parse_proc_cpuinfo_flags(text);
        assert_eq!(model.as_deref(), Some("AMD Ryzen 7 5800X"));
        assert_eq!(feats, vec!["avx", "avx2", "f16c", "fma", "sse4_2"]);
    }
}
