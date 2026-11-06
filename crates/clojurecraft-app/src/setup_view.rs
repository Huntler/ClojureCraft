//! First-run setup, also reachable from settings: hardware, backend, the llama.cpp runtime, and
//! coding models. The same sections appear in settings.

use clojurecraft_hardware::{
    Backend, BudgetBasis, DetectionSource, GIB, GpuVendor, HardwareReport, ManualGpu,
};
use clojurecraft_runtime::catalog::{Candidate, Fit, Status};
use clojurecraft_runtime::models::ModelRecord;
use clojurecraft_runtime::settings::{BudgetOverride, SetupState};
use egui::{RichText, Ui};

use crate::inference::{InferenceState, Missing, TaskState};
use crate::theme;

/// Sizes for display: MiB below one GiB, GiB above.
pub fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes < GIB {
        format!("{:.0} MiB", b / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GiB", b / GIB as f64)
    }
}

fn gib(bytes: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let g = bytes as f64 / GIB as f64;
    g
}

fn bytes_of(gib: f64) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let b = (gib.max(0.0) * GIB as f64) as u64;
    b
}

pub fn heading(ui: &mut Ui, number: Option<usize>, title: &str, done: Option<bool>) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        if let Some(n) = number {
            ui.label(RichText::new(format!("{n}")).size(16.0).color(theme::MUTED));
        }
        ui.label(RichText::new(title).size(16.0));
        match done {
            Some(true) => ui.label(RichText::new("done").small().color(theme::ACCENT_INK)),
            Some(false) => ui.label(RichText::new("needed").small().color(theme::WARN)),
            None => ui.label(""),
        };
    });
    ui.add_space(4.0);
}

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(theme::MUTED));
}

fn source_label(s: &DetectionSource) -> &'static str {
    match s {
        DetectionSource::NvidiaSmi => "nvidia-smi",
        DetectionSource::Sysfs => "sysfs",
        DetectionSource::Sysctl => "sysctl",
        DetectionSource::Manual => "entered manually",
    }
}

