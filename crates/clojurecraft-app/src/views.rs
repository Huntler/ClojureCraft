//! Centre-column pages other than the function editor: welcome, trash, knowledge sources, and
//! settings.

use clojurecraft_store::FunctionSummary;
use egui::{RichText, Ui};

use crate::inference::InferenceState;
use crate::keys;
use crate::model::RemedyAction;
use crate::model_view;
use crate::models_view::{self, ModelsPage};
use crate::setup_view;
use crate::theme;

pub fn page_title(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).size(24.0).color(theme::INK));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(theme::MUTED));
    }
    ui.add_space(16.0);
}

pub enum WelcomeAction {
    None,
    Create,
}

pub fn welcome(ui: &mut Ui, library_error: Option<&str>) -> WelcomeAction {
    page_title(
        ui,
        "ClojureCraft",
        "Describe a data transformation; get a tested Clojure function.",
    );
    if let Some(e) = library_error {
        ui.label(RichText::new("The function library could not be opened.").color(theme::BAD));
        ui.label(RichText::new(e).small().color(theme::MUTED));
        return WelcomeAction::None;
    }
    let mut action = WelcomeAction::None;
    if ui.button("Create a function").clicked() {
        action = WelcomeAction::Create;
    }
    ui.add_space(6.0);
    ui.label(
        RichText::new(format!(
            "Or select a saved function on the left. Press {} at any time to start a new one.",
            keys::label(&keys::NEW_FUNCTION)
        ))
        .color(theme::MUTED),
    );
    action
}

pub enum TrashAction {
    Restore(FunctionSummary),
    Purge(FunctionSummary),
}

pub fn trash(ui: &mut Ui, trash: &[FunctionSummary]) -> Option<TrashAction> {
    page_title(
        ui,
        "Trash",
        "Functions moved to the trash keep their history and can be restored.",
    );
    if trash.is_empty() {
        ui.label(RichText::new("The trash is empty.").color(theme::MUTED));
        return None;
    }
    let mut action = None;
    egui::ScrollArea::vertical().id_salt("trash").show(ui, |ui| {
        for f in trash {
            egui::Frame::new()
                .stroke(theme::hairline())
                .corner_radius(theme::RADIUS)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(display_name(&f.name)).strong());
                            let revisions = match f.revision_count {
                                0 => "no saved revisions".to_owned(),
                                1 => "1 revision".to_owned(),
                                n => format!("{n} revisions"),
                            };
                            ui.label(RichText::new(revisions).small().color(theme::MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Delete permanently…").clicked() {
                                action = Some(TrashAction::Purge(f.clone()));
                            }
                            if ui.button("Restore").clicked() {
                                action = Some(TrashAction::Restore(f.clone()));
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
    });
    action
}

pub fn display_name(name: &str) -> &str {
    let name = name.trim();
    if name.is_empty() {
        "Untitled function"
    } else {
        name
    }
}

pub fn knowledge(ui: &mut Ui, inference: &InferenceState) {
    page_title(
        ui,
        "Knowledge sources",
        "Clojure documentation used to ground generated code. It is stored and searched locally.",
    );
    egui::Grid::new("knowledge")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Clojure documentation").color(theme::MUTED));
            ui.label(RichText::new("not installed").color(theme::WARN));
            ui.end_row();
            if let Some(snapshot) = &inference.snapshot {
                ui.label(RichText::new("Java runtime").color(theme::MUTED));
                installed(ui, snapshot.java, None);
                ui.end_row();
                ui.label(RichText::new("Clojure").color(theme::MUTED));
                installed(ui, snapshot.clojure, None);
                ui.end_row();
            }
        });
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Installing, updating, and browsing the documentation collection happens here. \
             Specifications can be written without it.",
        )
        .color(theme::MUTED),
    );
}

fn installed(ui: &mut Ui, ok: bool, detail: Option<String>) {
    let (text, color) = if ok {
        ("installed", theme::ACCENT_INK)
    } else {
        ("not installed", theme::WARN)
    };
    ui.label(RichText::new(detail.unwrap_or_else(|| text.to_owned())).color(color));
}

pub enum SettingsAction {
    None,
    OpenSetup,
}

pub fn settings(ui: &mut Ui, inference: &mut InferenceState, page: &mut ModelsPage) -> SettingsAction {
    let mut action = SettingsAction::None;
    page_title(ui, "Settings", "");
    egui::ScrollArea::vertical().id_salt("settings").show(ui, |ui| {
        ui.set_max_width(ui.available_width().min(900.0));
        if let Some(problem) = &inference.settings_problem {
            ui.label(RichText::new(problem).color(theme::WARN));
        }
        section(ui, "Inference");
        if ui.button("Open guided setup").clicked() {
            action = SettingsAction::OpenSetup;
        }
        ui.add_space(6.0);
        models_view::generation(ui, inference);
        section(ui, "Model");
        if model_view::status(ui, inference, false) == RemedyAction::OpenSetup {
            action = SettingsAction::OpenSetup;
        }
        ui.add_space(10.0);
        ui.label(RichText::new("Try the model").strong());
        if model_view::trial(ui, inference) == RemedyAction::OpenSetup {
            action = SettingsAction::OpenSetup;
        }
        section(ui, "Hardware and memory budget");
        setup_view::hardware(ui, inference);
        section(ui, "Inference backend");
        setup_view::backend(ui, inference);
        section(ui, "llama.cpp runtime");
        models_view::runtimes(ui, inference);
        section(ui, "Installed models");
        models_view::installed(ui, inference, page);
        section(ui, "Curated models");
        egui::CollapsingHeader::new("Browse the curated catalog")
            .id_salt("settings-catalog-open")
            .show(ui, |ui| {
                let (mut show_all, mut selection) = ui
                    .data(|d| d.get_temp::<(bool, Vec<String>)>(egui::Id::unique("settings-catalog")))
                    .unwrap_or_default();
                setup_view::models(ui, inference, &mut show_all, &mut selection);
                ui.data_mut(|d| d.insert_temp(egui::Id::unique("settings-catalog"), (show_all, selection)));
            });
        section(ui, "Models from Hugging Face");
        models_view::hugging_face(ui, inference);
        section(ui, "Hugging Face access token");
        models_view::token(ui, inference, page);
        section(ui, "Storage");
        models_view::storage(ui, inference);
        section(ui, "Appearance");
        let ctx = ui.ctx().clone();
        let mut zoom = ctx.zoom_factor();
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 220.0;
            ui.label("Text size");
            let r = ui.add(
                egui::Slider::new(&mut zoom, theme::ZOOM_RANGE)
                    .step_by(0.05)
                    .custom_formatter(|v, _| format!("{:.0} %", v * 100.0)),
            );
            if r.changed() {
                ctx.set_zoom_factor(zoom);
            }
            if ui.button("Reset").clicked() {
                ctx.set_zoom_factor(1.0);
            }
        });
        ui.add_space(16.0);
        shortcuts(ui);
        ui.add_space(24.0);
    });
    action
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(18.0);
    ui.label(RichText::new(title).size(16.0));
    ui.add_space(4.0);
}

fn shortcuts(ui: &mut Ui) {
    ui.label(RichText::new("Keyboard").size(16.0));
    ui.add_space(4.0);
    // Each row wraps on its own, so descriptions stay readable in a narrow column.
    for (keys, what) in keys::reference() {
        ui.horizontal_wrapped(|ui| {
            ui.add_sized(
                [220.0_f32.min(ui.available_width() * 0.45), 18.0],
                egui::Label::new(RichText::new(keys).monospace()).truncate(),
            );
            ui.label(what);
        });
    }
}
