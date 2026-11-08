//! Installation and lookup of the concrete runtime components.

use std::path::{Path, PathBuf};
use std::process::Command;

use clojurecraft_hardware::Backend;

use crate::download::{Cancel, DownloadError, Progress, download_verified};
use crate::install::{ComponentDir, InstallError, Installed, extract_tar_gz, extract_tar_gz_stripped};
use crate::manifest::{Artifact, Manifest};
use crate::paths::DataDirs;

#[derive(Debug, thiserror::Error)]
pub enum ComponentError {
    #[error("no pinned {component} build for {platform}{backend}")]
    Unavailable {
        component: &'static str,
        platform: String,
        backend: String,
    },
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(
        "not enough disk space: {} needed, {} available. Free some space and try again.",
        crate::catalog::format_gib(*needed), crate::catalog::format_gib(*available)
    )]
    InsufficientSpace { needed: u64, available: u64 },
}

/// Refuses to start when the data directory lacks room for the downloads still missing plus their
/// extracted contents (estimated at twice the archive size).
fn ensure_space(dirs: &DataDirs, artifacts: &[&Artifact]) -> Result<(), ComponentError> {
    let missing: u64 = artifacts
        .iter()
        .filter(|a| !download_path(dirs, a).is_file())
        .map(|a| a.size)
        .sum();
    let total: u64 = artifacts.iter().map(|a| a.size).sum();
    let needed = missing + 2 * total + crate::models::DISK_MARGIN;
    match crate::models::available_space(dirs.root()) {
        Some(available) if available < needed => Err(ComponentError::InsufficientSpace { needed, available }),
        _ => Ok(()),
    }
}

/// An installed llama.cpp build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlamaBuild {
    pub version: String,
    pub release: String,
    pub backend: Option<Backend>,
    pub active: bool,
    /// Built from the release pinned by this application.
    pub pinned: bool,
}

/// Every installed llama.cpp build, newest release first.
pub fn llama_builds(manifest: &Manifest, dirs: &DataDirs) -> Vec<LlamaBuild> {
    let comp = component(dirs, LLAMA_CPP);
    let active = comp.active().map(|a| a.version);
    let mut out: Vec<LlamaBuild> = comp
        .installed()
        .into_iter()
        .map(|version| {
            let (release, backend) = split_version(&version);
            LlamaBuild {
                active: active.as_deref() == Some(version.as_str()),
                pinned: release == manifest.llama_cpp.release,
                version,
                release,
                backend,
            }
        })
        .collect();
    out.sort_by(|a, b| b.release.cmp(&a.release).then(a.version.cmp(&b.version)));
    out
}

/// Splits a version id such as `b12138-metal` into its release and backend.
fn split_version(version: &str) -> (String, Option<Backend>) {
    let backend = [
        Backend::Metal,
        Backend::Cuda,
        Backend::Rocm,
        Backend::Vulkan,
        Backend::Cpu,
    ]
    .into_iter()
    .find(|b| version.ends_with(&format!("-{}", backend_slug(*b))));
    let release = backend.map_or(version.to_owned(), |b| {
        version
            .trim_end_matches(&format!("-{}", backend_slug(b)))
            .to_owned()
    });
    (release, backend)
}

/// The server binary of an installed build, relative to its directory. Archives name their top
/// directory after the release, so builds other than the pinned one use their own release there.
pub fn server_path(manifest: &Manifest, version: &str) -> String {
    let (release, _) = split_version(version);
    manifest
        .llama_cpp
        .server_path
        .replace(&manifest.llama_cpp.release, &release)
}

pub fn activate_llama(dirs: &DataDirs, version: &str) -> Result<Installed, ComponentError> {
    Ok(component(dirs, LLAMA_CPP).activate(version)?)
}

pub fn remove_llama(dirs: &DataDirs, version: &str) -> Result<u64, ComponentError> {
    Ok(component(dirs, LLAMA_CPP).remove(version)?)
}

pub fn rollback_llama(dirs: &DataDirs) -> Result<Installed, ComponentError> {
    Ok(component(dirs, LLAMA_CPP).rollback()?)
}

fn backend_slug(backend: Backend) -> String {
    backend.label().to_lowercase().replace('/', "-")
}

pub const LLAMA_CPP: &str = "llama.cpp";
pub const JRE: &str = "jre";
pub const CLOJURE: &str = "clojure";

fn component(dirs: &DataDirs, name: &str) -> ComponentDir {
    ComponentDir::new(dirs.runtimes().join(name))
}

