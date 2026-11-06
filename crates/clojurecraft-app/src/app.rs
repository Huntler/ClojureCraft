//! The application shell: function library on the left, specification in the centre, and
//! implementation and results on the right.

use std::sync::Arc;

use clojurecraft_domain::SecretRegistry;
use clojurecraft_hardware::{HardwareReport, MemoryBudget};
use clojurecraft_runtime::catalog::{Catalog, format_gib};
use clojurecraft_store::FunctionSummary;
use egui::{Color32, RichText, Ui};

use crate::jobs::{Event, Jobs, RuntimeStatus};
use crate::theme;

struct SystemInfo {
    report: HardwareReport,
    budget: MemoryBudget,
    runtime: RuntimeStatus,
}

pub struct ClojureCraftApp {
    jobs: Jobs,
    system: Option<SystemInfo>,
    /// `None` while loading.
    library: Option<Result<Vec<FunctionSummary>, String>>,
}

impl ClojureCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let jobs = Jobs::new(cc.egui_ctx.clone());
        jobs.check_system();
        // Every credential the application handles is registered here so it can never be
        // written into function history or results.
        jobs.load_library(Arc::new(SecretRegistry::new()));
        Self {
            jobs,
            system: None,
            library: None,
        }
    }

    fn handle_events(&mut self) {
        for event in self.jobs.drain() {
            match event {
                Event::SystemChecked {
                    report,
                    budget,
                    runtime,
                } => {
                    self.system = Some(SystemInfo {
                        report: *report,
                        budget,
                        runtime,
                    });
                }
                Event::LibraryLoaded(result) => self.library = Some(result),
            }
        }
    }

    fn library(&self, ui: &mut Ui) {
        ui.add_space(8.0);
        ui.heading("Functions");
        ui.add_space(4.0);
        match &self.library {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Opening library…").color(theme::MUTED));
                });
            }
            Some(Err(e)) => {
                ui.label(RichText::new("The function library could not be opened.").color(theme::BAD));
                ui.label(RichText::new(e).color(theme::MUTED).small());
            }
            Some(Ok(functions)) if functions.is_empty() => {
                ui.label(RichText::new("No saved functions yet.").color(theme::MUTED));
            }
            Some(Ok(functions)) => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for f in functions {
                        ui.label(&f.name).on_hover_text(&f.description);
                    }
                });
            }
        }
    }

    fn system_panel(&self, ui: &mut Ui) {
        ui.heading("System");
        ui.add_space(6.0);
        let Some(sys) = &self.system else {
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
                let status = |ok: bool| {
                    if ok {
                        ("installed", theme::ACCENT_INK)
                    } else {
                        ("not installed", theme::WARN)
                    }
                };
                for (name, ok, detail) in [
                    (
                        "llama.cpp",
                        sys.runtime.llama_cpp.is_some(),
                        sys.runtime.llama_cpp.clone(),
                    ),
                    ("Java runtime", sys.runtime.java, None),
                    ("Clojure", sys.runtime.clojure, None),
                ] {
                    ui.label(RichText::new(name).color(theme::MUTED));
                    let (text, color) = status(ok);
                    ui.label(RichText::new(detail.unwrap_or_else(|| text.to_owned())).color(color));
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
                None => {
                    ui.label(RichText::new("No catalog model fits the current budget.").color(theme::WARN))
                }
            };
        }
    }
}

impl eframe::App for ClojureCraftApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.handle_events();
        let side = egui::Frame::new()
            .fill(theme::PANEL)
            .inner_margin(12.0)
            .stroke(theme::hairline());
        egui::Panel::left("library")
            .resizable(true)
            .default_size(240.0)
            .size_range(200.0..=360.0)
            .frame(side)
            .show(ui, |ui| self.library(ui));
        egui::Panel::right("implementation")
            .resizable(true)
            .default_size(420.0)
            .size_range(320.0..=720.0)
            .frame(side)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading("Implementation");
                ui.label(RichText::new("Generated code and results appear here.").color(theme::MUTED));
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(20.0))
            .show(ui, |ui| {
                ui.label(RichText::new("ClojureCraft").size(26.0).color(Color32::WHITE));
                ui.label(
                    RichText::new("Describe a data transformation; get a tested Clojure function.")
                        .color(theme::MUTED),
                );
                ui.add_space(18.0);
                self.system_panel(ui);
            });
    }
}
