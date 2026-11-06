//! The right column's generation panel: starting and following a generation with its validation
//! and repair attempts, reviewing held or failing code, and the current implementation with its
//! check status, provenance, and explanation.

use std::time::Duration;

use clojurecraft_domain::{FunctionDocument, GenerationRecord, Implementation};
use clojurecraft_inference::Remedy;
use egui::{Id, RichText, Ui};

use crate::edn_view;
use crate::implementation::{ValidationContext, current_report};
use crate::theme;
use crate::validation_view;
use clojurecraft_workflow::assistant::{Assistant, Job, JobKind, Pending, Report};

/// What the user asked for in the panel.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    Generate,
    Cancel,
    AcceptPending,
    DiscardPending,
    Dismiss,
    Remedy(Remedy),
    /// Validate the current implementation.
    RunChecks,
    CancelChecks,
    InstallRuntime,
    CancelInstall,
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

fn code_box(ui: &mut Ui, id: Id, source: &str) {
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(6.0)
        .show(ui, |ui| edn_view::code_view(ui, id, source));
}

fn notes(ui: &mut Ui, notes: &[String]) {
    for n in notes {
        muted(ui, n.clone());
    }
}

/// A readable preview of a JSON answer still being streamed: escaped line breaks and quotes are
/// shown as such. Only for display; the final answer is parsed properly.
fn preview(raw: &str) -> String {
    let tail_start = raw.len().saturating_sub(3000);
    let mut start = tail_start;
    while !raw.is_char_boundary(start) {
        start += 1;
    }
    raw[start..]
        .replace("\\n", "\n")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn running(ui: &mut Ui, doc: &FunctionDocument, job: &Job, actions: &mut Option<PanelAction>) {
    ui.ctx().request_repaint_after(Duration::from_millis(250));
    boxed(ui, theme::LINE_STRONG, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            match job.attempt() {
                Some((n, max))
                    if n > 1 || job.phase == clojurecraft_workflow::assistant::Phase::Validating =>
                {
                    ui.label(format!("{}… (attempt {n} of {max})", job.phase.label()));
                }
                _ => {
                    ui.label(format!("{}…", job.phase.label()));
                }
            }
            ui.label(
                RichText::new(format!("{} s", job.started.elapsed().as_secs()))
                    .small()
                    .color(theme::MUTED),
            );
            if ui.button("Cancel").clicked() {
                *actions = Some(PanelAction::Cancel);
            }
        });
        if job.output.is_empty() && !job.reasoning.is_empty() {
            muted(
                ui,
                format!(
                    "The model is thinking ({} characters so far).",
                    job.reasoning.len()
                ),
            );
        }
        if !job.attempts().is_empty() {
            validation_view::attempts(ui, doc, job.attempts(), "running");
        }
        if !job.output.is_empty() {
            muted(ui, "Live output, not yet checked:");
            egui::ScrollArea::vertical()
                .id_salt("live-output")
                .max_height(180.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(preview(&job.output))
                            .monospace()
                            .small()
                            .color(theme::MUTED),
                    );
                });
        }
    });
}

fn pending(ui: &mut Ui, doc: &FunctionDocument, p: &Pending, actions: &mut Option<PanelAction>) {
    boxed(ui, theme::WARN, |ui| {
        ui.label(RichText::new("New code is waiting for your review").color(theme::WARN));
        if !p.needs_approval.is_empty() {
            ui.label(format!(
                "It requires {}, which this function is not allowed to use yet. Allowing it adds it to \
                 the contract's allowed namespaces.",
                p.needs_approval.join(", ")
            ));
        }
        if p.replaces_manual {
            ui.label("It would replace code that was written or edited by hand.");
        }
        match &p.validation {
            Some(r) if r.passed() => {
                ui.label(RichText::new(validation_view::summary(r)).color(theme::ACCENT_INK));
            }
            Some(r) => {
                ui.label(RichText::new(validation_view::summary(r)).color(theme::BAD));
            }
            None => muted(ui, "This code was not run."),
        }
        if p.spec_changed {
            ui.label(
                RichText::new(
                    "The specification or tests changed while this code was being generated; it was \
                     checked against the earlier version.",
                )
                .color(theme::WARN),
            );
        }
        notes(ui, &p.notes);
        if !p.attempts.is_empty() {
            validation_view::attempts(ui, doc, &p.attempts, "pending");
        }
        code_box(ui, Id::unique(("pending-code", doc.id)), &p.candidate.code);
        ui.horizontal_wrapped(|ui| {
            let label = if p.needs_approval.is_empty() {
                "Use this code".to_owned()
            } else {
                format!("Allow {} and use this code", p.needs_approval.join(", "))
            };
            if ui.button(label).clicked() {
                *actions = Some(PanelAction::AcceptPending);
            }
            if ui.button("Discard").clicked() {
                *actions = Some(PanelAction::DiscardPending);
            }
        });
    });
}

