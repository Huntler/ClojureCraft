use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a sandboxed process may do. Everything not listed is denied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    /// Files and directory trees the process may read.
    pub read: Vec<PathBuf>,
    /// Directory trees whose files may be read, mapped, and executed (the runtime itself and the
    /// system libraries it links against).
    pub exec: Vec<PathBuf>,
    /// CPU time limit in seconds (the kernel kills the process when it is exceeded).
    pub cpu_seconds: u64,
    /// Address-space limit in bytes. `None` leaves it unlimited; JVMs reserve large virtual
    /// ranges, so heap limits are usually enforced by runtime flags instead.
    pub address_space_bytes: Option<u64>,
    pub max_open_files: u64,
    /// Directory trees the process may read, write, and map executable code from: a private
    /// scratch directory, for libraries that unpack native code into the temporary directory.
    #[serde(default)]
    pub write: Vec<PathBuf>,
}

/// Largest file a process with a scratch directory may write; without one, nothing may be
/// written at all.
pub const MAX_SCRATCH_FILE_BYTES: u64 = 256 * 1024 * 1024;

impl Policy {
    /// The file-size limit: zero unless the policy grants a scratch directory.
    pub fn max_file_bytes(&self) -> u64 {
        if self.write.is_empty() {
            0
        } else {
            MAX_SCRATCH_FILE_BYTES
        }
    }

    /// System directories every dynamically linked program needs, filtered to those present.
    pub fn system_library_dirs() -> Vec<PathBuf> {
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &["/usr/lib", "/System/Library", "/Library/Apple/System/Library"]
        } else {
            &["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/usr/lib32"]
        };
        candidates
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect()
    }

    /// Read-only files needed by the dynamic loader and runtime on this platform.
    pub fn system_read_files() -> Vec<PathBuf> {
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &["/dev/urandom", "/dev/random", "/private/etc/localtime"]
        } else {
            &["/etc/ld.so.cache", "/dev/urandom", "/dev/random", "/proc/self"]
        };
        candidates
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect()
    }

    pub fn to_arg(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_arg(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}
