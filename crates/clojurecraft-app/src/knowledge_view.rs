//! The knowledge sources page and the documentation result list shared with the right column.

use std::time::Duration;

use clojurecraft_knowledge::manifest::EmbeddingStatus;
use clojurecraft_knowledge::{Hit, Phase, Query, Semantic};
use egui::{RichText, Ui};

use crate::knowledge::{KnowledgeState, SearchState, Slot, Stage};
use crate::theme;

/// Typing pauses this long before a search starts.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(theme::MUTED));
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(18.0);
    ui.label(RichText::new(title).size(16.0));
    ui.add_space(4.0);
}

fn mib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let m = bytes as f64 / 1_048_576.0;
    if m >= 1024.0 {
        format!("{:.1} GiB", m / 1024.0)
    } else {
        format!("{m:.1} MiB")
    }
}

fn date(secs: u64) -> String {
    // Days since the epoch to a civil date (proleptic Gregorian calendar).
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn progress_bar(ui: &mut Ui, fraction: f32, text: String) {
    ui.add(
        egui::ProgressBar::new(fraction)
            .text(text)
            .desired_width(ui.available_width().min(520.0)),
    );
}

fn stage(ui: &mut Ui, stage: Option<&Stage>) {
    match stage {
        None => progress_bar(ui, 0.0, "Starting…".into()),
        Some(Stage::Runtime { file, progress }) => {
            #[allow(clippy::cast_precision_loss)]
            let f = progress.received as f32 / progress.total.max(1) as f32;
            progress_bar(
                ui,
                f,
                format!(
                    "Installing the Clojure runtime: {file} ({} of {})",
                    mib(progress.received),
                    mib(progress.total)
                ),
            );
        }
        Some(Stage::StartingEmbedder { gpu }) => progress_bar(
            ui,
            0.0,
            if *gpu {
                "Starting the embedding model on the GPU…".into()
            } else {
                "Starting the embedding model on the CPU…".into()
            },
        ),
        Some(Stage::Build(p)) => {
            #[allow(clippy::cast_precision_loss)]
            let f = p.done as f32 / p.total.max(1) as f32;
            let count = match p.phase {
                Phase::Api | Phase::Verifying => String::new(),
                _ => format!(" ({} of {})", p.done, p.total),
            };
            progress_bar(ui, f, format!("{}{count}", p.phase.label()));
        }
    }
}

/// Whether the build may use the GPU for embeddings (decided by the caller from the generator's
/// state).
pub struct PageContext {
    pub gpu_free: bool,
}

pub fn page(ui: &mut Ui, st: &mut KnowledgeState, cx: &PageContext) {
    crate::views::page_title(
        ui,
        "Knowledge sources",
        "Clojure documentation that grounds generated code. It is stored and searched on this \
         computer; searches never leave it.",
    );
    if let Some(problem) = st.setup_problem() {
        ui.label(RichText::new(problem).color(theme::BAD));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("knowledge")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(900.0));
            collection(ui, st, cx);
            sources(ui, st);
            semantic(ui, st);
            section(ui, "Browse the documentation");
            browser(ui, st);
            ui.add_space(24.0);
        });
}

