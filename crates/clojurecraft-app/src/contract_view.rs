//! The contract editor: input and output shapes with key rules, output order, allowed
//! namespaces, and notes. A contract proposed by the model is shown for review and has no effect
//! until accepted.

use clojurecraft_domain::{
    Contract, ContractStatus, Edn, FunctionId, KeySpec, NilBehavior, OrderingRequirement, Shape, ValueType,
};
use clojurecraft_workflow::deps;
use egui::{Id, RichText, Ui};

use crate::editor::{Action, EditorContext, Undo, section};
use crate::session::OpenDocument;
use crate::theme;
use clojurecraft_workflow::assistant::{JobKind, Report};

fn muted(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

fn edn_ok(text: &str) -> bool {
    let p = clojurecraft_edn::parse_value(text);
    !p.has_errors() && p.values.len() == 1
}

fn type_label(t: &ValueType) -> &str {
    t.label()
}

/// A value-type picker; "other" lets the user name the type in words.
fn type_combo(ui: &mut Ui, id: Id, value: &mut ValueType) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(type_label(value).to_owned())
        .width(130.0)
        .show_ui(ui, |ui| {
            for option in ValueType::KNOWN {
                let selected = std::mem::discriminant(&option) == std::mem::discriminant(value);
                if ui.selectable_label(selected, type_label(&option)).clicked() && !selected {
                    *value = option;
                    changed = true;
                }
            }
        });
    if let ValueType::Other(words) = value {
        changed |= ui
            .add(
                egui::TextEdit::singleline(words)
                    .id(id.with("other"))
                    .hint_text("describe the type")
                    .desired_width(140.0),
            )
            .changed();
    }
    changed
}

fn nil_label(n: &NilBehavior) -> &'static str {
    match n {
        NilBehavior::Unspecified => "nil: not specified",
        NilBehavior::Disallowed => "nil: not allowed",
        NilBehavior::TreatAsMissing => "nil: same as missing",
        NilBehavior::PassThrough => "nil: passed through",
        NilBehavior::Default(_) => "nil or missing: default",
    }
}

fn nil_combo(ui: &mut Ui, id: Id, value: &mut NilBehavior) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(nil_label(value))
        .width(170.0)
        .show_ui(ui, |ui| {
            for option in [
                NilBehavior::Unspecified,
                NilBehavior::Disallowed,
                NilBehavior::TreatAsMissing,
                NilBehavior::PassThrough,
                NilBehavior::Default(Edn::default()),
            ] {
                let selected = std::mem::discriminant(&option) == std::mem::discriminant(value);
                if ui.selectable_label(selected, nil_label(&option)).clicked() && !selected {
                    *value = option;
                    changed = true;
                }
            }
        });
    if let NilBehavior::Default(v) = value {
        let ok = v.as_str().trim().is_empty() || edn_ok(v.as_str());
        let r = ui.add(
            egui::TextEdit::singleline(&mut v.0)
                .id(id.with("default"))
                .font(egui::TextStyle::Monospace)
                .hint_text("default, e.g. 0")
                .desired_width(110.0),
        );
        if !ok {
            r.clone().on_hover_text("Not a single valid EDN value.");
            ui.label(RichText::new("invalid EDN").small().color(theme::BAD));
        }
        changed |= r.changed();
    }
    changed
}

fn key_rule(ui: &mut Ui, id: Id, k: &mut KeySpec, remove: &mut bool) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let ok = k.key.as_str().trim().is_empty() || edn_ok(k.key.as_str());
                let r = ui.add(
                    egui::TextEdit::singleline(&mut k.key.0)
                        .id(id.with("key"))
                        .font(egui::TextStyle::Monospace)
                        .hint_text(":key")
                        .desired_width(130.0),
                );
                changed |= r.changed();
                if !ok {
                    ui.label(RichText::new("invalid EDN").small().color(theme::BAD));
                }
                changed |= ui.checkbox(&mut k.required, "required").changed();
                changed |= type_combo(ui, id.with("type"), &mut k.value_type);
                if ui.small_button("Remove").clicked() {
                    *remove = true;
                }
            });
            ui.horizontal_wrapped(|ui| {
                changed |= nil_combo(ui, id.with("nil"), &mut k.nil);
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut k.notes)
                            .id(id.with("notes"))
                            .hint_text("notes")
                            .desired_width(ui.available_width().max(120.0)),
                    )
                    .changed();
            });
        });
    changed
}

