//! Installed models.
//!
//! Each model lives in its own directory under `models/`, named by its key:
//! `models/<catalog-id>/<quant>/` for catalog entries and
//! `models/hf/<owner>/<name>/<commit12>/<quant>/` for other repositories. The directory holds the
//! GGUF file(s) and `model.json`, which records where they came from and their verified sizes and
//! hashes. A directory only appears under its final name after every file was downloaded and
//! verified, so a listed model is always complete when it was installed; later changes on disk
//! are found by [`ModelStore::health`].

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::catalog::{Model, Quant};
use crate::download::{Cancel, DownloadError, Progress, download_verified_auth, sha256_file};
use crate::hub::{Hub, Inspection, RepoInfo};
use crate::manifest::Artifact;

pub const RECORD: &str = "model.json";
const RECORD_FORMAT: u32 = 1;
/// Space kept free beyond what a download needs.
pub const DISK_MARGIN: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error("file system error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error(
        "not enough disk space: {} needed, {} available. Free some space and try again; finished files are kept.",
        crate::catalog::format_gib(*needed), crate::catalog::format_gib(*available)
    )]
    InsufficientSpace { needed: u64, available: u64 },
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("the model record at {path} is unreadable: {reason}")]
    BadRecord { path: PathBuf, reason: String },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ModelError + '_ {
    move |source| ModelError::Io {
        path: path.to_owned(),
        source,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ModelSource {
    Catalog { id: String },
    HuggingFace,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFile {
    /// Path inside the repository.
    pub repo_path: String,
    /// File name inside the model directory.
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// Everything needed to install a model; becomes the installed record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRecord {
    pub format: u32,
    /// Stable identifier and relative directory, e.g. `qwen2.5-coder-7b-instruct/Q4_K_M`.
    pub key: String,
    pub display_name: String,
    pub source: ModelSource,
    pub repo: String,
    /// Commit the files were downloaded from.
    pub revision: String,
    pub quant: String,
    pub architecture: String,
    pub license: Option<String>,
    /// In load order; the first file is the one given to llama.cpp.
    pub files: Vec<ModelFile>,
    pub context_length: Option<u32>,
    pub layers: Option<u32>,
    pub kv_bytes_per_token: Option<u64>,
    /// Seconds since the Unix epoch; zero until installed.
    #[serde(default)]
    pub installed_at: u64,
}

impl ModelRecord {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn from_catalog(model: &Model, quant: &Quant) -> Self {
        let repo = quant.repo.clone().unwrap_or_else(|| model.repo.clone());
        Self {
            format: RECORD_FORMAT,
            key: format!("{}/{}", model.id, sanitize(&quant.name)),
            display_name: model.name.clone(),
            source: ModelSource::Catalog { id: model.id.clone() },
            repo,
            revision: quant.revision.clone().unwrap_or_else(|| model.revision.clone()),
            quant: quant.name.clone(),
            architecture: model.architecture.clone(),
            license: Some(model.license.clone()),
            files: quant
                .files
                .iter()
                .map(|f| ModelFile {
                    repo_path: f.path.clone(),
                    name: base_name(&f.path),
                    size: f.size,
                    sha256: f.sha256.to_lowercase(),
                })
                .collect(),
            context_length: Some(model.max_context),
            layers: Some(model.layers),
            kv_bytes_per_token: Some(model.kv_bytes_per_token),
            installed_at: 0,
        }
    }

    /// A record for an inspected option of another repository. Returns `None` when the
    /// inspection found problems or a file lacks a trusted hash.
    pub fn from_inspection(info: &RepoInfo, inspection: &Inspection) -> Option<Self> {
        if !inspection.is_installable() {
            return None;
        }
        let (owner, name) = info.repo.split_once('/')?;
        let files = inspection
            .option
            .files
            .iter()
            .map(|f| {
                Some(ModelFile {
                    repo_path: f.path.clone(),
                    name: base_name(&f.path),
                    size: f.size,
                    sha256: f.sha256.clone()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            format: RECORD_FORMAT,
            key: format!(
                "hf/{}/{}/{}/{}",
                sanitize(owner),
                sanitize(name),
                &info.revision[..12],
                sanitize(&inspection.option.quant)
            ),
            display_name: inspection
                .facts
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| name.to_owned()),
            source: ModelSource::HuggingFace,
            repo: info.repo.clone(),
            revision: info.revision.clone(),
            quant: inspection.option.quant.clone(),
            architecture: inspection.facts.architecture.clone()?,
            license: None,
            files,
            context_length: inspection.facts.context_length,
            layers: inspection.facts.block_count,
            kv_bytes_per_token: inspection.facts.kv_bytes_per_token(),
            installed_at: 0,
        })
    }
}

fn base_name(repo_path: &str) -> String {
    sanitize(repo_path.rsplit('/').next().unwrap_or(repo_path))
}

/// Keeps only characters that are safe in a single path component.
fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let out = out.trim_start_matches('.').to_owned();
    if out.is_empty() { "_".to_owned() } else { out }
}

/// A file of an installed model that is not as it was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileProblem {
    Missing(String),
    SizeChanged {
        name: String,
        expected: u64,
        actual: u64,
    },
    Corrupt(String),
}

impl std::fmt::Display for FileProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(n) => write!(f, "{n} is missing"),
            Self::SizeChanged { name, .. } => write!(f, "{name} changed size since it was installed"),
            Self::Corrupt(n) => write!(f, "{n} does not match its recorded checksum"),
        }
    }
}

