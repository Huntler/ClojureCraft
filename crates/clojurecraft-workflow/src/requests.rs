//! The model requests of the specification workflow. Each request is a role with its own task
//! text, the specification as required sections, and the schema of the expected answer.

use std::collections::BTreeSet;

use clojurecraft_domain::{ClarificationStatus, FunctionDocument, PromptMessage, Step};
use clojurecraft_inference::{GenerateRequest, Placement, Role, Section, ToolSet};

use crate::answers::{self, InterpretationAnswer};
use crate::deps;
use crate::grammar;
use crate::naming;
use crate::spec;

/// Drop order of retrieved documentation in a generation request: left out first.
pub const DOCUMENTATION_DROP_ORDER: u32 = 3;
/// Drop order of the earlier turns of a fix conversation, oldest first, after the documentation
/// and before the idioms.
pub const HISTORY_DROP_ORDER: u32 = 2;
/// Drop order of project definitions: after the documentation, before the earlier turns of a
/// fix conversation (which come later in the prompt).
pub const PROJECT_CODE_DROP_ORDER: u32 = 2;

/// The namespace and function the generated code should define.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub namespace: String,
    pub function: String,
    /// How many arguments the function takes.
    pub arguments: u32,
}

impl Names {
    /// `is_core` reports whether a symbol names a `clojure.core` var. A project function keeps
    /// the namespace and symbol it has in its file.
    pub fn for_document(doc: &FunctionDocument, is_core: impl Fn(&str) -> bool) -> Self {
        if let Some(p) = &doc.project {
            return Self {
                namespace: p.namespace.clone(),
                function: p.function.clone(),
                arguments: p.arguments,
            };
        }
        let function = naming::function_symbol(&doc.name, is_core);
        Self {
            namespace: naming::namespace_for(&function),
            function,
            arguments: 1,
        }
    }
}

fn task(text: String) -> Section {
    Section::required("Task", Placement::User, text)
}

fn request(
    role: Role,
    task_text: String,
    rest: Vec<Section>,
    schema: clojurecraft_inference::Schema,
) -> GenerateRequest {
    let mut sections = vec![task(task_text)];
    sections.extend(rest);
    let mut r = GenerateRequest::new(role, sections);
    r.schema = Some(schema);
    r
}

/// Non-empty steps in order; the model sees them numbered from 1 in this order.
pub fn numbered_steps(doc: &FunctionDocument) -> Vec<&Step> {
    doc.steps.iter().filter(|s| !s.text.trim().is_empty()).collect()
}

const PART_NAMES: &str = "Refer to parts of the specification as \"description\", \"step N\", \
\"example N\", \"contract\", or \"answers\".";

pub fn interpretation_request(doc: &FunctionDocument) -> GenerateRequest {
    let text = format!(
        "Read the specification of a data transformation below.\n\
         1. In \"restatement\", restate in plain language what the function does, keeping the user's \
         wording where it is precise.\n\
         2. In \"assumptions\", list what you would have to assume because the specification is silent.\n\
         3. In \"issues\", list contradictions between parts of the specification (check every example \
         against the description, the steps, and the contract) and gaps that change the output, such as \
         empty input, missing keys, nil values, duplicates, or ties when sorting. Report only issues \
         that change the function's output. {PART_NAMES}\n\
         Answer with JSON only."
    );
    request(
        Role::Interpretation,
        text,
        spec::specification_sections(doc),
        answers::interpretation_schema(),
    )
}

/// Questions for the issues an interpretation found.
pub fn questions_request(doc: &FunctionDocument, interpretation: &InterpretationAnswer) -> GenerateRequest {
    let issues: Vec<String> = interpretation
        .issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let parts = if issue.refers_to.is_empty() {
                String::new()
            } else {
                format!(" ({})", issue.refers_to.join(", "))
            };
            format!("{}. {}{parts}", i + 1, issue.detail)
        })
        .collect();
    let mut sections = spec::specification_sections(doc);
    sections.push(Section::required(
        "Issues",
        Placement::User,
        format!("Issues found in the specification:\n{}", issues.join("\n")),
    ));
    let dismissed: Vec<String> = doc
        .clarifications
        .iter()
        .filter(|c| c.status == ClarificationStatus::Dismissed)
        .map(|c| format!("- {}", c.question.trim()))
        .collect();
    if !dismissed.is_empty() {
        sections.push(Section::required(
            "Dismissed questions",
            Placement::User,
            format!(
                "The user said these questions do not matter; do not ask them again:\n{}",
                dismissed.join("\n")
            ),
        ));
    }
    let text = format!(
        "Turn the issues listed below into at most six short questions for the user. Ask only what \
         changes the function's output and is not already answered. Each question has \"why\" (the \
         contradiction or gap it resolves), up to five likely answers in \"options\" (short phrases or \
         small EDN examples), and \"refers_to\". {PART_NAMES} Answer with JSON only."
    );
    request(Role::Clarification, text, sections, answers::questions_schema())
}

