//! Review state of open functions, kept free of UI code so it can be tested: revision history and
//! comparison, manual editing of the implementation, the accepted version, and edge-case editors.
//!
//! Every change to a function made here goes through the same draft and revision path as other
//! edits: the caller saves the draft or records a revision with the reason returned.

use std::collections::{HashMap, HashSet};

use clojurecraft_domain::{
    FunctionDocument, FunctionId, Implementation, RevisionId, RevisionKind, RevisionReason, TestCaseId,
};
use clojurecraft_store::{Revision, RevisionSummary};
use clojurecraft_workflow::Names;
use clojurecraft_workflow::edge_cases::CaseEdit;

/// What the user asked for in the review views.
#[derive(Clone, Debug, PartialEq)]
pub enum ReviewAction {
    /// Opens the code editor (creating a hand-written implementation if there is none).
    EditCode,
    /// The editor's text changed.
    CodeChanged(String),
    /// Closes the editor, recording a revision if the code changed.
    FinishEditing,
    /// Closes the editor and puts back the code it started with.
    CancelEditing,
    /// Puts the accepted code back as the implementation.
    RevertToAccepted,
    /// Shows a revision for comparison (loading it if needed); `None` closes the comparison.
    Compare(Option<RevisionId>),
    /// Restores everything from a revision; the current draft stays in history.
    Restore(RevisionId),
    /// Uses only the code of a revision.
    RestoreCode(RevisionId),
    /// Accepts the current code and proposes edge cases.
    TestEdgeCases,
    /// Proposes more edge cases for already accepted code.
    ProposeCases,
    /// Generates edge cases from the title, description, and examples only.
    GenerateCases,
    FixFailing,
    RunChecks,
    CancelJob,
    DismissReport,
    AddCase,
    Case(TestCaseId, CaseOp),
    /// Switches the right column to the implementation, where held code is reviewed.
    ShowImplementation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaseOp {
    Accept,
    Reject,
    Reopen,
    Remove,
    /// Opens the case's editor.
    Edit,
    Save,
    CancelEdit,
    /// Adds the case's open question to the specification.
    Ask,
}

/// The implementation editor of one function.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeEditor {
    pub function: FunctionId,
    pub text: String,
    /// The code when editing started, restored by cancelling.
    pub original: Option<Implementation>,
    /// The text last written to the function, to notice changes made elsewhere.
    written: String,
}

/// What closing the editor should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    /// Nothing changed.
    Unchanged,
    /// Save the draft.
    Save,
    /// Save the draft and record it as a revision with this reason.
    Commit(RevisionReason),
}

/// A skeleton for code written by hand: the namespace and function validation expects.
pub fn hand_written_template(doc: &FunctionDocument) -> String {
    let names = Names::for_document(doc, |_| false);
    format!(
        "(ns {})\n\n(defn {} [input]\n  input)\n",
        names.namespace, names.function
    )
}

impl CodeEditor {
    /// Starts editing `doc`'s implementation, creating a hand-written one if there is none.
    /// Returns whether the document changed.
    pub fn start(doc: &mut FunctionDocument) -> (Self, bool) {
        let original = doc.implementation.clone();
        let created = original.is_none();
        if created {
            doc.implementation = Some(Implementation::manual(hand_written_template(doc)));
        }
        let text = doc
            .implementation
            .as_ref()
            .map(|i| i.source.clone())
            .unwrap_or_default();
        (
            Self {
                function: doc.id,
                written: text.clone(),
                text,
                original,
            },
            created,
        )
    }

    /// Writes the editor's text into the implementation. Generation provenance is kept, so the
    /// code shows as generated and then edited by hand, and its checks become outdated.
    pub fn apply(&mut self, doc: &mut FunctionDocument) -> bool {
        let Some(i) = &mut doc.implementation else {
            doc.implementation = Some(Implementation::manual(self.text.clone()));
            self.written.clone_from(&self.text);
            return true;
        };
        if i.source == self.text {
            return false;
        }
        i.source.clone_from(&self.text);
        self.written.clone_from(&self.text);
        true
    }