/// Detected hardware with manual corrections and the resulting memory budget.
pub fn hardware(ui: &mut Ui, st: &mut InferenceState) {
    let Some(report) = st.report() else {
        ui.horizontal(|ui| {
            ui.spinner();
            muted(ui, "Detecting hardware…");
        });
        return;
    };
    summary(ui, &report);
    if let Some(Some(Ok(devices))) = st.snapshot.as_ref().map(|s| s.devices.clone())
        && !devices.is_empty()
    {
        muted(ui, format!("Seen by llama.cpp: {}", devices.join("; ")));
    }
    let mut changed = false;
    egui::CollapsingHeader::new("Correct detected values")
        .id_salt("hardware-corrections")
        .default_open(report.total_ram_bytes.is_none())
        .show(ui, |ui| {
            muted(
                ui,
                "Detection can miss or misreport memory (virtual machines, unusual drivers). Values entered here replace it.",
            );
            let detected_ram = st.detected.as_ref().and_then(|d| d.total_ram_bytes);
            ui.horizontal(|ui| {
                ui.label("System memory");
                let mut on = st.settings.hardware.total_ram_bytes.is_some();
                if ui.checkbox(&mut on, "override").changed() {
                    st.settings.hardware.total_ram_bytes = on.then(|| detected_ram.unwrap_or(16 * GIB));
                    changed = true;
                }
                if let Some(ram) = &mut st.settings.hardware.total_ram_bytes {
                    let mut g = gib(*ram);
                    if ui
                        .add(egui::DragValue::new(&mut g).range(1.0..=2048.0).speed(0.5).suffix(" GiB").max_decimals(1))
                        .changed()
                    {
                        *ram = bytes_of(g);
                        changed = true;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("GPU memory");
                let mut on = st.settings.hardware.gpu.is_some();
                if ui.checkbox(&mut on, "override").changed() {
                    st.settings.hardware.gpu = on.then(|| {
                        let g = st.detected.as_ref().and_then(|d| {
                            d.gpus.iter().max_by_key(|g| g.vram_total_bytes.unwrap_or(0)).cloned()
                        });
                        ManualGpu {
                            vendor: g.as_ref().map_or(GpuVendor::Nvidia, |g| g.vendor),
                            vram_bytes: g.and_then(|g| g.vram_total_bytes).unwrap_or(8 * GIB),
                        }
                    });
                    changed = true;
                }
                if let Some(gpu) = &mut st.settings.hardware.gpu {
                    egui::ComboBox::from_id_salt("manual-gpu-vendor")
                        .selected_text(format!("{:?}", gpu.vendor))
                        .show_ui(ui, |ui| {
                            for v in [GpuVendor::Nvidia, GpuVendor::Amd, GpuVendor::Intel, GpuVendor::Apple, GpuVendor::Other] {
                                changed |= ui.selectable_value(&mut gpu.vendor, v, format!("{v:?}")).changed();
                            }
                        });
                    let mut g = gib(gpu.vram_bytes);
                    if ui
                        .add(egui::DragValue::new(&mut g).range(0.5..=512.0).speed(0.25).suffix(" GiB").max_decimals(1))
                        .changed()
                    {
                        gpu.vram_bytes = bytes_of(g);
                        changed = true;
                    }
                }
            });
        });
    budget(ui, st, &mut changed);
    if changed {
        st.save_settings();
    }
}

fn summary(ui: &mut Ui, r: &HardwareReport) {
    egui::Grid::new("hardware-summary")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            muted(ui, "Platform");
            ui.label(format!("{} {}", r.os, r.arch));
            ui.end_row();
            muted(ui, "Processor");
            let features: Vec<&str> = r
                .cpu
                .features
                .iter()
                .map(String::as_str)
                .filter(|f| matches!(*f, "avx2" | "avx512f" | "fma" | "neon" | "f16c"))
                .collect();
            ui.label(format!(
                "{}{} threads{}",
                r.cpu
                    .model
                    .as_deref()
                    .map(|m| format!("{m}, "))
                    .unwrap_or_default(),
                r.cpu.logical_cores,
                if features.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", features.join(", "))
                }
            ));
            ui.end_row();
            muted(ui, "Memory");
            ui.label(r.total_ram_bytes.map_or_else(|| "unknown".to_owned(), size));
            ui.end_row();
            if r.gpus.is_empty() {
                muted(ui, "GPU");
                ui.label("none detected");
                ui.end_row();
            }
            for g in &r.gpus {
                muted(ui, "GPU");
                ui.label(format!(
                    "{} — {}{} ({})",
                    g.name,
                    g.vram_total_bytes
                        .map_or_else(|| "memory unknown".to_owned(), size),
                    if g.unified_memory { ", unified" } else { "" },
                    source_label(&g.source)
                ));
                ui.end_row();
            }
        });
    for note in &r.notes {
        ui.label(RichText::new(note).small().color(theme::WARN));
    }
}

