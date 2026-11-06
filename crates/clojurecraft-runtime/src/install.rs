//! Atomic installation of verified archives.
//!
//! Layout under `runtimes/<component>/`:
//!
//! ```text
//! <version-id>/          fully extracted and smoke-tested installation
//! .staging-<version-id>/ extraction in progress (deleted on failure or on next attempt)
//! active.json            {"version": "<version-id>", "previous": "<version-id>"?}
//! ```
//!
//! `active.json` is replaced with write-then-rename, so a crash leaves either the old or the new
//! pointer, never a torn one. The previous installation is kept for rollback.

use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("file system error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("archive entry {0} escapes the installation directory")]
    UnsafeEntry(String),
    #[error("smoke test failed: {0}")]
    SmokeTest(String),
    #[error("no installation is active")]
    NothingActive,
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("{0} is in use; activate another version first")]
    InUse(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> InstallError + '_ {
    move |source| InstallError::Io {
        path: path.to_owned(),
        source,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivePointer {
    pub version: String,
    #[serde(default)]
    pub previous: Option<String>,
}

/// A completed installation directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub dir: PathBuf,
}

/// Extracts a `.tar.gz` into `dest`, refusing absolute paths, `..` components, and links that
/// point outside `dest`.
pub fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), InstallError> {
    extract_tar_gz_stripped(archive, dest, 0)
}

/// Like [`extract_tar_gz`], dropping the first `strip` path components of every entry (entries
/// that become empty are skipped).
pub fn extract_tar_gz_stripped(archive: &Path, dest: &Path, strip: usize) -> Result<(), InstallError> {
    let file = File::open(archive).map_err(io_err(archive))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    tar.set_preserve_permissions(true);
    tar.set_overwrite(true);
    let entries = tar.entries().map_err(io_err(archive))?;
    for entry in entries {
        let mut entry = entry.map_err(io_err(archive))?;
        let path = entry.path().map_err(io_err(archive))?.into_owned();
        let name = path.display().to_string();
        if !is_contained(&path) {
            return Err(InstallError::UnsafeEntry(name));
        }
        if let Some(target) = entry.link_name().map_err(io_err(archive))? {
            let parent = path.parent().unwrap_or(Path::new(""));
            if target.is_absolute() || !is_contained(&parent.join(&target)) {
                return Err(InstallError::UnsafeEntry(name));
            }
        }
        let rel: PathBuf = path.components().skip(strip).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        if strip == 0 {
            entry.unpack_in(dest).map_err(io_err(dest))?;
        } else {
            let target = dest.join(&rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(io_err(parent))?;
            }
            entry.unpack(&target).map_err(io_err(&target))?;
        }
    }
    Ok(())
}

