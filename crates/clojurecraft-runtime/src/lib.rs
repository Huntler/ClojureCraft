//! Installation of the third-party components ClojureCraft runs but does not ship: llama.cpp,
//! a Java runtime, and the Clojure jars.
//!
//! Every artifact is pinned in a manifest compiled into the application. Downloads resume into
//! `.part` files, are checked against the pinned size and SHA-256, and are extracted into a
//! staging directory that only becomes visible under its final name after extraction and any
//! smoke test succeed. A previously active installation stays in place until then.

pub mod catalog;
pub mod components;
pub mod download;
pub mod install;
pub mod manifest;
pub mod paths;

pub use download::{Cancel, DownloadError, Progress, download_verified};
pub use install::{InstallError, Installed};
pub use manifest::{Manifest, ManifestError};
pub use paths::DataDirs;

/// The platform key used in manifests and catalogs, e.g. `linux-x86_64` or `macos-aarch64`.
pub fn platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