/// File name for a downloaded artifact: the last URL segment, prefixed by its hash so two
/// releases with the same file name never share a download.
fn download_path(dirs: &DataDirs, a: &Artifact) -> PathBuf {
    let name = a.url.rsplit('/').next().unwrap_or("artifact");
    dirs.downloads().join(format!("{}-{name}", &a.sha256[..12]))
}

fn fetch_all(
    dirs: &DataDirs,
    artifacts: &[&Artifact],
    cancel: &Cancel,
    progress: &mut dyn FnMut(&str, Progress),
) -> Result<Vec<PathBuf>, ComponentError> {
    let mut paths = Vec::new();
    for a in artifacts {
        let dest = download_path(dirs, a);
        let label = a.url.rsplit('/').next().unwrap_or_default().to_owned();
        download_verified(a, &dest, cancel, |p| progress(&label, p))?;
        paths.push(dest);
    }
    Ok(paths)
}

/// Runs `program args` and requires success; returns combined output for diagnostics.
fn smoke(program: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "{} exited with {}: {text}",
            program.display(),
            out.status
        ))
    }
}

pub fn llama_version_id(manifest: &Manifest, backend: Backend) -> String {
    format!("{}-{}", manifest.llama_cpp.release, backend_slug(backend))
}

/// Downloads, verifies, smoke-tests, and activates the pinned llama.cpp build for `backend`.
pub fn install_llama(
    manifest: &Manifest,
    dirs: &DataDirs,
    platform: &str,
    backend: Backend,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&str, Progress),
) -> Result<Installed, ComponentError> {
    let asset = manifest
        .llama_asset(platform, backend)
        .ok_or_else(|| ComponentError::Unavailable {
            component: LLAMA_CPP,
            platform: platform.to_owned(),
            backend: format!(" with {}", backend.label()),
        })?;
    let artifacts: Vec<&Artifact> = std::iter::once(&asset.artifact)
        .chain(asset.extra.iter().map(|e| &e.artifact))
        .collect();
    ensure_space(dirs, &artifacts)?;
    let archives = fetch_all(dirs, &artifacts, cancel, progress)?;
    let server = manifest.llama_cpp.server_path.clone();
    let installed = component(dirs, LLAMA_CPP).install_with(
        &llama_version_id(manifest, backend),
        |staging| {
            extract_tar_gz(&archives[0], staging)?;
            for (extra, archive) in asset.extra.iter().zip(&archives[1..]) {
                extract_tar_gz_stripped(archive, &staging.join(&extra.extract_to), extra.strip_components)?;
            }
            Ok(())
        },
        |dir| {
            let server = dir.join(&server);
            smoke(&server, &["--version"])?;
            if backend.uses_gpu() {
                let devices = smoke(&server, &["--list-devices"])?;
                if parse_device_list(&devices).is_empty() {
                    return Err(format!(
                        "the {} build found no usable device:\n{devices}",
                        backend.label()
                    ));
                }
            }
            Ok(())
        },
    )?;
    Ok(installed)
}

/// Device lines from `llama-server --list-devices` output, e.g. `CUDA0: NVIDIA ... (8192 MiB, ...)`.
pub fn parse_device_list(output: &str) -> Vec<String> {
    output
        .lines()
        .skip_while(|l| !l.starts_with("Available devices"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "(none)" && l.contains(':'))
        .map(str::to_owned)
        .collect()
}

/// The active llama.cpp server binary, if installed.
pub fn llama_server(manifest: &Manifest, dirs: &DataDirs) -> Option<(Installed, PathBuf)> {
    let active = component(dirs, LLAMA_CPP).active()?;
    let server = active.dir.join(server_path(manifest, &active.version));
    server.is_file().then_some((active, server))
}

pub fn install_jre(
    manifest: &Manifest,
    dirs: &DataDirs,
    platform: &str,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&str, Progress),
) -> Result<Installed, ComponentError> {
    let asset = manifest
        .jre_asset(platform)
        .ok_or_else(|| ComponentError::Unavailable {
            component: JRE,
            platform: platform.to_owned(),
            backend: String::new(),
        })?;
    let version = manifest.jre.version.replace('+', "_");
    if let Some(active) = component(dirs, JRE).active().filter(|a| a.version == version) {
        return Ok(active);
    }
    ensure_space(dirs, &[&asset.artifact])?;
    let archives = fetch_all(dirs, &[&asset.artifact], cancel, progress)?;
    let java = asset.java_path.clone();
    Ok(component(dirs, JRE).install(&version, &archives, |dir| {
        smoke(&dir.join(&java), &["-version"]).map(|_| ())
    })?)
}

