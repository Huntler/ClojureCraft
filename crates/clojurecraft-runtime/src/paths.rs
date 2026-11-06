//! Locations of application data outside the source tree and outside exported functions.

use std::path::{Path, PathBuf};

/// Overrides the data root (used by tests and development tooling).
pub const DATA_DIR_ENV: &str = "CLOJURECRAFT_DATA_DIR";

#[derive(Clone, Debug)]
pub struct DataDirs {
    root: PathBuf,
}

impl DataDirs {
    /// `$CLOJURECRAFT_DATA_DIR`, else the platform data directory
    /// (`~/.local/share/clojurecraft` on Linux, `~/Library/Application Support/ClojureCraft` on macOS).
    pub fn discover() -> Option<Self> {
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
            return Some(Self::at(PathBuf::from(dir)));
        }
        directories::ProjectDirs::from("", "", "ClojureCraft").map(|p| Self::at(p.data_dir().to_owned()))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Downloaded archives and model files in transit.
    pub fn downloads(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// Installed runtime components, one directory per component.
    pub fn runtimes(&self) -> PathBuf {
        self.root.join("runtimes")
    }

    pub fn models(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Saved functions, revision history, and results.
    pub fn library(&self) -> PathBuf {
        self.root.join("library")
    }

    /// Inference settings (backend, active model, budget, hardware corrections).
    pub fn settings(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    /// File holding window geometry, panel sizes, text size, and the last open function.
    pub fn ui_state(&self) -> PathBuf {
        self.root.join("ui-state.ron")
    }
}