fn report(ui: &mut Ui, doc: &FunctionDocument, report: &Report, actions: &mut Option<PanelAction>) {
    match report {
        Report::Generated { notes: n, attempts } => {
            if !n.is_empty() || attempts.len() > 1 {
                boxed(ui, theme::LINE, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Generated").color(theme::ACCENT_INK));
                        if ui.small_button("Dismiss").clicked() {
                            *actions = Some(PanelAction::Dismiss);
                        }
                    });
                    notes(ui, n);
                    if attempts.len() > 1 {
                        validation_view::attempts(ui, doc, attempts, "generated");
                    }
                });
            }
        }
        Report::Pending(p) => pending(ui, doc, p, actions),
        Report::Unresolved {
            attempts,
            best,
            notes: n,
        } => boxed(ui, theme::BAD, |ui| {
            ui.label(
                RichText::new("No generated code passed every check; the implementation was not changed.")
                    .color(theme::BAD),
            );
            notes(ui, n);
            validation_view::attempts(ui, doc, attempts, "unresolved");
            ui.horizontal_wrapped(|ui| {
                if let Some(best) = best {
                    let attempt = best
                        .validation
                        .as_ref()
                        .and_then(|r| r.attempt)
                        .map_or_else(String::new, |n| format!(" from attempt {n}"));
                    let mut label = format!("Use the code{attempt} anyway");
                    if !best.needs_approval.is_empty() {
                        label = format!(
                            "Allow {} and use the code{attempt} anyway",
                            best.needs_approval.join(", ")
                        );
                    }
                    let r = ui.button(label).on_hover_text(
                        "Replaces the implementation with code that does not pass every check. Its \
                         results stay visible.",
                    );
                    if r.clicked() {
                        *actions = Some(PanelAction::AcceptPending);
                    }
                }
                if ui.button("Try again").clicked() {
                    *actions = Some(PanelAction::Generate);
                }
                if ui.small_button("Dismiss").clicked() {
                    *actions = Some(PanelAction::Dismiss);
                }
            });
            if best.is_some() && !clojurecraft_workflow::replaces_silently(doc) {
                muted(
                    ui,
                    "Using it would replace code that was written or edited by hand.",
                );
            }
        }),
        Report::Failed {
            what,
            message,
            remedies,
            raw,
        } => boxed(ui, theme::BAD, |ui| {
            ui.label(RichText::new(format!("Could not {what}: {message}.")).color(theme::BAD));
            for r in remedies {
                match r {
                    Remedy::ShortenInput(parts) => muted(ui, format!("Shorten: {}.", parts.join(", "))),
                    other => {
                        if ui.button(other.describe()).clicked() {
                            *actions = Some(PanelAction::Remedy(other.clone()));
                        }
                    }
                }
            }
            if let Some(raw) = raw.as_ref().filter(|r| !r.trim().is_empty()) {
                ui.collapsing("Show the model's answer", |ui| {
                    ui.label(RichText::new(edn_view::abbreviate(raw, 4000)).monospace().small());
                });
            }
            ui.horizontal_wrapped(|ui| {
                if ui.small_button("Dismiss").clicked() {
                    *actions = Some(PanelAction::Dismiss);
                }
            });
        }),
        Report::Blocked(why) => boxed(ui, theme::WARN, |ui| {
            ui.label(RichText::new("The specification needs attention first").color(theme::WARN));
            for w in why {
                ui.label(format!("• {w}"));
            }
        }),
        Report::Cancelled => muted(ui, "Cancelled."),
        // Shown in the specification editor.
        Report::Interpreted { .. } | Report::ContractProposed { .. } => {}
    }
}