fn budget(ui: &mut Ui, st: &mut InferenceState, changed: &mut bool) {
    let (Some(derived), Some(effective)) = (st.derived_budget(), st.budget()) else {
        return;
    };
    let basis = match effective.basis {
        BudgetBasis::DedicatedVram => "GPU memory",
        BudgetBasis::UnifiedMemory => "unified memory",
        BudgetBasis::SystemRam => "system memory (CPU inference)",
        BudgetBasis::Fallback => "a placeholder, because memory could not be detected",
    };
    let spill = if effective.host_spill_bytes > 0 {
        format!(
            " + {} of system memory for partial offload",
            size(effective.host_spill_bytes)
        )
    } else {
        String::new()
    };
    ui.label(format!(
        "Inference memory budget: {} of {basis}{spill} (estimate).",
        size(effective.device_bytes)
    ));
    if effective.basis == BudgetBasis::Fallback {
        ui.label(
            RichText::new("Enter the system memory above so models can be matched to this computer.")
                .color(theme::WARN),
        );
    }
    egui::CollapsingHeader::new("Adjust the memory budget")
        .id_salt("budget-override")
        .show(ui, |ui| {
            muted(
                ui,
                "The budget covers model weights, the context cache, and compute buffers. Lower it if other programs need memory.",
            );
            let mut on = st.settings.budget.is_some();
            if ui.checkbox(&mut on, "Use my own budget").changed() {
                st.settings.budget = on.then_some(BudgetOverride {
                    device_bytes: derived.device_bytes,
                    host_spill_bytes: derived.host_spill_bytes,
                });
                *changed = true;
            }
            if let Some(o) = &mut st.settings.budget {
                ui.horizontal(|ui| {
                    ui.label("Device");
                    let mut g = gib(o.device_bytes);
                    if ui
                        .add(egui::DragValue::new(&mut g).range(0.5..=1024.0).speed(0.25).suffix(" GiB").max_decimals(1))
                        .changed()
                    {
                        o.device_bytes = bytes_of(g);
                        *changed = true;
                    }
                    ui.label("System memory for offload");
                    let mut g = gib(o.host_spill_bytes);
                    if ui
                        .add(egui::DragValue::new(&mut g).range(0.0..=1024.0).speed(0.25).suffix(" GiB").max_decimals(1))
                        .changed()
                    {
                        o.host_spill_bytes = bytes_of(g);
                        *changed = true;
                    }
                });
            }
        });
}

fn backend_blurb(b: Backend) -> &'static str {
    match b {
        Backend::Metal => "Apple Silicon GPU",
        Backend::Cuda => "NVIDIA GPUs; fastest on NVIDIA hardware",
        Backend::Rocm => "AMD GPUs supported by ROCm",
        Backend::Vulkan => "most GPUs (NVIDIA, AMD, Intel)",
        Backend::Cpu => "works everywhere; much slower than a GPU",
    }
}

/// Backend choice: recommended first, unsuitable ones marked.
pub fn backend(ui: &mut Ui, st: &mut InferenceState) {
    let recommended = st.recommended_backend();
    let suitable = st.suitable_backends();
    let mut chosen = st.backend();
    for b in st.offered_backends() {
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut chosen, b, b.label());
            muted(ui, backend_blurb(b));
            if b == recommended {
                ui.label(RichText::new("recommended").small().color(theme::ACCENT_INK));
            }
            if !suitable.contains(&b) {
                ui.label(
                    RichText::new("no matching GPU detected")
                        .small()
                        .color(theme::WARN),
                );
            }
            if st.installed_build(b).is_some() {
                ui.label(RichText::new("installed").small().color(theme::MUTED));
            }
        });
    }
    if recommended == Backend::Cpu {
        ui.label(
            RichText::new(
                "No supported GPU was detected, so inference runs on the CPU. If this computer has a GPU, correct the hardware values above.",
            )
            .color(theme::WARN),
        );
    }
    if chosen != st.backend() {
        st.settings.backend = Some(chosen);
        st.save_settings();
        // Switch to an already installed build of the chosen backend.
        if let Some(v) = st
            .installed_build(chosen)
            .filter(|b| !b.active)
            .map(|b| b.version.clone())
        {
            st.activate_build(&v);
        }
    }
}

fn progress_line(ui: &mut Ui, state: &TaskState) {
    match state {
        TaskState::Queued => muted(ui, "Waiting for the current download to finish…"),
        TaskState::Running { file, progress } => {
            let (fraction, text) = match progress {
                #[allow(clippy::cast_precision_loss)]
                Some(p) if p.total > 0 => (
                    p.received as f32 / p.total as f32,
                    format!("{file}: {} of {}", size(p.received), size(p.total)),
                ),
                _ => (0.0, "Starting…".to_owned()),
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .text(text)
                    .desired_width(ui.available_width().min(520.0)),
            );
        }
        TaskState::Failed(e) => {
            ui.label(RichText::new(e).color(theme::BAD));
        }
    }
}

