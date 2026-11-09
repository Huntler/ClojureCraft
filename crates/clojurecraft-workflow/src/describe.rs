//! Describing existing code: the model writes the specification (description, steps, example
//! inputs) that the code implements, and the code itself is run to record each example's output.
//! The model never states an expected output, so every example is exactly what the code returns.

use clojurecraft_clojure::{Call, CallStatus, EvalOutcome, EvalRequest, EvalStatus};
use clojurecraft_domain::{Edn, Example, FunctionDocument, Step};
use clojurecraft_edn::TokenKind;
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
                    Field::required("input", Schema::string()).describe(
                        "one EDN value, not JSON: maps with keyword keys such as {:id 1 :name \"a\"}",
                    ),
                ]),
                Some(MAX_EXAMPLES),
            ),
        ),
    ])
}

/// The request for a specification of `code`, the current implementation of `doc`, with the
/// definitions it calls in other namespaces when they were found.
pub fn request(doc: &FunctionDocument, code: &str, project_code: Option<Section>) -> GenerateRequest {
    let arguments = doc.arguments();
    let existing: Vec<String> = spec::usable_examples(doc)
        .into_iter()
        .map(|(_, _, input)| excerpt(input.trim()))
        .collect();
    // With enough examples already, a few that add something are worth more than five more.
    let wanted = if existing.len() >= MAX_EXAMPLES {
        2
    } else {
        MAX_EXAMPLES
    };
    let input_shape = if arguments == 1 {
        "Each input is one EDN value the function is called with; write a string in double quotes, \
         for example \"abc\" or \"\" for the empty string."
            .to_owned()
    } else {
        format!(
            "The function takes {arguments} arguments, so each input is an EDN vector of the \
             arguments in order: input [a b] means the call (f a b)."
        )
    };
    let time = [
        "java.time",
        "Instant",
        "Duration",
        "LocalDate",
        "LocalTime",
        "inst-ms",
    ]
    .iter()
    .any(|w| code.contains(w));
    let input_shape = if time {
        format!(
            "{input_shape} Write time values as tagged literals: #inst \"2024-01-31T10:00:00Z\" (an \
             instant, read as java.util.Date), #time/duration \"PT30M\", #time/date \"2024-01-31\", \
             #time/date-time \"2024-01-31T10:00\", or #time/time \"10:00\"."
        )
    } else {
        input_shape
    };
    let input_shape = if code.contains("UUID") || code.contains("uuid") {
        format!(
            "{input_shape} Write UUIDs as tagged literals, for example \
             #uuid \"3b2a9f30-6f0e-4bd8-9e64-0c6f1c1b4a52\", never as bare words."
        )
    } else {
        input_shape
    };
    let text = format!(
        "Below is the implementation of a pure Clojure function. Write the specification an engineer \
         would have written before the code existed, so that a programmer could write this exact \
         function from it.\n\
         1. In \"description\", say in one to three sentences what the function returns for its \
         input, in terms of the data, not the code. Mention how it treats empty input, missing keys, \
         or nil where the code decides it. {COMBINING}\n\
         2. In \"steps\", list the transformation steps in order, each one short sentence about the \
         data (for example \"Group the orders by :customer.\"). Name keys and values, not Clojure \
         functions. Use at most {MAX_STEPS} steps; a simple function may have one.\n\
         3. In \"examples\", give {wanted} example inputs with a short name each: typical \
         inputs first, then edge cases the code handles, such as empty input. Together they must \
         show what every part of the code does, so that code without that part would give a \
         different output: take every branch, and where the code converts a value (for example \
         with str, double, long, keyword, or name), give a value the conversion changes, such as \
         an integer where it calls double or a number or UUID where it calls str. Where the code \
         looks a value up in a map or set from the input, include the key it looks up, written \
         with the same type (a UUID key for a UUID, not a string), so the lookup finds something. \
         {input_shape} \
         Use small, realistic data the code accepts; where the code rejects input on purpose (it \
         throws, for example with ex-info or from a case without a default), add one input it \
         rejects. Do not write outputs; the code is run to record them.\n\
         Answer with JSON only."
    );
    let mut sections = vec![Section::required("Task", Placement::User, text)];
    if !existing.is_empty() {
        sections.push(Section::required(
            "Existing examples",
            Placement::User,
            format!(
                "The function already has examples with these inputs. Propose inputs that show \
                 something they do not, not variants of the same cases:\n{}",
                existing.join("\n")
            ),
        ));
    }
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
    sections.extend(project_code);
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

/// The inputs that gave no example, each with why, from the answer's notes (unreadable inputs)
/// and the run's `notes` (inputs that threw). Empty when the run itself failed, which other
/// inputs would not change.
pub fn rejected(answer: &DescriptionAnswer, notes: &[String]) -> Vec<String> {
    if notes.iter().any(|n| n.starts_with("No example outputs")) {
        return Vec::new();
    }
    let unreadable = answer
        .notes
        .iter()
        .filter(|n| n.contains(" was left out: "))
        .filter_map(|n| n.split_once(" was left out: ").map(|(_, why)| why.to_owned()));
    let failed = answer.inputs.iter().filter_map(|(name, _)| {
        notes
            .iter()
            .find(|n| n.starts_with(&format!("“{name}” was left out: ")))
            .and_then(|n| n.split_once(" was left out: "))
            .map(|(_, why)| why.trim_end_matches('.').to_owned())
    });
    unreadable.chain(failed).collect()
}

/// The request of `request` again, with the inputs of the first answer that gave no example.
pub fn retry_request(
    doc: &FunctionDocument,
    code: &str,
    project_code: Option<Section>,
    retry: &Retry,
) -> GenerateRequest {
    let mut r = request(doc, code, project_code);
    let text = match retry {
        Retry::Rejected(lines) => format!(
            "A first answer proposed these inputs, and none of them gave an example:\n{}\n\
             Write different inputs that avoid these problems: valid EDN that the code accepts \
             without throwing.",
            lines.join("\n")
        ),
        Retry::SameOutput { output, inputs } => format!(
            "A first answer proposed these inputs, and the code returned {output} for every one \
             of them:\n{}\nWrite inputs that make the code return different results, so that \
             together they show every outcome, for example both true and false.",
            inputs.join("\n")
        ),
    };
    r.sections
        .push(Section::required("First answer", Placement::User, text));
    r
}

/// Why the example inputs are asked for again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retry {
    /// No input gave an example; each line is an input that did not, and why.
    Rejected(Vec<String>),
    /// Every recorded example has this output.
    SameOutput { output: String, inputs: Vec<String> },
}

