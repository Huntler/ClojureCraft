//! The index of an open project's code: one chunk per definition of its own sources and of the
//! Clojure sources in its dependency jars, searched like a documentation collection.
//!
//! The index is rebuilt whenever the project changes. Vectors of chunks whose text is unchanged
//! are taken from the previous index, so a rebuild only embeds what changed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::build::{BuildError, embedding_text};
use crate::chunk::Chunk;
use crate::collection::{CollectionInfo, INFO, read_info};
use crate::embed::{Cancelled, EmbedError, Embedder};
use crate::index::{DATABASE, IndexReader, IndexWriter, Provenance};

/// Version of the project index layout; a change discards cached vectors.
pub const PROJECT_FORMAT: u32 = 1;
/// `manifest_revision` of a project index, which has no knowledge manifest.
pub const PROJECT_REVISION: &str = "project";
/// Chunks embedded per call.
const EMBED_STEP: usize = 16;

/// Vectors of the index in `dir`, by the text they were computed from, when it was built with
/// the same embedding model.
fn cached_vectors(dir: &Path, embedder: &dyn Embedder) -> HashMap<String, Vec<f32>> {
    let usable = read_info(dir)
        .is_ok_and(|i| i.format == PROJECT_FORMAT && i.embedding.as_ref() == Some(embedder.identity()));
    let Some(reader) = usable
        .then(|| IndexReader::open(&dir.join(DATABASE)).ok())
        .flatten()
    else {
        return HashMap::new();
    };
    let Ok(vectors) = reader.vectors() else {
        return HashMap::new();
    };
    let ids: Vec<i64> = vectors.iter().map(|(id, _)| *id).collect();
    let Ok(chunks) = reader.by_rowids(&ids) else {
        return HashMap::new();
    };
    let texts: HashMap<i64, String> = chunks
        .into_iter()
        .map(|c| (c.rowid, embedding_text(&c.chunk)))
        .collect();
    vectors
        .into_iter()
        .filter_map(|(id, v)| texts.get(&id).map(|t| (t.clone(), v)))
        .collect()
}

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    dir.with_file_name(name)
}

