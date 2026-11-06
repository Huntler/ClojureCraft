//! Embedding vectors for semantic search.
//!
//! [`Embedder`] is the replaceable interface. [`LlamaEmbedder`] runs the pinned embedding model in
//! its own `llama-server` process (embedding mode, private socket, per-session key), separate
//! from the generator. [`HashEmbedder`] is a deterministic stand-in for tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clojurecraft_inference::LoadError;
use clojurecraft_inference::http::{Control, HttpError};
use clojurecraft_inference::process::{EmbeddingMode, LlamaServer, ServerConfig};
use serde::{Deserialize, Serialize};

use crate::manifest::EmbeddingModel;

/// Texts sent to the server per request.
const BATCH: usize = 8;
/// Characters kept from one input. A token covers at least two characters of documentation or
/// code, so inputs stay within the model's batch even for dense code.
const CHARS_PER_TOKEN_FLOOR: usize = 2;
/// Input limit for search queries (see [`LlamaEmbedderConfig::max_input_tokens`]).
pub const QUERY_TOKENS: u32 = 512;
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Which model produced a set of vectors; vectors of different models are never compared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedderIdentity {
    pub model_id: String,
    pub name: String,
    pub repo: String,
    pub revision: String,
    pub sha256: String,
    pub dimensions: usize,
    /// Prefix applied to queries (see [`EmbeddingModel::query_text`]).
    pub query_instruction: String,
}

impl EmbedderIdentity {
    pub fn of(model: &EmbeddingModel) -> Self {
        Self {
            model_id: model.id.clone(),
            name: model.name.clone(),
            repo: model.repo.clone(),
            revision: model.revision.clone(),
            sha256: model.file.sha256.clone(),
            dimensions: model.dimensions,
            query_instruction: model.query_instruction.clone(),
        }
    }

    pub fn query_text(&self, query: &str) -> String {
        format!("Instruct: {}\nQuery:{query}", self.query_instruction)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("the embedding model could not be started: {0}")]
    Load(#[from] LoadError),
    #[error("embedding request failed: {0}")]
    Http(#[from] HttpError),
    #[error("the embedding server answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unexpected embedding response: {0}")]
    Malformed(String),
    #[error("the embedding server stopped: {0}")]
    Exited(String),
    #[error("cancelled")]
    Cancelled,
}

/// Polled by long operations; returns true once the work should stop.
pub type Cancelled<'a> = dyn Fn() -> bool + Sync + 'a;

pub trait Embedder: Send {
    fn identity(&self) -> &EmbedderIdentity;

    /// Unit-length vectors for `texts`, in order. `cancelled` is polled between batches.
    fn embed(&mut self, texts: &[String], cancelled: &Cancelled<'_>) -> Result<Vec<Vec<f32>>, EmbedError>;
}

pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Where and how to run the embedding model.
#[derive(Clone, Debug)]
pub struct LlamaEmbedderConfig {
    pub server: PathBuf,
    pub model_file: PathBuf,
    pub model: EmbeddingModel,
    /// Keep the model off the GPU (it then never competes with the generator for GPU memory).
    pub cpu_only: bool,
    pub threads: Option<u32>,
    pub pid_file: Option<PathBuf>,
    /// Longest input in tokens, below the model's limit. Search queries are short, and a smaller
    /// batch needs much less memory; longer inputs are shortened.
    pub max_input_tokens: Option<u32>,
}

impl LlamaEmbedderConfig {
    fn input_tokens(&self) -> u32 {
        self.max_input_tokens
            .map_or(self.model.max_tokens, |t| t.clamp(64, self.model.max_tokens))
    }
}

pub struct LlamaEmbedder {
    identity: EmbedderIdentity,
    max_chars: usize,
    server: LlamaServer,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

impl LlamaEmbedder {
    /// Starts the server and waits until the model is loaded.
    pub fn start(cfg: &LlamaEmbedderConfig, cancelled: &Cancelled<'_>) -> Result<Self, EmbedError> {
        let server_cfg = ServerConfig {
            server: cfg.server.clone(),
            model: cfg.model_file.clone(),
            context: cfg.input_tokens(),
            gpu_layers: if cfg.cpu_only { Some(0) } else { None },
            threads: cfg.threads,
            pid_file: cfg.pid_file.clone(),
            socket_base: None,
            embedding: Some(EmbeddingMode {
                pooling: cfg.model.pooling.clone(),
                batch: cfg.input_tokens(),
                cpu_only: cfg.cpu_only,
            }),
        };
        let mut server = LlamaServer::start(&server_cfg)?;
        let cancel = clojurecraft_inference::Cancel::default();
        let backend = if cfg.cpu_only { "CPU" } else { "GPU" };
        let done = AtomicBool::new(false);
        // The readiness wait polls the server's own flag; mirror the caller's into it meanwhile.
        let ready = std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::SeqCst) {
                    if cancelled() {
                        cancel.cancel();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            let r = server.wait_ready(LOAD_TIMEOUT, &cancel, backend);
            done.store(true, Ordering::SeqCst);
            r
        });
        if let Err(e) = ready {
            server.stop();
            return Err(e.into());
        }
        Ok(Self {
            identity: EmbedderIdentity::of(&cfg.model),
            max_chars: cfg.input_tokens() as usize * CHARS_PER_TOKEN_FLOOR,
            server,
        })
    }

    pub fn log_tail(&self, lines: usize) -> String {
        self.server.log_tail(lines)
    }

    pub fn stop(&mut self) {
        self.server.stop();
    }

    fn request(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let body = serde_json::json!({ "input": inputs, "encoding_format": "float" }).to_string();
        let control = Control::new(
            clojurecraft_inference::Cancel::default(),
            Some(REQUEST_TIMEOUT),
            Some(Duration::from_secs(120)),
        );
        let response = match self
            .server
            .request("POST", "/v1/embeddings", Some(body.as_bytes()), &control)
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(status) = self.server.exited() {
                    return Err(EmbedError::Exited(status));
                }
                return Err(e.into());
            }
        };
        let status = response.status;
        let text = response.text(64 << 20)?;
        if status != 200 {
            return Err(EmbedError::Status {
                status,
                body: text.chars().take(400).collect(),
            });
        }
        let mut parsed: EmbeddingResponse =
            serde_json::from_str(&text).map_err(|e| EmbedError::Malformed(e.to_string()))?;
        parsed.data.sort_by_key(|d| d.index);
        if parsed.data.len() != inputs.len() {
            return Err(EmbedError::Malformed(format!(
                "{} vectors for {} inputs",
                parsed.data.len(),
                inputs.len()
            )));
        }
        parsed
            .data
            .into_iter()
            .map(|d| {
                if d.embedding.len() == self.identity.dimensions {
                    let mut v = d.embedding;
                    normalize(&mut v);
                    Ok(v)
                } else {
                    Err(EmbedError::Malformed(format!(
                        "{} dimensions, expected {}",
                        d.embedding.len(),
                        self.identity.dimensions
                    )))
                }
            })
            .collect()
    }
}

/// Shortens `text` to at most `max` bytes at a character boundary.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

impl Embedder for LlamaEmbedder {
    fn identity(&self) -> &EmbedderIdentity {
        &self.identity
    }

