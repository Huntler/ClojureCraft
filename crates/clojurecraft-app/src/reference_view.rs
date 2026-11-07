//! Example inputs taken from another function's output, in the example editor: choosing the
//! source (function, revision, example), its provenance and state, the recorded value with a
//! large inspector, and refreshing it from a newer revision.

use clojurecraft_domain::{Example, ExampleInput, FunctionDocument, FunctionId, RevisionPin};
use clojurecraft_store::FunctionSummary;
use clojurecraft_workflow::references::{SnapshotState, snapshot_state};
use egui::{Id, RichText, Ui};

use crate::edn_view::{self, EdnCache};
use crate::references::{Catalog, Choice, LastResult, ReferencesState, Request};
use crate::theme;
use crate::validation_view;
use crate::views;

/// What the example editor needs for referenced inputs.
pub struct ReferenceUi<'a> {
    pub state: &'a mut ReferencesState,
    /// Saved functions that can be referenced.
    pub functions: &'a [FunctionSummary],
    /// Why code cannot be run on this computer, if it cannot.
    pub unavailable: Option<&'a str>,
}

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

fn name_of(functions: &[FunctionSummary], id: FunctionId) -> String {
    functions.iter().find(|f| f.id == id).map_or_else(
        || "a function not in the library".into(),
        |f| views::display_name(&f.name).to_owned(),
    )
}

/// The choice between written input and another function's output. Returns whether the
/// document changed.
pub fn source_selector(
    ui: &mut Ui,
    function: FunctionId,
    example: &mut Example,
    cx: &mut ReferenceUi<'_>,
) -> bool {
    let key = (function, example.id);
    let referenced = matches!(example.input, ExampleInput::FunctionOutput { .. });
    let choosing = cx.state.choices.contains_key(&key);
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Input").strong());
        ui.add_space(6.0);
        if ui
            .selectable_label(!referenced && !choosing, "Written EDN")
            .on_hover_text("Type the input as EDN.")
            .clicked()
        {
            cx.state.choices.remove(&key);
            // The recorded output stays as the written text, so nothing is lost.
            changed = example.use_direct_input();
        }
        if ui
            .selectable_label(referenced || choosing, "Output of another function")
            .on_hover_text(
                "Use what another saved function returns for one of its examples. The output is recorded \
                 and only changes when you refresh it.",
            )
            .clicked()
            && !referenced
            && !choosing
        {
            cx.state.choices.insert(key, Choice::default());
        }
    });
    changed
}

/// The source picker, shown while a source is being chosen.
pub fn picker(ui: &mut Ui, doc_id: FunctionId, example: &Example, cx: &mut ReferenceUi<'_>) {
    let key = (doc_id, example.id);
    let Some(mut choice) = cx.state.choices.get(&key).cloned() else {
        return;
    };
    let mut close = false;
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Take the input from another function").strong());
            muted(
                ui,
                "The chosen example is run in the sandbox with the chosen revision's code; its output \
                 becomes this example's input. Nothing is added to this function's code.",
            );
            ui.add_space(4.0);
            let candidates: Vec<&FunctionSummary> = cx.functions.iter().filter(|f| f.id != doc_id).collect();
            let selected_name = choice
                .function
                .map_or_else(|| "Choose a function".to_owned(), |f| name_of(cx.functions, f));
            egui::ComboBox::from_id_salt(Id::unique(("ref-function", example.id)))
                .selected_text(selected_name)
                .width(ui.available_width().min(320.0))
                .show_ui(ui, |ui| {
                    for f in &candidates {
                        if ui
                            .selectable_label(choice.function == Some(f.id), views::display_name(&f.name))
                            .clicked()
                            && choice.function != Some(f.id)
                        {
                            choice = Choice {
                                function: Some(f.id),
                                pin: Some(RevisionPin::Latest),
                                example: None,
                            };
                        }
                    }
                });
            if candidates.is_empty() {
                muted(ui, "There are no other saved functions yet.");
            }
            if let Some(function) = choice.function {
                match cx.state.catalog(function).cloned() {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            muted(ui, "Reading its revisions…");
                        });
                    }
                    Some(Err(e)) => {
                        ui.label(
                            RichText::new(format!("Its revisions could not be read: {e}")).color(theme::BAD),
                        );
                    }
                    Some(Ok(catalog)) => revision_and_example(ui, example, &catalog, &mut choice),
                }
            }
            ui.add_space(6.0);
            let resolving = cx.state.is_resolving(doc_id, example.id);
            ui.horizontal_wrapped(|ui| {
                if resolving {
                    ui.spinner();
                    ui.label("Running the upstream example…");
                    if ui.small_button("Cancel").clicked() {
                        cx.state.requests.push(Request::Cancel {
                            function: doc_id,
                            example: example.id,
                        });
                    }
                    return;
                }
                let ready = choice.reference().is_some() && cx.unavailable.is_none();
                let r = ui
                    .add_enabled(ready, egui::Button::new("Run and use this output"))
                    .on_hover_text("Runs the upstream example in the sandbox and records its output.");
                if let Some(why) = cx.unavailable {
                    r.on_disabled_hover_text(why);
                } else if r.clicked()
                    && let Some(source) = choice.reference()
                {
                    cx.state.requests.push(Request::Resolve {
                        function: doc_id,
                        example: example.id,
                        source,
                    });
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
            last_result(ui, cx.state.last_result(doc_id, example.id));
        });
    if close {
        cx.state.choices.remove(&key);
        if cx.state.is_resolving(doc_id, example.id) {
            cx.state.requests.push(Request::Cancel {
                function: doc_id,
                example: example.id,
            });
        }
    } else {
        cx.state.choices.insert(key, choice);
    }
}