/// Which documentation to look up. `None` when there is no description or step to plan from.
pub fn planning_request(doc: &FunctionDocument) -> Option<GenerateRequest> {
    let sections: Vec<Section> = spec::specification_sections(doc)
        .into_iter()
        .filter(|s| {
            matches!(
                s.label.as_str(),
                "Description" | "Transformation steps" | "Contract"
            )
        })
        .collect();
    let has_text = !doc.description.trim().is_empty() || !numbered_steps(doc).is_empty();
    if !has_text {
        return None;
    }
    let text = "List the Clojure standard-library functions and macros most useful for implementing \
                the transformation below, at most twelve, in \"symbols\" (for example group-by, \
                update-vals, clojure.string/join), and at most six short phrases naming the concepts \
                involved in \"concepts\" (for example \"sorting with a comparator\"). Answer with JSON only."
        .to_owned();
    Some(request(
        Role::RetrievalPlanning,
        text,
        sections,
        answers::planning_schema(),
    ))
}

pub fn contract_request(doc: &FunctionDocument) -> GenerateRequest {
    let arguments = match doc.arguments() {
        1 => String::new(),
        n => format!(
            " The input is the vector of the function's {n} arguments in order; describe each \
             argument by its position in the input description."
        ),
    };
    let text = format!(
        "Propose a contract for the transformation below: the input and output value types; for maps \
         (or collections of maps) their keys, whether each is required, its value type, and what nil \
         means for it (\"default\" with an EDN default value, \"treat_as_missing\", \"pass_through\", \
         \"disallowed\", or \"unspecified\"); and the output order. Write keys as EDN, for example \
         :customer-id. Derive everything from the specification and the examples; where they do not \
         decide something, use \"unspecified\" or \"any\" instead of inventing a requirement. The user \
         reviews the proposal before it applies.{arguments} Answer with JSON only. {PART_NAMES}"
    );
    request(
        Role::Interpretation,
        text,
        spec::specification_sections(doc),
        answers::contract_schema(),
    )
}

pub struct GenerationInput<'a> {
    pub doc: &'a FunctionDocument,
    pub names: &'a Names,
    /// Namespaces besides `clojure.core` the function may require.
    pub allowed: &'a BTreeSet<String>,
    pub clojure_version: &'a str,
    /// Retrieved reference material, optional in the context policy.
    pub documentation: Option<Section>,
    /// Definitions of the project that may be reused, optional like the documentation.
    pub project_code: Option<Section>,
    pub seed: u64,
}