fn collection(ui: &mut Ui, st: &mut KnowledgeState, cx: &PageContext) {
    section(ui, "Clojure documentation");
    let Some(snapshot) = &st.snapshot else {
        ui.spinner();
        return;
    };
    let with_embeddings = st.model_identity().is_some() && snapshot.llama;
    let clojure = st.runtime_clojure().unwrap_or("?").to_owned();
    let mut build = None;
    let mut cancel = false;
    let mut rollback = false;
    if let Some(job) = &st.job {
        stage(ui, job.stage.as_ref());
        cancel = ui.button("Cancel").clicked();
        ui.add_space(8.0);
    }
    let busy = st.job.is_some();
    match &snapshot.active {
        None => {
            muted(
                ui,
                format!(
                    "Not installed. Installing reads the API documentation of every supported namespace \
                     from the local Clojure {clojure} runtime and downloads selected clojure.org \
                     reference and guide pages (about 0.3 MB), pinned to a reviewed revision and \
                     verified. The Java runtime and Clojure are installed first if they are missing \
                     (about 60 MB)."
                ),
            );
            if !(snapshot.java && snapshot.clojure) {
                ui.label(
                    RichText::new("The Java runtime and Clojure will be installed first.")
                        .small()
                        .color(theme::WARN),
                );
            }
            ui.add_space(6.0);
            if ui
                .add_enabled(!busy, egui::Button::new("Install documentation"))
                .clicked()
            {
                build = Some(with_embeddings);
            }
            if !with_embeddings {
                ui.label(
                    RichText::new("Without the embedding model, search matches symbols and words only.")
                        .small()
                        .color(theme::MUTED),
                );
            }
        }
        Some(Err(e)) => {
            ui.label(RichText::new(e).color(theme::BAD));
            if ui.add_enabled(!busy, egui::Button::new("Rebuild")).clicked() {
                build = Some(with_embeddings);
            }
        }
        Some(Ok(active)) => {
            let i = &active.info;
            egui::Grid::new("collection")
                .num_columns(2)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    muted(ui, "Collection");
                    ui.label(&active.version);
                    ui.end_row();
                    muted(ui, "Clojure version");
                    ui.label(&i.clojure_version);
                    ui.end_row();
                    muted(ui, "Contents");
                    ui.label(format!(
                        "{} API entries, {} sections from {} pages",
                        i.api_entries, i.page_sections, i.pages
                    ));
                    ui.end_row();
                    muted(ui, "Semantic search");
                    match &i.embedding {
                        Some(e) => ui.label(format!("on ({})", e.name)),
                        None => ui.label(RichText::new("off (symbols and words only)").color(theme::MUTED)),
                    };
                    ui.end_row();
                    muted(ui, "Built");
                    ui.label(date(i.built_at));
                    ui.end_row();
                    muted(ui, "Size");
                    ui.label(mib(snapshot.collections_bytes));
                    ui.end_row();
                });
            let updates = st.updates();
            ui.add_space(6.0);
            if updates.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Up to date.").color(theme::ACCENT_INK));
                    if ui
                        .add_enabled(!busy, egui::Button::new("Rebuild"))
                        .on_hover_text("Builds the collection again from the pinned sources.")
                        .clicked()
                    {
                        build = Some(with_embeddings);
                    }
                });
            } else {
                ui.label(RichText::new("An update is available:").color(theme::WARN));
                for u in &updates {
                    ui.label(format!("• {}", u.describe()));
                }
                if ui.add_enabled(!busy, egui::Button::new("Update")).clicked() {
                    build = Some(with_embeddings);
                }
                muted(
                    ui,
                    "The current collection stays in use until the update is ready.",
                );
            }
        }
    }
    if let Some(prev) = &snapshot.previous
        && ui
            .add_enabled(
                !busy,
                egui::Button::new(format!("Use the previous collection ({})", prev.version)),
            )
            .clicked()
    {
        rollback = true;
    }
    if let Some(f) = st.failure.clone() {
        ui.add_space(6.0);
        egui::Frame::new()
            .stroke(egui::Stroke::new(1.0, theme::BAD))
            .corner_radius(theme::RADIUS)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Building the documentation collection failed").color(theme::BAD));
                ui.label(RichText::new(&f.message).small());
                if st.active().is_some() {
                    muted(ui, "The previous collection is still in use.");
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Try again")).clicked() {
                        build = Some(with_embeddings);
                    }
                    if f.embedding
                        && ui
                            .add_enabled(!busy, egui::Button::new("Build without semantic search"))
                            .clicked()
                    {
                        build = Some(false);
                    }
                });
            });
    }
    if let Some(n) = &st.notice {
        ui.label(RichText::new(n).color(theme::ACCENT_INK));
    }
    if let Some(embeddings) = build {
        st.build(embeddings, cx.gpu_free);
    }
    if cancel {
        st.cancel_build();
    }
    if rollback {
        st.rollback();
    }
}

fn sources(ui: &mut Ui, st: &KnowledgeState) {
    section(ui, "Sources and licenses");
    let from_index = st
        .snapshot
        .as_ref()
        .map(|s| s.sources.clone())
        .unwrap_or_default();
    if !from_index.is_empty() {
        for p in from_index {
            ui.label(RichText::new(&p.name).strong());
            muted(ui, format!("{} · {} · {}", p.license, p.revision, p.origin));
            ui.label(RichText::new(&p.attribution).small().color(theme::MUTED));
            ui.add_space(6.0);
        }
    } else if let Some(m) = st.manifest() {
        ui.label(RichText::new(&m.api.name).strong());
        muted(
            ui,
            format!("{} · from the installed Clojure runtime", m.api.license),
        );
        ui.label(RichText::new(&m.site.name).strong());
        muted(
            ui,
            format!(
                "{} · {} pages from {} at {}",
                m.site.license,
                m.site.pages.len(),
                m.site.repository,
                &m.site.commit[..12]
            ),
        );
    }
}

