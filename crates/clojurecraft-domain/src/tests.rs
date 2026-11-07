//! Staleness rules: which edits invalidate which results.

use crate::*;

fn sample() -> FunctionDocument {
    let mut doc = FunctionDocument::new("orders by customer");
    doc.description = "Group orders by customer id and total their amounts.".into();
    doc.steps = vec![Step::new("group by :customer"), Step::new("sum :amount")];
    doc.examples = vec![Example::direct(
        "two orders",
        Edn::new("[{:customer 1 :amount 2} {:customer 1 :amount 3}]"),
        Edn::new("{1 5}"),
    )];
    doc.implementation = Some(Implementation::manual("(ns x)\n(defn f [xs] xs)"));
    doc.tests = vec![TestCase {
        id: TestCaseId::generate(),
        name: "empty".into(),
        input: Edn::new("[]"),
        expectation: Expectation::Value {
            expected: Edn::new("{}"),
        },
        status: TestStatus::Accepted,
        origin: TestOrigin::User,
        notes: String::new(),
    }];
    doc
}

fn report_for(doc: &FunctionDocument) -> ValidationReport {
    ValidationReport {
        id: ValidationId::generate(),
        created_at: Timestamp::now(),
        basis: doc.basis(),
        environment: ExecutionEnvironment {
            clojure_version: "1.12.6".into(),
            java_version: "25.0.5".into(),
            platform: "linux-x86_64".into(),
        },
        checks: vec![Check {
            subject: CheckSubject::Load,
            status: CheckStatus::Passed,
            expected: None,
            actual: None,
            diagnostics: vec![],
            duration_ms: Some(3),
        }],
        attempt: None,
        run: None,
        source: None,
        settings: None,
    }
}

fn stale(changed: &[Component]) -> Freshness {
    Freshness::Stale(changed.to_vec())
}

#[test]
fn material_edits_mark_validation_stale_by_component() {
    let doc = sample();
    let report = report_for(&doc);
    assert!(report.passed());
    assert_eq!(report.freshness(&doc.basis()), Freshness::Current);

    type Edit = Box<dyn Fn(&mut FunctionDocument)>;
    let edits: Vec<(&str, Edit, Freshness)> = vec![
        (
            "description",
            Box::new(|d| d.description.push('!')),
            stale(&[Component::Specification]),
        ),
        (
            "step text",
            Box::new(|d| d.steps[1].text = "sum :total".into()),
            stale(&[Component::Specification]),
        ),
        (
            "step order",
            Box::new(|d| d.steps.swap(0, 1)),
            stale(&[Component::Specification]),
        ),
        (
            "expected output",
            Box::new(|d| d.examples[0].expected = Edn::new("{1 6}")),
            stale(&[Component::Specification]),
        ),
        (
            "code",
            Box::new(|d| {
                if let Some(i) = &mut d.implementation {
                    i.source.push(' ');
                }
            }),
            stale(&[Component::Code]),
        ),
        (
            "accepted test",
            Box::new(|d| d.tests[0].input = Edn::new("()")),
            stale(&[Component::Tests]),
        ),
        (
            "test rejected",
            Box::new(|d| d.tests[0].status = TestStatus::Rejected),
            stale(&[Component::Tests]),
        ),
        (
            "spec and code",
            Box::new(|d| {
                d.description.clear();
                d.implementation = None;
            }),
            stale(&[Component::Specification, Component::Code]),
        ),
    ];
    for (what, edit, expected) in edits {
        let mut edited = doc.clone();
        edit(&mut edited);
        assert_eq!(report.freshness(&edited.basis()), expected, "{what}");
    }
}

#[test]
fn cosmetic_edits_and_proposals_keep_results_current() {
    let doc = sample();
    let report = report_for(&doc);
    let mut edited = doc.clone();
    edited.name = "totals by customer".into();
    edited.examples[0].name = "renamed".into();
    // Rewording keeps the identity; only the text matters, and here it is unchanged.
    edited.steps[0].id = StepId::generate();
    edited.tests.push(TestCase {
        id: TestCaseId::generate(),
        name: "proposed".into(),
        input: Edn::new("nil"),
        expectation: Expectation::NeedsClarification {
            question: "Should nil input return {} or nil?".into(),
        },
        status: TestStatus::Proposed,
        origin: TestOrigin::Model,
        notes: String::new(),
    });
    edited.contract = Some(Contract {
        status: ContractStatus::Proposed,
        input: Shape::default(),
        output: Shape::default(),
        ordering: OrderingRequirement::Unspecified,
        allowed_dependencies: vec![],
        notes: String::new(),
    });
    assert_eq!(report.freshness(&edited.basis()), Freshness::Current);
    assert_ne!(edited.content_fingerprint(), doc.content_fingerprint());

    // Accepting the proposed contract turns it into a requirement.
    if let Some(c) = &mut edited.contract {
        c.status = ContractStatus::Accepted;
    }
    assert_eq!(
        report.freshness(&edited.basis()),
        stale(&[Component::Specification])
    );
}

