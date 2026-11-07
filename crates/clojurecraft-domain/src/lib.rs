//! Domain records for ClojureCraft functions.
//!
//! A function has one editable draft ([`FunctionDocument`]) and a linear history of immutable
//! revisions that freeze a document. Results (validation reports, benchmarks) are stored
//! separately and carry the fingerprints of the content they were computed from, so changing
//! relevant content marks them stale without discarding them.

pub mod fingerprint;
pub mod format;
pub mod function;
pub mod ids;
pub mod reference;
pub mod results;
pub mod review;
pub mod revision;
pub mod secrets;
pub mod time;

pub use fingerprint::{Basis, Component, Fingerprint, Freshness};
pub use function::*;
pub use ids::*;
pub use results::*;
pub use review::{AcceptBlocker, AcceptedCode};
pub use revision::{RevisionKind, RevisionReason};
pub use secrets::SecretRegistry;
pub use time::Timestamp;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("test case {0} has an open question and cannot be accepted until its expected result is defined")]
    AmbiguousTest(TestCaseId),
}

#[cfg(test)]
mod tests;
