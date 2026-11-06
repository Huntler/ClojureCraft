//! Model status (load, unload, failures with their remedies) and the prompt for trying the model.

use std::time::Duration;

use clojurecraft_inference::{LoadPhase, ModelStatus, Remedy};
use egui::{RichText, Ui};

use crate::inference::InferenceState;
use crate::model::{RemedyAction, TrialOutcome};
use crate::theme;

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

fn remedy_buttons(ui: &mut Ui, remedies: &[Remedy], chosen: &mut Option<Remedy>) {
    ui.horizontal_wrapped(|ui| {
        for r in remedies {
            if matches!(r, Remedy::ShortenInput(_)) {
                ui.label(RichText::new(r.describe()).small().color(theme::MUTED));
            } else if ui.button(r.describe()).clicked() {
                *chosen = Some(r.clone());
            }
        }
    });
}

fn log_view(ui: &mut Ui, id: &str, log: &str) {
    if log.trim().is_empty() {
        return;
    }
    egui::CollapsingHeader::new(RichText::new("llama.cpp log").small())
        .id_salt(id)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt((id, "scroll"))
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.label(RichText::new(log).monospace().small().color(theme::MUTED));
                });
        });
}

/// Shows the model's state with the actions that apply to it. `compact` leaves out details for
/// the narrow implementation column.
pub fn status(ui: &mut Ui, st: &mut InferenceState, compact: bool) -> RemedyAction {
    let mut chosen: Option<Remedy> = None;
    let mut load = false;
    let mut unload = false;
    let elapsed = st.model.since.elapsed();
    let spec = st.load_spec();
    match st.model.status.clone() {
        ModelStatus::Unloaded => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Model not loaded").color(theme::MUTED));
                if ui
                    .add_enabled(spec.is_ok(), egui::Button::new("Load model"))
                    .on_hover_text("Starts llama.cpp with the selected model. Loading can take a while for large models.")
                    .clicked()
                {
                    load = true;
                }
            });
            match &spec {
                Ok((s, _)) if !compact => muted(
                    ui,
                    format!(
                        "{} with llama.cpp {}; context {} tokens; {}.",
                        s.label,
                        s.runtime,
                        s.context,
                        match s.gpu_layers {
                            Some(0) => "CPU only".to_owned(),
                            Some(n) => format!("{n} layers on the GPU"),
                            None => "all layers on the GPU".to_owned(),
                        }
                    ),
                ),
                Ok(_) => {}
                Err(why) => muted(ui, why.clone()),
            }
        }
        ModelStatus::Loading { model, phase } => {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            ui.horizontal_wrapped(|ui| {
                ui.spinner();
                ui.label(format!("Loading {model}…"));
                if ui.button("Cancel").clicked() {
                    unload = true;
                }
            });
            let what = match phase {
                LoadPhase::Starting => "Checking memory and starting llama.cpp",
                LoadPhase::Loading => "llama.cpp is reading the model into memory",
            };
            muted(ui, format!("{what} ({} s).", elapsed.as_secs()));
        }
        ModelStatus::Ready { model, info } => {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Ready").color(theme::ACCENT_INK));
                ui.label(&model);
                if ui
                    .button("Unload")
                    .on_hover_text("Stops llama.cpp and frees its memory.")
                    .clicked()
                {
                    unload = true;
                }
            });
            if !compact {
                let backend = st.model.loaded.as_ref().map_or("", |(s, _)| s.backend.as_str());
                muted(
                    ui,
                    format!(
                        "{backend} · context {} tokens · loaded in {:.1} s · tool calls {}",
                        info.context,
                        info.load_time.as_secs_f64(),
                        if info.supports_tools {
                            "supported"
                        } else {
                            "not supported by its chat template"
                        }
                    ),
                );
            }
            if st.model_needs_reload() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("Settings changed since the model was loaded.")
                            .small()
                            .color(theme::WARN),
                    );
                    if ui.small_button("Reload").clicked() {
                        load = true;
                    }
                });
            }
        }
        ModelStatus::Failed {
            model,
            error,
            remedies,
            log,
        } => {
            ui.label(RichText::new(format!("Could not load {model}")).color(theme::BAD));
            ui.label(RichText::new(error.to_string()).small());
            if error.is_memory() {
                muted(ui, "Memory figures are estimates.");
            }
            remedy_buttons(ui, &remedies, &mut chosen);
            if ui.small_button("Try again").clicked() {
                load = true;
            }
            if !compact {
                log_view(ui, "load-log", &log);
            }
        }
        ModelStatus::Crashed {
            model,
            detail,
            restarting,
            log,
        } => {
            if restarting {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(format!(
                            "llama.cpp stopped unexpectedly ({detail}); loading {model} again…"
                        ))
                        .color(theme::WARN),
                    );
                });
            } else {
                ui.label(
                    RichText::new(format!("llama.cpp stopped unexpectedly again ({detail})"))
                        .color(theme::BAD),
                );
                muted(
                    ui,
                    "It was not restarted automatically because it stopped several times in a short period.",
                );
                remedy_buttons(ui, &[Remedy::Restart, Remedy::SmallerModel], &mut chosen);
            }
            if !compact {
                log_view(ui, "crash-log", &log);
            }
        }
    }
    if load {
        st.load_model(false);
    }
    if unload {
        st.unload_model();
    }
    chosen.map_or(RemedyAction::None, |r| st.apply_remedy(&r))
}

/// A free-form prompt for checking that the model works, with streaming output.
pub fn trial(ui: &mut Ui, st: &mut InferenceState) -> RemedyAction {
    let mut chosen = None;
    let ready = st.model.is_ready();
    let running = st.model.trial.running.is_some();
    ui.add(
        egui::TextEdit::multiline(&mut st.model.trial.prompt)
            .hint_text("Ask the model something, e.g. “What does clojure.core/frequencies return?”")
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    );
    let mut send = false;
    ui.horizontal(|ui| {
        let enabled = ready && !st.model.trial.prompt.trim().is_empty();
        if ui
            .add_enabled(
                enabled,
                egui::Button::new(if running { "Ask again" } else { "Ask" }),
            )
            .clicked()
        {
            send = true;
        }
        if running {
            if ui.button("Stop").clicked() {
                st.cancel_trial();
            }
            ui.spinner();
        }
        if !ready {
            muted(ui, "Load the model first.");
        }
    });
    if send {
        st.start_trial();
    }
    let t = &st.model.trial;
    if !t.reasoning.is_empty() {
        egui::CollapsingHeader::new(RichText::new("Reasoning").small())
            .id_salt("trial-reasoning")
            .show(ui, |ui| {
                ui.label(RichText::new(&t.reasoning).small().color(theme::MUTED));
            });
    }
    if !t.output.is_empty() {
        egui::Frame::new()
            .fill(theme::SUNKEN)
            .stroke(theme::hairline())
            .corner_radius(theme::RADIUS)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_salt("trial-output")
                    .max_height(320.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| ui.label(RichText::new(&t.output).monospace()));
            });
    }
    match t.outcome.clone() {
        Some(TrialOutcome::Done(summary)) => muted(ui, summary),
        Some(TrialOutcome::Cancelled) => muted(ui, "Stopped."),
        Some(TrialOutcome::Failed { message, remedies }) => {
            ui.label(RichText::new(message).small().color(theme::BAD));
            remedy_buttons(ui, &remedies, &mut chosen);
        }
        None => {}
    }
    chosen.map_or(RemedyAction::None, |r| st.apply_remedy(&r))
}
