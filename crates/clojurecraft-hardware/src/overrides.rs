//! Manual corrections applied on top of detection. Detection can be wrong or empty (missing vendor
//! tools, virtual machines, unusual drivers); the user's values always win, and they are kept
//! separately from the detected report so the original observation stays visible.

use serde::{Deserialize, Serialize};

use crate::{DetectionSource, GpuDevice, GpuVendor, HardwareReport};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareOverrides {
    /// Total system memory, replacing the detected value.
    #[serde(default)]
    pub total_ram_bytes: Option<u64>,
    /// Memory of the GPU used for inference. Replaces the largest detected GPU's memory, or adds a
    /// manually described GPU when none was detected.
    #[serde(default)]
    pub gpu: Option<ManualGpu>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualGpu {
    pub vendor: GpuVendor,
    pub vram_bytes: u64,
}

impl HardwareOverrides {
    pub fn is_empty(&self) -> bool {
        self.total_ram_bytes.is_none() && self.gpu.is_none()
    }

    /// The report with corrections applied. Corrected values are marked as manual.
    pub fn apply(&self, detected: &HardwareReport) -> HardwareReport {
        let mut report = detected.clone();
        if let Some(ram) = self.total_ram_bytes {
            report.total_ram_bytes = Some(ram);
            report.available_ram_bytes = Some(report.available_ram_bytes.map_or(ram, |a| a.min(ram)));
        }
        if let Some(manual) = &self.gpu {
            let target = report
                .gpus
                .iter_mut()
                .filter(|g| g.vendor == manual.vendor)
                .max_by_key(|g| g.vram_total_bytes.unwrap_or(0));
            match target {
                Some(gpu) => {
                    gpu.vram_total_bytes = Some(manual.vram_bytes);
                    gpu.source = DetectionSource::Manual;
                }
                None => report.gpus.push(GpuDevice {
                    vendor: manual.vendor,
                    name: format!("{:?} GPU (entered manually)", manual.vendor),
                    vram_total_bytes: Some(manual.vram_bytes),
                    vram_used_bytes: None,
                    unified_memory: manual.vendor == GpuVendor::Apple,
                    driver_version: None,
                    source: DetectionSource::Manual,
                }),
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, BudgetBasis, CpuInfo, GIB, default_budget};

    fn failed_detection() -> HardwareReport {
        HardwareReport {
            os: "linux".into(),
            arch: "x86_64".into(),
            total_ram_bytes: None,
            available_ram_bytes: None,
            cpu: CpuInfo::default(),
            gpus: vec![],
            notes: vec!["Could not read system memory: permission denied".into()],
        }
    }

    #[test]
    fn failed_detection_degrades_to_a_placeholder_until_corrected() {
        let detected = failed_detection();
        assert_eq!(
            default_budget(&detected, Backend::Cpu).basis,
            BudgetBasis::Fallback
        );
        let corrected = HardwareOverrides {
            total_ram_bytes: Some(32 * GIB),
            gpu: None,
        }
        .apply(&detected);
        let budget = default_budget(&corrected, Backend::Cpu);
        assert_eq!(budget.basis, BudgetBasis::SystemRam);
        assert_eq!(budget.device_bytes, 16 * GIB);
    }

    #[test]
    fn a_manual_gpu_enables_its_backend() {
        let corrected = HardwareOverrides {
            total_ram_bytes: Some(32 * GIB),
            gpu: Some(ManualGpu {
                vendor: GpuVendor::Nvidia,
                vram_bytes: 12 * GIB,
            }),
        }
        .apply(&failed_detection());
        assert_eq!(corrected.recommended_backend(), Backend::Cuda);
        let budget = default_budget(&corrected, Backend::Cuda);
        assert_eq!(budget.basis, BudgetBasis::DedicatedVram);
        assert!(budget.device_bytes > 11 * GIB);
        assert_eq!(corrected.gpus[0].source, DetectionSource::Manual);
    }

    #[test]
    fn a_correction_replaces_the_detected_memory_of_the_same_vendor() {
        let mut detected = failed_detection();
        detected.gpus.push(GpuDevice {
            vendor: GpuVendor::Amd,
            name: "Radeon".into(),
            vram_total_bytes: Some(512 * 1024 * 1024),
            vram_used_bytes: None,
            unified_memory: false,
            driver_version: None,
            source: DetectionSource::Sysfs,
        });
        let corrected = HardwareOverrides {
            total_ram_bytes: None,
            gpu: Some(ManualGpu {
                vendor: GpuVendor::Amd,
                vram_bytes: 16 * GIB,
            }),
        }
        .apply(&detected);
        assert_eq!(corrected.gpus.len(), 1);
        assert_eq!(corrected.gpus[0].vram_total_bytes, Some(16 * GIB));
        assert_eq!(corrected.gpus[0].name, "Radeon");
        // The detected report itself is left untouched for display.
        assert_eq!(detected.gpus[0].vram_total_bytes, Some(512 * 1024 * 1024));
    }
}
