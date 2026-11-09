//! Compact JSON views for agents. They are kept short on purpose: the point of the server is to
//! spare the agent tokens, so values and diagnostics are bounded and only failures are listed.

use clojurecraft_domain::{
    Check, CheckStatus, CheckSubject, Contract, ExampleInput, Expectation, Freshness, FunctionDocument,
    KeySpec, NilBehavior, OrderingRequirement, Shape, ValidationReport,
};
#[cfg(test)]
use clojurecraft_workflow::assistant::AttemptKind;
use clojurecraft_workflow::assistant::{Attempt, Job, JobKind, Report};
use clojurecraft_workflow::edge_cases::{self, CaseState};
use serde_json::{Value, json};

use crate::edit::{origin_name, type_name};

/// Longest value (input, expected, actual) shown in a failure.
const VALUE_CHARS: usize = 600;
/// Longest diagnostic shown, and how many per failure.
const DIAGNOSTIC_CHARS: usize = 500;
const DIAGNOSTICS: usize = 2;
/// Failures listed in one report.
const FAILURES: usize = 8;

fn shorten(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}… ({} characters)", &text[..cut], text.chars().count()),
    }
}

// ---- function ------------------------------------------------------------------------------------

pub fn summary(id: impl ToString, name: &str, description: &str) -> Value {
    json!({"id": id.to_string(), "name": name, "description": shorten(description, 200)})
}

/// The specification and test cases, with the ids an agent needs to review them.
pub fn function(doc: &FunctionDocument) -> Value {
    let examples: Vec<Value> = doc
        .examples
        .iter()
        .map(|e| {
            let input = match &e.input {
                ExampleInput::Direct { value } => json!(value.as_str()),
                ExampleInput::FunctionOutput { snapshot, .. } => json!({
                    "from_function": snapshot.as_ref().map(|s| shorten(s.value.as_str(), VALUE_CHARS)),
                }),
            };
            json!({"name": e.name, "input": input, "expected": e.expected.as_str()})
        })
        .collect();
    let tests: Vec<Value> = doc
        .tests
        .iter()
        .map(|t| {
            let mut v = json!({
                "id": t.id.to_string(),
                "name": t.name,
                "input": t.input.as_str(),
                "state": case_state(edge_cases::state(t)),
                "origin": origin_name(t.origin),
            });
            match &t.expectation {
                Expectation::Value { expected } => v["expected"] = json!(expected.as_str()),
                Expectation::NeedsClarification { question } => v["question"] = json!(question),
            }
            if !t.notes.trim().is_empty() {
                v["notes"] = json!(t.notes);
            }
            v
        })
        .collect();
    let mut v = json!({
        "id": doc.id.to_string(),
        "name": doc.name,
        "description": doc.description,
        "steps": doc.steps.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
        "examples": examples,
        "contract": doc.contract.as_ref().map(contract),
        "tests": tests,
        "has_code": doc.implementation.is_some(),
    });
    let open: Vec<&str> = doc
        .clarifications
        .iter()
        .filter(|c| !c.is_answered() && c.status != clojurecraft_domain::ClarificationStatus::Dismissed)
        .map(|c| c.question.as_str())
        .collect();
    if !open.is_empty() {
        v["open_questions_in_app"] = json!(open);
    }
    v
}

fn case_state(s: CaseState) -> &'static str {
    match s {
        CaseState::Regression => "accepted",
        CaseState::AwaitingAcceptance => "proposed",
        CaseState::NeedsClarification => "needs_expected_value",
        CaseState::Rejected => "rejected",
    }
}

fn contract(c: &Contract) -> Value {
    json!({
        "status": match c.status {
            clojurecraft_domain::ContractStatus::Accepted => "accepted",
            clojurecraft_domain::ContractStatus::Proposed => "proposed",
        },
        "input": shape(&c.input),
        "output": shape(&c.output),
        "ordering": match &c.ordering {
            OrderingRequirement::Unspecified => "unspecified".to_owned(),
            OrderingRequirement::PreserveInput => "preserve_input".to_owned(),
            OrderingRequirement::Unordered => "unordered".to_owned(),
            OrderingRequirement::Sorted(how) => format!("sorted: {how}"),
        },
        "allowed_namespaces": c.allowed_dependencies,
        "notes": c.notes,
    })
}

fn shape(s: &Shape) -> Value {
    json!({
        "description": s.description,
        "type": type_name(&s.value_type),
        "keys": s.keys.iter().map(key).collect::<Vec<_>>(),
    })
}