/// The llama.cpp runtime for the chosen backend.
pub fn runtime(ui: &mut Ui, st: &mut InferenceState) {
    let backend = st.backend();
    let Some(manifest) = st.manifest.clone() else {
        ui.label(RichText::new("The runtime manifest could not be read.").color(theme::BAD));
        return;
    };
    let release = manifest.llama_cpp.release.clone();
    match st.active_build() {
        Some(b) => {
            let devices = match st.snapshot.as_ref().and_then(|s| s.devices.clone()) {
                Some(Ok(d)) if !d.is_empty() => format!(" · {}", d.join("; ")),
                Some(Ok(_)) => " · no GPU device in use".to_owned(),
                Some(Err(e)) => format!(" · check failed: {e}"),
                None => String::new(),
            };
            ui.label(format!(
                "Active: llama.cpp {} ({}){devices}",
                b.release,
                b.backend.map_or("unknown backend", Backend::label)
            ));
        }
        None => muted(ui, "Not installed yet."),
    }
    if let Some((task_backend, state)) = st.runtime_task.as_ref().map(|t| (t.backend, t.state.clone())) {
        let failed = matches!(state, TaskState::Failed(_));
        ui.label(format!(
            "Installing llama.cpp {release} for {}",
            task_backend.label()
        ));
        progress_line(ui, &state);
        ui.horizontal(|ui| {
            if failed {
                muted(
                    ui,
                    "Finished downloads are kept; trying again continues where it stopped.",
                );
                if ui.button("Try again").clicked() {
                    st.dismiss_runtime_error();
                    st.install_runtime(task_backend);
                }
                if ui.button("Dismiss").clicked() {
                    st.dismiss_runtime_error();
                }
            } else if ui.button("Cancel").clicked() {
                st.cancel_runtime();
            }
        });
        return;
    }
    let installed = st.installed_build(backend).is_some();
    if !installed {
        let download: u64 = manifest
            .llama_asset(&st.platform, backend)
            .map(|a| a.artifact.size + a.extra.iter().map(|e| e.artifact.size).sum::<u64>())
            .unwrap_or(0);
        if download == 0 {
            ui.label(
                RichText::new(format!(
                    "No pinned llama.cpp build exists for {} on this platform.",
                    backend.label()
                ))
                .color(theme::WARN),
            );
            return;
        }
        if ui
            .button(format!(
                "Download and install llama.cpp {release} for {} ({})",
                backend.label(),
                size(download)
            ))
            .on_hover_text(
                "Downloads the pinned upstream build, verifies its SHA-256, and tests it before use.",
            )
            .clicked()
        {
            st.install_runtime(backend);
        }
    }
}

