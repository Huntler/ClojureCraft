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

/// Whether `node` has type `t`; `None` when the type cannot be checked mechanically.
fn type_matches(t: &ValueType, node: &clojurecraft_edn::Node) -> Option<bool> {
    use clojurecraft_edn::NodeKind as K;
    let k = &node.kind;
    Some(match t {
        ValueType::Nil => matches!(k, K::Nil),
        ValueType::Boolean => matches!(k, K::Boolean(_)),
        ValueType::Integer => matches!(k, K::Integer),
        ValueType::Number => matches!(k, K::Integer | K::Float | K::Ratio),
        ValueType::String => matches!(k, K::String(_)),
        ValueType::Keyword => matches!(k, K::Keyword),
        ValueType::Symbol => matches!(k, K::Symbol),
        ValueType::Map => matches!(k, K::Map(_)),
        ValueType::Vector => matches!(k, K::Vector(_)),
        ValueType::List => matches!(k, K::List(_)),
        ValueType::Set => matches!(k, K::Set(_)),
        ValueType::Sequential => matches!(k, K::List(_) | K::Vector(_)),
        ValueType::Any | ValueType::Other(_) => return None,
    })
}

fn map_problems(
    src: &str,
    entries: &[(clojurecraft_edn::Node, clojurecraft_edn::Node)],
    keys: &[KeySpec],
) -> Vec<String> {
    let mut out = Vec::new();
    for k in keys {
        let key = k.key.as_str().trim();
        let Some((_, v)) = entries.iter().find(|(kn, _)| kn.text(src).trim() == key) else {
            if k.required {
                out.push(format!("required key {key} is missing"));
            }
            continue;
        };
        if matches!(v.kind, clojurecraft_edn::NodeKind::Nil) {
            if k.nil == NilBehavior::Disallowed {
                out.push(format!("key {key} is nil, but nil is not allowed"));
            }
        } else if type_matches(&k.value_type, v) == Some(false) {
            out.push(format!(
                "key {key} is {}, not {}",
                v.type_name(),
                type_text(&k.value_type)
            ));
        }
    }
    out
}

/// The type of an EDN value, as a contract states it.
fn type_of(node: &clojurecraft_edn::Node) -> ValueType {
    use clojurecraft_edn::NodeKind as K;
    match &node.kind {
        K::Nil => ValueType::Nil,
        K::Boolean(_) => ValueType::Boolean,
        K::Integer => ValueType::Integer,
        K::Float | K::Ratio => ValueType::Number,
        K::String(_) => ValueType::String,
        K::Keyword => ValueType::Keyword,
        K::Symbol => ValueType::Symbol,
        K::Map(_) => ValueType::Map,
        K::Vector(_) => ValueType::Vector,
        K::List(_) => ValueType::List,
        K::Set(_) => ValueType::Set,
        K::Char(_) => ValueType::Other("character".into()),
        K::Tagged { tag, .. } => ValueType::Other(match tag.as_str() {
            "uuid" => "UUID".into(),
            "inst" => "instant".into(),
            t => format!("#{t}"),
        }),
    }
}

