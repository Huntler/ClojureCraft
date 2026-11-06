//! GGUF header reading. Only the metadata section is parsed: it states the architecture, layer
//! and attention geometry, context length, and split layout, which is everything needed to judge
//! compatibility and estimate memory before downloading the weights.
//!
//! Parsing works on a prefix of the file. When the prefix ends inside the metadata, the error
//! says so ([`GgufError::Truncated`]) and the caller can fetch more bytes.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

const MAGIC: &[u8; 4] = b"GGUF";
/// Metadata larger than this is not plausible for a model file and is rejected.
pub const MAX_HEADER_BYTES: usize = 64 * 1024 * 1024;
/// Integer arrays up to this length are kept (per-layer head counts); longer ones are skipped.
const MAX_KEPT_ARRAY: u64 = 4096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GgufError {
    #[error("not a GGUF file")]
    NotGguf,
    #[error("GGUF version {0} is not supported")]
    Version(u32),
    #[error("the metadata continues beyond the first {have} bytes")]
    Truncated { have: usize },
    #[error("malformed GGUF metadata: {0}")]
    Malformed(&'static str),
    #[error("could not read the file: {0}")]
    Io(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(String),
    /// Integer array short enough to keep.
    Ints(Vec<i128>),
    /// Any other array; only its length is kept.
    Array(u64),
}

impl MetaValue {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Int(v) => u64::try_from(*v).ok(),
            // Per-layer values (for example head counts in hybrid models): the largest one is
            // the safe figure for memory estimates.
            Self::Ints(v) => v.iter().copied().max().and_then(|m| u64::try_from(m).ok()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GgufHeader {
    pub version: u32,
    pub tensor_count: u64,
    pub metadata: BTreeMap<String, MetaValue>,
    /// Bytes from the start of the file to the end of the metadata.
    pub header_len: usize,
}

/// The facts ClojureCraft uses from a model's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelFacts {
    pub architecture: Option<String>,
    pub name: Option<String>,
    pub block_count: Option<u32>,
    pub context_length: Option<u32>,
    pub head_count: Option<u32>,
    pub head_count_kv: Option<u32>,
    pub embedding_length: Option<u32>,
    pub key_length: Option<u32>,
    pub value_length: Option<u32>,
    /// Number of files the model is split into (absent or 1 for a single file).
    pub split_count: Option<u16>,
    pub split_index: Option<u16>,
    /// Tensors in this file (for split models, in this part only).
    pub tensor_count: u64,
}

impl ModelFacts {
    /// Estimated f16 KV-cache bytes per context token. An estimate: hybrid architectures that keep
    /// a cache on only some layers need less.
    pub fn kv_bytes_per_token(&self) -> Option<u64> {
        let layers = u64::from(self.block_count?);
        let kv_heads = u64::from(self.head_count_kv.or(self.head_count)?);
        let head_dim = self.embedding_length? / self.head_count?.max(1);
        let k = u64::from(self.key_length.unwrap_or(head_dim));
        let v = u64::from(self.value_length.unwrap_or(head_dim));
        Some(layers * kv_heads * (k + v) * 2)
    }
}

impl GgufHeader {
    fn get(&self, key: &str) -> Option<&MetaValue> {
        self.metadata.get(key)
    }

    fn u32_of(&self, key: &str) -> Option<u32> {
        self.get(key)?.as_u64().and_then(|v| u32::try_from(v).ok())
    }

    pub fn facts(&self) -> ModelFacts {
        let arch = match self.get("general.architecture") {
            Some(MetaValue::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let a = |suffix: &str| arch.as_ref().and_then(|a| self.u32_of(&format!("{a}.{suffix}")));
        ModelFacts {
            name: match self.get("general.name") {
                Some(MetaValue::Str(s)) => Some(s.clone()),
                _ => None,
            },
            block_count: a("block_count"),
            context_length: a("context_length"),
            head_count: a("attention.head_count"),
            head_count_kv: a("attention.head_count_kv"),
            embedding_length: a("embedding_length"),
            key_length: a("attention.key_length"),
            value_length: a("attention.value_length"),
            split_count: self.u32_of("split.count").and_then(|v| u16::try_from(v).ok()),
            split_index: self.u32_of("split.no").and_then(|v| u16::try_from(v).ok()),
            tensor_count: self.tensor_count,
            architecture: arch,
        }
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], GgufError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(GgufError::Malformed("length overflow"))?;
        if end > MAX_HEADER_BYTES {
            return Err(GgufError::Malformed("metadata is implausibly large"));
        }
        let slice = self.bytes.get(self.pos..end).ok_or(GgufError::Truncated {
            have: self.bytes.len(),
        })?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], GgufError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, GgufError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, GgufError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn len(&mut self) -> Result<usize, GgufError> {
        usize::try_from(self.u64()?).map_err(|_| GgufError::Malformed("length overflow"))
    }

    fn string(&mut self) -> Result<String, GgufError> {
        let n = self.len()?;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    /// Reads one value of `ty`. Strings inside arrays are skipped rather than stored.
    fn value(&mut self, ty: u32, keep: bool) -> Result<MetaValue, GgufError> {
        Ok(match ty {
            0 => MetaValue::Int(self.array::<1>()?[0].into()),
            1 => MetaValue::Int(i8::from_le_bytes(self.array()?).into()),
            2 => MetaValue::Int(u16::from_le_bytes(self.array()?).into()),
            3 => MetaValue::Int(i16::from_le_bytes(self.array()?).into()),
            4 => MetaValue::Int(self.u32()?.into()),
            5 => MetaValue::Int(i32::from_le_bytes(self.array()?).into()),
            6 => MetaValue::Float(f32::from_le_bytes(self.array()?).into()),
            7 => MetaValue::Bool(self.array::<1>()?[0] != 0),
            8 => {
                if keep {
                    MetaValue::Str(self.string()?)
                } else {
                    let n = self.len()?;
                    self.take(n)?;
                    MetaValue::Array(0)
                }
            }
            9 => {
                let elem = self.u32()?;
                let count = self.u64()?;
                let keep_ints = keep && count <= MAX_KEPT_ARRAY && matches!(elem, 0..=5 | 10 | 11);
                let mut ints = Vec::new();
                for _ in 0..count {
                    if elem == 9 {
                        return Err(GgufError::Malformed("nested arrays are not supported"));
                    }
                    match self.value(elem, false)? {
                        MetaValue::Int(v) if keep_ints => ints.push(v),
                        _ => {}
                    }
                }
                if keep_ints {
                    MetaValue::Ints(ints)
                } else {
                    MetaValue::Array(count)
                }
            }
            10 => MetaValue::Int(self.u64()?.into()),
            11 => MetaValue::Int(i64::from_le_bytes(self.array()?).into()),
            12 => MetaValue::Float(f64::from_le_bytes(self.array()?)),
            _ => return Err(GgufError::Malformed("unknown value type")),
        })
    }
}

/// Parses the header from the beginning of a GGUF file.
pub fn parse_header(bytes: &[u8]) -> Result<GgufHeader, GgufError> {
    if bytes.len() >= 4 && &bytes[..4] != MAGIC {
        return Err(GgufError::NotGguf);
    }
    let mut c = Cursor { bytes, pos: 0 };
    c.take(4)?;
    let version = c.u32()?;
    if !(2..=3).contains(&version) {
        return Err(GgufError::Version(version));
    }
    let tensor_count = c.u64()?;
    let kv_count = c.u64()?;
    if kv_count > 1_000_000 {
        return Err(GgufError::Malformed("too many metadata entries"));
    }
    let mut metadata = BTreeMap::new();
    for _ in 0..kv_count {
        let key = c.string()?;
        let ty = c.u32()?;
        let value = c.value(ty, true)?;
        metadata.insert(key, value);
    }
    Ok(GgufHeader {
        version,
        tensor_count,
        metadata,
        header_len: c.pos,
    })
}

/// Reads the header of a local file, reading only as much as the metadata needs.
pub fn read_header(path: &Path) -> Result<GgufHeader, GgufError> {
    let mut file = std::fs::File::open(path).map_err(|e| GgufError::Io(e.to_string()))?;
    let mut buf = Vec::new();
    let mut want = 1 << 20;
    loop {
        let start = buf.len();
        buf.resize(want, 0);
        let mut filled = start;
        while filled < want {
            match file
                .read(&mut buf[filled..])
                .map_err(|e| GgufError::Io(e.to_string()))?
            {
                0 => break,
                n => filled += n,
            }
        }
        let eof = filled < want;
        buf.truncate(filled);
        match parse_header(&buf) {
            Err(GgufError::Truncated { .. }) if !eof && want < MAX_HEADER_BYTES => {
                want = (want * 4).min(MAX_HEADER_BYTES);
            }
            other => return other,
        }
    }
}

/// Builds GGUF headers for tests (no tensors).
#[cfg(test)]
pub mod fixture {
    pub enum V<'a> {
        U32(u32),
        Str(&'a str),
        Strs(Vec<&'a str>),
        I32s(Vec<i32>),
    }

    pub fn header(kv: &[(&str, V<'_>)]) -> Vec<u8> {
        header_with_tensors(0, kv)
    }

    pub fn header_with_tensors(tensors: u64, kv: &[(&str, V<'_>)]) -> Vec<u8> {
        let mut b = b"GGUF".to_vec();
        b.extend(3u32.to_le_bytes());
        b.extend(tensors.to_le_bytes());
        b.extend((kv.len() as u64).to_le_bytes());
        let s = |b: &mut Vec<u8>, s: &str| {
            b.extend((s.len() as u64).to_le_bytes());
            b.extend(s.as_bytes());
        };
        for (k, v) in kv {
            s(&mut b, k);
            match v {
                V::U32(x) => {
                    b.extend(4u32.to_le_bytes());
                    b.extend(x.to_le_bytes());
                }
                V::Str(x) => {
                    b.extend(8u32.to_le_bytes());
                    s(&mut b, x);
                }
                V::Strs(xs) => {
                    b.extend(9u32.to_le_bytes());
                    b.extend(8u32.to_le_bytes());
                    b.extend((xs.len() as u64).to_le_bytes());
                    for x in xs {
                        s(&mut b, x);
                    }
                }
                V::I32s(xs) => {
                    b.extend(9u32.to_le_bytes());
                    b.extend(5u32.to_le_bytes());
                    b.extend((xs.len() as u64).to_le_bytes());
                    for x in xs {
                        b.extend(x.to_le_bytes());
                    }
                }
            }
        }
        b
    }

    /// A small model header in the shape llama.cpp writes for `arch`: 28 layers with nine tensors
    /// each, plus embeddings and output.
    pub fn model(arch: &str) -> Vec<u8> {
        model_with_tensors(arch, 28 * 9 + 3)
    }

    pub fn model_with_tensors(arch: &str, tensors: u64) -> Vec<u8> {
        let k = |s: &str| format!("{arch}.{s}");
        let (bc, cl, hc, hkv, el) = (
            k("block_count"),
            k("context_length"),
            k("attention.head_count"),
            k("attention.head_count_kv"),
            k("embedding_length"),
        );
        let vocab: Vec<&str> = std::iter::repeat_n("tok", 50_000).collect();
        header_with_tensors(
            tensors,
            &[
                ("general.architecture", V::Str(arch)),
                ("general.name", V::Str("Fixture")),
                (&bc, V::U32(28)),
                (&cl, V::U32(32768)),
                (&hc, V::U32(12)),
                (&hkv, V::U32(2)),
                (&el, V::U32(1536)),
                ("tokenizer.ggml.tokens", V::Strs(vocab)),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{V, header, model};
    use super::*;

    #[test]
    fn reads_architecture_and_geometry() {
        let bytes = model("qwen2");
        let h = parse_header(&bytes).expect("header");
        let f = h.facts();
        assert_eq!(f.architecture.as_deref(), Some("qwen2"));
        assert_eq!(f.block_count, Some(28));
        assert_eq!(f.context_length, Some(32768));
        // Qwen2.5-Coder 1.5B geometry: 28 layers × 2 KV heads × 128 × 2 (K, V) × 2 bytes.
        assert_eq!(f.kv_bytes_per_token(), Some(28_672));
        assert_eq!(h.header_len, bytes.len());
    }

    #[test]
    fn a_prefix_reports_truncation_so_more_can_be_fetched() {
        let bytes = model("qwen2");
        for cut in [10, 100, bytes.len() / 2, bytes.len() - 1] {
            assert!(matches!(
                parse_header(&bytes[..cut]),
                Err(GgufError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn rejects_other_formats_and_versions() {
        assert_eq!(parse_header(b"PK\x03\x04rest"), Err(GgufError::NotGguf));
        let mut v9 = model("qwen2");
        v9[4..8].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(parse_header(&v9), Err(GgufError::Version(9)));
    }

    #[test]
    fn hostile_lengths_do_not_allocate_or_loop() {
        let mut b = b"GGUF".to_vec();
        b.extend(3u32.to_le_bytes());
        b.extend(0u64.to_le_bytes());
        b.extend(1u64.to_le_bytes());
        b.extend(u64::MAX.to_le_bytes()); // key length
        assert!(matches!(parse_header(&b), Err(GgufError::Malformed(_))));
    }

    #[test]
    fn split_and_per_layer_values() {
        let bytes = header(&[
            ("general.architecture", V::Str("qwen3next")),
            ("qwen3next.attention.head_count_kv", V::I32s(vec![0, 0, 0, 2])),
            ("split.count", V::U32(4)),
            ("split.no", V::U32(0)),
        ]);
        let f = parse_header(&bytes).expect("header").facts();
        assert_eq!(f.head_count_kv, Some(2));
        assert_eq!(f.split_count, Some(4));
        assert_eq!(f.split_index, Some(0));
    }

    #[test]
    fn reads_local_files_incrementally() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("m.gguf");
        let mut bytes = model("mistral3");
        bytes.extend(vec![0u8; 3 << 20]); // tensor data after the header
        std::fs::write(&path, &bytes).expect("write");
        let h = read_header(&path).expect("header");
        assert_eq!(h.facts().architecture.as_deref(), Some("mistral3"));
    }
}