/// Catalog entries ranked for the budget, with selection and downloads.
pub fn models(ui: &mut Ui, st: &mut InferenceState, show_all: &mut bool, selection: &mut Vec<String>) {
    let (Some(catalog), Some(budget)) = (st.catalog.clone(), st.budget()) else {
        ui.horizontal(|ui| {
            ui.spinner();
            muted(ui, "Waiting for hardware detection…");
        });
        return;
    };
    let ctx = st.settings.effective_context(catalog.default_context, None);
    let ranked = catalog.ranked(&budget, ctx);
    let recommended = catalog
        .recommended(&budget, ctx)
        .map(|c| (c.model.id.clone(), c.quant.name.clone()));
    muted(
        ui,
        format!(
            "Memory figures are estimates at a {}k-token context. Models are verified against pinned checksums after download.",
            ctx / 1024
        ),
    );
    let hidden = ranked.iter().filter(|c| c.fit == Fit::TooLarge).count();
    for c in ranked.iter().filter(|c| *show_all || c.fit != Fit::TooLarge) {
        let is_rec = recommended.as_ref() == Some(&(c.model.id.clone(), c.quant.name.clone()));
        model_row(ui, st, c, is_rec, selection);
    }
    if hidden > 0 {
        let label = if *show_all {
            "Hide models that exceed the budget".to_owned()
        } else {
            format!("Show {hidden} more that exceed the budget")
        };
        if ui.button(label).clicked() {
            *show_all = !*show_all;
        }
    }
    let records: Vec<ModelRecord> = ranked
        .iter()
        .map(|c| ModelRecord::from_catalog(c.model, c.quant))
        .filter(|r| selection.contains(&r.key))
        .collect();
    let total: u64 = records.iter().map(ModelRecord::total_size).sum();
    let free = st.snapshot.as_ref().and_then(|s| s.usage.free);
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let label = if records.is_empty() {
            "Select models to download".to_owned()
        } else {
            format!("Download {} selected ({})", records.len(), size(total))
        };
        if ui
            .add_enabled(!records.is_empty(), egui::Button::new(label))
            .clicked()
        {
            for r in records {
                st.install_model(r);
            }
            selection.clear();
        }
        if let Some(free) = free {
            let color = if total > free { theme::BAD } else { theme::MUTED };
            ui.label(RichText::new(format!("{} free", size(free))).color(color));
        }
    });
    downloads(ui, st);
}

/// Model downloads that are queued, running, or failed, with their controls.
pub fn downloads(ui: &mut Ui, st: &mut InferenceState) {
    let tasks: Vec<(String, String, TaskState)> = st
        .model_tasks
        .iter()
        .map(|t| {
            (
                t.record.key.clone(),
                format!("{} · {}", t.record.display_name, t.record.quant),
                t.state.clone(),
            )
        })
        .collect();
    for (key, name, state) in tasks {
        ui.horizontal_wrapped(|ui| {
            ui.label(name);
            let failed = matches!(state, TaskState::Failed(_));
            if failed
                && ui.button("Try again").clicked()
                && let Some(record) = st.model_task(&key).map(|t| t.record.clone())
            {
                st.install_model(record);
            }
            if ui.button(if failed { "Dismiss" } else { "Cancel" }).clicked() {
                st.cancel_model(&key);
            }
        });
        progress_line(ui, &state);
    }
}

fn model_row(
    ui: &mut Ui,
    st: &mut InferenceState,
    c: &Candidate<'_>,
    recommended: bool,
    selection: &mut Vec<String>,
) {
    let record = ModelRecord::from_catalog(c.model, c.quant);
    let key = record.key.clone();
    let installed = st.installed(&key).is_some();
    let task = st.model_task(&key).map(|t| t.state.clone());
    egui::Frame::new()
        .stroke(if recommended { egui::Stroke::new(1.5, theme::ACCENT) } else { theme::hairline() })
        .corner_radius(theme::RADIUS)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if !installed && task.is_none() {
                    let mut on = selection.contains(&key);
                    if ui.checkbox(&mut on, "").on_hover_text("Select for download").changed() {
                        if on {
                            selection.push(key.clone());
                        } else {
                            selection.retain(|k| *k != key);
                        }
                    }
                }
                ui.label(RichText::new(format!("{} · {}", c.model.name, c.quant.name)).strong());
                if recommended {
                    ui.label(RichText::new("Recommended").small().color(theme::ACCENT_INK));
                }
                let (fit, color) = match c.fit {
                    Fit::Full if st.backend() == Backend::Cpu => ("fits in memory", theme::MUTED),
                    Fit::Full => ("fits on the GPU", theme::MUTED),
                    Fit::PartialOffload => ("partial GPU offload", theme::WARN),
                    Fit::TooLarge => ("exceeds the budget", theme::BAD),
                };
                ui.label(RichText::new(fit).small().color(color));
                if c.model.status == Status::Unverified {
                    ui.label(RichText::new("not yet tested with this llama.cpp release").small().color(theme::WARN));
                }
                if installed {
                    ui.label(RichText::new("installed").small().color(theme::ACCENT_INK));
                }
            });
            muted(
                ui,
                format!(
                    "Est. {} in use · {} download · up to {}k context · {}",
                    size(c.estimate.total()),
                    size(c.quant.weights_bytes()),
                    c.model.max_context / 1024,
                    c.model.license
                ),
            );
            muted(ui, &c.model.summary);
            if let Some(note) = &c.model.license_note {
                ui.label(RichText::new(note).small().color(theme::WARN));
            }
            if c.fit == Fit::PartialOffload {
                muted(
                    ui,
                    format!(
                        "About {} of {} layers on the GPU, {} of weights in system memory (estimate); slower than a full fit.",
                        c.offload.gpu_layers,
                        c.offload.total_layers,
                        size(c.offload.host_bytes)
                    ),
                );
            }
            if c.fit == Fit::TooLarge {
                ui.label(
                    RichText::new("Needs more memory than the budget allows; it may fail to load or run very slowly.")
                        .small()
                        .color(theme::BAD),
                );
            }
            if let Some(state) = task {
                progress_line(ui, &state);
                ui.horizontal(|ui| match state {
                    TaskState::Failed(_) => {
                        if ui.button("Try again").clicked() {
                            st.install_model(record.clone());
                        }
                        if ui.button("Dismiss").clicked() {
                            st.cancel_model(&key);
                        }
                    }
                    _ => {
                        if ui.button("Cancel").clicked() {
                            st.cancel_model(&key);
                        }
                    }
                });
            }
        });
    ui.add_space(4.0);
}

