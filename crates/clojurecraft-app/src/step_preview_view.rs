//! The Steps tab: which code carries out each step and, after a step inspection run, the values
//! each step's code produced for an example.
//!
//! Every value shown here was produced by running the code in the sandbox. The explanation's
//! mapping of steps to code is labelled as the model's explanation; values are labelled with the
//! expression they belong to, whether the preview is complete, and how often the expression was
//! evaluated.

use clojurecraft_clojure::probe::{PRINT_CHARS, PRINT_LENGTH, PRINT_LEVEL, VALUES_KEPT};
use clojurecraft_clojure::{ProbeValue, ValueKind};
use clojurecraft_domain::{ExampleId, FunctionDocument, StepId};
use clojurecraft_workflow::intermediate::{Inspection, StepPlan, StepValue};
use egui::{Id, RichText, Ui};

use crate::edn_view;
use crate::generation_view;
use crate::step_inspection::StepInspections;
use crate::theme;

pub enum StepAction {
    Run,
    Cancel,
    Select(ExampleId),
}

/// Whether inspection can run, and its results.
pub struct StepsContext<'a> {
    pub inspections: &'a StepInspections,
    /// Why code cannot be run on this computer, if it cannot.
    pub unavailable: Option<String>,
}

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

pub fn show(ui: &mut Ui, doc: &FunctionDocument, cx: &StepsContext<'_>) -> Option<StepAction> {
    let mut action = None;
    ui.add_space(6.0);
    if doc.steps.is_empty() {
        ui.label(RichText::new("This function has no transformation steps.").color(theme::MUTED));
        return None;
    }
    controls(ui, doc, cx, &mut action);
    if let Some(p) = cx.inspections.problem(doc.id) {
        ui.label(RichText::new(p).small().color(theme::WARN));
    }
    ui.add_space(6.0);
    match cx.inspections.result(doc.id) {
        Some(inspection) => values(ui, doc, inspection, cx, &mut action),
        None => {
            generation_view::step_mapping(ui, doc);
            ui.add_space(6.0);
            muted(
                ui,
                "Run the step inspection to see the values each step's code produces for your examples.",
            );
        }
    }
    action
}

fn controls(ui: &mut Ui, doc: &FunctionDocument, cx: &StepsContext<'_>, action: &mut Option<StepAction>) {
    if let Some(why) = &cx.unavailable {
        ui.label(RichText::new(why).small().color(theme::WARN));
        return;
    }
    ui.horizontal_wrapped(|ui| {
        if cx.inspections.is_running(doc.id) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
            ui.spinner();
            ui.label("Running the examples with step probes…");
            if ui.small_button("Cancel").clicked() {
                *action = Some(StepAction::Cancel);
            }
        } else if ui
            .add_enabled(
                doc.implementation.is_some(),
                egui::Button::new("Inspect step values"),
            )
            .on_hover_text(
                "Runs every example in the sandbox with probes added at the code of each step, and \
                 records the values that code produces.",
            )
            .clicked()
        {
            *action = Some(StepAction::Run);
        }
    });
}

