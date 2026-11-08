//! Applying an agent's specification and test-case review to a function.
//!
//! Both are all-or-nothing: every value is checked first and nothing changes when any of it is
//! invalid, so an agent never leaves a function half-updated. EDN is stored exactly as given
//! (trimmed), like text typed in the editor.

use clojurecraft_domain::{
    Contract, ContractStatus, Edn, Example, FunctionDocument, KeySpec, NilBehavior, OrderingRequirement,
    Shape, Step, TestCaseId, TestOrigin, ValueType,
};
use clojurecraft_workflow::edge_cases::{self, CaseEdit, single_value};

use crate::tools::{CaseAction, CaseVerb, ContractArg, KeyArg, NewCase, ShapeArg, SpecArgs};

/// Namespaces a function may require when no contract says otherwise.
const DEFAULT_NAMESPACES: [&str; 2] = ["clojure.string", "clojure.set"];

/// Applies a specification. Errors name the offending field, e.g. `examples[1].input: …`.
pub fn apply_spec(doc: &mut FunctionDocument, spec: &SpecArgs) -> Result<(), String> {
    let mut errors = Vec::new();
    let examples = spec.examples.as_ref().map(|examples| {
        examples
            .iter()
            .enumerate()
            .map(|(i, e)| {
                for (field, text) in [("input", &e.input), ("expected", &e.expected)] {
                    if let Err(why) = single_value(text) {
                        errors.push(format!("examples[{i}].{field}: {why}"));
                    }
                }
                let name = match e.name.trim() {
                    "" => format!("Example {}", i + 1),
                    n => n.to_owned(),
                };
                Example::direct(name, Edn::new(e.input.trim()), Edn::new(e.expected.trim()))
            })
            .collect::<Vec<_>>()
    });
    let contract = match &spec.contract {
        Some(Some(c)) => Some(Some(contract(c, &mut errors))),
        Some(None) => Some(None),
        None => None,
    };
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    if let Some(name) = &spec.name {
        doc.name = name.trim().to_owned();
    }
    if let Some(description) = &spec.description {
        doc.description = description.trim().to_owned();
    }
    if let Some(steps) = &spec.steps {
        doc.steps = steps
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(Step::new)
            .collect();
    }
    if let Some(examples) = examples {
        doc.examples = examples;
    }
    if let Some(contract) = contract {
        doc.contract = contract;
    }
    Ok(())
}

fn contract(c: &ContractArg, errors: &mut Vec<String>) -> Contract {
    Contract {
        status: ContractStatus::Accepted,
        input: shape("contract.input", &c.input, errors),
        output: shape("contract.output", &c.output, errors),
        ordering: ordering(c.ordering.as_deref(), errors),
        allowed_dependencies: match &c.allowed_namespaces {
            Some(list) => list
                .iter()
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .collect(),
            None => DEFAULT_NAMESPACES.iter().map(|n| (*n).to_owned()).collect(),
        },
        notes: c.notes.trim().to_owned(),
    }
}

fn shape(at: &str, s: &ShapeArg, errors: &mut Vec<String>) -> Shape {
    Shape {
        description: s.description.trim().to_owned(),
        value_type: value_type(s.value_type.as_deref()),
        keys: s
            .keys
            .iter()
            .enumerate()
            .map(|(i, k)| key(&format!("{at}.keys[{i}]"), k, errors))
            .collect(),
    }
}

fn key(at: &str, k: &KeyArg, errors: &mut Vec<String>) -> KeySpec {
    if let Err(why) = single_value(&k.key) {
        errors.push(format!("{at}.key: {why}"));
    }
    let nil = match k.nil.as_deref().map(str::trim).unwrap_or("unspecified") {
        "" | "unspecified" => NilBehavior::Unspecified,
        "disallowed" => NilBehavior::Disallowed,
        "treat_as_missing" => NilBehavior::TreatAsMissing,
        "pass_through" => NilBehavior::PassThrough,
        "default" => {
            let value = k.default.as_deref().unwrap_or_default();
            if let Err(why) = single_value(value) {
                errors.push(format!("{at}.default: {why}"));
            }
            NilBehavior::Default(Edn::new(value.trim()))
        }
        other => {
            errors.push(format!("{at}.nil: unknown behaviour `{other}`"));
            NilBehavior::Unspecified
        }
    };
    KeySpec {
        key: Edn::new(k.key.trim()),
        required: k.required,
        value_type: value_type(k.value_type.as_deref()),
        nil,
        notes: k.notes.trim().to_owned(),
    }
}

