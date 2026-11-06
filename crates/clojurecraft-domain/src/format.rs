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
            Self::Document | Self::RevisionReason | Self::ValidationReport | Self::BenchmarkResult => 1,
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
fn upgrade(kind: RecordKind, from: u32, _value: serde_json::Value) -> Result<serde_json::Value, FormatError> {
    Err(FormatError::Unknown {
        kind: kind.label(),
        found: from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionDocument;

    #[test]
    fn newer_formats_are_refused() {
        let doc = FunctionDocument::new("f");
        let json = encode(&doc).unwrap();
        let back: FunctionDocument = decode(RecordKind::Document, 1, &json).unwrap();
        assert_eq!(back, doc);
        let err = decode::<FunctionDocument>(RecordKind::Document, 2, &json).unwrap_err();
        assert!(matches!(err, FormatError::TooNew { found: 2, .. }), "{err}");
        assert!(decode::<FunctionDocument>(RecordKind::Document, 1, "{").is_err());
    }
}
