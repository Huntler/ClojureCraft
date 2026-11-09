//! Describing existing code: the model writes the specification of the current implementation,
//! and the code is run to record the examples' outputs.

use clojurecraft_domain::FunctionDocument;
use clojurecraft_workflow::assistant::{JobKind, Report};
use egui::{RichText, Ui};

use crate::editor::{Action, EditorContext};
use crate::theme;

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

/// The describe button with the running job or its last report; shown only for functions that
/// have code.
pub fn describe(ui: &mut Ui, doc: &FunctionDocument, cx: &mut EditorContext<'_>) {
    if clojurecraft_workflow::describe::blocker(doc).is_some() {
        return;
    }
    ui.add_space(8.0);
    if let Some((_, phase)) = cx.assist.job.filter(|(k, _)| *k == JobKind::Describe) {
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
    let r = ui
        .add_enabled(blocked.is_none(), egui::Button::new("Describe from code"))
        .on_hover_text(
            "The local model writes the description and steps from the code and proposes example \
             inputs. The code is run to record their outputs, then a contract is proposed. Only empty \
             fields are filled; examples are added to the existing ones.",
        );
    let r = match &blocked {
        Some(why) => r.on_disabled_hover_text(why.as_str()),
        None => r,
    };
    if r.clicked() {
        cx.actions.push(Action::Describe);
    }
    match &cx.assist.report {
        Some(Report::Described { filled, notes }) => {
            if filled.is_empty() {
                muted(
                    ui,
                    "Nothing was filled: every part of the specification already had content.",
                );
            } else {
                muted(ui, format!("Filled from the code: {}.", filled.join(", ")));
            }
            for n in notes {
                muted(ui, n.clone());
            }
        }
        Some(Report::Failed {
            what, message, raw, ..
        }) if *what == JobKind::Describe.label() => {
            ui.label(
                RichText::new(format!("The code could not be described: {message}."))
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
        _ => {}
    }
}
