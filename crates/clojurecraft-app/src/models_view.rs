//! Settings sections for inference: generation settings, runtime builds, installed models,
//! models from Hugging Face, the access token, and disk usage.

use clojurecraft_hardware::Backend;
use clojurecraft_runtime::catalog::{Fit, fit, offload_plan};
use clojurecraft_runtime::hub::Inspection;
use clojurecraft_runtime::models::{ModelRecord, ModelSource};
use clojurecraft_runtime::settings::GpuLayers;
use egui::{RichText, Ui};

use crate::inference::{InferenceState, TokenState};
use crate::setup_view::{self, size};
use crate::theme;

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(theme::MUTED));
}

/// UI state for these sections that is not part of the settings.
#[derive(Default)]
pub struct ModelsPage {
    pub token_input: String,
    /// Key awaiting a second click to confirm removal.
    pub confirm_remove: Option<String>,
}

pub fn generation(ui: &mut Ui, st: &mut InferenceState) {
    let mut changed = false;
    let models: Vec<ModelRecord> = st.models().to_vec();
    ui.horizontal(|ui| {
        ui.label("Model for generation");
        let current = st.active_model().map_or_else(|| "none".to_owned(), label);
        egui::ComboBox::from_id_salt("active-model")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for m in &models {
                    let selected = st.settings.active_model.as_deref() == Some(m.key.as_str());
                    if ui.selectable_label(selected, label(m)).clicked() && !selected {
                        st.settings.active_model = Some(m.key.clone());
                        changed = true;
                    }
                }
            });
    });
    if models.is_empty() {
        muted(ui, "No model is installed yet.");
    }
    let active = st.active_model().cloned();
    let max_ctx = active.as_ref().and_then(|m| m.context_length).unwrap_or(262_144);
    ui.horizontal(|ui| {
        ui.label("Context limit");
        let mut ctx = st
            .settings
            .context_limit
            .unwrap_or(st.default_context())
            .min(max_ctx);
        if ui
            .add(
                egui::DragValue::new(&mut ctx)
                    .range(1024..=max_ctx)
                    .speed(256)
                    .suffix(" tokens"),
            )
            .on_hover_text("Longer contexts fit more documentation and examples but need more memory.")
            .changed()
        {
            st.settings.context_limit = Some(ctx);
            changed = true;
        }
        if st.settings.context_limit.is_some() && ui.button("Default").clicked() {
            st.settings.context_limit = None;
            changed = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label("GPU layers");
        let gpu = st.backend() != Backend::Cpu;
        ui.add_enabled_ui(gpu, |ui| {
            let mut mode = st.settings.gpu_layers;
            ui.radio_value(&mut mode, GpuLayers::Auto, "Automatic")
                .on_hover_text("Chosen from the memory budget and the model's estimated size.");
            ui.radio_value(&mut mode, GpuLayers::All, "All");
            let count = match mode {
                GpuLayers::Count(n) => n,
                _ => active
                    .as_ref()
                    .and_then(|m| st.offload_for(m))
                    .map_or(0, |p| p.gpu_layers),
            };
            if ui.radio(matches!(mode, GpuLayers::Count(_)), "Fixed").clicked() {
                mode = GpuLayers::Count(count);
            }
            if let GpuLayers::Count(n) = &mut mode {
                let max = active.as_ref().and_then(|m| m.layers).unwrap_or(200);
                ui.add(egui::DragValue::new(n).range(0..=max));
            }
            if mode != st.settings.gpu_layers {
                st.settings.gpu_layers = mode;
                changed = true;
            }
        });
    });
    if let Some(m) = &active
        && let Some(plan) = st.offload_for(m)
        && st.backend() != Backend::Cpu
    {
        let text = if plan.is_full() {
            format!(
                "Estimate: the whole model fits on the GPU ({}).",
                size(plan.device_bytes)
            )
        } else {
            format!(
                "Estimate: {} of {} layers fit on the GPU; {} of weights stay in system memory.",
                plan.gpu_layers,
                plan.total_layers,
                size(plan.host_bytes)
            )
        };
        muted(ui, text);
    }
    if changed {
        st.save_settings();
    }
}

fn label(m: &ModelRecord) -> String {
    format!("{} · {}", m.display_name, m.quant)
}