fn namespace_list(names: &BTreeSet<String>) -> String {
    names
        .iter()
        .map(|n| match deps::alias(n) {
            Some(a) => format!("{n} (as {a})"),
            None => n.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The task text of a generation request.
pub fn generation_task(input: &GenerationInput<'_>) -> String {
    // Project namespaces are offered with the project code found for the task, not listed here.
    let listed: BTreeSet<String> = match &input.doc.project {
        Some(p) => {
            let requires = crate::project::file_requires(&p.file);
            input
                .allowed
                .iter()
                .filter(|n| !p.usable.contains(*n) || requires.contains(*n))
                .cloned()
                .collect()
        }
        None => input.allowed.clone(),
    };
    let allowed = if listed.is_empty() {
        "clojure.core only".to_owned()
    } else {
        format!("clojure.core and {}", namespace_list(&listed))
    };
    let others: BTreeSet<String> = deps::supported()
        .into_iter()
        .filter(|n| !input.allowed.contains(n))
        .collect();
    let others = if others.is_empty() {
        String::new()
    } else {
        format!(
            " If one of {} would make the code clearly simpler you may require it, but it then needs \
             the user's approval.",
            namespace_list(&others)
        )
    };
    let Names {
        namespace,
        function,
        arguments,
    } = input.names;
    let (start, project) = match &input.doc.project {
        Some(p) => (
            format!(
                "- Start with this ns declaration, adding a requirement only if you need one:\n{}\n",
                crate::project::empty_fragment(p)
            ),
            format!(
                "- The function belongs to the existing namespace {namespace}; its other definitions are \
                 listed under \"Namespace\". Call them instead of writing the same logic again, and \
                 refer to a def by its name, never by its value, but do not define or change them: write only {function} and the private helpers it needs, with \
                 names the namespace does not use yet.\n"
            ),
        ),
        None => (
            format!(
                "- Start with (ns {namespace}) and require namespaces only in its :require clause, as \
                 [namespace :as alias].\n"
            ),
            String::new(),
        ),
    };
    let existing = input.doc.project.as_ref().and_then(crate::project::signature);
    let signature = match (*arguments, &existing) {
        (_, Some(head)) => format!(
            "{function}, which already exists. Keep its docstring and parameter names exactly as \
             they are:\n{head}\n"
        ),
        (1, None) => format!("({function} [input] …), taking one argument: the input value of an example"),
        (n, None) => format!(
            "({function} [{}] …), taking {n} arguments: the elements of an example's input vector",
            (1..=n).map(|i| format!("arg{i}")).collect::<Vec<_>>().join(" ")
        ),
    };
    format!(
        "{opening}\n\
         Rules:\n\
         {start}\
         - Define exactly one public function, {signature}{period} It must return the expected output of \
         every example and accepted test, and follow the steps, the contract, and the user's answers.\n\
         {project}\
         - Define helpers with defn-. Top-level forms other than defn, defn-, def, and declare are \
         not allowed. {comments}\n\
         - Use {allowed}.{others}\n\
         - Keep the function free of side effects: no I/O, printing, atoms, futures, agents, or \
         dynamic vars; no eval, read-string, resolve, or other dynamic evaluation. Avoid randomness \
         and the current time unless the specification asks for them.\n\
         - Java interop is allowed where Clojure has no equivalent (for example java.time): import \
         classes in the ns declaration's :import clause, as (:import [java.time Instant Duration]), \
         or write them fully qualified. Call a method of an object as (.method obj arg), for example \
         (.plus d other), and a static method as (Class/method arg), for example (Duration/ofSeconds 5).\n\
         - Prefer clear, idiomatic clojure.core functions. Return eager collections (vectors, maps, \
         sets) unless the examples show a sequence.\n\
         {closing}",
        opening = match &input.doc.project {
            Some(p) => format!(
                "Write one function of the existing Clojure {} namespace {}, implementing the \
                 transformation specified below.",
                input.clojure_version, p.namespace
            ),
            None => format!(
                "Write a Clojure {} namespace implementing the transformation specified below.",
                input.clojure_version
            ),
        },
        closing = if input.doc.project.is_some() {
            format!(
                "Answer with only the ns declaration, the private helpers you add, and {function} in a \
                 ```clojure code block, nothing before or after it; the rest of the namespace stays as it is."
            )
        } else {
            "Answer with only the complete source in a ```clojure code block, nothing before or after it."
                .to_owned()
        },
        period = if existing.is_some() { "" } else { "." },
        comments = if existing.is_some() {
            "Write no comments other than that docstring, and no example calls."
        } else {
            "Write no comments and no example calls."
        },
    )
}

/// The opening message of a generation: the task, the specification, idioms, and
/// documentation.
fn generation_sections(input: &GenerationInput<'_>) -> Vec<Section> {
    let mut sections = vec![task(generation_task(input))];
    if let Some(p) = &input.doc.project {
        let context = crate::project::reference(p);
        if !context.is_empty() {
            sections.push(Section::required(
                "Namespace",
                Placement::User,
                format!(
                    "{} already defines these; call them by name (a def too, instead of copying its \
                     value), but do not write them in your answer:\n{context}",
                    p.namespace
                ),
            ));
        }
    }
    sections.extend(input.project_code.clone());
    sections.extend(spec::specification_sections(input.doc));
    sections.extend(crate::idioms::section(input.doc));
    sections.extend(input.documentation.clone());
    sections
}

pub fn generation_request(input: GenerationInput<'_>) -> GenerateRequest {
    let mut r = GenerateRequest::new(Role::Generation, generation_sections(&input));
    r.seed = Some(input.seed);
    r.grammar = Some(grammar::generation());
    r
}

/// The messages `request` is sent as, keeping the planned sections in `included` (all when
/// `None`).
pub fn prompt_messages(request: &GenerateRequest, included: Option<&[usize]>) -> Vec<PromptMessage> {
    let sections = clojurecraft_inference::planned_sections(request.role, request.sections.clone());
    let kept: Vec<&Section> = match included {
        Some(indices) => indices.iter().filter_map(|i| sections.get(*i)).collect(),
        None => sections.iter().collect(),
    };
    clojurecraft_inference::context::messages(&kept)
        .into_iter()
        .map(|m| PromptMessage {
            role: m.role,
            content: m.content,
        })
        .collect()
}

/// Sampling temperature of a fix: a little above a first generation, because a model that sees
/// its own failing code tends to return it nearly unchanged.
pub const FIX_TEMPERATURE: f32 = 0.4;

/// Sampling temperature of a fresh start after a conversation got stuck: high enough that a
/// different seed gives a different approach.
pub const RESTART_TEMPERATURE: f32 = 0.6;

/// Answer budget of an analysis: a few short lines, plus tool calls.
pub const ANALYSIS_TOKENS: u32 = 700;

/// One exchange of a fix conversation: the model's answer exactly as it was given, the reply
/// with the checked code and its failures, the model's analysis of the cause, and the request
/// to fix the code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub answer: String,
    pub feedback: String,
    /// The analysis with the evaluations the model made; `None` while it is being written.
    pub analysis: Option<String>,
    pub fix: String,
}

/// The answer a fix conversation starts from when the user asks to fix the current code: the
/// code as if the model had written it, so every fix continues the same conversation shape.
pub fn existing_code_answer(code: &str) -> String {
    format!("```clojure\n{}\n```", code.trim_end())
}

/// What a fix should start from: the first failing call, or the check that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    /// A failing example or test: its label and input.
    Call { label: String, input: String },
    /// Another failing check, such as "the lint check".
    Check(String),
}

/// The reply to an answer whose code failed its checks (the code that ran, the failures, and
/// the request to find the cause before writing code), and the request to fix the code that
/// follows the analysis. `readable` is false when the code could not be read, so the fix must
/// be the complete namespace; `tools` says whether the model can evaluate expressions.
pub fn feedback(
    code: &str,
    failures: &str,
    focus: Option<&Focus>,
    readable: bool,
    tools: bool,
) -> (String, String) {
    let (why, analysis) = match focus {
        Some(Focus::Call { label, input }) => {
            let input: String = if input.chars().count() > 300 {
                format!("{} …", input.chars().take(300).collect::<String>())
            } else {
                input.clone()
            };
            (
                format!(
                    "Find out why {label} (input {}) did not pass: follow that input through the code, \
                     using the values recorded above, to the first expression whose value is wrong.",
                    input.trim()
                ),
                "quote the part of the input whose output is wrong, name the expression that turned \
                 it into the wrong value and the value it produced, and say what it must produce \
                 instead; then name the change",
            )
        }
        Some(Focus::Check(check)) => (
            format!("Find out why {check} failed."),
            "quote the reported problem and its line, say what in the code causes it, and name the \
             change",
        ),
        None => (
            "Find out why the checks failed.".to_owned(),
            "quote the reported problem, say what in the code causes it, and name the change",
        ),
    };
    let tools = if tools {
        format!(
            " You can check what an expression returns with the {} tool, for example a helper \
             called with a value recorded above; check before you conclude.",
            crate::eval_tool::NAME
        )
    } else {
        String::new()
    };
    let hints: String = hints(code, failures)
        .iter()
        .map(|h| format!("Hint: {h}\n"))
        .collect();
    let hints = if hints.is_empty() {
        hints
    } else {
        format!("{hints}\n")
    };
    let feedback = format!(
        "That code was run in a sandbox on every example and accepted test. This is the code that \
         ran, after automatic cleanup; line numbers refer to it:\n```clojure\n{code}\n```\n\n\
         These checks failed (examples and tests are named as in the specification):\n{failures}\n\n\
         {hints}{why}{tools} Do not write code yet. Answer with your analysis only, in at most six short \
         lines: {analysis}.",
        code = code.trim_end(),
    );
    let code_part = if readable {
        "<only the top-level forms you change or add, each complete: (defn- name …), (defn name …), \
         or (def name …). Forms you leave out stay as they are. Add the (ns …) form only to change \
         the requires.>"
    } else {
        "<the complete namespace, starting with (ns …), because the code above could not be read>"
    };
    let fix = format!(
        "Now fix the code as your analysis says. The examples, accepted tests, contract, and answers \
         are fixed requirements: change the code, never what it must return. An integer never \
         equals a floating-point number (1 is not 1.0).\n\
         Answer in exactly this format and nothing else:\n```clojure\n{code_part}\n```"
    );
    (feedback, fix)
}

/// What a small model tends to misread in some errors, stated once: the usual cause of a known
/// compiler or JVM message, when the code shows it.
fn hints(code: &str, failures: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // `(Duration/plus a b)`: an instance method called like a static one.
    let mut rest = failures;
    while let Some(at) = rest.find("No matching method ") {
        rest = &rest[at + "No matching method ".len()..];
        let method: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        let Some(class_at) = rest.find("for class ") else {
            break;
        };
        let class: String = rest[class_at + "for class ".len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '$'))
            .collect();
        let short = class.rsplit('.').next().unwrap_or_default();
        let called = format!("({short}/{method} ");
        if short.is_empty() || method.is_empty() || !code.contains(&called) {
            continue;
        }
        let hint = format!(
            "({short}/{method} …) calls a static method of {short}, and there is none with these \
             arguments. If {method} is a method of a {short} object, write (.{method} d …) with the \
             object first, for example (.{method} d other); otherwise {short} has no {method}."
        );
        if !out.contains(&hint) {
            out.push(hint);
        }
    }
    // `(max d1 d2)` on java.time values and the like.
    if let Some(class) = cast_class(failures, "java.lang.Number") {
        out.push(format!(
            "+, -, *, max, min, <, and > take numbers only, and a {class} is not one: compare such \
             values with (compare a b), for example (if (pos? (compare a b)) a b), and combine them \
             with their own methods, such as (.plus a b)."
        ));
    }
    // `(-> n dec + 2)`, `((:x m))`: a value in call position.
    if let Some(class) = cast_class(failures, "clojure.lang.IFn") {
        out.push(format!(
            "A {class} value was called as a function: look for parentheses around a value, or a \
             threading step that is not a call, such as (-> n inc + 2) instead of (-> n inc (+ 2))."
        ));
    }
    // A local with the name of a function hides it.
    let marker = "because \"";
    if failures.contains("clojure.lang.IFn.invoke")
        && let Some(at) = failures.find(marker)
    {
        let local: String = failures[at + marker.len()..]
            .chars()
            .take_while(|c| *c != '"')
            .collect::<String>()
            .replace('_', "-");
        let defined = [format!("(defn- {local} "), format!("(defn {local} ")]
            .iter()
            .any(|d| code.contains(d.as_str()));
        if defined {
            out.push(format!(
                "A local named {local} hides the function {local}, so calling it calls the local's \
                 value: give the local another name."
            ));
        }
    }
    if failures.contains("Too many arguments to if") {
        out.push(
            "if takes a test, a then-branch, and at most one else-branch; for more branches use cond.".into(),
        );
    }
    out
}

