//! The on-disk index of one documentation collection: a SQLite database with the chunks, their
//! provenance, a full-text index (FTS5), and optional embedding vectors.
//!
//! A collection's database is written once while it is built in a staging directory and only
//! read afterwards, opened immutable so readers never create journal files next to it.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::chunk::{Chunk, ChunkKind, SourceKey};

pub const DATABASE: &str = "index.sqlite3";

/// Where documentation of one source comes from and under which terms it is used.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    pub source: SourceKey,
    pub name: String,
    pub license: String,
    pub attribution: String,
    /// The pinned revision of the source: a repository commit, or the Clojure version for API
    /// documentation read from the runtime.
    pub revision: String,
    /// Where the source can be inspected.
    pub origin: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("documentation index error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("documentation index is damaged: {0}")]
    Damaged(String),
}

const SCHEMA: &str = "
CREATE TABLE sources(
  key TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  license TEXT NOT NULL,
  attribution TEXT NOT NULL,
  revision TEXT NOT NULL,
  origin TEXT NOT NULL
);
CREATE TABLE chunks(
  rowid INTEGER PRIMARY KEY,
  id TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  source TEXT NOT NULL REFERENCES sources(key),
  title TEXT NOT NULL,
  namespace TEXT,
  symbol TEXT,
  url TEXT NOT NULL,
  since TEXT,
  restricted INTEGER NOT NULL,
  mentions TEXT NOT NULL,
  text TEXT NOT NULL
);
CREATE INDEX chunks_symbol ON chunks(symbol);
CREATE VIRTUAL TABLE chunks_fts USING fts5(symbol, title, mentions, text, tokenize = 'porter unicode61');
CREATE TABLE embeddings(
  rowid INTEGER PRIMARY KEY REFERENCES chunks(rowid),
  vector BLOB NOT NULL
);
";

/// Words in symbol names are separated by punctuation; the full-text index sees them as words
/// ("group-by" also matches "group by").
fn symbol_words(chunk: &Chunk) -> String {
    match (&chunk.namespace, &chunk.symbol) {
        (Some(ns), Some(sym)) => format!("{sym} {ns}/{sym}"),
        (None, Some(sym)) => sym.clone(),
        _ => String::new(),
    }
}

pub struct IndexWriter {
    conn: Connection,
}

