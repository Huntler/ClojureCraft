//! Edge-case testing: accepting the current code for deeper testing, proposed cases grouped by
//! whether they can run, need a decision, or await acceptance, and fixing code that fails them.

use clojurecraft_domain::{
    CheckStatus, CheckSubject, Expectation, FunctionDocument, TestCase, TestOrigin, ValidationReport,
};
use clojurecraft_workflow::assistant::{Assistant, JobKind, Report};
use clojurecraft_workflow::edge_cases::{self, CaseEdit, CaseState};
use egui::{Id, RichText, Ui};

use crate::edn_view;
use crate::review::{CaseOp, ReviewAction, ReviewState};
use crate::theme;
use crate::validation_view;

pub struct EdgeCaseContext<'a> {
    pub assistant: &'a Assistant,
    /// Why no model work can start now.
    pub blocked: Option<&'a str>,
    /// Stored reports of the function, newest first.
    pub reports: &'a [ValidationReport],
    /// A check of the current code is running.
    pub checking: bool,
    /// Why code cannot be run, if it cannot.
    pub unavailable: Option<&'a str>,
}

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

fn boxed(ui: &mut Ui, stroke: egui::Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(theme::RADIUS)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn value(ui: &mut Ui, id: Id, label: &str, text: &str) {
    ui.label(RichText::new(label).small().color(theme::MUTED));
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(4.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt(id.with("scroll"))
                .max_height(120.0)
                .show(ui, |ui| edn_view::code_view(ui, id, text));
        });
}

fn edn_input(ui: &mut Ui, id: Id, label: &str, text: &mut String) {
    ui.label(RichText::new(label).small().color(theme::MUTED));
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let job = edn_view::highlight_job(buf.as_str(), font.clone(), &[], wrap);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    ui.add(
        egui::TextEdit::multiline(text)
            .id(id)
            .code_editor()
            .lock_focus(false)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .layouter(&mut layouter),
    );
    if !text.trim().is_empty()
        && let Err(e) = edge_cases::single_value(text)
    {
        ui.label(
            RichText::new(format!("Not a valid EDN value: {e}"))
                .small()
                .color(theme::BAD),
        );
    }
}

/// Shows the tab; returns what the user asked for.
pub fn show(
    ui: &mut Ui,
    doc: &FunctionDocument,
    review: &mut ReviewState,
    cx: &EdgeCaseContext<'_>,
) -> Option<ReviewAction> {
    let mut action = None;
    ui.add_space(6.0);
    acceptance(ui, doc, cx, &mut action);
    ui.add_space(8.0);
    checks(ui, doc, cx, &mut action);
    ui.add_space(8.0);
    cases(ui, doc, review, cx, &mut action);
    action
}