fn key(k: &KeySpec) -> Value {
    let mut v = json!({
        "key": k.key.as_str(),
        "required": k.required,
        "type": type_name(&k.value_type),
    });
    let nil = match &k.nil {
        NilBehavior::Unspecified => None,
        NilBehavior::Disallowed => Some("disallowed"),
        NilBehavior::TreatAsMissing => Some("treat_as_missing"),
        NilBehavior::PassThrough => Some("pass_through"),
        NilBehavior::Default(d) => {
            v["default"] = json!(d.as_str());
            Some("default")
        }
    };
    if let Some(nil) = nil {
        v["nil"] = json!(nil);
    }
    if !k.notes.is_empty() {
        v["notes"] = json!(k.notes);
    }
    v
}

// ---- checks ------------------------------------------------------------------------------------

fn subject(doc: &FunctionDocument, s: &CheckSubject) -> (String, Option<String>) {
    match s {
        CheckSubject::Parse => ("parse".into(), None),
        CheckSubject::Structure => ("structure".into(), None),
        CheckSubject::Lint => ("lint".into(), None),
        CheckSubject::Load => ("load".into(), None),
        CheckSubject::Contract => ("output contract".into(), None),
        CheckSubject::Example(id) => match doc.examples.iter().position(|e| e.id == *id) {
            Some(i) => {
                let e = &doc.examples[i];
                let input = match &e.input {
                    ExampleInput::Direct { value } => Some(value.as_str().to_owned()),
                    ExampleInput::FunctionOutput { snapshot, .. } => {
                        snapshot.as_ref().map(|s| s.value.as_str().to_owned())
                    }
                };
                (format!("example {} “{}”", i + 1, e.name), input)
            }
            None => ("an example that no longer exists".into(), None),
        },
        CheckSubject::Test(id) => match doc.test(*id) {
            Some(t) => (
                format!("test case “{}” ({})", t.name, t.id),
                Some(t.input.as_str().to_owned()),
            ),
            None => ("a test case that no longer exists".into(), None),
        },
    }
}

fn status_name(s: CheckStatus) -> &'static str {
    match s {
        CheckStatus::Passed => "passed",
        CheckStatus::Failed => "failed",
        CheckStatus::Error => "error",
        CheckStatus::Timeout => "timeout",
        CheckStatus::ResourceLimit => "resource_limit",
        CheckStatus::Forbidden => "forbidden",
        CheckStatus::Skipped => "not_run",
    }
}

/// How much a reply shows: compact by default, everything with `verbose`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Detail {
    #[default]
    Compact,
    Verbose,
}

/// Compact limits: values, diagnostics, and a value longer than this is replaced by the
/// harness's description of the first difference.
const COMPACT_INPUT_CHARS: usize = 200;
const COMPACT_VALUE_CHARS: usize = 160;
const COMPACT_DIAGNOSTIC_CHARS: usize = 300;

/// The harness's description of where a result differs (`at [0 :total]: expected …`).
fn difference(c: &Check) -> Option<&str> {
    let d = c.diagnostics.first()?;
    (c.status == CheckStatus::Failed && (d.starts_with("at ") || d.starts_with("the output")))
        .then_some(d.as_str())
}

fn failure(doc: &FunctionDocument, c: &Check, detail: Detail) -> Value {
    let (label, input) = subject(doc, &c.subject);
    let mut v = json!({"check": label, "status": status_name(c.status)});
    let (input_max, value_max, diagnostic_max, diagnostics) = match detail {
        Detail::Verbose => (VALUE_CHARS, VALUE_CHARS, DIAGNOSTIC_CHARS, DIAGNOSTICS),
        Detail::Compact => (
            COMPACT_INPUT_CHARS,
            COMPACT_VALUE_CHARS,
            COMPACT_DIAGNOSTIC_CHARS,
            1,
        ),
    };
    if let Some(i) = input {
        v["input"] = json!(shorten(&i, input_max));
    }
    let long = |e: Option<&clojurecraft_domain::Edn>| {
        e.is_some_and(|e| e.as_str().trim().chars().count() > value_max)
    };
    if detail == Detail::Compact
        && let Some(d) = difference(c)
        && (long(c.expected.as_ref()) || long(c.actual.as_ref()))
    {
        // The values are long; where they differ says more in fewer tokens.
        v["difference"] = json!(shorten(d, diagnostic_max));
        return v;
    }
    if let Some(e) = &c.expected {
        v["expected"] = json!(shorten(e.as_str(), value_max));
    }
    if let Some(a) = &c.actual {
        v["actual"] = json!(shorten(a.as_str(), value_max));
    }
    let diagnostics: Vec<String> = c
        .diagnostics
        .iter()
        .take(diagnostics)
        .map(|d| shorten(d, diagnostic_max))
        .collect();
    if !diagnostics.is_empty() {
        v["diagnostics"] = json!(diagnostics);
    }
    v
}

