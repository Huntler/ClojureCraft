//! Versioned serialisation of persisted records.
//!
//! Every stored record is written as JSON together with a format number. Reading upgrades older
//! formats step by step in memory; immutable records (revisions, reports) are never rewritten.
//! Records written by a newer application version are refused instead of being misread.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// The kinds of independently versioned records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Document,
    RevisionReason,
    ValidationReport,
    BenchmarkResult,
}

impl RecordKind {
    /// The format this application writes.
    pub const fn current(self) -> u32 {
        match self {
            // 2: clarifications, interpretation, a whole-value type in contract shapes, and the
            // generated code's namespace, requirements, assumptions, and explanation.
            Self::Document => 2,
            // 2: the generation run, the checked source, and the comparison settings.
            Self::ValidationReport => 2,
            Self::RevisionReason | Self::BenchmarkResult => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Document => "function document",
            Self::RevisionReason => "revision reason",
            Self::ValidationReport => "validation report",
            Self::BenchmarkResult => "benchmark result",
        }
    }
}

#[derive(Debug, Error)]
pub enum FormatError {
    #[error(
        "this {kind} was written by a newer version of ClojureCraft (format {found}, supported up to {supported})"
    )]
    TooNew {
        kind: &'static str,
        found: u32,
        supported: u32,
    },
    #[error("unknown {kind} format {found}")]
    Unknown { kind: &'static str, found: u32 },
    #[error("unreadable {kind}: {source}")]
    Malformed {
        kind: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

pub fn encode<T: Serialize>(record: &T) -> serde_json::Result<String> {
    serde_json::to_string(record)
}

pub fn decode<T: DeserializeOwned>(kind: RecordKind, format: u32, json: &str) -> Result<T, FormatError> {
    let malformed = |source| FormatError::Malformed {
        kind: kind.label(),
        source,
    };
    let supported = kind.current();
    if format > supported {
        return Err(FormatError::TooNew {
            kind: kind.label(),
            found: format,
            supported,
        });
    }
    if format == 0 {
        return Err(FormatError::Unknown {
            kind: kind.label(),
            found: format,
        });
    }
    let mut value: serde_json::Value = serde_json::from_str(json).map_err(malformed)?;
    for from in format..supported {
        value = upgrade(kind, from, value)?;
    }
    serde_json::from_value(value).map_err(malformed)
}

/// Converts a record from format `from` to `from + 1`. Each new format adds one arm here.
fn upgrade(kind: RecordKind, from: u32, value: serde_json::Value) -> Result<serde_json::Value, FormatError> {
    match (kind, from) {
        // Format 2 of documents and validation reports only adds fields, all of which default to
        // empty when absent.
        (RecordKind::Document, 1) => Ok(value),
        (RecordKind::ValidationReport, 1) => Ok(value),
        _ => Err(FormatError::Unknown {
            kind: kind.label(),
            found: from,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionDocument;

    #[test]
    fn newer_formats_are_refused() {
        let doc = FunctionDocument::new("f");
        let json = encode(&doc).unwrap();
        let back: FunctionDocument = decode(RecordKind::Document, 2, &json).unwrap();
        assert_eq!(back, doc);
        let err = decode::<FunctionDocument>(RecordKind::Document, 3, &json).unwrap_err();
        assert!(matches!(err, FormatError::TooNew { found: 3, .. }), "{err}");
        assert!(decode::<FunctionDocument>(RecordKind::Document, 2, "{").is_err());
    }

    /// A document as format 1 wrote it: no clarifications, interpretation, shape types, or
    /// generation details.
    const FORMAT_1_DOCUMENT: &str = r#"{
        "id": "6f1d2c1e-9a55-4c55-9a0e-6f3c8a0c2b11",
        "name": "totals",
        "description": "Sum amounts",
        "contract": {"status": "accepted",
                     "input": {"description": "orders", "keys": []},
                     "output": {"description": "total", "keys": []},
                     "ordering": {"kind": "unspecified"},
                     "allowed_dependencies": [], "notes": ""},
        "steps": [], "examples": [],
        "implementation": {"source": "(ns a)", "generation": {
            "generated_source": "(ns a)",
            "model": {"display_name": "m", "repository": "r", "revision": "c", "file": "f.gguf",
                      "sha256": "00", "quantization": "Q4_K_M", "architecture": "qwen2"},
            "runtime": {"llama_cpp_release": "b1", "backend": "cpu"},
            "settings": {"temperature": 0.2, "top_p": 0.9, "seed": null, "context_length": 8192,
                         "max_output_tokens": 100},
            "documentation": [], "spec_basis": "sha256:00", "attempts": 1, "created_at": 0}},
        "tests": [], "benchmarks": []
    }"#;

    #[test]
    fn format_1_documents_are_upgraded_with_empty_new_fields() {
        let doc: FunctionDocument = decode(RecordKind::Document, 1, FORMAT_1_DOCUMENT).unwrap();
        assert!(doc.clarifications.is_empty());
        assert!(doc.interpretation.is_none());
        let contract = doc.contract.as_ref().unwrap();
        assert!(contract.input.value_type.is_any());
        let generation = doc.implementation.as_ref().unwrap().generation.as_ref().unwrap();
        assert!(generation.explanation.is_none() && generation.requires.is_empty());
        // The upgrade adds nothing that changes what the specification means.
        let reencoded: serde_json::Value = serde_json::from_str(&encode(contract).unwrap()).unwrap();
        assert!(reencoded["input"].get("value_type").is_none());
    }

    #[test]
    fn format_1_validation_reports_are_upgraded_without_run_details() {
        let json = r#"{
            "id": "6f1d2c1e-9a55-4c55-9a0e-6f3c8a0c2b12",
            "created_at": 5,
            "basis": {"spec": "sha256:1", "code": "sha256:2", "tests": "sha256:3"},
            "environment": {"clojure_version": "1.12.6", "java_version": "25", "platform": "linux-x86_64"},
            "checks": [{"subject": {"kind": "parse"}, "status": "passed", "expected": null,
                        "actual": null, "diagnostics": [], "duration_ms": null}],
            "attempt": 2
        }"#;
        let r: crate::ValidationReport = decode(RecordKind::ValidationReport, 1, json).unwrap();
        assert_eq!(r.attempt, Some(2));
        assert!(r.run.is_none() && r.source.is_none() && r.settings.is_none());
        assert!(r.passed());
        let err = decode::<crate::ValidationReport>(RecordKind::ValidationReport, 3, json).unwrap_err();
        assert!(matches!(err, FormatError::TooNew { found: 3, .. }));
    }
}