/// The simple name of the class a value of which could not be cast to `target`, from a JVM
/// ClassCastException message.
fn cast_class(failures: &str, target: &str) -> Option<String> {
    let end = failures.find(&format!("cannot be cast to class {target}"))?;
    let start = failures[..end].rfind("class ")? + "class ".len();
    let class = failures[start..end].trim();
    Some(class.rsplit('.').next().unwrap_or(class).to_owned())
}

/// The conversation after the opening message: each earlier answer and the reply to it, and,
/// once written, the analysis and the request to fix. Only the last exchange is required;
/// earlier ones are left out, oldest first, when the context is short.
fn conversation(input: &GenerationInput<'_>, turns: &[Turn]) -> Vec<Section> {
    let mut sections = generation_sections(input);
    let last = turns.len().saturating_sub(1);
    for (i, t) in turns.iter().enumerate() {
        let n = i + 1;
        let mut parts = vec![
            (format!("Answer {n}"), Placement::Assistant, t.answer.clone()),
            (format!("Reply {n}"), Placement::User, t.feedback.clone()),
        ];
        if let Some(a) = &t.analysis {
            let a = if a.trim().is_empty() {
                "(no analysis)"
            } else {
                a.as_str()
            };
            parts.push((format!("Analysis {n}"), Placement::Assistant, a.to_owned()));
            parts.push((format!("Fix request {n}"), Placement::User, t.fix.clone()));
        }
        for (label, placement, text) in parts {
            sections.push(if i == last {
                Section::required(&label, placement, text)
            } else {
                Section::optional(&label, HISTORY_DROP_ORDER, text).with_placement(placement)
            });
        }
    }
    sections
}