/// What kind of check failed, for listing one failure per kind in compact replies.
fn failure_kind(c: &Check) -> (u8, CheckStatus) {
    let kind = match c.subject {
        CheckSubject::Parse => 0,
        CheckSubject::Structure => 1,
        CheckSubject::Lint => 2,
        CheckSubject::Load => 3,
        CheckSubject::Contract => 4,
        CheckSubject::Example(_) | CheckSubject::Test(_) => 5,
    };
    (kind, c.status)
}

/// A validation report: counts, and the failing checks with their values. With `freshness`, also
/// whether it is about the current content (not for candidates that were never applied).
pub fn report(doc: &FunctionDocument, r: &ValidationReport, freshness: bool, detail: Detail) -> Value {
    let ran: Vec<&Check> = r.checks.iter().collect();
    let passed = ran.iter().filter(|c| c.status == CheckStatus::Passed).count();
    let failing: Vec<&&Check> = ran
        .iter()
        .filter(|c| !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped))
        .collect();
    let not_run = ran.iter().filter(|c| c.status == CheckStatus::Skipped).count();
    let mut v = json!({
        "passed": r.passed(),
        "checks_passed": passed,
        "checks_total": ran.len(),
    });
    match r.freshness(&doc.basis()) {
        _ if !freshness => {}
        Freshness::Current => v["current"] = json!(true),
        Freshness::Stale(changed) => {
            v["current"] = json!(false);
            v["changed_since"] = json!(
                changed
                    .iter()
                    .map(|c| format!("{c:?}").to_lowercase())
                    .collect::<Vec<_>>()
            );
        }
    }
    if not_run > 0 {
        v["not_run"] = json!(not_run);
    }
    if !failing.is_empty() {
        // Compact: the first failure of each kind (a failing parse, a wrong value, a thrown
        // error, …); the rest are counted.
        let shown: Vec<&&Check> = match detail {
            Detail::Verbose => failing.iter().take(FAILURES).copied().collect(),
            Detail::Compact => {
                let mut kinds = Vec::new();
                failing
                    .iter()
                    .filter(|c| {
                        let k = failure_kind(c);
                        let new = !kinds.contains(&k);
                        kinds.push(k);
                        new
                    })
                    .copied()
                    .collect()
            }
        };
        // The same error often fails every call; its text is shown once.
        let mut seen: Vec<&[String]> = Vec::new();
        let listed: Vec<Value> = shown
            .iter()
            .map(|c| {
                let mut f = failure(doc, c, detail);
                if !c.diagnostics.is_empty() {
                    if seen.contains(&c.diagnostics.as_slice()) {
                        if let Some(o) = f.as_object_mut() {
                            o.remove("diagnostics");
                            o.insert("diagnostics_as_above".into(), json!(true));
                        }
                    } else {
                        seen.push(&c.diagnostics);
                    }
                }
                f
            })
            .collect();
        v["failures"] = json!(listed);
        if failing.len() > listed.len() {
            v["more_failures"] = json!(failing.len() - listed.len());
        }
    }
    v
}

/// The newest report about exactly the current content, else the newest about the current code
/// (stale because the specification or tests changed since). Reports of generated candidates
/// that were never applied are about other code and are left out. `reports` are newest first.
pub fn latest_report<'a>(
    doc: &FunctionDocument,
    reports: &'a [ValidationReport],
) -> Option<&'a ValidationReport> {
    doc.implementation.as_ref()?;
    let basis = doc.basis();
    reports
        .iter()
        .find(|r| r.basis == basis)
        .or_else(|| reports.iter().find(|r| r.basis.code == basis.code))
}

// ---- progress ----------------------------------------------------------------------------------

