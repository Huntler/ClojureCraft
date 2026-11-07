//! Reviewing code: line differences, the editable implementation with its accepted version, and
//! the revision history with comparison and restoration.

use clojurecraft_domain::{Component, FunctionDocument, RevisionKind};
use clojurecraft_workflow::diff::{self, CodeDiff, LineKind};
use egui::{Color32, Id, RichText, TextStyle, Ui};

use crate::edn_view;
use crate::review::{self, ReviewAction, ReviewState};
use crate::theme;
use crate::validation_view;

const REMOVED_FILL: Color32 = Color32::from_rgba_premultiplied(60, 18, 18, 120);
const ADDED_FILL: Color32 = Color32::from_rgba_premultiplied(16, 48, 28, 120);

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

/// Line differences as hunks of numbered, coloured lines.
pub fn diff_view(ui: &mut Ui, id: Id, d: &CodeDiff, max_height: f32) {
    if d.is_empty() {
        muted(ui, "The code is identical.");
        return;
    }
    let font = TextStyle::Monospace.resolve(ui.style());
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::both()
                .id_salt(id)
                .max_height(max_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, hunk) in d.hunks.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(6.0);
                        }
                        ui.label(
                            RichText::new(hunk.header())
                                .font(font.clone())
                                .color(theme::MUTED),
                        );
                        for line in &hunk.lines {
                            let (sign, fill, color) = match line.kind {
                                LineKind::Unchanged => (' ', Color32::TRANSPARENT, theme::MUTED),
                                LineKind::Removed => ('-', REMOVED_FILL, theme::BAD),
                                LineKind::Added => ('+', ADDED_FILL, theme::ACCENT_INK),
                            };
                            let number =
                                |n: Option<usize>| n.map_or_else(|| "    ".to_owned(), |n| format!("{n:>4}"));
                            egui::Frame::new().fill(fill).show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.label(
                                    RichText::new(format!(
                                        "{} {} {sign} {}",
                                        number(line.old),
                                        number(line.new),
                                        line.text
                                    ))
                                    .font(font.clone())
                                    .color(color),
                                );
                            });
                        }
                    }
                });
        });
}

/// The changes `new` would make to the current implementation, for reviewing held code.
pub fn replacement(ui: &mut Ui, doc: &FunctionDocument, new: &str) {
    let current = doc.implementation.as_ref().map_or("", |i| i.source.as_str());
    let d = diff::diff(current, new);
    let title = if doc.implementation.is_none() {
        format!("New code ({})", d.summary())
    } else {
        format!("Changes to the current code ({})", d.summary())
    };
    egui::CollapsingHeader::new(title)
        .id_salt(("replacement", doc.id))
        .default_open(true)
        .show(ui, |ui| {
            diff_view(ui, Id::unique(("replacement-diff", doc.id)), &d, 320.0)
        });
}

/// A plain code editor with syntax colouring; returns whether the text changed.
fn code_editor(ui: &mut Ui, id: Id, text: &mut String) -> bool {
    let font = TextStyle::Monospace.resolve(ui.style());
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let job = edn_view::highlight_job(buf.as_str(), font.clone(), &[], f32::INFINITY);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let mut changed = false;
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(6.0)
        .show(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt(id.with("scroll"))
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    changed = egui::TextEdit::multiline(text)
                        .id(id)
                        .code_editor()
                        // Tab moves focus on; the editor is not a keyboard trap.
                        .lock_focus(false)
                        .desired_width(f32::INFINITY)
                        .desired_rows(8)
                        .layouter(&mut layouter)
                        .show(ui)
                        .response
                        .changed();
                });
        });
    changed
}

