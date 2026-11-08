//! Automatic validation of an implementation against its specification.
//!
//! The sequence is: (1) parse the source, (2) check its structure and dependencies, (3) lint it,
//! (4) load it in the sandboxed runtime, (5) run every usable example and accepted test and check
//! the accepted output contract, (6) compare results with the expected EDN values structurally,
//! and (7) record diagnostics, timing, and bounded output. Steps 1 and 2 run here before
//! anything executes; the rest run in the evaluation harness (see [`clojurecraft_clojure`]).
//!
//! Comparison rules are documented on [`Equality`]; a sequential output is compared without
//! regard to order only when the accepted contract says that order does not matter.
//!
//! Timeouts, crashes, forbidden access, results that are too large, and resource-limit
//! violations are failures. A report passes only when every check passed.

use std::time::Duration;

use clojurecraft_clojure::{
    Call, CallResult, CallStatus, Equality, EvalOutcome, EvalRequest, EvalStatus, HarnessError, KeyRule,
    LintLevel, OutputRules, TypeRule,
};
use clojurecraft_domain::{
    Basis, Check, CheckStatus, CheckSubject, ContractStatus, Edn, ExecutionEnvironment, Expectation,
    FunctionDocument, GenerationRunId, NilBehavior, OrderingRequirement, Timestamp, ValidationId,
    ValidationReport, ValidationSettings, ValueType,
};

use crate::deps;
use crate::insert;
use crate::intermediate::{self, StepLines, StepProbes};
use crate::locals::{self, Locals};
use crate::requests::Names;
use crate::spec;
use crate::structure::{self, CodeStructure, Expectations, ProblemKind};

/// Failing calls whose recorded values a failure summary includes.
const MAX_CALLS_WITH_VALUES: usize = 2;
/// How diagnostics with recorded values begin (see [`crate::locals`] and
/// [`crate::intermediate::describe_steps`]).
const VALUES_PREFIX: &str = "Values ";

/// Longest diagnostic line kept in a report.
const MAX_DIAGNOSTIC_CHARS: usize = 2000;
/// Longest expected or actual value kept in a check.
const MAX_VALUE_CHARS: usize = 20_000;
/// Time limit of each call, including realizing its result.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Elements a realized result may have.
pub const MAX_ELEMENTS: u64 = 1_000_000;

/// One call of the function, and the check it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedCall {
    pub subject: CheckSubject,
    /// The name the specification uses for it, for example "Example 2" or "Test empty input".
    pub label: String,
    pub input: String,
    pub expected: String,
}

/// What to check for one piece of source code.
#[derive(Clone, Debug)]
pub struct Plan {
    pub source: String,
    pub structure: CodeStructure,
    pub calls: Vec<PlannedCall>,
    pub output_rules: Option<OutputRules>,
    pub equality: Equality,
    /// The request for the harness; `None` when the source cannot be run (parse or structure
    /// problems).
    pub request: Option<EvalRequest>,
    /// The scopes whose locals the instrumented copy records (see [`crate::locals`]).
    pub locals: Option<Locals>,
    /// The steps whose values the instrumented copy records (see [`crate::intermediate`]).
    pub steps: Option<StepProbes>,
}