/// What the application knows about work on one function.
pub struct Progress<'a> {
    /// The model job in progress, when it is about this function.
    pub job: Option<&'a Job>,
    /// Why no model work can start now (setup incomplete, busy with another function).
    pub blocked: Option<String>,
    /// How the last model job about this function ended.
    pub last: Option<&'a Report>,
    /// A check of the current code is running.
    pub checks_running: bool,
    /// Why code cannot be run (no Java runtime), if so.
    pub runtime_unavailable: Option<String>,
    /// Position (from 1) of this function's generation in the queue, while it waits there.
    pub queued: Option<usize>,
    /// Why the queued generation could not start, if so.
    pub queue_error: Option<String>,
    pub detail: Detail,
}

fn task(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Interpret => "interpret",
        JobKind::ProposeContract => "propose_contract",
        JobKind::Generate => "generate",
        JobKind::ProposeTests => "generate_test_cases",
        JobKind::Explain => "explain",
        JobKind::Describe => "describe",
    }
}

fn attempt(doc: &FunctionDocument, a: &Attempt, detail: Detail) -> Value {
    let mut v = json!({"attempt": a.label(), "passed": a.passed()});
    if let Some(r) = &a.report {
        let failing: Vec<&Check> = r
            .checks
            .iter()
            .filter(|c| !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped))
            .collect();
        v["checks_passed"] = json!(
            r.checks
                .iter()
                .filter(|c| c.status == CheckStatus::Passed)
                .count()
        );
        v["checks_total"] = json!(r.checks.len());
        if let Some(first) = failing.first() {
            v["first_failure"] = failure(doc, first, detail);
        }
    }
    if let Some(n) = &a.note {
        v["note"] = json!(shorten(n, DIAGNOSTIC_CHARS));
    }
    v
}

/// Attempts in order; a failure identical to the previous attempt's is named, not repeated.
fn attempt_list(doc: &FunctionDocument, attempts: &[Attempt], detail: Detail) -> Vec<Value> {
    let mut previous: Option<(Value, Value)> = None;
    attempts
        .iter()
        .map(|a| {
            let mut v = attempt(doc, a, detail);
            let failure = v.as_object_mut().and_then(|o| o.remove("first_failure"));
            match (failure, &previous) {
                (Some(f), Some((seen, first))) if f == *seen => v["same_failure_as"] = first.clone(),
                (Some(f), _) => {
                    previous = Some((f.clone(), v["attempt"].clone()));
                    v["first_failure"] = f;
                }
                (None, _) => previous = None,
            }
            v
        })
        .collect()
}

/// How the last job ended.
pub fn last_job(doc: &FunctionDocument, r: &Report, detail: Detail) -> Value {
    let notes = |n: &[String]| n.iter().map(|s| shorten(s, DIAGNOSTIC_CHARS)).collect::<Vec<_>>();
    match r {
        Report::Interpreted { questions, notes: n } => {
            json!({"outcome": "interpreted", "questions": questions, "notes": notes(n)})
        }
        Report::ContractProposed { notes: n } => json!({"outcome": "contract_proposed", "notes": notes(n)}),
        Report::Explained { notes: n } => json!({"outcome": "explained", "notes": notes(n)}),
        Report::Described { filled, notes: n } => {
            json!({"outcome": "described", "filled": filled, "notes": notes(n)})
        }
        Report::TestsProposed {
            added,
            ambiguous,
            notes: n,
        } => json!({
            "outcome": "test_cases_proposed",
            "added": added,
            "need_expected_value": ambiguous,
            "notes": notes(n),
        }),
        Report::Generated { notes: n, attempts } => json!({
            "outcome": "code_generated",
            "attempts": attempts.len(),
            "notes": notes(n),
        }),
        Report::Pending(p) => {
            let mut why = Vec::new();
            if p.fix {
                why.push("it is a fix, which is always reviewed");
            }
            if !p.needs_approval.is_empty() {
                why.push("it requires namespaces the function does not allow yet");
            }
            if p.replaces_manual {
                why.push("it would replace hand-written code");
            }
            if p.replaces_accepted {
                why.push("it would replace the accepted code");
            }
            if p.spec_changed {
                why.push("the specification or tests changed while it was generated");
            }
            json!({
                "outcome": "code_waiting_for_acceptance",
                "passed": p.passed(),
                "why_waiting": why,
                "needs_namespaces": p.needs_approval,
                "differs_from_current_code": p.differences,
                "attempts": p.attempts.len(),
                "notes": notes(&p.notes),
            })
        }
        Report::Unresolved {
            attempts,
            best,
            notes: n,
        } => json!({
            "outcome": "all_attempts_failed",
            "attempts": attempt_list(doc, attempts, detail),
            "best_candidate_available": best.is_some(),
            "notes": notes(n),
        }),
        Report::Failed {
            what,
            message,
            remedies,
            ..
        } => json!({
            "outcome": "failed",
            "while": what,
            "message": shorten(message, 1000),
            "remedies": remedies.iter().map(|r| r.describe()).collect::<Vec<_>>(),
        }),
        Report::Blocked(reasons) => json!({"outcome": "blocked", "reasons": reasons}),
        Report::Cancelled => json!({"outcome": "cancelled"}),
    }
}