fn semantic(ui: &mut Ui, st: &mut KnowledgeState) {
    section(ui, "Semantic search");
    let Some(m) = st.manifest().map(|m| m.embedding.clone()) else {
        return;
    };
    muted(
        ui,
        format!(
            "Optional. The {} embedding model ({}, {} download, {}) finds documentation by meaning, \
             not only by matching words. Searches run it on the CPU, using about 0.85 GiB of memory \
             (an estimate) until it has been idle for five minutes.",
            m.name,
            m.quant,
            mib(m.file.size),
            m.license
        ),
    );
    if m.status == EmbeddingStatus::Unverified {
        ui.label(
            RichText::new("Not yet validated with the pinned llama.cpp release on every platform.")
                .small()
                .color(theme::WARN),
        );
    }
    let Some(snapshot) = &st.snapshot else { return };
    if !snapshot.llama {
        ui.label(RichText::new("Needs the llama.cpp runtime, installed during setup.").color(theme::WARN));
    }
    ui.add_space(4.0);
    if let Some(task) = &st.model_task {
        let (f, text) = match task.progress {
            #[allow(clippy::cast_precision_loss)]
            Some(p) if p.total > 0 => (
                p.received as f32 / p.total as f32,
                format!("{} of {}", mib(p.received), mib(p.total)),
            ),
            _ => (0.0, "Starting…".to_owned()),
        };
        progress_bar(ui, f, text);
        if ui.button("Cancel").clicked() {
            st.cancel_model();
        }
    } else if let Some(record) = &snapshot.model {
        if snapshot.model_problems.is_empty() {
            ui.label(
                RichText::new(format!("Installed ({})", mib(snapshot.model_bytes))).color(theme::ACCENT_INK),
            );
        } else {
            for p in &snapshot.model_problems {
                ui.label(RichText::new(p.to_string()).color(theme::WARN));
            }
        }
        let damaged = !snapshot.model_problems.is_empty();
        let key = record.key.clone();
        ui.horizontal(|ui| {
            if damaged && ui.button("Repair").clicked() {
                st.install_model();
            }
            if ui.button("Remove").on_hover_text(key).clicked() {
                st.remove_model();
            }
        });
    } else if ui.button("Download the embedding model").clicked() {
        st.install_model();
    }
    if let Some(e) = &st.model_error {
        ui.label(RichText::new(e).color(theme::BAD));
        muted(ui, "Finished parts are kept; trying again resumes the download.");
    }
}

/// Runs the debounced search of a slot when its text changed.
pub fn maybe_search(ui: &Ui, st: &mut KnowledgeState, slot: Slot, query: Query) {
    let s = st.search_state(slot);
    if let Some(since) = s.dirty_since {
        let waited = since.elapsed();
        if waited >= SEARCH_DEBOUNCE {
            if query.text.trim().is_empty() {
                s.dirty_since = None;
                s.result = None;
            } else {
                st.search(slot, query);
            }
        } else {
            ui.ctx().request_repaint_after(SEARCH_DEBOUNCE - waited);
        }
    }
}

fn browser(ui: &mut Ui, st: &mut KnowledgeState) {
    if st.active().is_none() {
        muted(ui, "Install the documentation to search it.");
        return;
    }
    let s = st.search_state(Slot::Browser);
    let r = ui.add(
        egui::TextEdit::singleline(&mut s.query)
            .hint_text("A symbol such as group-by or str/join, or a description such as \"count each value\"")
            .desired_width(ui.available_width().min(640.0)),
    );
    let restricted = ui
        .checkbox(
            &mut s.include_restricted,
            "Include symbols generated functions may not use",
        )
        .changed();
    if r.changed() || restricted {
        s.dirty_since = Some(
            std::time::Instant::now()
                - if restricted {
                    SEARCH_DEBOUNCE
                } else {
                    Duration::ZERO
                },
        );
    }
    let mut query = Query::new(s.query.clone());
    query.include_restricted = s.include_restricted;
    query.limit = 20;
    query.clojure_version = st.active().map(|a| a.info.clojure_version.clone());
    maybe_search(ui, st, Slot::Browser, query);
    let s = st.search_state(Slot::Browser);
    results(ui, s, "browser");
}

