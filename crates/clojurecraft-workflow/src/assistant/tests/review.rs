use clojurecraft_domain::{
    Check, CheckStatus, CheckSubject, Edn, Expectation, TestCase, TestCaseId, TestOrigin, TestStatus,
    ValidationId,
};

use super::*;

const FIXED: &str = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (into {} (map (fn [[c os]] [c (reduce + (map :amount os))]) (group-by :customer orders))))\n";

/// Generates `CODE`, lets it pass, and applies it.
fn generated(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs, id: FunctionId) {
    generate_code(a, p, d, id, CODE);
    validated(a, p, d, true);
    let updates = reply(a, p, d, explanation());
    assert!(matches!(&updates[..], [Update::Commit(..)]));
}

fn report_of(doc: &FunctionDocument, checks: Vec<Check>) -> ValidationReport {
    ValidationReport {
        id: ValidationId::generate(),
        created_at: Timestamp::now(),
        basis: doc.basis(),
        environment: env(),
        checks,
        attempt: None,
        run: None,
        source: doc.implementation.as_ref().map(|i| i.source.clone()),
        settings: None,
    }
}

fn check(subject: CheckSubject, status: CheckStatus) -> Check {
    Check {
        subject,
        status,
        expected: Some(Edn::new("{}")),
        actual: Some(Edn::new("nil")),
        diagnostics: vec![],
        duration_ms: None,
    }
}

/// Accepts the current code with a passing report about exactly the current content.
fn accept_current(d: &mut Docs, id: FunctionId) {
    let doc = d.0.get_mut(&id).unwrap();
    let passing = report_of(doc, vec![check(CheckSubject::Load, CheckStatus::Passed)]);
    doc.accept_implementation(&passing).unwrap();
}

/// Adds an accepted edge case and returns a failing report of the current code for it.
fn failing_case(d: &mut Docs, id: FunctionId) -> (TestCaseId, ValidationReport) {
    let doc = d.0.get_mut(&id).unwrap();
    let case = TestCase {
        id: TestCaseId::generate(),
        name: "no orders".into(),
        input: Edn::new("[]"),
        expectation: Expectation::Value {
            expected: Edn::new("{}"),
        },
        status: TestStatus::Accepted,
        origin: TestOrigin::ModelEdited,
        notes: String::new(),
    };
    let case_id = case.id;
    doc.tests.push(case);
    let failing = report_of(
        doc,
        vec![
            check(CheckSubject::Load, CheckStatus::Passed),
            check(CheckSubject::Test(case_id), CheckStatus::Failed),
        ],
    );
    (case_id, failing)
}

#[test]
fn proposed_edge_cases_are_added_for_review_without_changing_results() {
    let (mut a, mut p, mut d, id) = setup();
    generated(&mut a, &mut p, &mut d, id);
    let before = d.0[&id].clone();
    a.propose_tests(&d.0[&id], &mut p);
    assert_eq!(a.job().unwrap().kind, JobKind::ProposeTests);
    assert_eq!(p.last().1, Role::Interpretation);
    assert!(!p.last_text().contains("update-vals"), "the code is not sent");
    let updates = reply(
        &mut a,
        &mut p,
        &mut d,
        json!({"cases": [
            {"name": "no orders", "category": "empty", "input": "[]", "outcome": "defined",
             "expected": "{}", "question": "", "why": "nothing to group"},
            {"name": "nil amount", "category": "nil", "input": "[{:customer 1 :amount nil}]",
             "outcome": "ambiguous", "expected": "", "question": "Is nil 0?", "why": "unspecified"}
        ]}),
    );
    assert_eq!(updates, [Update::Edited(id)]);
    assert!(a.job().is_none());
    assert!(matches!(
        a.report(id),
        Some(Report::TestsProposed {
            added: 2,
            ambiguous: 1,
            ..
        })
    ));
    let doc = &d.0[&id];
    assert_eq!(doc.tests.len(), 2);
    assert!(
        doc.tests
            .iter()
            .all(|t| t.origin == TestOrigin::Model && t.status == TestStatus::Proposed)
    );
    // Discovery changes neither the code nor what validation checks.
    assert_eq!(doc.implementation, before.implementation);
    assert_eq!(doc.basis(), before.basis());
}

#[test]
fn edge_cases_are_generated_from_examples_without_any_code() {
    let (mut a, mut p, mut d, id) = setup();
    assert!(d.0[&id].implementation.is_none());
    a.generate_tests_from_examples(&d.0[&id], &mut p);
    assert_eq!(a.job().unwrap().kind, JobKind::ProposeTests);
    assert_eq!(p.last().1, Role::Interpretation);
    let updates = reply(
        &mut a,
        &mut p,
        &mut d,
        json!({"cases": [
            {"name": "no orders", "category": "empty", "input": "[]", "outcome": "defined",
             "expected": "{}", "question": "", "why": "nothing to group"}
        ]}),
    );
    assert_eq!(updates, [Update::Edited(id)]);
    assert!(matches!(
        a.report(id),
        Some(Report::TestsProposed { added: 1, .. })
    ));
    let t = &d.0[&id].tests[0];
    assert_eq!((t.origin, t.status), (TestOrigin::Model, TestStatus::Proposed));
}

#[test]
fn generating_edge_cases_without_examples_is_blocked() {
    let (mut a, mut p, d, id) = setup();
    let mut doc = d.0[&id].clone();
    doc.examples.clear();
    a.generate_tests_from_examples(&doc, &mut p);
    assert!(a.job().is_none());
    assert!(matches!(a.report(id), Some(Report::Blocked(_))));
}

