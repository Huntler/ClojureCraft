//! The model requests of the specification workflow. Each request is a role with its own task
//! text, the specification as required sections, and the schema of the expected answer.

use std::collections::BTreeSet;

use clojurecraft_domain::{ClarificationStatus, FunctionDocument, Step};
use clojurecraft_inference::{GenerateRequest, Placement, Role, Section};

use crate::answers::{self, InterpretationAnswer};
use crate::deps;
use crate::grammar;
use crate::naming;
use crate::spec;

/// Drop order of retrieved documentation in a generation request.
pub const DOCUMENTATION_DROP_ORDER: u32 = 1;

/// The namespace and function the generated code should define.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub namespace: String,
    pub function: String,
}

impl Names {
    /// `is_core` reports whether a symbol names a `clojure.core` var.
    pub fn for_document(doc: &FunctionDocument, is_core: impl Fn(&str) -> bool) -> Self {
        let function = naming::function_symbol(&doc.name, is_core);
        Self {
            namespace: naming::namespace_for(&function),
            function,
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
    let text = format!(
        "Propose a contract for the transformation below: the input and output value types; for maps \
         (or collections of maps) their keys, whether each is required, its value type, and what nil \
         means for it (\"default\" with an EDN default value, \"treat_as_missing\", \"pass_through\", \
         \"disallowed\", or \"unspecified\"); and the output order. Write keys as EDN, for example \
         :customer-id. Derive everything from the specification and the examples; where they do not \
         decide something, use \"unspecified\" or \"any\" instead of inventing a requirement. The user \
         reviews the proposal before it applies. Answer with JSON only. {PART_NAMES}"
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
    let allowed = if input.allowed.is_empty() {
        "clojure.core only".to_owned()
    } else {
        format!("clojure.core and {}", namespace_list(input.allowed))
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
    let Names { namespace, function } = input.names;
    format!(
        "Write a Clojure {version} namespace implementing the transformation specified below.\n\
         Rules:\n\
         - Start with (ns {namespace}) and require namespaces only in its :require clause, as \
         [namespace :as alias].\n\
         - Define exactly one public function, ({function} [input] …), taking one argument: the input \
         value of an example. It must return the expected output of every example and accepted test, \
         and follow the steps, the contract, and the user's answers.\n\
         - Define helpers with defn-. Top-level forms other than defn, defn-, def, and declare are \
         not allowed. Write no comments and no example calls.\n\
         - Use {allowed}.{others}\n\
         - The function must be pure: no I/O, printing, randomness, time, atoms, futures, agents, or \
         dynamic vars; no eval, read-string, resolve, or other dynamic evaluation; no Java interop \
         except static methods of Math, Long, Integer, Double, Character, and Boolean.\n\
         - Prefer clear, idiomatic clojure.core functions. Return eager collections (vectors, maps, \
         sets) unless the examples show a sequence.\n\
         Answer in exactly this format and nothing else:\n\
         PLAN: <at most six short lines, no code and no backticks: trace example 1 by hand. Take one element of its \
         input (one line of text, one map, or one item) and quote each part of it next to the output \
         value it becomes, for example 'id=7' -> :id 7, noting conversions (text to number; integer \
         or decimal). Say how the other examples differ (missing parts, empty input), then name the \
         core functions you will use.>\n\
         ```clojure\n\
         <the complete source, following the plan>\n\
         ```\n\
         {steps}ASSUMPTIONS:\n\
         - <a decision you made where the specification is silent; at most four, or none>",
        version = input.clojure_version,
        steps = if numbered_steps(input.doc).is_empty() {
            ""
        } else {
            "STEPS:\n\
             STEP <n>: <for each numbered transformation step, the code from the source that carries \
             it out, copied exactly: one expression or one stage of a pipeline, on one line>\n"
        },
    )
}

pub fn generation_request(input: GenerationInput<'_>) -> GenerateRequest {
    let mut sections = spec::specification_sections(input.doc);
    sections.extend(crate::idioms::section(input.doc));
    sections.extend(input.documentation.clone());
    let mut sections_all = vec![task(generation_task(&input))];
    sections_all.extend(sections);
    let mut r = GenerateRequest::new(Role::Generation, sections_all);
    r.seed = Some(input.seed);
    r.grammar = Some(grammar::generation(numbered_steps(input.doc).len()));
    r
}

/// Sampling temperature of repairs: higher than a first generation, because a model that sees
/// its own failing code tends to return it nearly unchanged.
pub const REPAIR_TEMPERATURE: f32 = 0.5;

/// Sampling temperature of further candidates for the same generation request: high enough that
/// a different seed gives a different approach, not the first candidate again.
pub const SAMPLE_TEMPERATURE: f32 = 0.6;

/// Prompt label of the code a repair starts from.
pub const PREVIOUS_CODE_LABEL: &str = "Previous code";
/// Prompt label of the failed checks a repair must fix.
pub const CHECK_RESULTS_LABEL: &str = "Check results";

/// A request to fix code that failed automatic validation. The specification, the code, and the
/// failures are required sections; documentation stays optional.
pub fn repair_request(
    input: GenerationInput<'_>,
    code: &str,
    failures: &str,
    attempt: u32,
    attempts: u32,
    // The attempt being repaired and an earlier one that failed in exactly the same way.
    repeated: Option<(u32, u32)>,
) -> GenerateRequest {
    let mut sections = spec::specification_sections(input.doc);
    sections.push(Section::required(
        PREVIOUS_CODE_LABEL,
        Placement::User,
        format!("Your previous implementation:\n{}", code.trim_end()),
    ));
    sections.push(Section::required(
        CHECK_RESULTS_LABEL,
        Placement::User,
        format!(
            "The previous implementation was run in a sandbox on every example and accepted test. \
             These checks failed (examples and tests are named as in the specification):\n{failures}"
        ),
    ));
    sections.extend(crate::idioms::section(input.doc));
    sections.extend(input.documentation.clone());
    let repeated = repeated.map_or_else(String::new, |(this, n)| {
        format!(
            " Attempt {this} failed in exactly the same way as attempt {n}, so small adjustments are not \
             enough: re-read the examples and expected outputs, and implement the transformation with a \
             different approach."
        )
    });
    let text = format!(
        "This is attempt {attempt} of {attempts}.{repeated} The previous implementation, shown below, failed \
         automatic checks. Fix it so that every check passes. The examples, accepted tests, contract, \
         and answers are fixed requirements: change the code, never what it must return. Keep what \
         already works and change only what the failures require; if the approach cannot work, \
         rewrite it. Compare the expected and actual values carefully: vectors, lists, and sequences \
         with the same elements in order are equal, but an integer never equals a floating-point \
         number (1 is not 1.0). A failing check may list the values after each transformation step \
         and the values of locals, recorded while that call ran: find the first step whose value is \
         not what the step should produce for that input, and fix the code of that step.\n\n{}",
        generation_task(&input)
    );
    let mut all = vec![task(text)];
    all.extend(sections);
    let mut r = GenerateRequest::new(Role::Generation, all);
    r.seed = Some(input.seed);
    r.temperature = Some(REPAIR_TEMPERATURE);
    r.grammar = Some(grammar::generation(numbered_steps(input.doc).len()));
    r
}

pub fn explanation_request(doc: &FunctionDocument, code: &str) -> GenerateRequest {
    let steps = numbered_steps(doc);
    let mut sections: Vec<Section> = spec::specification_sections(doc)
        .into_iter()
        .filter(|s| matches!(s.label.as_str(), "Description" | "Transformation steps"))
        .collect();
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
         it. In \"symbols\", list at most eight standard-library functions and macros used and what \
         each does here. Answer with JSON only."
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
            seed: 7,
        });
        assert_eq!(r.role, Role::Generation);
        assert_eq!(r.seed, Some(7));
        assert!(
            r.schema.is_none(),
            "answers are plain text constrained by the grammar"
        );
        assert_eq!(
            r.grammar,
            Some(crate::grammar::generation(numbered_steps(&d).len()))
        );
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
        ] {
            assert!(t.contains(needle), "missing {needle:?}");
        }
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
    fn repair_requests_carry_code_failures_and_the_whole_specification() {
        let d = doc();
        let names = Names::for_document(&d, |_| false);
        let allowed = BTreeSet::new();
        let r = repair_request(
            GenerationInput {
                doc: &d,
                names: &names,
                allowed: &allowed,
                clojure_version: "1.12.6",
                documentation: Some(Section::optional(
                    "Clojure documentation",
                    DOCUMENTATION_DROP_ORDER,
                    "[excerpt 1]",
                )),
                seed: 9,
            },
            "(ns order-totals.core)\n(defn order-totals [x] x)\n",
            "Example 1 — failed\n  Expected: {}\n  Actual: []",
            3,
            4,
            Some((2, 1)),
        );
        assert_eq!((r.role, r.seed), (Role::Generation, Some(9)));
        assert_eq!(r.temperature, Some(REPAIR_TEMPERATURE));
        assert_eq!(
            r.grammar,
            Some(crate::grammar::generation(numbered_steps(&d).len()))
        );
        let t = all_text(&r);
        for needle in [
            "attempt 3 of 4",
            "Attempt 2 failed in exactly the same way as attempt 1",
            "never what it must return",
            "(defn order-totals [x] x)",
            "Actual: []",
            "Sum amounts per customer",
            "(ns order-totals.core)",
        ] {
            assert!(t.contains(needle), "missing {needle:?}");
        }
        for label in [PREVIOUS_CODE_LABEL, CHECK_RESULTS_LABEL, "Example 1"] {
            let s = r.sections.iter().find(|s| s.label == label).unwrap();
            assert_eq!(s.priority, clojurecraft_inference::Priority::Required, "{label}");
        }
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