fn revision_and_example(ui: &mut Ui, example: &Example, catalog: &Catalog, choice: &mut Choice) {
    if catalog.revisions.is_empty() {
        ui.label(
            RichText::new(
                "This function has no saved revision yet. Its output can be used once it has one (for \
                 example after code is generated, or after saving with the save shortcut).",
            )
            .color(theme::WARN),
        );
        return;
    }
    let pin = choice.pin.unwrap_or(RevisionPin::Latest);
    let latest = &catalog.revisions[0];
    let pin_text = |pin: RevisionPin| match pin {
        RevisionPin::Latest => format!("Latest revision (now {})", latest.number),
        RevisionPin::Pinned(id) => catalog.revision(id).map_or_else(
            || "A revision that no longer exists".into(),
            |r| {
                format!(
                    "Revision {} only · {} · {}",
                    r.number,
                    r.reason,
                    validation_view::ago(r.created_at)
                )
            },
        ),
    };
    ui.horizontal_wrapped(|ui| {
        muted(ui, "Revision");
        egui::ComboBox::from_id_salt(Id::unique(("ref-revision", example.id)))
            .selected_text(pin_text(pin))
            .width(ui.available_width().min(320.0))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(pin == RevisionPin::Latest, pin_text(RevisionPin::Latest))
                    .on_hover_text("Refreshing later uses the newest revision at that time.")
                    .clicked()
                {
                    choice.pin = Some(RevisionPin::Latest);
                }
                for r in &catalog.revisions {
                    let p = RevisionPin::Pinned(r.id);
                    if ui
                        .selectable_label(pin == p, pin_text(p))
                        .on_hover_text("Always use this revision.")
                        .clicked()
                    {
                        choice.pin = Some(p);
                    }
                }
            });
    });
    let Some(revision) = catalog.resolve(pin) else {
        return;
    };
    if !revision.has_implementation {
        ui.label(
            RichText::new(format!(
                "Revision {} has no implementation to run.",
                revision.number
            ))
            .color(theme::WARN),
        );
    }
    if choice
        .example
        .is_some_and(|e| !revision.examples.iter().any(|x| x.id == e))
    {
        choice.example = None;
    }
    ui.horizontal_wrapped(|ui| {
        muted(ui, "Example");
        let selected = choice
            .example
            .and_then(|e| revision.examples.iter().find(|x| x.id == e))
            .map_or_else(|| "Choose an example".to_owned(), |x| x.label.clone());
        egui::ComboBox::from_id_salt(Id::unique(("ref-example", example.id)))
            .selected_text(selected)
            .width(ui.available_width().min(320.0))
            .show_ui(ui, |ui| {
                for x in &revision.examples {
                    if ui
                        .selectable_label(choice.example == Some(x.id), &x.label)
                        .clicked()
                    {
                        choice.example = Some(x.id);
                    }
                }
            });
    });
    if revision.examples.is_empty() {
        muted(ui, "This revision has no examples.");
    }
}

