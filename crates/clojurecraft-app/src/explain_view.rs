//! The Explain tab: asks the model to explain the current generated code (which code carries out
//! each step, the decisions it makes where the specification is silent) and shows the result.

use clojurecraft_domain::FunctionDocument;
use egui::{RichText, Ui};

use crate::generation_view::{self, PanelAction};
use crate::theme;
use clojurecraft_workflow::assistant::{self, Assistant, JobKind, Report};

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

/// Whether `report` is about an explanation, so it belongs on this tab.
pub fn is_explanation_report(report: &Report) -> bool {
    match report {
        Report::Explained { .. } => true,
        Report::Failed { what, .. } => *what == JobKind::Explain.label(),
        _ => false,
    }
}

/// `blocked` explains why the model cannot be used right now.
pub fn show(
    ui: &mut Ui,
    doc: &FunctionDocument,
    assistant: &Assistant,
    blocked: Option<&str>,
) -> Option<PanelAction> {
    let mut action = None;
    let job = assistant.busy_with(doc.id).filter(|j| j.kind == JobKind::Explain);
    let generation = doc
        .implementation
        .as_ref()
        .filter(|i| !i.is_manually_edited())
        .and_then(|i| i.generation.as_ref());
    let explanation = generation.and_then(|g| g.explanation.as_ref());
    ui.add_space(6.0);
    if job.is_none() {
        let why = assistant::explain_blocker(doc).or(blocked);
        let label = if explanation.is_some() {
            "Explain again"
        } else {
            "Generate explanation"
        };
        let r = ui
            .add_enabled(why.is_none(), egui::Button::new(label))
            .on_hover_text(
                "The local model reads the specification and the generated code, then names the code \
                 that carries out each step and the decisions the code makes where the specification \
                 is silent.",
            );
        let r = match why {
            Some(why) => r.on_disabled_hover_text(why),
            None => r,
        };
        if r.clicked() {
            action = Some(PanelAction::Explain);
        }
        if let Some(why) = why {
            muted(ui, why);
        }
    }
    ui.add_space(4.0);
    match job {
        Some(job) => generation_view::running(ui, doc, job, &mut action),
        None => {
            if let Some(r) = assistant.report(doc.id).filter(|r| is_explanation_report(r)) {
                generation_view::report(ui, doc, r, &mut action);
            }
        }
    }
    ui.add_space(8.0);
    let Some(g) = generation else {
        return action;
    };
    if let Some(e) = explanation
        && !e.summary.is_empty()
    {
        ui.label(RichText::new("How it works").strong());
        ui.label(&e.summary);
        ui.add_space(8.0);
    }
    if explanation.is_some() {
        ui.label(RichText::new("Steps").strong());
        generation_view::step_mapping(ui, doc);
        ui.add_space(8.0);
    }
    let assumptions = match explanation {
        Some(e) => &e.assumptions,
        None => &g.assumptions,
    };
    if !assumptions.is_empty() {
        ui.label(RichText::new("Assumptions").strong());
        for a in assumptions {
            ui.label(format!("• {a}"));
        }
        muted(
            ui,
            "Answer them as questions or add them to the specification if they matter.",
        );
        ui.add_space(8.0);
    }
    if let Some(e) = explanation
        && !e.symbols.is_empty()
    {
        ui.label(RichText::new("Standard-library functions used").strong());
        for s in &e.symbols {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&s.symbol).monospace().color(theme::syntax::KEYWORD));
                ui.label(&s.purpose);
            });
        }
    }
    action
}
