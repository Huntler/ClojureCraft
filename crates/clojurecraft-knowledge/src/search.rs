//! Retrieval from an installed collection.
//!
//! Three retrievers run on every query. With vectors, candidates are ranked by cosine similarity
//! plus bonuses for named symbols and matching words; without them, exact and full-text ranks are
//! merged with reciprocal rank fusion:
//!
//! - **exact symbol lookup**: a query that is a symbol (`group-by`, `str/join`,
//!   `clojure.set/rename-keys`) puts that symbol's documentation first; code-like symbols inside a
//!   longer query (`assoc-in`, `` `keep` ``) are boosted;
//! - **full-text search** (SQLite FTS5, BM25, Porter stemming) over symbol names, titles, linked
//!   symbols, and text;
//! - **semantic search** by cosine similarity of embedding vectors, when the collection has them
//!   and the same embedding model is available.
//!
//! Results can be limited to symbols available in a given Clojure version and exclude symbols
//! that generated functions may not use.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::chunk::{Chunk, SourceKey};
use crate::collection::{CollectionError, CollectionInfo, read_info};
use crate::embed::{Embedder, dot};
use crate::index::{DATABASE, IndexError, IndexReader, Provenance, StoredChunk};

/// Reciprocal rank fusion constant.
const RRF_K: f64 = 60.0;
/// Candidates taken from each retriever before merging.
const CANDIDATES: usize = 50;
const WEIGHT_EXACT_QUERY: f64 = 10.0;
const WEIGHT_EXACT_MENTION: f64 = 1.5;
/// Full-text weight in rank fusion, used when no vectors are available.
const WEIGHT_TEXT_ALONE: f64 = 1.0;
const BONUS_EXACT_QUERY: f64 = 1.0;
const BONUS_EXACT_MENTION: f64 = 0.15;
/// Largest bonus for matching words (full-text rank 1) on top of cosine similarity. Chosen with
/// `clojurecraft-devtool knowledge-eval`: 0.06 to 0.12 all improve on similarity alone; larger
/// values let word matches override meaning.
const BONUS_TEXT: f64 = 0.08;

/// Namespace aliases commonly used for the supported namespaces.
const ALIASES: &[(&str, &str)] = &[
    ("str", "clojure.string"),
    ("string", "clojure.string"),
    ("set", "clojure.set"),
    ("walk", "clojure.walk"),
    ("edn", "clojure.edn"),
    ("zip", "clojure.zip"),
    ("data", "clojure.data"),
    ("core", "clojure.core"),
];

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "does", "each", "for", "from", "how", "i",
    "if", "in", "into", "is", "it", "its", "me", "my", "of", "on", "or", "should", "so", "some", "that",
    "the", "their", "them", "then", "there", "these", "this", "those", "to", "use", "using", "want", "was",
    "we", "what", "when", "where", "which", "while", "with", "would", "you",
];

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Include symbols generated functions may not use (I/O, evaluation, interop).
    pub include_restricted: bool,
    /// Leave out symbols added after this Clojure version.
    pub clojure_version: Option<String>,
    pub retrievers: Retrievers,
}

/// Which retrievers contribute to the ranking (all by default; others are for evaluation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retrievers {
    pub exact: bool,
    pub text: bool,
    pub semantic: bool,
}

impl Default for Retrievers {
    fn default() -> Self {
        Self {
            exact: true,
            text: true,
            semantic: true,
        }
    }
}

impl Query {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            limit: 10,
            include_restricted: false,
            clojure_version: None,
            retrievers: Retrievers::default(),
        }
    }
}

