//! In-memory editing state for open functions, kept free of UI code so it can be tested.
//!
//! The editor's copy of a document is always the newest version: every edit bumps the document's
//! edit sequence and is handed to the autosaver, and the store ignores writes older than what it
//! holds. Reopening a function during a session therefore uses this copy rather than reloading a
//! possibly older draft from disk.

use clojurecraft_domain::{ExampleId, FunctionDocument};
use clojurecraft_store::{AutosaveEvent, Draft, FunctionSummary};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveStatus {
    /// Everything up to the latest edit is on disk.
    Saved,
    /// Edits are waiting for the autosave quiet period or being written.
    Pending,
    /// The last write failed and will be retried automatically.
    Retrying(String),
    /// The last write failed and will not be retried without user action.
    Failed(String),
    /// Another writer stored a newer version of this function.
    Conflict,
}

impl SaveStatus {
    /// Whether some edits are known not to be on disk and may be lost.
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Retrying(_) | Self::Failed(_) | Self::Conflict)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SaveFailure {
    seq: u64,
    message: String,
    will_retry: bool,
    conflict: bool,
}

/// Per-document view state that is not part of the function itself.
#[derive(Clone, Debug, Default)]
pub struct DocumentView {
    pub selected_example: Option<ExampleId>,
}

pub struct OpenDocument {
    pub doc: FunctionDocument,
    pub view: DocumentView,
    seq: u64,
    saved_seq: u64,
    failure: Option<SaveFailure>,
}

impl OpenDocument {
    pub fn new(draft: Draft) -> Self {
        let view = DocumentView {
            selected_example: draft.document.examples.first().map(|e| e.id),
        };
        Self {
            doc: draft.document,
            view,
            seq: draft.seq,
            saved_seq: draft.seq,
            failure: None,
        }
    }

    /// Records that the document changed and returns the edit sequence to save it under.
    pub fn edited(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Applies an autosave outcome. Outcomes may arrive out of order relative to edits.
    pub fn apply(&mut self, event: &AutosaveEvent) {
        match event {
            AutosaveEvent::Saved { seq, .. } | AutosaveEvent::Committed { seq, .. } => {
                self.saved_seq = self.saved_seq.max(*seq);
                if self
                    .failure
                    .as_ref()
                    .is_some_and(|f| f.seq <= *seq && !f.conflict)
                {
                    self.failure = None;
                }
            }
            AutosaveEvent::Superseded { seq, .. } => {
                // This editor's sequences only grow and the autosaver writes them in order, so a
                // newer stored sequence means another writer (such as a second application
                // window on the same library) changed the function.
                self.failure = Some(SaveFailure {
                    seq: *seq,
                    message: "a newer version of this function was saved elsewhere".into(),
                    will_retry: false,
                    conflict: true,
                });
            }
            // The draft is still saved as an ordinary edit, which reports its own outcome.
            AutosaveEvent::CommitFailed { .. } => {}
            AutosaveEvent::Failed {
                seq,
                error,
                will_retry,
                ..
            } => {
                if *seq > self.saved_seq {
                    self.failure = Some(SaveFailure {
                        seq: *seq,
                        message: error.clone(),
                        will_retry: *will_retry,
                        conflict: false,
                    });
                }
            }
        }
    }

    pub fn status(&self) -> SaveStatus {
        match &self.failure {
            Some(f) if f.conflict => SaveStatus::Conflict,
            Some(f) if f.will_retry => SaveStatus::Retrying(f.message.clone()),
            Some(f) => SaveStatus::Failed(f.message.clone()),
            None if self.saved_seq < self.seq => SaveStatus::Pending,
            None => SaveStatus::Saved,
        }
    }

    /// Prepares a new attempt to save after a permanent failure; returns the sequence to submit.
    pub fn retry(&mut self) -> u64 {
        if self.failure.as_ref().is_some_and(|f| !f.conflict) {
            self.failure = None;
        }
        self.edited()
    }
}

/// The open documents, as the assistant changes them.
pub struct OpenDocs<'a>(pub &'a mut std::collections::HashMap<clojurecraft_domain::FunctionId, OpenDocument>);

impl clojurecraft_workflow::assistant::Documents for OpenDocs<'_> {
    fn document(&mut self, id: clojurecraft_domain::FunctionId) -> Option<&mut FunctionDocument> {
        self.0.get_mut(&id).map(|o| &mut o.doc)
    }
}

/// `base`, or `base 2`, `base 3`, … — the first that no existing name uses (case-insensitively).
pub fn unique_name<'a>(base: &str, taken: impl IntoIterator<Item = &'a str>) -> String {
    let taken: Vec<String> = taken.into_iter().map(|t| t.trim().to_lowercase()).collect();
    let free = |candidate: &str| !taken.iter().any(|t| *t == candidate.to_lowercase());
    if free(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|c| free(c))
        .unwrap_or_else(|| base.to_owned())
}

