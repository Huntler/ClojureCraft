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

    /// Unix socket on which the running application serves MCP clients (through
    /// `clojurecraft mcp`): `mcp.sock` in the data root, or, when that path is too long for a
    /// socket address, a name derived from the data root in the per-user runtime directory
    /// (`$XDG_RUNTIME_DIR`, else the temporary directory, which is per-user on macOS).
    pub fn mcp_socket(&self) -> PathBuf {
        use std::hash::{DefaultHasher, Hash, Hasher};
        /// Socket addresses hold 104 (macOS) or 108 (Linux) bytes including the terminator.
        const MAX_SOCKET_PATH: usize = 100;
        let path = self.root.join("mcp.sock");
        if path.as_os_str().len() <= MAX_SOCKET_PATH {
            return path;
        }
        // Same binary for the application and the bridge, so the hash agrees between them.
        let mut h = DefaultHasher::new();
        self.root.hash(&mut h);
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map_or_else(std::env::temp_dir, PathBuf::from);
        dir.join(format!("clojurecraft-mcp-{:016x}.sock", h.finish()))
    }

    /// File holding window geometry, panel sizes, text size, and the last open function.
    pub fn ui_state(&self) -> PathBuf {
        self.root.join("ui-state.ron")
    }
}