fn values(
    ui: &mut Ui,
    doc: &FunctionDocument,
    inspection: &Inspection,
    cx: &StepsContext<'_>,
    action: &mut Option<StepAction>,
) {
    let code_changed = inspection.code != doc.code_fingerprint();
    let spec_changed = inspection.spec != doc.spec_fingerprint();
    if code_changed || spec_changed {
        let what = match (code_changed, spec_changed) {
            (true, true) => "the code and the specification",
            (true, false) => "the code",
            _ => "the specification",
        };
        ui.label(
            RichText::new(format!(
                "Outdated: these values were recorded before {what} changed. Run the inspection again \
                 for current values."
            ))
            .small()
            .color(theme::WARN),
        );
    }
    if let Some(p) = &inspection.problem {
        ui.label(RichText::new(format!("No values were recorded: {p}")).color(theme::BAD));
    }
    if inspection.examples.is_empty() {
        return;
    }
    let selected = cx
        .inspections
        .selected
        .get(&doc.id)
        .copied()
        .filter(|id| inspection.example(*id).is_some())
        .unwrap_or(inspection.examples[0].example);
    ui.horizontal_wrapped(|ui| {
        for e in &inspection.examples {
            if ui
                .selectable_label(e.example == selected, edn_view::abbreviate(&e.label, 32))
                .clicked()
            {
                *action = Some(StepAction::Select(e.example));
            }
        }
    });
    let Some(example) = inspection.example(selected) else {
        return;
    };
    if let Err(why) = &example.call {
        ui.label(
            RichText::new(format!(
                "The call did not return: {why} Values recorded before that are shown."
            ))
            .small()
            .color(theme::WARN),
        );
    }
    ui.add_space(6.0);
    for (i, step) in doc.steps.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}.", i + 1)).color(theme::MUTED));
            let text = step.text.trim();
            ui.label(if text.is_empty() { "(empty step)" } else { text });
        });
        let plan = inspection.plan(step.id);
        if let Some(StepPlan::Probed { probe, fused_with }) = plan {
            if let Some(target) = inspection.targets.get(*probe) {
                muted(ui, format!("Shows the {}.", target.describe()));
            }
            if !fused_with.is_empty() {
                ui.label(
                    RichText::new(fused_note(doc, step.id, fused_with))
                        .small()
                        .color(theme::WARN),
                );
            }
        }
        match example.value(step.id) {
            Some(StepValue::Observed { evaluations, values }) => {
                observed(ui, (doc.id, selected, step.id), *evaluations, values)
            }
            Some(StepValue::NotReached) => muted(
                ui,
                "Not evaluated for this example (for example a branch that was not taken, or code after \
                 an error).",
            ),
            Some(StepValue::Unavailable(why)) => {
                ui.label(
                    RichText::new(format!("Unavailable: {why}"))
                        .small()
                        .color(theme::WARN),
                );
            }
            None => muted(ui, "Unavailable: this step was added after the inspection ran."),
        }
        ui.add_space(8.0);
    }
    muted(
        ui,
        format!(
            "Values come from running the code in the sandbox with probes added; nothing here is \
             predicted by the model. Previews show at most {PRINT_LENGTH} elements per collection, \
             {PRINT_LEVEL} levels of nesting, and {PRINT_CHARS} characters, and at most {VALUES_KEPT} \
             evaluations per step."
        ),
    );
}

fn step_number(doc: &FunctionDocument, id: StepId) -> String {
    doc.steps
        .iter()
        .position(|s| s.id == id)
        .map_or_else(|| "a removed step".into(), |i| format!("step {}", i + 1))
}

fn fused_note(doc: &FunctionDocument, step: StepId, others: &[StepId]) -> String {
    let mut all: Vec<StepId> = others.to_vec();
    all.push(step);
    all.sort_by_key(|s| doc.steps.iter().position(|x| x.id == *s));
    let names: Vec<String> = all.iter().map(|s| step_number(doc, *s)).collect();
    format!(
        "The code carries out {} together in one expression, so there is no separate value between \
         them; the value shown is the result of all of them.",
        names.join(" and ")
    )
}

fn observed(
    ui: &mut Ui,
    key: (clojurecraft_domain::FunctionId, ExampleId, StepId),
    evaluations: u64,
    values: &[ProbeValue],
) {
    match evaluations {
        1 => {}
        n => muted(
            ui,
            format!(
                "Evaluated {n} times in this call (for example once per element); the first {} {} shown.",
                values.len(),
                if values.len() == 1 { "is" } else { "are" }
            ),
        ),
    }
    for (i, v) in values.iter().enumerate() {
        match v.kind {
            ValueKind::Function => muted(
                ui,
                "This code produces a function (for example a transducer or a composed function) that is \
                 applied together with other steps, so it has no data of its own to show.",
            ),
            ValueKind::Error => {
                ui.label(
                    RichText::new(format!(
                        "The value could not be printed: {}",
                        v.message.as_deref().unwrap_or("unknown error")
                    ))
                    .small()
                    .color(theme::WARN),
                );
            }
            ValueKind::Value => {
                let label = if v.complete {
                    "Executed value"
                } else {
                    "Executed value · shortened preview"
                };
                ui.label(RichText::new(label).small().color(theme::ACCENT_INK));
                let id = Id::unique(("step-value", key.0, key.1, key.2, i));
                egui::Frame::new()
                    .fill(theme::SUNKEN)
                    .stroke(theme::hairline())
                    .corner_radius(theme::RADIUS)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt(id.with("scroll"))
                            .max_height(160.0)
                            .show(ui, |ui| edn_view::code_view(ui, id, &v.text));
                    });
                if v.complete {
                    egui::CollapsingHeader::new(RichText::new("Inspect").small())
                        .id_salt(id.with("inspect"))
                        .show(ui, |ui| {
                            let parsed = clojurecraft_edn::parse_value(&v.text);
                            edn_view::inspector(ui, id.with("tree"), &parsed, &v.text);
                        });
                }
            }
        }
    }
}