/// Asks the model why the last answer's code failed; `tools` lets it evaluate expressions.
pub fn analysis_request(input: GenerationInput<'_>, turns: &[Turn], tools: ToolSet) -> GenerateRequest {
    let mut r = GenerateRequest::new(Role::Generation, conversation(&input, turns));
    r.seed = Some(input.seed);
    r.temperature = Some(FIX_TEMPERATURE);
    r.max_output_tokens = Some(ANALYSIS_TOKENS);
    r.tools = tools;
    r
}

/// Asks for the fix the analysis named, as changed forms.
pub fn fix_request(input: GenerationInput<'_>, turns: &[Turn]) -> GenerateRequest {
    let mut r = GenerateRequest::new(Role::Generation, conversation(&input, turns));
    r.seed = Some(input.seed);
    r.temperature = Some(FIX_TEMPERATURE);
    r.grammar = Some(grammar::fix());
    r
}

pub fn explanation_request(doc: &FunctionDocument, code: &str) -> GenerateRequest {
    let steps = numbered_steps(doc);
    let mut sections = spec::specification_sections(doc);
    sections.push(Section::required(
        "Code",
        Placement::User,
        format!("The implementation:\n{}", code.trim_end()),
    ));
    let step_task = if steps.is_empty() {
        "Leave \"steps\" empty.".to_owned()
    } else {
        format!(
            "For each of the {} transformation steps, give its number in \"step\", quote in \"code\" the \
             exact code from the implementation that carries it out (copied, not paraphrased), and \
             explain it in \"explanation\". Leave out steps that no code implements.",
            steps.len()
        )
    };
    let text = format!(
        "Explain the implementation below to an engineer who reads Clojure but may not know its \
         standard library. In \"summary\", describe in at most four sentences how it works. {step_task} \
         Keep each step's explanation to one or two sentences and quote only the lines that implement \
         it. In \"assumptions\", list at most four decisions the code makes where the specification \
         is silent, or none. In \"symbols\", list at most eight standard-library functions and macros \
         used and what each does here. Answer with JSON only."
    );
    let mut r = request(
        Role::Explanation,
        text,
        sections,
        answers::explanation_schema(steps.len()),
    );
    // Quoted code and per-step text need more room than a prose explanation.
    r.max_output_tokens = Some(EXPLANATION_TOKENS);
    r
}