/// Free bytes for unprivileged writes on the file system holding `path` (or its nearest existing
/// ancestor).
pub fn available_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let st = rustix::fs::statvfs(existing).ok()?;
    Some(st.f_bavail.saturating_mul(st.f_frsize))
}

pub struct ModelStore {
    root: PathBuf,
    downloads: PathBuf,
    free_space: fn(&Path) -> Option<u64>,
}

impl ModelStore {
    pub fn new(root: PathBuf, downloads: PathBuf) -> Self {
        Self {
            root,
            downloads,
            free_space: available_space,
        }
    }

    #[cfg(test)]
    fn with_free_space(mut self, probe: fn(&Path) -> Option<u64>) -> Self {
        self.free_space = probe;
        self
    }

    pub fn dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// The file to hand to llama.cpp.
    pub fn primary_file(&self, record: &ModelRecord) -> Option<PathBuf> {
        Some(self.dir(&record.key).join(&record.files.first()?.name))
    }

    pub fn get(&self, key: &str) -> Option<ModelRecord> {
        read_record(&self.dir(key).join(RECORD)).ok()
    }

    /// Every installed model, plus records that could not be read.
    pub fn list(&self) -> (Vec<ModelRecord>, Vec<ModelError>) {
        let mut found = Vec::new();
        let mut bad = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else if name == RECORD {
                    match read_record(&path) {
                        Ok(r) => found.push(r),
                        Err(e) => bad.push(e),
                    }
                }
            }
        }
        found.sort_by(|a, b| a.display_name.cmp(&b.display_name).then(a.quant.cmp(&b.quant)));
        (found, bad)
    }

    /// Checks the files of an installed model. `full` also re-hashes them (slow for large models).
    pub fn health(&self, record: &ModelRecord, full: bool) -> Vec<FileProblem> {
        let dir = self.dir(&record.key);
        let mut problems = Vec::new();
        for f in &record.files {
            let path = dir.join(&f.name);
            match fs::metadata(&path) {
                Err(_) => problems.push(FileProblem::Missing(f.name.clone())),
                Ok(m) if m.len() != f.size => problems.push(FileProblem::SizeChanged {
                    name: f.name.clone(),
                    expected: f.size,
                    actual: m.len(),
                }),
                Ok(_) if full && sha256_file(&path).ok().as_deref() != Some(f.sha256.as_str()) => {
                    problems.push(FileProblem::Corrupt(f.name.clone()));
                }
                Ok(_) => {}
            }
        }
        problems
    }

    fn download_path(&self, f: &ModelFile) -> PathBuf {
        self.downloads.join(format!("{}-{}", &f.sha256[..12], f.name))
    }

    fn part_len(&self, f: &ModelFile) -> u64 {
        let mut part = self.download_path(f).into_os_string();
        part.push(".part");
        fs::metadata(PathBuf::from(part)).map(|m| m.len()).unwrap_or(0)
    }

    /// Bytes still to download for `record`, counting finished and partial downloads.
    pub fn remaining_bytes(&self, record: &ModelRecord) -> u64 {
        record
            .files
            .iter()
            .filter(|f| self.reusable(record, f).is_none())
            .map(|f| {
                if self.download_path(f).is_file() {
                    0
                } else {
                    f.size.saturating_sub(self.part_len(f))
                }
            })
            .sum()
    }

    /// A file already present in the model's final directory with the recorded size and hash
    /// (for example from an earlier, interrupted installation).
    fn reusable(&self, record: &ModelRecord, f: &ModelFile) -> Option<PathBuf> {
        let path = self.dir(&record.key).join(&f.name);
        let len = fs::metadata(&path).ok()?.len();
        (len == f.size).then_some(path)
    }

    /// Downloads, verifies, and atomically activates a model. Already verified files and partial
    /// downloads are reused. On failure or cancellation nothing becomes visible as installed and
    /// partial downloads are kept for a later attempt.
    pub fn install(
        &self,
        plan: &ModelRecord,
        hub: &Hub,
        cancel: &Cancel,
        progress: &mut dyn FnMut(&str, Progress),
    ) -> Result<ModelRecord, ModelError> {
        if let Some(existing) = self.get(&plan.key)
            && existing.files == plan.files
            && self.health(&existing, false).is_empty()
        {
            return Ok(existing);
        }
        let needed = self.remaining_bytes(plan);
        if needed > 0
            && let Some(available) = (self.free_space)(&self.downloads)
            && available < needed + DISK_MARGIN
        {
            return Err(ModelError::InsufficientSpace {
                needed: needed + DISK_MARGIN,
                available,
            });
        }
        let mut sources = Vec::new();
        for f in &plan.files {
            if let Some(path) = self.reusable(plan, f)
                && sha256_file(&path).ok().as_deref() == Some(f.sha256.as_str())
            {
                sources.push(path);
                continue;
            }
            let dest = self.download_path(f);
            let artifact = Artifact {
                url: hub.file_url(&plan.repo, &plan.revision, &f.repo_path),
                size: f.size,
                sha256: f.sha256.clone(),
            };
            download_verified_auth(&artifact, &dest, hub.token(), cancel, |p| progress(&f.name, p))?;
            sources.push(dest);
        }
        self.activate(plan, &sources)
    }

    fn activate(&self, plan: &ModelRecord, sources: &[PathBuf]) -> Result<ModelRecord, ModelError> {
        let final_dir = self.dir(&plan.key);
        let parent = final_dir.parent().unwrap_or(&self.root).to_owned();
        fs::create_dir_all(&parent).map_err(io_err(&parent))?;
        let leaf = final_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging = parent.join(format!(".staging-{leaf}"));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(io_err(&staging))?;
        }
        fs::create_dir_all(&staging).map_err(io_err(&staging))?;
        let result = (|| {
            for (f, src) in plan.files.iter().zip(sources) {
                move_file(src, &staging.join(&f.name))?;
            }
            let mut record = plan.clone();
            record.installed_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            write_record(&staging.join(RECORD), &record)?;
            if final_dir.exists() {
                let old = parent.join(format!(".old-{leaf}"));
                let _ = fs::remove_dir_all(&old);
                fs::rename(&final_dir, &old).map_err(io_err(&final_dir))?;
                let _ = fs::remove_dir_all(&old);
            }
            fs::rename(&staging, &final_dir).map_err(io_err(&final_dir))?;
            Ok(record)
        })();
        if result.is_err() {
            // Files moved into staging go back to the downloads directory so nothing is lost.
            for (f, src) in plan.files.iter().zip(sources) {
                let staged = staging.join(&f.name);
                if staged.exists() && !src.exists() {
                    let _ = move_file(&staged, src);
                }
            }
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    /// Removes an installed model. Returns the bytes freed.
    pub fn remove(&self, key: &str) -> Result<u64, ModelError> {
        let dir = self.dir(key);
        if !dir.join(RECORD).is_file() {
            return Err(ModelError::NotInstalled(key.to_owned()));
        }
        let size = dir_size(&dir);
        let parent = dir.parent().unwrap_or(&self.root).to_owned();
        let leaf = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Rename first so a half-deleted model never looks installed.
        let doomed = parent.join(format!(".removing-{leaf}"));
        let _ = fs::remove_dir_all(&doomed);
        fs::rename(&dir, &doomed).map_err(io_err(&dir))?;
        fs::remove_dir_all(&doomed).map_err(io_err(&doomed))?;
        // Drop now-empty parents (catalog id or repository directories).
        let mut p = parent;
        while p != self.root && fs::remove_dir(&p).is_ok() {
            match p.parent() {
                Some(up) => p = up.to_owned(),
                None => break,
            }
        }
        Ok(size)
    }

    /// Bytes held by partial (resumable) downloads.
    pub fn partial_download_bytes(&self) -> u64 {
        files_matching(&self.downloads, |n| n.ends_with(".part"))
    }

    /// Deletes partial downloads; they would otherwise be resumed.
    pub fn discard_partial_downloads(&self) -> u64 {
        remove_matching(&self.downloads, |n| n.ends_with(".part"))
    }

    /// Removes leftovers of interrupted installations or removals.
    pub fn clean_leftovers(&self) {
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if [".staging-", ".old-", ".removing-"]
                    .iter()
                    .any(|p| n.starts_with(p))
                {
                    let _ = fs::remove_dir_all(e.path());
                } else if e.path().is_dir() && !e.path().join(RECORD).exists() {
                    stack.push(e.path());
                }
            }
        }
    }
}