impl Plan {
    pub fn settings(&self) -> ValidationSettings {
        ValidationSettings {
            float_tolerance: self.equality.float_tolerance,
            unordered_output: self.equality.unordered,
            call_timeout_ms: u64::try_from(CALL_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            max_elements: MAX_ELEMENTS,
        }
    }

    /// The label of a check's subject, as the specification names it.
    pub fn label(&self, subject: &CheckSubject) -> String {
        match subject {
            CheckSubject::Parse => "Reading the code".into(),
            CheckSubject::Structure => "Structure and dependencies".into(),
            CheckSubject::Lint => "Lint".into(),
            CheckSubject::Load => "Loading".into(),
            CheckSubject::Contract => "Output contract".into(),
            CheckSubject::Example(_) | CheckSubject::Test(_) => self
                .calls
                .iter()
                .find(|c| &c.subject == subject)
                .map_or_else(|| "A removed example".into(), |c| c.label.clone()),
        }
    }
}

fn type_rule(t: &ValueType) -> Option<TypeRule> {
    Some(match t {
        ValueType::Any | ValueType::Other(_) => return None,
        ValueType::Nil => TypeRule::Nil,
        ValueType::Boolean => TypeRule::Boolean,
        ValueType::Integer => TypeRule::Integer,
        ValueType::Number => TypeRule::Number,
        ValueType::String => TypeRule::String,
        ValueType::Keyword => TypeRule::Keyword,
        ValueType::Symbol => TypeRule::Symbol,
        ValueType::Map => TypeRule::Map,
        ValueType::Vector => TypeRule::Vector,
        ValueType::List => TypeRule::List,
        ValueType::Set => TypeRule::Set,
        ValueType::Sequential => TypeRule::Sequential,
    })
}

/// Every usable example and every accepted test with a defined expectation, in that order.
pub fn planned_calls(doc: &FunctionDocument) -> Vec<PlannedCall> {
    let mut calls: Vec<PlannedCall> = spec::usable_examples(doc)
        .into_iter()
        .map(|(label, e, input)| PlannedCall {
            subject: CheckSubject::Example(e.id),
            label,
            input: input.to_owned(),
            expected: e.expected.as_str().to_owned(),
        })
        .collect();
    for (n, t) in doc.tests.iter().filter(|t| t.is_active()).enumerate() {
        if let Expectation::Value { expected } = &t.expectation {
            calls.push(PlannedCall {
                subject: CheckSubject::Test(t.id),
                label: spec::test_label(n, t),
                input: t.input.as_str().to_owned(),
                expected: expected.as_str().to_owned(),
            });
        }
    }
    calls
}

/// The checkable rules of the accepted output contract, if any.
pub fn output_rules(doc: &FunctionDocument) -> Option<OutputRules> {
    let c = doc
        .contract
        .as_ref()
        .filter(|c| c.status == ContractStatus::Accepted)?;
    let rules = OutputRules {
        value_type: type_rule(&c.output.value_type),
        keys: c
            .output
            .keys
            .iter()
            .filter(|k| !k.key.as_str().trim().is_empty())
            .map(|k| KeyRule {
                key: k.key.as_str().trim().to_owned(),
                required: k.required,
                value_type: type_rule(&k.value_type),
                nil_allowed: k.nil != NilBehavior::Disallowed,
            })
            .collect(),
    };
    (!rules.is_empty()).then_some(rules)
}

/// The comparison rules for this function.
pub fn equality(doc: &FunctionDocument) -> Equality {
    Equality {
        unordered: doc.contract.as_ref().is_some_and(|c| {
            c.status == ContractStatus::Accepted && c.ordering == OrderingRequirement::Unordered
        }),
        ..Equality::default()
    }
}

/// Plans checking `source` against `doc`'s specification. `names` are the namespace and
/// function the code is expected to define; a different name is only a warning. When `source`
/// is the explained implementation, failing calls show the values of its steps.
pub fn plan(doc: &FunctionDocument, source: &str, names: &Names) -> Plan {
    plan_with_steps(
        doc,
        source,
        names,
        intermediate::explained_step_lines(doc, source),
    )
}

/// [`plan`] with the lines of the code that carries out each step, for example as the model
/// stated them with generated code.
///
/// Every failing call is run again with an instrumented copy of the code that records the value
/// of each step's code and the locals of every function and binding form, so its diagnostics
/// show where the data flow went wrong.
pub fn plan_with_steps(
    doc: &FunctionDocument,
    source: &str,
    names: &Names,
    step_lines: Option<StepLines>,
) -> Plan {
    let allowed = deps::allowed(doc);
    let structure = structure::check(
        source,
        &Expectations {
            namespace: &names.namespace,
            function: &names.function,
            allowed: &allowed,
        },
    );
    let calls = planned_calls(doc);
    let output_rules = output_rules(doc);
    let equality = equality(doc);
    let mut locals = None;
    let mut steps = None;
    let request = match (&structure.function, structure.problems.is_empty()) {
        (Some(function), true) => {
            let mut r = EvalRequest::new(
                source,
                function.clone(),
                calls
                    .iter()
                    .map(|c| Call {
                        input: c.input.clone(),
                        expected: Some(c.expected.clone()),
                    })
                    .collect(),
            );
            r.output_rules.clone_from(&output_rules);
            r.equality = equality;
            r.call_timeout = CALL_TIMEOUT;
            r.max_elements = MAX_ELEMENTS;
            let mut insertions = Vec::new();
            if let Some(lines) = step_lines.filter(|l| l.iter().any(|(_, r)| r.is_ok()))
                && let Ok((targets, plans, ins)) = intermediate::step_probes(source, &lines)
                && !ins.is_empty()
            {
                let texts = crate::requests::numbered_steps(doc)
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.id, i + 1, s.text.trim().to_owned()))
                    .collect();
                steps = Some(StepProbes {
                    targets,
                    plans,
                    texts,
                });
                insertions.extend(ins);
            }
            let first_id = steps.as_ref().map_or(0, |s| s.targets.len());
            if let Some((l, ins)) = locals::instrument(source, first_id) {
                locals = Some(l);
                insertions.extend(ins);
            }
            if !insertions.is_empty() {
                r.probe_code = Some(insert::apply(source, &insertions));
                r.probe_failing_only = true;
            }
            Some(r)
        }
        _ => None,
    };
    Plan {
        source: source.to_owned(),
        structure,
        calls,
        output_rules,
        equality,
        request,
        locals,
        steps,
    }
}

fn bounded(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(" … (shortened)");
    out
}

fn check(subject: CheckSubject, status: CheckStatus, diagnostics: Vec<String>) -> Check {
    Check {
        subject,
        status,
        expected: None,
        actual: None,
        diagnostics: diagnostics
            .into_iter()
            .map(|d| bounded(&d, MAX_DIAGNOSTIC_CHARS))
            .collect(),
        duration_ms: None,
    }
}