pub fn runtimes(ui: &mut Ui, st: &mut InferenceState) {
    let builds = st.snapshot.as_ref().map(|s| s.builds.clone()).unwrap_or_default();
    if builds.is_empty() {
        muted(ui, "No llama.cpp build is installed.");
    }
    let mut action = None;
    egui::Grid::new("runtime-builds")
        .num_columns(3)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            for b in &builds {
                ui.label(format!(
                    "llama.cpp {} · {}",
                    b.release,
                    b.backend.map_or("unknown", Backend::label)
                ));
                let mut status = Vec::new();
                if b.active {
                    status.push("active");
                }
                if !b.pinned {
                    status.push("not the release pinned by this version");
                }
                muted(ui, status.join(", "));
                ui.horizontal(|ui| {
                    if !b.active {
                        if ui.button("Activate").clicked() {
                            action = Some((true, b.version.clone()));
                        }
                        if ui.button("Remove").clicked() {
                            action = Some((false, b.version.clone()));
                        }
                    }
                });
                ui.end_row();
            }
        });
    match action {
        Some((true, v)) => st.activate_build(&v),
        Some((false, v)) => st.remove_build(&v),
        None => {}
    }
    if builds.len() > 1 && ui.button("Roll back to the previously active build").clicked() {
        st.rollback_build();
    }
    setup_view::runtime(ui, st);
}

pub fn installed(ui: &mut Ui, st: &mut InferenceState, page: &mut ModelsPage) {
    let models = st.models().to_vec();
    if let Some(s) = &st.snapshot {
        for bad in &s.unreadable {
            ui.label(RichText::new(bad).small().color(theme::WARN));
        }
    }
    if models.is_empty() {
        muted(ui, "No models are installed.");
    }
    for m in models {
        let problems: Vec<String> = st.problems(&m.key).iter().map(ToString::to_string).collect();
        let verifying = st.verifying.contains(&m.key);
        let active = st.settings.active_model.as_deref() == Some(m.key.as_str());
        egui::Frame::new()
            .stroke(theme::hairline())
            .corner_radius(theme::RADIUS)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(label(&m)).strong());
                    if active {
                        ui.label(
                            RichText::new("used for generation")
                                .small()
                                .color(theme::ACCENT_INK),
                        );
                    }
                    if !problems.is_empty() {
                        ui.label(RichText::new("needs repair").small().color(theme::BAD));
                    }
                });
                let source = match &m.source {
                    ModelSource::Catalog { .. } => "curated catalog",
                    ModelSource::HuggingFace => "Hugging Face",
                };
                muted(
                    ui,
                    format!(
                        "{} · {} · {}@{} · {}",
                        size(m.total_size()),
                        m.architecture,
                        m.repo,
                        &m.revision[..m.revision.len().min(12)],
                        source
                    ),
                );
                for p in &problems {
                    ui.label(RichText::new(p).small().color(theme::BAD));
                }
                ui.horizontal(|ui| {
                    if !active && problems.is_empty() && ui.button("Use for generation").clicked() {
                        st.use_model(&m.key);
                    }
                    if !problems.is_empty()
                        && ui
                            .button("Repair")
                            .on_hover_text("Downloads the damaged files again.")
                            .clicked()
                    {
                        let mut plan = m.clone();
                        plan.installed_at = 0;
                        st.install_model(plan);
                    }
                    if verifying {
                        ui.spinner();
                        muted(ui, "Checking every file…");
                    } else if ui
                        .button("Verify")
                        .on_hover_text("Re-computes every file's checksum.")
                        .clicked()
                    {
                        st.verify_model(&m.key);
                    }
                    if page.confirm_remove.as_deref() == Some(m.key.as_str()) {
                        if ui
                            .button(
                                RichText::new(format!("Remove ({} freed)", size(m.total_size())))
                                    .color(theme::BAD),
                            )
                            .clicked()
                        {
                            page.confirm_remove = None;
                            st.remove_model(&m.key);
                        }
                        if ui.button("Keep").clicked() {
                            page.confirm_remove = None;
                        }
                    } else if ui.button("Remove…").clicked() {
                        page.confirm_remove = Some(m.key.clone());
                    }
                });
            });
        ui.add_space(4.0);
    }
    // Downloads in progress, including models that are not installed yet.
    setup_view::downloads(ui, st);
}

