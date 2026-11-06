//! Installed documentation collections.
//!
//! Each collection lives in its own directory under `knowledge/collections/`, holding the index
//! database and `collection.json`. A collection is built in a staging directory, checked, and only
//! then renamed into place and made active by atomically replacing `active.json`; the previously
//! active collection stays installed until its replacement is active, and can be restored.

use std::fs;
use std::path::{Path, PathBuf};

use clojurecraft_runtime::install::{ComponentDir, InstallError, Installed};
use serde::{Deserialize, Serialize};

use crate::embed::EmbedderIdentity;
use crate::index::{DATABASE, IndexError, IndexReader};
use crate::manifest::KnowledgeManifest;

/// Version of the index layout and chunking. A change makes installed collections out of date.
pub const FORMAT: u32 = 1;
pub const INFO: &str = "collection.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionInfo {
    pub format: u32,
    /// Revision of the knowledge manifest the collection was built from.
    pub manifest_revision: String,
    /// The Clojure version the API documentation was read from.
    pub clojure_version: String,
    pub site_commit: String,
    pub chunks: usize,
    pub api_entries: usize,
    pub page_sections: usize,
    pub pages: usize,
    /// The model whose vectors are stored, if semantic search is available.
    pub embedding: Option<EmbedderIdentity>,
    /// Seconds since the Unix epoch.
    pub built_at: u64,
}