fn last_result(ui: &mut Ui, result: Option<&LastResult>) {
    match result {
        Some(LastResult::Failed(e)) => {
            ui.label(RichText::new(e).color(theme::BAD));
        }
        Some(LastResult::Recorded { notes }) => {
            for n in notes {
                ui.label(RichText::new(n).small().color(theme::WARN));
            }
        }
        None => {}
    }
}

/// A referenced input: provenance, state, actions, and the recorded value. Returns whether the
/// document changed.
pub fn referenced_input(
    ui: &mut Ui,
    doc_id: FunctionId,
    example: &mut Example,
    cache: &mut EdnCache,
    cx: &mut ReferenceUi<'_>,
) -> bool {
    let ExampleInput::FunctionOutput { source, snapshot } = &example.input else {
        return false;
    };
    let (source, snapshot) = (source.clone(), snapshot.clone());
    let check = cx.state.check(doc_id, example.id).cloned();
    let upstream_name = check.as_ref().and_then(|c| c.upstream_name.clone()).map_or_else(
        || name_of(cx.functions, source.function),
        |n| views::display_name(&n).to_owned(),
    );
    let upstream_example = match cx.state.catalog(source.function) {
        Some(Ok(catalog)) => catalog
            .revisions
            .iter()
            .find_map(|r| r.examples.iter().find(|e| e.id == source.example))
            .map(|e| e.label.clone()),
        _ => None,
    }
    .unwrap_or_else(|| "an example".into());
    let latest = check.as_ref().and_then(|c| c.latest);
    let pin = match source.revision {
        RevisionPin::Latest => "latest revision".to_owned(),
        RevisionPin::Pinned(_) => "pinned revision".to_owned(),
    };
    let recorded_number = check.as_ref().and_then(|c| c.snapshot_number);
    let mut changed = false;
    egui::Frame::new()
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(format!("From “{upstream_name}”, {upstream_example} · follows the {pin}"));
            if let Some(s) = &snapshot {
                muted(
                    ui,
                    format!(
                        "Output of revision {} recorded {}.",
                        recorded_number.map_or_else(|| "(no longer in its history)".into(), |n| n.to_string()),
                        validation_view::ago(s.resolved_at)
                    ),
                );
            }
            if let Some(c) = &check {
                for p in &c.problems {
                    ui.label(RichText::new(p).color(theme::BAD));
                }
            }
            let state = snapshot_state(&example.input, latest.map(|(id, _)| id));
            let mut refresh_label = "Run upstream example";
            match state {
                Some(SnapshotState::NotRecorded) => {
                    ui.label(
                        RichText::new("No output has been recorded yet, so this example is not used.")
                            .color(theme::WARN),
                    );
                }
                Some(SnapshotState::NewerRevision { .. }) => {
                    let n = latest.map(|(_, n)| n.to_string()).unwrap_or_default();
                    ui.label(
                        RichText::new(format!(
                            "Revision {n} of “{upstream_name}” is newer than the recorded output. This example \
                             keeps using the recorded output until you refresh it; refreshing marks earlier \
                             checks of this function as outdated."
                        ))
                        .color(theme::WARN),
                    );
                    refresh_label = "Refresh from the latest revision";
                }
                Some(SnapshotState::Current) => {
                    muted(ui, "Up to date with the revision it follows.");
                    refresh_label = "Run again";
                }
                Some(SnapshotState::NoRevision) | None => {}
            }
            ui.horizontal_wrapped(|ui| {
                if cx.state.is_resolving(doc_id, example.id) {
                    ui.spinner();
                    ui.label("Running the upstream example…");
                    if ui.small_button("Cancel").clicked() {
                        cx.state.requests.push(Request::Cancel {
                            function: doc_id,
                            example: example.id,
                        });
                    }
                } else {
                    let r = ui.add_enabled(cx.unavailable.is_none(), egui::Button::new(refresh_label));
                    if let Some(why) = cx.unavailable {
                        r.on_disabled_hover_text(why);
                    } else if r
                        .on_hover_text("Runs the upstream example in the sandbox and records its output.")
                        .clicked()
                    {
                        cx.state.requests.push(Request::Resolve {
                            function: doc_id,
                            example: example.id,
                            source: source.clone(),
                        });
                    }
                }
                if ui.button("Change source…").clicked() {
                    cx.state.choices.insert(
                        (doc_id, example.id),
                        Choice {
                            function: Some(source.function),
                            pin: Some(source.revision),
                            example: Some(source.example),
                        },
                    );
                }
                if ui
                    .button("Use as written input")
                    .on_hover_text("Keeps the recorded value as text you can edit; the link is removed.")
                    .clicked()
                {
                    changed = example.use_direct_input();
                }
            });
            if !cx.state.choices.contains_key(&(doc_id, example.id)) {
                last_result(ui, cx.state.last_result(doc_id, example.id));
            }
        });
    if changed {
        return true;
    }
    if let Some(value) = &snapshot {
        let id = Id::unique(("ref-snapshot", example.id));
        let parsed = cache.parsed(id, value.value.as_str());
        ui.horizontal(|ui| {
            muted(ui, edn_view::abbreviate(&summary(&parsed), 60));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Open large view").clicked() {
                    cx.state.viewing = Some((doc_id, example.id));
                }
            });
        });
        egui::Frame::new()
            .fill(theme::SUNKEN)
            .stroke(theme::hairline())
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .id_salt(id.with("scroll"))
                    .max_height(260.0)
                    .show(ui, |ui| {
                        edn_view::inspector(ui, id.with("tree"), &parsed, value.value.as_str())
                    });
            });
    }
    false
}

