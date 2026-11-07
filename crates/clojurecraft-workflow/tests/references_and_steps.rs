//! Referenced inputs and intermediate step values with real sandboxed execution.
//!
//! Needs the pinned Java runtime and Clojure (`clojurecraft-devtool install-runtime`) and the
//! sandbox helper (built by `cargo test --workspace`); without them the tests print why and pass
//! without running.

use clojurecraft_clojure::{ClojureRuntime, EvalOutcome, EvalRequest, Limits};
use clojurecraft_domain::{
    Edn, Example, Explanation, Fingerprint, FunctionDocument, GenerationRecord, GenerationSettings,
    Implementation, InferenceRuntime, LineRange, ModelIdentity, OutputReference, RevisionId, RevisionPin,
    Step, StepMapping, Timestamp,
};
use clojurecraft_runtime::{DataDirs, Manifest, components, platform_key};
use clojurecraft_workflow::intermediate::{self, StepPlan, StepValue};
use clojurecraft_workflow::references::{self, ReferenceError, Upstream};
use clojurecraft_workflow::validation;

fn runtime() -> Option<ClojureRuntime> {
    let manifest = Manifest::bundled().ok()?;
    let dirs = DataDirs::discover()?;
    let Some((java, java_home)) = components::java(&manifest, &dirs, &platform_key()) else {
        eprintln!("skipping: Java runtime not installed");
        return None;
    };
    let jars = components::clojure_jars(&manifest, &dirs)?;
    let helper = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .join(clojurecraft_sandbox::HELPER_NAME);
    if !helper.is_file() {
        eprintln!("skipping: sandbox helper not built at {}", helper.display());
        return None;
    }
    Some(ClojureRuntime {
        java,
        java_home,
        jars,
        sandbox_helper: helper,
        work_dir: std::env::temp_dir().join(format!("clojurecraft-references-test-{}", std::process::id())),
    })
}

fn run(rt: &ClojureRuntime, request: &EvalRequest) -> EvalOutcome {
    rt.evaluate(request, &Limits::for_request(request), None)
        .expect("evaluate")
}

fn upstream(code: &str) -> FunctionDocument {
    let mut up = FunctionDocument::new("paid orders");
    up.examples.push(Example::direct(
        "mixed",
        Edn::new("[{:id 1 :amount 3 :paid true} {:id 2 :amount 4} {:id 3 :amount 5 :paid true}]"),
        Edn::new("[{:id 1 :amount 3 :paid true} {:id 3 :amount 5 :paid true}]"),
    ));
    up.implementation = Some(Implementation::manual(code));
    up
}

fn downstream(up: &FunctionDocument) -> (FunctionDocument, OutputReference) {
    let mut down = FunctionDocument::new("order total");
    let id = down.add_example("from paid orders");
    down.example_mut(id).expect("example").expected = Edn::new("8");
    let source = OutputReference {
        function: up.id,
        revision: RevisionPin::Latest,
        example: up.examples[0].id,
    };
    down.set_input_reference(id, source.clone());
    (down, source)
}

#[test]
fn an_upstream_output_becomes_the_downstream_input_used_by_validation() {
    let Some(rt) = runtime() else { return };
    let up = upstream("(ns paid-orders.core)\n(defn paid-orders [orders]\n  (filterv :paid orders))");
    let (mut down, source) = downstream(&up);
    let revision = RevisionId::generate();
    let plan = references::plan(
        &down,
        down.examples[0].id,
        &source,
        &Upstream {
            revision,
            number: 4,
            document: &up,
        },
        &[],
    )
    .expect("plan");
    let outcome = run(&rt, &plan.request);
    let resolution = references::resolve(&plan, Ok(&outcome), Timestamp(10)).expect("resolved");
    assert_eq!(
        resolution.snapshot.value.as_str(),
        "[{:id 1, :amount 3, :paid true} {:id 3, :amount 5, :paid true}]"
    );
    assert_eq!(resolution.snapshot.revision, revision);
    assert!(resolution.notes.is_empty(), "{:?}", resolution.notes);

    let id = down.examples[0].id;
    assert!(down.record_reference_snapshot(id, &source, resolution.snapshot.clone()));
    // Validation runs the downstream function on exactly the recorded value.
    let calls = validation::planned_calls(&down);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].input, resolution.snapshot.value.as_str());
}