/// Whether code is waiting for `accept_code`.
pub fn pending(last: Option<&Report>) -> Option<&clojurecraft_workflow::assistant::Pending> {
    match last? {
        Report::Pending(p) => Some(p),
        Report::Unresolved { best: Some(p), .. } => Some(p),
        _ => None,
    }
}

/// Test cases waiting for review, as `review_test_cases` needs them.
fn to_review(doc: &FunctionDocument) -> Vec<Value> {
    doc.tests
        .iter()
        .filter(|t| {
            matches!(
                edge_cases::state(t),
                CaseState::AwaitingAcceptance | CaseState::NeedsClarification
            )
        })
        .map(|t| {
            let mut v =
                json!({"id": t.id.to_string(), "input": shorten(t.input.as_str(), COMPACT_INPUT_CHARS)});
            match &t.expectation {
                Expectation::Value { expected } => {
                    v["expected"] = json!(shorten(expected.as_str(), COMPACT_VALUE_CHARS));
                }
                Expectation::NeedsClarification { question } => v["question"] = json!(question),
            }
            v
        })
        .collect()
}

/// Progress, the latest checks, and a hint for the next step. Only what is not empty or false
/// is listed. Once the current code passes every check, `result` holds it, and test cases
/// waiting for review are listed with their ids, so neither needs another call.
pub fn status(doc: &FunctionDocument, reports: &[ValidationReport], p: &Progress<'_>) -> Value {
    let working = p.job.is_some() || p.checks_running || p.queued.is_some();
    let mut v = json!({"working": working});
    if let Some(n) = p.queued {
        v["queued"] = json!(n);
    }
    if let Some(e) = &p.queue_error {
        v["queue_error"] = json!(e);
    }
    if let Some(job) = p.job {
        let mut j = json!({
            "task": task(job.kind),
            "phase": job.phase.label(),
            "seconds": job.started.elapsed().as_secs(),
        });
        if let Some(progress) = job.progress() {
            j["attempt"] = json!(progress);
        }
        v["job"] = j;
    }
    if p.checks_running {
        v["running_tests"] = json!(true);
    }
    if let Some(last) = p.last {
        v["last_job"] = last_job(doc, last, p.detail);
    }
    let latest = latest_report(doc, reports);
    if let Some(r) = latest {
        v["checks"] = report(doc, r, true, p.detail);
    }
    let (accepted, proposed, ambiguous) = edge_cases::counts(doc);
    if accepted > 0 {
        v["accepted_tests"] = json!(accepted);
    }
    let code = doc.implementation.as_ref();
    if code.is_none() {
        v["has_code"] = json!(false);
    } else if code.is_some_and(|c| c.generation.is_some() && c.is_manually_edited()) {
        v["code_edited_by_hand"] = json!(true);
    }
    if let Some(b) = &p.blocked {
        v["model_unavailable"] = json!(b);
    }
    if let Some(u) = &p.runtime_unavailable {
        v["runtime_unavailable"] = json!(u);
    }
    let current = latest.filter(|r| r.basis == doc.basis());
    if !working && proposed + ambiguous > 0 {
        v["tests_to_review"] = json!(to_review(doc));
    }
    let passing = !working && current.is_some_and(ValidationReport::passed);
    if passing && let Some(c) = code {
        let n = clojurecraft_workflow::export::names(doc);
        v["result"] = json!({"namespace": n.namespace, "function": n.function, "code": c.source});
    }
    let next = if working {
        "Call status with wait_seconds to wait until the work finishes."
    } else if pending(p.last).is_some() {
        "Generated code is waiting: inspect it with result, then accept_code (or change the \
         specification and generate again)."
    } else if proposed + ambiguous > 0 {
        "Review tests_to_review with review_test_cases (accept, edit expected, or reject)."
    } else if code.is_none() {
        match &p.blocked {
            Some(_) => "Generation cannot start now; see model_unavailable.",
            None => "Call generate.",
        }
    } else if p.runtime_unavailable.is_some() {
        "The code cannot be run until the Java runtime is installed in the app."
    } else if current.is_none() {
        "The code has not been checked against the current specification and tests: call run_tests \
         or generate."
    } else if current.is_some_and(|r| !r.passed()) {
        "Checks fail: call generate with mode=fix, or correct the specification or test cases."
    } else {
        "Done: all checks pass; the code is in result."
    };
    v["next"] = json!(next);
    v
}

