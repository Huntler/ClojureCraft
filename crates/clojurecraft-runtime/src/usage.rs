//! Disk usage of everything ClojureCraft downloads, so users can see and reclaim space.

use std::path::Path;

use crate::DataDirs;
use crate::models::{ModelStore, available_space, dir_size, files_matching, remove_matching};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Installed models by key.
    pub models: Vec<(String, u64)>,
    /// Installed runtime components by `component/version`.
    pub runtimes: Vec<(String, u64)>,
    /// Interrupted downloads that a later attempt resumes.
    pub partial_downloads: u64,
    /// Completed runtime archives kept for reinstallation; safe to delete.
    pub cached_archives: u64,
    /// Free space on the data directory's file system.
    pub free: Option<u64>,
}

impl DiskUsage {
    pub fn total(&self) -> u64 {
        self.models
            .iter()
            .chain(&self.runtimes)
            .map(|(_, b)| b)
            .sum::<u64>()
            + self.partial_downloads
            + self.cached_archives
    }
}

pub fn disk_usage(dirs: &DataDirs) -> DiskUsage {
    let store = ModelStore::new(dirs.models(), dirs.downloads());
    let models = store
        .list()
        .0
        .into_iter()
        .map(|r| {
            let size = dir_size(&store.dir(&r.key));
            (r.key, size)
        })
        .collect();
    let mut runtimes = Vec::new();
    for component in subdirs(&dirs.runtimes()) {
        for version in subdirs(&dirs.runtimes().join(&component)) {
            let size = dir_size(&dirs.runtimes().join(&component).join(&version));
            runtimes.push((format!("{component}/{version}"), size));
        }
    }
    DiskUsage {
        models,
        runtimes,
        partial_downloads: store.partial_download_bytes(),
        cached_archives: files_matching(&dirs.downloads(), |n| !n.ends_with(".part")),
        free: available_space(dirs.root()),
    }
}

/// Deletes completed downloads kept in the download directory (runtime archives). Installed
/// components and models are not affected.
pub fn clear_cached_archives(dirs: &DataDirs) -> u64 {
    remove_matching(&dirs.downloads(), |n| !n.ends_with(".part"))
}

fn subdirs(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_for_each_kind_of_download() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dirs = DataDirs::at(tmp.path().to_owned());
        std::fs::create_dir_all(dirs.downloads()).expect("mkdir");
        std::fs::write(dirs.downloads().join("a.tar.gz"), vec![0u8; 100]).expect("archive");
        std::fs::write(dirs.downloads().join("b.gguf.part"), vec![0u8; 40]).expect("part");
        let rt = dirs.runtimes().join("llama.cpp/b1-cpu");
        std::fs::create_dir_all(&rt).expect("mkdir");
        std::fs::write(rt.join("llama-server"), vec![0u8; 7]).expect("bin");
        let u = disk_usage(&dirs);
        assert_eq!(u.cached_archives, 100);
        assert_eq!(u.partial_downloads, 40);
        assert_eq!(u.runtimes, vec![("llama.cpp/b1-cpu".to_owned(), 7)]);
        assert!(u.free.is_some());
        assert_eq!(clear_cached_archives(&dirs), 100);
        assert_eq!(
            disk_usage(&dirs).partial_downloads,
            40,
            "partial downloads are kept"
        );
    }
}