    /// Whether the implementation was changed by something other than this editor (for example
    /// restored from history or replaced by reviewed code) while it was open.
    pub fn changed_elsewhere(&self, doc: &FunctionDocument) -> bool {
        doc.implementation.as_ref().map(|i| i.source.as_str()) != Some(self.written.as_str())
    }

    /// Closes the editor: a revision records the edit when the code differs from where it started.
    pub fn finish(self, doc: &FunctionDocument) -> EditOutcome {
        let before = self.original.as_ref().map(|i| i.source.as_str());
        let now = doc.implementation.as_ref().map(|i| i.source.as_str());
        if before == now {
            EditOutcome::Unchanged
        } else {
            EditOutcome::Commit(RevisionReason::with_note(
                RevisionKind::CodeEdited,
                "Code edited by hand",
            ))
        }
    }

    /// Closes the editor and puts back the implementation it started with.
    pub fn cancel(self, doc: &mut FunctionDocument) -> EditOutcome {
        if doc.implementation == self.original {
            return EditOutcome::Unchanged;
        }
        doc.implementation = self.original;
        EditOutcome::Save
    }
}

/// Puts the accepted code back as the implementation. The generation record is kept when the
/// accepted code is what was generated.
pub fn revert_to_accepted(doc: &mut FunctionDocument) -> Option<RevisionReason> {
    let accepted = doc.accepted.as_ref()?.source.clone();
    if !doc.differs_from_accepted() {
        return None;
    }
    match &mut doc.implementation {
        Some(i) => i.source = accepted,
        None => doc.implementation = Some(Implementation::manual(accepted)),
    }
    Some(RevisionReason::with_note(
        RevisionKind::CodeEdited,
        "Accepted code restored",
    ))
}

/// The revisions that restore `target` without losing the current draft: first the current
/// content (recorded only if it differs from the latest revision), then the restored content.
pub fn restore_steps(target: &Revision) -> (RevisionReason, RevisionReason) {
    (
        RevisionReason::new(RevisionKind::BeforeRestore),
        RevisionReason::new(RevisionKind::Restored {
            from: target.summary.id,
            from_number: target.summary.number,
        }),
    )
}

/// Uses the implementation of `revision` while keeping the current specification and tests.
pub fn restore_code(doc: &mut FunctionDocument, revision: &Revision) -> Option<RevisionReason> {
    if doc.implementation == revision.document.implementation {
        return None;
    }
    doc.implementation.clone_from(&revision.document.implementation);
    Some(RevisionReason::with_note(
        RevisionKind::CodeEdited,
        format!("Code restored from revision {}", revision.summary.number),
    ))
}

/// The newest revision recording an accepted implementation (`revisions` are newest first).
pub fn latest_accepted(revisions: &[RevisionSummary]) -> Option<&RevisionSummary> {
    revisions.iter().find(|r| r.reason.kind == RevisionKind::Accepted)
}

pub fn kind_label(kind: &RevisionKind) -> String {
    match kind {
        RevisionKind::Checkpoint => "Saved".into(),
        RevisionKind::Generated => "Generated".into(),
        RevisionKind::CodeEdited => "Code edited".into(),
        RevisionKind::TestsAccepted => "Edge cases accepted".into(),
        RevisionKind::Accepted => "Accepted for edge-case testing".into(),
        RevisionKind::ImplementationChosen => "Implementation chosen".into(),
        RevisionKind::BeforeRestore => "Before a restore".into(),
        RevisionKind::Restored { from_number, .. } => format!("Restored from revision {from_number}"),
    }
}