fn summary(parsed: &clojurecraft_edn::Parsed) -> String {
    match parsed.values.first().map(|v| &v.kind) {
        Some(clojurecraft_edn::NodeKind::Map(e)) => format!("Recorded value: map · {} entries", e.len()),
        Some(
            clojurecraft_edn::NodeKind::Vector(items)
            | clojurecraft_edn::NodeKind::List(items)
            | clojurecraft_edn::NodeKind::Set(items),
        ) => format!(
            "Recorded value: {} · {} elements",
            parsed.values[0].type_name(),
            items.len()
        ),
        Some(_) => format!("Recorded value: {}", parsed.values[0].type_name()),
        None => "Recorded value".into(),
    }
}

/// The large, read-only view of a recorded input: its text beside the inspector.
pub fn large_view(
    ctx: &egui::Context,
    doc: &FunctionDocument,
    cache: &mut EdnCache,
    state: &mut ReferencesState,
) {
    let Some((function, example)) = state.viewing else {
        return;
    };
    if function != doc.id {
        return;
    }
    let Some(e) = doc.example(example) else {
        state.viewing = None;
        return;
    };
    let Some(snapshot) = e.input.snapshot() else {
        state.viewing = None;
        return;
    };
    let id = Id::unique(("ref-large", example));
    let text = snapshot.value.as_str();
    let parsed = cache.parsed(id, text);
    let screen = ctx.content_rect();
    let mut close = false;
    let response = egui::Modal::new(id.with("modal")).show(ctx, |ui| {
        ui.set_width((screen.width() - 80.0).max(400.0));
        let height = (screen.height() - 200.0).max(240.0);
        ui.horizontal(|ui| {
            ui.heading(format!("{} — recorded input", e.name.trim()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Done").on_hover_text("Close (Esc)").clicked() {
                    close = true;
                }
            });
        });
        muted(
            ui,
            "Read-only: this is the recorded output of another function. Use “Use as written input” to \
             edit it.",
        );
        ui.add_space(6.0);
        ui.allocate_ui(egui::vec2(ui.available_width(), height + 24.0), |ui| {
            ui.columns(2, |cols| {
                cols[0].label(RichText::new("Text").small().color(theme::MUTED));
                egui::ScrollArea::vertical()
                    .id_salt(id.with("text"))
                    .max_height(height)
                    .show(&mut cols[0], |ui| edn_view::code_view(ui, id.with("code"), text));
                cols[1].label(RichText::new("Inspector").small().color(theme::MUTED));
                egui::Frame::new()
                    .fill(theme::SUNKEN)
                    .stroke(theme::hairline())
                    .corner_radius(theme::RADIUS)
                    .inner_margin(6.0)
                    .show(&mut cols[1], |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt(id.with("tree-scroll"))
                            .max_height(height)
                            .auto_shrink([false, false])
                            .show(ui, |ui| edn_view::inspector(ui, id.with("tree"), &parsed, text));
                    });
            });
        });
    });
    if close || response.should_close() {
        state.viewing = None;
    }
}