/// `shape` made to agree with `values`, the example inputs or outputs it describes: a type that
/// some value contradicts becomes the values' common type (or "any"), and keys of a value that
/// is no map or collection are left out. Returns what was changed.
fn fit_shape(shape: &mut Shape, values: &[&str], side: &str) -> Vec<String> {
    let nodes: Vec<_> = values
        .iter()
        .filter_map(|v| {
            let p = parse_value(v);
            (!p.has_errors()).then(|| p.values.into_iter().next()).flatten()
        })
        .filter(|n| !matches!(n.kind, clojurecraft_edn::NodeKind::Nil))
        .collect();
    let Some(first) = nodes.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if nodes
        .iter()
        .any(|n| type_matches(&shape.value_type, n) == Some(false))
    {
        let common = type_of(first);
        let common = if nodes.iter().all(|n| type_of(n) == common) {
            common
        } else {
            ValueType::Any
        };
        out.push(format!(
            "the {side} type was corrected from {} to {}, as the examples show",
            type_text(&shape.value_type),
            type_text(&common)
        ));
        shape.value_type = common;
    }
    let holds_maps = nodes.iter().any(|n| {
        use clojurecraft_edn::NodeKind as K;
        matches!(n.kind, K::Map(_) | K::Vector(_) | K::List(_) | K::Set(_))
    });
    if !holds_maps && !shape.keys.is_empty() {
        shape.keys.clear();
        out.push(format!(
            "the {side} keys were left out: no example {side} is a map or collection"
        ));
    }
    // A required key that no map of the examples has is contradicted by them. The maps are the
    // values themselves, or the elements of collections.
    let mut seen: Vec<String> = Vec::new();
    let mut maps = 0;
    for v in values {
        use clojurecraft_edn::NodeKind as K;
        let p = parse_value(v);
        let Some(node) = p.values.first().filter(|_| !p.has_errors()) else {
            continue;
        };
        let elements: Vec<&clojurecraft_edn::Node> = match &node.kind {
            K::Map(_) => vec![node],
            K::Vector(items) | K::List(items) | K::Set(items) => items.iter().collect(),
            _ => Vec::new(),
        };
        for e in elements {
            if let K::Map(entries) = &e.kind {
                maps += 1;
                seen.extend(entries.iter().map(|(k, _)| k.text(v).trim().to_owned()));
            }
        }
    }
    if maps > 0 {
        let (kept, dropped): (Vec<KeySpec>, Vec<KeySpec>) = std::mem::take(&mut shape.keys)
            .into_iter()
            .partition(|k| !k.required || seen.iter().any(|s| s == k.key.as_str().trim()));
        shape.keys = kept;
        if !dropped.is_empty() {
            let names: Vec<&str> = dropped.iter().map(|k| k.key.as_str().trim()).collect();
            out.push(format!(
                "the required {side} key{} {} {} left out: no example {side} has {}",
                if names.len() == 1 { "" } else { "s" },
                names.join(", "),
                if names.len() == 1 { "was" } else { "were" },
                if names.len() == 1 { "it" } else { "them" }
            ));
        }
    }
    out
}

/// `contract` made to agree with the usable examples of `doc` (see `fit_shape`). Returns what
/// was changed.
pub fn fit_contract(contract: &mut Contract, doc: &FunctionDocument) -> Vec<String> {
    let examples = usable_examples(doc);
    let inputs: Vec<&str> = examples.iter().map(|(_, _, i)| *i).collect();
    let outputs: Vec<&str> = examples
        .iter()
        .map(|(_, e, _)| e.expected.as_str())
        .filter(|v| clojurecraft_edn::expected_throws(v).is_none())
        .collect();
    let mut out = fit_shape(&mut contract.input, &inputs, "input");
    out.extend(fit_shape(&mut contract.output, &outputs, "output"));
    out
}

