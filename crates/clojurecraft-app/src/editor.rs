//! The specification editor in the centre column: name, description, examples, contract,
//! ordered transformation steps, and the model's interpretation with its questions.

use clojurecraft_domain::{Contract, Example, ExampleId, ExampleInput, FunctionId, Step, StepId};
use egui::text::{CCursor, CCursorRange};
use egui::{Id, RichText, TextStyle, Ui};

use crate::edn_view::{self, EdnCache};
use crate::keys::{self, ADD_STEP_AFTER, MOVE_STEP_DOWN, MOVE_STEP_UP};
use crate::reference_view;
use crate::session::{OpenDocument, SaveStatus};
use crate::theme;
use clojurecraft_workflow::assistant::{JobKind, Phase, Report};

/// A keyboard focus the application wants to move to on the next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Search,
    /// The function name, with its text selected.
    Name,
    Step(StepId),
    ExampleName(ExampleId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleField {
    Input,
    Expected,
}

/// An example field shown in the large editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpandTarget {
    pub function: FunctionId,
    pub example: ExampleId,
    pub field: ExampleField,
}

/// Something removed that can be put back from a notification.
pub enum Undo {
    Function(FunctionId),
    Step {
        function: FunctionId,
        index: usize,
        step: Step,
    },
    Example {
        function: FunctionId,
        index: usize,
        example: Box<Example>,
    },
    Contract {
        function: FunctionId,
        contract: Box<Contract>,
    },
}

/// Requests from the editor that the application carries out.
pub enum Action {
    Duplicate,
    Trash,
    RetrySave,
    Removed { message: String, undo: Undo },
    Interpret,
    ProposeContract,
    CancelJob,
    DismissReport,
}

/// The model's availability and work for the open function.
pub struct AssistStatus {
    /// Why no model work can start right now.
    pub blocked: Option<String>,
    /// A job running for this function.
    pub job: Option<(JobKind, Phase)>,
    /// How the last job for this function ended.
    pub report: Option<Report>,
}

pub struct EditorContext<'a> {
    pub cache: &'a mut EdnCache,
    pub focus: &'a mut Option<Focus>,
    pub expanded: &'a mut Option<ExpandTarget>,
    pub actions: &'a mut Vec<Action>,
    pub assist: &'a AssistStatus,
    pub references: crate::reference_view::ReferenceUi<'a>,
}

pub fn name_id(function: FunctionId) -> Id {
    Id::unique(("function-name", function))
}

fn step_id(step: StepId) -> Id {
    Id::unique(("step", step))
}

/// Shows the editor; returns whether the document changed.
pub fn show(ui: &mut Ui, open: &mut OpenDocument, cx: &mut EditorContext<'_>) -> bool {
    let mut changed = false;
    let status = open.status();
    changed |= header(ui, open, &status, cx);
    if let Some(namespace) = &open.doc.namespace {
        ui.label(
            RichText::new(format!(
                "Namespace {namespace}: the name above is its main function"
            ))
            .small()
            .color(theme::MUTED),
        );
    }
    save_problem(ui, &status, cx);
    ui.add_space(14.0);

    section(ui, "Description", "What the function does, in your own words.");
    changed |= ui
        .add(
            egui::TextEdit::multiline(&mut open.doc.description)
                .id(Id::unique(("description", open.doc.id)))
                .hint_text("For example: group orders by customer and total their amounts")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        )
        .changed();
    ui.add_space(18.0);

    changed |= examples(ui, open, cx);
    ui.add_space(18.0);
    changed |= crate::contract_view::contract(ui, open, cx);
    ui.add_space(18.0);
    changed |= steps(ui, open, cx);
    ui.add_space(18.0);
    changed |= crate::interpretation_view::interpretation(ui, open, cx);
    ui.add_space(24.0);
    changed
}

pub fn section(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.label(RichText::new(title).text_style(TextStyle::Heading).size(16.0));
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).small().color(theme::MUTED));
    }
    ui.add_space(4.0);
}