/// The implementation's code: read-only with an edit action, or the editor while editing, and
/// the changes since the accepted version.
pub fn code(
    ui: &mut Ui,
    doc: &FunctionDocument,
    review: &mut ReviewState,
    action: &mut Option<ReviewAction>,
) {
    let editing = review.code.as_mut().filter(|e| e.function == doc.id);
    match editing {
        Some(editor) => {
            ui.label(
                RichText::new(
                    "Editing by hand. Changes are saved as you type; earlier checks no longer describe \
                     the code until they are run again.",
                )
                .small()
                .color(theme::WARN),
            );
            if code_editor(ui, Id::unique(("code-editor", doc.id)), &mut editor.text) {
                *action = Some(ReviewAction::CodeChanged(editor.text.clone()));
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button("Done")
                    .on_hover_text("Closes the editor and records the edit in the history.")
                    .clicked()
                {
                    *action = Some(ReviewAction::FinishEditing);
                }
                if ui
                    .button("Cancel")
                    .on_hover_text("Puts back the code as it was before editing.")
                    .clicked()
                {
                    *action = Some(ReviewAction::CancelEditing);
                }
            });
        }
        None => {
            let Some(implementation) = &doc.implementation else {
                if ui
                    .button("Write it yourself")
                    .on_hover_text("Starts a hand-written implementation from a skeleton.")
                    .clicked()
                {
                    *action = Some(ReviewAction::EditCode);
                }
                return;
            };
            egui::Frame::new()
                .fill(theme::SUNKEN)
                .stroke(theme::hairline())
                .corner_radius(theme::RADIUS)
                .inner_margin(6.0)
                .show(ui, |ui| {
                    edn_view::code_view(ui, Id::unique(("implementation", doc.id)), &implementation.source);
                });
            ui.horizontal_wrapped(|ui| {
                let r = ui.button("Edit code").on_hover_text(
                    "Edit the implementation by hand. Generated code edited by hand is never replaced \
                     without your review.",
                );
                if r.clicked() {
                    *action = Some(ReviewAction::EditCode);
                }
            });
        }
    }
    accepted(ui, doc, action);
}

/// Whether the code is the accepted version, and the changes since if not.
fn accepted(ui: &mut Ui, doc: &FunctionDocument, action: &mut Option<ReviewAction>) {
    let Some(a) = &doc.accepted else { return };
    ui.add_space(6.0);
    if doc.is_accepted_code() {
        ui.label(
            RichText::new(format!(
                "This is the version accepted for edge-case testing ({}).",
                validation_view::ago(a.accepted_at)
            ))
            .small()
            .color(theme::ACCENT_INK),
        );
        return;
    }
    let current = doc.implementation.as_ref().map_or("", |i| i.source.as_str());
    let d = diff::diff(&a.source, current);
    ui.label(
        RichText::new(format!(
            "The code differs from the version accepted for edge-case testing {} ({}).",
            validation_view::ago(a.accepted_at),
            d.summary()
        ))
        .small()
        .color(theme::WARN),
    );
    egui::CollapsingHeader::new("Changes since the accepted version")
        .id_salt(("since-accepted", doc.id))
        .show(ui, |ui| {
            diff_view(ui, Id::unique(("since-accepted-diff", doc.id)), &d, 320.0);
            if ui
                .button("Put the accepted code back")
                .on_hover_text("Replaces the current code with the accepted version; the change is recorded in the history.")
                .clicked()
            {
                *action = Some(ReviewAction::RevertToAccepted);
            }
        });
}