/// Review state of every open function.
#[derive(Default)]
pub struct ReviewState {
    /// Revision history per function, newest first.
    pub revisions: HashMap<FunctionId, Vec<RevisionSummary>>,
    /// Revisions read for comparison or restoring.
    pub loaded: HashMap<RevisionId, Revision>,
    requested: HashSet<RevisionId>,
    /// The revision being compared with the current draft, per function.
    pub compare: HashMap<FunctionId, RevisionId>,
    /// A restore waiting for the revision to be read.
    pub pending_restore: Option<(FunctionId, RevisionId, bool)>,
    pub code: Option<CodeEditor>,
    /// Open case editors.
    pub cases: HashMap<TestCaseId, CaseEdit>,
    /// Why the last operation on a case failed.
    pub case_errors: HashMap<TestCaseId, String>,
}

impl ReviewState {
    /// Marks `id` as requested; returns false when it is already loaded or requested.
    pub fn request(&mut self, id: RevisionId) -> bool {
        !self.loaded.contains_key(&id) && self.requested.insert(id)
    }

    pub fn loaded(&mut self, revision: Revision) {
        self.requested.remove(&revision.summary.id);
        self.loaded.insert(revision.summary.id, revision);
    }

    /// The editor of `function`, if it is open.
    pub fn editor(&self, function: FunctionId) -> Option<&CodeEditor> {
        self.code.as_ref().filter(|e| e.function == function)
    }
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{
        Basis, Fingerprint, GenerationRecord, GenerationSettings, InferenceRuntime, ModelIdentity, Timestamp,
    };

    use super::*;

    fn generated(source: &str) -> Implementation {
        Implementation {
            source: source.into(),
            generation: Some(GenerationRecord {
                generated_source: source.into(),
                model: ModelIdentity {
                    display_name: "m".into(),
                    repository: "r".into(),
                    revision: "c".into(),
                    file: "f".into(),
                    sha256: "00".into(),
                    quantization: "Q4".into(),
                    architecture: "qwen2".into(),
                },
                runtime: InferenceRuntime {
                    llama_cpp_release: "b1".into(),
                    backend: "cpu".into(),
                },
                settings: GenerationSettings {
                    temperature: 0.2,
                    top_p: 0.9,
                    seed: None,
                    context_length: 4096,
                    max_output_tokens: 100,
                },
                documentation: vec![],
                spec_basis: Fingerprint::from_stored("sha256:0"),
                attempts: 1,
                created_at: Timestamp(0),
                namespace: String::new(),
                function_name: String::new(),
                requires: vec![],
                assumptions: vec![],
                omitted_context: vec![],
                prompt: Vec::new(),
                explanation: None,
            }),
        }
    }

    #[test]
    fn manual_edits_change_the_code_keep_provenance_and_are_recorded_once() {
        let mut doc = FunctionDocument::new("Totals");
        doc.implementation = Some(generated("(ns totals.core)\n(defn totals [x] x)\n"));
        let checked = doc.basis();
        let (mut editor, created) = CodeEditor::start(&mut doc);
        assert!(!created);
        assert_eq!(editor.finish(&doc), EditOutcome::Unchanged);

        let (mut editor2, _) = CodeEditor::start(&mut doc);
        editor2.text.push_str(";; note\n");
        assert!(editor2.apply(&mut doc));
        assert!(!editor2.apply(&mut doc), "nothing new to write");
        let i = doc.implementation.as_ref().unwrap();
        assert!(i.is_manually_edited() && i.generation.is_some());
        // Results of the earlier code are now outdated.
        assert_ne!(doc.basis().code, checked.code);
        assert_eq!(doc.basis().spec, checked.spec);
        assert!(matches!(
            editor2.finish(&doc),
            EditOutcome::Commit(r) if r.kind == RevisionKind::CodeEdited
        ));
        editor = CodeEditor::start(&mut doc).0;
        editor.text = "(ns x)".into();
        editor.apply(&mut doc);
        assert_eq!(editor.cancel(&mut doc), EditOutcome::Save);
        assert!(doc.implementation.as_ref().unwrap().source.ends_with(";; note\n"));
    }

