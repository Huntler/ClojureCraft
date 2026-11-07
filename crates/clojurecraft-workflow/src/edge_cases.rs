//! Edge-case tests: proposing cases from the specification, reviewing them, and turning accepted
//! ones into regression tests.
//!
//! Cases are proposed by the model from the specification and contract only; the implementation
//! is never shown to it, and an expected output is never taken from running the code. Every
//! proposal starts as [`TestStatus::Proposed`] with [`TestOrigin::Model`] and takes no part in
//! validation until the engineer accepts it. A case whose result the specification does not
//! determine carries an open question ([`Expectation::NeedsClarification`]) and cannot be
//! accepted, so an ambiguity never becomes a hidden requirement; it can be turned into a question
//! on the specification instead.

use clojurecraft_domain::{
    CheckStatus, CheckSubject, Clarification, ClarificationId, ClarificationStatus, DomainError, Edn,
    Expectation, FunctionDocument, TestCase, TestCaseId, TestOrigin, TestStatus, ValidationReport,
};
use clojurecraft_edn::{TokenKind, parse_value, tokenize};
use clojurecraft_inference::{Field, GenerateRequest, Placement, Role, Schema, Section};
use serde::Deserialize;
use serde_json::Value;

use crate::spec;

/// Cases asked for in one proposal.
pub const MAX_CASES: usize = 10;
/// Answer budget of a proposal: EDN values and prose for every case.
pub const PROPOSAL_TOKENS: u32 = 3000;

/// The kinds of edge cases asked for, as named in the answer schema.
pub const CATEGORIES: [&str; 8] = [
    "empty",
    "missing",
    "nil",
    "duplicate",
    "boundary",
    "ordering",
    "nested",
    "other",
];

fn category_label(category: &str) -> &'static str {
    match category {
        "empty" => "Empty input",
        "missing" => "Missing keys",
        "nil" => "Nil values",
        "duplicate" => "Duplicates",
        "boundary" => "Boundary values",
        "ordering" => "Ordering and ties",
        "nested" => "Nested data",
        _ => "Other",
    }
}

/// Where a case stands in review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseState {
    /// Accepted with a defined expectation: run by every validation as a regression test.
    Regression,
    /// Has an expected output that nobody has accepted yet.
    AwaitingAcceptance,
    /// The specification does not determine the result; the engineer must decide.
    NeedsClarification,
    Rejected,
}

impl CaseState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Regression => "Accepted · regression test",
            Self::AwaitingAcceptance => "Proposed expectation · awaiting acceptance",
            Self::NeedsClarification => "Ambiguous · needs your decision",
            Self::Rejected => "Rejected",
        }
    }
}

pub fn state(case: &TestCase) -> CaseState {
    match (case.status, &case.expectation) {
        (TestStatus::Rejected, _) => CaseState::Rejected,
        (_, Expectation::NeedsClarification { .. }) => CaseState::NeedsClarification,
        (TestStatus::Accepted, Expectation::Value { .. }) => CaseState::Regression,
        (TestStatus::Proposed, Expectation::Value { .. }) => CaseState::AwaitingAcceptance,
    }
}

/// Whether the case came from the model and has not been accepted, so it must be shown as the
/// model's proposal rather than as a requirement.
pub fn is_unaccepted_proposal(case: &TestCase) -> bool {
    case.origin != TestOrigin::User && case.status != TestStatus::Accepted
}

/// How many cases are in each state: (regression, awaiting acceptance, needs clarification).
pub fn counts(doc: &FunctionDocument) -> (usize, usize, usize) {
    let mut out = (0, 0, 0);
    for t in &doc.tests {
        match state(t) {
            CaseState::Regression => out.0 += 1,
            CaseState::AwaitingAcceptance => out.1 += 1,
            CaseState::NeedsClarification => out.2 += 1,
            CaseState::Rejected => {}
        }
    }
    out
}

// ---- proposals ---------------------------------------------------------------------------------