/// How a hit was found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Signals {
    /// The query named this symbol.
    pub exact: bool,
    /// 1-based rank in full-text search.
    pub text_rank: Option<usize>,
    /// 1-based rank and cosine similarity in semantic search.
    pub semantic: Option<(usize, f32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub chunk: Chunk,
    pub provenance: Provenance,
    pub score: f64,
    pub signals: Signals,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Semantic {
    Used,
    /// The collection was built without vectors.
    NotIndexed,
    /// No embedding model is running.
    Unavailable,
    /// The available embedding model is not the one the collection was built with.
    ModelMismatch,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct SearchResults {
    pub hits: Vec<Hit>,
    pub semantic: Semantic,
}

#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Collection(#[from] CollectionError),
}

/// Compares dotted version numbers numerically (`1.9` < `1.10`).
pub fn version_le(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split(['.', '-']).map_while(|p| p.parse::<u64>().ok()).collect() };
    let (mut x, mut y) = (parse(a), parse(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x <= y
}

fn is_symbol_char(c: char) -> bool {
    c.is_alphanumeric() || "*+!-_'?<>=/.".contains(c)
}

/// Whether a token is clearly code rather than an English word.
fn code_like(token: &str) -> bool {
    token.chars().any(|c| "-?!*/<>=".contains(c)) && token.chars().any(char::is_alphabetic)
}

/// Splits `ns/name` and resolves common aliases.
fn split_symbol(token: &str) -> (Option<String>, String) {
    match token.split_once('/') {
        Some((ns, name)) if !ns.is_empty() && !name.is_empty() => {
            let ns = ALIASES
                .iter()
                .find(|(alias, _)| *alias == ns)
                .map_or(ns, |(_, full)| full);
            (Some(ns.to_owned()), name.to_owned())
        }
        _ => (None, token.to_owned()),
    }
}

/// The symbol a query consists of, if it is a single symbol.
fn whole_symbol(text: &str) -> Option<&str> {
    let t = text
        .trim()
        .trim_matches('`')
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    (!t.is_empty() && !t.contains(char::is_whitespace) && t.chars().all(is_symbol_char)).then_some(t)
}

/// Code-like symbols mentioned inside a longer query.
fn mentioned_symbols(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // The operator of a form, as in "(update m k f)".
    for (i, _) in text.match_indices('(') {
        let op: String = text[i + 1..].chars().take_while(|c| is_symbol_char(*c)).collect();
        if op.chars().any(char::is_alphabetic) && !out.contains(&op) {
            out.push(op);
        }
    }
    for (i, part) in text.split('`').enumerate() {
        let in_ticks = i % 2 == 1;
        for raw in part.split(|c: char| !is_symbol_char(c)) {
            let token = raw.trim_matches(|c: char| c == '.' || c == '\'');
            if token.is_empty() || token.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if (in_ticks || code_like(token)) && !out.iter().any(|t| t == token) {
                out.push(token.to_owned());
            }
        }
    }
    out
}

/// An FTS5 query matching any of the meaningful words; hyphenated symbols become phrases.
pub fn fts_query(text: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut push = |t: String| {
        if !terms.contains(&t) && terms.len() < 32 {
            terms.push(t);
        }
    };
    for token in text.split(|c: char| !is_symbol_char(c)) {
        let words: Vec<String> = token
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        if words.len() > 1 && code_like(token) {
            push(format!("\"{}\"", words.join(" ")));
        }
        for w in words {
            if w.len() > 1 && !STOPWORDS.contains(&w.as_str()) {
                push(format!("\"{w}\""));
            }
        }
    }
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

#[allow(clippy::cast_precision_loss)]
fn rrf(weight: f64, rank: usize) -> f64 {
    weight / (RRF_K + rank as f64)
}

/// Without vectors: reciprocal rank fusion of exact and full-text ranks.
fn rank_fusion_score(exact: Option<(f64, usize)>, s: &Signals) -> f64 {
    exact.map_or(0.0, |(w, r)| rrf(w, r)) + s.text_rank.map_or(0.0, |r| rrf(WEIGHT_TEXT_ALONE, r))
}

/// With vectors: cosine similarity ranks the candidates. Unlike rank fusion it keeps how much
/// better the best match is; a named symbol is lifted above everything, a mentioned one strongly,
/// and matching words only break near-ties.
fn similarity_score(exact: Option<f64>, s: &Signals) -> f64 {
    let exact = match exact {
        Some(w) if w >= WEIGHT_EXACT_QUERY => BONUS_EXACT_QUERY,
        Some(_) => BONUS_EXACT_MENTION,
        None => 0.0,
    };
    let sim = s.semantic.map_or(0.0, |(_, sim)| f64::from(sim));
    let words = s.text_rank.map_or(0.0, |r| rrf(RRF_K * BONUS_TEXT, r));
    exact + sim + words
}

pub struct Retriever {
    reader: IndexReader,
    info: CollectionInfo,
    sources: Vec<Provenance>,
    vectors: Option<Vec<(i64, Vec<f32>)>>,
}

impl Retriever {
    /// Opens the collection in `dir` (its database and description).
    pub fn open(dir: &Path) -> Result<Self, RetrievalError> {
        let info = read_info(dir)?;
        let reader = IndexReader::open(&dir.join(DATABASE))?;
        let sources = reader.sources()?;
        Ok(Self {
            reader,
            info,
            sources,
            vectors: None,
        })
    }

    pub fn info(&self) -> &CollectionInfo {
        &self.info
    }

    pub fn sources(&self) -> &[Provenance] {
        &self.sources
    }

    fn provenance(&self, source: SourceKey) -> Provenance {
        self.sources
            .iter()
            .find(|p| p.source == source)
            .cloned()
            .unwrap_or_else(|| Provenance {
                source,
                name: "unknown".into(),
                license: "unknown".into(),
                attribution: String::new(),
                revision: String::new(),
                origin: String::new(),
            })
    }

    fn hit(&self, stored: StoredChunk, score: f64, signals: Signals) -> Hit {
        Hit {
            provenance: self.provenance(stored.chunk.source),
            chunk: stored.chunk,
            score,
            signals,
        }
    }

    /// One chunk by its identifier.
    pub fn chunk(&self, id: &str) -> Result<Option<Hit>, RetrievalError> {
        Ok(self
            .reader
            .by_id(id)?
            .map(|c| self.hit(c, 0.0, Signals::default())))
    }

    /// Documentation of a symbol (`map`, `str/join`, `clojure.set/union`).
    pub fn lookup(&self, symbol: &str) -> Result<Vec<Hit>, RetrievalError> {
        let (ns, name) = split_symbol(symbol);
        Ok(self
            .reader
            .by_symbol(ns.as_deref(), &name)?
            .into_iter()
            .map(|c| {
                self.hit(
                    c,
                    0.0,
                    Signals {
                        exact: true,
                        ..Signals::default()
                    },
                )
            })
            .collect())
    }

    fn admissible(&self, q: &Query) -> Result<HashSet<i64>, RetrievalError> {
        Ok(self
            .reader
            .admissible(q.include_restricted)?
            .into_iter()
            .filter(|(_, since)| match (&q.clojure_version, since) {
                (Some(target), Some(since)) => version_le(since, target),
                _ => true,
            })
            .map(|(id, _)| id)
            .collect())
    }

    fn semantic(
        &mut self,
        q: &Query,
        admissible: &HashSet<i64>,
        embedder: Option<&mut dyn Embedder>,
    ) -> (Vec<(i64, f32)>, Semantic) {
        let Some(expected) = &self.info.embedding else {
            return (Vec::new(), Semantic::NotIndexed);
        };
        let Some(embedder) = embedder else {
            return (Vec::new(), Semantic::Unavailable);
        };
        if embedder.identity().sha256 != expected.sha256
            || embedder.identity().dimensions != expected.dimensions
        {
            return (Vec::new(), Semantic::ModelMismatch);
        }
        let query = vec![expected.query_text(q.text.trim())];
        let v = match embedder.embed(&query, &|| false) {
            Ok(mut v) if v.len() == 1 => v.remove(0),
            Ok(_) => return (Vec::new(), Semantic::Failed("no vector returned".into())),
            Err(e) => return (Vec::new(), Semantic::Failed(e.to_string())),
        };
        if self.vectors.is_none() {
            match self.reader.vectors() {
                Ok(vs) => self.vectors = Some(vs),
                Err(e) => return (Vec::new(), Semantic::Failed(e.to_string())),
            }
        }
        let mut scored: Vec<(i64, f32)> = self
            .vectors
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|(id, _)| admissible.contains(id))
            .map(|(id, d)| (*id, dot(&v, d)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        scored.truncate(CANDIDATES);
        (scored, Semantic::Used)
    }

    /// Searches the collection, merging the rankings of all available retrievers.
    pub fn search(
        &mut self,
        q: &Query,
        embedder: Option<&mut dyn Embedder>,
    ) -> Result<SearchResults, RetrievalError> {
        let admissible = self.admissible(q)?;
        // Per candidate: the exact-match weight and rank, and the other retrievers' signals.
        let mut found: HashMap<i64, (Option<(f64, usize)>, Signals)> = HashMap::new();

        // Exact symbols.
        let symbols: Vec<(String, f64)> = match whole_symbol(&q.text) {
            _ if !q.retrievers.exact => Vec::new(),
            Some(sym) => vec![(sym.to_owned(), WEIGHT_EXACT_QUERY)],
            None => mentioned_symbols(&q.text)
                .into_iter()
                .map(|s| (s, WEIGHT_EXACT_MENTION))
                .collect(),
        };
        let mut exact: Vec<(i64, f64)> = Vec::new();
        for (token, weight) in symbols {
            let (ns, name) = split_symbol(&token);
            for c in self.reader.by_symbol(ns.as_deref(), &name)? {
                exact.push((c.rowid, weight));
            }
        }
        for (rank, (id, w)) in exact
            .into_iter()
            .filter(|(id, _)| admissible.contains(id))
            .enumerate()
        {
            let e = found.entry(id).or_default();
            if e.0.is_none_or(|(old, _)| old < w) {
                e.0 = Some((w, rank + 1));
            }
            e.1.exact = true;
        }

        // Semantic.
        let (semantic_hits, semantic) =
            self.semantic(q, &admissible, embedder.filter(|_| q.retrievers.semantic));
        for (rank, (id, sim)) in semantic_hits.into_iter().enumerate() {
            found.entry(id).or_default().1.semantic = Some((rank + 1, sim));
        }

        // Full text.
        if let Some(fts) = fts_query(&q.text).filter(|_| q.retrievers.text) {
            let hits = self.reader.full_text(&fts, CANDIDATES * 2)?;
            for (rank, (id, _)) in hits
                .into_iter()
                .filter(|(id, _)| admissible.contains(id))
                .take(CANDIDATES)
                .enumerate()
            {
                found.entry(id).or_default().1.text_rank = Some(rank + 1);
            }
        }

        let use_similarity = semantic == Semantic::Used;
        let scores: HashMap<i64, (f64, Signals)> = found
            .into_iter()
            .map(|(id, (exact, signals))| {
                let score = if use_similarity {
                    similarity_score(exact.map(|e| e.0), &signals)
                } else {
                    rank_fusion_score(exact, &signals)
                };
                (id, (score, signals))
            })
            .collect();

        let mut ranked: Vec<(i64, f64, Signals)> =
            scores.into_iter().map(|(id, (s, sig))| (id, s, sig)).collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(q.limit);
        let ids: Vec<i64> = ranked.iter().map(|r| r.0).collect();
        let mut chunks: HashMap<i64, StoredChunk> = self
            .reader
            .by_rowids(&ids)?
            .into_iter()
            .map(|c| (c.rowid, c))
            .collect();
        let hits = ranked
            .into_iter()
            .filter_map(|(id, score, signals)| chunks.remove(&id).map(|c| self.hit(c, score, signals)))
            .collect();
        Ok(SearchResults { hits, semantic })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(version_le("1.9", "1.10"));
        assert!(version_le("1.12", "1.12.6"));
        assert!(!version_le("1.12", "1.11.4"));
    }

    #[test]
    fn queries_are_classified_into_symbols_and_words() {
        assert_eq!(whole_symbol(" group-by "), Some("group-by"));
        assert_eq!(whole_symbol("`str/join`"), Some("str/join"));
        assert_eq!(whole_symbol("group items"), None);
        assert_eq!(
            mentioned_symbols("use assoc-in, `keep`, and the map function"),
            vec!["assoc-in", "keep"]
        );
        assert_eq!(mentioned_symbols("like (update m k f) does"), vec!["update"]);
        assert_eq!(
            split_symbol("str/join"),
            (Some("clojure.string".into()), "join".into())
        );
        let fts = fts_query("Group the items by their key with group-by").unwrap_or_default();
        assert_eq!(fts, r#""group" OR "items" OR "key" OR "group by""#);
        assert_eq!(fts_query("the of a"), None);
    }
}