fn shape(ui: &mut Ui, id: Id, label: &str, hint: &str, shape: &mut Shape) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).strong());
        changed |= type_combo(ui, id.with("type"), &mut shape.value_type);
    });
    changed |= ui
        .add(
            egui::TextEdit::multiline(&mut shape.description)
                .id(id.with("description"))
                .hint_text(hint)
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        )
        .changed();
    if !shape.keys.is_empty() {
        muted(ui, "Keys of the map, or of each map in the collection:");
    }
    let mut remove = None;
    for (i, k) in shape.keys.iter_mut().enumerate() {
        let mut r = false;
        changed |= key_rule(ui, id.with(("key", i)), k, &mut r);
        if r {
            remove = Some(i);
        }
    }
    if let Some(i) = remove {
        shape.keys.remove(i);
        changed = true;
    }
    if ui.small_button("+ Add key rule").clicked() {
        shape.keys.push(KeySpec {
            key: Edn::default(),
            required: true,
            value_type: ValueType::Any,
            nil: NilBehavior::Unspecified,
            notes: String::new(),
        });
        changed = true;
    }
    ui.add_space(6.0);
    changed
}

fn ordering(ui: &mut Ui, function: FunctionId, ordering: &mut OrderingRequirement) -> bool {
    let mut changed = false;
    let label = |o: &OrderingRequirement| match o {
        OrderingRequirement::Unspecified => "Not specified",
        OrderingRequirement::PreserveInput => "Same order as the input",
        OrderingRequirement::Unordered => "Order does not matter",
        OrderingRequirement::Sorted(_) => "Sorted",
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Output order").strong());
        egui::ComboBox::from_id_salt(("ordering", function))
            .selected_text(label(ordering))
            .show_ui(ui, |ui| {
                for option in [
                    OrderingRequirement::Unspecified,
                    OrderingRequirement::PreserveInput,
                    OrderingRequirement::Unordered,
                    OrderingRequirement::Sorted(String::new()),
                ] {
                    let selected = std::mem::discriminant(&option) == std::mem::discriminant(ordering);
                    if ui.selectable_label(selected, label(&option)).clicked() && !selected {
                        *ordering = option;
                        changed = true;
                    }
                }
            });
    });
    if let OrderingRequirement::Sorted(by) = ordering {
        changed |= ui
            .add(
                egui::TextEdit::singleline(by)
                    .id(Id::unique(("contract-sorted", function)))
                    .hint_text("Sorted by… (for example: descending total, then customer id)")
                    .desired_width(f32::INFINITY),
            )
            .changed();
    }
    changed
}

fn namespaces(ui: &mut Ui, contract: &mut Contract) -> bool {
    let mut changed = false;
    ui.label(RichText::new("Allowed namespaces").strong());
    muted(
        ui,
        "clojure.core is always available. Generated code that needs another namespace asks for your \
         approval first.",
    );
    ui.horizontal_wrapped(|ui| {
        for ns in deps::supported() {
            let mut on = contract.allowed_dependencies.contains(&ns);
            if ui.checkbox(&mut on, &ns).changed() {
                if on {
                    contract.allowed_dependencies.push(ns);
                    contract.allowed_dependencies.sort();
                } else {
                    contract.allowed_dependencies.retain(|n| *n != ns);
                }
                changed = true;
            }
        }
    });
    changed
}

