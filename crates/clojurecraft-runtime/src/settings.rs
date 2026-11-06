//! Inference settings chosen during setup or later in settings: backend, active model, context
//! limit, GPU offload, memory budget, and hardware corrections.
//!
//! Stored as JSON and replaced atomically. An unreadable file never blocks the application: the
//! defaults apply, the problem is reported, and the bad file is kept next to it for inspection.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

use clojurecraft_hardware::{Backend, HardwareOverrides, MemoryBudget};
use serde::{Deserialize, Serialize};

const FORMAT: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupState {
    /// First start: setup is offered.
    #[default]
    NotStarted,
    /// The user chose to configure things later.
    Skipped,
    Completed,
}

/// How many layers llama.cpp places on the GPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "layers")]
pub enum GpuLayers {
    /// Derived from the memory budget and the model's estimated size.
    #[default]
    Auto,
    All,
    Count(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetOverride {
    pub device_bytes: u64,
    pub host_spill_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InferenceSettings {
    pub format: u32,
    pub setup: SetupState,
    /// `None` follows the recommendation for the detected hardware.
    pub backend: Option<Backend>,
    /// Key of the installed model used for generation.
    pub active_model: Option<String>,
    /// Context tokens; `None` uses the catalog default, capped by the model.
    pub context_limit: Option<u32>,
    pub gpu_layers: GpuLayers,
    /// Replaces the budget derived from hardware.
    pub budget: Option<BudgetOverride>,
    pub hardware: HardwareOverrides,
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            format: FORMAT,
            setup: SetupState::default(),
            backend: None,
            active_model: None,
            context_limit: None,
            gpu_layers: GpuLayers::default(),
            budget: None,
            hardware: HardwareOverrides::default(),
        }
    }
}

impl InferenceSettings {
    /// Reads settings; on any problem returns the defaults and a message for the user.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("Settings could not be read ({e}); defaults are in use.")),
                );
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(s) if s.format <= FORMAT => (s, None),
            Ok(s) => (
                Self::default(),
                Some(format!(
                    "Settings were written by a newer version (format {}); defaults are in use and the file was left unchanged.",
                    s.format
                )),
            ),
            Err(e) => {
                let mut kept = path.as_os_str().to_owned();
                kept.push(".unreadable");
                let _ = fs::copy(path, &kept);
                (
                    Self::default(),
                    Some(format!(
                        "Settings were unreadable ({e}); defaults are in use. The old file was kept as {}.",
                        Path::new(&kept).display()
                    )),
                )
            }
        }
    }

    /// Writes settings with write-then-rename, so a crash leaves the old or the new file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// The budget in effect: the override, else the one derived from hardware.
    pub fn effective_budget(&self, derived: MemoryBudget) -> MemoryBudget {
        match self.budget {
            Some(o) => MemoryBudget {
                device_bytes: o.device_bytes,
                host_spill_bytes: o.host_spill_bytes,
                basis: derived.basis,
            },
            None => derived,
        }
    }

    /// Context tokens to use with a model supporting `model_max` tokens.
    pub fn effective_context(&self, default: u32, model_max: Option<u32>) -> u32 {
        let wanted = self.context_limit.unwrap_or(default).max(512);
        model_max.map_or(wanted, |m| wanted.min(m))
    }
}

#[cfg(test)]
mod tests {
    use clojurecraft_hardware::{BudgetBasis, GIB, GpuVendor, ManualGpu};

    use super::*;

    #[test]
    fn round_trips_and_defaults_when_missing() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("settings.json");
        assert_eq!(
            InferenceSettings::load(&path),
            (InferenceSettings::default(), None)
        );
        let s = InferenceSettings {
            setup: SetupState::Completed,
            backend: Some(Backend::Vulkan),
            active_model: Some("qwen2.5-coder-7b-instruct/Q4_K_M".into()),
            context_limit: Some(8192),
            gpu_layers: GpuLayers::Count(20),
            budget: Some(BudgetOverride {
                device_bytes: 6 * GIB,
                host_spill_bytes: 0,
            }),
            hardware: HardwareOverrides {
                total_ram_bytes: Some(32 * GIB),
                gpu: Some(ManualGpu {
                    vendor: GpuVendor::Nvidia,
                    vram_bytes: 8 * GIB,
                }),
            },
            ..InferenceSettings::default()
        };
        s.save(&path).expect("save");
        assert_eq!(InferenceSettings::load(&path), (s, None));
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn unreadable_settings_fall_back_and_are_kept() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{ broken").expect("write");
        let (s, problem) = InferenceSettings::load(&path);
        assert_eq!(s, InferenceSettings::default());
        assert!(problem.expect("problem").contains("defaults are in use"));
        assert!(dir.path().join("settings.json.unreadable").exists());
    }

    #[test]
    fn newer_settings_are_not_overwritten_silently() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("settings.json");
        fs::write(&path, br#"{"format": 99, "setup": "completed"}"#).expect("write");
        let (s, problem) = InferenceSettings::load(&path);
        assert_eq!(s.setup, SetupState::NotStarted);
        assert!(problem.expect("problem").contains("newer version"));
    }

    #[test]
    fn overrides_and_limits() {
        let derived = MemoryBudget {
            device_bytes: 7 * GIB,
            host_spill_bytes: 16 * GIB,
            basis: BudgetBasis::DedicatedVram,
        };
        let mut s = InferenceSettings::default();
        assert_eq!(s.effective_budget(derived), derived);
        s.budget = Some(BudgetOverride {
            device_bytes: 5 * GIB,
            host_spill_bytes: 0,
        });
        assert_eq!(s.effective_budget(derived).device_bytes, 5 * GIB);
        assert_eq!(s.effective_context(16384, Some(8192)), 8192);
        s.context_limit = Some(4096);
        assert_eq!(s.effective_context(16384, Some(32768)), 4096);
    }
}