pub fn hugging_face(ui: &mut Ui, st: &mut InferenceState) {
    muted(
        ui,
        format!(
            "Install a GGUF model from any Hugging Face repository. Only these architectures are accepted: {}. The file header is checked before the download starts, and files are verified against the checksums Hugging Face publishes. Scripts in repositories are never run.",
            st.catalog
                .as_ref()
                .map(|c| c.compatibility.architectures.join(", "))
                .unwrap_or_default()
        ),
    );
    let mut submit = false;
    ui.horizontal_wrapped(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut st.lookup.repo)
                .hint_text("owner/repository, e.g. Qwen/Qwen2.5-Coder-7B-Instruct-GGUF")
                .desired_width(360.0),
        );
        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add(
            egui::TextEdit::singleline(&mut st.lookup.revision)
                .hint_text("branch or commit (main)")
                .desired_width(160.0),
        );
        submit |= ui
            .add_enabled(
                !st.lookup.busy && !st.lookup.repo.trim().is_empty(),
                egui::Button::new("Look up"),
            )
            .clicked();
        if st.lookup.busy {
            ui.spinner();
        }
    });
    if submit && !st.lookup.busy && !st.lookup.repo.trim().is_empty() {
        st.look_up();
    }
    let info = match &st.lookup.result {
        None => return,
        Some(Err(e)) => {
            ui.label(RichText::new(e).color(theme::BAD));
            return;
        }
        Some(Ok(info)) => info.clone(),
    };
    muted(
        ui,
        format!(
            "{} at commit {}{}",
            info.repo,
            &info.revision[..12],
            if info.gated {
                " · gated: you must accept its conditions on huggingface.co and add a token below"
            } else {
                ""
            }
        ),
    );
    if st.lookup.options.is_empty() {
        let safetensors = info.files.iter().any(|f| f.path.ends_with(".safetensors"));
        ui.label(
            RichText::new(if safetensors {
                "This repository has no GGUF files (it holds safetensors weights). ClojureCraft runs GGUF models only; look for a repository whose name ends in -GGUF."
            } else {
                "This repository has no GGUF model files."
            })
            .color(theme::WARN),
        );
        return;
    }
    let budget = st.budget();
    let ctx = st.settings.effective_context(st.default_context(), None);
    for i in 0..st.lookup.options.len() {
        let option = st.lookup.options[i].clone();
        let inspection = st.lookup.inspections.get(i).cloned().flatten();
        let inspecting = st.lookup.inspecting.get(i).copied().unwrap_or(false);
        egui::Frame::new()
            .stroke(theme::hairline())
            .corner_radius(theme::RADIUS)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&option.quant).strong());
                    // The file name tells apart options with the same quantization (for example a
                    // model and a variant of it in a subfolder).
                    if let Some(first) = option.files.first() {
                        ui.label(RichText::new(&first.path).small().color(theme::MUTED));
                    }
                    muted(
                        ui,
                        format!(
                            "{}{}",
                            size(option.total_size()),
                            if option.is_split() {
                                format!(" in {} parts", option.expected_parts)
                            } else {
                                String::new()
                            }
                        ),
                    );
                    if inspection.is_none() {
                        if inspecting {
                            ui.spinner();
                            muted(ui, "Reading the file header…");
                        } else if ui
                            .button("Check")
                            .on_hover_text("Reads only the file header (a few MB).")
                            .clicked()
                        {
                            st.inspect_option(i);
                        }
                    }
                });
                match &inspection {
                    None => {}
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(theme::BAD));
                    }
                    Some(Ok(insp)) => inspected(ui, st, &info, insp, budget, ctx),
                }
            });
        ui.add_space(4.0);
    }
}