/// The propose button, or the proposal in progress, and the last proposal's outcome.
fn propose(ui: &mut Ui, cx: &mut EditorContext<'_>, label: &str) {
    let running = cx.assist.job.filter(|(k, _)| *k == JobKind::ProposeContract);
    if let Some((_, phase)) = running {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("{}…", phase.label()));
            if ui.button("Cancel").clicked() {
                cx.actions.push(Action::CancelJob);
            }
        });
        return;
    }
    let blocked = cx.assist.blocked.clone();
    let r = ui
        .add_enabled(blocked.is_none(), egui::Button::new(label))
        .on_hover_text(
            "The local model proposes shapes, keys, nil handling, and order from your description and \
             examples. You review the proposal before it applies.",
        );
    let r = match &blocked {
        Some(why) => r.on_disabled_hover_text(why.as_str()),
        None => r,
    };
    if r.clicked() {
        cx.actions.push(Action::ProposeContract);
    }
}

fn proposal_report(ui: &mut Ui, cx: &mut EditorContext<'_>) {
    match &cx.assist.report {
        Some(Report::ContractProposed { notes }) if !notes.is_empty() => {
            for n in notes {
                muted(ui, n.clone());
            }
        }
        Some(Report::Failed { what, message, .. }) if *what == "propose a contract" => {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("The contract could not be proposed: {message}."))
                        .small()
                        .color(theme::BAD),
                );
                if ui.small_button("Dismiss").clicked() {
                    cx.actions.push(Action::DismissReport);
                }
            });
        }
        _ => {}
    }
}

pub fn contract(ui: &mut Ui, open: &mut OpenDocument, cx: &mut EditorContext<'_>) -> bool {
    let function = open.doc.id;
    let mut changed = false;
    section(
        ui,
        "Contract",
        "Optional. Input and output shapes, nil handling, order, and allowed namespaces stated explicitly.",
    );
    if open.doc.contract.is_none() {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Add contract").clicked() {
                open.doc.contract = Some(deps::new_contract(ContractStatus::Accepted));
                changed = true;
            }
            propose(ui, cx, "Propose a contract");
        });
        proposal_report(ui, cx);
        return changed;
    }
    let mut remove = false;
    let mut propose_again = false;
    egui::Frame::new()
        .stroke(theme::hairline())
        .corner_radius(theme::RADIUS)
        .inner_margin(10.0)
        .show(ui, |ui| {
            let Some(contract) = &mut open.doc.contract else {
                return;
            };
            if contract.status == ContractStatus::Proposed {
                ui.label(
                    RichText::new(
                        "Proposed by the model. It is not a requirement and is not used until you accept it.",
                    )
                    .color(theme::WARN),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Accept").clicked() {
                        contract.status = ContractStatus::Accepted;
                        changed = true;
                    }
                    if ui.button("Discard").clicked() {
                        remove = true;
                    }
                    propose_again = true;
                });
                ui.add_space(4.0);
            }
            let id = |part: &str| Id::unique(("contract", function, part.to_owned()));
            changed |= shape(
                ui,
                id("input"),
                "Input",
                "For example: a vector of order maps with :customer and :amount",
                &mut contract.input,
            );
            changed |= shape(
                ui,
                id("output"),
                "Output",
                "For example: a map from customer id to total amount",
                &mut contract.output,
            );
            changed |= ordering(ui, function, &mut contract.ordering);
            ui.add_space(6.0);
            changed |= namespaces(ui, contract);
            ui.add_space(6.0);
            ui.label(RichText::new("Notes").strong());
            changed |= ui
                .add(
                    egui::TextEdit::multiline(&mut contract.notes)
                        .id(id("notes"))
                        .hint_text("Anything else the function must guarantee")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                )
                .changed();
            if contract.status == ContractStatus::Accepted && ui.button("Remove contract").clicked() {
                remove = true;
            }
        });
    if propose_again {
        propose(ui, cx, "Propose again");
    }
    proposal_report(ui, cx);
    if remove && let Some(contract) = open.doc.contract.take() {
        cx.actions.push(Action::Removed {
            message: "Removed the contract".into(),
            undo: Undo::Contract {
                function,
                contract: Box::new(contract),
            },
        });
        changed = true;
    }
    changed
}
