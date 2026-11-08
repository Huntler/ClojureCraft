//! Carrying out review actions: manual code edits, restoring from history, accepting code for
//! edge-case testing, reviewing cases, and fixing failing cases.

use clojurecraft_domain::{RevisionId, RevisionKind};
use clojurecraft_store::Revision;
use clojurecraft_workflow::edge_cases::{self, CaseEdit};

use super::*;
use crate::review::{self, CaseOp, CodeEditor, EditOutcome, ReviewAction};

impl ClojureCraftApp {
    /// Once per frame: closes the code editor when its function was closed or its code was
    /// replaced by something else, so a stale editor never overwrites newer code.
    pub(super) fn review_frame(&mut self) {
        let Some(editor) = &self.review.code else {
            return;
        };
        match self.docs.get(&editor.function) {
            None => self.review.code = None,
            Some(open) if editor.changed_elsewhere(&open.doc) => {
                self.review.code = None;
                self.toast(
                    "The code was replaced while you were editing it, so the editor was closed.".into(),
                    None,
                );
            }
            Some(_) => {}
        }
    }

    pub(super) fn revision_loaded(&mut self, revision: Revision) {
        let id = revision.summary.id;
        self.review.loaded(revision);
        if let Some((function, waiting, code_only)) = self.review.pending_restore
            && waiting == id
        {
            self.review.pending_restore = None;
            self.restore(function, id, code_only);
        }
    }

    fn request_revision(&mut self, id: RevisionId) {
        if self.review.request(id) {
            self.library.send(Command::LoadRevision(id));
        }
    }

    pub(super) fn review_action(&mut self, id: FunctionId, action: ReviewAction) {
        match action {
            ReviewAction::EditCode => self.edit_code(id),
            ReviewAction::CodeChanged(text) => {
                let (Some(editor), Some(open)) = (
                    self.review.code.as_mut().filter(|e| e.function == id),
                    self.docs.get_mut(&id),
                ) else {
                    return;
                };
                editor.text = text;
                if editor.apply(&mut open.doc) {
                    self.submit(id, false);
                }
            }
            ReviewAction::FinishEditing => self.finish_editing(id),
            ReviewAction::CancelEditing => {
                let Some(editor) = self.review.code.take_if(|e| e.function == id) else {
                    return;
                };
                if let Some(open) = self.docs.get_mut(&id)
                    && editor.cancel(&mut open.doc) == EditOutcome::Save
                {
                    self.submit(id, false);
                }
            }
            ReviewAction::RevertToAccepted => {
                self.review.code.take_if(|e| e.function == id);
                if let Some(open) = self.docs.get_mut(&id)
                    && let Some(reason) = review::revert_to_accepted(&mut open.doc)
                {
                    self.commit(id, reason);
                }
            }
            ReviewAction::Compare(Some(revision)) => {
                self.review.compare.insert(id, revision);
                self.request_revision(revision);
            }
            ReviewAction::Compare(None) => {
                self.review.compare.remove(&id);
            }
            ReviewAction::Restore(revision) => self.restore(id, revision, false),
            ReviewAction::RestoreCode(revision) => self.restore(id, revision, true),
            ReviewAction::TestEdgeCases => self.test_edge_cases(id),
            ReviewAction::ProposeCases => self.start_job(id, JobKind::ProposeTests),
            ReviewAction::GenerateCases => self.generate_cases(id),
            ReviewAction::FixFailing => self.fix_failing(id),
            ReviewAction::RunChecks => self.run_checks(id),
            ReviewAction::CancelJob => self.cancel_job(),
            ReviewAction::DismissReport => self.assistant.dismiss_report(id),
            ReviewAction::AddCase => {
                let Some(open) = self.docs.get_mut(&id) else {
                    return;
                };
                let case = edge_cases::add(&mut open.doc);
                if let Some(t) = open.doc.test(case) {
                    self.review.cases.insert(case, CaseEdit::of(t));
                }
                self.submit(id, false);
            }
            ReviewAction::Case(case, op) => self.case_op(id, case, op),
            ReviewAction::ShowImplementation => self.right_tab = Tab::Implementation,
        }
    }

    fn edit_code(&mut self, id: FunctionId) {
        if let Some(other) = self.review.code.as_ref().map(|e| e.function)
            && other != id
        {
            self.finish_editing(other);
        }
        if self.review.editor(id).is_some() {
            return;
        }
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        let (editor, created) = CodeEditor::start(&mut open.doc);
        self.review.code = Some(editor);
        if created {
            self.submit(id, false);
        }
    }

    fn finish_editing(&mut self, id: FunctionId) {
        let Some(editor) = self.review.code.take_if(|e| e.function == id) else {
            return;
        };
        let Some(open) = self.docs.get(&id) else {
            return;
        };
        match editor.finish(&open.doc) {
            EditOutcome::Commit(reason) => self.commit(id, reason),
            EditOutcome::Save => self.submit(id, false),
            EditOutcome::Unchanged => {}
        }
    }