    fn embed(&mut self, texts: &[String], cancelled: &Cancelled<'_>) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            if cancelled() {
                return Err(EmbedError::Cancelled);
            }
            let inputs: Vec<String> = batch.iter().map(|t| clip(t, self.max_chars)).collect();
            out.extend(self.request(&inputs)?);
        }
        Ok(out)
    }
}

impl Drop for LlamaEmbedder {
    fn drop(&mut self) {
        self.server.stop();
    }
}

/// Deterministic bag-of-words vectors: texts sharing words point in similar directions. Only for
/// tests of the retrieval plumbing; it carries no semantic knowledge.
pub struct HashEmbedder {
    identity: EmbedderIdentity,
}

impl HashEmbedder {
    pub fn new(dimensions: usize) -> Self {
        Self {
            identity: EmbedderIdentity {
                model_id: "hash".into(),
                name: "Hash embedder".into(),
                repo: "local/hash".into(),
                revision: "0".repeat(40),
                sha256: "0".repeat(64),
                dimensions,
                query_instruction: "find".into(),
            },
        }
    }

    pub fn vector(&self, text: &str) -> Vec<f32> {
        let dims = self.identity.dimensions;
        let mut v = vec![0.0f32; dims];
        // Queries carry an instruction prefix; only their query part is embedded.
        let text = text.rsplit("Query:").next().unwrap_or(text);
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(str::to_lowercase)
        {
            let h = word.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
            let slot = usize::try_from(h % dims as u64).unwrap_or(0);
            v[slot] += 1.0;
        }
        normalize(&mut v);
        v
    }
}

impl Embedder for HashEmbedder {
    fn identity(&self) -> &EmbedderIdentity {
        &self.identity
    }

    fn embed(&mut self, texts: &[String], cancelled: &Cancelled<'_>) -> Result<Vec<Vec<f32>>, EmbedError> {
        if cancelled() {
            return Err(EmbedError::Cancelled);
        }
        Ok(texts.iter().map(|t| self.vector(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_respects_character_boundaries() {
        assert_eq!(clip("añb", 2), "a");
        assert_eq!(clip("abc", 10), "abc");
    }

    #[test]
    fn vectors_are_unit_length() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
    }
}
