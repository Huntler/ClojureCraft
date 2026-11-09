//! Hardware discovery for local inference.
//!
//! Detection never requires elevated privileges and never fails as a whole: every probe that
//! cannot run (missing tool, unreadable file, unsupported platform) is recorded as a note and the
//! report degrades to whatever could be observed. Callers must allow manual correction of the
//! result, because vendor tools and kernel interfaces differ between machines.
//!
//! All memory figures derived here (budgets, usable fractions) are estimates.

mod budget;
mod overrides;
mod parse;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

pub use budget::{BudgetBasis, MemoryBudget, default_budget};
pub use overrides::{HardwareOverrides, ManualGpu};
pub use parse::{parse_meminfo, parse_nvidia_smi_csv, parse_proc_cpuinfo_flags};

use serde::{Deserialize, Serialize};

pub const GIB: u64 = 1024 * 1024 * 1024;
pub const MIB: u64 = 1024 * 1024;

/// An inference backend offered by the pinned llama.cpp builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Metal,
    Cuda,
    Rocm,
    Vulkan,
    Cpu,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Metal => "Metal",
            Backend::Cuda => "CUDA",
            Backend::Rocm => "ROCm/HIP",
            Backend::Vulkan => "Vulkan",
            Backend::Cpu => "CPU",
        }
    }

    pub fn uses_gpu(self) -> bool {
        self != Backend::Cpu
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuVendor {
    Apple,
    Nvidia,
    Amd,
    Intel,
    Other,
}

/// Where a GPU observation came from, so the UI can explain (and users can distrust) it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DetectionSource {
    NvidiaSmi,
    Sysfs,
    Sysctl,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub vendor: GpuVendor,
    pub name: String,
    /// Dedicated device memory. `None` when the probe could not report it.
    pub vram_total_bytes: Option<u64>,
    pub vram_used_bytes: Option<u64>,
    /// True when the GPU shares system memory (Apple Silicon).
    pub unified_memory: bool,
    pub driver_version: Option<String>,
    pub source: DetectionSource,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuInfo {
    pub model: Option<String>,
    pub logical_cores: usize,
    /// Instruction-set extensions relevant to llama.cpp CPU kernels (e.g. `avx2`, `avx512f`, `neon`).
    pub features: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareReport {
    pub os: String,
    pub arch: String,
    pub total_ram_bytes: Option<u64>,
    pub available_ram_bytes: Option<u64>,
    pub cpu: CpuInfo,
    pub gpus: Vec<GpuDevice>,
    /// Probe failures and caveats, phrased for display.
    pub notes: Vec<String>,
}

impl HardwareReport {
    /// Backends worth offering on this machine, best first. CPU is always last and always present.
    pub fn candidate_backends(&self) -> Vec<Backend> {
        let mut out = Vec::new();
        let has = |v: GpuVendor| self.gpus.iter().any(|g| g.vendor == v);
        if self.os == "macos" && has(GpuVendor::Apple) {
            out.push(Backend::Metal);
        }
        if self.os == "linux" {
            if has(GpuVendor::Nvidia) {
                out.push(Backend::Cuda);
            }
            if has(GpuVendor::Amd) {
                out.push(Backend::Rocm);
            }
            if self.gpus.iter().any(|g| g.vendor != GpuVendor::Apple) {
                out.push(Backend::Vulkan);
            }
        }
        out.push(Backend::Cpu);
        out
    }

    pub fn recommended_backend(&self) -> Backend {
        self.candidate_backends()[0]
    }

    /// The largest dedicated or unified GPU memory pool usable by `backend`.
    pub fn gpu_memory_for(&self, backend: Backend) -> Option<&GpuDevice> {
        let vendor_ok = |g: &&GpuDevice| match backend {
            Backend::Metal => g.vendor == GpuVendor::Apple,
            Backend::Cuda => g.vendor == GpuVendor::Nvidia,
            Backend::Rocm => g.vendor == GpuVendor::Amd,
            Backend::Vulkan => g.vendor != GpuVendor::Apple,
            Backend::Cpu => false,
        };
        self.gpus
            .iter()
            .filter(vendor_ok)
            .max_by_key(|g| g.vram_total_bytes.unwrap_or(0))
    }
}

/// Bytes one process may keep in use on an Apple GPU before Metal fails its commands: Metal's
/// default working set (3/4 of memory above 32 GiB, else 2/3), or the `iogpu.wired_limit_mb` the
/// user set instead.
pub fn metal_working_set(total_ram: u64, wired_limit: Option<u64>) -> u64 {
    wired_limit.unwrap_or(if total_ram > 32 * GIB {
        total_ram / 4 * 3
    } else {
        total_ram / 3 * 2
    })
}

/// The GPU memory limit set with `iogpu.wired_limit_mb`, if any.
pub fn metal_wired_limit() -> Option<u64> {
    #[cfg(target_os = "macos")]
    return macos::wired_limit();
    #[cfg(not(target_os = "macos"))]
    None
}

/// Probes the current machine.
pub fn detect() -> HardwareReport {
    let mut report = HardwareReport {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        total_ram_bytes: None,
        available_ram_bytes: None,
        cpu: CpuInfo {
            logical_cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            ..CpuInfo::default()
        },
        gpus: Vec::new(),
        notes: Vec::new(),
    };
    #[cfg(target_os = "linux")]
    linux::probe(&mut report);
    #[cfg(target_os = "macos")]
    macos::probe(&mut report);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    report
        .notes
        .push("This operating system is not supported; configure memory manually.".to_owned());
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(os: &str, gpus: Vec<GpuDevice>) -> HardwareReport {
        HardwareReport {
            os: os.into(),
            arch: "x86_64".into(),
            total_ram_bytes: Some(32 * GIB),
            available_ram_bytes: Some(24 * GIB),
            cpu: CpuInfo::default(),
            gpus,
            notes: vec![],
        }
    }

    fn gpu(vendor: GpuVendor, vram: u64) -> GpuDevice {
        GpuDevice {
            vendor,
            name: "test".into(),
            vram_total_bytes: Some(vram),
            vram_used_bytes: None,
            unified_memory: vendor == GpuVendor::Apple,
            driver_version: None,
            source: DetectionSource::Manual,
        }
    }

    #[test]
    fn metal_working_set_follows_memory_size_and_the_wired_limit() {
        assert_eq!(metal_working_set(36 * GIB, None), 27 * GIB);
        assert_eq!(metal_working_set(16 * GIB, None), 16 * GIB / 3 * 2);
        assert_eq!(metal_working_set(36 * GIB, Some(32 * GIB)), 32 * GIB);
    }

    #[test]
    fn nvidia_linux_prefers_cuda_then_vulkan_then_cpu() {
        let r = report("linux", vec![gpu(GpuVendor::Nvidia, 8 * GIB)]);
        assert_eq!(
            r.candidate_backends(),
            vec![Backend::Cuda, Backend::Vulkan, Backend::Cpu]
        );
    }

    #[test]
    fn no_gpu_falls_back_to_cpu_only() {
        let r = report("linux", vec![]);
        assert_eq!(r.candidate_backends(), vec![Backend::Cpu]);
        assert_eq!(r.recommended_backend(), Backend::Cpu);
    }

    #[test]
    fn apple_silicon_prefers_metal() {
        let r = report("macos", vec![gpu(GpuVendor::Apple, 36 * GIB)]);
        assert_eq!(r.candidate_backends(), vec![Backend::Metal, Backend::Cpu]);
    }
}