/// The generate button, the running job or last report, then the current implementation.
/// `blocked` explains why nothing can be generated right now.
pub fn panel(
    ui: &mut Ui,
    doc: &FunctionDocument,
    assistant: &Assistant,
    blocked: Option<&str>,
    validation: &ValidationContext<'_>,
) -> Option<PanelAction> {
    let mut action = None;
    let job = assistant
        .busy_with(doc.id)
        .filter(|j| j.kind == JobKind::Generate);
    ui.add_space(6.0);
    if job.is_none() {
        ui.horizontal_wrapped(|ui| {
            let label = if doc.implementation.is_some() {
                "Generate again"
            } else {
                "Generate implementation"
            };
            let r = ui
                .add_enabled(blocked.is_none(), egui::Button::new(label))
                .on_hover_text(
                    "The local model writes the function from the description, steps, examples, \
                     contract, and your answers, using the installed Clojure documentation. The code \
                     is run on every example and accepted test; failing code is repaired up to three \
                     times.",
                );
            let r = match blocked {
                Some(why) => r.on_disabled_hover_text(why),
                None => r,
            };
            if r.clicked() {
                action = Some(PanelAction::Generate);
            }
        });
        if let Some(why) = blocked {
            muted(ui, why);
        }
    }
    ui.add_space(4.0);
    match job {
        Some(job) => running(ui, doc, job, &mut action),
        None => {
            if let Some(r) = assistant.report(doc.id) {
                report(ui, doc, r, &mut action);
            }
        }
    }
    if validation.unavailable.is_some() && job.is_none() {
        ui.add_space(4.0);
        crate::implementation::runtime_controls(ui, doc, validation, &mut action);
    }
    ui.add_space(8.0);
    implementation(ui, doc, validation);
    action
}

fn origin(implementation: &Implementation) -> String {
    match &implementation.generation {
        Some(g) if !implementation.is_manually_edited() => format!(
            "Generated by {} ({}) with llama.cpp {} on {}",
            g.model.display_name, g.model.quantization, g.runtime.llama_cpp_release, g.runtime.backend
        ),
        Some(_) => "Generated, then edited by hand".to_owned(),
        None => "Written by hand".to_owned(),
    }
}

/// One line on whether the current code passed its checks.
fn check_status(ui: &mut Ui, doc: &FunctionDocument, v: &ValidationContext<'_>) {
    let (text, color) = match current_report(doc, v.reports) {
        _ if v.running => ("Checks are running…".to_owned(), theme::MUTED),
        Some(r) if !r.freshness(&doc.basis()).is_current() => (
            format!(
                "{} — outdated: the specification or tests changed since.",
                validation_view::summary(r)
            ),
            theme::WARN,
        ),
        Some(r) if r.passed() => (validation_view::summary(r), theme::ACCENT_INK),
        Some(r) => (validation_view::summary(r), theme::BAD),
        None => (
            "Not checked yet. See Result to run the checks.".to_owned(),
            theme::MUTED,
        ),
    };
    ui.label(RichText::new(text).small().color(color));
}

fn implementation(ui: &mut Ui, doc: &FunctionDocument, v: &ValidationContext<'_>) {
    let Some(implementation) = &doc.implementation else {
        muted(
            ui,
            "No implementation yet. Implementations are generated from the specification by the local \
             model and checked against your examples.",
        );
        return;
    };
    muted(ui, origin(implementation));
    check_status(ui, doc, v);
    if let Some(g) = &implementation.generation
        && g.spec_basis != doc.spec_fingerprint()
    {
        ui.label(
            RichText::new("The specification changed after this code was generated.")
                .small()
                .color(theme::WARN),
        );
    }
    ui.add_space(4.0);
    code_box(ui, Id::unique(("implementation", doc.id)), &implementation.source);
    if let Some(g) = &implementation.generation {
        generation_details(ui, g);
    }
}