fn located(message: &str, line: Option<u32>) -> String {
    match line {
        Some(l) => format!("line {l}: {message}"),
        None => message.to_owned(),
    }
}

fn error_text(e: &HarnessError) -> String {
    let class = e.class.rsplit('.').next().unwrap_or_default();
    let mut message = if class.is_empty() || e.message.contains(class) {
        e.message.clone()
    } else {
        format!("{class}: {}", e.message)
    };
    if let Some(within) = &e.within {
        message.push_str(&format!(" (inside {within})"));
    }
    located(&message, e.line)
}

/// Whether an exception shows the sandbox refusing an operation.
fn is_denial(e: &HarnessError) -> bool {
    let m = e.message.to_lowercase();
    e.class.contains("SecurityException")
        || e.class.contains("AccessDenied")
        || m.contains("permission denied")
        || m.contains("operation not permitted")
        || m.contains("function not implemented")
}

/// Why the process stopped, for the check that was running.
fn process_failure(status: &EvalStatus, stderr: &str) -> (CheckStatus, String) {
    let tail = |s: &str| {
        let s = s.trim();
        let start = s.char_indices().rev().nth(1500).map_or(0, |(i, _)| i);
        s[start..].to_owned()
    };
    match status {
        EvalStatus::TimedOut => (
            CheckStatus::Timeout,
            "The runtime exceeded its overall time limit and was stopped.".into(),
        ),
        EvalStatus::MemoryLimitExceeded => (
            CheckStatus::ResourceLimit,
            "The runtime ran out of memory and was stopped.".into(),
        ),
        EvalStatus::OutputLimitExceeded => (
            CheckStatus::ResourceLimit,
            "The code wrote more output than allowed and was stopped.".into(),
        ),
        EvalStatus::Crashed(how) => (
            CheckStatus::Error,
            format!("The runtime ended unexpectedly ({how}). {}", tail(stderr)),
        ),
        EvalStatus::SandboxUnavailable(why) => (
            CheckStatus::Error,
            format!(
                "The sandbox could not be set up, so nothing was run: {}",
                tail(why)
            ),
        ),
        EvalStatus::Cancelled => (CheckStatus::Skipped, "The check was cancelled.".into()),
        EvalStatus::Completed => (
            CheckStatus::Error,
            "The runtime stopped without reporting this step.".into(),
        ),
    }
}

/// The trimmed text of 1-based `line` in `source`, when it is short enough to quote.
fn source_line(source: &str, line: Option<u32>) -> Option<String> {
    let text = source
        .lines()
        .nth(usize::try_from(line?).ok()?.checked_sub(1)?)?
        .trim();
    (!text.is_empty() && text.chars().count() <= 200).then(|| text.to_owned())
}

fn call_check(call: &PlannedCall, r: &CallResult, values: Vec<String>, source: &str) -> Check {
    let mut c = check(call.subject, CheckStatus::Passed, Vec::new());
    c.expected = Some(Edn::new(bounded(&call.expected, MAX_VALUE_CHARS)));
    c.actual = r.actual.as_deref().map(Edn::new);
    c.duration_ms = Some(r.elapsed_ns / 1_000_000);
    match r.status {
        CallStatus::Ok => match (r.matches, &r.difference) {
            (Some(true), _) => {}
            (_, Some(d)) => {
                c.status = CheckStatus::Failed;
                c.diagnostics.push(bounded(d, MAX_DIAGNOSTIC_CHARS));
            }
            _ => {
                c.status = CheckStatus::Failed;
                c.diagnostics
                    .push("The result differs from the expected output.".into());
            }
        },
        CallStatus::Error => {
            let e = r.error.clone().unwrap_or_default();
            c.status = if is_denial(&e) {
                CheckStatus::Forbidden
            } else {
                CheckStatus::Error
            };
            let quoted = source_line(source, e.line)
                .map_or_else(String::new, |l| format!("\nThe line that threw: {l}"));
            c.diagnostics.push(bounded(
                &format!("The function threw {}{quoted}", error_text(&e)),
                MAX_DIAGNOSTIC_CHARS,
            ));
        }
        CallStatus::Timeout => {
            c.status = CheckStatus::Timeout;
            c.diagnostics.push(format!(
                "The call did not finish within {} s (including realizing a lazy result).",
                CALL_TIMEOUT.as_secs()
            ));
        }
        CallStatus::TooLarge => {
            c.status = CheckStatus::ResourceLimit;
            c.diagnostics.push(
                r.message
                    .clone()
                    .unwrap_or_else(|| "The result is too large.".into()),
            );
        }
    }
    if !r.out.is_empty() {
        c.diagnostics.push(bounded(
            &format!("Printed: {}", r.out.trim_end()),
            MAX_DIAGNOSTIC_CHARS,
        ));
    }
    if c.status != CheckStatus::Passed {
        c.diagnostics
            .extend(values.iter().map(|v| bounded(v, MAX_DIAGNOSTIC_CHARS)));
    }
    c
}