fn read_record(path: &Path) -> Result<ModelRecord, ModelError> {
    let bad = |reason: String| ModelError::BadRecord {
        path: path.to_owned(),
        reason,
    };
    let text = fs::read_to_string(path).map_err(|e| bad(e.to_string()))?;
    let record: ModelRecord = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
    if record.format != RECORD_FORMAT {
        return Err(bad(format!("unsupported record format {}", record.format)));
    }
    let expected_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    if !expected_dir.ends_with(&record.key) || record.files.is_empty() {
        return Err(bad("the record does not match its directory".into()));
    }
    Ok(record)
}

fn write_record(path: &Path, record: &ModelRecord) -> Result<(), ModelError> {
    let json = serde_json::to_vec_pretty(record).map_err(|e| ModelError::Io {
        path: path.to_owned(),
        source: io::Error::other(e),
    })?;
    let mut f = File::create(path).map_err(io_err(path))?;
    f.write_all(&json).map_err(io_err(path))?;
    f.sync_all().map_err(io_err(path))
}

/// Renames, falling back to copy-and-delete across file systems.
fn move_file(from: &Path, to: &Path) -> Result<(), ModelError> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to).map_err(io_err(to))?;
    File::open(to).and_then(|f| f.sync_all()).map_err(io_err(to))?;
    fs::remove_file(from).map_err(io_err(from))
}

