//! The specification as the model sees it, and whether it is complete enough to work from.
//!
//! Every part the user wrote is sent as a *required* prompt section, so it is never shortened or
//! dropped to fit the context: description, steps, every usable example, accepted tests, the
//! accepted contract, and answered questions. Proposed contracts, open questions, and the model's
//! own interpretation are never sent as requirements.

use clojurecraft_domain::{
    Contract, ContractStatus, Example, ExampleInput, Expectation, FunctionDocument, KeySpec, NilBehavior,
    OrderingRequirement, Shape, TestCase, ValueType,
};
use clojurecraft_edn::parse_value;
use clojurecraft_inference::{Placement, Section};

/// Problems that stop a request, and remarks about parts that are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Readiness {
    pub blocking: Vec<String>,
    pub notes: Vec<String>,
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        self.blocking.is_empty()
    }
}

pub fn example_label(index: usize, example: &Example) -> String {
    let name = example.name.trim();
    if name.is_empty() || name == format!("Example {}", index + 1) {
        format!("Example {}", index + 1)
    } else {
        format!("Example {} ({name})", index + 1)
    }
}

/// The label of the `n`th active test (0-based), as the specification and reports name it.
pub fn test_label(n: usize, test: &TestCase) -> String {
    let name = test.name.trim();
    if name.is_empty() {
        format!("Test {}", n + 1)
    } else {
        format!("Test {name}")
    }
}

/// An example's input as used for validation: typed text, or the recorded upstream output.
fn example_input(example: &Example) -> Option<&str> {
    match &example.input {
        ExampleInput::Direct { value } => Some(value.as_str()),
        ExampleInput::FunctionOutput { snapshot, .. } => snapshot.as_ref().map(|s| s.value.as_str()),
    }
}

/// The first EDN error in `text`, if any.
fn edn_error(text: &str) -> Option<String> {
    let parsed = parse_value(text);
    parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == clojurecraft_edn::Severity::Error)
        .map(|d| {
            let (line, col) = clojurecraft_edn::line_col(text, d.span.start);
            format!("{} (line {line}, column {col})", d.message)
        })
}

/// Examples that take part in requests, with their labels and input text.
pub fn usable_examples(doc: &FunctionDocument) -> Vec<(String, &Example, &str)> {
    doc.examples
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let input = example_input(e)?;
            (!input.trim().is_empty() && !e.expected.as_str().trim().is_empty())
                .then(|| (example_label(i, e), e, input))
        })
        .collect()
}

pub fn readiness(doc: &FunctionDocument) -> Readiness {
    let mut r = Readiness::default();
    let mut usable = 0;
    for (i, e) in doc.examples.iter().enumerate() {
        let label = example_label(i, e);
        let expected = e.expected.as_str();
        let Some(input) = example_input(e) else {
            r.notes.push(format!(
                "{label} takes its input from another function, but no output has been recorded yet, \
                 so it is not used."
            ));
            continue;
        };
        match (input.trim().is_empty(), expected.trim().is_empty()) {
            (true, true) => {
                r.notes.push(format!("{label} is empty and is not used."));
                continue;
            }
            (true, false) => r.blocking.push(format!("{label} has no input.")),
            (false, true) => r.blocking.push(format!("{label} has no expected output.")),
            (false, false) => usable += 1,
        }
        if let Some(err) = edn_error(input) {
            r.blocking
                .push(format!("{label}: the input is not valid EDN: {err}."));
        }
        if let Some(err) = edn_error(expected) {
            r.blocking
                .push(format!("{label}: the expected output is not valid EDN: {err}."));
        }
    }
    for t in doc.tests.iter().filter(|t| t.is_active()) {
        let name = if t.name.trim().is_empty() {
            "an accepted test"
        } else {
            t.name.trim()
        };
        if let Some(err) = edn_error(t.input.as_str()) {
            r.blocking
                .push(format!("Test {name}: the input is not valid EDN: {err}."));
        }
        if let Expectation::Value { expected } = &t.expectation
            && let Some(err) = edn_error(expected.as_str())
        {
            r.blocking.push(format!(
                "Test {name}: the expected output is not valid EDN: {err}."
            ));
        }
    }
    if let Some(c) = doc
        .contract
        .as_ref()
        .filter(|c| c.status == ContractStatus::Accepted)
    {
        for (side, shape) in [("input", &c.input), ("output", &c.output)] {
            for k in &shape.keys {
                if k.key.as_str().trim().is_empty() {
                    r.blocking
                        .push(format!("Contract {side}: a key rule has no key."));
                } else if let Some(err) = edn_error(k.key.as_str()) {
                    r.blocking.push(format!(
                        "Contract {side}: the key {} is not valid EDN: {err}.",
                        k.key.as_str()
                    ));
                }
                if let NilBehavior::Default(v) = &k.nil
                    && let Some(err) = edn_error(v.as_str())
                {
                    r.blocking.push(format!(
                        "Contract {side}: the default for {} is not valid EDN: {err}.",
                        k.key.as_str()
                    ));
                }
            }
        }
    }
    let has_steps = doc.steps.iter().any(|s| !s.text.trim().is_empty());
    if doc.description.trim().is_empty() && !has_steps && usable == 0 {
        r.blocking.insert(
            0,
            "Describe the transformation, add steps, or add an example first.".into(),
        );
    }
    if usable == 0 && r.blocking.is_empty() {
        r.notes.push(
            "There are no complete examples, so the result cannot be checked against expected output.".into(),
        );
    }
    r
}

