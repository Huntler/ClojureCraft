//! The right column: the current implementation and its result, step, and source views.

use clojurecraft_domain::{FunctionDocument, ValidationReport};
use clojurecraft_knowledge::Query;
use egui::{RichText, Ui};

use crate::edge_cases_view::{self, EdgeCaseContext};
use crate::execution::InstallState;
use crate::generation_view::{self, PanelAction};
use crate::inference::Missing;
use crate::knowledge::{KnowledgeState, Slot};
use crate::knowledge_view;
use crate::review::{ReviewAction, ReviewState};
use crate::review_view;
use crate::theme;
use crate::validation_view;
use clojurecraft_workflow::assistant::Assistant;

/// Longest query built from a specification; retrieval needs the gist, not every word.
const SPEC_QUERY_CHARS: usize = 1200;

pub enum ColumnAction {
    None,
    OpenSetup,
    OpenKnowledge,
    Generation(PanelAction),
    Review(ReviewAction),
    Steps(crate::step_preview_view::StepAction),
}

/// What the column shows besides the function itself.
pub struct ColumnContext<'a> {
    pub missing: &'a [Missing],
    pub knowledge: &'a mut KnowledgeState,
    pub assistant: &'a Assistant,
    /// Why generation cannot start right now.
    pub blocked: Option<String>,
    pub validation: ValidationContext<'a>,
    pub review: &'a mut ReviewState,
    pub steps: crate::step_preview_view::StepsContext<'a>,
}

/// Validation results and whether checks can run.
pub struct ValidationContext<'a> {
    /// Stored reports of the open function, newest first.
    pub reports: &'a [ValidationReport],
    /// A check of the current code is running.
    pub running: bool,
    /// Why code cannot be run, if it cannot.
    pub unavailable: Option<String>,
    /// Installing the Java runtime and Clojure would make checks possible.
    pub installable: bool,
    pub install: &'a InstallState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Implementation,
    Result,
    EdgeCases,
    Steps,
    Sources,
    History,
}

impl Tab {
    const ALL: [Self; 6] = [
        Self::Implementation,
        Self::Result,
        Self::EdgeCases,
        Self::Steps,
        Self::Sources,
        Self::History,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Implementation => "Implementation",
            Self::Result => "Result",
            Self::EdgeCases => "Edge cases",
            Self::History => "History",
            Self::Steps => "Steps",
            Self::Sources => "Sources",
        }
    }
}

/// Shows the column; returns what the user asked to open.
pub fn show(
    ui: &mut Ui,
    tab: &mut Tab,
    doc: Option<&FunctionDocument>,
    cx: ColumnContext<'_>,
) -> ColumnAction {
    let ColumnContext {
        missing,
        knowledge,
        assistant,
        blocked,
        validation,
        review,
        steps,
    } = cx;
    let mut action = ColumnAction::None;
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().button_padding.x = 6.0;
        ui.spacing_mut().item_spacing.x = 2.0;
        for t in Tab::ALL {
            if ui.selectable_label(*tab == t, t.label()).clicked() {
                *tab = t;
            }
        }
    });
    ui.separator();
    if !missing.is_empty() {
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, theme::WARN))
            .corner_radius(theme::RADIUS)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Code generation is not available yet").color(theme::WARN));
                for m in missing {
                    ui.label(RichText::new(m.describe()).small().color(theme::MUTED));
                }
                ui.label(
                    RichText::new("Specifications can be written and saved in the meantime.")
                        .small()
                        .color(theme::MUTED),
                );
                if ui.button("Open setup").clicked() {
                    action = ColumnAction::OpenSetup;
                }
            });
        ui.add_space(6.0);
    }
    let Some(doc) = doc else {
        empty(ui, "Open or create a function to see its implementation.");
        return action;
    };
    egui::ScrollArea::vertical()
        .id_salt("right-column")
        .auto_shrink([false, false])
        .show(ui, |ui| match tab {
            Tab::Implementation => {
                let mut review_action = None;
                if let Some(a) = generation_view::panel(
                    ui,
                    doc,
                    assistant,
                    blocked.as_deref(),
                    &validation,
                    review,
                    &mut review_action,
                ) {
                    action = ColumnAction::Generation(a);
                }
                if let Some(a) = review_action {
                    action = ColumnAction::Review(a);
                }
            }
            Tab::EdgeCases => {
                let cx = EdgeCaseContext {
                    assistant,
                    blocked: blocked.as_deref(),
                    reports: validation.reports,
                    checking: validation.running,
                    unavailable: validation.unavailable.as_deref(),
                };
                if let Some(a) = edge_cases_view::show(ui, doc, review, &cx) {
                    action = ColumnAction::Review(a);
                }
            }
            Tab::History => {
                let mut review_action = None;
                review_view::history(ui, doc, review, &mut review_action);
                if let Some(a) = review_action {
                    action = ColumnAction::Review(a);
                }
            }
            Tab::Result => {
                if let Some(a) = result(ui, doc, &validation) {
                    action = ColumnAction::Generation(a);
                }
            }
            Tab::Steps => {
                if let Some(a) = crate::step_preview_view::show(ui, doc, &steps) {
                    action = ColumnAction::Steps(a);
                }
            }
            Tab::Sources => {
                if sources(ui, doc, knowledge) {
                    action = ColumnAction::OpenKnowledge;
                }
            }
        });
    action
}