/// Where an expected output contradicts the accepted output contract, so no code could pass both.
pub fn expected_contract_problems(c: &Shape, expected: &str) -> Vec<String> {
    use clojurecraft_edn::NodeKind as K;
    let parsed = parse_value(expected);
    let Some(value) = parsed.values.first().filter(|_| !parsed.has_errors()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if type_matches(&c.value_type, value) == Some(false) {
        out.push(format!(
            "it is {}, but the contract's output type is {}",
            value.type_name(),
            type_text(&c.value_type)
        ));
    }
    if !c.keys.is_empty() {
        match &value.kind {
            K::Map(entries) => out.extend(map_problems(expected, entries, &c.keys)),
            K::List(items) | K::Vector(items) | K::Set(items) => {
                for (i, item) in items.iter().enumerate() {
                    if let K::Map(entries) = &item.kind {
                        out.extend(
                            map_problems(expected, entries, &c.keys)
                                .into_iter()
                                .map(|p| format!("element {i}: {p}")),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    out.truncate(5);
    out
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
        if r.blocking.is_empty() {
            let mut expected: Vec<(String, &str)> = usable_examples(doc)
                .into_iter()
                .map(|(label, e, _)| (label, e.expected.as_str()))
                .collect();
            for (n, t) in doc.tests.iter().filter(|t| t.is_active()).enumerate() {
                if let Expectation::Value { expected: v } = &t.expectation {
                    expected.push((test_label(n, t), v.as_str()));
                }
            }
            for (label, value) in expected {
                if clojurecraft_edn::expected_throws(value).is_some() {
                    continue;
                }
                let problems = expected_contract_problems(&c.output, value);
                if !problems.is_empty() {
                    r.blocking.push(format!(
                        "{label}: the expected output contradicts the accepted contract ({}). Change \
                         the contract or the expected output.",
                        problems.join("; ")
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
    let arguments = doc.arguments();
    if arguments != 1 {
        out.push(Section::required(
            "Arguments",
            Placement::User,
            format!(
                "The function takes {arguments} arguments. Every example and test input is a vector of \
                 the arguments in order: input [a b] means the call (f a b)."
            ),
        ));
    }
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
    let called = if arguments == 1 {
        "input"
    } else {
        "elements of the input as its arguments"
    };
    for (label, example, input) in usable_examples(doc) {
        let expected = example.expected.as_str().trim();
        let text = match clojurecraft_edn::expected_throws(expected) {
            Some(class) => format!(
                "{label}. The function is called with the {called} and must throw {class} (or a \
                 subclass of it) instead of returning.\nInput (EDN):\n{}",
                input.trim()
            ),
            None => format!(
                "{label}. The function is called with the {called} and must return the expected \
                 output.\nInput (EDN):\n{}\nExpected output (EDN):\n{expected}",
                input.trim()
            ),
        };
        out.push(Section::required(&label, Placement::User, text));
    }
    for (n, t) in doc.tests.iter().filter(|t| t.is_active()).enumerate() {
        if let Expectation::Value { expected } = &t.expectation {
            let label = test_label(n, t);
            let text = match clojurecraft_edn::expected_throws(expected.as_str()) {
                Some(class) => format!(
                    "{label}, accepted by the user. The call must throw {class} (or a subclass of it) \
                     instead of returning.\nInput (EDN):\n{}",
                    t.input.as_str().trim()
                ),
                None => format!(
                    "{label}, accepted by the user.\nInput (EDN):\n{}\nExpected output (EDN):\n{}",
                    t.input.as_str().trim(),
                    expected.as_str().trim()
                ),
            };
            out.push(Section::required(&label, Placement::User, text));
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

    fn key(k: &str, required: bool, t: ValueType, nil: NilBehavior) -> KeySpec {
        KeySpec {
            key: Edn::new(k),
            required,
            value_type: t,
            nil,
            notes: String::new(),
        }
    }

    #[test]
    fn an_expected_output_missing_a_required_contract_key_blocks_generation() {
        let mut d = FunctionDocument::new("parse workout");
        d.description = "Parse text input line by line into structured maps".into();
        d.examples = vec![Example::direct(
            "",
            Edn::new(r#""2026-12-01 Pull-up 3@10 5kg\n2026-12-03 Jogging""#),
            Edn::new(
                r#"[{:date "2026-12-01" :workout "pull-up" :sets 3} {:date "2026-12-03" :workout "jogging"}]"#,
            ),
        )];
        let mut c = crate::deps::new_contract(ContractStatus::Accepted);
        c.output.keys = vec![
            key(":date", true, ValueType::String, NilBehavior::Disallowed),
            key(":sets", true, ValueType::Number, NilBehavior::TreatAsMissing),
        ];
        d.contract = Some(c);
        let r = readiness(&d);
        assert_eq!(r.blocking.len(), 1, "{r:?}");
        assert!(r.blocking[0].starts_with("Example 1:"), "{r:?}");
        assert!(
            r.blocking[0].contains("element 1: required key :sets is missing"),
            "{r:?}"
        );
    }

    #[test]
    fn a_proposed_contract_the_examples_contradict_takes_their_types() {
        let mut d = FunctionDocument::new("health score id");
        d.examples = vec![
            Example::direct(
                "",
                Edn::new("#uuid \"12345678-1234-1234-1234-123456789012\""),
                Edn::new("#uuid \"b749ede6-21af-5d10-9a45-572e36af09b8\""),
            ),
            Example::direct("", Edn::new("nil"), Edn::new("nil")),
        ];
        let mut c = Contract::empty(ContractStatus::Proposed);
        c.input.value_type = ValueType::Map;
        c.input.keys = vec![key(":sensor-id", true, ValueType::Any, NilBehavior::Unspecified)];
        c.output.value_type = ValueType::Any;
        c.output.keys = vec![key(":uuid", true, ValueType::Any, NilBehavior::Unspecified)];
        let notes = fit_contract(&mut c, &d);
        assert_eq!(c.input.value_type, ValueType::Other("UUID".into()));
        assert!(c.input.keys.is_empty());
        assert_eq!(
            c.output.value_type,
            ValueType::Any,
            "an \"any\" type contradicts nothing"
        );
        assert!(c.output.keys.is_empty());
        assert_eq!(notes.len(), 3, "{notes:?}");

        let mut agreeing = Contract::empty(ContractStatus::Proposed);
        agreeing.input.value_type = ValueType::Other("UUID".into());
        assert!(fit_contract(&mut agreeing, &d).is_empty());
    }

    #[test]
    fn an_expected_exception_is_stated_as_such_and_never_checked_against_the_contract() {
        let mut d = doc();
        d.examples.push(Example::direct(
            "bad",
            Edn::new("[{:customer nil}]"),
            Edn::new("#throws IllegalArgumentException"),
        ));
        let mut c = Contract::empty(ContractStatus::Accepted);
        c.output.value_type = ValueType::Map;
        d.contract = Some(c.clone());
        assert!(readiness(&d).blocking.is_empty(), "{:?}", readiness(&d));
        let text: String = specification_sections(&d)
            .iter()
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("must throw IllegalArgumentException (or a subclass of it)"),
            "{text}"
        );
        let mut proposed = Contract::empty(ContractStatus::Proposed);
        proposed.output.value_type = ValueType::Map;
        assert!(fit_contract(&mut proposed, &d).is_empty());
    }

    #[test]
    fn contract_keys_no_example_has_are_left_out() {
        let mut d = FunctionDocument::new("tag");
        d.examples = vec![
            Example::direct(
                "",
                Edn::new("{:id 1}"),
                Edn::new("{:id 1 :source \"healthscore\"}"),
            ),
            Example::direct("", Edn::new("{}"), Edn::new("{:source \"healthscore\"}")),
        ];
        let mut c = Contract::empty(ContractStatus::Proposed);
        c.output.value_type = ValueType::Map;
        c.output.keys = vec![
            key(":tag", true, ValueType::Any, NilBehavior::Unspecified),
            key(":source", true, ValueType::String, NilBehavior::Unspecified),
        ];
        let notes = fit_contract(&mut c, &d);
        let keys: Vec<&str> = c.output.keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(keys, [":source"]);
        assert_eq!(
            notes,
            ["the required output key :tag was left out: no example output has it"]
        );
        c.output.keys = vec![key(":note", false, ValueType::Any, NilBehavior::Unspecified)];
        assert!(
            fit_contract(&mut c, &d).is_empty(),
            "an optional key may be missing"
        );
    }

    #[test]
    fn expected_outputs_are_checked_against_contract_types_and_nil_rules() {
        let mut shape = Shape {
            value_type: ValueType::Vector,
            ..Shape::default()
        };
        shape.keys = vec![
            key(":n", false, ValueType::Integer, NilBehavior::Disallowed),
            key(":s", false, ValueType::Any, NilBehavior::Unspecified),
        ];
        assert!(expected_contract_problems(&shape, "[{:n 1 :s 2} {}]").is_empty());
        assert_eq!(
            expected_contract_problems(&shape, "({:n 1.5} {:n nil})"),
            vec![
                "it is list, but the contract's output type is vector".to_owned(),
                "element 0: key :n is decimal, not integer".to_owned(),
                "element 1: key :n is nil, but nil is not allowed".to_owned(),
            ]
        );
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