#[test]
fn a_failing_upstream_example_is_reported_and_nothing_is_recorded() {
    let Some(rt) = runtime() else { return };
    let up =
        upstream("(ns paid-orders.core)\n(defn paid-orders [orders]\n  (mapv #(/ (:amount %) 0) orders))");
    let (down, source) = downstream(&up);
    let plan = references::plan(
        &down,
        down.examples[0].id,
        &source,
        &Upstream {
            revision: RevisionId::generate(),
            number: 1,
            document: &up,
        },
        &[],
    )
    .expect("plan");
    let outcome = run(&rt, &plan.request);
    let err = references::resolve(&plan, Ok(&outcome), Timestamp(1)).unwrap_err();
    assert!(matches!(err, ReferenceError::UpstreamFailed(_)), "{err:?}");
    assert!(err.to_string().contains("ArithmeticException"), "{err}");
    assert!(err.to_string().contains("Divide by zero"), "{err}");

    // Returning a function is not data.
    let mut up = up;
    up.implementation = Some(Implementation::manual(
        "(ns paid-orders.core)\n(defn paid-orders [orders]\n  (fn [] orders))",
    ));
    let plan = references::plan(
        &down,
        down.examples[0].id,
        &source,
        &Upstream {
            revision: RevisionId::generate(),
            number: 1,
            document: &up,
        },
        &[],
    )
    .expect("plan");
    let outcome = run(&rt, &plan.request);
    let err = references::resolve(&plan, Ok(&outcome), Timestamp(1)).unwrap_err();
    assert!(matches!(err, ReferenceError::OutputNotData(_)), "{err:?}");
}

const TOTALS: &str = "(ns order-total.core)

(defn order-total [orders]
  (let [paid (filter :paid orders)]
    (->> paid
         (map #(* (:amount %) (or (:qty %) 1)))
         (reduce +))))";

fn generated(source: &str, steps: &[(&str, (u32, u32))]) -> FunctionDocument {
    let mut doc = FunctionDocument::new("order total");
    doc.examples.push(Example::direct(
        "two paid",
        Edn::new("[{:amount 3 :paid true :qty 2} {:amount 4} {:amount 5 :paid true}]"),
        Edn::new("11"),
    ));
    doc.examples.push(Example::direct(
        "none paid",
        Edn::new("[{:amount 4}]"),
        Edn::new("0"),
    ));
    let mut mappings = Vec::new();
    for (text, (start, end)) in steps {
        let step = Step::new(*text);
        mappings.push(StepMapping {
            step: step.id,
            step_text: (*text).into(),
            excerpt: String::new(),
            lines: Some(LineRange {
                start: *start,
                end: *end,
            }),
            explanation: String::new(),
        });
        doc.steps.push(step);
    }
    doc.implementation = Some(Implementation {
        source: source.into(),
        generation: Some(GenerationRecord {
            generated_source: source.into(),
            model: ModelIdentity {
                display_name: "m".into(),
                repository: "r".into(),
                revision: "c".into(),
                file: "f.gguf".into(),
                sha256: "00".into(),
                quantization: "Q4_K_M".into(),
                architecture: "qwen2".into(),
            },
            runtime: InferenceRuntime {
                llama_cpp_release: "b1".into(),
                backend: "cpu".into(),
            },
            settings: GenerationSettings {
                temperature: 0.2,
                top_p: 0.9,
                seed: None,
                context_length: 8192,
                max_output_tokens: 1000,
            },
            documentation: Vec::new(),
            spec_basis: Fingerprint::from_stored("sha256:0"),
            attempts: 1,
            created_at: Timestamp(0),
            namespace: "order-total.core".into(),
            function_name: "order-total".into(),
            requires: Vec::new(),
            assumptions: Vec::new(),
            omitted_context: Vec::new(),
            explanation: Some(Explanation {
                summary: String::new(),
                steps: mappings,
                symbols: Vec::new(),
            }),
        }),
    });
    doc
}

#[test]
fn step_values_are_executed_bounded_and_labelled() {
    let Some(rt) = runtime() else { return };
    let doc = generated(
        TOTALS,
        &[
            ("keep paid orders", (4, 4)),
            ("multiply amount by quantity", (6, 6)),
            ("sum", (7, 7)),
            ("only the parameters", (3, 3)),
        ],
    );
    let plan = intermediate::plan(&doc).expect("plan");
    assert!(matches!(
        plan.instrumentation.steps[3].1,
        StepPlan::Unavailable(_)
    ));
    let outcome = run(&rt, &plan.request);
    let inspection = intermediate::inspect(&plan, Ok(&outcome));
    assert_eq!(inspection.problem, None, "{:?}", outcome.report);

    let first = inspection.example(doc.examples[0].id).expect("first example");
    assert_eq!(first.call, Ok(()));
    let Some(StepValue::Observed { evaluations, values }) = first.value(doc.steps[0].id) else {
        panic!("{:?}", first.value(doc.steps[0].id))
    };
    assert_eq!(*evaluations, 1);
    assert_eq!(
        values[0].text,
        "({:amount 3, :paid true, :qty 2} {:amount 5, :paid true})"
    );
    assert!(values[0].complete);
    let Some(StepValue::Observed { values, .. }) = first.value(doc.steps[1].id) else {
        panic!("{:?}", first.value(doc.steps[1].id))
    };
    assert_eq!(values[0].text, "(6 5)");
    let Some(StepValue::Observed { values, .. }) = first.value(doc.steps[2].id) else {
        panic!()
    };
    assert_eq!(values[0].text, "11");
    assert!(matches!(
        first.value(doc.steps[3].id),
        Some(StepValue::Unavailable(_))
    ));

    let none = inspection.example(doc.examples[1].id).expect("second example");
    let Some(StepValue::Observed { values, .. }) = none.value(doc.steps[0].id) else {
        panic!()
    };
    assert_eq!(values[0].text, "()");
}