/// Name for a duplicate of `name`.
pub fn copy_name<'a>(name: &str, taken: impl IntoIterator<Item = &'a str>) -> String {
    let name = name.trim();
    let base = if name.is_empty() {
        "Untitled function copy".to_owned()
    } else {
        format!("{name} copy")
    };
    unique_name(&base, taken)
}

/// Whether a library entry matches a search query: every whitespace-separated term must occur,
/// case-insensitively, in the name or the description.
pub fn matches_search(summary: &FunctionSummary, query: &str) -> bool {
    let haystack = format!("{}\n{}", summary.name, summary.description).to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{FunctionId, Timestamp};

    use super::*;

    fn open(seq: u64) -> OpenDocument {
        OpenDocument::new(Draft {
            document: FunctionDocument::new("f"),
            seq,
            base_revision: None,
            updated_at: Timestamp(0),
            has_unrevisioned_changes: true,
        })
    }

    fn saved(doc: &OpenDocument, seq: u64) -> AutosaveEvent {
        AutosaveEvent::Saved {
            function: doc.doc.id,
            seq,
        }
    }

    fn failed(doc: &OpenDocument, seq: u64, will_retry: bool) -> AutosaveEvent {
        AutosaveEvent::Failed {
            function: doc.doc.id,
            seq,
            error: "disk full".into(),
            will_retry,
        }
    }

    #[test]
    fn edits_continue_from_the_stored_sequence() {
        let mut doc = open(41);
        assert_eq!(doc.status(), SaveStatus::Saved);
        assert_eq!(doc.edited(), 42);
        assert_eq!(doc.status(), SaveStatus::Pending);
    }

    #[test]
    fn saving_an_older_edit_leaves_newer_ones_pending() {
        let mut doc = open(0);
        doc.edited();
        doc.edited();
        doc.apply(&saved(&doc, 1));
        assert_eq!(doc.status(), SaveStatus::Pending);
        doc.apply(&saved(&doc, 2));
        assert_eq!(doc.status(), SaveStatus::Saved);
        // A late notification for an older write changes nothing.
        doc.apply(&saved(&doc, 1));
        assert_eq!(doc.status(), SaveStatus::Saved);
    }

    #[test]
    fn failures_stay_visible_until_a_later_save_succeeds() {
        let mut doc = open(0);
        doc.edited();
        doc.apply(&failed(&doc, 1, true));
        assert_eq!(doc.status(), SaveStatus::Retrying("disk full".into()));
        assert!(doc.status().is_problem());
        doc.apply(&failed(&doc, 1, false));
        assert_eq!(doc.status(), SaveStatus::Failed("disk full".into()));

        let seq = doc.retry();
        assert_eq!(seq, 2);
        assert_eq!(doc.status(), SaveStatus::Pending);
        doc.apply(&saved(&doc, 2));
        assert_eq!(doc.status(), SaveStatus::Saved);
    }

    #[test]
    fn a_failure_of_an_already_saved_edit_is_ignored() {
        let mut doc = open(0);
        doc.edited();
        doc.edited();
        doc.apply(&saved(&doc, 2));
        doc.apply(&failed(&doc, 1, false));
        assert_eq!(doc.status(), SaveStatus::Saved);
    }

    #[test]
    fn a_newer_version_from_another_writer_is_a_conflict() {
        let mut doc = open(5);
        doc.edited();
        doc.apply(&AutosaveEvent::Superseded {
            function: doc.doc.id,
            seq: 6,
            stored_seq: 9,
        });
        assert_eq!(doc.status(), SaveStatus::Conflict);
        // Saving later edits does not hide the conflict.
        doc.edited();
        doc.apply(&saved(&doc, 7));
        assert_eq!(doc.status(), SaveStatus::Conflict);
    }

    #[test]
    fn names_are_made_unique_case_insensitively() {
        let taken = ["Untitled function", "untitled function 2", "Totals"];
        assert_eq!(unique_name("Untitled function", taken), "Untitled function 3");
        assert_eq!(unique_name("Orders", taken), "Orders");
        assert_eq!(copy_name("Totals", taken), "Totals copy");
        assert_eq!(copy_name("Totals", ["Totals", "Totals copy"]), "Totals copy 2");
        assert_eq!(copy_name("  ", []), "Untitled function copy");
    }

    #[test]
    fn search_matches_all_terms_in_name_or_description() {
        let summary = FunctionSummary {
            id: FunctionId::generate(),
            name: "Order totals".into(),
            description: "Sum amounts per Customer".into(),
            created_at: Timestamp(0),
            updated_at: Timestamp(0),
            deleted_at: None,
            revision_count: 0,
        };
        assert!(matches_search(&summary, ""));
        assert!(matches_search(&summary, "ORDER customer"));
        assert!(!matches_search(&summary, "order invoice"));
    }
}
