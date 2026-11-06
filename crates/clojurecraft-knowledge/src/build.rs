//! Building a documentation collection.
//!
//! 1. Download the pinned pages and verify each against its size and SHA-256.
//! 2. Read the API documentation from the installed Clojure runtime, inside the sandbox.
//! 3. Chunk, index, and (with an embedding model) embed everything into a fresh directory.
//! 4. Verify the result and activate it atomically; the active collection is kept until then.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clojurecraft_clojure::{API_DOCS_SOURCE, ClojureRuntime, EvalStatus, Limits};
use clojurecraft_runtime::download::{Cancel, DownloadError, download_verified};

use crate::chunk::{ApiDocs, Chunk, SourceKey, api_chunks, page_chunks, restricted_core_symbols};
use crate::collection::{CollectionError, CollectionInfo, Collections, FORMAT, INFO, InstalledCollection};
use crate::embed::{Cancelled, EmbedError, Embedder};
use crate::index::{DATABASE, IndexError, IndexWriter, Provenance};
use crate::manifest::{KnowledgeManifest, Page};

/// Chunks embedded per progress step.
const EMBED_STEP: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Sources,
    Api,
    Indexing,
    Embedding,
    Verifying,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sources => "Downloading documentation pages",
            Self::Api => "Reading API documentation from the Clojure runtime",
            Self::Indexing => "Indexing",
            Self::Embedding => "Computing embeddings",
            Self::Verifying => "Verifying the new collection",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildProgress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("cancelled")]
    Cancelled,
    #[error("downloading {page} failed: {source}")]
    Download { page: String, source: DownloadError },
    #[error("{page} is not valid UTF-8 text")]
    NotText { page: String },
    #[error("the sandbox could not be set up: {0}")]
    SandboxUnavailable(String),
    #[error("reading the API documentation failed: {0}")]
    Extraction(String),
    #[error("the Clojure runtime reports version {found}, expected {expected}")]
    ClojureVersion { found: String, expected: String },
    #[error("embedding failed: {0}")]
    Embedding(#[from] EmbedError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Collection(#[from] CollectionError),
    #[error("file system error: {0}")]
    Io(String),
}

/// The raw documentation a collection is built from.
pub struct Sources {
    pub api: ApiDocs,
    pub pages: Vec<(Page, String)>,
}

/// Downloads every pinned page into `downloads` (verified, resumable) and reads it.
pub fn fetch_pages(
    manifest: &KnowledgeManifest,
    downloads: &Path,
    cancel: &Cancel,
    progress: &mut dyn FnMut(BuildProgress),
) -> Result<Vec<(Page, String)>, BuildError> {
    let total = manifest.site.pages.len() as u64;
    let mut out = Vec::new();
    for (i, page) in manifest.site.pages.iter().enumerate() {
        progress(BuildProgress {
            phase: Phase::Sources,
            done: i as u64,
            total,
        });
        let name = page.path.rsplit('/').next().unwrap_or("page.adoc");
        let dest = downloads.join(format!("{}-{name}", &page.sha256[..12]));
        download_verified(&manifest.site.artifact(page), &dest, cancel, |_| {}).map_err(|source| {
            if matches!(source, DownloadError::Cancelled) {
                BuildError::Cancelled
            } else {
                BuildError::Download {
                    page: page.url.clone(),
                    source,
                }
            }
        })?;
        let bytes = fs::read(&dest).map_err(|e| BuildError::Io(format!("{}: {e}", dest.display())))?;
        let text = String::from_utf8(bytes).map_err(|_| BuildError::NotText {
            page: page.url.clone(),
        })?;
        out.push((page.clone(), text));
    }
    progress(BuildProgress {
        phase: Phase::Sources,
        done: total,
        total,
    });
    Ok(out)
}

/// Reads the API documentation from the installed Clojure runtime, inside the sandbox.
pub fn extract_api(runtime: &ClojureRuntime, expected_version: &str) -> Result<ApiDocs, BuildError> {
    let limits = Limits {
        wall: Duration::from_secs(120),
        cpu_seconds: 120,
        heap_mb: 512,
        max_output_bytes: 32 << 20,
    };
    let out = runtime
        .run_script("api-docs", API_DOCS_SOURCE, b"", &limits)
        .map_err(|e| BuildError::Extraction(e.to_string()))?;
    match out.status {
        EvalStatus::Completed => {}
        EvalStatus::SandboxUnavailable(why) => return Err(BuildError::SandboxUnavailable(why)),
        other => {
            let tail: String = out.stderr.lines().rev().take(8).collect::<Vec<_>>().join("\n");
            return Err(BuildError::Extraction(format!("{other:?}: {tail}")));
        }
    }
    let api: ApiDocs = serde_json::from_str(out.stdout.trim())
        .map_err(|e| BuildError::Extraction(format!("unreadable output: {e}")))?;
    if api.clojure_version != expected_version {
        return Err(BuildError::ClojureVersion {
            found: api.clojure_version,
            expected: expected_version.to_owned(),
        });
    }
    if api.entries.len() < 100 {
        return Err(BuildError::Extraction(format!(
            "only {} documented symbols were found",
            api.entries.len()
        )));
    }
    Ok(api)
}