impl CollectionInfo {
    /// Directory name of this collection: what it was built from, without the build time.
    pub fn version_id(&self) -> String {
        let emb = self.embedding.as_ref().map_or_else(
            || "text".to_owned(),
            |e| format!("emb-{}", &e.sha256[..12.min(e.sha256.len())]),
        );
        format!(
            "{}-f{}-clojure-{}-{}",
            self.manifest_revision, self.format, self.clojure_version, emb
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CollectionError {
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("collection description unreadable: {0}")]
    Info(String),
}

/// Why an installed collection should be rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateReason {
    NewSources {
        installed: String,
        available: String,
    },
    ClojureVersion {
        installed: String,
        runtime: String,
    },
    Format {
        installed: u32,
        current: u32,
    },
    /// The embedding model is installed but the collection has no vectors.
    EmbeddingsAvailable,
    /// The collection's vectors come from a model other than the installed one.
    EmbeddingModelChanged,
}

impl UpdateReason {
    pub fn describe(&self) -> String {
        match self {
            Self::NewSources { installed, available } => {
                format!("newer documentation sources ({installed} → {available})")
            }
            Self::ClojureVersion { installed, runtime } => {
                format!("built for Clojure {installed}; the runtime is Clojure {runtime}")
            }
            Self::Format { .. } => "the index format changed".into(),
            Self::EmbeddingsAvailable => {
                "semantic search can be added (the embedding model is installed)".into()
            }
            Self::EmbeddingModelChanged => "built with a different embedding model".into(),
        }
    }
}

/// What has to change for the installed collection to match this application and its runtime.
pub fn update_reasons(
    info: &CollectionInfo,
    manifest: &KnowledgeManifest,
    runtime_clojure: &str,
    embedding_installed: Option<&EmbedderIdentity>,
) -> Vec<UpdateReason> {
    let mut out = Vec::new();
    if info.manifest_revision != manifest.revision {
        out.push(UpdateReason::NewSources {
            installed: info.manifest_revision.clone(),
            available: manifest.revision.clone(),
        });
    }
    if info.clojure_version != runtime_clojure {
        out.push(UpdateReason::ClojureVersion {
            installed: info.clojure_version.clone(),
            runtime: runtime_clojure.to_owned(),
        });
    }
    if info.format != FORMAT {
        out.push(UpdateReason::Format {
            installed: info.format,
            current: FORMAT,
        });
    }
    match (&info.embedding, embedding_installed) {
        (None, Some(_)) => out.push(UpdateReason::EmbeddingsAvailable),
        (Some(a), Some(b)) if a.sha256 != b.sha256 => out.push(UpdateReason::EmbeddingModelChanged),
        _ => {}
    }
    out
}

#[derive(Clone, Debug)]
pub struct InstalledCollection {
    pub version: String,
    pub dir: PathBuf,
    pub info: CollectionInfo,
}

impl InstalledCollection {
    pub fn database(&self) -> PathBuf {
        self.dir.join(DATABASE)
    }
}

pub fn read_info(dir: &Path) -> Result<CollectionInfo, CollectionError> {
    let text = fs::read_to_string(dir.join(INFO)).map_err(|e| CollectionError::Info(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| CollectionError::Info(e.to_string()))
}

/// Checks a built collection before it may become active.
pub fn verify(dir: &Path) -> Result<CollectionInfo, String> {
    let info = read_info(dir).map_err(|e| e.to_string())?;
    let reader = IndexReader::open(&dir.join(DATABASE)).map_err(|e| e.to_string())?;
    reader.check().map_err(|e| e.to_string())?;
    let chunks = reader.count().map_err(|e| e.to_string())?;
    if chunks != info.chunks {
        return Err(format!("{chunks} chunks stored, {} described", info.chunks));
    }
    if let Some(e) = &info.embedding {
        let vectors = reader.vectors().map_err(|e| e.to_string())?;
        if vectors.len() != chunks {
            return Err(format!("{} vectors for {chunks} chunks", vectors.len()));
        }
        if let Some((_, v)) = vectors.iter().find(|(_, v)| v.len() != e.dimensions) {
            return Err(format!(
                "a vector has {} dimensions, expected {}",
                v.len(),
                e.dimensions
            ));
        }
    }
    Ok(info)
}

/// The installed collections.
#[derive(Clone, Debug)]
pub struct Collections {
    dir: ComponentDir,
    root: PathBuf,
}

impl Collections {
    pub fn new(root: PathBuf) -> Self {
        Self {
            dir: ComponentDir::new(root.clone()),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn described(&self, installed: Installed) -> Result<InstalledCollection, CollectionError> {
        let info = read_info(&installed.dir)?;
        Ok(InstalledCollection {
            version: installed.version,
            dir: installed.dir,
            info,
        })
    }

    /// The active collection; `Err` when it is present but its description cannot be read.
    pub fn active(&self) -> Option<Result<InstalledCollection, CollectionError>> {
        self.dir.active().map(|i| self.described(i))
    }

    /// The collection that was active before the current one, if still installed.
    pub fn previous(&self) -> Option<InstalledCollection> {
        let text = fs::read_to_string(self.root.join("active.json")).ok()?;
        let ptr: clojurecraft_runtime::install::ActivePointer = serde_json::from_str(&text).ok()?;
        let version = ptr.previous?;
        let dir = self.root.join(&version);
        let info = read_info(&dir).ok()?;
        Some(InstalledCollection { version, dir, info })
    }

    /// Moves a fully built collection from `built` into place after verifying it, and makes it
    /// active. On failure the active collection is untouched and `built` is removed.
    pub fn activate_built(
        &self,
        built: &Path,
        info: &CollectionInfo,
    ) -> Result<InstalledCollection, CollectionError> {
        let version = info.version_id();
        let result = self.dir.install_with(
            &version,
            |staging| {
                for name in [DATABASE, INFO] {
                    let (from, to) = (built.join(name), staging.join(name));
                    fs::rename(&from, &to).map_err(|source| InstallError::Io { path: to, source })?;
                }
                Ok(())
            },
            |staging| verify(staging).map(|_| ()),
        );
        let _ = fs::remove_dir_all(built);
        let installed = result?;
        self.prune();
        self.described(installed)
    }

    /// Makes the previous collection active again.
    pub fn rollback(&self) -> Result<InstalledCollection, CollectionError> {
        let installed = self.dir.rollback()?;
        self.described(installed)
    }

    /// Removes everything except the active and previous collections, and leftovers of
    /// interrupted builds.
    pub fn prune(&self) {
        let keep: Vec<String> = [
            self.dir.active().map(|a| a.version),
            self.previous().map(|p| p.version),
        ]
        .into_iter()
        .flatten()
        .collect();
        for version in self.dir.installed() {
            if !keep.contains(&version) {
                let _ = self.dir.remove(&version);
            }
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".build-") || name.starts_with(".staging-") || name.starts_with(".old-") {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }

    /// A fresh directory for building a collection.
    pub fn build_dir(&self) -> Result<PathBuf, CollectionError> {
        let dir = self.root.join(format!(".build-{}", std::process::id()));
        let io = |source| {
            CollectionError::Install(InstallError::Io {
                path: dir.clone(),
                source,
            })
        };
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(io)?;
        }
        fs::create_dir_all(&dir).map_err(io)?;
        Ok(dir)
    }

    /// Bytes used by installed collections.
    pub fn disk_usage(&self) -> u64 {
        clojurecraft_runtime::models::dir_size(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> CollectionInfo {
        CollectionInfo {
            format: FORMAT,
            manifest_revision: "2026.11.1".into(),
            clojure_version: "1.12.6".into(),
            site_commit: "a".repeat(40),
            chunks: 1,
            api_entries: 1,
            page_sections: 0,
            pages: 0,
            embedding: None,
            built_at: 0,
        }
    }

    #[test]
    fn updates_are_offered_for_new_sources_runtime_and_embeddings() {
        let manifest = KnowledgeManifest::bundled().expect("manifest");
        let mut i = info();
        i.manifest_revision = manifest.revision.clone();
        assert!(update_reasons(&i, &manifest, "1.12.6", None).is_empty());

        let model = EmbedderIdentity::of(&manifest.embedding);
        assert_eq!(
            update_reasons(&i, &manifest, "1.12.6", Some(&model)),
            vec![UpdateReason::EmbeddingsAvailable]
        );
        i.embedding = Some(EmbedderIdentity {
            sha256: "f".repeat(64),
            ..model.clone()
        });
        assert_eq!(
            update_reasons(&i, &manifest, "1.12.6", Some(&model)),
            vec![UpdateReason::EmbeddingModelChanged]
        );
        // Removing the embedding model later does not demand a rebuild; semantic search just
        // becomes unavailable until it is installed again.
        assert!(update_reasons(&i, &manifest, "1.12.6", None).is_empty());

        i.manifest_revision = "2020.1.1".into();
        let reasons = update_reasons(&i, &manifest, "1.13.0", None);
        assert!(matches!(reasons[0], UpdateReason::NewSources { .. }));
        assert!(matches!(reasons[1], UpdateReason::ClojureVersion { .. }));
    }

    #[test]
    fn version_ids_name_what_the_collection_was_built_from() {
        let mut i = info();
        assert_eq!(i.version_id(), "2026.11.1-f1-clojure-1.12.6-text");
        i.embedding = Some(EmbedderIdentity {
            model_id: "m".into(),
            name: "M".into(),
            repo: "o/m".into(),
            revision: "0".repeat(40),
            sha256: "06507c7b42688469c4e7".into(),
            dimensions: 4,
            query_instruction: String::new(),
        });
        assert_eq!(i.version_id(), "2026.11.1-f1-clojure-1.12.6-emb-06507c7b4268");
    }
}