/// True when `p` is relative and never climbs above its starting directory.
fn is_contained(p: &Path) -> bool {
    let mut depth: i64 = 0;
    for c in p.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Manages the installations of one component (e.g. `llama.cpp`).
#[derive(Clone, Debug)]
pub struct ComponentDir {
    root: PathBuf,
}

impl ComponentDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn pointer_path(&self) -> PathBuf {
        self.root.join("active.json")
    }

    pub fn active(&self) -> Option<Installed> {
        let text = fs::read_to_string(self.pointer_path()).ok()?;
        let ptr: ActivePointer = serde_json::from_str(&text).ok()?;
        let dir = self.root.join(&ptr.version);
        dir.is_dir().then_some(Installed {
            version: ptr.version,
            dir,
        })
    }

    /// Installs `version` from the given verified archives, runs `smoke_test` against the staged
    /// directory, and only then makes it active. On any failure the active installation is
    /// untouched and the staging directory is removed.
    pub fn install(
        &self,
        version: &str,
        archives: &[PathBuf],
        smoke_test: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<Installed, InstallError> {
        self.install_with(
            version,
            |staging| archives.iter().try_for_each(|a| extract_tar_gz(a, staging)),
            smoke_test,
        )
    }

    /// Like [`ComponentDir::install`], with `populate` filling the staging directory.
    pub fn install_with(
        &self,
        version: &str,
        populate: impl FnOnce(&Path) -> Result<(), InstallError>,
        smoke_test: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<Installed, InstallError> {
        fs::create_dir_all(&self.root).map_err(io_err(&self.root))?;
        let staging = self.root.join(format!(".staging-{version}"));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(io_err(&staging))?;
        }
        fs::create_dir_all(&staging).map_err(io_err(&staging))?;
        let result = populate(&staging).and_then(|()| smoke_test(&staging).map_err(InstallError::SmokeTest));
        if let Err(e) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        let final_dir = self.root.join(version);
        if final_dir.exists() {
            // Re-installing the same version: the old copy is replaced only after the new one passed.
            let trash = self.root.join(format!(".old-{version}"));
            let _ = fs::remove_dir_all(&trash);
            fs::rename(&final_dir, &trash).map_err(io_err(&final_dir))?;
            let _ = fs::remove_dir_all(&trash);
        }
        fs::rename(&staging, &final_dir).map_err(io_err(&final_dir))?;
        let previous = self.active().map(|i| i.version).filter(|v| v != version);
        self.write_pointer(&ActivePointer {
            version: version.to_owned(),
            previous,
        })?;
        Ok(Installed {
            version: version.to_owned(),
            dir: final_dir,
        })
    }

    /// Completed installations (version ids), sorted.
    pub fn installed(&self) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    /// Makes an already installed version active, remembering the current one for rollback.
    pub fn activate(&self, version: &str) -> Result<Installed, InstallError> {
        let dir = self.root.join(version);
        if version.starts_with('.') || version.contains('/') || !dir.is_dir() {
            return Err(InstallError::NotInstalled(version.to_owned()));
        }
        let previous = self.active().map(|i| i.version).filter(|v| v != version);
        self.write_pointer(&ActivePointer {
            version: version.to_owned(),
            previous,
        })?;
        Ok(Installed {
            version: version.to_owned(),
            dir,
        })
    }

    /// Deletes an inactive installation. Returns the bytes freed.
    pub fn remove(&self, version: &str) -> Result<u64, InstallError> {
        if self.active().is_some_and(|a| a.version == version) {
            return Err(InstallError::InUse(version.to_owned()));
        }
        let dir = self.root.join(version);
        if version.starts_with('.') || version.contains('/') || !dir.is_dir() {
            return Err(InstallError::NotInstalled(version.to_owned()));
        }
        let size = crate::models::dir_size(&dir);
        let doomed = self.root.join(format!(".removing-{version}"));
        let _ = fs::remove_dir_all(&doomed);
        fs::rename(&dir, &doomed).map_err(io_err(&dir))?;
        fs::remove_dir_all(&doomed).map_err(io_err(&doomed))?;
        Ok(size)
    }

    /// Re-activates the previous installation, if it is still present.
    pub fn rollback(&self) -> Result<Installed, InstallError> {
        let text = fs::read_to_string(self.pointer_path()).map_err(|_| InstallError::NothingActive)?;
        let ptr: ActivePointer = serde_json::from_str(&text).map_err(|_| InstallError::NothingActive)?;
        let prev = ptr.previous.ok_or(InstallError::NothingActive)?;
        let dir = self.root.join(&prev);
        if !dir.is_dir() {
            return Err(InstallError::NothingActive);
        }
        self.write_pointer(&ActivePointer {
            version: prev.clone(),
            previous: Some(ptr.version),
        })?;
        Ok(Installed { version: prev, dir })
    }

    fn write_pointer(&self, ptr: &ActivePointer) -> Result<(), InstallError> {
        let path = self.pointer_path();
        let tmp = self.root.join("active.json.tmp");
        let json = serde_json::to_vec_pretty(ptr).map_err(|e| InstallError::Io {
            path: path.clone(),
            source: io::Error::other(e),
        })?;
        {
            use std::io::Write;
            let mut f = File::create(&tmp).map_err(io_err(&tmp))?;
            f.write_all(&json).map_err(io_err(&tmp))?;
            f.sync_all().map_err(io_err(&tmp))?;
        }
        fs::rename(&tmp, &path).map_err(io_err(&path))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn make_archive(dir: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let path = dir.join(name);
        let gz = GzEncoder::new(File::create(&path).expect("create"), Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (p, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, p, content.as_bytes()).expect("append");
        }
        b.into_inner().expect("finish").finish().expect("gz");
        path
    }

    /// Writes a raw tar header so that hostile paths bypass the builder's own validation.
    fn make_hostile_archive(dir: &Path, entry: &str) -> PathBuf {
        let path = dir.join("hostile.tar.gz");
        let gz = GzEncoder::new(File::create(&path).expect("create"), Compression::fast());
        let mut b = tar::Builder::new(gz);
        let mut h = tar::Header::new_old();
        h.as_old_mut().name[..entry.len()].copy_from_slice(entry.as_bytes());
        h.set_size(1);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, &b"x"[..]).expect("append");
        b.into_inner().expect("finish").finish().expect("gz");
        path
    }

    #[test]
    fn install_activates_only_after_smoke_test() {
        let tmp = tempfile::tempdir().expect("tmp");
        let a = make_archive(tmp.path(), "a.tgz", &[("pkg/bin/tool", "v1")]);
        let comp = ComponentDir::new(tmp.path().join("runtimes/tool"));
        let err = comp.install("v1", std::slice::from_ref(&a), |_| Err("broken".into()));
        assert!(matches!(err, Err(InstallError::SmokeTest(_))));
        assert!(comp.active().is_none());
        assert!(!tmp.path().join("runtimes/tool/.staging-v1").exists());

        let inst = comp
            .install("v1", &[a], |dir| {
                fs::read_to_string(dir.join("pkg/bin/tool"))
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .expect("install");
        assert_eq!(comp.active(), Some(inst));
    }

    #[test]
    fn failed_upgrade_keeps_previous_and_rollback_works() {
        let tmp = tempfile::tempdir().expect("tmp");
        let comp = ComponentDir::new(tmp.path().join("rt"));
        let v1 = make_archive(tmp.path(), "1.tgz", &[("t", "1")]);
        let v2 = make_archive(tmp.path(), "2.tgz", &[("t", "2")]);
        comp.install("v1", &[v1], |_| Ok(())).expect("v1");
        assert!(
            comp.install("v2", std::slice::from_ref(&v2), |_| Err("no".into()))
                .is_err()
        );
        assert_eq!(comp.active().expect("active").version, "v1");
        comp.install("v2", &[v2], |_| Ok(())).expect("v2");
        assert_eq!(comp.active().expect("active").version, "v2");
        assert_eq!(comp.rollback().expect("rollback").version, "v1");
        assert_eq!(comp.active().expect("active").version, "v1");
    }

    #[test]
    fn switching_and_removing_versions() {
        let tmp = tempfile::tempdir().expect("tmp");
        let comp = ComponentDir::new(tmp.path().join("rt"));
        for v in ["b1-cpu", "b1-cuda"] {
            let a = make_archive(tmp.path(), &format!("{v}.tgz"), &[("t", v)]);
            comp.install(v, &[a], |_| Ok(())).expect("install");
        }
        assert_eq!(comp.installed(), vec!["b1-cpu", "b1-cuda"]);
        assert_eq!(comp.active().expect("active").version, "b1-cuda");
        assert!(matches!(comp.remove("b1-cuda"), Err(InstallError::InUse(_))));
        comp.activate("b1-cpu").expect("activate");
        assert_eq!(comp.rollback().expect("rollback").version, "b1-cuda");
        comp.activate("b1-cpu").expect("activate");
        assert!(comp.remove("b1-cuda").expect("remove") > 0);
        assert_eq!(comp.installed(), vec!["b1-cpu"]);
        assert!(matches!(
            comp.activate("../x"),
            Err(InstallError::NotInstalled(_))
        ));
        assert!(matches!(
            comp.remove("missing"),
            Err(InstallError::NotInstalled(_))
        ));
    }

    #[test]
    fn path_traversal_is_rejected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let a = make_hostile_archive(tmp.path(), "../evil");
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).expect("mkdir");
        assert!(matches!(
            extract_tar_gz(&a, &out),
            Err(InstallError::UnsafeEntry(_))
        ));
        assert!(!tmp.path().join("evil").exists());
    }

    #[test]
    fn stripped_extraction_merges_into_target() {
        let tmp = tempfile::tempdir().expect("tmp");
        let a = make_archive(tmp.path(), "x.tgz", &[("top/lib.so", "l"), ("top/sub/x", "x")]);
        let out = tmp.path().join("out");
        fs::create_dir_all(&out).expect("mkdir");
        extract_tar_gz_stripped(&a, &out, 1).expect("extract");
        assert_eq!(fs::read_to_string(out.join("lib.so")).expect("read"), "l");
        assert_eq!(fs::read_to_string(out.join("sub/x")).expect("read"), "x");
    }

    #[test]
    fn containment_rules() {
        assert!(is_contained(Path::new("a/b/../c")));
        assert!(!is_contained(Path::new("a/../../c")));
        assert!(!is_contained(Path::new("/etc/passwd")));
    }
}