/// The active Java runtime: `(java executable, java home)`.
pub fn java(manifest: &Manifest, dirs: &DataDirs, platform: &str) -> Option<(PathBuf, PathBuf)> {
    let active = component(dirs, JRE).active()?;
    let rel = &manifest.jre_asset(platform)?.java_path;
    let java = active.dir.join(rel);
    // java_path is <home>/bin/java.
    let home = java.parent()?.parent()?.to_owned();
    java.is_file().then_some((java, home))
}

pub fn install_clojure(
    manifest: &Manifest,
    dirs: &DataDirs,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&str, Progress),
) -> Result<Installed, ComponentError> {
    if let Some(active) = component(dirs, CLOJURE)
        .active()
        .filter(|a| a.version == manifest.clojure.version)
    {
        return Ok(active);
    }
    let artifacts: Vec<&Artifact> = manifest.clojure.jars.iter().map(|j| &j.artifact).collect();
    let files = fetch_all(dirs, &artifacts, cancel, progress)?;
    let names: Vec<String> = manifest.clojure.jars.iter().map(|j| j.name.clone()).collect();
    let installed = component(dirs, CLOJURE).install_with(
        &manifest.clojure.version,
        |staging| {
            for (src, name) in files.iter().zip(&names) {
                let dest = staging.join(name);
                std::fs::copy(src, &dest).map_err(|source| InstallError::Io { path: dest, source })?;
            }
            Ok(())
        },
        |staging| {
            names
                .iter()
                .all(|n| staging.join(n).is_file())
                .then_some(())
                .ok_or_else(|| "a Clojure jar is missing".to_owned())
        },
    )?;
    Ok(installed)
}

/// The installed Clojure jars, in classpath order.
pub fn clojure_jars(manifest: &Manifest, dirs: &DataDirs) -> Option<Vec<PathBuf>> {
    let active = component(dirs, CLOJURE).active()?;
    let jars: Vec<PathBuf> = manifest
        .clojure
        .jars
        .iter()
        .map(|j| active.dir.join(&j.name))
        .collect();
    jars.iter().all(|j| j.is_file()).then_some(jars)
}

/// Smoke-tests an installed build: `--version`, and for GPU backends `--list-devices` must report
/// at least one device. Returns the device lines llama.cpp reported.
pub fn check_llama(manifest: &Manifest, dirs: &DataDirs, version: &str) -> Result<Vec<String>, String> {
    let server = dirs
        .runtimes()
        .join(LLAMA_CPP)
        .join(version)
        .join(server_path(manifest, version));
    smoke(&server, &["--version"])?;
    Ok(parse_device_list(&smoke(&server, &["--list-devices"])?))
}

/// The llama.cpp server of a specific installed backend build, active or not.
pub fn llama_server_for(manifest: &Manifest, dirs: &DataDirs, backend: Backend) -> Option<PathBuf> {
    let dir = dirs
        .runtimes()
        .join(LLAMA_CPP)
        .join(llama_version_id(manifest, backend));
    let server = dir.join(&manifest.llama_cpp.server_path);
    server.is_file().then_some(server)
}

#[cfg(test)]
mod tests {
    use super::{parse_device_list, server_path};
    use crate::manifest::Manifest;

    #[test]
    fn server_path_follows_the_build_release() {
        let m = Manifest::bundled().expect("manifest");
        let pinned = m.llama_cpp.server_path.clone();
        assert_eq!(server_path(&m, &format!("{}-metal", m.llama_cpp.release)), pinned);
        assert_eq!(server_path(&m, "b12202-metal"), "llama-b12202/llama-server");
        assert_eq!(server_path(&m, "b12202-rocm-hip"), "llama-b12202/llama-server");
    }

    #[test]
    fn device_list_parsing() {
        let some = "ggml_cuda_init: found 1 CUDA devices\nAvailable devices:\n  CUDA0: NVIDIA GeForce RTX 3070 (8192 MiB, 7000 MiB free)\n";
        assert_eq!(
            parse_device_list(some),
            vec!["CUDA0: NVIDIA GeForce RTX 3070 (8192 MiB, 7000 MiB free)"]
        );
        assert!(parse_device_list("Available devices:\n  (none)\n").is_empty());
        assert!(parse_device_list("load_backend: failed\n").is_empty());
    }
}