fn acceptance(
    ui: &mut Ui,
    doc: &FunctionDocument,
    cx: &EdgeCaseContext<'_>,
    action: &mut Option<ReviewAction>,
) {
    let job = cx.assistant.busy_with(doc.id);
    if let Some(job) = job.filter(|j| j.kind == JobKind::ProposeTests) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            ui.label(format!("{}…", job.phase.label()));
            ui.label(
                RichText::new(format!("{} s", job.started.elapsed().as_secs()))
                    .small()
                    .color(theme::MUTED),
            );
            if ui.button("Cancel").clicked() {
                *action = Some(ReviewAction::CancelJob);
            }
        });
        return;
    }
    match &doc.accepted {
        Some(a) if doc.is_accepted_code() => {
            ui.label(
                RichText::new(format!(
                    "The current code was accepted for edge-case testing {}.",
                    validation_view::ago(a.accepted_at)
                ))
                .color(theme::ACCENT_INK),
            );
        }
        Some(a) => {
            ui.label(
                RichText::new(format!(
                    "The code changed after it was accepted {}. The accepted version is kept; see \
                     Implementation for the changes, or History to restore it.",
                    validation_view::ago(a.accepted_at)
                ))
                .color(theme::WARN),
            );
        }
        None => muted(
            ui,
            "Once the current code passes its checks, accept it for edge-case testing. The model then \
             proposes cases from the specification and contract; nothing changes the code until you \
             review it.",
        ),
    }
    let passing = doc.passing_report(cx.reports);
    let accepted_now =
        doc.is_accepted_code() && doc.accepted.as_ref().is_some_and(|a| a.basis == doc.basis());
    ui.horizontal_wrapped(|ui| {
        let (label, hover, act) = if accepted_now {
            (
                "Propose more edge cases",
                "Asks the model for further cases from the specification and contract.",
                ReviewAction::ProposeCases,
            )
        } else {
            (
                "Test edge cases",
                "Records the current code as accepted (recoverable from History), then asks the model \
                 to propose edge cases: empty, missing, nil, duplicate, boundary, ordering, and nested \
                 data.",
                ReviewAction::TestEdgeCases,
            )
        };
        let why = match (&passing, cx.blocked) {
            (Err(b), _) => Some(b.describe().to_owned()),
            (Ok(_), Some(why)) => Some(why.to_owned()),
            (Ok(_), None) => None,
        };
        let r = ui
            .add_enabled(why.is_none(), egui::Button::new(label))
            .on_hover_text(hover);
        let r = match &why {
            Some(w) => r.on_disabled_hover_text(w),
            None => r,
        };
        if r.clicked() {
            *action = Some(act);
        }
        let why = edge_cases::examples_blocker(doc).or(cx.blocked);
        let r = ui
            .add_enabled(why.is_none(), egui::Button::new("Generate"))
            .on_hover_text(
                "Asks the model for edge cases based only on the title, description, and examples. \
                 Works without code; the cases are proposals until you accept them.",
            );
        let r = match why {
            Some(w) => r.on_disabled_hover_text(w),
            None => r,
        };
        if r.clicked() {
            *action = Some(ReviewAction::GenerateCases);
        }
        if ui
            .button("Add a case")
            .on_hover_text("Write a case yourself.")
            .clicked()
        {
            *action = Some(ReviewAction::AddCase);
        }
    });
    if let Err(b) = &passing
        && doc.implementation.is_some()
    {
        muted(ui, b.describe());
    }
    if let Some(Report::TestsProposed {
        added,
        ambiguous,
        notes,
    }) = cx.assistant.report(doc.id)
    {
        boxed(ui, theme::LINE, |ui| {
            ui.horizontal_wrapped(|ui| {
                let text = match (added, ambiguous) {
                    (0, _) => "No new cases were proposed.".to_owned(),
                    (n, 0) => format!("{n} cases proposed for your review."),
                    (n, k) => format!("{n} cases proposed for your review; {k} need your decision."),
                };
                ui.label(RichText::new(text).color(theme::ACCENT_INK));
                if ui.small_button("Dismiss").clicked() {
                    *action = Some(ReviewAction::DismissReport);
                }
            });
            for n in notes {
                muted(ui, n.clone());
            }
        });
    }
    if let Some(Report::Blocked(why)) = cx.assistant.report(doc.id) {
        for w in why {
            ui.label(RichText::new(format!("• {w}")).small().color(theme::WARN));
        }
    }
    if let Some(Report::Failed { what, message, .. }) = cx.assistant.report(doc.id)
        && *what == "propose edge cases"
    {
        boxed(ui, theme::BAD, |ui| {
            ui.label(RichText::new(format!("Could not {what}: {message}.")).color(theme::BAD));
            if ui.small_button("Dismiss").clicked() {
                *action = Some(ReviewAction::DismissReport);
            }
        });
    }
}

/// The current check of the accepted cases, and fixing failures.
fn checks(ui: &mut Ui, doc: &FunctionDocument, cx: &EdgeCaseContext<'_>, action: &mut Option<ReviewAction>) {
    let (regression, awaiting, unclear) = edge_cases::counts(doc);
    if regression + awaiting + unclear == 0 {
        return;
    }
    ui.label(RichText::new("Checks").strong());
    let basis = doc.basis();
    let current = cx.reports.iter().find(|r| r.basis == basis);
    let (text, color) = match current {
        _ if cx.checking => ("Checks are running…".to_owned(), theme::MUTED),
        Some(r) if r.passed() => (validation_view::summary(r), theme::ACCENT_INK),
        Some(r) => (validation_view::summary(r), theme::BAD),
        None => (
            "The current code has not been checked with the accepted cases.".to_owned(),
            theme::MUTED,
        ),
    };
    ui.label(RichText::new(text).small().color(color));
    ui.horizontal_wrapped(|ui| {
        if !cx.checking {
            let r = ui
                .add_enabled(
                    cx.unavailable.is_none() && doc.implementation.is_some(),
                    egui::Button::new("Run checks"),
                )
                .on_hover_text("Runs every example and accepted case against the current code.");
            let r = match cx.unavailable {
                Some(why) => r.on_disabled_hover_text(why),
                None => r,
            };
            if r.clicked() {
                *action = Some(ReviewAction::RunChecks);
            }
        }
        let fixable = edge_cases::failing_report(doc, cx.reports);
        let why = match (&fixable, cx.blocked) {
            (Err(e), _) => Some((*e).to_owned()),
            (Ok(_), Some(b)) => Some(b.to_owned()),
            (Ok(_), None) => None,
        };
        let r = ui
            .add_enabled(why.is_none(), egui::Button::new("Fix failing cases"))
            .on_hover_text(
                "The model repairs the current code with every accepted case and example (one attempt \
                 plus up to three repairs). The result is shown as a change for your review; the \
                 accepted version is never replaced without it.",
            );
        let r = match &why {
            Some(w) => r.on_disabled_hover_text(w),
            None => r,
        };
        if r.clicked() {
            *action = Some(ReviewAction::FixFailing);
        }
    });
    let fixing = cx
        .assistant
        .busy_with(doc.id)
        .is_some_and(|j| j.kind == JobKind::Generate && j.fixing());
    let held = matches!(
        cx.assistant.report(doc.id),
        Some(Report::Pending(p)) if p.fix
    ) || matches!(
        cx.assistant.report(doc.id),
        Some(Report::Unresolved { best: Some(p), .. }) if p.fix
    );
    if fixing || held {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(if fixing {
                    "A fix is being written and checked."
                } else {
                    "A proposed fix is waiting for your review."
                })
                .small()
                .color(theme::WARN),
            );
            if ui.small_button("Show").clicked() {
                *action = Some(ReviewAction::ShowImplementation);
            }
        });
    }
}