/// Writes `chunks` as the project index in `dir`, replacing the previous one only once the new
/// one is complete. Returns the new index's description and how many chunks were embedded anew.
pub fn build_project_index(
    dir: &Path,
    chunks: &[Chunk],
    provenances: &[Provenance],
    clojure_version: &str,
    mut embedder: Option<&mut dyn Embedder>,
    cancelled: &Cancelled<'_>,
) -> Result<(CollectionInfo, usize), BuildError> {
    let io = |e: std::io::Error| BuildError::Io(e.to_string());
    let staging = sibling(dir, ".building");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(io)?;
    }
    fs::create_dir_all(&staging).map_err(io)?;
    let result = (|| {
        let mut writer = IndexWriter::create(&staging.join(DATABASE))?;
        for p in provenances {
            writer.add_source(p)?;
        }
        let rowids = writer.add_chunks(chunks)?;
        let mut identity = None;
        let mut embedded = 0;
        if let Some(embedder) = embedder.as_deref_mut() {
            identity = Some(embedder.identity().clone());
            let cache = cached_vectors(dir, embedder);
            let mut rows = Vec::with_capacity(chunks.len());
            let mut missing: Vec<(i64, String)> = Vec::new();
            for (c, id) in chunks.iter().zip(&rowids) {
                let text = embedding_text(c);
                match cache.get(&text) {
                    Some(v) => rows.push((*id, v.clone())),
                    None => missing.push((*id, text)),
                }
            }
            for batch in missing.chunks(EMBED_STEP) {
                if cancelled() {
                    return Err(BuildError::Cancelled);
                }
                let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
                let vectors = embedder.embed(&texts, cancelled).map_err(|e| match e {
                    EmbedError::Cancelled => BuildError::Cancelled,
                    other => BuildError::Embedding(other),
                })?;
                rows.extend(batch.iter().map(|(id, _)| *id).zip(vectors));
            }
            embedded = missing.len();
            writer.add_embeddings(&rows)?;
        }
        writer.finish()?;
        let info = CollectionInfo {
            format: PROJECT_FORMAT,
            manifest_revision: PROJECT_REVISION.to_owned(),
            clojure_version: clojure_version.to_owned(),
            site_commit: String::new(),
            chunks: chunks.len(),
            api_entries: 0,
            page_sections: 0,
            pages: 0,
            embedding: identity,
            built_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        let json = serde_json::to_string_pretty(&info).map_err(|e| BuildError::Io(e.to_string()))?;
        fs::write(staging.join(INFO), json).map_err(io)?;
        Ok((info, embedded))
    })();
    let built = match result {
        Ok(b) => b,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let old = sibling(dir, ".old");
    let _ = fs::remove_dir_all(&old);
    if dir.exists() {
        fs::rename(dir, &old).map_err(io)?;
    }
    fs::rename(&staging, dir).map_err(io)?;
    let _ = fs::remove_dir_all(&old);
    Ok(built)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{ChunkKind, SourceKey};
    use crate::embed::EmbedderIdentity;
    use crate::search::{Query, Retriever};

    struct Counting {
        identity: EmbedderIdentity,
        embedded: Vec<String>,
    }

    impl Embedder for Counting {
        fn identity(&self) -> &EmbedderIdentity {
            &self.identity
        }
        fn embed(&mut self, texts: &[String], _: &Cancelled<'_>) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.embedded.extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    fn chunk(ns: &str, name: &str, text: &str, restricted: bool) -> Chunk {
        Chunk {
            id: format!("project:{ns}/{name}"),
            kind: ChunkKind::Project,
            source: SourceKey::Project,
            title: format!("{ns}/{name}"),
            namespace: Some(ns.into()),
            symbol: Some(name.into()),
            url: format!("src/{ns}.clj"),
            since: None,
            restricted,
            mentions: Vec::new(),
            text: text.into(),
        }
    }

    fn provenance() -> Provenance {
        Provenance {
            source: SourceKey::Project,
            name: "shop".into(),
            license: "project code".into(),
            attribution: String::new(),
            revision: String::new(),
            origin: "/tmp/shop".into(),
        }
    }

    #[test]
    fn a_rebuild_embeds_only_changed_chunks_and_search_hides_restricted_ones() {
        let root = tempfile::tempdir().expect("temp dir");
        let dir = root.path().join("index");
        let mut e = Counting {
            identity: EmbedderIdentity {
                model_id: "m".into(),
                name: "m".into(),
                repo: String::new(),
                revision: String::new(),
                sha256: "00".into(),
                dimensions: 2,
                query_instruction: String::new(),
            },
            embedded: Vec::new(),
        };
        let first = [
            chunk("shop.util", "clean", "(defn clean [s])\nTrims a name.", false),
            chunk(
                "shop.io",
                "load-orders",
                "(defn load-orders [path])\nReads orders.",
                true,
            ),
        ];
        let (info, n) =
            build_project_index(&dir, &first, &[provenance()], "1.12.4", Some(&mut e), &|| false).unwrap();
        assert_eq!((info.chunks, n), (2, 2));

        let second = [
            first[0].clone(),
            chunk(
                "shop.io",
                "load-orders",
                "(defn load-orders [path])\nReads all orders.",
                true,
            ),
        ];
        let (_, n) =
            build_project_index(&dir, &second, &[provenance()], "1.12.4", Some(&mut e), &|| false).unwrap();
        assert_eq!(n, 1, "the unchanged chunk keeps its vector");
        assert!(!sibling(&dir, ".building").exists() && !sibling(&dir, ".old").exists());

        let mut r = Retriever::open(&dir).unwrap();
        let hits = r.search(&Query::new("orders names"), None).unwrap().hits;
        assert_eq!(
            hits.iter().map(|h| h.chunk.title.as_str()).collect::<Vec<_>>(),
            ["shop.util/clean"]
        );
        assert_eq!(hits[0].provenance.source, SourceKey::Project);
        let mut q = Query::new("orders");
        q.include_restricted = true;
        assert_eq!(
            r.search(&q, None).unwrap().hits[0].chunk.title,
            "shop.io/load-orders"
        );
    }
}