/// Total size of regular files below `dir`, without following symbolic links.
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    total
}

pub(crate) fn files_matching(dir: &Path, pred: impl Fn(&str) -> bool) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| pred(&e.file_name().to_string_lossy()))
        .filter_map(|e| e.metadata().ok())
        .filter(fs::Metadata::is_file)
        .map(|m| m.len())
        .sum()
}

pub(crate) fn remove_matching(dir: &Path, pred: impl Fn(&str) -> bool) -> u64 {
    let mut freed = 0;
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if pred(&e.file_name().to_string_lossy())
            && let Ok(m) = e.metadata()
            && m.is_file()
            && fs::remove_file(e.path()).is_ok()
        {
            freed += m.len();
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::test_http::{Route, Server, serve};

    const REV: &str = "fedcba9876543210fedcba9876543210fedcba98";

    struct Fixture {
        _tmp: tempfile::TempDir,
        store: ModelStore,
        server: Server,
        plan: ModelRecord,
        bodies: Vec<Vec<u8>>,
    }

    fn body(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// A two-part model served by a mock Hub. `tamper` serves different bytes for part 2.
    fn fixture(tamper: bool, token: Option<&str>) -> Fixture {
        let tmp = tempfile::tempdir().expect("tmp");
        let bodies = vec![body(1, 200_000), body(2, 150_000)];
        let mut routes = HashMap::new();
        let mut files = Vec::new();
        for (i, b) in bodies.iter().enumerate() {
            let repo_path = format!("dir/m-Q4_K_M-{:05}-of-00002.gguf", i + 1);
            let mut served = b.clone();
            if tamper && i == 1 {
                served[10] ^= 0xff;
            }
            let mut route = Route::ok(served);
            if let Some(t) = token {
                route = route.gated(t);
            }
            routes.insert(format!("/o/m/resolve/{REV}/{repo_path}"), route);
            files.push(ModelFile {
                name: base_name(&repo_path),
                repo_path,
                size: b.len() as u64,
                sha256: hex::encode(Sha256::digest(b)),
            });
        }
        let plan = ModelRecord {
            format: RECORD_FORMAT,
            key: format!("hf/o/m/{}/Q4_K_M", &REV[..12]),
            display_name: "M".into(),
            source: ModelSource::HuggingFace,
            repo: "o/m".into(),
            revision: REV.into(),
            quant: "Q4_K_M".into(),
            architecture: "qwen2".into(),
            license: None,
            files,
            context_length: Some(4096),
            layers: Some(4),
            kv_bytes_per_token: Some(1024),
            installed_at: 0,
        };
        let store = ModelStore::new(tmp.path().join("models"), tmp.path().join("downloads"));
        Fixture {
            _tmp: tmp,
            store,
            server: serve(routes),
            plan,
            bodies,
        }
    }

    fn hub(f: &Fixture, token: Option<&str>) -> Hub {
        Hub::with_base(&f.server.base, token.map(str::to_owned))
    }

    #[test]
    fn installs_verifies_and_lists() {
        let f = fixture(false, None);
        let rec = f
            .store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect("install");
        assert!(rec.installed_at > 0);
        let (listed, bad) = f.store.list();
        assert!(bad.is_empty());
        assert_eq!(listed, vec![rec.clone()]);
        let primary = f.store.primary_file(&rec).expect("primary");
        assert_eq!(fs::read(&primary).expect("read"), f.bodies[0]);
        assert!(f.store.health(&rec, true).is_empty());
        // Installing again downloads nothing.
        let before = f.server.requests().len();
        f.store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect("again");
        assert_eq!(f.server.requests().len(), before);
    }

    #[test]
    fn tampered_files_are_rejected_and_nothing_is_activated() {
        let f = fixture(true, None);
        let err = f
            .store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect_err("tampered");
        assert!(matches!(
            err,
            ModelError::Download(DownloadError::HashMismatch { .. })
        ));
        assert!(f.store.list().0.is_empty());
        assert!(!f.store.dir(&f.plan.key).exists());
    }

    #[test]
    fn cancelled_install_resumes_without_refetching_finished_parts() {
        let f = fixture(false, None);
        let cancel = Cancel::default();
        let c2 = cancel.clone();
        let err = f
            .store
            .install(&f.plan, &hub(&f, None), &cancel, &mut |name, p| {
                if name.contains("00002-of") && p.received > 0 {
                    c2.cancel();
                }
            })
            .expect_err("cancelled");
        assert!(matches!(err, ModelError::Download(DownloadError::Cancelled)));
        assert!(f.store.list().0.is_empty(), "nothing visible after cancellation");
        assert!(f.store.partial_download_bytes() > 0);
        assert!(f.store.remaining_bytes(&f.plan) < f.plan.total_size() - f.plan.files[0].size);

        f.store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect("resume");
        let part1 = format!("/o/m/resolve/{REV}/{}", f.plan.files[0].repo_path);
        let fetched_part1 = f.server.requests().iter().filter(|r| r.path == part1).count();
        assert_eq!(
            fetched_part1, 1,
            "the finished first part was not downloaded again"
        );
        let part2 = format!("/o/m/resolve/{REV}/{}", f.plan.files[1].repo_path);
        assert!(
            f.server
                .requests()
                .iter()
                .any(|r| r.path == part2 && r.range.is_some())
        );
        assert_eq!(f.store.partial_download_bytes(), 0);
    }

    #[test]
    fn insufficient_space_is_reported_before_downloading() {
        let f = fixture(false, None);
        let store =
            ModelStore::new(f.store.root.clone(), f.store.downloads.clone()).with_free_space(|_| Some(1000));
        let err = store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect_err("no space");
        assert!(matches!(
            err,
            ModelError::InsufficientSpace { available: 1000, .. }
        ));
        assert!(f.server.requests().is_empty(), "nothing was downloaded");
        assert!(err.to_string().contains("try again"));
    }

    #[test]
    fn gated_downloads_send_the_token_and_fail_recoverably_without_it() {
        let token = "hf_secret_token_abcdefghijklmnop";
        let f = fixture(false, Some(token));
        let err = f
            .store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect_err("unauthorized");
        assert!(matches!(err, ModelError::Download(DownloadError::Unauthorized)));
        assert!(!err.to_string().contains(token));
        f.store
            .install(&f.plan, &hub(&f, Some(token)), &Cancel::default(), &mut |_, _| {})
            .expect("with token");
    }

    #[test]
    fn changed_and_corrupt_files_are_detected() {
        let f = fixture(false, None);
        let rec = f
            .store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect("install");
        let dir = f.store.dir(&rec.key);
        let first = dir.join(&rec.files[0].name);
        let mut bytes = fs::read(&first).expect("read");
        bytes[5] ^= 1;
        fs::write(&first, &bytes).expect("corrupt");
        assert!(
            f.store.health(&rec, false).is_empty(),
            "same size passes the quick check"
        );
        assert_eq!(
            f.store.health(&rec, true),
            vec![FileProblem::Corrupt(rec.files[0].name.clone())]
        );
        fs::write(dir.join(&rec.files[1].name), b"short").expect("truncate");
        assert!(matches!(
            f.store.health(&rec, false)[0],
            FileProblem::SizeChanged { .. }
        ));
        fs::remove_file(&first).expect("delete");
        assert!(
            f.store
                .health(&rec, false)
                .contains(&FileProblem::Missing(rec.files[0].name.clone()))
        );
        // Reinstalling repairs it, fetching only what is broken.
        f.store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect("repair");
        assert!(f.store.health(&rec, true).is_empty());
    }

    #[test]
    fn removal_frees_space_and_cleans_empty_parents() {
        let f = fixture(false, None);
        let rec = f
            .store
            .install(&f.plan, &hub(&f, None), &Cancel::default(), &mut |_, _| {})
            .expect("install");
        let freed = f.store.remove(&rec.key).expect("remove");
        assert!(freed >= rec.total_size());
        assert!(f.store.list().0.is_empty());
        assert!(!f.store.root.join("hf").exists());
        assert!(matches!(
            f.store.remove(&rec.key),
            Err(ModelError::NotInstalled(_))
        ));
    }

    #[test]
    fn unreadable_records_are_reported_not_listed() {
        let f = fixture(false, None);
        let dir = f.store.root.join("broken/Q8_0");
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join(RECORD), b"{ not json").expect("write");
        let (ok, bad) = f.store.list();
        assert!(ok.is_empty());
        assert_eq!(bad.len(), 1);
    }

    #[test]
    fn catalog_records_are_pinned_and_safe() {
        let c = crate::catalog::Catalog::bundled().expect("catalog");
        for m in &c.models {
            for q in &m.quants {
                let r = ModelRecord::from_catalog(m, q);
                assert!(!r.key.contains(".."));
                assert_eq!(r.revision.len(), 40);
                assert!(r.files.iter().all(|f| !f.name.contains('/')));
            }
        }
        assert_eq!(sanitize("../evil"), "_evil");
        assert_eq!(sanitize("a/b"), "a_b");
    }
}