fn type_text(t: &ValueType) -> String {
    match t {
        ValueType::Other(s) if !s.trim().is_empty() => s.trim().to_owned(),
        other => other.label().to_owned(),
    }
}

fn key_text(k: &KeySpec) -> String {
    let mut parts = vec![
        if k.required { "required" } else { "optional" }.to_owned(),
        type_text(&k.value_type),
    ];
    match &k.nil {
        NilBehavior::Unspecified => {}
        NilBehavior::Disallowed => parts.push("nil is not allowed".into()),
        NilBehavior::TreatAsMissing => parts.push("nil counts as missing".into()),
        NilBehavior::PassThrough => parts.push("nil passes through unchanged".into()),
        NilBehavior::Default(v) => parts.push(format!("nil or missing means {}", v.as_str().trim())),
    }
    let notes = k.notes.trim();
    format!(
        "- {}: {}{}",
        k.key.as_str().trim(),
        parts.join(", "),
        if notes.is_empty() {
            String::new()
        } else {
            format!(". {notes}")
        }
    )
}

fn shape_text(label: &str, shape: &Shape) -> String {
    let mut out = format!("{label}: {}", type_text(&shape.value_type));
    let d = shape.description.trim();
    if !d.is_empty() {
        out.push_str(&format!(". {d}"));
    }
    if !shape.keys.is_empty() {
        out.push_str(&format!(
            "\n{label} keys (of the map, or of each map in the collection):\n{}",
            shape.keys.iter().map(key_text).collect::<Vec<_>>().join("\n")
        ));
    }
    out
}

pub fn ordering_text(o: &OrderingRequirement) -> Option<String> {
    match o {
        OrderingRequirement::Unspecified => None,
        OrderingRequirement::PreserveInput => Some("Output order: the same order as the input.".into()),
        OrderingRequirement::Unordered => Some("Output order: does not matter.".into()),
        OrderingRequirement::Sorted(by) if by.trim().is_empty() => Some("Output order: sorted.".into()),
        OrderingRequirement::Sorted(by) => Some(format!("Output order: sorted by {}.", by.trim())),
    }
}

/// The accepted contract as text.
pub fn contract_text(c: &Contract) -> String {
    let mut parts = vec![
        "Contract, accepted by the user (these are requirements):".to_owned(),
        shape_text("Input", &c.input),
        shape_text("Output", &c.output),
    ];
    parts.extend(ordering_text(&c.ordering));
    let notes = c.notes.trim();
    if !notes.is_empty() {
        parts.push(format!("Notes: {notes}"));
    }
    parts.join("\n")
}