/// A type name as offered in the editor; anything else is kept as the agent's own words.
pub fn value_type(text: Option<&str>) -> ValueType {
    let text = text.unwrap_or_default().trim();
    let lower = text.to_lowercase();
    ValueType::KNOWN
        .iter()
        .find(|t| !matches!(t, ValueType::Other(_)) && type_name(t) == lower)
        .cloned()
        .unwrap_or_else(|| match lower.as_str() {
            "" => ValueType::Any,
            "int" | "long" => ValueType::Integer,
            "seq" | "sequence" | "coll" => ValueType::Sequential,
            _ => ValueType::Other(text.to_owned()),
        })
}

/// The short name used in arguments and views.
pub fn type_name(t: &ValueType) -> String {
    match t {
        ValueType::Other(words) => words.clone(),
        ValueType::Sequential => "sequential".into(),
        ValueType::Any => "any".into(),
        t => t.label().to_owned(),
    }
}

fn ordering(text: Option<&str>, errors: &mut Vec<String>) -> OrderingRequirement {
    let text = text.unwrap_or_default().trim();
    match text {
        "" | "unspecified" => OrderingRequirement::Unspecified,
        "preserve_input" => OrderingRequirement::PreserveInput,
        "unordered" => OrderingRequirement::Unordered,
        t if t.starts_with("sorted") => {
            let how = t["sorted".len()..].trim_start_matches([':', ' ']).trim();
            OrderingRequirement::Sorted(how.to_owned())
        }
        other => {
            errors.push(format!("contract.ordering: unknown ordering `{other}`"));
            OrderingRequirement::Unspecified
        }
    }
}

/// What a review changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// Cases added, in the order given.
    pub added: Vec<TestCaseId>,
    /// Whether the set of accepted cases changed (a revision is recorded for that).
    pub accepted_changed: bool,
}