pub fn proposal_schema() -> Schema {
    Schema::object(vec![Field::required(
        "cases",
        Schema::array(
            Schema::object(vec![
                Field::required("name", Schema::bounded_string(80)),
                Field::required("category", Schema::one_of(&CATEGORIES)),
                Field::required("input", Schema::string()).describe("one EDN value"),
                Field::required("outcome", Schema::one_of(&["defined", "ambiguous"])),
                Field::required("expected", Schema::string())
                    .describe("one EDN value when the outcome is defined, otherwise empty"),
                Field::required("question", Schema::bounded_string(300))
                    .describe("what the user must decide when the outcome is ambiguous, otherwise empty"),
                Field::required("why", Schema::bounded_string(300)),
            ]),
            Some(MAX_CASES),
        ),
    )])
}

/// A request for edge cases of `doc`'s specification. The implementation is deliberately not
/// part of it, so expectations come from the specification rather than from the code.
pub fn proposal_request(doc: &FunctionDocument) -> GenerateRequest {
    let mut sections = vec![Section::required(
        "Task",
        Placement::User,
        format!(
            "Propose at most {MAX_CASES} edge-case tests for the transformation specified below. \
             Cover what is relevant to it: empty input, missing keys, nil values, duplicates, \
             boundary values (zero, negative numbers, a single element, large values), ordering and \
             ties, and nested data. Each case has one input in \"input\" as a single EDN value.\n\
             Derive each expected output only from the description, steps, contract, answers, \
             examples, and accepted tests. Never guess what an implementation would happen to \
             return. When the specification determines the output, set \"outcome\" to \"defined\" and \
             write it in \"expected\" as a single EDN value. When it does not, set \"outcome\" to \
             \"ambiguous\", leave \"expected\" empty, and write in \"question\" what the user must \
             decide. In \"why\", say which part of the specification decides the result or what is \
             unclear.\n\
             Do not repeat the examples, the accepted tests, or the cases listed as already \
             considered. Answer with JSON only."
        ),
    )];
    sections.extend(spec::specification_sections(doc));
    let considered: Vec<String> = doc
        .tests
        .iter()
        .filter(|t| t.status != TestStatus::Accepted)
        .map(|t| format!("- {}", t.input.as_str().trim()))
        .collect();
    if !considered.is_empty() {
        sections.push(Section::required(
            "Considered cases",
            Placement::User,
            format!(
                "Inputs already proposed or rejected (do not propose them again):\n{}",
                considered.join("\n")
            ),
        ));
    }
    let mut r = GenerateRequest::new(Role::Interpretation, sections);
    r.schema = Some(proposal_schema());
    r.max_output_tokens = Some(PROPOSAL_TOKENS);
    r
}

/// Why edge cases cannot be generated from the examples alone, if they cannot.
pub fn examples_blocker(doc: &FunctionDocument) -> Option<&'static str> {
    spec::usable_examples(doc)
        .is_empty()
        .then_some("Add at least one example with an input and an expected output first.")
}

