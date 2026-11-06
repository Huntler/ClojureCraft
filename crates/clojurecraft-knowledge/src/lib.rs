//! Versioned, offline Clojure documentation for ClojureCraft.
//!
//! A *collection* is built from sources pinned in the knowledge manifest shipped with the
//! application: the API documentation of the installed Clojure runtime (read from var metadata
//! inside the sandbox) and selected clojure.org reference and guide pages (downloaded at a pinned
//! commit and verified by SHA-256). Sources are chunked by symbol and by section, indexed for
//! exact symbol lookup and full-text search in SQLite, and optionally embedded for semantic search
//! with a small local embedding model run through llama.cpp. Every chunk keeps its source URL,
//! title, symbol, namespace, version, source revision, and license.
//!
//! Collections are replaced atomically: a new one is built and verified next to the active one
//! and only then activated; the previous collection stays installed for rollback.
//!
//! Retrieved text is reference data. [`prompt`] presents it to models in delimited sections
//! marked as such, never as instructions.

pub mod asciidoc;
pub mod build;
pub mod chunk;
pub mod collection;
pub mod embed;
pub mod index;
pub mod manifest;
pub mod prompt;
pub mod search;

pub use build::{BuildError, BuildProgress, Phase, Sources, build_collection, extract_api, fetch_pages};
pub use chunk::{Chunk, ChunkKind};
pub use collection::{CollectionInfo, Collections, InstalledCollection, UpdateReason, update_reasons};
pub use embed::{EmbedError, Embedder, EmbedderIdentity, LlamaEmbedder, LlamaEmbedderConfig};
pub use index::Provenance;
pub use manifest::KnowledgeManifest;
pub use search::{Hit, Query, Retriever, Retrievers, SearchResults, Semantic, Signals};

use std::path::PathBuf;

use clojurecraft_runtime::DataDirs;
use clojurecraft_runtime::models::ModelStore;

/// Where collections, downloaded pages, and the embedding model live in the data directory.
#[derive(Clone, Debug)]
pub struct KnowledgeDirs {
    root: PathBuf,
}

impl KnowledgeDirs {
    pub fn new(dirs: &DataDirs) -> Self {
        Self {
            root: dirs.root().join("knowledge"),
        }
    }

    pub fn collections(&self) -> Collections {
        Collections::new(self.root.join("collections"))
    }

    /// Downloaded source pages (verified, reused by later builds).
    pub fn downloads(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// The embedding model, kept apart from the generator models.
    pub fn model_store(&self) -> ModelStore {
        ModelStore::new(self.root.join("models"), self.root.join("downloads"))
    }

    /// Records a running embedding server so one left behind by a crash can be stopped.
    pub fn embed_pid_file(&self) -> PathBuf {
        self.root.join("llama-embedding.pid")
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}
