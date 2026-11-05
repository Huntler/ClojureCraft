//! Default inference memory budgets. These are starting points the user can override, not
//! guarantees: drivers, display servers, and other applications all consume memory we cannot see.

use serde::{Deserialize, Serialize};

use crate::{Backend, GIB, HardwareReport, MIB};

/// Memory kept free on a dedicated GPU for the display server, driver, and other processes.
const DEDICATED_GPU_RESERVE: u64 = 768 * MIB;
/// Share of Apple Silicon unified memory given to inference by default (36 GB -> 18 GB).
const UNIFIED_FRACTION: f64 = 0.5;
/// Share of system RAM given to CPU inference by default.
const CPU_RAM_FRACTION: f64 = 0.5;
/// RAM always left for the OS, the application, and the Clojure runtime.
const SYSTEM_RESERVE: u64 = 4 * GIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetBasis {
    DedicatedVram,
    UnifiedMemory,
    SystemRam,
    /// Nothing could be detected; the value is a conservative placeholder.
    Fallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    /// Estimated bytes available for weights, KV cache, and compute buffers on the primary device.
    pub device_bytes: u64,
    /// Estimated system RAM usable for layers that do not fit on the device (partial offload).
    /// Zero for unified-memory and CPU-only budgets, where `device_bytes` already is system RAM.
    pub host_spill_bytes: u64,
    pub basis: BudgetBasis,
}

impl MemoryBudget {
    pub fn total_bytes(&self) -> u64 {
        self.device_bytes + self.host_spill_bytes
    }
}

fn fraction(bytes: u64, f: f64) -> u64 {
    // Truncation is intended: budgets are coarse estimates.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let v = (bytes as f64 * f) as u64;
    v
}

/// Derives the default budget for running `backend` on the reported hardware.
pub fn default_budget(report: &HardwareReport, backend: Backend) -> MemoryBudget {
    let ram = report.total_ram_bytes;
    let cpu_budget = |ram: u64| fraction(ram, CPU_RAM_FRACTION).min(ram.saturating_sub(SYSTEM_RESERVE));
    match (backend, report.gpu_memory_for(backend), ram) {
        (Backend::Metal, Some(gpu), _) => {
            let unified = gpu.vram_total_bytes.or(ram).unwrap_or(0);
            MemoryBudget {
                device_bytes: fraction(unified, UNIFIED_FRACTION),
                host_spill_bytes: 0,
                basis: BudgetBasis::UnifiedMemory,
            }
        }
        (b, Some(gpu), ram) if b.uses_gpu() && gpu.vram_total_bytes.is_some() => {
            let vram = gpu.vram_total_bytes.unwrap_or(0);
            MemoryBudget {
                device_bytes: vram.saturating_sub(DEDICATED_GPU_RESERVE),
                host_spill_bytes: ram.map(cpu_budget).unwrap_or(0),
                basis: BudgetBasis::DedicatedVram,
            }
        }
        (_, _, Some(ram)) => MemoryBudget {
            device_bytes: cpu_budget(ram),
            host_spill_bytes: 0,
            basis: BudgetBasis::SystemRam,
        },
        _ => MemoryBudget {
            device_bytes: 4 * GIB,
            host_spill_bytes: 0,
            basis: BudgetBasis::Fallback,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CpuInfo, DetectionSource, GpuDevice, GpuVendor};

    fn report(os: &str, ram: Option<u64>, gpus: Vec<GpuDevice>) -> HardwareReport {
        HardwareReport {
            os: os.into(),
            arch: "x".into(),
            total_ram_bytes: ram,
            available_ram_bytes: ram,
            cpu: CpuInfo::default(),
            gpus,
            notes: vec![],
        }
    }

    fn gpu(vendor: GpuVendor, vram: Option<u64>) -> GpuDevice {
        GpuDevice {
            vendor,
            name: "g".into(),
            vram_total_bytes: vram,
            vram_used_bytes: None,
            unified_memory: vendor == GpuVendor::Apple,
            driver_version: None,
            source: DetectionSource::Manual,
        }
    }

    #[test]
    fn reference_mac_gets_half_of_unified_memory() {
        let r = report(
            "macos",
            Some(36 * GIB),
            vec![gpu(GpuVendor::Apple, Some(36 * GIB))],
        );
        let b = default_budget(&r, Backend::Metal);
        assert_eq!(b.device_bytes, 18 * GIB);
        assert_eq!(b.basis, BudgetBasis::UnifiedMemory);
    }

    #[test]
    fn dedicated_gpu_keeps_reserve_and_offers_ram_spill() {
        let r = report(
            "linux",
            Some(32 * GIB),
            vec![gpu(GpuVendor::Nvidia, Some(8 * GIB))],
        );
        let b = default_budget(&r, Backend::Cuda);
        assert_eq!(b.device_bytes, 8 * GIB - 768 * MIB);
        assert_eq!(b.host_spill_bytes, 16 * GIB);
        assert_eq!(b.basis, BudgetBasis::DedicatedVram);
    }

    #[test]
    fn unknown_vram_falls_back_to_ram() {
        let r = report("linux", Some(16 * GIB), vec![gpu(GpuVendor::Intel, None)]);
        let b = default_budget(&r, Backend::Vulkan);
        assert_eq!(b.basis, BudgetBasis::SystemRam);
        assert_eq!(b.device_bytes, 8 * GIB);
    }

    #[test]
    fn small_machines_keep_system_reserve() {
        let r = report("linux", Some(6 * GIB), vec![]);
        assert_eq!(default_budget(&r, Backend::Cpu).device_bytes, 2 * GIB);
    }

    #[test]
    fn nothing_detected_uses_placeholder() {
        let r = report("linux", None, vec![]);
        assert_eq!(default_budget(&r, Backend::Cpu).basis, BudgetBasis::Fallback);
    }
}