/// A request for edge cases derived only from the title, the description, and the examples.
/// Steps, contract, answers, accepted tests, and the implementation are deliberately left out,
/// so the cases test what the examples show rather than what was written down elsewhere.
pub fn example_proposal_request(doc: &FunctionDocument) -> GenerateRequest {
    let mut sections = vec![Section::required(
        "Task",
        Placement::User,
        format!(
            "Propose at most {MAX_CASES} edge-case tests for the transformation described below. You \
             are given only its title, description, and input/output examples. Cover what is \
             relevant to it: empty input, missing keys, nil values, duplicates, boundary values \
             (zero, negative numbers, a single element, large values), ordering and ties, and nested \
             data. Each case has one input in \"input\" as a single EDN value, shaped like the \
             example inputs.\n\
             Derive each expected output only from the title, the description, and the examples. \
             Never guess what an implementation would happen to return. When they determine the \
             output, set \"outcome\" to \"defined\" and write it in \"expected\" as a single EDN value \
             in the same shape as the example outputs. When they do not, set \"outcome\" to \
             \"ambiguous\", leave \"expected\" empty, and write in \"question\" what the user must \
             decide. In \"why\", say which example or which part of the description decides the \
             result, or what is unclear.\n\
             Do not repeat the examples or the cases listed as already considered. Answer with JSON \
             only."
        ),
    )];
    let name = doc.name.trim();
    sections.push(Section::required(
        "Title",
        Placement::User,
        format!("Title: {}", if name.is_empty() { "(none)" } else { name }),
    ));
    let description = doc.description.trim();
    sections.push(Section::required(
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
    for (label, example, input) in spec::usable_examples(doc) {
        sections.push(Section::required(
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
    // Every existing case is listed by input only, so nothing is repeated while no expectation
    // other than the examples' informs the new ones.
    let considered: Vec<String> = doc
        .tests
        .iter()
        .map(|t| t.input.as_str().trim())
        .filter(|input| !input.is_empty())
        .map(|input| format!("- {input}"))
        .collect();
    if !considered.is_empty() {
        sections.push(Section::required(
            "Considered cases",
            Placement::User,
            format!(
                "Inputs already considered (do not propose them again):\n{}",
                considered.join("\n")
            ),
        ));
    }
    let mut r = GenerateRequest::new(Role::Interpretation, sections);
    r.schema = Some(proposal_schema());
    r.max_output_tokens = Some(PROPOSAL_TOKENS);
    r
}

#[derive(Deserialize)]
struct RawCase {
    name: String,
    category: String,
    input: String,
    outcome: String,
    expected: String,
    question: String,
    why: String,
}

#[derive(Deserialize)]
struct RawCases {
    cases: Vec<RawCase>,
}

/// Proposed cases and remarks about proposals that were left out.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Proposals {
    pub cases: Vec<TestCase>,
    pub notes: Vec<String>,
}

impl Proposals {
    pub fn ambiguous(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| matches!(c.expectation, Expectation::NeedsClarification { .. }))
            .count()
    }
}

fn shorten(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

/// Whether `text` is exactly one EDN value without errors; `Err` explains why not.
pub fn single_value(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("it is empty".into());
    }
    let parsed = parse_value(text);
    if let Some(d) = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == clojurecraft_edn::Severity::Error)
    {
        return Err(d.message.clone());
    }
    if parsed.values.len() != 1 {
        return Err("it is not exactly one value".into());
    }
    Ok(())
}

/// EDN text with whitespace, commas, and comments removed between tokens, so that differently
/// formatted copies of one value compare equal.
fn normalized(text: &str) -> String {
    tokenize(text)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .map(|t| &text[t.span])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Inputs the function already has a case or example for.
fn known_inputs(doc: &FunctionDocument) -> Vec<String> {
    let mut known: Vec<String> = doc.tests.iter().map(|t| normalized(t.input.as_str())).collect();
    known.extend(
        spec::usable_examples(doc)
            .into_iter()
            .map(|(_, _, input)| normalized(input)),
    );
    known
}

/// Converts a validated proposal answer into proposed cases, leaving out cases with unreadable
/// EDN and inputs the function already has.
pub fn parse_proposals(v: &Value, doc: &FunctionDocument) -> Result<Proposals, String> {
    let raw: RawCases =
        serde_json::from_value(v.clone()).map_err(|e| format!("the answer could not be read: {e}"))?;
    let mut known = known_inputs(doc);
    let mut out = Proposals::default();
    let mut repeated = 0;
    for (i, c) in raw.cases.into_iter().take(MAX_CASES).enumerate() {
        let name = match shorten(&c.name, 80) {
            n if n.is_empty() => format!("{} {}", category_label(&c.category), i + 1),
            n => n,
        };
        if let Err(e) = single_value(&c.input) {
            out.notes.push(format!(
                "“{name}” was left out: its input is not valid EDN ({e})."
            ));
            continue;
        }
        let key = normalized(&c.input);
        if known.contains(&key) {
            repeated += 1;
            continue;
        }
        let expectation = if c.outcome == "defined" {
            if let Err(e) = single_value(&c.expected) {
                out.notes.push(format!(
                    "“{name}” was left out: its expected output is not valid EDN ({e})."
                ));
                continue;
            }
            Expectation::Value {
                expected: Edn::new(c.expected.trim()),
            }
        } else {
            let question = match shorten(&c.question, 300) {
                q if q.is_empty() => "What should the function return for this input?".to_owned(),
                q => q,
            };
            Expectation::NeedsClarification { question }
        };
        known.push(key);
        let why = shorten(&c.why, 300);
        out.cases.push(TestCase {
            id: TestCaseId::generate(),
            name,
            input: Edn::new(c.input.trim()),
            expectation,
            status: TestStatus::Proposed,
            origin: TestOrigin::Model,
            notes: if why.is_empty() {
                category_label(&c.category).to_owned()
            } else {
                format!("{}. {why}", category_label(&c.category))
            },
        });
    }
    if repeated > 0 {
        out.notes.push(format!(
            "{repeated} proposed case{} repeated an existing example or case and {} left out.",
            if repeated == 1 { "" } else { "s" },
            if repeated == 1 { "was" } else { "were" }
        ));
    }
    Ok(out)
}

// ---- review ------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaseError {
    NotFound,
    InvalidInput(String),
    InvalidExpected(String),
    /// The case still has an open question.
    Ambiguous,
}

impl std::fmt::Display for CaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("the case no longer exists"),
            Self::InvalidInput(e) => write!(f, "the input is not a valid EDN value: {e}"),
            Self::InvalidExpected(e) => write!(f, "the expected output is not a valid EDN value: {e}"),
            Self::Ambiguous => {
                f.write_str("its expected output is still an open question; decide it (edit the case) first")
            }
        }
    }
}