/// Carries out a review on the function. Nothing changes when any action fails.
pub fn review(
    doc: &mut FunctionDocument,
    actions: &[CaseAction],
    add: &[NewCase],
) -> Result<ReviewOutcome, String> {
    let mut work = doc.clone();
    let before = work.basis();
    let mut errors = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        if let Err(why) = act(&mut work, a) {
            errors.push(format!("actions[{i}] ({}): {why}", a.id));
        }
    }
    let mut added = Vec::new();
    for (i, c) in add.iter().enumerate() {
        let id = edge_cases::add(&mut work);
        let name = match c.name.trim() {
            "" => work.test(id).map(|t| t.name.clone()).unwrap_or_default(),
            n => n.to_owned(),
        };
        let edit = CaseEdit {
            name,
            input: c.input.clone(),
            expected: Some(c.expected.clone()),
            question: String::new(),
        };
        let result = edge_cases::save(&mut work, id, edit).and_then(|()| {
            if c.accept {
                edge_cases::accept(&mut work, id)
            } else {
                Ok(())
            }
        });
        match result {
            Ok(()) => added.push(id),
            Err(why) => errors.push(format!("add[{i}]: {why}")),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let accepted_changed = work.basis().tests != before.tests;
    *doc = work;
    Ok(ReviewOutcome {
        added,
        accepted_changed,
    })
}

fn act(doc: &mut FunctionDocument, a: &CaseAction) -> Result<(), String> {
    let id: TestCaseId = a.id.trim().parse().map_err(|_| "not a test case id".to_owned())?;
    let case = doc.test(id).ok_or("no such test case")?;
    let edits = a.name.is_some() || a.input.is_some() || a.expected.is_some();
    if edits && a.action != CaseVerb::Edit {
        return Err("name, input, and expected are only used with `edit`".into());
    }
    match a.action {
        CaseVerb::Accept => edge_cases::accept(doc, id).map_err(|e| e.to_string()),
        CaseVerb::Reject => edge_cases::reject(doc, id).map_err(|e| e.to_string()),
        CaseVerb::Reopen => edge_cases::reopen(doc, id).map_err(|e| e.to_string()),
        CaseVerb::Remove => {
            edge_cases::remove(doc, id);
            Ok(())
        }
        CaseVerb::Edit => {
            if !edits {
                return Err("`edit` needs name, input, or expected".into());
            }
            let mut edit = CaseEdit::of(case);
            if let Some(n) = &a.name {
                edit.name.clone_from(n);
            }
            if let Some(i) = &a.input {
                edit.input.clone_from(i);
            }
            if let Some(e) = &a.expected {
                edit.expected = Some(e.clone());
            }
            edge_cases::save(doc, id, edit).map_err(|e| e.to_string())
        }
    }
}

/// Where a case came from, as shown to agents.
pub fn origin_name(o: TestOrigin) -> &'static str {
    match o {
        TestOrigin::User => "user",
        TestOrigin::Model => "model",
        TestOrigin::ModelEdited => "model_edited",
    }
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Expectation, TestCase, TestStatus};
    use serde_json::json;

    use super::*;

    fn spec(v: serde_json::Value) -> SpecArgs {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_specification_is_applied_whole_or_not_at_all() {
        let mut doc = FunctionDocument::new("old");
        doc.description = "keep".into();
        let bad = spec(json!({
            "name": "new",
            "examples": [{"input": "[1 2", "expected": "3"}, {"input": "[]", "expected": ""}]
        }));
        let err = apply_spec(&mut doc, &bad).unwrap_err();
        assert!(err.contains("examples[0].input"), "{err}");
        assert!(err.contains("examples[1].expected"), "{err}");
        assert_eq!(doc.name, "old", "nothing changed");

        let good = spec(json!({
            "name": " sum ",
            "steps": ["add them", "  "],
            "examples": [{"input": "[1 2]", "expected": "3"}],
            "contract": {
                "input": {"type": "vector"},
                "output": {"type": "int", "keys": []},
                "ordering": "sorted: by :id",
            }
        }));
        apply_spec(&mut doc, &good).unwrap();
        assert_eq!(doc.name, "sum");
        assert_eq!(doc.description, "keep", "absent fields are kept");
        assert_eq!(doc.steps.len(), 1, "blank steps are dropped");
        assert_eq!(doc.examples[0].name, "Example 1");
        let c = doc.contract.as_ref().unwrap();
        assert_eq!(c.status, ContractStatus::Accepted);
        assert_eq!(c.input.value_type, ValueType::Vector);
        assert_eq!(c.output.value_type, ValueType::Integer);
        assert_eq!(c.ordering, OrderingRequirement::Sorted("by :id".into()));
        assert_eq!(c.allowed_dependencies, ["clojure.string", "clojure.set"]);

        apply_spec(&mut doc, &spec(json!({"contract": null}))).unwrap();
        assert!(doc.contract.is_none());
    }

    #[test]
    fn contract_keys_are_checked() {
        let mut doc = FunctionDocument::new("f");
        let err = apply_spec(
            &mut doc,
            &spec(json!({"contract": {"output": {"keys": [
                {"key": ":a", "nil": "default"},
                {"key": ":b", "nil": "sometimes"}
            ]}}})),
        )
        .unwrap_err();
        assert!(err.contains("keys[0].default"), "{err}");
        assert!(err.contains("keys[1].nil"), "{err}");
    }

    fn proposed(doc: &mut FunctionDocument, expectation: Expectation) -> TestCaseId {
        let case = TestCase {
            id: TestCaseId::generate(),
            name: "p".into(),
            input: Edn::new("[]"),
            expectation,
            status: TestStatus::Proposed,
            origin: TestOrigin::Model,
            notes: String::new(),
        };
        let id = case.id;
        doc.tests.push(case);
        id
    }

    fn action(id: TestCaseId, verb: &str) -> CaseAction {
        serde_json::from_value(json!({"id": id.to_string(), "action": verb})).unwrap()
    }

    #[test]
    fn reviews_accept_edit_and_add_cases_atomically() {
        let mut doc = FunctionDocument::new("f");
        let plain = proposed(
            &mut doc,
            Expectation::Value {
                expected: Edn::new("0"),
            },
        );
        let open = proposed(
            &mut doc,
            Expectation::NeedsClarification {
                question: "nil?".into(),
            },
        );

        // An ambiguous case cannot be accepted, so nothing happens.
        let err = review(&mut doc, &[action(plain, "accept"), action(open, "accept")], &[]).unwrap_err();
        assert!(err.contains("actions[1]"), "{err}");
        assert!(doc.tests.iter().all(|t| t.status == TestStatus::Proposed));

        // Deciding it with an expected value makes it acceptable.
        let mut edit = action(open, "edit");
        edit.expected = Some("nil".into());
        let add: NewCase = serde_json::from_value(json!({"input": "[5]", "expected": "5"})).unwrap();
        let out = review(
            &mut doc,
            &[action(plain, "accept"), edit, action(open, "accept")],
            &[add],
        )
        .unwrap();
        assert!(out.accepted_changed);
        assert_eq!(out.added.len(), 1);
        assert!(doc.tests.iter().all(|t| t.status == TestStatus::Accepted));
        assert_eq!(doc.test(open).unwrap().origin, TestOrigin::ModelEdited);
        assert_eq!(doc.test(out.added[0]).unwrap().origin, TestOrigin::User);

        let out = review(&mut doc, &[action(plain, "remove")], &[]).unwrap();
        assert!(out.accepted_changed);
        let bad: NewCase = serde_json::from_value(json!({"input": "(", "expected": "1"})).unwrap();
        assert!(review(&mut doc, &[], &[bad]).is_err());
        assert_eq!(doc.tests.len(), 2);
    }
}