/// The retry for recorded examples that all have the same output, as the function's existing
/// examples do, which shows only one outcome of the code. Two examples may agree by chance;
/// three or more that agree are asked again.
pub fn same_output(doc: &FunctionDocument, examples: &[Example]) -> Option<Retry> {
    let first = examples.first()?.expected.as_str().trim();
    let all_same = examples.len() >= 3
        && examples
            .iter()
            .map(|e| e.expected.as_str())
            .chain(
                spec::usable_examples(doc)
                    .iter()
                    .map(|(_, e, _)| e.expected.as_str()),
            )
            .all(|o| o.trim() == first);
    all_same.then(|| Retry::SameOutput {
        output: excerpt(first),
        inputs: examples
            .iter()
            .filter_map(|e| match &e.input {
                clojurecraft_domain::ExampleInput::Direct { value } => Some(excerpt(value.as_str())),
                clojurecraft_domain::ExampleInput::FunctionOutput { .. } => None,
            })
            .collect(),
    })
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

/// Parses the answer for a function of `arguments` arguments, whose inputs must then be vectors
/// of that many values.
pub fn parse(v: &Value, arguments: u32) -> Result<DescriptionAnswer, String> {
    let raw: RawDescription =
        serde_json::from_value(v.clone()).map_err(|e| format!("the answer could not be read: {e}"))?;
    let description = raw.description.trim().to_owned();
    if description.is_empty() {
        return Err("the description is empty".into());
    }
    let steps = unique_steps(&raw.steps);
    let mut inputs: Vec<(String, String)> = Vec::new();
    let mut notes = Vec::new();
    for (i, e) in raw.examples.iter().enumerate() {
        let name = match e.name.trim() {
            "" => format!("example {}", i + 1),
            n => n.to_owned(),
        };
        let quoted = match json_as_edn(&e.input) {
            Some(edn) => {
                notes.push(format!(
                    "The input of “{name}” was written as JSON and is read as the EDN value {}.",
                    excerpt(&edn)
                ));
                Some(edn)
            }
            None => match unquoted_text(&e.input) {
                Some(q) => {
                    notes.push(format!(
                        "The input of “{name}” was written without quotes and is read as the string {q}."
                    ));
                    Some(q)
                }
                None => unwrapped(&e.input, arguments).map(|(edn, what)| {
                    notes.push(format!(
                        "The input of “{name}” was written without its brackets and is read as {what}."
                    ));
                    edn
                }),
            },
        };
        let input = quoted.as_deref().unwrap_or(e.input.trim());
        if let Err(why) = single_value(input) {
            notes.push(format!(
                "The input of “{name}” was left out: it is not one EDN value ({why}): {}",
                excerpt(input)
            ));
        } else if let Some(why) = not_arguments(input, arguments) {
            notes.push(format!(
                "The input of “{name}” was left out: {why}: {}",
                excerpt(input)
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

/// Answer budget of a revision: prose only.
pub const REVISION_TOKENS: u32 = 1200;

pub fn revision_schema() -> Schema {
    Schema::object(vec![
        Field::required("description", Schema::bounded_string(600)),
        Field::required(
            "steps",
            Schema::array(Schema::bounded_string(200), Some(MAX_STEPS)),
        ),
    ])
}

/// What running the code showed: each recorded example as input and output, and each input left
/// out with why (usually an exception), as lines for a revision.
pub fn evidence(answer: &DescriptionAnswer, examples: &[Example], notes: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    for e in examples {
        if let clojurecraft_domain::ExampleInput::Direct { value } = &e.input {
            let expected = e.expected.as_str();
            lines.push(match clojurecraft_edn::expected_throws(expected) {
                Some(class) => format!("{} throws {class}", value.as_str()),
                None => format!("{} returns {expected}", value.as_str()),
            });
        }
    }
    for (name, input) in &answer.inputs {
        if examples.iter().any(|e| &e.name == name) {
            continue;
        }
        if let Some(note) = notes.iter().find(|n| n.contains(&format!("“{name}”"))) {
            let why = note.split_once(": ").map_or(note.as_str(), |(_, w)| w);
            lines.push(format!("{input}: {}", why.trim_end_matches('.')));
        }
    }
    lines
}

/// Asks for the description and steps corrected against what running the code showed.
/// How a description must state a result built from the elements of a collection: which value
/// is computed for one element, and how the elements' values are combined.
const COMBINING: &str = "Where the result combines values computed for each element of a collection, \
    say what is computed for one element and how those values are combined, for example \"the largest, \
    over the models, of a model's window plus that model's longest delay\" rather than \"the window \
    plus the longest delay of all models\".";

pub fn revision_request(code: &str, answer: &DescriptionAnswer, evidence: &[String]) -> GenerateRequest {
    let steps: String = answer
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {s}\n", i + 1))
        .collect();
    let text = "Below is a pure Clojure function, a description and steps that were written for it \
                before it was run, and what running it on example inputs showed. Correct the \
                description and the steps wherever they disagree with what the code did, for example \
                a claim about empty input, nil, or missing keys that the results contradict, or an \
                input said to be handled that made the code throw. Check against the code that \
                the description says what is computed for each element and how those results are \
                combined. Keep what is right and its wording. Describe the data, not the code. \
                Answer with JSON only.";
    let text = format!("{text} {COMBINING}");
    let sections = vec![
        Section::required("Task", Placement::User, text),
        Section::required(
            "Code",
            Placement::User,
            format!("The implementation:\n{}", code.trim_end()),
        ),
        Section::required(
            "Draft",
            Placement::User,
            format!("Description: {}\nSteps:\n{steps}", answer.description),
        ),
        Section::required(
            "Results",
            Placement::User,
            format!("What running the code showed:\n{}", evidence.join("\n")),
        ),
    ];
    let mut r = GenerateRequest::new(Role::Explanation, sections);
    r.schema = Some(revision_schema());
    r.max_output_tokens = Some(REVISION_TOKENS);
    r
}

/// Contradictions an audit reports at most.
pub const MAX_CONTRADICTIONS: usize = 5;

pub fn audit_schema() -> Schema {
    Schema::object(vec![Field::required(
        "contradictions",
        Schema::array(Schema::bounded_string(300), Some(MAX_CONTRADICTIONS)),
    )])
}

/// Asks where the user's description and steps, which are kept, disagree with what running the
/// code showed.
pub fn audit_request(
    code: &str,
    description: &str,
    steps: &[String],
    evidence: &[String],
) -> GenerateRequest {
    let steps: String = steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {s}\n", i + 1))
        .collect();
    let text = "Below is a pure Clojure function, its description and steps, and what running it on \
                example inputs showed. In \"contradictions\", list each statement of the description \
                or the steps that the results or the code contradict, for example a claim about \
                empty input, nil, or missing keys where the code throws or returns something else. \
                Quote the statement and say what the code does instead, in one sentence each. List \
                nothing that agrees with the code, and nothing the results do not decide. Answer \
                with JSON only.";
    let sections = vec![
        Section::required("Task", Placement::User, text),
        Section::required(
            "Code",
            Placement::User,
            format!("The implementation:\n{}", code.trim_end()),
        ),
        Section::required(
            "Specification",
            Placement::User,
            format!("Description: {}\nSteps:\n{steps}", description.trim()),
        ),
        Section::required(
            "Results",
            Placement::User,
            format!("What running the code showed:\n{}", evidence.join("\n")),
        ),
    ];
    let mut r = GenerateRequest::new(Role::Explanation, sections);
    r.schema = Some(audit_schema());
    r.max_output_tokens = Some(REVISION_TOKENS);
    r
}

#[derive(Deserialize)]
struct RawAudit {
    contradictions: Vec<String>,
}

/// The contradictions an audit found, as notes.
pub fn parse_audit(v: &Value) -> Result<Vec<String>, String> {
    let raw: RawAudit =
        serde_json::from_value(v.clone()).map_err(|e| format!("the check could not be read: {e}"))?;
    Ok(raw
        .contradictions
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| format!("The specification may be wrong: {c}"))
        .collect())
}

#[derive(Deserialize)]
struct RawRevision {
    description: String,
    steps: Vec<String>,
}

/// `answer` with the revised description and steps; an empty revision keeps the draft's.
pub fn revise(answer: &mut DescriptionAnswer, v: &Value) -> Result<(), String> {
    let raw: RawRevision =
        serde_json::from_value(v.clone()).map_err(|e| format!("the revision could not be read: {e}"))?;
    let description = raw.description.trim();
    if !description.is_empty() {
        description.clone_into(&mut answer.description);
    }
    let steps = unique_steps(&raw.steps);
    if !steps.is_empty() {
        answer.steps = steps;
    }
    Ok(())
}

/// Why `input`, one EDN value, is not the vector of `arguments` arguments it must be when the
/// function takes more than one.
fn not_arguments(input: &str, arguments: u32) -> Option<String> {
    if arguments == 1 {
        return None;
    }
    let parsed = clojurecraft_edn::parse_value(input);
    match parsed.values.first().map(|v| &v.kind) {
        Some(clojurecraft_edn::NodeKind::Vector(items)) if items.len() == arguments as usize => None,
        Some(clojurecraft_edn::NodeKind::Vector(items)) => Some(format!(
            "the function takes {arguments} arguments, but the vector has {}",
            items.len()
        )),
        _ => Some(format!(
            "the function takes {arguments} arguments, so the input must be a vector of them in order"
        )),
    }
}

/// `input` for a note, cut at 160 characters.
fn excerpt(input: &str) -> String {
    const MAX: usize = 160;
    match input.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &input[..i]),
        None => input.to_owned(),
    }
}

/// The steps without blanks, repeats, and the numbers models put in front (`1.`, `2)`), which the
/// interface adds itself.
fn unique_steps(raw: &[String]) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    for s in raw.iter().map(|s| strip_number(s.trim())) {
        if !s.is_empty() && !steps.iter().any(|x| x == s) {
            steps.push(s.to_owned());
        }
    }
    steps
}

fn strip_number(step: &str) -> &str {
    let digits = step.len() - step.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    match step[digits..].strip_prefix(['.', ')']) {
        Some(rest) if digits > 0 && rest.starts_with(' ') => rest.trim_start(),
        _ => step,
    }
}

/// `input` wrapped into one value when the model wrote a value's contents without its brackets:
/// as many values as the function has arguments become their vector, and keyword–value pairs
/// a map. Returns the value and what it was read as.
pub(crate) fn unwrapped(input: &str, arguments: u32) -> Option<(String, &'static str)> {
    let text = input.trim();
    let parsed = clojurecraft_edn::parse(text);
    if parsed.has_errors() || parsed.values.len() < 2 {
        return None;
    }
    let n = parsed.values.len();
    if arguments > 1 && n == arguments as usize {
        return Some((format!("[{text}]"), "the vector of the arguments"));
    }
    let pairs = n.is_multiple_of(2)
        && parsed
            .values
            .iter()
            .step_by(2)
            .all(|v| matches!(v.kind, clojurecraft_edn::NodeKind::Keyword));
    pairs.then(|| (format!("{{{text}}}"), "a map"))
}

/// `input` as EDN when it is a JSON object or array, which models often write for maps and
/// vectors: object keys become keywords where they can be, and null becomes nil.
pub(crate) fn json_as_edn(input: &str) -> Option<String> {
    let text = input.trim();
    if !text.starts_with(['{', '[']) || !clojurecraft_edn::parse(text).has_errors() {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    fn edn(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("nil"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    edn(item, out);
                }
                out.push(']');
            }
            Value::Object(entries) => {
                out.push('{');
                for (i, (k, item)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let keyword = !k.is_empty()
                        && !k.starts_with(|c: char| c.is_ascii_digit())
                        && k.chars().all(|c| c.is_alphanumeric() || "-_?!*+.<>=".contains(c));
                    if keyword {
                        out.push(':');
                        out.push_str(k);
                    } else {
                        out.push_str(&serde_json::to_string(k).unwrap_or_default());
                    }
                    out.push(' ');
                    edn(item, out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    edn(&value, &mut out);
    Some(out)
}

/// `input` as an EDN string when it is text the model meant as a string but wrote without
/// quotes: nothing, only bare words (symbols, which pure functions rarely take), or text that
/// is not EDN at all, such as JSON. The code is run on it, so the recorded output still is
/// exactly what the code returns.
pub(crate) fn unquoted_text(input: &str) -> Option<String> {
    let text = input.trim();
    let tokens: Vec<_> = clojurecraft_edn::tokenize(text)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect();
    let words = tokens.iter().all(|t| t.kind == TokenKind::Symbol);
    // Cut-off EDN (open delimiters or strings) is not text.
    let opened = tokens
        .iter()
        .filter(|t| matches!(t.kind, TokenKind::Open(_)))
        .count();
    let closed = tokens
        .iter()
        .filter(|t| matches!(t.kind, TokenKind::Close(_)))
        .count();
    let complete = opened == closed
        && !tokens
            .iter()
            .any(|t| matches!(t.kind, TokenKind::String { terminated: false }));
    let not_edn = complete && clojurecraft_edn::parse(text).has_errors();
    (words || not_edn).then(|| serde_json::to_string(text).unwrap_or_default())
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

/// Exceptions code throws on purpose, for input it rejects: an input that throws one becomes an
/// example expecting it. Others (a NullPointerException, a ClassCastException) are accidents of
/// input the code was not written for, and are left out.
const INTENDED_EXCEPTIONS: [&str; 4] = [
    "IllegalArgumentException",
    "ExceptionInfo",
    "IllegalStateException",
    "UnsupportedOperationException",
];

/// Examples from the inputs whose call returned a value or threw on purpose, and notes on the
/// ones left out.
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
                if INTENDED_EXCEPTIONS.contains(&class.as_str()) {
                    examples.push(Example::direct(
                        name.clone(),
                        Edn::new(input),
                        Edn::new(format!("#throws {class}")),
                    ));
                    continue;
                }
                notes.push(left_out(format!(
                    "the function threw {class}: {} (input {})",
                    e.message.trim().trim_end_matches('.'),
                    excerpt(input)
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
    let mut names: Vec<String> = doc.examples.iter().map(|e| e.name.clone()).collect();
    let added: Vec<Example> = added
        .into_iter()
        .map(|mut e| {
            let base = e.name.clone();
            let mut n = 2;
            while names.contains(&e.name) {
                e.name = format!("{base} ({n})");
                n += 1;
            }
            names.push(e.name.clone());
            e
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
            usable: Vec::new(),
        };
        doc.implementation = project::fragment(&binding).map(Implementation::manual);
        doc.project = Some(binding);
        doc
    }

    fn parse_one(v: &Value) -> Result<DescriptionAnswer, String> {
        parse(v, 1)
    }

    #[test]
    fn inputs_rejected_on_purpose_become_examples_expecting_the_exception() {
        let error = |class: &str| {
            format!(r#""status":"error","error":{{"phase":"call","class":"{class}","message":"m"}}"#)
        };
        let mut out = String::from("{\"event\":\"load\",\"ok\":true}\n");
        for (i, r) in [
            error("java.lang.IllegalArgumentException"),
            error("java.lang.NullPointerException"),
            r#""status":"ok","actual":"0""#.to_owned(),
        ]
        .iter()
        .enumerate()
        {
            out.push_str(&format!("{{\"event\":\"result\",\"index\":{i},{r}}}\n"));
        }
        let outcome = EvalOutcome {
            status: clojurecraft_clojure::EvalStatus::Completed,
            report: clojurecraft_clojure::HarnessReport::parse(&out),
            stderr: String::new(),
            elapsed: std::time::Duration::ZERO,
        };
        let inputs: Vec<(String, String)> = [
            ("other kind", "\"m_x\""),
            ("nil", "nil"),
            ("model", "\"m_model\""),
        ]
        .map(|(n, i)| (n.to_owned(), i.to_owned()))
        .to_vec();
        let (examples, notes) = recorded_examples(&inputs, Ok(&outcome));
        let expected: Vec<&str> = examples.iter().map(|e| e.expected.as_str()).collect();
        assert_eq!(expected, ["#throws IllegalArgumentException", "0"]);
        assert_eq!(notes.len(), 1, "the NullPointerException is left out: {notes:?}");
        let answer = DescriptionAnswer {
            description: "d".into(),
            steps: Vec::new(),
            inputs,
            notes: Vec::new(),
        };
        assert_eq!(
            evidence(&answer, &examples, &[])[0],
            "\"m_x\" throws IllegalArgumentException"
        );
    }

    #[test]
    fn added_examples_never_repeat_a_name() {
        let mut doc = FunctionDocument::new("f");
        doc.examples = vec![Example::direct("empty", Edn::new("[]"), Edn::new("0"))];
        let answer = parse_one(&json!({"description": "d", "steps": [], "examples": []})).unwrap();
        let recorded = vec![
            Example::direct("empty", Edn::new("{}"), Edn::new("0")),
            Example::direct("empty", Edn::new("nil"), Edn::new("0")),
        ];
        apply(&mut doc, &answer, recorded);
        let names: Vec<&str> = doc.examples.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["empty", "empty (2)", "empty (3)"]);
    }

    #[test]
    fn inputs_of_a_function_of_several_arguments_must_be_vectors_of_them() {
        let a = parse(
            &json!({"description": "d", "steps": [], "examples": [
                {"name": "ok", "input": "[{:a 1} {}]"},
                {"name": "record only", "input": "{:a 1}"},
                {"name": "three", "input": "[1 2 3]"}]}),
            2,
        )
        .unwrap();
        assert_eq!(a.inputs, [("ok".to_owned(), "[{:a 1} {}]".to_owned())]);
        assert!(
            a.notes[0].contains("must be a vector") && a.notes[0].ends_with("{:a 1}"),
            "{:?}",
            a.notes
        );
        assert!(a.notes[1].contains("but the vector has 3"), "{:?}", a.notes);
    }

    fn code(doc: &FunctionDocument) -> String {
        doc.implementation.as_ref().unwrap().source.clone()
    }

    #[test]
    fn a_project_function_is_described_with_its_namespace_and_argument_vectors() {
        let doc = project_doc();
        let text: String = request(&doc, &code(&doc), None)
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("(defn other\n  \"Other.\"\n  [x]\n  x)"), "{text}");
        assert!(text.contains("each input is an EDN vector of the arguments"));
        assert!(!text.contains("#time/duration"), "no time hint without time code");
        let timed =
            "(ns shop.orders (:import [java.time Duration]))\n(defn total [a b] (Duration/ofMinutes a))\n";
        let timed_text: String = request(&doc, timed, None)
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(timed_text.contains("#time/duration \"PT30M\""), "{timed_text}");
        assert!(text.contains("(* rate (count orders))"));
        assert!(!text.contains("#uuid"), "no UUID hint without UUID code");
        assert!(!text.contains("Existing examples"));
        let mut with_examples = doc.clone();
        with_examples.examples = (0..5)
            .map(|i| Example::direct("", Edn::new(format!("[[{i}] 1]")), Edn::new("1")))
            .collect();
        let existing: String = request(&with_examples, &code(&doc), None)
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(existing.contains("already has examples with these inputs") && existing.contains("[[4] 1]"));
        assert!(existing.contains("give 2 example inputs"), "{existing}");
        let ids = "(ns shop.orders (:import [java.util UUID]))\n(defn total [^UUID a b] (str a b))\n";
        let ids_text: String = request(&doc, ids, None)
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(ids_text.contains("#uuid \"3b2a9f30"), "{ids_text}");
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
    fn the_evidence_lists_outputs_and_the_inputs_that_threw() {
        let answer = DescriptionAnswer {
            description: "d".into(),
            steps: Vec::new(),
            inputs: vec![
                ("typical".into(), "\"{}\"".into()),
                ("empty".into(), "\"\"".into()),
            ],
            notes: Vec::new(),
        };
        let examples = vec![Example::direct("typical", Edn::new("\"{}\""), Edn::new("{}"))];
        let notes = vec!["“empty” was left out: the function threw EOFException: JSON error.".to_owned()];
        assert_eq!(
            evidence(&answer, &examples, &notes),
            [
                "\"{}\" returns {}",
                "\"\": the function threw EOFException: JSON error"
            ]
        );
        let r = revision_request("(ns a)", &answer, &evidence(&answer, &examples, &notes));
        assert!(r.sections.iter().any(|s| s.text.contains("threw EOFException")));
    }

    #[test]
    fn contents_written_without_brackets_are_wrapped() {
        assert_eq!(
            unwrapped(":machine-id 123, :items []", 1),
            Some(("{:machine-id 123, :items []}".to_owned(), "a map"))
        );
        assert_eq!(
            unwrapped("{:a 1} {}", 2),
            Some(("[{:a 1} {}]".to_owned(), "the vector of the arguments"))
        );
        assert_eq!(unwrapped("1 2 3", 1), None);
        assert_eq!(unwrapped("{:a 1}", 1), None);
        let a = parse(
            &json!({"description": "d", "steps": [], "examples": [{"name": "r", "input": ":source \"x\""}]}),
            1,
        )
        .unwrap();
        assert_eq!(a.inputs, [("r".to_owned(), "{:source \"x\"}".to_owned())]);
    }

    #[test]
    fn json_objects_and_arrays_are_read_as_edn() {
        assert_eq!(
            json_as_edn(r#"{"source":null, "machine-id":"abc", "items":[{"val":0.8}], "a b":true}"#)
                .as_deref(),
            Some(r#"{:source nil, :machine-id "abc", :items [{:val 0.8}], "a b" true}"#)
        );
        assert_eq!(json_as_edn(r#"[{"a": 1}, 2]"#).as_deref(), Some("[{:a 1} 2]"));
        assert_eq!(json_as_edn("[1, 2]"), None, "valid EDN as well");
        assert_eq!(json_as_edn("[1 2]"), None, "EDN already");
        assert_eq!(json_as_edn("{:a 1}"), None);
        assert_eq!(json_as_edn("arn:aws"), None);
    }

    #[test]
    fn text_written_without_quotes_is_read_as_a_string() {
        let a = parse_one(&json!({"description": "Stream name.", "steps": [], "examples": [
            {"name": "arn", "input": "arn:aws:kinesis:eu-central-1:1234:stream/orders"},
            {"name": "nested", "input": "arn:aws:kinesis:us-west-1:2222:stream/with/slashes"},
            {"name": "empty-string", "input": ""},
            {"name": "words", "input": "hello world"},
            {"name": "json", "input": "{\"a\": 1}"},
            {"name": "two values", "input": "1 2"},
            {"name": "cut off", "input": "[{:a"},
            {"name": "nil", "input": "nil"},
            {"name": "keyword", "input": ":k"},
            {"name": "duration", "input": "#time/duration \"PT1M\""}]}))
        .unwrap();
        let inputs: Vec<&str> = a.inputs.iter().map(|(_, i)| i.as_str()).collect();
        assert_eq!(
            inputs,
            [
                "\"arn:aws:kinesis:eu-central-1:1234:stream/orders\"",
                "\"arn:aws:kinesis:us-west-1:2222:stream/with/slashes\"",
                "\"\"",
                "\"hello world\"",
                "{:a 1}",
                "nil",
                ":k",
                "#time/duration \"PT1M\""
            ]
        );
        assert_eq!(a.notes.len(), 7, "{:?}", a.notes);
    }

    #[test]
    fn unreadable_and_repeated_inputs_are_left_out_and_unnamed_ones_numbered() {
        let a = parse_one(
            &json!({"description": " Doubles. ", "steps": ["Double it.", "Double it.", " "],
                              "examples": [{"name": "", "input": "2"}, {"name": "again", "input": " 2 "},
                                           {"name": "two values", "input": "1 2"}]}),
        )
        .unwrap();
        assert_eq!(a.description, "Doubles.");
        assert_eq!(a.steps, ["Double it."]);
        let numbered = parse_one(&json!({"description": "d", "examples": [],
                              "steps": ["1. Group the orders.", "2) Sum them.", "3.5 is kept.", "10. Last."]}))
        .unwrap();
        assert_eq!(
            numbered.steps,
            ["Group the orders.", "Sum them.", "3.5 is kept.", "Last."]
        );
        assert_eq!(a.inputs, [("example 1".to_owned(), "2".to_owned())]);
        assert_eq!(a.notes.len(), 1);
        assert!(
            a.notes[0].ends_with(": 1 2"),
            "the note shows the input: {}",
            a.notes[0]
        );
        assert!(parse_one(&json!({"description": "", "steps": [], "examples": []})).is_err());
    }
}
