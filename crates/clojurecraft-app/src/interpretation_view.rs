//! The interpretation section of the specification editor: the model's reading of the
//! specification next to the user's own text, and its questions. Only the user's answers become
//! requirements; the reading itself and unanswered questions never do.

use clojurecraft_domain::{ClarificationStatus, IssueKind};
use egui::{Id, RichText, Ui};

use crate::editor::{Action, EditorContext, section};
use crate::session::OpenDocument;
use crate::theme;
use clojurecraft_workflow::assistant::{JobKind, Report};

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

fn controls(ui: &mut Ui, has_reading: bool, cx: &mut EditorContext<'_>) {
    if let Some((_, phase)) = cx.assist.job.filter(|(k, _)| *k == JobKind::Interpret) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("{}…", phase.label()));
            if ui.button("Cancel").clicked() {
                cx.actions.push(Action::CancelJob);
            }
        });
        return;
    }
    let blocked = cx.assist.blocked.clone();
    let label = if has_reading {
        "Check again"
    } else {
        "Check the specification"
    };
    let r = ui
        .add_enabled(blocked.is_none(), egui::Button::new(label))
        .on_hover_text(
            "The local model restates what the function does and asks about contradictions and gaps. \
             Your text is never changed.",
        );
    let r = match &blocked {
        Some(why) => r.on_disabled_hover_text(why.as_str()),
        None => r,
    };
    if r.clicked() {
        cx.actions.push(Action::Interpret);
    }
    match &cx.assist.report {
        Some(Report::Interpreted { questions, notes }) => {
            if *questions == 0 {
                muted(ui, "No questions: the model found no contradictions or gaps.");
            }
            for n in notes {
                muted(ui, n.clone());
            }
        }
        Some(Report::Failed {
            what, message, raw, ..
        }) if *what == "check the specification" => {
            ui.label(
                RichText::new(format!("The specification could not be checked: {message}."))
                    .small()
                    .color(theme::BAD),
            );
            if let Some(raw) = raw.as_ref().filter(|r| !r.trim().is_empty()) {
                ui.collapsing("Show the model's answer", |ui| {
                    ui.label(
                        RichText::new(crate::edn_view::abbreviate(raw, 4000))
                            .monospace()
                            .small(),
                    );
                });
            }
            if ui.small_button("Dismiss").clicked() {
                cx.actions.push(Action::DismissReport);
            }
        }
        Some(Report::Blocked(why)) => {
            for w in why {
                ui.label(RichText::new(format!("• {w}")).small().color(theme::WARN));
            }
        }
        _ => {}
    }
}

pub fn interpretation(ui: &mut Ui, open: &mut OpenDocument, cx: &mut EditorContext<'_>) -> bool {
    let mut changed = false;
    section(
        ui,
        "Interpretation and questions",
        "Optional. Answers to questions become part of the specification.",
    );
    let spec = open.doc.spec_fingerprint();
    controls(ui, open.doc.interpretation.is_some(), cx);
    if let Some(reading) = &open.doc.interpretation {
        ui.add_space(6.0);
        egui::Frame::new()
            .stroke(theme::hairline())
            .corner_radius(theme::RADIUS)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("The model's reading").strong());
                    muted(ui, format!("by {}", reading.model));
                });
                if reading.spec_basis != spec {
                    ui.label(
                        RichText::new("The specification changed since; check again to update this reading.")
                            .small()
                            .color(theme::WARN),
                    );
                }
                ui.label(&reading.restatement);
                if !reading.assumptions.is_empty() {
                    ui.add_space(4.0);
                    muted(ui, "It would assume:");
                    for a in &reading.assumptions {
                        ui.label(format!("• {a}"));
                    }
                }
                if !reading.issues.is_empty() {
                    ui.add_space(4.0);
                    muted(ui, "It found:");
                    for i in &reading.issues {
                        let kind = match i.kind {
                            IssueKind::Contradiction => "Contradiction",
                            IssueKind::Gap => "Gap",
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(kind).color(theme::WARN));
                            ui.label(&i.detail);
                            if !i.refers_to.is_empty() {
                                muted(ui, format!("({})", i.refers_to.join(", ")));
                            }
                        });
                    }
                }
            });
    }
    let function = open.doc.id;
    let mut dismissed = 0;
    for c in &mut open.doc.clarifications {
        if c.status == ClarificationStatus::Dismissed {
            dismissed += 1;
            continue;
        }
        ui.add_space(6.0);
        let id = Id::unique(("clarification", function, c.id));
        let answered = c.is_answered();
        egui::Frame::new()
            .stroke(egui::Stroke::new(
                1.0,
                if answered { theme::LINE } else { theme::WARN },
            ))
            .corner_radius(theme::RADIUS)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(if answered { "Answered" } else { "Open question" })
                            .small()
                            .color(if answered { theme::ACCENT_INK } else { theme::WARN }),
                    );
                    if !c.refers_to.is_empty() {
                        muted(ui, c.refers_to.join(", "));
                    }
                });
                ui.label(RichText::new(&c.question).strong());
                if !c.context.trim().is_empty() {
                    muted(ui, c.context.clone());
                }
                if !c.options.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for o in c.options.clone() {
                            if ui.selectable_label(c.answer.trim() == o.trim(), &o).clicked() {
                                c.answer(o);
                                changed = true;
                            }
                        }
                    });
                }
                let mut answer = c.answer.clone();
                let r = ui.add(
                    egui::TextEdit::multiline(&mut answer)
                        .id(id.with("answer"))
                        .hint_text("Your answer")
                        .desired_rows(1)
                        .desired_width(f32::INFINITY),
                );
                if r.changed() {
                    c.answer(answer);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    if answered && ui.small_button("Clear answer").clicked() {
                        c.answer("");
                        changed = true;
                    }
                    if ui
                        .small_button("Dismiss")
                        .on_hover_text("This question does not matter; it is not asked again.")
                        .clicked()
                    {
                        c.status = ClarificationStatus::Dismissed;
                        changed = true;
                    }
                });
            });
    }
    if dismissed > 0 {
        ui.add_space(4.0);
        ui.collapsing(format!("Dismissed questions ({dismissed})"), |ui| {
            for c in &mut open.doc.clarifications {
                if c.status != ClarificationStatus::Dismissed {
                    continue;
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label(&c.question);
                    if ui.small_button("Ask again").clicked() {
                        c.status = ClarificationStatus::Open;
                        c.answer.clear();
                        changed = true;
                    }
                });
            }
        });
    }
    changed
}
