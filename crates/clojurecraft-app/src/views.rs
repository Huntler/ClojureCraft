//! Centre-column pages other than the function editor: welcome, trash, knowledge sources, and
//! settings.

use clojurecraft_hardware::{HardwareReport, MemoryBudget};
use clojurecraft_runtime::catalog::{Catalog, format_gib};
use clojurecraft_store::FunctionSummary;
use egui::{RichText, Ui};

use crate::jobs::RuntimeStatus;
use crate::keys;
use crate::theme;

pub struct SystemInfo {
    pub report: HardwareReport,
    pub budget: MemoryBudget,
    pub runtime: RuntimeStatus,
}

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

pub fn knowledge(ui: &mut Ui, system: Option<&SystemInfo>) {
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
            if let Some(sys) = system {
                ui.label(RichText::new("Clojure runtime").color(theme::MUTED));
                installed(ui, sys.runtime.clojure, None);
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

pub fn settings(ui: &mut Ui, system: Option<&SystemInfo>) {
    page_title(ui, "Settings", "");
    egui::ScrollArea::vertical().id_salt("settings").show(ui, |ui| {
        ui.label(RichText::new("Appearance").size(16.0));
        ui.add_space(4.0);
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
        ui.add_space(16.0);
        ui.label(RichText::new("System").size(16.0));
        ui.add_space(4.0);
        system_panel(ui, system);
    });
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

fn system_panel(ui: &mut Ui, system: Option<&SystemInfo>) {
    let Some(sys) = system else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking hardware and installed runtimes…");
        });
        return;
    };
    let r = &sys.report;
    egui::Grid::new("system")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Platform").color(theme::MUTED));
            ui.label(format!("{} {}", r.os, r.arch));
            ui.end_row();
            ui.label(RichText::new("Memory").color(theme::MUTED));
            ui.label(r.total_ram_bytes.map_or_else(|| "unknown".to_owned(), format_gib));
            ui.end_row();
            for g in &r.gpus {
                ui.label(RichText::new("GPU").color(theme::MUTED));
                let vram = g
                    .vram_total_bytes
                    .map_or_else(|| "memory unknown".to_owned(), format_gib);
                ui.label(format!(
                    "{} ({vram}{})",
                    g.name,
                    if g.unified_memory { ", unified" } else { "" }
                ));
                ui.end_row();
            }
            ui.label(RichText::new("Backend").color(theme::MUTED));
            ui.label(r.recommended_backend().label());
            ui.end_row();
            ui.label(RichText::new("Inference budget").color(theme::MUTED));
            ui.label(format!(
                "{} on device + {} spill (estimate)",
                format_gib(sys.budget.device_bytes),
                format_gib(sys.budget.host_spill_bytes)
            ));
            ui.end_row();
            ui.label(RichText::new("llama.cpp").color(theme::MUTED));
            installed(ui, sys.runtime.llama_cpp.is_some(), sys.runtime.llama_cpp.clone());
            ui.end_row();
            ui.label(RichText::new("Java runtime").color(theme::MUTED));
            installed(ui, sys.runtime.java, None);
            ui.end_row();
            ui.label(RichText::new("Clojure").color(theme::MUTED));
            installed(ui, sys.runtime.clojure, None);
            ui.end_row();
            if let Some(dir) = &sys.runtime.data_dir {
                ui.label(RichText::new("Data folder").color(theme::MUTED));
                ui.label(RichText::new(dir).monospace());
                ui.end_row();
            }
        });
    for note in &r.notes {
        ui.label(RichText::new(note).color(theme::WARN));
    }
    if let Ok(catalog) = Catalog::bundled() {
        ui.add_space(10.0);
        match catalog.recommended(&sys.budget, catalog.default_context) {
            Some(c) => ui.label(format!(
                "Suggested model: {} {} (est. {})",
                c.model.name,
                c.quant.name,
                format_gib(c.estimate.total())
            )),
            None => ui.label(RichText::new("No catalog model fits the current budget.").color(theme::WARN)),
        };
    }
}