fn inspected(
    ui: &mut Ui,
    st: &mut InferenceState,
    info: &clojurecraft_runtime::hub::RepoInfo,
    insp: &Inspection,
    budget: Option<clojurecraft_hardware::MemoryBudget>,
    ctx: u32,
) {
    let f = &insp.facts;
    muted(
        ui,
        format!(
            "Architecture {} · {} layers · up to {} context",
            f.architecture.as_deref().unwrap_or("unknown"),
            f.block_count.map_or_else(|| "?".to_owned(), |n| n.to_string()),
            f.context_length
                .map_or_else(|| "unknown".to_owned(), |n| format!("{}k", n / 1024))
        ),
    );
    for p in &insp.problems {
        ui.label(RichText::new(p.to_string()).color(theme::BAD));
    }
    if !insp.is_installable() {
        return;
    }
    match (insp.estimate(ctx), budget) {
        (Some(est), Some(b)) => {
            let fit_text = match fit(&est, &b) {
                Fit::Full => ("fits the budget".to_owned(), theme::MUTED),
                Fit::PartialOffload => {
                    let plan = offload_plan(&est, f.block_count.unwrap_or(1), &b);
                    (
                        format!(
                            "partial offload: about {} of {} layers on the GPU",
                            plan.gpu_layers, plan.total_layers
                        ),
                        theme::WARN,
                    )
                }
                Fit::TooLarge => ("exceeds the budget; it may fail to load".to_owned(), theme::BAD),
            };
            ui.label(
                RichText::new(format!(
                    "Est. {} in use at {}k context · {}",
                    size(est.total()),
                    ctx / 1024,
                    fit_text.0
                ))
                .color(fit_text.1),
            );
        }
        _ => muted(ui, "Memory use cannot be estimated from this file's metadata."),
    }
    let Some(record) = ModelRecord::from_inspection(info, insp) else {
        return;
    };
    let installed = st.installed(&record.key).is_some();
    let queued = st.model_task(&record.key).is_some();
    if installed {
        ui.label(RichText::new("installed").small().color(theme::ACCENT_INK));
    } else if queued {
        muted(ui, "Download started; progress is shown under Installed models.");
    } else if ui
        .button(format!("Install ({})", size(record.total_size())))
        .clicked()
    {
        st.install_model(record);
    }
}

pub fn token(ui: &mut Ui, st: &mut InferenceState, page: &mut ModelsPage) {
    muted(
        ui,
        "Needed only for private or gated repositories. The token is kept in the system credential store, never in ClojureCraft's files or logs. ClojureCraft never accepts a model's license or access conditions for you: accept them on huggingface.co first.",
    );
    let status = match st.token_state {
        TokenState::Loading => "Checking the credential store…".to_owned(),
        TokenState::Saved => format!(
            "A token is saved in the {}.",
            st.store_name().unwrap_or_else(|| "credential store".into())
        ),
        TokenState::NotSaved => "No token is saved.".to_owned(),
        TokenState::SessionOnly => "A token is in use for this session only.".to_owned(),
    };
    ui.label(status);
    if let Some(p) = st.store_problem() {
        ui.label(RichText::new(p).small().color(theme::WARN));
    }
    if let Some(p) = &st.token_problem {
        ui.label(RichText::new(p).small().color(theme::WARN));
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut page.token_input)
                .password(true)
                .hint_text("hf_…")
                .desired_width(280.0),
        );
        if ui
            .add_enabled(
                !page.token_input.trim().is_empty(),
                egui::Button::new("Save token"),
            )
            .clicked()
        {
            st.save_token(&page.token_input);
            page.token_input.clear();
        }
        if st.token_present() && ui.button("Remove token").clicked() {
            st.remove_token();
        }
    });
}

pub fn storage(ui: &mut Ui, st: &mut InferenceState) {
    let Some(snapshot) = &st.snapshot else {
        ui.spinner();
        return;
    };
    let u = snapshot.usage.clone();
    let models: u64 = u.models.iter().map(|(_, b)| b).sum();
    let runtimes: u64 = u.runtimes.iter().map(|(_, b)| b).sum();
    let mut discard = false;
    let mut clear = false;
    egui::Grid::new("storage")
        .num_columns(3)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            muted(ui, "Models");
            ui.label(size(models));
            ui.label("");
            ui.end_row();
            muted(ui, "Runtimes");
            ui.label(size(runtimes));
            ui.label("");
            ui.end_row();
            muted(ui, "Downloaded runtime archives");
            ui.label(size(u.cached_archives));
            if u.cached_archives > 0 {
                clear = ui
                    .button("Delete")
                    .on_hover_text("Only needed to reinstall a runtime without downloading again.")
                    .clicked();
            } else {
                ui.label("");
            }
            ui.end_row();
            muted(ui, "Unfinished downloads");
            ui.label(size(u.partial_downloads));
            if u.partial_downloads > 0 {
                discard = ui
                    .button("Discard")
                    .on_hover_text("A later download would start from the beginning.")
                    .clicked();
            } else {
                ui.label("");
            }
            ui.end_row();
            muted(ui, "Free space");
            ui.label(u.free.map_or_else(|| "unknown".to_owned(), size));
            ui.label("");
            ui.end_row();
        });
    if let Some(dir) = st.dirs.as_ref().map(|d| d.root().display().to_string()) {
        muted(ui, format!("Data folder: {dir}"));
    }
    if discard {
        st.discard_partial_downloads();
    }
    if clear {
        st.clear_cached_archives();
    }
}