/// The result list of a search field, with status line.
pub fn results(ui: &mut Ui, s: &mut SearchState, id: &str) {
    if s.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            muted(ui, "Searching…");
        });
    }
    match &s.result {
        None => {}
        Some(Err(e)) => {
            ui.label(RichText::new(e).color(theme::BAD));
        }
        Some(Ok(out)) => {
            let how = match &out.semantic {
                Semantic::Used => "by symbol, words, and meaning".to_owned(),
                Semantic::NotIndexed => "by symbol and words (the collection has no embeddings)".to_owned(),
                Semantic::Unavailable => {
                    "by symbol and words (the embedding model is not installed)".to_owned()
                }
                Semantic::ModelMismatch => {
                    "by symbol and words (the collection was built with another embedding model; update it)"
                        .to_owned()
                }
                Semantic::Failed(why) => format!("by symbol and words (semantic search failed: {why})"),
            };
            ui.label(
                RichText::new(format!(
                    "{} results {how}, {:.0} ms · Clojure {}",
                    out.hits.len(),
                    out.elapsed.as_secs_f64() * 1000.0,
                    out.info.clojure_version
                ))
                .small()
                .color(theme::MUTED),
            );
            if out.hits.is_empty() {
                muted(ui, "Nothing found.");
            }
            ui.add_space(4.0);
            for h in &out.hits {
                hit(ui, h, &mut s.expanded, id);
                ui.add_space(4.0);
            }
        }
    }
}

fn how_found(h: &Hit) -> String {
    let mut parts = Vec::new();
    if h.signals.exact {
        parts.push("named in the query".to_owned());
    }
    if let Some((_, sim)) = h.signals.semantic {
        parts.push(format!("similar meaning ({sim:.2})"));
    }
    if h.signals.text_rank.is_some() {
        parts.push("matching words".to_owned());
    }
    parts.join(" · ")
}

fn hit(ui: &mut Ui, h: &Hit, expanded: &mut Option<String>, id: &str) {
    let open = expanded.as_deref() == Some(h.chunk.id.as_str());
    egui::Frame::new()
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let toggle = ui
                    .add(egui::Button::new(RichText::new(&h.chunk.title).strong()).frame(false))
                    .on_hover_text(if open { "Hide the text" } else { "Show the text" });
                if toggle.clicked() {
                    *expanded = if open { None } else { Some(h.chunk.id.clone()) };
                }
                ui.label(RichText::new(h.chunk.kind.label()).small().color(theme::MUTED));
                if let Some(since) = &h.chunk.since {
                    ui.label(
                        RichText::new(format!("since {since}"))
                            .small()
                            .color(theme::MUTED),
                    );
                }
                if h.chunk.restricted {
                    ui.label(
                        RichText::new("not available in functions")
                            .small()
                            .color(theme::WARN),
                    );
                }
            });
            let found = how_found(h);
            if !found.is_empty() {
                ui.label(RichText::new(found).small().color(theme::MUTED));
            }
            if !open {
                let first = h
                    .chunk
                    .text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(first).monospace().color(theme::MUTED)).truncate());
                return;
            }
            ui.add_space(4.0);
            egui::Frame::new()
                .fill(theme::SUNKEN)
                .corner_radius(theme::RADIUS)
                .inner_margin(6.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt((id, &h.chunk.id))
                        .max_height(360.0)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&h.chunk.text).monospace()).wrap());
                        });
                });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.hyperlink_to(RichText::new(&h.chunk.url).small(), &h.chunk.url);
                if ui.small_button("Copy link").clicked() {
                    ui.ctx().copy_text(h.chunk.url.clone());
                }
            });
            ui.label(
                RichText::new(format!(
                    "{} · {} · {}",
                    h.provenance.name, h.provenance.license, h.provenance.revision
                ))
                .small()
                .color(theme::MUTED),
            );
            ui.label(
                RichText::new(&h.provenance.attribution)
                    .small()
                    .color(theme::MUTED),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::date;

    #[test]
    fn dates_are_civil() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_783_296_000), "2026-07-06");
    }
}