fn case_mut(doc: &mut FunctionDocument, id: TestCaseId) -> Result<&mut TestCase, CaseError> {
    doc.tests
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or(CaseError::NotFound)
}

/// Accepts a case as a regression test. Its input and expected output must be valid EDN values,
/// and a case with an open question cannot be accepted.
pub fn accept(doc: &mut FunctionDocument, id: TestCaseId) -> Result<(), CaseError> {
    let case = case_mut(doc, id)?;
    single_value(case.input.as_str()).map_err(CaseError::InvalidInput)?;
    if let Expectation::Value { expected } = &case.expectation {
        single_value(expected.as_str()).map_err(CaseError::InvalidExpected)?;
    }
    case.accept().map_err(|e| match e {
        DomainError::AmbiguousTest(_) => CaseError::Ambiguous,
    })
}

/// Rejects a case; it stays listed (and is not proposed again) but takes no part in validation.
pub fn reject(doc: &mut FunctionDocument, id: TestCaseId) -> Result<(), CaseError> {
    case_mut(doc, id)?.status = TestStatus::Rejected;
    Ok(())
}

/// Returns a rejected case to review.
pub fn reopen(doc: &mut FunctionDocument, id: TestCaseId) -> Result<(), CaseError> {
    let case = case_mut(doc, id)?;
    if case.status == TestStatus::Rejected {
        case.status = TestStatus::Proposed;
    }
    Ok(())
}

/// Removes a case entirely.
pub fn remove(doc: &mut FunctionDocument, id: TestCaseId) -> Option<(usize, TestCase)> {
    let index = doc.tests.iter().position(|t| t.id == id)?;
    Some((index, doc.tests.remove(index)))
}

/// A new, empty case written by the engineer; it is accepted like any other case.
pub fn add(doc: &mut FunctionDocument) -> TestCaseId {
    let case = TestCase {
        id: TestCaseId::generate(),
        name: format!("Case {}", doc.tests.len() + 1),
        input: Edn::default(),
        expectation: Expectation::Value {
            expected: Edn::default(),
        },
        status: TestStatus::Proposed,
        origin: TestOrigin::User,
        notes: String::new(),
    };
    let id = case.id;
    doc.tests.push(case);
    id
}