/// Required sections holding everything the user specified.
pub fn specification_sections(doc: &FunctionDocument) -> Vec<Section> {
    let mut out = Vec::new();
    let description = doc.description.trim();
    out.push(Section::required(
        "Description",
        Placement::User,
        format!(
            "Description, in the user's words:\n{}",
            if description.is_empty() {
                "(none)"
            } else {
                description
            }
        ),
    ));
    let steps: Vec<String> = doc
        .steps
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .enumerate()
        .map(|(i, s)| format!("{}. {}", i + 1, s.text.trim()))
        .collect();
    if !steps.is_empty() {
        out.push(Section::required(
            "Transformation steps",
            Placement::User,
            format!("Transformation steps, in order:\n{}", steps.join("\n")),
        ));
    }
    if let Some(c) = doc
        .contract
        .as_ref()
        .filter(|c| c.status == ContractStatus::Accepted)
    {
        out.push(Section::required("Contract", Placement::User, contract_text(c)));
    }
    let answers: Vec<String> = doc
        .answered_clarifications()
        .map(|c| format!("Q: {}\nA: {}", c.question.trim(), c.answer.trim()))
        .collect();
    if !answers.is_empty() {
        out.push(Section::required(
            "Answers to questions",
            Placement::User,
            format!(
                "The user answered these questions about the specification; the answers are requirements:\n{}",
                answers.join("\n\n")
            ),
        ));
    }
    for (label, example, input) in usable_examples(doc) {
        out.push(Section::required(
            &label,
            Placement::User,
            format!(
                "{label}. The function is called with the input and must return the expected output.\n\
                 Input (EDN):\n{}\nExpected output (EDN):\n{}",
                input.trim(),
                example.expected.as_str().trim()
            ),
        ));
    }
    for (n, t) in doc.tests.iter().filter(|t| t.is_active()).enumerate() {
        if let Expectation::Value { expected } = &t.expectation {
            let label = test_label(n, t);
            out.push(Section::required(
                &label,
                Placement::User,
                format!(
                    "{label}, accepted by the user.\nInput (EDN):\n{}\nExpected output (EDN):\n{}",
                    t.input.as_str().trim(),
                    expected.as_str().trim()
                ),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Clarification, Edn, Step};

    use super::*;

    fn doc() -> FunctionDocument {
        let mut doc = FunctionDocument::new("totals");
        doc.description = "Sum amounts per customer".into();
        doc.steps = vec![
            Step::new("group by :customer"),
            Step::new("  "),
            Step::new("sum :amount"),
        ];
        doc.examples = vec![Example::direct(
            "",
            Edn::new("[{:customer 1 :amount 2}]"),
            Edn::new("{1 2}"),
        )];
        doc
    }

    fn texts(doc: &FunctionDocument) -> String {
        specification_sections(doc)
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    #[test]
    fn every_part_of_the_specification_is_required() {
        let mut d = doc();
        let mut c = crate::deps::new_contract(ContractStatus::Accepted);
        c.output.value_type = ValueType::Map;
        c.ordering = OrderingRequirement::Sorted("customer id".into());
        c.input.keys.push(KeySpec {
            key: Edn::new(":amount"),
            required: false,
            value_type: ValueType::Number,
            nil: NilBehavior::Default(Edn::new("0")),
            notes: "may be absent".into(),
        });
        d.contract = Some(c);
        let mut q = Clarification::open("Negative amounts?", "");
        q.answer("Keep them.");
        d.clarifications = vec![q, Clarification::open("Unanswered?", "")];
        let sections = specification_sections(&d);
        assert!(
            sections
                .iter()
                .all(|s| s.priority == clojurecraft_inference::Priority::Required)
        );
        let t = texts(&d);
        for needle in [
            "Sum amounts per customer",
            "1. group by :customer\n2. sum :amount",
            "Output: map",
            "- :amount: optional, number, nil or missing means 0. may be absent",
            "sorted by customer id",
            "Q: Negative amounts?\nA: Keep them.",
            "Input (EDN):\n[{:customer 1 :amount 2}]\nExpected output (EDN):\n{1 2}",
        ] {
            assert!(t.contains(needle), "missing {needle:?} in\n{t}");
        }
        assert!(!t.contains("Unanswered"), "open questions are not requirements");
    }

    #[test]
    fn a_proposed_contract_is_never_sent_as_a_requirement() {
        let mut d = doc();
        let mut c = Contract::empty(ContractStatus::Proposed);
        c.notes = "PROPOSAL".into();
        d.contract = Some(c);
        assert!(!texts(&d).contains("PROPOSAL"));
    }

    #[test]
    fn incomplete_or_invalid_examples_block_with_actionable_messages() {
        let mut d = doc();
        assert!(readiness(&d).is_ready());
        d.examples
            .push(Example::direct("broken", Edn::new("{:a"), Edn::new("1")));
        d.examples
            .push(Example::direct("", Edn::new("1"), Edn::default()));
        d.examples
            .push(Example::direct("", Edn::default(), Edn::default()));
        let r = readiness(&d);
        assert_eq!(r.blocking.len(), 2, "{r:?}");
        assert!(r.blocking[0].starts_with("Example 2 (broken): the input is not valid EDN"));
        assert!(r.blocking[0].contains("line 1"));
        assert_eq!(r.blocking[1], "Example 3 has no expected output.");
        assert_eq!(r.notes, ["Example 4 is empty and is not used."]);
    }

    #[test]
    fn an_empty_specification_cannot_be_used() {
        let r = readiness(&FunctionDocument::new("f"));
        assert!(!r.is_ready());
        assert!(r.blocking[0].contains("Describe the transformation"));
        let mut d = FunctionDocument::new("f");
        d.description = "Reverse a list".into();
        let r = readiness(&d);
        assert!(r.is_ready());
        assert!(r.notes[0].contains("no complete examples"));
    }
}