/// What the instrumented run of a failing call recorded: the values after each step, then the
/// locals around an exception or, for a wrong value, of every scope the call entered.
fn values(plan: &Plan, result: &CallResult, run: &clojurecraft_clojure::ProbeCall) -> Vec<String> {
    let steps = plan
        .steps
        .as_ref()
        .and_then(|s| intermediate::describe_steps(s, run));
    let locals = plan.locals.as_ref().and_then(|l| match &result.error {
        Some(e) if result.status == CallStatus::Error => locals::describe(l, e, run),
        _ if result.status == CallStatus::Ok => locals::describe_returned(l, run),
        _ => None,
    });
    steps.into_iter().chain(locals).collect()
}

/// Builds the report for `plan` from the harness outcome, or from the reason it could not run.
pub struct ReportContext<'a> {
    pub basis: Basis,
    pub environment: ExecutionEnvironment,
    pub attempt: Option<u32>,
    pub run: Option<GenerationRunId>,
    pub outcome: Result<&'a EvalOutcome, &'a str>,
}

pub fn report(plan: &Plan, cx: ReportContext<'_>) -> ValidationReport {
    let checks = checks(plan, cx.outcome);
    ValidationReport {
        id: ValidationId::generate(),
        created_at: Timestamp::now(),
        basis: cx.basis,
        environment: cx.environment,
        checks,
        attempt: cx.attempt,
        run: cx.run,
        source: Some(plan.source.clone()),
        settings: Some(plan.settings()),
    }
}

