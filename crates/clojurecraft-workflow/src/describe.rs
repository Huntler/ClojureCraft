//! Describing existing code: the model writes the specification (description, steps, example
//! inputs) that the code implements, and the code itself is run to record each example's output.
//! The model never states an expected output, so every example is exactly what the code returns.

use clojurecraft_clojure::{Call, CallStatus, EvalOutcome, EvalRequest, EvalStatus};
use clojurecraft_domain::{Edn, Example, FunctionDocument, Step};
use clojurecraft_inference::{Field, GenerateRequest, Placement, Role, Schema, Section};
use serde::Deserialize;
use serde_json::Value;

use crate::edge_cases::single_value;
use crate::structure::{self, Expectations};
use crate::{Names, deps, project, spec, validation};

/// Example inputs asked for.
pub const MAX_EXAMPLES: usize = 5;
/// Steps asked for at most.
pub const MAX_STEPS: usize = 8;
/// Answer budget: prose plus EDN inputs.
pub const DESCRIPTION_TOKENS: u32 = 2500;
/// Recorded outputs longer than this are left out rather than cut.
pub const MAX_OUTPUT_CHARS: usize = 4000;

/// Why `doc`'s code cannot be described.
pub fn blocker(doc: &FunctionDocument) -> Option<&'static str> {
    match &doc.implementation {
        Some(i) if !i.source.trim().is_empty() => None,
        _ => Some("There is no code to describe."),
    }
}

pub fn schema() -> Schema {
    Schema::object(vec![
        Field::required("description", Schema::bounded_string(600)),
        Field::required(
            "steps",
            Schema::array(Schema::bounded_string(200), Some(MAX_STEPS)),
        ),
        Field::required(
            "examples",
            Schema::array(
                Schema::object(vec![
                    Field::required("name", Schema::bounded_string(80)),
                    Field::required("input", Schema::string()).describe("one EDN value"),
                ]),
                Some(MAX_EXAMPLES),
            ),
        ),
    ])
}