/// The engineer's changes to a case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseEdit {
    pub name: String,
    pub input: String,
    /// The expected output, or `None` to keep the result an open question.
    pub expected: Option<String>,
    /// The open question, used when `expected` is `None`.
    pub question: String,
}

impl CaseEdit {
    pub fn of(case: &TestCase) -> Self {
        let (expected, question) = match &case.expectation {
            Expectation::Value { expected } => (Some(expected.as_str().to_owned()), String::new()),
            Expectation::NeedsClarification { question } => (None, question.clone()),
        };
        Self {
            name: case.name.clone(),
            input: case.input.as_str().to_owned(),
            expected,
            question,
        }
    }
}

/// Saves the engineer's changes. The input must be a valid EDN value, and so must a given
/// expected output. A model proposal whose input or expectation changed becomes
/// [`TestOrigin::ModelEdited`]; an accepted case whose result becomes an open question returns
/// to review.
pub fn save(doc: &mut FunctionDocument, id: TestCaseId, edit: CaseEdit) -> Result<(), CaseError> {
    single_value(&edit.input).map_err(CaseError::InvalidInput)?;
    let expectation = match &edit.expected {
        Some(e) => {
            single_value(e).map_err(CaseError::InvalidExpected)?;
            Expectation::Value {
                expected: Edn::new(e.trim()),
            }
        }
        None => Expectation::NeedsClarification {
            question: match edit.question.trim() {
                "" => "What should the function return for this input?".to_owned(),
                q => q.to_owned(),
            },
        },
    };
    let case = case_mut(doc, id)?;
    let input = Edn::new(edit.input.trim());
    let material = case.input != input || case.expectation != expectation;
    if material && case.origin == TestOrigin::Model {
        case.origin = TestOrigin::ModelEdited;
    }
    case.name = edit.name.trim().to_owned();
    case.input = input;
    case.expectation = expectation;
    if matches!(case.expectation, Expectation::NeedsClarification { .. })
        && case.status == TestStatus::Accepted
    {
        case.status = TestStatus::Proposed;
    }
    Ok(())
}

/// Adds the open question of an ambiguous case to the specification's questions, so that the
/// decision is made explicitly and becomes part of the specification once answered. Returns the
/// question (an existing one when it was asked already).
pub fn ask_on_specification(
    doc: &mut FunctionDocument,
    id: TestCaseId,
) -> Result<ClarificationId, CaseError> {
    let case = doc.tests.iter().find(|t| t.id == id).ok_or(CaseError::NotFound)?;
    let Expectation::NeedsClarification { question } = &case.expectation else {
        return Err(CaseError::NotFound);
    };
    let question = question.trim().to_owned();
    if let Some(existing) = doc.clarifications.iter().find(|c| {
        c.status != ClarificationStatus::Dismissed && c.question.trim().eq_ignore_ascii_case(&question)
    }) {
        return Ok(existing.id);
    }
    let mut c = Clarification::open(
        question,
        format!(
            "Edge case “{}” with input {}",
            case.name.trim(),
            shorten(case.input.as_str(), 200)
        ),
    );
    c.refers_to = vec!["edge cases".into()];
    let cid = c.id;
    doc.clarifications.push(c);
    Ok(cid)
}

// ---- fixing failing cases ----------------------------------------------------------------------

/// The newest report about exactly the current content, when it failed: the failures a fix
/// starts from. `reports` are newest first.
pub fn failing_report<'a>(
    doc: &FunctionDocument,
    reports: &'a [ValidationReport],
) -> Result<&'a ValidationReport, &'static str> {
    if doc.implementation.is_none() {
        return Err("There is no implementation to fix.");
    }
    let basis = doc.basis();
    match reports.iter().find(|r| r.basis == basis) {
        None => Err("Run the checks on the current code and accepted cases first."),
        Some(r) if r.passed() => Err("Every check passes; there is nothing to fix."),
        Some(r) => Ok(r),
    }
}