fn cases(
    ui: &mut Ui,
    doc: &FunctionDocument,
    review: &mut ReviewState,
    cx: &EdgeCaseContext<'_>,
    action: &mut Option<ReviewAction>,
) {
    let basis = doc.basis();
    let report = cx.reports.iter().find(|r| r.basis == basis);
    let groups = [
        (
            CaseState::NeedsClarification,
            "Needs your decision",
            "The specification does not determine these results. Decide each one (edit it and give \
             the expected output) or ask it as a question on the specification. They are not \
             requirements and are never run.",
        ),
        (
            CaseState::AwaitingAcceptance,
            "Proposed expectations",
            "Expected outputs proposed from the specification, not taken from the code. They are not \
             run until you accept them.",
        ),
        (
            CaseState::Regression,
            "Accepted cases",
            "Regression tests: run by every check, generation, and repair.",
        ),
    ];
    for (state, title, help) in groups {
        let list: Vec<&TestCase> = doc
            .tests
            .iter()
            .filter(|t| edge_cases::state(t) == state)
            .collect();
        if list.is_empty() {
            continue;
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("{title} ({})", list.len())).strong());
        muted(ui, help);
        for t in list {
            ui.add_space(4.0);
            case(ui, t, report, review, action);
        }
    }
    let rejected: Vec<&TestCase> = doc
        .tests
        .iter()
        .filter(|t| edge_cases::state(t) == CaseState::Rejected)
        .collect();
    if !rejected.is_empty() {
        ui.add_space(6.0);
        egui::CollapsingHeader::new(format!("Rejected ({})", rejected.len()))
            .id_salt(("rejected-cases", doc.id))
            .show(ui, |ui| {
                for t in rejected {
                    case(ui, t, report, review, action);
                }
            });
    }
}

fn origin_badge(ui: &mut Ui, t: &TestCase) {
    let (text, color) = match (edge_cases::is_unaccepted_proposal(t), t.origin) {
        (true, TestOrigin::Model) => ("Proposed by the model", theme::WARN),
        (true, _) => ("Proposed by the model, edited by you", theme::WARN),
        (false, TestOrigin::User) => ("Written by you", theme::MUTED),
        (false, TestOrigin::Model) => ("Proposed by the model, accepted by you", theme::MUTED),
        (false, TestOrigin::ModelEdited) => {
            ("Proposed by the model, edited and accepted by you", theme::MUTED)
        }
    };
    ui.label(RichText::new(text).small().color(color));
}