#[test]
fn regenerated_code_never_silently_replaces_the_accepted_version() {
    let (mut a, mut p, mut d, id) = setup();
    generated(&mut a, &mut p, &mut d, id);
    accept_current(&mut d, id);
    // Unedited generated code would normally be replaced; accepted code is not.
    assert!(crate::replaces_silently(&d.0[&id]));
    generate_code(&mut a, &mut p, &mut d, id, FIXED);
    validated(&mut a, &mut p, &mut d, true);
    let updates = reply(&mut a, &mut p, &mut d, explanation());
    assert!(updates.is_empty(), "{updates:?}");
    let Some(Report::Pending(pending)) = a.report(id) else {
        panic!("{:?}", a.report(id))
    };
    assert!(pending.replaces_accepted && !pending.fix);
    let doc = &d.0[&id];
    assert_eq!(doc.implementation.as_ref().unwrap().source, CODE);
    assert!(doc.is_accepted_code());

    // Choosing the new code replaces the implementation but does not move the acceptance.
    let updates = a.accept_pending(d.0.get_mut(&id).unwrap()).unwrap();
    assert!(matches!(&updates[..], [Update::Commit(_, r)] if r.kind == RevisionKind::Generated));
    let doc = &d.0[&id];
    assert_eq!(doc.implementation.as_ref().unwrap().source, FIXED);
    assert!(doc.differs_from_accepted());
    assert_eq!(doc.accepted.as_ref().unwrap().source, CODE);
}

#[test]
fn a_fix_repairs_the_current_code_with_every_accepted_case_and_waits_for_review() {
    let (mut a, mut p, mut d, id) = setup();
    generated(&mut a, &mut p, &mut d, id);
    accept_current(&mut d, id);
    let (case_id, failing) = failing_case(&mut d, id);
    let tests_before = d.0[&id].tests.clone();

    a.fix_failing(&d.0[&id], model(), false, &failing, &mut p);
    let job = a.job().unwrap();
    assert!(job.fixing());
    assert_eq!(job.phase, Phase::Repairing);
    assert_eq!(job.attempt(), Some((1, MAX_ATTEMPTS)));
    let text = p.last_text();
    for needle in [
        "attempt 1 of 4",
        "Your previous implementation",
        "update-vals",
        "Test no orders — failed",
        "Expected: {}",
        "Actual: nil",
        "Test no orders, accepted by the user",
        "(ns order-totals.core)",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }

    reply(&mut a, &mut p, &mut d, json!({"code": FIXED, "assumptions": []}));
    // The candidate runs against the example and the accepted case.
    let req = p.validations.last().unwrap();
    assert_eq!(req.calls.len(), 2);
    assert_eq!(req.calls[1].expected.as_deref(), Some("{}"));
    validated(&mut a, &mut p, &mut d, true);
    let updates = reply(&mut a, &mut p, &mut d, explanation());
    assert!(updates.is_empty(), "a fix is never applied without review");
    let Some(Report::Pending(pending)) = a.report(id) else {
        panic!("{:?}", a.report(id))
    };
    assert!(pending.fix && pending.passed());
    assert_eq!(pending.candidate.code, FIXED);
    assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, CODE);

    // After review the fix replaces the code and becomes the accepted version.
    let updates = a.accept_pending(d.0.get_mut(&id).unwrap()).unwrap();
    assert!(
        matches!(&updates[..], [Update::Commit(_, r)] if r.kind == RevisionKind::Accepted),
        "{updates:?}"
    );
    let doc = &d.0[&id];
    assert_eq!(doc.implementation.as_ref().unwrap().source, FIXED);
    assert!(doc.is_accepted_code());
    assert_eq!(doc.tests, tests_before, "the accepted cases are unchanged");
    assert!(doc.test(case_id).unwrap().is_active());
}

#[test]
fn a_fix_that_never_passes_changes_nothing_and_never_weakens_the_cases() {
    let (mut a, mut p, mut d, id) = setup();
    generated(&mut a, &mut p, &mut d, id);
    accept_current(&mut d, id);
    let (case_id, failing) = failing_case(&mut d, id);
    let before = d.0[&id].clone();

    a.fix_failing(&d.0[&id], model(), false, &failing, &mut p);
    for _ in 1..=MAX_ATTEMPTS {
        reply(&mut a, &mut p, &mut d, json!({"code": WRONG, "assumptions": []}));
        // Every attempt is checked against the accepted case with its original expectation.
        let req = p.validations.last().unwrap();
        assert!(req.calls.iter().any(|c| c.expected.as_deref() == Some("{}")));
        validated(&mut a, &mut p, &mut d, false);
    }
    assert!(a.job().is_none());
    let Some(Report::Unresolved { attempts, best, .. }) = a.report(id) else {
        panic!("{:?}", a.report(id))
    };
    assert_eq!(attempts.len(), MAX_ATTEMPTS as usize);
    assert!(best.as_ref().is_some_and(|b| b.fix));
    assert_eq!(d.0[&id], before, "nothing was applied or changed");
    assert!(d.0[&id].test(case_id).unwrap().is_active());
}

#[test]
fn a_fix_needs_failures_of_exactly_the_current_content() {
    let (mut a, mut p, mut d, id) = setup();
    generated(&mut a, &mut p, &mut d, id);
    let (_, failing) = failing_case(&mut d, id);
    // The code was edited after the failing check.
    if let Some(i) = &mut d.0.get_mut(&id).unwrap().implementation {
        i.source.push_str("\n;; edited");
    }
    let sent = p.sent.len();
    a.fix_failing(&d.0[&id], model(), false, &failing, &mut p);
    assert!(a.job().is_none());
    assert_eq!(p.sent.len(), sent);
    assert!(matches!(a.report(id), Some(Report::Blocked(_))));
}