#[test]
fn ambiguous_cases_cannot_be_accepted() {
    let mut case = TestCase {
        id: TestCaseId::generate(),
        name: "nil".into(),
        input: Edn::new("nil"),
        expectation: Expectation::NeedsClarification { question: "?".into() },
        status: TestStatus::Proposed,
        origin: TestOrigin::Model,
        notes: String::new(),
    };
    assert_eq!(case.accept(), Err(DomainError::AmbiguousTest(case.id)));
    assert_eq!(case.status, TestStatus::Proposed);
    case.expectation = Expectation::Value {
        expected: Edn::new("{}"),
    };
    case.accept().unwrap();
    assert!(case.is_active());
}

#[test]
fn referenced_inputs_use_the_recorded_snapshot() {
    let mut doc = sample();
    let reference = OutputReference {
        function: FunctionId::generate(),
        revision: RevisionPin::Latest,
        example: ExampleId::generate(),
    };
    doc.examples[0].input = ExampleInput::FunctionOutput {
        source: reference,
        snapshot: Some(ReferencedValue {
            value: Edn::new("[1]"),
            revision: RevisionId::generate(),
            resolved_at: Timestamp(1),
        }),
    };
    let before = doc.spec_fingerprint();
    // Re-resolving to the same value at a later time is not a material change.
    if let ExampleInput::FunctionOutput {
        snapshot: Some(s), ..
    } = &mut doc.examples[0].input
    {
        s.resolved_at = Timestamp(2);
    }
    // The resolution time is provenance only; a refresh that changes the value (or the upstream
    // revision it came from) marks results stale.
    let after_time = doc.spec_fingerprint();
    if let ExampleInput::FunctionOutput {
        snapshot: Some(s), ..
    } = &mut doc.examples[0].input
    {
        s.value = Edn::new("[2]");
    }
    assert_eq!(before, after_time);
    assert_ne!(after_time, doc.spec_fingerprint());
}

#[test]
fn benchmarks_go_stale_on_content_or_configuration_changes() {
    let mut doc = sample();
    let config = BenchmarkConfig {
        id: BenchmarkConfigId::generate(),
        name: "sizes".into(),
        inputs: vec![BenchmarkInput {
            id: BenchmarkInputId::generate(),
            label: "small".into(),
            value: Edn::new("[]"),
            size_label: "0".into(),
        }],
        warmup_runs: 5,
        measured_runs: 20,
        max_duration_ms: 30_000,
        max_heap_mib: 512,
        realization: Realization::Full,
    };
    doc.benchmarks.push(config.clone());
    let result = BenchmarkResult {
        id: BenchmarkResultId::generate(),
        created_at: Timestamp::now(),
        config_id: config.id,
        config_fingerprint: config.fingerprint(),
        basis: doc.basis(),
        candidate_label: "baseline".into(),
        environment: BenchmarkEnvironment {
            execution: report_for(&doc).environment,
            os_version: "test".into(),
            cpu: "test".into(),
            inference_server_loaded: false,
        },
        startup_ms: Some(400.0),
        measurements: vec![],
    };
    let fp = |d: &FunctionDocument| d.benchmark(config.id).map(BenchmarkConfig::fingerprint);
    assert!(result.freshness(&doc.basis(), fp(&doc).as_ref()).is_current());

    doc.benchmarks[0].name = "renamed".into();
    doc.benchmarks[0].inputs[0].label = "tiny".into();
    assert!(result.freshness(&doc.basis(), fp(&doc).as_ref()).is_current());

    doc.benchmarks[0].measured_runs = 21;
    assert_eq!(
        result.freshness(&doc.basis(), fp(&doc).as_ref()),
        stale(&[Component::BenchmarkConfiguration])
    );
    doc.benchmarks.clear();
    assert_eq!(
        result.freshness(&doc.basis(), None),
        stale(&[Component::BenchmarkConfiguration])
    );
}