impl IndexWriter {
    pub fn create(path: &Path) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = DELETE; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn add_source(&self, p: &Provenance) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO sources(key, name, license, attribution, revision, origin) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![p.source.as_str(), p.name, p.license, p.attribution, p.revision, p.origin],
        )?;
        Ok(())
    }

    /// Inserts all chunks in one transaction; returns their row ids in order.
    pub fn add_chunks(&mut self, chunks: &[Chunk]) -> Result<Vec<i64>, IndexError> {
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(chunks.len());
        {
            let mut ins = tx.prepare(
                "INSERT INTO chunks(id, kind, source, title, namespace, symbol, url, since, restricted, mentions, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            let mut fts = tx.prepare(
                "INSERT INTO chunks_fts(rowid, symbol, title, mentions, text) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for c in chunks {
                let mentions = c.mentions.join(" ");
                ins.execute(params![
                    c.id,
                    c.kind.as_str(),
                    c.source.as_str(),
                    c.title,
                    c.namespace,
                    c.symbol,
                    c.url,
                    c.since,
                    c.restricted,
                    mentions,
                    c.text
                ])?;
                let rowid = tx.last_insert_rowid();
                fts.execute(params![rowid, symbol_words(c), c.title, mentions, c.text])?;
                ids.push(rowid);
            }
        }
        tx.commit()?;
        Ok(ids)
    }

    pub fn add_embeddings(&mut self, rows: &[(i64, Vec<f32>)]) -> Result<(), IndexError> {
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare("INSERT INTO embeddings(rowid, vector) VALUES (?1, ?2)")?;
            for (rowid, v) in rows {
                ins.execute(params![rowid, encode_vector(v)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Optimizes the full-text index and closes the database.
    pub fn finish(self) -> Result<(), IndexError> {
        self.conn
            .execute_batch("INSERT INTO chunks_fts(chunks_fts) VALUES ('optimize'); VACUUM;")?;
        self.conn.close().map_err(|(_, e)| IndexError::Sqlite(e))
    }
}

pub fn encode_vector(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn decode_vector(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// A stored chunk with its row id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredChunk {
    pub rowid: i64,
    pub chunk: Chunk,
}

/// Escapes a path for a SQLite `file:` URI.
fn uri_path(path: &Path) -> String {
    let mut out = String::new();
    for c in path.to_string_lossy().chars() {
        match c {
            '%' | '?' | '#' => out.push_str(&format!("%{:02X}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

pub struct IndexReader {
    conn: Connection,
}

const COLUMNS: &str =
    "rowid, id, kind, source, title, namespace, symbol, url, since, restricted, mentions, text";

fn row_to_chunk(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredChunk> {
    let kind: String = r.get(2)?;
    let source: String = r.get(3)?;
    let mentions: String = r.get(10)?;
    Ok(StoredChunk {
        rowid: r.get(0)?,
        chunk: Chunk {
            id: r.get(1)?,
            kind: ChunkKind::parse(&kind).unwrap_or(ChunkKind::Reference),
            source: if source == "api" {
                SourceKey::Api
            } else {
                SourceKey::Site
            },
            title: r.get(4)?,
            namespace: r.get(5)?,
            symbol: r.get(6)?,
            url: r.get(7)?,
            since: r.get(8)?,
            restricted: r.get(9)?,
            mentions: mentions.split_whitespace().map(str::to_owned).collect(),
            text: r.get(11)?,
        },
    })
}

impl IndexReader {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        if !path.is_file() {
            return Err(IndexError::Damaged(format!("{} is missing", path.display())));
        }
        let uri = format!("file:{}?immutable=1", uri_path(path));
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { conn })
    }

    /// Structural check used before a collection is activated and when it is opened.
    pub fn check(&self) -> Result<(), IndexError> {
        let result: String = self.conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if result != "ok" {
            return Err(IndexError::Damaged(result));
        }
        let (chunks, fts): (i64, i64) = self.conn.query_row(
            "SELECT (SELECT count(*) FROM chunks), (SELECT count(*) FROM chunks_fts)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if chunks == 0 || chunks != fts {
            return Err(IndexError::Damaged(format!(
                "{chunks} chunks but {fts} full-text entries"
            )));
        }
        Ok(())
    }

    pub fn count(&self) -> Result<usize, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    pub fn embedding_count(&self) -> Result<usize, IndexError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    pub fn sources(&self) -> Result<Vec<Provenance>, IndexError> {
        let mut st = self
            .conn
            .prepare("SELECT key, name, license, attribution, revision, origin FROM sources ORDER BY key")?;
        let rows = st.query_map([], |r| {
            let key: String = r.get(0)?;
            Ok(Provenance {
                source: if key == "api" {
                    SourceKey::Api
                } else {
                    SourceKey::Site
                },
                name: r.get(1)?,
                license: r.get(2)?,
                attribution: r.get(3)?,
                revision: r.get(4)?,
                origin: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn by_id(&self, id: &str) -> Result<Option<StoredChunk>, IndexError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM chunks WHERE id = ?1"),
                [id],
                row_to_chunk,
            )
            .optional()?)
    }

    pub fn by_rowids(&self, rowids: &[i64]) -> Result<Vec<StoredChunk>, IndexError> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT {COLUMNS} FROM chunks WHERE rowid = ?1"))?;
        let mut out = Vec::with_capacity(rowids.len());
        for id in rowids {
            if let Some(c) = st.query_row([id], row_to_chunk).optional()? {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// Chunks documenting `symbol`, optionally in one namespace.
    pub fn by_symbol(&self, namespace: Option<&str>, symbol: &str) -> Result<Vec<StoredChunk>, IndexError> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM chunks WHERE symbol = ?1 AND (?2 IS NULL OR namespace = ?2)
             ORDER BY CASE namespace WHEN 'clojure.core' THEN 0 ELSE 1 END, namespace"
        ))?;
        let rows = st.query_map(params![symbol, namespace], row_to_chunk)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Full-text matches for an FTS5 query, best first, as `(rowid, bm25)`.
    pub fn full_text(&self, fts_query: &str, limit: usize) -> Result<Vec<(i64, f64)>, IndexError> {
        let mut st = self.conn.prepare_cached(
            "SELECT rowid, bm25(chunks_fts, 8.0, 4.0, 2.0, 1.0) AS score FROM chunks_fts
             WHERE chunks_fts MATCH ?1 ORDER BY score LIMIT ?2",
        )?;
        let rows = st.query_map(params![fts_query, i64::try_from(limit).unwrap_or(50)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every stored vector.
    pub fn vectors(&self) -> Result<Vec<(i64, Vec<f32>)>, IndexError> {
        let mut st = self.conn.prepare("SELECT rowid, vector FROM embeddings")?;
        let rows = st.query_map([], |r| {
            let b: Vec<u8> = r.get(1)?;
            Ok((r.get(0)?, decode_vector(&b)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Row ids that may be returned under the given filters.
    pub fn admissible(&self, include_restricted: bool) -> Result<Vec<(i64, Option<String>)>, IndexError> {
        let mut st = self
            .conn
            .prepare_cached("SELECT rowid, since FROM chunks WHERE ?1 OR restricted = 0")?;
        let rows = st.query_map([include_restricted], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