fn header(ui: &mut Ui, open: &mut OpenDocument, status: &SaveStatus, cx: &mut EditorContext<'_>) -> bool {
    let id = name_id(open.doc.id);
    let mut changed = false;
    ui.horizontal(|ui| {
        let actions_width = 190.0;
        let output = egui::TextEdit::singleline(&mut open.doc.name)
            .id(id)
            .font(egui::FontId::proportional(22.0))
            .hint_text("Function name")
            .desired_width((ui.available_width() - actions_width).max(160.0))
            .show(ui);
        changed = output.response.changed();
        if *cx.focus == Some(Focus::Name) {
            *cx.focus = None;
            let mut state = output.state;
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(0),
                CCursor::end_of_str(&open.doc.name),
            )));
            state.store(ui.ctx(), id);
            output.response.request_focus();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("More…", |ui| {
                if ui.button("Duplicate").clicked() {
                    cx.actions.push(Action::Duplicate);
                }
                if ui.button("Move to trash").clicked() {
                    cx.actions.push(Action::Trash);
                }
            });
            save_chip(ui, status);
        });
    });
    if open.doc.name.trim().is_empty() {
        ui.label(
            RichText::new("A name is needed to find and export this function.")
                .small()
                .color(theme::WARN),
        );
    }
    changed
}

fn save_chip(ui: &mut Ui, status: &SaveStatus) {
    let (text, color, hover) = match status {
        SaveStatus::Saved => ("Saved", theme::MUTED, "All changes are saved on this computer."),
        SaveStatus::Pending => ("Saving…", theme::MUTED, "Changes are saved automatically."),
        SaveStatus::Retrying(_) => ("Not saved – retrying", theme::WARN, "Saving failed; retrying."),
        SaveStatus::Failed(_) => ("Not saved", theme::BAD, "Saving failed; see the message below."),
        SaveStatus::Conflict => ("Conflict", theme::BAD, "This function was changed elsewhere."),
    };
    ui.label(RichText::new(text).small().color(color))
        .on_hover_text(hover);
}

fn save_problem(ui: &mut Ui, status: &SaveStatus, cx: &mut EditorContext<'_>) {
    let (message, retry) = match status {
        SaveStatus::Retrying(e) => (
            format!("Changes are not saved yet: {e}. Retrying automatically."),
            false,
        ),
        SaveStatus::Failed(e) => (format!("Changes are not saved: {e}."), true),
        SaveStatus::Conflict => (
            "This function was changed by another ClojureCraft window. Your edits here are kept in \
             this window but were not saved; copy anything you need, then reopen the function."
                .to_owned(),
            false,
        ),
        SaveStatus::Saved | SaveStatus::Pending => return,
    };
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(theme::SUNKEN)
        .stroke(egui::Stroke::new(1.0, theme::BAD))
        .corner_radius(theme::RADIUS)
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(message).color(theme::BAD));
                if retry && ui.button("Try again").clicked() {
                    cx.actions.push(Action::RetrySave);
                }
            });
        });
}

fn examples(ui: &mut Ui, open: &mut OpenDocument, cx: &mut EditorContext<'_>) -> bool {
    let mut changed = false;
    let function = open.doc.id;
    section(
        ui,
        "Examples",
        "Inputs and the outputs you expect, written as EDN data.",
    );
    if open
        .view
        .selected_example
        .is_none_or(|id| open.doc.example(id).is_none())
    {
        open.view.selected_example = open.doc.examples.first().map(|e| e.id);
    }
    ui.horizontal_wrapped(|ui| {
        for (i, example) in open.doc.examples.iter().enumerate() {
            let selected = open.view.selected_example == Some(example.id);
            let label = example_label(example, i);
            if ui.selectable_label(selected, label).clicked() {
                open.view.selected_example = Some(example.id);
            }
        }
        if ui.button("+ Add example").clicked() {
            let n = open.doc.examples.len() + 1;
            let id = open.doc.add_example(format!("Example {n}"));
            open.view.selected_example = Some(id);
            *cx.focus = Some(Focus::ExampleName(id));
            changed = true;
        }
    });
    let Some(selected) = open.view.selected_example else {
        ui.label(
            RichText::new("No examples yet. Examples are how generated code is checked.").color(theme::MUTED),
        );
        return changed;
    };
    let mut remove = false;
    egui::Frame::new()
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(10.0)
        .show(ui, |ui| {
            let Some(example) = open.doc.example_mut(selected) else {
                return;
            };
            ui.horizontal(|ui| {
                let name_id = Id::unique(("example-name", selected));
                let r = ui.add(
                    egui::TextEdit::singleline(&mut example.name)
                        .id(name_id)
                        .hint_text("Example name")
                        .desired_width((ui.available_width() - 150.0).max(120.0)),
                );
                if *cx.focus == Some(Focus::ExampleName(selected)) {
                    *cx.focus = None;
                    r.request_focus();
                }
                changed |= r.changed();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Remove example").clicked() {
                        remove = true;
                    }
                });
            });
            ui.add_space(6.0);
            let wide = ui.available_width() >= 720.0;
            if wide {
                ui.columns(2, |cols| {
                    changed |= example_input(&mut cols[0], function, example, cx);
                    changed |= expected_ui(&mut cols[1], function, example, cx);
                });
            } else {
                changed |= example_input(ui, function, example, cx);
                ui.add_space(8.0);
                changed |= expected_ui(ui, function, example, cx);
            }
        });
    if remove && let Some((index, example)) = open.doc.remove_example(selected) {
        let label = example_label(&example, index);
        open.view.selected_example = open
            .doc
            .examples
            .get(index.min(open.doc.examples.len().saturating_sub(1)))
            .map(|e| e.id);
        cx.actions.push(Action::Removed {
            message: format!("Removed “{label}”"),
            undo: Undo::Example {
                function,
                index,
                example: Box::new(example),
            },
        });
        changed = true;
    }
    changed |= expanded(ui, open, cx);
    reference_view::large_view(ui.ctx(), &open.doc, cx.cache, cx.references.state);
    changed
}