#[test]
fn reordering_steps_keeps_identities_and_marks_results_stale() {
    let mut doc = sample();
    let third = doc.add_step(None, "sort by total");
    let ids: Vec<StepId> = doc.steps.iter().map(|s| s.id).collect();
    let report = report_for(&doc);

    assert!(doc.move_step(third, 0));
    let moved: Vec<StepId> = doc.steps.iter().map(|s| s.id).collect();
    assert_eq!(moved, [ids[2], ids[0], ids[1]]);
    assert_eq!(doc.steps[0].text, "sort by total");
    // Order is part of the specification.
    assert_eq!(report.freshness(&doc.basis()), stale(&[Component::Specification]));

    // Moving back restores the original fingerprint exactly.
    assert!(doc.move_step(third, usize::MAX));
    assert_eq!(report.freshness(&doc.basis()), Freshness::Current);
    assert!(!doc.move_step(third, 99), "already last: nothing to move");
    assert!(!doc.move_step(StepId::generate(), 0));
}

#[test]
fn inserted_steps_land_after_their_anchor() {
    let mut doc = sample();
    let first = doc.steps[0].id;
    let new = doc.add_step(Some(first), "drop cancelled orders");
    assert_eq!(doc.steps[1].id, new);
    let unknown_anchor = doc.add_step(Some(StepId::generate()), "last");
    assert_eq!(doc.steps.last().map(|s| s.id), Some(unknown_anchor));
}

#[test]
fn undoing_a_removal_restores_the_same_step_and_example() {
    let mut doc = sample();
    let before = doc.basis();
    let step = doc.steps[0].id;
    let (index, removed) = doc.remove_step(step).expect("step exists");
    assert_ne!(doc.basis(), before);
    assert!(doc.insert_step(index, removed.clone()));
    assert!(!doc.insert_step(index, removed), "never duplicated");
    assert_eq!(doc.steps[0].id, step);
    assert_eq!(doc.basis(), before);

    let example = doc.examples[0].id;
    let (index, removed) = doc.remove_example(example).expect("example exists");
    assert!(doc.examples.is_empty());
    assert!(doc.insert_example(index, removed));
    assert_eq!(doc.examples[0].id, example);
    assert_eq!(doc.basis(), before);
}

#[test]
fn a_new_example_is_empty_and_direct() {
    let mut doc = FunctionDocument::new("f");
    let id = doc.add_example("Example 1");
    let example = doc.example(id).expect("added");
    assert_eq!(
        example.input,
        ExampleInput::Direct {
            value: Edn::default()
        }
    );
    assert!(example.expected.as_str().is_empty());
}

#[test]
fn only_answered_clarifications_change_the_specification() {
    let mut doc = sample();
    let before = doc.spec_fingerprint();
    let mut question = Clarification::open("Are refunds negative amounts?", "step 2 says sum");
    doc.replace_open_questions(vec![question.clone()]);
    assert_eq!(
        doc.spec_fingerprint(),
        before,
        "an open question is not a requirement"
    );

    doc.interpretation = Some(Interpretation {
        restatement: "Totals per customer".into(),
        assumptions: vec!["amounts are numbers".into()],
        issues: vec![],
        spec_basis: before.clone(),
        model: "m".into(),
        created_at: Timestamp(0),
    });
    assert_eq!(
        doc.spec_fingerprint(),
        before,
        "the model's reading is not a requirement"
    );

    let id = question.id;
    doc.clarification_mut(id)
        .unwrap()
        .answer("yes, keep them negative");
    let answered = doc.spec_fingerprint();
    assert_ne!(answered, before);

    // An empty answer reopens the question.
    doc.clarification_mut(id).unwrap().answer("  ");
    assert_eq!(doc.spec_fingerprint(), before);
    doc.clarification_mut(id).unwrap().status = ClarificationStatus::Dismissed;
    assert_eq!(doc.spec_fingerprint(), before);

    question.answer("yes");
    doc.clarifications = vec![question];
    assert_ne!(doc.spec_fingerprint(), before);
}

#[test]
fn new_questions_never_discard_answers() {
    let mut doc = sample();
    let mut answered = Clarification::open("a?", "");
    answered.answer("yes");
    let mut dismissed = Clarification::open("b?", "");
    dismissed.status = ClarificationStatus::Dismissed;
    doc.clarifications = vec![
        answered.clone(),
        Clarification::open("old?", ""),
        dismissed.clone(),
    ];
    doc.replace_open_questions(vec![Clarification::open("new?", "")]);
    let questions: Vec<&str> = doc.clarifications.iter().map(|c| c.question.as_str()).collect();
    assert_eq!(questions, ["a?", "b?", "new?"]);
}

#[test]
fn whole_value_types_change_the_accepted_contract_fingerprint() {
    let mut doc = sample();
    doc.contract = Some(Contract::empty(ContractStatus::Accepted));
    let before = doc.spec_fingerprint();
    doc.contract.as_mut().unwrap().output.value_type = ValueType::Map;
    assert_ne!(doc.spec_fingerprint(), before);
}
