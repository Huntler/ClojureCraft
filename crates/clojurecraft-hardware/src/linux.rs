//! Linux probes: `/proc`, sysfs, and `nvidia-smi` when it is installed.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::parse::{parse_meminfo, parse_nvidia_smi_csv, parse_proc_cpuinfo_flags};
use crate::{DetectionSource, GpuDevice, GpuVendor, HardwareReport};

const PCI_VENDOR_NVIDIA: &str = "0x10de";
const PCI_VENDOR_AMD: &str = "0x1002";
const PCI_VENDOR_INTEL: &str = "0x8086";

pub(crate) fn probe(report: &mut HardwareReport) {
    match fs::read_to_string("/proc/meminfo") {
        Ok(text) => (report.total_ram_bytes, report.available_ram_bytes) = parse_meminfo(&text),
        Err(e) => report.notes.push(format!("Could not read system memory: {e}")),
    }
    if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
        let (model, features) = parse_proc_cpuinfo_flags(&text);
        report.cpu.model = model;
        report.cpu.features = features;
    }
    let nvidia = probe_nvidia(report);
    probe_drm(report, nvidia);
}

fn probe_nvidia(report: &mut HardwareReport) -> bool {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.used,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let gpus = parse_nvidia_smi_csv(&String::from_utf8_lossy(&o.stdout));
            let found = !gpus.is_empty();
            report.gpus.extend(gpus);
            found
        }
        Ok(o) => {
            report.notes.push(format!("nvidia-smi failed ({})", o.status));
            false
        }
        // Not installed: normal on machines without the proprietary NVIDIA driver.
        Err(_) => false,
    }
}

/// Walks `/sys/class/drm/card*/device` for display controllers. AMD exposes VRAM size there;
/// NVIDIA and Intel are only listed when no better source reported them.
fn probe_drm(report: &mut HardwareReport, nvidia_reported: bool) {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return;
    };
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = entry.path().join("device");
        let Ok(canonical) = fs::canonicalize(&dev) else {
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        let vendor_id = read_trim(&dev.join("vendor")).unwrap_or_default();
        let vendor = match vendor_id.as_str() {
            PCI_VENDOR_NVIDIA if nvidia_reported => continue,
            PCI_VENDOR_NVIDIA => GpuVendor::Nvidia,
            PCI_VENDOR_AMD => GpuVendor::Amd,
            PCI_VENDOR_INTEL => GpuVendor::Intel,
            _ => GpuVendor::Other,
        };
        let vram = read_trim(&dev.join("mem_info_vram_total")).and_then(|s| s.parse().ok());
        let used = read_trim(&dev.join("mem_info_vram_used")).and_then(|s| s.parse().ok());
        let device_id = read_trim(&dev.join("device")).unwrap_or_default();
        report.gpus.push(GpuDevice {
            vendor,
            name: format!("PCI {vendor_id}:{device_id}"),
            vram_total_bytes: vram,
            vram_used_bytes: used,
            unified_memory: false,
            driver_version: None,
            source: DetectionSource::Sysfs,
        });
        if vendor == GpuVendor::Nvidia {
            report
                .notes
                .push("An NVIDIA GPU was found but nvidia-smi is unavailable; VRAM is unknown.".to_owned());
        }
    }
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}