fn case(
    ui: &mut Ui,
    t: &TestCase,
    report: Option<&ValidationReport>,
    review: &mut ReviewState,
    action: &mut Option<ReviewAction>,
) {
    let state = edge_cases::state(t);
    let stroke = match state {
        CaseState::NeedsClarification => theme::WARN,
        CaseState::AwaitingAcceptance => theme::LINE_STRONG,
        CaseState::Regression | CaseState::Rejected => theme::LINE,
    };
    boxed(ui, stroke, |ui| {
        ui.horizontal_wrapped(|ui| {
            let name = if t.name.trim().is_empty() {
                "(unnamed case)"
            } else {
                t.name.trim()
            };
            ui.label(RichText::new(name).strong());
            origin_badge(ui, t);
            if state == CaseState::Regression
                && let Some(c) =
                    report.and_then(|r| r.checks.iter().find(|c| c.subject == CheckSubject::Test(t.id)))
            {
                let (s, color) = validation_view::status_text(c.status);
                ui.label(RichText::new(s).small().color(color));
            }
        });
        if !t.notes.trim().is_empty() {
            muted(ui, t.notes.trim());
        }
        if let Some(edit) = review.cases.get_mut(&t.id) {
            editor(ui, t, edit, action);
        } else {
            value(ui, Id::unique(("case-input", t.id)), "Input", t.input.as_str());
            match &t.expectation {
                Expectation::Value { expected } => {
                    let label = if state == CaseState::Regression {
                        "Expected output"
                    } else {
                        "Proposed expected output (not yet a requirement)"
                    };
                    value(ui, Id::unique(("case-expected", t.id)), label, expected.as_str());
                }
                Expectation::NeedsClarification { question } => {
                    ui.label(RichText::new(format!("Open question: {question}")).color(theme::WARN));
                }
            }
            if state == CaseState::Regression
                && let Some(c) =
                    report.and_then(|r| r.checks.iter().find(|c| c.subject == CheckSubject::Test(t.id)))
                && !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped)
            {
                if let Some(actual) = &c.actual {
                    value(
                        ui,
                        Id::unique(("case-actual", t.id)),
                        "Actual output of the current code",
                        actual.as_str(),
                    );
                }
                for d in c.diagnostics.iter().take(3) {
                    ui.label(
                        RichText::new(edn_view::abbreviate(d, 300))
                            .small()
                            .color(theme::BAD),
                    );
                }
            }
            buttons(ui, t, state, action);
        }
        if let Some(e) = review.case_errors.get(&t.id) {
            ui.label(RichText::new(e).small().color(theme::BAD));
        }
    });
}

fn buttons(ui: &mut Ui, t: &TestCase, state: CaseState, action: &mut Option<ReviewAction>) {
    let op = |a: &mut Option<ReviewAction>, op| *a = Some(ReviewAction::Case(t.id, op));
    ui.horizontal_wrapped(|ui| {
        match state {
            CaseState::AwaitingAcceptance => {
                if ui
                    .button("Accept")
                    .on_hover_text("Makes this case a regression test that every check runs.")
                    .clicked()
                {
                    op(action, CaseOp::Accept);
                }
            }
            CaseState::NeedsClarification => {
                if ui
                    .button("Ask on the specification")
                    .on_hover_text(
                        "Adds the question to the specification's questions. Your answer becomes part \
                         of the specification.",
                    )
                    .clicked()
                {
                    op(action, CaseOp::Ask);
                }
            }
            CaseState::Regression | CaseState::Rejected => {}
        }
        if state != CaseState::Rejected && ui.button("Edit").clicked() {
            op(action, CaseOp::Edit);
        }
        if state == CaseState::Rejected {
            if ui.button("Reconsider").clicked() {
                op(action, CaseOp::Reopen);
            }
            if ui.button("Delete").clicked() {
                op(action, CaseOp::Remove);
            }
        } else if ui
            .button("Reject")
            .on_hover_text("Keeps the case listed but never runs it; it is not proposed again.")
            .clicked()
        {
            op(action, CaseOp::Reject);
        }
    });
}

fn editor(ui: &mut Ui, t: &TestCase, edit: &mut CaseEdit, action: &mut Option<ReviewAction>) {
    ui.label(RichText::new("Name").small().color(theme::MUTED));
    ui.add(egui::TextEdit::singleline(&mut edit.name).desired_width(f32::INFINITY));
    edn_input(
        ui,
        Id::unique(("edit-input", t.id)),
        "Input (EDN)",
        &mut edit.input,
    );
    let mut defined = edit.expected.is_some();
    ui.horizontal_wrapped(|ui| {
        ui.radio_value(&mut defined, true, "Expected output");
        ui.radio_value(&mut defined, false, "Undecided (open question)");
    });
    match (defined, &mut edit.expected) {
        (true, None) => edit.expected = Some(String::new()),
        (false, Some(_)) => edit.expected = None,
        _ => {}
    }
    match &mut edit.expected {
        Some(expected) => edn_input(
            ui,
            Id::unique(("edit-expected", t.id)),
            "Expected output (EDN)",
            expected,
        ),
        None => {
            ui.label(RichText::new("Question").small().color(theme::MUTED));
            ui.add(
                egui::TextEdit::multiline(&mut edit.question)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
        }
    }
    ui.horizontal_wrapped(|ui| {
        if ui.button("Save").clicked() {
            *action = Some(ReviewAction::Case(t.id, CaseOp::Save));
        }
        if ui.button("Cancel").clicked() {
            *action = Some(ReviewAction::Case(t.id, CaseOp::CancelEdit));
        }
    });
}