fn empty(ui: &mut Ui, text: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(text).color(theme::MUTED));
}

/// The newest report about exactly the current code, if any.
pub fn current_report<'a>(
    doc: &FunctionDocument,
    reports: &'a [ValidationReport],
) -> Option<&'a ValidationReport> {
    let code = doc.code_fingerprint();
    reports.iter().find(|r| r.basis.code == code)
}

/// Runs and shows validation of the current implementation.
fn result(ui: &mut Ui, doc: &FunctionDocument, v: &ValidationContext<'_>) -> Option<PanelAction> {
    let mut action = None;
    ui.add_space(6.0);
    if doc.implementation.is_none() {
        empty(
            ui,
            "Nothing to check yet: generate or write an implementation first.",
        );
    } else {
        runtime_controls(ui, doc, v, &mut action);
        ui.add_space(6.0);
        match current_report(doc, v.reports) {
            Some(r) => {
                let freshness = r.freshness(&doc.basis());
                ui.horizontal_wrapped(|ui| {
                    let color = match (r.passed(), freshness.is_current()) {
                        (true, true) => theme::ACCENT_INK,
                        (_, false) => theme::WARN,
                        (false, true) => theme::BAD,
                    };
                    ui.label(RichText::new(validation_view::summary(r)).color(color).strong());
                    ui.label(
                        RichText::new(validation_view::ago(r.created_at))
                            .small()
                            .color(theme::MUTED),
                    );
                });
                validation_view::freshness_banner(ui, &freshness);
                ui.add_space(4.0);
                validation_view::checks(ui, doc, r);
            }
            None => empty(ui, "This implementation has not been checked yet."),
        }
    }
    let earlier: Vec<&ValidationReport> = {
        let shown = current_report(doc, v.reports).map(|r| r.id);
        v.reports
            .iter()
            .filter(|r| Some(r.id) != shown)
            .take(12)
            .collect()
    };
    if !earlier.is_empty() {
        ui.add_space(10.0);
        ui.collapsing(format!("Earlier checks ({})", earlier.len()), |ui| {
            for r in earlier {
                let origin = match r.attempt {
                    Some(n) if r.run.is_some() => format!("generation attempt {n}"),
                    _ => "check".to_owned(),
                };
                let stale = if r.freshness(&doc.basis()).is_current() {
                    ""
                } else {
                    " · outdated"
                };
                egui::CollapsingHeader::new(format!(
                    "{} · {origin} · {}{stale}",
                    validation_view::ago(r.created_at),
                    validation_view::summary(r)
                ))
                .id_salt(("earlier", r.id))
                .show(ui, |ui| {
                    validation_view::freshness_banner(ui, &r.freshness(&doc.basis()));
                    validation_view::checks(ui, doc, r);
                    if let Some(code) = &r.source {
                        ui.collapsing("Checked code", |ui| {
                            egui::Frame::new()
                                .fill(theme::SUNKEN)
                                .stroke(theme::hairline())
                                .corner_radius(theme::RADIUS)
                                .inner_margin(6.0)
                                .show(ui, |ui| {
                                    crate::edn_view::code_view(
                                        ui,
                                        egui::Id::unique(("checked-code", r.id)),
                                        code,
                                    );
                                });
                        });
                    }
                });
            }
        });
    }
    action
}