/// The revision history: what each revision is, comparison with the current draft, and
/// restoration.
pub fn history(ui: &mut Ui, doc: &FunctionDocument, review: &ReviewState, action: &mut Option<ReviewAction>) {
    ui.add_space(6.0);
    let Some(revisions) = review.revisions.get(&doc.id) else {
        ui.horizontal(|ui| {
            ui.spinner();
            muted(ui, "Reading the history…");
        });
        return;
    };
    if revisions.is_empty() {
        muted(
            ui,
            "No revisions yet. Generating code, editing it, accepting it, and accepting edge cases \
             record revisions.",
        );
        return;
    }
    let content = doc.content_fingerprint();
    if let Some(a) = review::latest_accepted(revisions)
        && a.content != content
    {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("Revision {} is the latest accepted version.", a.number)).small());
            if ui.small_button("Compare").clicked() {
                *action = Some(ReviewAction::Compare(Some(a.id)));
            }
        });
        ui.add_space(4.0);
    }
    muted(
        ui,
        "Restoring never deletes history: the current draft is recorded first, then the restored \
         content becomes the newest revision.",
    );
    ui.add_space(4.0);
    let selected = review.compare.get(&doc.id).copied();
    for r in revisions {
        let current = r.content == content;
        let mut text = format!("{} · {}", r.number, review::kind_label(&r.reason.kind));
        if !r.reason.note.is_empty() && r.reason.kind != RevisionKind::Accepted {
            text.push_str(&format!(" · {}", r.reason.note));
        }
        ui.horizontal_wrapped(|ui| {
            let mut label = RichText::new(text);
            if r.reason.kind == RevisionKind::Accepted {
                label = label.color(theme::ACCENT_INK);
            }
            if ui.selectable_label(selected == Some(r.id), label).clicked() {
                *action = Some(ReviewAction::Compare(if selected == Some(r.id) {
                    None
                } else {
                    Some(r.id)
                }));
            }
            muted(ui, validation_view::ago(r.created_at));
            if current {
                ui.label(RichText::new("current").small().color(theme::ACCENT_INK));
            }
        });
        if selected == Some(r.id) {
            comparison(ui, doc, review, r.id, action);
        }
    }
}

fn comparison(
    ui: &mut Ui,
    doc: &FunctionDocument,
    review: &ReviewState,
    id: clojurecraft_domain::RevisionId,
    action: &mut Option<ReviewAction>,
) {
    egui::Frame::new()
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let Some(revision) = review.loaded.get(&id) else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    muted(ui, "Reading the revision…");
                });
                return;
            };
            let old = &revision.document;
            let changed: Vec<&str> = match revision.summary.basis.freshness(&doc.basis()) {
                clojurecraft_domain::Freshness::Current => Vec::new(),
                clojurecraft_domain::Freshness::Stale(c) => c
                    .iter()
                    .map(|c| match c {
                        Component::Specification => "specification",
                        Component::Code => "code",
                        Component::Tests => "accepted tests",
                        Component::BenchmarkConfiguration => "benchmarks",
                    })
                    .collect(),
            };
            if changed.is_empty() {
                muted(
                    ui,
                    "The specification, code, and accepted tests are the same as now; only names, \
                     proposals, or notes differ.",
                );
            } else {
                ui.label(
                    RichText::new(format!(
                        "Differs from the current draft in: {}.",
                        changed.join(", ")
                    ))
                    .small(),
                );
            }
            let old_code = old.implementation.as_ref().map_or("", |i| i.source.as_str());
            let new_code = doc.implementation.as_ref().map_or("", |i| i.source.as_str());
            let d = diff::diff(old_code, new_code);
            ui.label(
                RichText::new(format!(
                    "Code from revision {} to the current draft: {}",
                    revision.summary.number,
                    d.summary()
                ))
                .small()
                .color(theme::MUTED),
            );
            diff_view(ui, Id::unique(("history-diff", id)), &d, 280.0);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(format!("Restore revision {}", revision.summary.number))
                    .on_hover_text(
                        "Restores the specification, accepted tests, and code of this revision. The \
                         current draft is recorded in the history first.",
                    )
                    .clicked()
                {
                    *action = Some(ReviewAction::Restore(id));
                }
                if old.implementation.is_some()
                    && old.implementation != doc.implementation
                    && ui
                        .button("Use only its code")
                        .on_hover_text("Keeps the current specification and tests.")
                        .clicked()
                {
                    *action = Some(ReviewAction::RestoreCode(id));
                }
            });
        });
}