/// Answer budget of an explanation: quoted code for every step plus prose.
pub const EXPLANATION_TOKENS: u32 = 2500;

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Clarification, Contract, ContractStatus, Edn, Example, Step};

    use super::*;

    fn doc() -> FunctionDocument {
        let mut d = FunctionDocument::new("Order totals");
        d.description = "Sum amounts per customer".into();
        d.steps = vec![Step::new("group"), Step::new(""), Step::new("sum")];
        d.examples = vec![Example::direct("", Edn::new("[]"), Edn::new("{}"))];
        d
    }

    fn all_text(r: &GenerateRequest) -> String {
        r.sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_project_function_keeps_its_namespace_and_symbol() {
        let mut d = doc();
        d.project = Some(clojurecraft_domain::ProjectBinding {
            namespace: "shop.orders".into(),
            function: "valid-order?".into(),
            helpers: Vec::new(),
            arguments: 1,
            file: String::new(),
            usable: Vec::new(),
        });
        let names = Names::for_document(&d, |_| false);
        assert_eq!(names.namespace, "shop.orders");
        assert_eq!(names.function, "valid-order?");
    }

    #[test]
    fn a_project_function_is_generated_against_its_namespace() {
        let mut d = doc();
        d.project = Some(clojurecraft_domain::ProjectBinding {
            namespace: "shop.orders".into(),
            function: "price".into(),
            helpers: Vec::new(),
            arguments: 2,
            file: "(ns shop.orders (:require [shop.util :as util]))\n\n\
                   (defn label \"Names an order.\" [order] (util/clean (:name order)))\n"
                .into(),
            usable: Vec::new(),
        });
        let names = Names::for_document(&d, |_| false);
        let allowed = deps::allowed(&d);
        assert!(allowed.contains("shop.util"));
        let r = generation_request(GenerationInput {
            doc: &d,
            names: &names,
            allowed: &allowed,
            clojure_version: "1.12.6",
            documentation: None,
            project_code: None,
            seed: 1,
        });
        let text = all_text(&r);
        assert!(
            text.contains("(ns shop.orders\n  (:require [shop.util :as util]))"),
            "{text}"
        );
        assert!(
            text.contains("(price [arg1 arg2] …), taking 2 arguments"),
            "{text}"
        );
        assert!(
            text.contains("write only price and the private helpers"),
            "{text}"
        );
        assert!(
            text.contains(
                "- label (function of [order]): Names an order. Defined as \
                 `(defn label \"Names an order.\" [order] (util/clean (:name order)))`"
            ),
            "{text}"
        );
        assert!(text.contains("The function takes 2 arguments."), "{text}");
        assert!(text.contains("do not write them in your answer"), "{text}");
        assert!(text.contains("Write one function of the existing Clojure"));
        assert!(text.contains("Answer with only the ns declaration, the private helpers you add, and price"));
        assert!(
            text.contains("called with the elements of the input as its arguments"),
            "{text}"
        );
    }

    #[test]
    fn an_existing_project_function_keeps_its_docstring_and_parameters() {
        let mut d = doc();
        d.project = Some(clojurecraft_domain::ProjectBinding {
            namespace: "shop.orders".into(),
            function: "label".into(),
            helpers: Vec::new(),
            arguments: 1,
            file: "(ns shop.orders)\n\n(defn label\n  \"Names an order.\"\n  [order]\n  (:name order))\n"
                .into(),
            usable: Vec::new(),
        });
        let names = Names::for_document(&d, |_| false);
        let r = generation_request(GenerationInput {
            doc: &d,
            names: &names,
            allowed: &deps::allowed(&d),
            clojure_version: "1.12.6",
            documentation: None,
            project_code: None,
            seed: 1,
        });
        let text = all_text(&r);
        assert!(
            text.contains("(defn label\n  \"Names an order.\"\n  [order]\n  …)"),
            "{text}"
        );
        assert!(!text.contains("[input]"), "{text}");
        assert!(!text.contains("Write no comments."), "{text}");
    }

    #[test]
    fn usable_project_namespaces_are_allowed_but_not_listed_in_the_task() {
        let mut d = doc();
        d.project = Some(clojurecraft_domain::ProjectBinding {
            namespace: "shop.orders".into(),
            function: "label".into(),
            helpers: Vec::new(),
            arguments: 1,
            file: "(ns shop.orders (:require [clojure.string :as str]))\n".into(),
            usable: vec!["shop.util".into(), "xlib.ids".into()],
        });
        let names = Names::for_document(&d, |_| false);
        let allowed = deps::allowed(&d);
        assert!(allowed.contains("shop.util") && allowed.contains("xlib.ids"));
        let text = generation_task(&GenerationInput {
            doc: &d,
            names: &names,
            allowed: &allowed,
            clojure_version: "1.12.6",
            documentation: None,
            project_code: None,
            seed: 1,
        });
        assert!(
            !text.contains("shop.util") && !text.contains("xlib.ids"),
            "{text}"
        );
        assert!(text.contains("clojure.string"), "{text}");
    }

    #[test]
    fn generation_requests_carry_the_whole_specification_and_constraints() {
        let mut d = doc();
        let mut q = Clarification::open("Ties?", "");
        q.answer("by id");
        d.clarifications.push(q);
        let names = Names::for_document(&d, |_| false);
        assert_eq!(names.function, "order-totals");
        let allowed = BTreeSet::from(["clojure.string".to_owned()]);
        let docs = Section::optional("Clojure documentation", DOCUMENTATION_DROP_ORDER, "[excerpt 1] …");
        let r = generation_request(GenerationInput {
            doc: &d,
            names: &names,
            allowed: &allowed,
            clojure_version: "1.12.6",
            documentation: Some(docs),
            project_code: None,
            seed: 7,
        });
        assert_eq!(r.role, Role::Generation);
        assert_eq!(r.seed, Some(7));
        assert!(
            r.schema.is_none(),
            "answers are plain text constrained by the grammar"
        );
        assert_eq!(r.grammar, Some(crate::grammar::generation()));
        let t = all_text(&r);
        for needle in [
            "Clojure 1.12.6",
            "(ns order-totals.core)",
            "(order-totals [input] …)",
            "clojure.core and clojure.string (as str)",
            "clojure.walk (as walk)",
            "needs the user's approval",
            "Sum amounts per customer",
            "1. group\n2. sum",
            "Example 1",
            "Q: Ties?\nA: by id",
            "[excerpt 1]",
            "Java interop is allowed",
            ":import clause",
        ] {
            assert!(t.contains(needle), "missing {needle:?}");
        }
        assert!(!t.contains("no Java interop"), "{t}");
        let docs = r
            .sections
            .iter()
            .find(|s| s.label == "Clojure documentation")
            .unwrap();
        assert!(matches!(
            docs.priority,
            clojurecraft_inference::Priority::Optional(_)
        ));
        assert!(
            r.sections
                .iter()
                .filter(|s| s.label != "Clojure documentation" && s.label != crate::idioms::LABEL)
                .all(|s| s.priority == clojurecraft_inference::Priority::Required)
        );
    }

    #[test]
    fn a_fix_continues_the_conversation_after_an_analysis() {
        let d = doc();
        let names = Names::for_document(&d, |_| false);
        let allowed = BTreeSet::new();
        let input = || GenerationInput {
            doc: &d,
            names: &names,
            allowed: &allowed,
            clojure_version: "1.12.6",
            documentation: Some(Section::optional(
                "Clojure documentation",
                DOCUMENTATION_DROP_ORDER,
                "[excerpt 1]",
            )),
            project_code: None,
            seed: 9,
        };
        let focus = Focus::Call {
            label: "Example 1".into(),
            input: "[]".into(),
        };
        let (feedback1, fix1) = feedback(
            "(ns order-totals.core)\n(defn order-totals [x] x)",
            "Example 1 — failed",
            Some(&focus),
            true,
            true,
        );
        let (feedback2, fix2) = feedback(
            "(ns order-totals.core)\n(defn order-totals [x] {})",
            "Example 2 — failed",
            None,
            true,
            false,
        );
        let mut turns = vec![
            Turn {
                answer: "PLAN: a\n```clojure\n(ns order-totals.core)\n(defn order-totals [x] x)\n```".into(),
                feedback: feedback1,
                analysis: Some("x is returned unchanged".into()),
                fix: fix1,
            },
            Turn {
                answer: "```clojure\n(defn order-totals [x] {})\n```".into(),
                feedback: feedback2,
                analysis: None,
                fix: fix2,
            },
        ];
        // First the analysis: the conversation ends with the reply, without a grammar.
        let a = analysis_request(input(), &turns, ToolSet::default());
        assert_eq!(a.grammar, None);
        assert_eq!(a.max_output_tokens, Some(ANALYSIS_TOKENS));
        let opening = generation_request(input()).sections;
        assert_eq!(
            &a.sections[..opening.len()],
            &opening[..],
            "the cached opening is reused"
        );
        let rest = &a.sections[opening.len()..];
        let placements: Vec<Placement> = rest.iter().map(|s| s.placement).collect();
        use Placement::{Assistant, User};
        assert_eq!(placements, [Assistant, User, Assistant, User, Assistant, User]);
        assert!(matches!(
            rest[0].priority,
            clojurecraft_inference::Priority::Optional(HISTORY_DROP_ORDER)
        ));
        assert_eq!(rest[5].priority, clojurecraft_inference::Priority::Required);
        let last = &rest[5].text;
        for needle in [
            "(defn order-totals [x] {})",
            "Example 2 — failed",
            "Do not write code yet",
        ] {
            assert!(last.contains(needle), "missing {needle:?} in {last}");
        }
        assert!(!last.contains("clojure_eval"), "no tool, no hint");
        assert!(rest[1].text.contains("Find out why Example 1 (input [])"));
        assert!(rest[1].text.contains("clojure_eval"));
        assert!(rest[3].text.contains("Now fix the code"));

        // Then the fix: the analysis and the request for changed forms, with the grammar.
        turns[1].analysis = Some("returns an empty map".into());
        let f = fix_request(input(), &turns);
        assert_eq!((f.seed, f.temperature), (Some(9), Some(FIX_TEMPERATURE)));
        assert_eq!(f.grammar, Some(crate::grammar::fix()));
        let rest = &f.sections[opening.len()..];
        assert_eq!(rest.len(), 8);
        assert_eq!(rest[6].text, "returns an empty map");
        assert!(rest[7].text.contains("only the top-level forms you change"));
    }

    #[test]
    fn unreadable_code_asks_for_the_complete_namespace() {
        let (f, fix) = feedback(
            "(ns a",
            "Parse — failed",
            Some(&Focus::Check("the parse check".into())),
            false,
            false,
        );
        assert!(fix.contains("the complete namespace"));
        assert!(f.contains("Find out why the parse check failed."));
        assert!(f.contains("quote the reported problem and its line"));
        assert!(!f.contains("part of the input"));
    }

    #[test]
    fn known_errors_come_with_a_hint() {
        let code = "(ns a (:import [java.time Duration]))\n\
                    (defn f [d e] (if (pos? (Duration/compareTo d e)) d e))";
        let failures = "Load — error\n  line 2: IllegalArgumentException: No matching method compareTo \
                        found taking 2 args for class java.time.Duration";
        let (text, _) = feedback(code, failures, None, true, false);
        assert!(
            text.contains("(Duration/compareTo …) calls a static method"),
            "{text}"
        );
        assert!(text.contains("(.compareTo d …)"), "{text}");
        let (text, _) = feedback(
            "(ns a)\n(defn f [x] (if x 1 2 3))",
            "line 2: RuntimeException: Too many arguments to if",
            None,
            true,
            false,
        );
        assert!(text.contains("use cond"), "{text}");
        let (text, _) = feedback(
            "(ns a)\n(defn f [a b] (max a b))",
            "line 2: ClassCastException: class java.time.Duration cannot be cast to class \
             java.lang.Number (java.time.Duration and java.lang.Number are in module java.base)",
            None,
            true,
            false,
        );
        assert!(text.contains("a Duration is not one"), "{text}");
        let (text, _) = feedback(
            "(ns a)\n(defn f [n] (-> n dec + 2))",
            "ClassCastException: class java.lang.Long cannot be cast to class clojure.lang.IFn (x)",
            None,
            true,
            false,
        );
        assert!(text.contains("A Long value was called as a function"), "{text}");
        let (text, _) = feedback(
            "(ns a)\n(defn- max-delay [m] 1)\n(defn f [ms] (reduce (fn [max-delay m] (max-delay m)) nil ms))",
            "NullPointerException: Cannot invoke \"clojure.lang.IFn.invoke(Object)\" because \"max_delay\" is null",
            None,
            true,
            false,
        );
        assert!(
            text.contains("A local named max-delay hides the function max-delay"),
            "{text}"
        );
        let (plain, _) = feedback(
            "(ns a)\n(defn f [x] x)",
            "example 1: expected 1 but got 2",
            None,
            true,
            false,
        );
        assert!(!plain.contains("Hint"), "{plain}");
    }

    #[test]
    fn a_contract_for_several_arguments_describes_them_by_position_not_as_keys() {
        let mut d = doc();
        assert!(!all_text(&contract_request(&d)).contains("arguments"));
        d.project = Some(clojurecraft_domain::ProjectBinding {
            namespace: "shop.orders".into(),
            function: "price".into(),
            helpers: Vec::new(),
            arguments: 2,
            file: "(ns shop.orders)\n".into(),
            usable: Vec::new(),
        });
        let text = all_text(&contract_request(&d));
        assert!(text.contains("vector of the function's 2 arguments"), "{text}");
    }

    #[test]
    fn a_contract_proposal_request_does_not_include_a_pending_proposal() {
        let mut d = doc();
        let mut c = Contract::empty(ContractStatus::Proposed);
        c.notes = "OLD PROPOSAL".into();
        d.contract = Some(c);
        assert!(!all_text(&contract_request(&d)).contains("OLD PROPOSAL"));
    }

    #[test]
    fn explanation_steps_are_numbered_like_the_specification() {
        let d = doc();
        let r = explanation_request(&d, "(ns a)");
        let t = all_text(&r);
        assert!(t.contains("each of the 2 transformation steps"));
        assert!(t.contains("1. group\n2. sum"));
        assert_eq!(numbered_steps(&d).len(), 2);
    }

    #[test]
    fn planning_needs_a_description_or_steps() {
        let mut d = FunctionDocument::new("f");
        d.examples = vec![Example::direct("", Edn::new("1"), Edn::new("2"))];
        assert!(planning_request(&d).is_none());
        assert!(planning_request(&doc()).is_some());
    }
}