/// The text a chunk's vector is computed from.
pub fn embedding_text(chunk: &Chunk) -> String {
    format!("{}\n\n{}", chunk.title, chunk.text)
}

fn provenances(manifest: &KnowledgeManifest, clojure_version: &str) -> [Provenance; 2] {
    [
        Provenance {
            source: SourceKey::Api,
            name: manifest.api.name.clone(),
            license: manifest.api.license.clone(),
            attribution: manifest.api.attribution.clone(),
            revision: format!("Clojure {clojure_version}"),
            origin: format!("metadata of the installed Clojure {clojure_version} runtime"),
        },
        Provenance {
            source: SourceKey::Site,
            name: manifest.site.name.clone(),
            license: manifest.site.license.clone(),
            attribution: manifest.site.attribution.clone(),
            revision: manifest.site.commit.clone(),
            origin: format!(
                "https://github.com/{}/tree/{}",
                manifest.site.repository, manifest.site.commit
            ),
        },
    ]
}

/// All chunks of `sources`, with unique identifiers.
pub fn chunk_sources(manifest: &KnowledgeManifest, sources: &Sources) -> (Vec<Chunk>, usize) {
    let mut chunks = api_chunks(&sources.api, &manifest.api, &restricted_core_symbols());
    let api_count = chunks.len();
    for (page, text) in &sources.pages {
        chunks.extend(page_chunks(page, text));
    }
    let mut seen = BTreeSet::new();
    for c in &mut chunks {
        let mut id = c.id.clone();
        let mut n = 2;
        while !seen.insert(id.clone()) {
            id = format!("{}~{n}", c.id);
            n += 1;
        }
        c.id = id;
    }
    (chunks, api_count)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Builds a collection from `sources`, verifies it, and makes it active. The previously active
/// collection stays active if anything fails or the build is cancelled.
pub fn build_collection(
    manifest: &KnowledgeManifest,
    collections: &Collections,
    sources: &Sources,
    embedder: Option<&mut dyn Embedder>,
    cancelled: &Cancelled<'_>,
    progress: &mut dyn FnMut(BuildProgress),
) -> Result<InstalledCollection, BuildError> {
    let dir = collections.build_dir()?;
    let result = write_collection(&dir, manifest, sources, embedder, cancelled, progress);
    let info = match result {
        Ok(info) => info,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    if cancelled() {
        let _ = fs::remove_dir_all(&dir);
        return Err(BuildError::Cancelled);
    }
    progress(BuildProgress {
        phase: Phase::Verifying,
        done: 0,
        total: 1,
    });
    Ok(collections.activate_built(&dir, &info)?)
}

fn write_collection(
    dir: &Path,
    manifest: &KnowledgeManifest,
    sources: &Sources,
    embedder: Option<&mut dyn Embedder>,
    cancelled: &Cancelled<'_>,
    progress: &mut dyn FnMut(BuildProgress),
) -> Result<CollectionInfo, BuildError> {
    let (chunks, api_entries) = chunk_sources(manifest, sources);
    let total = chunks.len() as u64;
    progress(BuildProgress {
        phase: Phase::Indexing,
        done: 0,
        total,
    });
    let mut writer = IndexWriter::create(&dir.join(DATABASE))?;
    for p in provenances(manifest, &sources.api.clojure_version) {
        writer.add_source(&p)?;
    }
    let rowids = writer.add_chunks(&chunks)?;
    progress(BuildProgress {
        phase: Phase::Indexing,
        done: total,
        total,
    });
    let mut identity = None;
    if let Some(embedder) = embedder {
        identity = Some(embedder.identity().clone());
        let mut rows = Vec::with_capacity(chunks.len());
        for (i, (batch, ids)) in chunks
            .chunks(EMBED_STEP)
            .zip(rowids.chunks(EMBED_STEP))
            .enumerate()
        {
            if cancelled() {
                return Err(BuildError::Cancelled);
            }
            progress(BuildProgress {
                phase: Phase::Embedding,
                done: (i * EMBED_STEP) as u64,
                total,
            });
            let texts: Vec<String> = batch.iter().map(embedding_text).collect();
            let vectors = embedder.embed(&texts, cancelled).map_err(|e| match e {
                EmbedError::Cancelled => BuildError::Cancelled,
                other => BuildError::Embedding(other),
            })?;
            rows.extend(ids.iter().copied().zip(vectors));
        }
        writer.add_embeddings(&rows)?;
        progress(BuildProgress {
            phase: Phase::Embedding,
            done: total,
            total,
        });
    }
    writer.finish()?;
    let info = CollectionInfo {
        format: FORMAT,
        manifest_revision: manifest.revision.clone(),
        clojure_version: sources.api.clojure_version.clone(),
        site_commit: manifest.site.commit.clone(),
        chunks: chunks.len(),
        api_entries,
        page_sections: chunks.len() - api_entries,
        pages: sources.pages.len(),
        embedding: identity,
        built_at: now(),
    };
    let json = serde_json::to_string_pretty(&info).map_err(|e| BuildError::Io(e.to_string()))?;
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = fs::File::create(dir.join(INFO))?;
        f.write_all(json.as_bytes())?;
        f.sync_all()
    };
    write().map_err(|e| BuildError::Io(e.to_string()))?;
    Ok(info)
}