fn generation_details(ui: &mut Ui, g: &GenerationRecord) {
    ui.add_space(8.0);
    if let Some(e) = &g.explanation
        && !e.summary.is_empty()
    {
        ui.label(RichText::new("How it works").strong());
        ui.label(&e.summary);
        ui.add_space(6.0);
    }
    if !g.assumptions.is_empty() {
        ui.label(RichText::new("Assumptions the model made").strong());
        for a in &g.assumptions {
            ui.label(format!("• {a}"));
        }
        muted(
            ui,
            "Answer them as questions or add them to the specification if they matter.",
        );
        ui.add_space(6.0);
    }
    if let Some(e) = &g.explanation
        && !e.symbols.is_empty()
    {
        ui.label(RichText::new("Standard-library functions used").strong());
        for s in &e.symbols {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&s.symbol).monospace().color(theme::syntax::KEYWORD));
                ui.label(&s.purpose);
            });
        }
        ui.add_space(6.0);
    }
    ui.collapsing("Provenance", |ui| {
        muted(
            ui,
            format!(
                "{} · {} @ {} · {} · sha256 {}",
                g.model.display_name,
                g.model.repository,
                &g.model.revision[..g.model.revision.len().min(12)],
                g.model.file,
                &g.model.sha256[..g.model.sha256.len().min(12)]
            ),
        );
        muted(
            ui,
            format!(
                "llama.cpp {} ({}) · temperature {} · top-p {} · seed {} · context {} tokens · {} attempt{}",
                g.runtime.llama_cpp_release,
                g.runtime.backend,
                g.settings.temperature,
                g.settings.top_p,
                g.settings
                    .seed
                    .map_or_else(|| "random".to_owned(), |s| s.to_string()),
                g.settings.context_length,
                g.attempts,
                if g.attempts == 1 { "" } else { "s" }
            ),
        );
        muted(
            ui,
            format!(
                "{} documentation excerpts used{}",
                g.documentation.len(),
                if g.omitted_context.is_empty() {
                    String::new()
                } else {
                    format!(" · left out to fit the context: {}", g.omitted_context.join(", "))
                }
            ),
        );
    });
}

/// Which code implements each step, from the explanation of the current implementation.
pub fn step_mapping(ui: &mut Ui, doc: &FunctionDocument) {
    let explanation = doc
        .implementation
        .as_ref()
        .filter(|i| !i.is_manually_edited())
        .and_then(|i| i.generation.as_ref())
        .and_then(|g| g.explanation.as_ref());
    for (i, step) in doc.steps.iter().enumerate() {
        let text = step.text.trim();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}.", i + 1)).color(theme::MUTED));
            ui.label(if text.is_empty() { "(empty step)" } else { text });
        });
        let Some(e) = explanation else { continue };
        match e.steps.iter().find(|m| m.step == step.id) {
            Some(m) => {
                let place = match m.lines {
                    Some(l) if l.start == l.end => format!("line {}", l.start),
                    Some(l) => format!("lines {}–{}", l.start, l.end),
                    None => "approximate: this code was not found verbatim in the implementation".into(),
                };
                if m.step_text != text {
                    ui.label(
                        RichText::new("The step was reworded after this explanation.")
                            .small()
                            .color(theme::WARN),
                    );
                }
                muted(ui, place);
                ui.label(RichText::new(&m.excerpt).monospace().small());
                if !m.explanation.is_empty() {
                    ui.label(RichText::new(&m.explanation).small());
                }
            }
            None if !text.is_empty() => muted(ui, "No code was attributed to this step."),
            None => {}
        }
        ui.add_space(6.0);
    }
    ui.add_space(4.0);
    muted(
        ui,
        match explanation {
            Some(_) => "The mapping is the model's explanation of the code, not an executed result.",
            None if doc.implementation.is_some() => {
                "No step mapping: the code was written or edited by hand, or not explained."
            }
            None => "Steps are mapped to code once an implementation is generated.",
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_unescape_streamed_json() {
        assert_eq!(
            preview(r#"{"code": "(ns a)\n(str \"x\")"#),
            "{\"code\": \"(ns a)\n(str \"x\")"
        );
        let long = "é".repeat(2000);
        assert!(preview(&long).chars().count() <= 1500);
    }
}
