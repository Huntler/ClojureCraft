//! macOS probes via `sysctl`. On Apple Silicon the GPU shares system memory.

use std::process::Command;

use crate::{DetectionSource, GpuDevice, GpuVendor, HardwareReport};

fn sysctl(name: &str) -> Option<String> {
    let out = Command::new("/usr/sbin/sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `iogpu.wired_limit_mb` when set (0 means the default).
pub(crate) fn wired_limit() -> Option<u64> {
    let mb: u64 = sysctl("iogpu.wired_limit_mb")?.parse().ok()?;
    (mb > 0).then(|| mb * 1024 * 1024)
}

pub(crate) fn probe(report: &mut HardwareReport) {
    report.total_ram_bytes = sysctl("hw.memsize").and_then(|s| s.parse().ok());
    if report.total_ram_bytes.is_none() {
        report.notes.push("Could not read hw.memsize.".to_owned());
    }
    report.cpu.model = sysctl("machdep.cpu.brand_string");
    let apple_silicon = sysctl("hw.optional.arm64").as_deref() == Some("1");
    if apple_silicon {
        report.cpu.features = vec!["neon".to_owned()];
        for (feature, key) in [
            ("dotprod", "FEAT_DotProd"),
            ("i8mm", "FEAT_I8MM"),
            ("bf16", "FEAT_BF16"),
        ] {
            if sysctl(&format!("hw.optional.arm.{key}")).as_deref() == Some("1") {
                report.cpu.features.push(feature.to_owned());
            }
        }
        report.gpus.push(GpuDevice {
            vendor: GpuVendor::Apple,
            name: report.cpu.model.clone().unwrap_or_else(|| "Apple GPU".to_owned()),
            vram_total_bytes: report.total_ram_bytes,
            vram_used_bytes: None,
            unified_memory: true,
            driver_version: None,
            source: DetectionSource::Sysctl,
        });
    } else {
        report
            .notes
            .push("Intel Macs are not a supported platform; only CPU inference is offered.".to_owned());
    }
}