pub enum SetupAction {
    None,
    Finished,
    Skipped,
}

/// UI state of the setup page.
#[derive(Default)]
pub struct SetupPage {
    pub show_all: bool,
    pub selection: Vec<String>,
}

pub fn page(ui: &mut Ui, st: &mut InferenceState, page: &mut SetupPage) -> SetupAction {
    let mut action = SetupAction::None;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Set up local inference").size(24.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("Skip for now")
                .on_hover_text(
                    "Specifications can be edited without a model; setup stays available in settings.",
                )
                .clicked()
            {
                action = SetupAction::Skipped;
            }
        });
    });
    muted(
        ui,
        "ClojureCraft generates code with a coding model that runs on this computer through llama.cpp. Nothing you write leaves this machine.",
    );
    let missing = st.missing();
    heading(
        ui,
        Some(1),
        "Hardware",
        st.report().map(|r| r.total_ram_bytes.is_some()),
    );
    hardware(ui, st);
    heading(ui, Some(2), "Inference backend", None);
    backend(ui, st);
    heading(
        ui,
        Some(3),
        "llama.cpp runtime",
        Some(!missing.contains(&Missing::Runtime)),
    );
    runtime(ui, st);
    heading(
        ui,
        Some(4),
        "Coding model",
        Some(!missing.contains(&Missing::Model)),
    );
    models(ui, st, &mut page.show_all, &mut page.selection);
    muted(
        ui,
        "Other GGUF models can be added from Hugging Face in Settings.",
    );
    ui.add_space(16.0);
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        let ready = st.snapshot.is_some() && missing.is_empty();
        if ui
            .add_enabled(ready, egui::Button::new("Finish setup"))
            .on_disabled_hover_text("Install the runtime and at least one model first, or skip setup.")
            .clicked()
        {
            action = SetupAction::Finished;
        }
        if !ready {
            for m in &missing {
                ui.label(RichText::new(m.describe()).small().color(theme::WARN));
            }
        }
        let downloading = st.runtime_task.is_some() || !st.model_tasks.is_empty();
        if downloading {
            muted(ui, "Downloads continue in the background while you work.");
        }
    });
    match action {
        SetupAction::Finished => st.set_setup(SetupState::Completed),
        SetupAction::Skipped => st.set_setup(SetupState::Skipped),
        SetupAction::None => {}
    }
    action
}