/// The request for a specification of `code`, the current implementation of `doc`.
pub fn request(doc: &FunctionDocument, code: &str) -> GenerateRequest {
    let arguments = doc.arguments();
    let input_shape = if arguments == 1 {
        "Each input is one EDN value the function is called with.".to_owned()
    } else {
        format!(
            "The function takes {arguments} arguments, so each input is an EDN vector of the \
             arguments in order: input [a b] means the call (f a b)."
        )
    };
    let text = format!(
        "Below is the implementation of a pure Clojure function. Write the specification an engineer \
         would have written before the code existed, so that a programmer could write this exact \
         function from it.\n\
         1. In \"description\", say in one to three sentences what the function returns for its \
         input, in terms of the data, not the code. Mention how it treats empty input, missing keys, \
         or nil where the code decides it.\n\
         2. In \"steps\", list the transformation steps in order, each one short sentence about the \
         data (for example \"Group the orders by :customer.\"). Name keys and values, not Clojure \
         functions. Use at most {MAX_STEPS} steps; a simple function may have one.\n\
         3. In \"examples\", give {MAX_EXAMPLES} example inputs with a short name each: typical \
         inputs first, then edge cases the code handles, such as empty input. {input_shape} Use \
         small, realistic data and only inputs the code accepts without throwing. Do not write \
         outputs; the code is run to record them.\n\
         Answer with JSON only."
    );
    let mut sections = vec![Section::required("Task", Placement::User, text)];
    if let Some(p) = &doc.project {
        let context = project::context(p);
        if !context.is_empty() {
            sections.push(Section::required(
                "Namespace",
                Placement::User,
                format!(
                    "The other definitions of {}, which the code may call:\n{context}",
                    p.namespace
                ),
            ));
        }
    }
    let name = doc.name.trim();
    if !name.is_empty() {
        sections.push(Section::required(
            "Name",
            Placement::User,
            format!("Function name: {name}"),
        ));
    }
    sections.push(Section::required(
        "Code",
        Placement::User,
        format!("The implementation:\n{}", code.trim_end()),
    ));
    let mut r = GenerateRequest::new(Role::Explanation, sections);
    r.schema = Some(schema());
    r.max_output_tokens = Some(DESCRIPTION_TOKENS);
    r
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptionAnswer {
    pub description: String,
    pub steps: Vec<String>,
    /// Named example inputs, each one EDN value.
    pub inputs: Vec<(String, String)>,
    /// Inputs that were left out, and why.
    pub notes: Vec<String>,
}

#[derive(Deserialize)]
struct RawExample {
    name: String,
    input: String,
}

#[derive(Deserialize)]
struct RawDescription {
    description: String,
    steps: Vec<String>,
    examples: Vec<RawExample>,
}

pub fn parse(v: &Value) -> Result<DescriptionAnswer, String> {
    let raw: RawDescription =
        serde_json::from_value(v.clone()).map_err(|e| format!("the answer could not be read: {e}"))?;
    let description = raw.description.trim().to_owned();
    if description.is_empty() {
        return Err("the description is empty".into());
    }
    let mut steps: Vec<String> = Vec::new();
    for s in raw.steps.iter().map(|s| s.trim()) {
        if !s.is_empty() && !steps.iter().any(|x| x == s) {
            steps.push(s.to_owned());
        }
    }
    let mut inputs: Vec<(String, String)> = Vec::new();
    let mut notes = Vec::new();
    for (i, e) in raw.examples.iter().enumerate() {
        let name = match e.name.trim() {
            "" => format!("example {}", i + 1),
            n => n.to_owned(),
        };
        let input = e.input.trim();
        if let Err(why) = single_value(input) {
            notes.push(format!(
                "The input of “{name}” was left out: it is not one EDN value ({why})."
            ));
        } else if !inputs.iter().any(|(_, x)| x == input) {
            inputs.push((name, input.to_owned()));
        }
    }
    Ok(DescriptionAnswer {
        description,
        steps,
        inputs,
        notes,
    })
}

/// A run of `code` on the proposed inputs, without expected values, to record what it returns.
pub fn run_request(
    doc: &FunctionDocument,
    code: &str,
    inputs: &[(String, String)],
) -> Result<EvalRequest, String> {
    let names = Names::for_document(doc, |_| false);
    let allowed = deps::allowed(doc);
    let structure = structure::check(
        code,
        &Expectations {
            namespace: &names.namespace,
            function: &names.function,
            allowed: &allowed,
            arguments: names.arguments,
        },
    );
    let Some(function) = structure
        .function
        .clone()
        .filter(|_| structure.problems.is_empty())
    else {
        let problems: Vec<String> = structure.problems.iter().map(ToString::to_string).collect();
        return Err(if problems.is_empty() {
            "the code does not define the function".into()
        } else {
            problems.join("; ")
        });
    };
    let calls = inputs
        .iter()
        .map(|(_, input)| Call {
            input: input.clone(),
            expected: None,
        })
        .collect();
    let mut r = EvalRequest::new(code, function, calls);
    r.call_timeout = validation::CALL_TIMEOUT;
    r.max_elements = validation::MAX_ELEMENTS;
    r.output_chars = Some(MAX_OUTPUT_CHARS);
    r.spread = doc.arguments() != 1;
    project::prepare(doc, &mut r)?;
    Ok(r)
}

/// Examples from the inputs whose call returned a value, and notes on the ones left out.
pub fn recorded_examples(
    inputs: &[(String, String)],
    outcome: Result<&EvalOutcome, &str>,
) -> (Vec<Example>, Vec<String>) {
    let outcome = match outcome {
        Ok(o) => o,
        Err(why) => {
            return (
                Vec::new(),
                vec![format!("No example outputs were recorded: {why}.")],
            );
        }
    };
    let report = &outcome.report;
    let stage_error = [&report.read, &report.check, &report.load]
        .into_iter()
        .find_map(|s| s.as_ref().and_then(|r| r.as_ref().err()));
    if let Some(e) = stage_error {
        return (
            Vec::new(),
            vec![format!(
                "No example outputs were recorded: the code could not be loaded: {}",
                e.message
            )],
        );
    }
    if let EvalStatus::SandboxUnavailable(why) = &outcome.status {
        return (
            Vec::new(),
            vec![format!(
                "No example outputs were recorded: the sandbox could not be set up: {}",
                why.trim()
            )],
        );
    }
    let mut examples = Vec::new();
    let mut notes = Vec::new();
    for (i, (name, input)) in inputs.iter().enumerate() {
        let left_out = |why: String| format!("“{name}” was left out: {why}.");
        let Some(result) = report.result(i) else {
            notes.push(left_out("the code was not run on it".into()));
            continue;
        };
        match result.status {
            CallStatus::Ok if result.truncated => notes.push(left_out("its output is too long".into())),
            CallStatus::Ok => {
                let actual = result.actual.clone().unwrap_or_default();
                match single_value(&actual) {
                    Ok(()) => examples.push(Example::direct(name.clone(), Edn::new(input), Edn::new(actual))),
                    Err(_) => notes.push(left_out(format!("its output {actual} is not EDN data"))),
                }
            }
            CallStatus::Error => {
                let e = result.error.clone().unwrap_or_default();
                let class = e.class.rsplit('.').next().unwrap_or_default().to_owned();
                notes.push(left_out(format!(
                    "the function threw {class}: {}",
                    e.message.trim()
                )));
            }
            CallStatus::Timeout => notes.push(left_out("the function did not finish in time".into())),
            CallStatus::TooLarge => notes.push(left_out("its output is too large".into())),
        }
    }
    (examples, notes)
}

/// Fills the parts of the specification that are still empty: the description and the steps are
/// only written when the user has none, and examples are added after existing ones. Returns
/// what was filled, for the report.
pub fn apply(doc: &mut FunctionDocument, answer: &DescriptionAnswer, examples: Vec<Example>) -> Vec<String> {
    let mut filled = Vec::new();
    if doc.description.trim().is_empty() {
        doc.description.clone_from(&answer.description);
        filled.push("description".to_owned());
    }
    if doc.steps.iter().all(|s| s.text.trim().is_empty()) && !answer.steps.is_empty() {
        doc.steps = answer.steps.iter().map(Step::new).collect();
        filled.push("steps".to_owned());
    }
    let known: Vec<String> = spec::usable_examples(doc)
        .into_iter()
        .map(|(_, _, input)| input.trim().to_owned())
        .collect();
    let added: Vec<Example> = examples
        .into_iter()
        .filter(|e| match &e.input {
            clojurecraft_domain::ExampleInput::Direct { value } => {
                !known.contains(&value.as_str().trim().to_owned())
            }
            clojurecraft_domain::ExampleInput::FunctionOutput { .. } => true,
        })
        .collect();
    if !added.is_empty() {
        filled.push(format!(
            "{} example{}",
            added.len(),
            if added.len() == 1 { "" } else { "s" }
        ));
        doc.examples.extend(added);
    }
    filled
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Implementation, ProjectBinding};
    use serde_json::json;

    use super::*;

    const FILE: &str = "(ns shop.orders)\n\n(defn other\n  \"Other.\"\n  [x]\n  x)\n\n(defn total [orders rate]\n  (* rate (count orders)))\n";

    fn project_doc() -> FunctionDocument {
        let mut doc = FunctionDocument::new("total");
        let binding = ProjectBinding {
            namespace: "shop.orders".into(),
            function: "total".into(),
            helpers: Vec::new(),
            arguments: 2,
            file: FILE.into(),
        };
        doc.implementation = project::fragment(&binding).map(Implementation::manual);
        doc.project = Some(binding);
        doc
    }

    fn code(doc: &FunctionDocument) -> String {
        doc.implementation.as_ref().unwrap().source.clone()
    }

    #[test]
    fn a_project_function_is_described_with_its_namespace_and_argument_vectors() {
        let doc = project_doc();
        let text: String = request(&doc, &code(&doc))
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("(defn other [x]"), "{text}");
        assert!(text.contains("each input is an EDN vector of the arguments"));
        assert!(text.contains("(* rate (count orders))"));
    }

    #[test]
    fn a_project_function_is_run_in_its_whole_file_with_spread_arguments_and_no_expectations() {
        let doc = project_doc();
        let inputs = vec![("one".to_owned(), "[[1 2] 3]".to_owned())];
        let r = run_request(&doc, &code(&doc), &inputs).unwrap();
        assert!(r.spread);
        assert!(r.project.is_some());
        assert!(r.code.contains("(defn other"));
        assert_eq!(r.calls[0].expected, None);
        assert_eq!(r.output_chars, Some(MAX_OUTPUT_CHARS));
    }

    #[test]
    fn unreadable_and_repeated_inputs_are_left_out_and_unnamed_ones_numbered() {
        let a = parse(
            &json!({"description": " Doubles. ", "steps": ["Double it.", "Double it.", " "],
                              "examples": [{"name": "", "input": "2"}, {"name": "again", "input": " 2 "},
                                           {"name": "two values", "input": "1 2"}]}),
        )
        .unwrap();
        assert_eq!(a.description, "Doubles.");
        assert_eq!(a.steps, ["Double it."]);
        assert_eq!(a.inputs, [("example 1".to_owned(), "2".to_owned())]);
        assert_eq!(a.notes.len(), 1);
        assert!(parse(&json!({"description": "", "steps": [], "examples": []})).is_err());
    }
}
