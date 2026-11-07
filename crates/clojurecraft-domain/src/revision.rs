use serde::{Deserialize, Serialize};

use crate::ids::RevisionId;

/// Why a revision was recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionReason {
    pub kind: RevisionKind,
    /// Optional user-visible note.
    pub note: String,
}

impl RevisionReason {
    pub fn new(kind: RevisionKind) -> Self {
        Self {
            kind,
            note: String::new(),
        }
    }

    pub fn with_note(kind: RevisionKind, note: impl Into<String>) -> Self {
        Self {
            kind,
            note: note.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RevisionKind {
    /// Saved explicitly by the user.
    Checkpoint,
    /// Code produced by generation (including any repairs).
    Generated,
    /// Code edited by hand.
    CodeEdited,
    /// Edge-case tests were accepted.
    TestsAccepted,
    /// The implementation was accepted for edge-case testing after passing its checks.
    Accepted,
    /// A benchmarked implementation was chosen.
    ImplementationChosen,
    /// The draft as it was immediately before an older revision was restored.
    BeforeRestore,
    /// The content of an older revision, restored as the newest revision.
    Restored { from: RevisionId, from_number: u32 },
}