/// One short line about a function, for `status` over several: whether work is going on, how
/// the last job ended, whether the current code passes, and the first failure.
pub fn brief(doc: &FunctionDocument, reports: &[ValidationReport], p: &Progress<'_>) -> Value {
    let mut v = json!({
        "id": doc.id.to_string(),
        "name": doc.name,
    });
    if let Some(job) = p.job {
        v["working"] = json!(job.phase.label());
    } else if let Some(n) = p.queued {
        v["queued"] = json!(n);
    } else if p.checks_running {
        v["working"] = json!("running tests");
    }
    if let Some(e) = &p.queue_error {
        v["error"] = json!(shorten(e, DIAGNOSTIC_CHARS));
    }
    if let Some(last) = p.last {
        let job = last_job(doc, last, Detail::Compact);
        v["last_job"] = job["outcome"].clone();
        if let Some(m) = job.get("message").or_else(|| job.get("reasons")) {
            v["last_job_detail"] = m.clone();
        }
    }
    let basis = doc.basis();
    let current = reports.iter().find(|r| r.basis == basis);
    v["passed"] = match (&doc.implementation, current) {
        (None, _) => json!("no code"),
        (Some(_), None) => json!("not checked"),
        (Some(_), Some(r)) => json!(r.passed()),
    };
    if let Some(r) = current.filter(|r| !r.passed())
        && let Some(c) = r
            .checks
            .iter()
            .find(|c| !matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped))
    {
        v["first_failure"] = failure(doc, c, Detail::Compact);
    }
    if pending(p.last).is_some() {
        v["code_waiting_for_acceptance"] = json!(true);
    }
    v
}

// ---- result ------------------------------------------------------------------------------------