    /// Restores a revision (or only its code) once it has been read. The current draft is
    /// recorded first, so nothing is lost.
    fn restore(&mut self, function: FunctionId, revision: RevisionId, code_only: bool) {
        let Some(target) = self.review.loaded.get(&revision).cloned() else {
            self.review.pending_restore = Some((function, revision, code_only));
            self.request_revision(revision);
            return;
        };
        if target.summary.function != function {
            return;
        }
        // Whatever was typed is already in the draft and is recorded below.
        self.review.code.take_if(|e| e.function == function);
        if code_only {
            let Some(open) = self.docs.get_mut(&function) else {
                return;
            };
            if let Some(reason) = review::restore_code(&mut open.doc, &target) {
                self.commit(function, reason);
                self.toast(
                    format!("Using the code of revision {}.", target.summary.number),
                    None,
                );
            }
            return;
        }
        let (before, restored) = review::restore_steps(&target);
        self.commit(function, before);
        let Some(open) = self.docs.get_mut(&function) else {
            return;
        };
        open.doc = target.document.clone();
        if open
            .view
            .selected_example
            .is_none_or(|e| open.doc.example(e).is_none())
        {
            open.view.selected_example = open.doc.examples.first().map(|e| e.id);
        }
        self.review.cases.retain(|case, _| open.doc.test(*case).is_some());
        self.commit(function, restored);
        self.toast(
            format!(
                "Restored revision {}. The previous draft is kept in the history.",
                target.summary.number
            ),
            None,
        );
    }

    /// Records the current code as accepted (when it passes its checks), then proposes edge
    /// cases.
    fn test_edge_cases(&mut self, id: FunctionId) {
        if let Some(why) = self.assist_blocked(id) {
            self.toast(why, None);
            return;
        }
        self.finish_editing(id);
        let reports = self.validations.get(&id).cloned().unwrap_or_default();
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        let report = match open.doc.passing_report(&reports) {
            Ok(r) => r.clone(),
            Err(blocker) => {
                self.toast(blocker.describe().to_owned(), None);
                return;
            }
        };
        let basis = open.doc.basis();
        let already =
            open.doc.is_accepted_code() && open.doc.accepted.as_ref().is_some_and(|a| a.basis == basis);
        if !already {
            if let Err(blocker) = open.doc.accept_implementation(&report) {
                self.toast(blocker.describe().to_owned(), None);
                return;
            }
            self.commit(
                id,
                RevisionReason::with_note(RevisionKind::Accepted, "Accepted for edge-case testing"),
            );
        }
        self.start_job(id, JobKind::ProposeTests);
    }

    /// Repairs the current code with all accepted cases; the result waits for review.
    fn fix_failing(&mut self, id: FunctionId) {
        if let Err(e) = self.try_fix_failing(id) {
            self.toast(e, None);
        }
    }

    pub(super) fn try_fix_failing(&mut self, id: FunctionId) -> Result<(), String> {
        if let Some(why) = self.assist_blocked(id) {
            return Err(why);
        }
        self.finish_editing(id);
        let reports = self.validations.get(&id).cloned().unwrap_or_default();
        let Some(doc) = self.docs.get(&id).map(|o| o.doc.clone()) else {
            return Err("The function is not open.".into());
        };
        let report = edge_cases::failing_report(&doc, &reports)?.clone();
        let model = self.model_context()?;
        let documentation = self.knowledge.active().is_some();
        let mut ports = AppPorts {
            inference: &mut self.inference,
            knowledge: &mut self.knowledge,
            execution: &mut self.execution,
        };
        self.assistant
            .fix_failing(&doc, model, documentation, &report, &mut ports);
        Ok(())
    }

    fn case_op(&mut self, id: FunctionId, case: clojurecraft_domain::TestCaseId, op: CaseOp) {
        self.review.case_errors.remove(&case);
        match op {
            CaseOp::Edit => {
                if let Some(t) = self.docs.get(&id).and_then(|o| o.doc.test(case)) {
                    self.review.cases.insert(case, CaseEdit::of(t));
                }
                return;
            }
            CaseOp::CancelEdit => {
                self.review.cases.remove(&case);
                return;
            }
            _ => {}
        }
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        let doc = &mut open.doc;
        let name = doc
            .test(case)
            .map(|t| t.name.trim().to_owned())
            .unwrap_or_default();
        let result: Result<Option<RevisionReason>, String> = match op {
            CaseOp::Accept => edge_cases::accept(doc, case)
                .map(|()| {
                    Some(RevisionReason::with_note(
                        RevisionKind::TestsAccepted,
                        format!("Accepted edge case “{name}”"),
                    ))
                })
                .map_err(|e| format!("This case cannot be accepted: {e}.")),
            CaseOp::Reject => edge_cases::reject(doc, case)
                .map(|()| None)
                .map_err(|e| e.to_string()),
            CaseOp::Reopen => edge_cases::reopen(doc, case)
                .map(|()| None)
                .map_err(|e| e.to_string()),
            CaseOp::Remove => {
                edge_cases::remove(doc, case);
                Ok(None)
            }
            CaseOp::Save => match self.review.cases.get(&case).cloned() {
                Some(edit) => edge_cases::save(doc, case, edit)
                    .map(|()| {
                        doc.test(case).filter(|t| t.is_active()).map(|_| {
                            RevisionReason::with_note(
                                RevisionKind::TestsAccepted,
                                format!("Edited edge case “{name}”"),
                            )
                        })
                    })
                    .map_err(|e| format!("Not saved: {e}.")),
                None => Ok(None),
            },
            CaseOp::Ask => edge_cases::ask_on_specification(doc, case)
                .map(|_| None)
                .map_err(|e| e.to_string()),
            CaseOp::Edit | CaseOp::CancelEdit => Ok(None),
        };
        match result {
            Ok(reason) => {
                if op == CaseOp::Save {
                    self.review.cases.remove(&case);
                }
                if op == CaseOp::Ask {
                    self.toast(
                        "The question was added to the specification. Answer it there; the answer \
                         becomes a requirement."
                            .into(),
                        None,
                    );
                }
                match reason {
                    Some(reason) => self.commit(id, reason),
                    None => self.submit(id, false),
                }
            }
            Err(e) => {
                self.review.case_errors.insert(case, e);
            }
        }
    }
}
