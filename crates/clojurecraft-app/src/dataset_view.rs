//! The fine-tuning dataset page: saved functions whose code (generated, or hand-written and
//! described) passes its checks, chosen for export as JSON Lines (model input messages and
//! validated code per function).

use std::collections::HashSet;
use std::path::PathBuf;

use clojurecraft_domain::FunctionId;
use clojurecraft_workflow::dataset::{self, PromptSource};
use egui::{RichText, Ui};

use crate::library::DatasetRow;
use crate::theme;
use crate::views;

pub struct DatasetPage {
    pub rows: Option<Vec<DatasetRow>>,
    pub loading: bool,
    /// Qualifying functions the user left out; every other qualifying function is selected.
    excluded: HashSet<FunctionId>,
    path: String,
    status: Option<(String, bool)>,
}

impl Default for DatasetPage {
    fn default() -> Self {
        Self {
            rows: None,
            loading: false,
            excluded: HashSet::new(),
            path: "~/clojurecraft-dataset.jsonl".into(),
            status: None,
        }
    }
}

pub enum DatasetAction {
    None,
    Refresh,
}

impl DatasetPage {
    fn selected(&self) -> Vec<&dataset::Entry> {
        self.rows
            .iter()
            .flatten()
            .filter(|r| !self.excluded.contains(&r.id))
            .filter_map(|r| r.entry.as_ref().ok())
            .collect()
    }

    fn export(&self) -> Result<String, String> {
        let entries = self.selected();
        let path = expand(&self.path);
        std::fs::write(&path, dataset::jsonl(entries.iter().copied()))
            .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
        Ok(format!(
            "Exported {} functions to {}",
            entries.len(),
            path.display()
        ))
    }
}

fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

pub fn page(ui: &mut Ui, page: &mut DatasetPage) -> DatasetAction {
    let mut action = DatasetAction::None;
    views::page_title(
        ui,
        "Fine-tune dataset",
        "Generated code that passes all its checks, with the prompt it answers. Exported as JSON \
         Lines: one function per line with its input messages and the validated code.",
    );
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!page.loading, egui::Button::new("Refresh"))
            .clicked()
        {
            action = DatasetAction::Refresh;
        }
        if page.loading {
            ui.spinner();
        }
    });
    let Some(rows) = &page.rows else {
        return action;
    };
    let (qualifying, other): (Vec<&DatasetRow>, Vec<&DatasetRow>) =
        rows.iter().partition(|r| r.entry.is_ok());
    ui.add_space(8.0);
    if qualifying.is_empty() {
        ui.label(
            RichText::new("No function has passing code: unedited generated code, or hand-written code with a description.")
                .color(theme::MUTED),
        );
    } else {
        ui.horizontal(|ui| {
            let selected = qualifying
                .iter()
                .filter(|r| !page.excluded.contains(&r.id))
                .count();
            ui.label(format!("{selected} of {} selected", qualifying.len()));
            if ui.small_button("All").clicked() {
                page.excluded.clear();
            }
            if ui.small_button("None").clicked() {
                page.excluded.extend(qualifying.iter().map(|r| r.id));
            }
        });
    }
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt("dataset")
        .max_height((ui.available_height() - 90.0).max(120.0))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for r in &qualifying {
                let Ok(e) = &r.entry else { continue };
                let mut on = !page.excluded.contains(&r.id);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut on, views::display_name(&r.name)).changed() {
                        if on {
                            page.excluded.remove(&r.id);
                        } else {
                            page.excluded.insert(r.id);
                        }
                    }
                    let prompt = match e.prompt_source {
                        PromptSource::Recorded => "prompt as sent",
                        PromptSource::Rebuilt => "prompt rebuilt without documentation",
                    };
                    let attempts = match e.attempts {
                        0 => String::new(),
                        1 => "1 attempt · ".to_owned(),
                        n => format!("{n} attempts · "),
                    };
                    ui.label(
                        RichText::new(format!("{} · {attempts}{prompt}", e.model))
                            .small()
                            .color(theme::MUTED),
                    );
                });
            }
            if !other.is_empty() {
                ui.add_space(6.0);
                egui::CollapsingHeader::new(format!("Not included ({})", other.len()))
                    .id_salt("dataset-other")
                    .show(ui, |ui| {
                        for r in &other {
                            let Err(reason) = &r.entry else { continue };
                            ui.horizontal_wrapped(|ui| {
                                ui.label(views::display_name(&r.name));
                                ui.label(RichText::new(reason).small().color(theme::MUTED));
                            });
                        }
                    });
            }
        });
    ui.add_space(10.0);
    let count = page.selected().len();
    ui.horizontal(|ui| {
        ui.label("File");
        ui.add(egui::TextEdit::singleline(&mut page.path).desired_width(420.0));
        if ui
            .add_enabled(
                count > 0 && !page.path.trim().is_empty(),
                egui::Button::new("Export"),
            )
            .clicked()
        {
            page.status = Some(match page.export() {
                Ok(m) => (m, false),
                Err(e) => (e, true),
            });
        }
    });
    if expand(&page.path).exists() {
        ui.label(
            RichText::new("Exporting replaces this file.")
                .small()
                .color(theme::WARN),
        );
    }
    if let Some((message, error)) = &page.status {
        ui.label(RichText::new(message).color(if *error { theme::BAD } else { theme::ACCENT_INK }));
    }
    action
}