fn example_label(example: &Example, index: usize) -> String {
    let name = example.name.trim();
    if name.is_empty() {
        format!("Example {}", index + 1)
    } else {
        edn_view::abbreviate(name, 32)
    }
}

fn field_id(example: ExampleId, field: ExampleField) -> Id {
    Id::unique(("example-field", example, field as u8))
}

fn example_input(
    ui: &mut Ui,
    function: FunctionId,
    example: &mut Example,
    cx: &mut EditorContext<'_>,
) -> bool {
    let mut changed = reference_view::source_selector(ui, function, example, &mut cx.references);
    if let ExampleInput::Direct { value } = &mut example.input {
        let out = edn_view::edn_field(
            ui,
            field_id(example.id, ExampleField::Input),
            "",
            "{:orders [{:id 1 :amount 10}]}",
            &mut value.0,
            cx.cache,
            260.0,
        );
        if out.expand {
            *cx.expanded = Some(ExpandTarget {
                function,
                example: example.id,
                field: ExampleField::Input,
            });
        }
        changed |= out.changed;
    } else {
        changed |= reference_view::referenced_input(ui, function, example, cx.cache, &mut cx.references);
    }
    reference_view::picker(ui, function, example, &mut cx.references);
    changed
}

fn expected_ui(ui: &mut Ui, function: FunctionId, example: &mut Example, cx: &mut EditorContext<'_>) -> bool {
    let out = edn_view::edn_field(
        ui,
        field_id(example.id, ExampleField::Expected),
        "Expected output",
        "{1 10}",
        &mut example.expected.0,
        cx.cache,
        260.0,
    );
    if out.expand {
        *cx.expanded = Some(ExpandTarget {
            function,
            example: example.id,
            field: ExampleField::Expected,
        });
    }
    out.changed
}

/// The large editor for the expanded example field, if it belongs to this document.
fn expanded(ui: &mut Ui, open: &mut OpenDocument, cx: &mut EditorContext<'_>) -> bool {
    let Some(target) = *cx.expanded else {
        return false;
    };
    if target.function != open.doc.id {
        return false;
    }
    let Some(example) = open.doc.example_mut(target.example) else {
        *cx.expanded = None;
        return false;
    };
    let name = example.name.clone();
    let text = match (target.field, &mut example.input) {
        (ExampleField::Input, ExampleInput::Direct { value }) => &mut value.0,
        (ExampleField::Expected, _) => &mut example.expected.0,
        (ExampleField::Input, ExampleInput::FunctionOutput { .. }) => {
            *cx.expanded = None;
            return false;
        }
    };
    let title = format!(
        "{} — {}",
        if name.trim().is_empty() {
            "Example"
        } else {
            name.trim()
        },
        match target.field {
            ExampleField::Input => "input",
            ExampleField::Expected => "expected output",
        }
    );
    let (changed, close) = edn_view::expanded_editor(
        ui.ctx(),
        field_id(target.example, target.field).with("expanded"),
        &title,
        text,
        cx.cache,
    );
    if close {
        *cx.expanded = None;
    }
    changed
}

enum StepEdit {
    Move(StepId, usize),
    Remove(StepId),
    AddAfter(Option<StepId>),
}