/// Accepted cases that did not pass in `report`.
pub fn failing_cases(report: &ValidationReport) -> Vec<TestCaseId> {
    report
        .checks
        .iter()
        .filter(|c| !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped))
        .filter_map(|c| match c.subject {
            CheckSubject::Test(id) => Some(id),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Example, Implementation, Step};
    use serde_json::json;

    use super::*;

    fn doc() -> FunctionDocument {
        let mut d = FunctionDocument::new("Order totals");
        d.description = "Total amounts per customer".into();
        d.steps = vec![Step::new("group orders by :customer"), Step::new("sum :amount")];
        d.examples = vec![Example::direct(
            "",
            Edn::new("[{:customer 1 :amount 2}]"),
            Edn::new("{1 2}"),
        )];
        d.implementation = Some(Implementation::manual(
            "(ns order-totals.core)\n(defn order-totals [x] {})",
        ));
        d
    }

    fn proposal(cases: Value) -> Value {
        json!({ "cases": cases })
    }

    fn case(name: &str, input: &str, outcome: &str, expected: &str, question: &str) -> Value {
        json!({"name": name, "category": "empty", "input": input, "outcome": outcome,
               "expected": expected, "question": question, "why": "the description says so"})
    }

    #[test]
    fn requests_carry_the_specification_but_never_the_code() {
        let mut d = doc();
        d.implementation = Some(Implementation::manual(
            "(ns secret.core) (defn SECRET-CODE [x] x)",
        ));
        d.tests.push(TestCase {
            id: TestCaseId::generate(),
            name: "old".into(),
            input: Edn::new("[:rejected-before]"),
            expectation: Expectation::Value {
                expected: Edn::new("{}"),
            },
            status: TestStatus::Rejected,
            origin: TestOrigin::Model,
            notes: String::new(),
        });
        let r = proposal_request(&d);
        assert_eq!(r.role, Role::Interpretation);
        assert!(r.schema.is_some());
        let text: String = r
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Total amounts per customer"));
        assert!(text.contains("[{:customer 1 :amount 2}]"));
        assert!(text.contains("Never guess what an implementation would"));
        assert!(
            text.contains("[:rejected-before]"),
            "considered inputs are listed"
        );
        assert!(!text.contains("SECRET-CODE"), "the implementation is not sent");
        assert!(
            r.sections
                .iter()
                .all(|s| s.priority == clojurecraft_inference::Priority::Required)
        );
    }

    #[test]
    fn example_requests_carry_only_title_description_and_examples() {
        let mut d = doc();
        d.implementation = Some(Implementation::manual(
            "(ns secret.core) (defn SECRET-CODE [x] x)",
        ));
        d.tests.push(TestCase {
            id: TestCaseId::generate(),
            name: "accepted".into(),
            input: Edn::new("[:accepted-input]"),
            expectation: Expectation::Value {
                expected: Edn::new("{:ACCEPTED-EXPECTATION 1}"),
            },
            status: TestStatus::Accepted,
            origin: TestOrigin::User,
            notes: String::new(),
        });
        let r = example_proposal_request(&d);
        assert_eq!(r.role, Role::Interpretation);
        assert!(r.schema.is_some());
        let text: String = r
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Title: Order totals"));
        assert!(text.contains("Total amounts per customer"));
        assert!(text.contains("[{:customer 1 :amount 2}]"));
        assert!(text.contains("{1 2}"));
        assert!(!text.contains("group orders by :customer"), "steps are not sent");
        assert!(!text.contains("SECRET-CODE"), "the implementation is not sent");
        assert!(text.contains("[:accepted-input]"), "existing inputs are listed");
        assert!(
            !text.contains("ACCEPTED-EXPECTATION"),
            "expectations of accepted tests are not sent"
        );
    }

    #[test]
    fn example_requests_need_a_usable_example() {
        let mut d = doc();
        assert_eq!(examples_blocker(&d), None);
        d.examples.clear();
        assert!(examples_blocker(&d).is_some());
    }

    #[test]
    fn proposals_are_unaccepted_model_cases_and_ambiguities_stay_questions() {
        let d = doc();
        let v = proposal(json!([
            case("no orders", "[]", "defined", "{}", ""),
            case(
                "nil amount",
                "[{:customer 1 :amount nil}]",
                "ambiguous",
                "",
                "Is a nil amount 0?"
            ),
            case("broken", "[{:customer 1", "defined", "{}", ""),
            case("bad expected", "[{:customer 2 :amount 1}]", "defined", "{2 1", ""),
            // Same value as the example, formatted differently: already covered.
            case("dupe", "[ {:customer 1, :amount 2} ]", "defined", "{1 2}", ""),
            case("dupe of first", "[ ]", "defined", "{}", ""),
        ]));
        let p = parse_proposals(&v, &d).unwrap();
        let names: Vec<&str> = p.cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["no orders", "nil amount"]);
        assert_eq!(p.ambiguous(), 1);
        for c in &p.cases {
            assert_eq!((c.status, c.origin), (TestStatus::Proposed, TestOrigin::Model));
            assert!(!c.is_active(), "a proposal takes no part in validation");
            assert!(is_unaccepted_proposal(c));
        }
        assert_eq!(state(&p.cases[0]), CaseState::AwaitingAcceptance);
        assert_eq!(state(&p.cases[1]), CaseState::NeedsClarification);
        assert!(p.notes.iter().any(|n| n.contains("“broken”")));
        assert!(p.notes.iter().any(|n| n.contains("“bad expected”")));
        assert!(p.notes.iter().any(|n| n.contains("2 proposed cases repeated")));

        // Adding proposals does not change what validation checks.
        let mut with = d.clone();
        with.tests.extend(p.cases);
        assert_eq!(with.basis(), d.basis());
        assert_eq!(counts(&with), (0, 1, 1));
    }

    #[test]
    fn only_defined_valid_cases_can_be_accepted_and_then_become_regression_tests() {
        let mut d = doc();
        let p = parse_proposals(
            &proposal(json!([
                case("no orders", "[]", "defined", "{}", ""),
                case(
                    "nil amount",
                    "[{:customer 1 :amount nil}]",
                    "ambiguous",
                    "",
                    "Is a nil amount 0?"
                ),
            ])),
            &d,
        )
        .unwrap();
        let (defined, ambiguous) = (p.cases[0].id, p.cases[1].id);
        d.tests.extend(p.cases);
        let before = d.tests_fingerprint();

        assert_eq!(accept(&mut d, ambiguous), Err(CaseError::Ambiguous));
        assert_eq!(
            d.tests_fingerprint(),
            before,
            "an ambiguity is never a requirement"
        );

        accept(&mut d, defined).unwrap();
        assert_ne!(d.tests_fingerprint(), before);
        let t = d.test(defined).unwrap();
        assert_eq!(state(t), CaseState::Regression);
        assert!(!is_unaccepted_proposal(t));
        // The accepted case is part of every later validation and generation.
        let calls = crate::validation::planned_calls(&d);
        assert!(
            calls
                .iter()
                .any(|c| c.subject == CheckSubject::Test(defined) && c.expected == "{}")
        );
        let sections = spec::specification_sections(&d);
        assert!(sections.iter().any(|s| s.label == "Test no orders"));

        // Deciding the ambiguous case by giving it an expected value makes it acceptable, and
        // marks it as changed by the engineer.
        let mut edit = CaseEdit::of(d.test(ambiguous).unwrap());
        edit.expected = Some("{1 0}".into());
        save(&mut d, ambiguous, edit).unwrap();
        let t = d.test(ambiguous).unwrap();
        assert_eq!(t.origin, TestOrigin::ModelEdited);
        assert_eq!(state(t), CaseState::AwaitingAcceptance);
        accept(&mut d, ambiguous).unwrap();
        assert_eq!(counts(&d), (2, 0, 0));

        // Rejecting removes a case from the regression tests.
        reject(&mut d, defined).unwrap();
        assert!(!d.test(defined).unwrap().is_active());
        reopen(&mut d, defined).unwrap();
        assert_eq!(d.test(defined).unwrap().status, TestStatus::Proposed);
    }

    #[test]
    fn invalid_edits_are_refused_and_reopening_a_question_returns_the_case_to_review() {
        let mut d = doc();
        let id = add(&mut d);
        assert!(matches!(accept(&mut d, id), Err(CaseError::InvalidInput(_))));
        let mut edit = CaseEdit::of(d.test(id).unwrap());
        edit.input = "[1 2".into();
        assert!(matches!(
            save(&mut d, id, edit.clone()),
            Err(CaseError::InvalidInput(_))
        ));
        edit.input = "[]".into();
        edit.expected = Some("{} {}".into());
        assert!(matches!(
            save(&mut d, id, edit.clone()),
            Err(CaseError::InvalidExpected(_))
        ));
        edit.expected = Some("{}".into());
        save(&mut d, id, edit.clone()).unwrap();
        accept(&mut d, id).unwrap();
        assert_eq!(d.test(id).unwrap().origin, TestOrigin::User);

        edit.expected = None;
        save(&mut d, id, edit).unwrap();
        let t = d.test(id).unwrap();
        assert_eq!(state(t), CaseState::NeedsClarification);
        assert_eq!(t.status, TestStatus::Proposed);
        assert!(!t.is_active());
    }

    #[test]
    fn an_ambiguous_case_can_become_a_question_on_the_specification() {
        let mut d = doc();
        let p = parse_proposals(
            &proposal(json!([case(
                "nil amount",
                "[{:customer 1 :amount nil}]",
                "ambiguous",
                "",
                "Is a nil amount 0?"
            )])),
            &d,
        )
        .unwrap();
        let id = p.cases[0].id;
        d.tests.extend(p.cases);
        let spec = d.spec_fingerprint();
        let q = ask_on_specification(&mut d, id).unwrap();
        assert_eq!(ask_on_specification(&mut d, id).unwrap(), q, "asked once");
        let c = d.clarifications.iter().find(|c| c.id == q).unwrap();
        assert_eq!(c.question, "Is a nil amount 0?");
        assert!(c.context.contains(":amount nil"));
        // Asking is not deciding: only the user's answer changes the specification.
        assert_eq!(d.spec_fingerprint(), spec);
        d.clarification_mut(q).unwrap().answer("yes, count it as 0");
        assert_ne!(d.spec_fingerprint(), spec);
    }

    #[test]
    fn a_fix_starts_only_from_a_failing_check_of_the_current_content() {
        use clojurecraft_domain::{Check, ExecutionEnvironment, Timestamp, ValidationId};
        let mut d = doc();
        let id = add(&mut d);
        let mut edit = CaseEdit::of(d.test(id).unwrap());
        edit.input = "[]".into();
        edit.expected = Some("{}".into());
        save(&mut d, id, edit).unwrap();
        accept(&mut d, id).unwrap();
        let report = |d: &FunctionDocument, status| ValidationReport {
            id: ValidationId::generate(),
            created_at: Timestamp::now(),
            basis: d.basis(),
            environment: ExecutionEnvironment {
                clojure_version: "1.12.6".into(),
                java_version: "25".into(),
                platform: "linux".into(),
            },
            checks: vec![Check {
                subject: CheckSubject::Test(id),
                status,
                expected: Some(Edn::new("{}")),
                actual: Some(Edn::new("nil")),
                diagnostics: vec![],
                duration_ms: None,
            }],
            attempt: None,
            run: None,
            source: None,
            settings: None,
        };
        assert!(failing_report(&d, &[]).is_err());
        let passing = report(&d, CheckStatus::Passed);
        assert!(failing_report(&d, std::slice::from_ref(&passing)).is_err());
        let failing = report(&d, CheckStatus::Failed);
        let reports = [failing.clone(), passing];
        assert_eq!(failing_report(&d, &reports).map(|r| r.id), Ok(failing.id));
        assert_eq!(failing_cases(&failing), [id]);
        // After a manual edit the failures no longer describe the code.
        if let Some(i) = &mut d.implementation {
            i.source.push(' ');
        }
        assert!(failing_report(&d, &reports).is_err());
    }
}