/// The checks of a validation, in the order of the sequence.
fn checks(plan: &Plan, outcome: Result<&EvalOutcome, &str>) -> Vec<Check> {
    let mut out = Vec::new();
    // Every check after the first one that stopped the sequence is skipped, with the reason.
    let mut stopped: Option<String> = None;
    let skip = |out: &mut Vec<Check>, subject, why: &str| {
        out.push(check(subject, CheckStatus::Skipped, vec![why.to_owned()]));
    };
    let later_subjects = |from: usize| -> Vec<CheckSubject> {
        let mut s = vec![
            CheckSubject::Parse,
            CheckSubject::Structure,
            CheckSubject::Lint,
            CheckSubject::Load,
        ];
        s.extend(plan.calls.iter().map(|c| c.subject));
        if plan.output_rules.is_some() {
            s.push(CheckSubject::Contract);
        }
        s.split_off(from.min(s.len()))
    };

    // 1. Parse.
    let syntax: Vec<String> = plan
        .structure
        .problems
        .iter()
        .filter(|p| p.kind == ProblemKind::Syntax)
        .map(ToString::to_string)
        .collect();
    let harness_read = outcome.ok().and_then(|o| o.report.read.clone());
    if !syntax.is_empty() {
        out.push(check(CheckSubject::Parse, CheckStatus::Failed, syntax));
        stopped = Some("Not run because the code could not be read.".into());
    } else if let Some(Err(e)) = &harness_read {
        out.push(check(
            CheckSubject::Parse,
            CheckStatus::Failed,
            vec![error_text(e)],
        ));
        stopped = Some("Not run because the code could not be read.".into());
    } else {
        out.push(check(CheckSubject::Parse, CheckStatus::Passed, Vec::new()));
    }

    // 2. Structure and dependencies.
    if let Some(why) = &stopped {
        skip(&mut out, CheckSubject::Structure, why);
    } else {
        let problems: Vec<_> = plan
            .structure
            .problems
            .iter()
            .filter(|p| p.kind != ProblemKind::Syntax)
            .collect();
        let harness_check = outcome.ok().and_then(|o| o.report.check.clone());
        if !problems.is_empty() {
            let forbidden = problems
                .iter()
                .any(|p| matches!(p.kind, ProblemKind::Forbidden | ProblemKind::Unsupported));
            out.push(check(
                CheckSubject::Structure,
                if forbidden {
                    CheckStatus::Forbidden
                } else {
                    CheckStatus::Failed
                },
                problems.iter().map(ToString::to_string).collect(),
            ));
            stopped = Some("Not run because the code's structure is not acceptable.".into());
        } else if let Some(Err(e)) = harness_check {
            out.push(check(
                CheckSubject::Structure,
                CheckStatus::Forbidden,
                vec![error_text(&e)],
            ));
            stopped = Some("Not run because the code's structure is not acceptable.".into());
        } else {
            let mut notes: Vec<String> = plan.structure.warnings.clone();
            if !plan.structure.needs_approval.is_empty() {
                notes.push(format!(
                    "Requires {}, which this function does not allow yet; it can be allowed when the \
                     code is used.",
                    plan.structure.needs_approval.join(", ")
                ));
            }
            out.push(check(CheckSubject::Structure, CheckStatus::Passed, notes));
        }
    }
    if let Some(why) = stopped {
        for s in later_subjects(2) {
            skip(&mut out, s, &why);
        }
        return out;
    }

    // The remaining checks need the harness.
    let o = match outcome {
        Ok(o) => o,
        Err(why) => {
            out.push(check(
                CheckSubject::Lint,
                CheckStatus::Skipped,
                vec![why.to_owned()],
            ));
            out.push(check(
                CheckSubject::Load,
                CheckStatus::Error,
                vec![why.to_owned()],
            ));
            for s in later_subjects(4) {
                skip(&mut out, s, "Not run because the code could not be loaded.");
            }
            return out;
        }
    };
    let r = &o.report;
    let finished = o.status == EvalStatus::Completed;
    let (died_status, died_why) = process_failure(&o.status, &o.stderr);
    // The process ended before reporting a stage: that stage carries the reason, later ones are
    // skipped.
    let died_reported = std::cell::Cell::new(false);
    let died = |out: &mut Vec<Check>, subject| {
        if died_reported.replace(true) {
            skip(out, subject, "Not run because the runtime stopped.");
        } else {
            out.push(check(subject, died_status, vec![died_why.clone()]));
        }
    };

    // 3. Lint.
    match &r.lint {
        Some(findings) => {
            let errors: Vec<String> = findings
                .iter()
                .filter(|f| f.level == LintLevel::Error)
                .map(|f| located(&f.message, f.line))
                .collect();
            let warnings = findings
                .iter()
                .filter(|f| f.level == LintLevel::Warning)
                .map(|f| located(&format!("warning: {}", f.message), f.line));
            let status = if errors.is_empty() {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
            out.push(check(
                CheckSubject::Lint,
                status,
                errors.into_iter().chain(warnings).collect(),
            ));
        }
        None if r.load.is_some() => skip(&mut out, CheckSubject::Lint, "The linter did not run."),
        None => died(&mut out, CheckSubject::Lint),
    }

    // 4. Load.
    match &r.load {
        Some(Ok(())) => out.push(check(CheckSubject::Load, CheckStatus::Passed, Vec::new())),
        Some(Err(e)) => {
            out.push(check(CheckSubject::Load, CheckStatus::Error, vec![error_text(e)]));
            for s in later_subjects(4) {
                skip(&mut out, s, "Not run because the code could not be loaded.");
            }
            return out;
        }
        None if finished => {
            out.push(check(
                CheckSubject::Load,
                CheckStatus::Error,
                vec!["The code did not load.".into()],
            ));
            for s in later_subjects(4) {
                skip(&mut out, s, "Not run because the code could not be loaded.");
            }
            return out;
        }
        None => died(&mut out, CheckSubject::Load),
    }

    // 5 and 6. Examples and tests, compared structurally.
    let mut contract: Vec<String> = Vec::new();
    let mut contract_checked = false;
    for (i, call) in plan.calls.iter().enumerate() {
        match r.result(i) {
            Some(result) => {
                if result.status == CallStatus::Ok {
                    contract_checked = true;
                    contract.extend(result.contract.iter().map(|p| format!("{}: {p}", call.label)));
                }
                let values = r
                    .probes
                    .iter()
                    .find(|p| p.index == i)
                    .map(|p| values(plan, result, p))
                    .unwrap_or_default();
                out.push(call_check(call, result, values, &plan.source));
            }
            None if r.halted => skip(
                &mut out,
                call.subject,
                "Not run because an earlier call exceeded its time limit and the runtime was stopped.",
            ),
            None if !finished => died(&mut out, call.subject),
            None => out.push(check(
                call.subject,
                CheckStatus::Error,
                vec!["The runtime did not report a result.".into()],
            )),
        }
    }

    // The output contract, over every result that was produced.
    if plan.output_rules.is_some() {
        if contract_checked {
            let status = if contract.is_empty() {
                CheckStatus::Passed
            } else {
                CheckStatus::Failed
            };
            out.push(check(CheckSubject::Contract, status, contract));
        } else {
            skip(
                &mut out,
                CheckSubject::Contract,
                "Not checked because no example or test produced a result.",
            );
        }
    }
    out
}

/// The failures of a report, written for the model: which checks failed and why, with expected
/// and actual values. Bounded to `max_chars`; examples are referred to by the specification's
/// labels.
pub fn failure_summary(plan: &Plan, report: &ValidationReport, max_chars: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut omitted = 0;
    // Linter warnings do not fail the code, but next to failing checks they often point at the
    // cause: a value that is computed and then dropped, or a binding shadowed by mistake.
    let warnings = lint_warnings(report);
    let mut used = warnings.as_ref().map_or(0, String::len);
    let mut with_values = 0;
    for c in &report.checks {
        if matches!(c.status, CheckStatus::Passed | CheckStatus::Skipped) {
            // Lint warnings on passing code are useful context, but only errors fail.
            continue;
        }
        let status = match c.status {
            CheckStatus::Failed => "failed",
            CheckStatus::Error => "error",
            CheckStatus::Timeout => "timed out",
            CheckStatus::ResourceLimit => "exceeded a resource limit",
            CheckStatus::Forbidden => "forbidden",
            CheckStatus::Passed | CheckStatus::Skipped => "",
        };
        let mut text = format!("{} — {status}", plan.label(&c.subject));
        // Recorded values are long; the first failing calls show where the data flow breaks.
        let show_values = with_values < MAX_CALLS_WITH_VALUES;
        let mut had_values = false;
        let errors: Vec<&String> = c
            .diagnostics
            .iter()
            .filter(|d| !d.starts_with("warning:") && !d.contains(": warning:"))
            .filter(|d| {
                let values = d.starts_with(VALUES_PREFIX);
                had_values |= values;
                show_values || !values
            })
            .collect();
        if had_values {
            with_values += 1;
        }
        for d in errors {
            text.push_str(&format!("\n  {}", bounded(d, 1600)));
        }
        if let (Some(e), CheckSubject::Example(_) | CheckSubject::Test(_)) = (&c.expected, c.subject) {
            if let Some(call) = plan.calls.iter().find(|p| p.subject == c.subject) {
                text.push_str(&format!("\n  Input: {}", bounded(call.input.trim(), 800)));
            }
            text.push_str(&format!("\n  Expected: {}", bounded(e.as_str().trim(), 800)));
            if let Some(a) = &c.actual {
                text.push_str(&format!("\n  Actual: {}", bounded(a.as_str().trim(), 800)));
            }
        }
        if used + text.len() > max_chars {
            omitted += 1;
            continue;
        }
        used += text.len();
        parts.push(text);
    }
    if omitted > 0 {
        parts.push(format!("({omitted} more failing checks are not shown.)"));
    }
    if !parts.is_empty() {
        parts.extend(warnings);
    }
    parts.join("\n\n")
}

/// The linter's warnings in a report, as a section of a failure summary.
fn lint_warnings(report: &ValidationReport) -> Option<String> {
    let lint = report.checks.iter().find(|c| c.subject == CheckSubject::Lint)?;
    let warnings: Vec<String> = lint
        .diagnostics
        .iter()
        .filter(|d| d.starts_with("warning:") || d.contains(": warning:"))
        .take(8)
        .map(|d| format!("\n  {}", bounded(d, 200)))
        .collect();
    if warnings.is_empty() {
        return None;
    }
    Some(format!(
        "Linter warnings — check whether one of these causes a failure above: a value that is \
         computed but never used usually means the result of a step is dropped instead of being \
         passed on.{}",
        warnings.concat()
    ))
}

/// Counts of a report's checks, for a short status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub fn tally(report: &ValidationReport) -> Tally {
    let mut t = Tally::default();
    for c in &report.checks {
        match c.status {
            CheckStatus::Passed => t.passed += 1,
            CheckStatus::Skipped => t.skipped += 1,
            _ => t.failed += 1,
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use clojurecraft_clojure::HarnessReport;
    use clojurecraft_domain::{
        Contract, Example, KeySpec, Shape, TestCase, TestCaseId, TestOrigin, TestStatus,
    };

    use super::*;

    const GOOD: &str = "(ns totals.core)\n(defn totals [xs] (reduce + xs))\n";

    fn doc() -> FunctionDocument {
        let mut d = FunctionDocument::new("Totals");
        d.examples = vec![
            Example::direct("", Edn::new("[1 2]"), Edn::new("3")),
            Example::direct("", Edn::new(""), Edn::new("")),
            Example::direct("empty", Edn::new("[]"), Edn::new("0")),
        ];
        d.tests = vec![
            TestCase {
                id: TestCaseId::generate(),
                name: "negative".into(),
                input: Edn::new("[-1]"),
                expectation: Expectation::Value {
                    expected: Edn::new("-1"),
                },
                status: TestStatus::Accepted,
                origin: TestOrigin::User,
                notes: String::new(),
            },
            TestCase {
                id: TestCaseId::generate(),
                name: "proposed".into(),
                input: Edn::new("[5]"),
                expectation: Expectation::Value {
                    expected: Edn::new("5"),
                },
                status: TestStatus::Proposed,
                origin: TestOrigin::Model,
                notes: String::new(),
            },
        ];
        d
    }

    fn names() -> Names {
        Names {
            namespace: "totals.core".into(),
            function: "totals".into(),
        }
    }

    fn env() -> ExecutionEnvironment {
        ExecutionEnvironment {
            clojure_version: "1.12.6".into(),
            java_version: "25".into(),
            platform: "linux-x86_64".into(),
        }
    }

    fn outcome(status: EvalStatus, stdout: &str) -> EvalOutcome {
        EvalOutcome {
            status,
            report: HarnessReport::parse(stdout),
            stderr: String::new(),
            elapsed: Duration::from_millis(10),
        }
    }

    fn build(plan: &Plan, doc: &FunctionDocument, o: Result<&EvalOutcome, &str>) -> ValidationReport {
        report(
            plan,
            ReportContext {
                basis: doc.basis_with_code(&plan.source),
                environment: env(),
                attempt: Some(1),
                run: None,
                outcome: o,
            },
        )
    }

    fn statuses(r: &ValidationReport) -> Vec<CheckStatus> {
        r.checks.iter().map(|c| c.status).collect()
    }

    const STAGES: &str = concat!(
        r#"{"event":"read","ok":true}"#,
        "\n",
        r#"{"event":"check","ok":true}"#,
        "\n",
        r#"{"event":"lint","findings":[{"level":"warning","line":2,"message":"x is bound but never used"}]}"#,
        "\n",
        r#"{"event":"load","ok":true}"#,
        "\n"
    );

    fn result(index: usize, matches: bool) -> String {
        format!(
            r#"{{"event":"result","index":{index},"status":"ok","actual":"4","match":{matches},"difference":{},"contract":[],"out":"","elapsed-ns":2000000}}"#,
            if matches {
                "null".to_owned()
            } else {
                r#""the output: expected 3 but got 4""#.to_owned()
            }
        )
    }

    #[test]
    fn examples_and_accepted_tests_become_calls_with_their_labels() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let labels: Vec<&str> = p.calls.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Example 1", "Example 3 (empty)", "Test negative"]);
        let req = p.request.as_ref().unwrap();
        assert_eq!(req.function, "totals");
        assert_eq!(req.calls.len(), 3);
        assert_eq!(req.calls[2].expected.as_deref(), Some("-1"));
        assert!(req.lint && req.static_checks);
    }

    #[test]
    fn the_accepted_contract_sets_output_rules_and_order() {
        let mut d = doc();
        let mut c = Contract::empty(ContractStatus::Proposed);
        c.output = Shape {
            description: String::new(),
            value_type: ValueType::Vector,
            keys: vec![KeySpec {
                key: Edn::new(":id"),
                required: true,
                value_type: ValueType::Integer,
                nil: NilBehavior::Disallowed,
                notes: String::new(),
            }],
        };
        c.ordering = OrderingRequirement::Unordered;
        d.contract = Some(c);
        let p = plan(&d, GOOD, &names());
        assert!(
            p.output_rules.is_none() && !p.equality.unordered,
            "a proposal is not a requirement"
        );
        d.contract.as_mut().unwrap().status = ContractStatus::Accepted;
        let p = plan(&d, GOOD, &names());
        let rules = p.output_rules.unwrap();
        assert_eq!(rules.value_type, Some(TypeRule::Vector));
        assert!(!rules.keys[0].nil_allowed && rules.keys[0].required);
        assert!(p.equality.unordered);
    }

    #[test]
    fn a_passing_run_passes_every_check() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n{}\n{}\n{{\"event\":\"done\"}}\n",
            result(0, true),
            result(1, true),
            result(2, true)
        );
        let o = outcome(EvalStatus::Completed, &stdout);
        let r = build(&p, &d, Ok(&o));
        assert!(r.passed(), "{:?}", r.checks);
        assert_eq!(r.checks.len(), 7);
        assert_eq!(
            r.checks[2].diagnostics,
            ["line 2: warning: x is bound but never used"]
        );
        assert_eq!(r.checks[4].duration_ms, Some(2));
        assert_eq!(r.source.as_deref(), Some(GOOD));
        assert_eq!(
            r.basis.code,
            clojurecraft_domain::fingerprint::code_fingerprint(Some(GOOD))
        );
    }

    #[test]
    fn a_mismatch_fails_with_expected_actual_and_the_difference() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n{}\n{}\n{{\"event\":\"done\"}}\n",
            result(0, false),
            result(1, true),
            result(2, true)
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &stdout)));
        assert!(!r.passed());
        let c = &r.checks[4];
        assert_eq!(c.status, CheckStatus::Failed);
        assert_eq!(c.expected.as_ref().map(Edn::as_str), Some("3"));
        assert_eq!(c.actual.as_ref().map(Edn::as_str), Some("4"));
        let summary = failure_summary(&p, &r, 4000);
        assert!(summary.contains("Example 1 — failed"), "{summary}");
        assert!(
            summary.contains("Input: [1 2]\n  Expected: 3\n  Actual: 4"),
            "{summary}"
        );
        // Warnings are no failing check, but are listed once as possible causes.
        assert_eq!(
            summary.matches("x is bound but never used").count(),
            1,
            "{summary}"
        );
        assert!(summary.contains("Linter warnings"), "{summary}");
    }

    #[test]
    fn structure_problems_stop_before_anything_runs() {
        let d = doc();
        let p = plan(&d, "(ns totals.core)\n(defn totals [x] (slurp x))\n", &names());
        assert!(p.request.is_none());
        let r = build(&p, &d, Err("unused"));
        assert_eq!(r.checks[1].status, CheckStatus::Forbidden);
        assert!(r.checks[2..].iter().all(|c| c.status == CheckStatus::Skipped));
        assert_eq!(r.checks.len(), 7, "every planned check is listed");

        let p = plan(&d, "(ns totals.core\n(defn totals [x] x)", &names());
        let r = build(&p, &d, Err("unused"));
        assert_eq!(r.checks[0].status, CheckStatus::Failed);
    }

    #[test]
    fn a_process_that_dies_is_attributed_to_the_running_call() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n{{\"event\":\"start\",\"index\":1}}\n",
            result(0, true)
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::MemoryLimitExceeded, &stdout)));
        assert_eq!(
            statuses(&r)[4..],
            [
                CheckStatus::Passed,
                CheckStatus::ResourceLimit,
                CheckStatus::Skipped
            ]
        );
        assert!(r.checks[5].diagnostics[0].contains("out of memory"));
    }

    #[test]
    fn a_timed_out_call_skips_the_rest() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n{}\n{{\"event\":\"halted\"}}\n",
            result(0, true),
            r#"{"event":"result","index":1,"status":"timeout","elapsed-ns":5000000000}"#
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &stdout)));
        assert_eq!(
            statuses(&r)[4..],
            [CheckStatus::Passed, CheckStatus::Timeout, CheckStatus::Skipped]
        );
    }

    #[test]
    fn lint_errors_and_load_errors_fail_with_lines() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = concat!(
            r#"{"event":"read","ok":true}"#,
            "\n",
            r#"{"event":"check","ok":true}"#,
            "\n",
            r#"{"event":"lint","findings":[{"level":"error","line":2,"message":"Unable to resolve symbol: sum"}]}"#,
            "\n",
            r#"{"event":"load","ok":false,"phase":"load","class":"java.lang.RuntimeException","message":"Unable to resolve symbol: sum in this context","line":2}"#,
            "\n",
            r#"{"event":"done"}"#
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, stdout)));
        assert_eq!(r.checks[2].status, CheckStatus::Failed);
        assert_eq!(r.checks[2].diagnostics, ["line 2: Unable to resolve symbol: sum"]);
        assert_eq!(r.checks[3].status, CheckStatus::Error);
        assert!(r.checks[3].diagnostics[0].starts_with("line 2: RuntimeException: Unable"));
        assert!(r.checks[4..].iter().all(|c| c.status == CheckStatus::Skipped));
    }

    #[test]
    fn a_thrown_error_quotes_the_source_line_that_threw() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n",
            r#"{"event":"result","index":0,"status":"error","error":{"phase":"call","class":"java.lang.IllegalArgumentException","message":"Don't know how to create ISeq from: java.lang.Long","line":2}}"#
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &stdout)));
        assert_eq!(
            r.checks[4].diagnostics[0],
            "The function threw line 2: IllegalArgumentException: Don't know how to create ISeq from: \
             java.lang.Long\nThe line that threw: (defn totals [xs] (reduce + xs))"
        );
    }

    #[test]
    fn sandbox_denials_are_forbidden_and_unavailable_runtimes_are_errors() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n",
            r#"{"event":"result","index":0,"status":"error","error":{"phase":"call","class":"java.io.FileNotFoundException","message":"/etc/hostname (Permission denied)"}}"#
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &stdout)));
        assert_eq!(r.checks[4].status, CheckStatus::Forbidden);

        let r = build(&p, &d, Err("The Java runtime is not installed."));
        assert_eq!(r.checks[3].status, CheckStatus::Error);
        assert_eq!(r.checks[3].diagnostics, ["The Java runtime is not installed."]);
        assert!(!r.passed());
    }

    #[test]
    fn contract_violations_are_collected_into_one_check() {
        let mut d = doc();
        let mut c = Contract::empty(ContractStatus::Accepted);
        c.output.value_type = ValueType::Integer;
        d.contract = Some(c);
        let p = plan(&d, GOOD, &names());
        let with_contract = |i: usize| {
            format!(
                r#"{{"event":"result","index":{i},"status":"ok","actual":"3","match":true,"contract":["the output must be an integer but is a string"],"elapsed-ns":1}}"#
            )
        };
        let stdout = format!(
            "{STAGES}{}\n{}\n{}\n{{\"event\":\"done\"}}\n",
            with_contract(0),
            result(1, true),
            result(2, true)
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &stdout)));
        let last = r.checks.last().unwrap();
        assert_eq!(last.subject, CheckSubject::Contract);
        assert_eq!(last.status, CheckStatus::Failed);
        assert_eq!(
            last.diagnostics,
            ["Example 1: the output must be an integer but is a string"]
        );
    }

    #[test]
    fn summaries_stay_within_their_budget() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let stdout = format!(
            "{STAGES}{}\n{}\n{}\n{{\"event\":\"done\"}}\n",
            result(0, false),
            result(1, false),
            result(2, false)
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &stdout)));
        let s = failure_summary(&p, &r, 250);
        assert!(s.len() < 600, "{s}");
        assert!(s.contains("more failing checks are not shown"), "{s}");
    }

    #[test]
    fn summaries_of_failing_code_carry_the_linter_warnings() {
        let d = doc();
        let p = plan(&d, GOOD, &names());
        let failing = format!(
            "{STAGES}{}\n{}\n{}\n{{\"event\":\"done\"}}\n",
            result(0, false),
            result(1, true),
            result(2, true)
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &failing)));
        let s = failure_summary(&p, &r, 6000);
        assert!(s.contains("expected 3 but got 4"), "{s}");
        assert!(s.contains("Linter warnings"), "{s}");
        assert!(s.contains("line 2: warning: x is bound but never used"), "{s}");

        // Warnings alone are no failure.
        let passing = format!(
            "{STAGES}{}\n{}\n{}\n{{\"event\":\"done\"}}\n",
            result(0, true),
            result(1, true),
            result(2, true)
        );
        let r = build(&p, &d, Ok(&outcome(EvalStatus::Completed, &passing)));
        assert_eq!(failure_summary(&p, &r, 6000), "");
    }
}