/// The current code and how it fares, plus code waiting for acceptance.
pub fn result(
    doc: &FunctionDocument,
    reports: &[ValidationReport],
    last: Option<&Report>,
    detail: Detail,
) -> Value {
    let n = clojurecraft_workflow::export::names(doc);
    let basis = doc.basis();
    let current = reports.iter().find(|r| r.basis == basis);
    let mut v = json!({
        "function_id": doc.id.to_string(),
        "namespace": n.namespace,
        "function": n.function,
        "code": doc.implementation.as_ref().map(|i| i.source.as_str()),
        "passes_all_checks": current.map(ValidationReport::passed),
    });
    if let Some(i) = &doc.implementation {
        if current.is_none() {
            v["checks"] = json!("not checked against the current specification and tests");
        }
        if let Some(g) = &i.generation {
            let assumptions = match &g.explanation {
                Some(e) if !e.assumptions.is_empty() => &e.assumptions,
                _ => &g.assumptions,
            };
            if !assumptions.is_empty() {
                v["assumptions"] = json!(assumptions);
            }
            if let Some(e) = &g.explanation
                && !e.summary.is_empty()
            {
                v["summary"] = json!(e.summary);
            }
        }
    }
    if let Some(p) = pending(last) {
        let mut w = json!({
            "namespace": p.names.namespace,
            "function": p.names.function,
            "code": p.candidate.code,
            "passed": p.passed(),
        });
        if !p.needs_approval.is_empty() {
            w["needs_namespaces"] = json!(p.needs_approval);
        }
        if let Some(r) = &p.validation
            && !r.passed()
        {
            w["checks"] = report(doc, r, false, detail);
        }
        v["waiting_for_acceptance"] = w;
    }
    v
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{
        Basis, Edn, Example, ExecutionEnvironment, Implementation, Timestamp, ValidationId,
    };

    use super::*;

    fn doc() -> FunctionDocument {
        let mut d = FunctionDocument::new("Sum numbers");
        d.examples = vec![Example::direct("pair", Edn::new("[1 2]"), Edn::new("3"))];
        d
    }

    fn report_for(d: &FunctionDocument, basis: Basis, pass: bool) -> ValidationReport {
        let e = d.examples[0].id;
        ValidationReport {
            id: ValidationId::generate(),
            created_at: Timestamp::now(),
            basis,
            environment: ExecutionEnvironment {
                clojure_version: "1.12".into(),
                java_version: "25".into(),
                platform: "linux".into(),
            },
            checks: vec![Check {
                subject: CheckSubject::Example(e),
                status: if pass {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                },
                expected: Some(Edn::new("3")),
                actual: Some(Edn::new(if pass { "3" } else { "\"12\"" })),
                diagnostics: vec!["x".repeat(2000)],
                duration_ms: Some(1),
            }],
            attempt: None,
            run: None,
            source: None,
            settings: None,
        }
    }

    fn idle<'a>() -> Progress<'a> {
        Progress {
            job: None,
            blocked: None,
            last: None,
            checks_running: false,
            runtime_unavailable: None,
            queued: None,
            queue_error: None,
            detail: Detail::Compact,
        }
    }

    #[test]
    fn status_points_to_the_next_step() {
        let mut d = doc();
        let s = status(&d, &[], &idle());
        assert_eq!(s["working"], false);
        assert_eq!(s["next"], "Call generate.");

        d.implementation = Some(Implementation::manual("(ns sum-numbers.core)"));
        let s = status(&d, &[], &idle());
        assert!(s["next"].as_str().unwrap().contains("run_tests"));

        let failing = report_for(&d, d.basis(), false);
        let s = status(&d, std::slice::from_ref(&failing), &idle());
        assert_eq!(s["checks"]["passed"], false);
        assert_eq!(s["checks"]["current"], true);
        let f = &s["checks"]["failures"][0];
        assert_eq!(f["input"], "[1 2]");
        assert_eq!(f["actual"], "\"12\"");
        assert!(
            f["diagnostics"][0].as_str().unwrap().len() < 700,
            "diagnostics are bounded"
        );
        assert!(s["next"].as_str().unwrap().contains("mode=fix"));

        let passing = report_for(&d, d.basis(), true);
        let s = status(&d, &[passing, failing], &idle());
        assert!(s["next"].as_str().unwrap().contains("result"));
        assert_eq!(s["result"]["code"], "(ns sum-numbers.core)");
        assert_eq!(s["result"]["function"], "sum-numbers");
        let r = result(&d, &[report_for(&d, d.basis(), true)], None, Detail::Compact);
        assert_eq!(r["passes_all_checks"], true);
        assert_eq!(r["namespace"], "sum-numbers.core");
        assert_eq!(r["function"], "sum-numbers");
    }

    #[test]
    fn repeated_attempt_failures_are_named_instead_of_repeated() {
        let d = doc();
        let failing = report_for(&d, d.basis(), false);
        let att = |number| Attempt {
            number,
            kind: AttemptKind::Fix(number),
            code: Some("(ns sum-numbers.core)".into()),
            report: Some(failing.clone()),
            note: None,
        };
        let empty = Attempt {
            number: 4,
            kind: AttemptKind::Fix(4),
            code: None,
            report: None,
            note: Some("cut off".into()),
        };
        let list = attempt_list(&d, &[att(1), att(2), att(3), empty, att(5)], Detail::Compact);
        assert!(list[0]["first_failure"].is_object());
        assert_eq!(list[1]["same_failure_as"], "Attempt 1");
        assert_eq!(list[2]["same_failure_as"], "Attempt 1");
        assert!(list[1].get("first_failure").is_none());
        assert!(list[3].get("same_failure_as").is_none());
        assert!(
            list[4]["first_failure"].is_object(),
            "a failure after a different attempt is shown again"
        );
    }

    #[test]
    fn queued_work_counts_as_working_and_briefs_are_short() {
        let mut d = doc();
        let mut p = idle();
        p.queued = Some(2);
        let s = status(&d, &[], &p);
        assert_eq!(s["working"], true);
        assert_eq!(s["queued"], 2);
        assert_eq!(brief(&d, &[], &p)["queued"], 2);
        assert_eq!(brief(&d, &[], &idle())["passed"], "no code");

        d.implementation = Some(Implementation::manual("(ns sum-numbers.core)"));
        let failing = report_for(&d, d.basis(), false);
        let b = brief(&d, &[failing], &idle());
        assert_eq!(b["passed"], false);
        assert_eq!(b["first_failure"]["actual"], "\"12\"");
        assert!(b.get("working").is_none());
    }

    #[test]
    fn compact_reports_show_one_failure_per_kind_and_differences_instead_of_long_values() {
        let d = doc();
        let mut r = report_for(&d, d.basis(), false);
        let long = format!("[{}]", "1 ".repeat(200));
        r.checks[0].expected = Some(Edn::new(&long));
        r.checks[0].actual = Some(Edn::new(&long));
        r.checks[0].diagnostics = vec!["at [3]: expected 1 but got 2".into()];
        r.checks.push(r.checks[0].clone());
        r.checks[1].diagnostics = vec!["at [4]: expected 1 but got 3".into()];

        let v = report(&d, &r, false, Detail::Compact);
        assert_eq!(v["failures"].as_array().unwrap().len(), 1);
        assert_eq!(v["more_failures"], 1);
        let f = &v["failures"][0];
        assert_eq!(f["difference"], "at [3]: expected 1 but got 2");
        assert!(f.get("expected").is_none() && f.get("actual").is_none());

        let v = report(&d, &r, false, Detail::Verbose);
        assert_eq!(v["failures"].as_array().unwrap().len(), 2);
        assert!(v["failures"][0]["expected"].as_str().unwrap().len() > 300);
    }

    #[test]
    fn reports_about_older_content_are_marked_stale() {
        let mut d = doc();
        d.implementation = Some(Implementation::manual("(ns a)"));
        let old = report_for(&d, d.basis(), true);
        d.description = "changed".into();
        let s = status(&d, std::slice::from_ref(&old), &idle());
        assert_eq!(s["checks"]["current"], false);
        assert_eq!(s["checks"]["changed_since"][0], "specification");
        let r = result(&d, &[old], None, Detail::Compact);
        assert_eq!(r["passes_all_checks"], Value::Null);
    }

    #[test]
    fn reports_of_other_code_are_not_the_current_codes_checks() {
        let mut d = doc();
        let candidate = report_for(&d, d.basis_with_code("(ns other)"), false);
        assert!(
            status(&d, std::slice::from_ref(&candidate), &idle())
                .get("checks")
                .is_none()
        );
        d.implementation = Some(Implementation::manual("(ns mine)"));
        assert!(status(&d, &[candidate], &idle()).get("checks").is_none());
    }

    #[test]
    fn a_repeated_error_is_shown_once() {
        let d = doc();
        let mut r = report_for(&d, d.basis(), false);
        r.checks.push(r.checks[0].clone());
        let v = report(&d, &r, false, Detail::Verbose);
        assert!(v.get("current").is_none());
        assert!(v["failures"][0].get("diagnostics").is_some());
        assert_eq!(v["failures"][1]["diagnostics_as_above"], true);
    }

    #[test]
    fn status_omits_empty_parts_and_lists_tests_to_review() {
        let mut d = doc();
        let s = status(&d, &[], &idle());
        assert_eq!(s["has_code"], false);
        assert!(s.get("accepted_tests").is_none() && s.get("tests_to_review").is_none());
        assert!(s.get("result").is_none(), "no code, no result");

        let id = edge_cases::add(&mut d);
        let s = status(&d, &[], &idle());
        assert_eq!(s["tests_to_review"][0]["id"], id.to_string());
        assert!(s["next"].as_str().unwrap().contains("review_test_cases"));

        let mut p = idle();
        p.queued = Some(1);
        assert!(
            status(&d, &[], &p).get("tests_to_review").is_none(),
            "nothing to review while work goes on"
        );
    }

    #[test]
    fn failing_code_is_not_offered_as_the_result() {
        let mut d = doc();
        d.implementation = Some(Implementation::manual("(ns sum-numbers.core)"));
        let failing = report_for(&d, d.basis(), false);
        assert!(status(&d, &[failing], &idle()).get("result").is_none());
        let old = report_for(&d, d.basis(), true);
        d.description = "changed".into();
        assert!(
            status(&d, &[old], &idle()).get("result").is_none(),
            "a pass of an older specification is not a pass"
        );
    }

    #[test]
    fn functions_list_test_cases_with_ids() {
        let mut d = doc();
        let id = edge_cases::add(&mut d);
        let v = function(&d);
        assert_eq!(v["tests"][0]["id"], id.to_string());
        assert_eq!(v["tests"][0]["state"], "proposed");
        assert_eq!(v["tests"][0]["origin"], "user");
        assert_eq!(v["examples"][0]["input"], "[1 2]");
    }
}