    #[test]
    fn writing_code_by_hand_starts_from_a_template_and_cancelling_removes_it() {
        let mut doc = FunctionDocument::new("Order totals");
        let (editor, created) = CodeEditor::start(&mut doc);
        assert!(created);
        assert!(editor.text.starts_with("(ns order-totals.core)"));
        assert!(editor.text.contains("(defn order-totals [input]"));
        assert!(doc.implementation.as_ref().unwrap().is_manually_edited());
        assert_eq!(editor.cancel(&mut doc), EditOutcome::Save);
        assert!(doc.implementation.is_none());
    }

    #[test]
    fn changes_made_elsewhere_while_editing_are_noticed() {
        let mut doc = FunctionDocument::new("f");
        doc.implementation = Some(Implementation::manual("(ns a)"));
        let (mut editor, _) = CodeEditor::start(&mut doc);
        assert!(!editor.changed_elsewhere(&doc));
        editor.text = "(ns b)".into();
        editor.apply(&mut doc);
        assert!(!editor.changed_elsewhere(&doc));
        doc.implementation = Some(generated("(ns c)"));
        assert!(editor.changed_elsewhere(&doc));
    }

    #[test]
    fn the_accepted_code_can_be_put_back_after_an_edit() {
        let mut doc = FunctionDocument::new("f");
        doc.implementation = Some(generated("(ns a)\n(defn f [x] x)\n"));
        doc.accepted = Some(clojurecraft_domain::AcceptedCode {
            source: "(ns a)\n(defn f [x] x)\n".into(),
            basis: doc.basis(),
            report: clojurecraft_domain::ValidationId::generate(),
            accepted_at: Timestamp(0),
        });
        assert_eq!(revert_to_accepted(&mut doc), None, "nothing to put back");
        if let Some(i) = &mut doc.implementation {
            i.source = "(ns a)\n(defn f [x] nil)\n".into();
        }
        let reason = revert_to_accepted(&mut doc).unwrap();
        assert_eq!(reason.kind, RevisionKind::CodeEdited);
        assert!(doc.is_accepted_code());
        // The accepted code is what was generated, so it no longer counts as edited by hand.
        assert!(!doc.implementation.as_ref().unwrap().is_manually_edited());
    }

    fn summary(number: u32, kind: RevisionKind) -> RevisionSummary {
        RevisionSummary {
            id: RevisionId::generate(),
            function: FunctionId::generate(),
            number,
            parent: None,
            created_at: Timestamp(0),
            reason: RevisionReason::new(kind),
            basis: Basis {
                spec: Fingerprint::from_stored("a"),
                code: Fingerprint::from_stored("b"),
                tests: Fingerprint::from_stored("c"),
            },
            content: Fingerprint::from_stored("d"),
        }
    }

    #[test]
    fn restoring_records_the_draft_first_and_names_the_source() {
        let history = vec![
            summary(3, RevisionKind::CodeEdited),
            summary(2, RevisionKind::Accepted),
            summary(1, RevisionKind::Accepted),
        ];
        assert_eq!(latest_accepted(&history).map(|r| r.number), Some(2));
        let target = Revision {
            summary: history[1].clone(),
            document: FunctionDocument::new("f"),
        };
        let (before, restored) = restore_steps(&target);
        assert_eq!(before.kind, RevisionKind::BeforeRestore);
        assert_eq!(
            restored.kind,
            RevisionKind::Restored {
                from: history[1].id,
                from_number: 2
            }
        );

        let mut doc = target.document.clone();
        doc.implementation = Some(Implementation::manual("(ns now)"));
        let mut old = target.clone();
        old.document.implementation = Some(Implementation::manual("(ns then)"));
        doc.description = "kept".into();
        let reason = restore_code(&mut doc, &old).unwrap();
        assert!(reason.note.contains("revision 2"));
        assert_eq!(doc.implementation.as_ref().unwrap().source, "(ns then)");
        assert_eq!(doc.description, "kept", "only the code is taken");
        assert_eq!(restore_code(&mut doc, &old), None);
    }
}