fn steps(ui: &mut Ui, open: &mut OpenDocument, cx: &mut EditorContext<'_>) -> bool {
    let function = open.doc.id;
    let mut changed = false;
    section(
        ui,
        "Transformation steps",
        &format!(
            "Ordered steps in plain language. {} and {} move the focused step; {} adds one after it.",
            keys::label(&MOVE_STEP_UP),
            keys::label(&MOVE_STEP_DOWN),
            keys::label(&ADD_STEP_AFTER)
        ),
    );
    let count = open.doc.steps.len();
    let mut edit = None;
    // Shortcuts are taken before the text fields see the keys.
    if let Some(focused) = ui.memory(|m| m.focused())
        && let Some(index) = open.doc.steps.iter().position(|s| step_id(s.id) == focused)
    {
        let id = open.doc.steps[index].id;
        if ui.input_mut(|i| i.consume_shortcut(&MOVE_STEP_UP)) && index > 0 {
            edit = Some(StepEdit::Move(id, index - 1));
        } else if ui.input_mut(|i| i.consume_shortcut(&MOVE_STEP_DOWN)) && index + 1 < count {
            edit = Some(StepEdit::Move(id, index + 1));
        } else if ui.input_mut(|i| i.consume_shortcut(&ADD_STEP_AFTER)) {
            edit = Some(StepEdit::AddAfter(Some(id)));
        }
    }
    for (index, step) in open.doc.steps.iter_mut().enumerate() {
        let id = step.id;
        ui.horizontal(|ui| {
            ui.add_sized(
                [26.0, theme::CONTROL_H],
                egui::Label::new(RichText::new(format!("{}.", index + 1)).color(theme::MUTED)),
            );
            // Exactly sized buttons keep every step's text field the same width; widgets are
            // created left to right so keyboard focus reaches the text before its buttons.
            let button = [72.0, theme::CONTROL_H];
            let buttons = 3.0 * (button[0] + ui.spacing().item_spacing.x);
            let r = ui.add(
                egui::TextEdit::multiline(&mut step.text)
                    .id(step_id(id))
                    .hint_text("Describe this step")
                    .desired_rows(1)
                    .desired_width((ui.available_width() - buttons).max(120.0)),
            );
            changed |= r.changed();
            if *cx.focus == Some(Focus::Step(id)) {
                *cx.focus = None;
                r.request_focus();
            }
            let up = ui.add_enabled_ui(index > 0, |ui| ui.add_sized(button, egui::Button::new("Up")));
            if up
                .inner
                .on_hover_text(format!("Move up ({})", keys::label(&MOVE_STEP_UP)))
                .clicked()
            {
                edit = Some(StepEdit::Move(id, index - 1));
            }
            let down = ui.add_enabled_ui(index + 1 < count, |ui| {
                ui.add_sized(button, egui::Button::new("Down"))
            });
            if down
                .inner
                .on_hover_text(format!("Move down ({})", keys::label(&MOVE_STEP_DOWN)))
                .clicked()
            {
                edit = Some(StepEdit::Move(id, index + 1));
            }
            if ui
                .add_sized(button, egui::Button::new("Remove"))
                .on_hover_text("Remove this step (can be undone)")
                .clicked()
            {
                edit = Some(StepEdit::Remove(id));
            }
        });
    }
    if count == 0 {
        ui.label(
            RichText::new("No steps yet. Steps are optional but make intent explicit.").color(theme::MUTED),
        );
    }
    if ui.button("+ Add step").clicked() {
        edit = Some(StepEdit::AddAfter(None));
    }
    match edit {
        Some(StepEdit::Move(id, to)) => {
            changed |= open.doc.move_step(id, to);
            // Focus follows the step, which keeps its identity.
            *cx.focus = Some(Focus::Step(id));
        }
        Some(StepEdit::Remove(id)) => {
            if let Some((index, step)) = open.doc.remove_step(id) {
                let label = edn_view::abbreviate(step.text.trim(), 40);
                cx.actions.push(Action::Removed {
                    message: if label.is_empty() {
                        format!("Removed step {}", index + 1)
                    } else {
                        format!("Removed step “{label}”")
                    },
                    undo: Undo::Step {
                        function,
                        index,
                        step,
                    },
                });
                changed = true;
            }
        }
        Some(StepEdit::AddAfter(after)) => {
            let id = open.doc.add_step(after, "");
            *cx.focus = Some(Focus::Step(id));
            changed = true;
        }
        None => {}
    }
    changed
}