/// "Run checks", or why it is unavailable with the install action.
pub fn runtime_controls(
    ui: &mut Ui,
    doc: &FunctionDocument,
    v: &ValidationContext<'_>,
    action: &mut Option<PanelAction>,
) {
    if let InstallState::Running { file, progress } = v.install {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            let pct = (progress.received * 100)
                .checked_div(progress.total)
                .map_or_else(String::new, |p| format!(" {p} %"));
            ui.label(format!("Installing the Java runtime and Clojure: {file}{pct}"));
            if ui.small_button("Cancel").clicked() {
                *action = Some(PanelAction::CancelInstall);
            }
        });
        return;
    }
    if let Some(why) = &v.unavailable {
        ui.label(RichText::new(why).small().color(theme::WARN));
        if let InstallState::Failed(e) = v.install {
            ui.label(
                RichText::new(format!("The installation failed: {e}"))
                    .small()
                    .color(theme::BAD),
            );
        }
        if v.installable
            && ui
                .button("Install the Java runtime and Clojure")
                .on_hover_text("Downloads the pinned, verified runtimes used to run and check code.")
                .clicked()
        {
            *action = Some(PanelAction::InstallRuntime);
        }
        return;
    }
    ui.horizontal_wrapped(|ui| {
        if v.running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
            ui.spinner();
            ui.label("Checking…");
            if ui.small_button("Cancel").clicked() {
                *action = Some(PanelAction::CancelChecks);
            }
        } else if ui
            .add_enabled(doc.implementation.is_some(), egui::Button::new("Run checks"))
            .on_hover_text(
                "Reads, lints, and loads the code in the sandbox, then runs every example and accepted \
                 test and checks the accepted contract.",
            )
            .clicked()
        {
            *action = Some(PanelAction::RunChecks);
        }
    });
}

/// The text searched for a function's related documentation: description, then steps.
fn spec_query(doc: &FunctionDocument) -> String {
    let mut text = doc.description.trim().to_owned();
    for step in &doc.steps {
        let t = step.text.trim();
        if !t.is_empty() {
            text.push('\n');
            text.push_str(t);
        }
    }
    let mut end = text.len().min(SPEC_QUERY_CHARS);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// Documentation used by the implementation, and a search for documentation related to the
/// specification. Returns true when the user asked to open the knowledge page.
fn sources(ui: &mut Ui, doc: &FunctionDocument, knowledge: &mut KnowledgeState) -> bool {
    let used = doc
        .implementation
        .as_ref()
        .and_then(|i| i.generation.as_ref())
        .map(|g| g.documentation.as_slice())
        .unwrap_or_default();
    ui.add_space(8.0);
    ui.label(RichText::new("Used for this implementation").strong());
    if used.is_empty() {
        ui.label(RichText::new("No documentation was used for this implementation.").color(theme::MUTED));
    }
    for d in used {
        ui.label(RichText::new(&d.title).strong());
        let symbol = match (&d.namespace, &d.symbol) {
            (Some(ns), Some(sym)) => format!("{ns}/{sym} · "),
            (None, Some(sym)) => format!("{sym} · "),
            _ => String::new(),
        };
        ui.label(
            RichText::new(format!(
                "{symbol}Clojure {} · collection {} · {}",
                d.clojure_version, d.collection_revision, d.source_url
            ))
            .small()
            .color(theme::MUTED),
        );
        ui.add_space(6.0);
    }
    ui.add_space(12.0);
    ui.label(RichText::new("Related documentation").strong());
    let Some(active) = knowledge.active().map(|a| a.info.clojure_version.clone()) else {
        ui.label(
            RichText::new("Install the Clojure documentation to find related functions and guides.")
                .color(theme::MUTED),
        );
        return ui.button("Open knowledge sources").clicked();
    };
    let text = spec_query(doc);
    let slot = Slot::Function(doc.id);
    let enabled = !text.trim().is_empty();
    let r = ui
        .add_enabled(enabled, egui::Button::new("Search for this specification"))
        .on_disabled_hover_text("Write a description or steps first.");
    if r.clicked() {
        let mut q = Query::new(text);
        q.clojure_version = Some(active);
        q.limit = 8;
        knowledge.search(slot, q);
    }
    let s = knowledge.search_state(slot);
    knowledge_view::results(ui, s, "related");
    false
}
