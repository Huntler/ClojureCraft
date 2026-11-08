//! The generation and repair loop with scripted model answers and real sandboxed execution.
//!
//! Needs the pinned Java runtime and Clojure (`clojurecraft-devtool install-runtime`) and the
//! sandbox helper (built by `cargo test --workspace`); without them the tests print why and pass
//! without running.

use std::collections::HashMap;
use std::time::Duration;

use clojurecraft_clojure::{ClojureRuntime, EvalRequest, Limits};
use clojurecraft_domain::{
    CheckStatus, CheckSubject, Edn, Example, ExecutionEnvironment, FunctionDocument, FunctionId,
    InferenceRuntime, ModelIdentity, Step,
};
use clojurecraft_inference::{
    Finish, GenerateRequest, Generation, PromptPlan, RequestId, Role, ServiceEvent, Usage,
};
use clojurecraft_runtime::{DataDirs, Manifest, components, platform_key};
use clojurecraft_workflow::assistant::{
    Assistant, Execution, Gather, MAX_ATTEMPTS, ModelContext, Ports, Report, SAMPLES, Update,
};
use serde_json::{Value, json};

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
        work_dir: std::env::temp_dir().join(format!("clojurecraft-workflow-test-{}", std::process::id())),
    })
}

/// Model requests are recorded; validation runs synchronously in the real sandbox.
struct RealPorts {
    rt: ClojureRuntime,
    sent: Vec<GenerateRequest>,
    executions: Vec<Execution>,
}

impl Ports for RealPorts {
    fn generate(&mut self, request: GenerateRequest) -> Result<RequestId, String> {
        self.sent.push(request);
        Ok(self.sent.len() as RequestId)
    }
    fn cancel(&mut self, _: RequestId) {}
    fn gather(&mut self, _: Gather) -> Option<u64> {
        None
    }
    fn validate(&mut self, request: EvalRequest) -> Result<u64, String> {
        let outcome = self
            .rt
            .evaluate(&request, &Limits::for_request(&request), None)
            .map_err(|e| e.to_string());
        self.executions.push(Execution {
            outcome,
            environment: ExecutionEnvironment {
                clojure_version: "1.12.6".into(),
                java_version: "25".into(),
                platform: platform_key(),
            },
        });
        Ok(self.executions.len() as u64)
    }
    fn cancel_validation(&mut self, _: u64) {}
}

fn model() -> ModelContext {
    ModelContext {
        label: "scripted".into(),
        model: ModelIdentity {
            display_name: "scripted".into(),
            repository: "test/scripted".into(),
            revision: "0".into(),
            file: "m.gguf".into(),
            sha256: "00".into(),
            quantization: "Q4_K_M".into(),
            architecture: "qwen2".into(),
        },
        runtime: InferenceRuntime {
            llama_cpp_release: "b0".into(),
            backend: "cpu".into(),
        },
        context_length: 8192,
        clojure_version: "1.12.6".into(),
    }
}

fn doc() -> FunctionDocument {
    let mut d = FunctionDocument::new("Order totals");
    d.description = "Total amount per customer".into();
    d.steps = vec![
        Step::new("group orders by :customer"),
        Step::new("sum :amount per group"),
    ];
    d.examples = vec![
        Example::direct(
            "",
            Edn::new("[{:customer \"a\" :amount 2} {:customer \"b\" :amount 1} {:customer \"a\" :amount 3}]"),
            Edn::new("{\"a\" 5 \"b\" 1}"),
        ),
        Example::direct("empty", Edn::new("[]"), Edn::new("{}")),
    ];
    d
}

struct Harness {
    a: Assistant,
    p: RealPorts,
    docs: HashMap<FunctionId, FunctionDocument>,
    id: FunctionId,
    stored: Vec<clojurecraft_domain::ValidationReport>,
}

impl Harness {
    fn new(rt: ClojureRuntime) -> Self {
        let d = doc();
        let id = d.id;
        Self {
            a: Assistant::default(),
            p: RealPorts {
                rt,
                sent: Vec::new(),
                executions: Vec::new(),
            },
            docs: HashMap::from([(id, d)]),
            id,
            stored: Vec::new(),
        }
    }

    /// Answers the last request with `value`, then delivers any validation it started.
    fn answer(&mut self, value: Value) {
        let request = self.p.sent.len() as RequestId;
        let role = self.p.sent.last().map_or(Role::Generation, |r| r.role);
        let before = self.p.executions.len();
        let generation = Generation {
            role,
            model: "scripted".into(),
            text: value.to_string(),
            reasoning: String::new(),
            value: Some(value),
            finish: Finish::Stop,
            usage: Usage::default(),
            plan: PromptPlan {
                included: vec![],
                omitted: vec![],
                prompt_tokens: 1,
                reserved_output: 1,
                context: 8192,
                exact: true,
            },
            tool_calls: vec![],
            tools_unavailable: false,
            elapsed: Duration::from_millis(1),
        };
        let updates = self.a.on_model(
            &ServiceEvent::Finished {
                request,
                result: Ok(Box::new(generation)),
            },
            &mut self.docs,
            &mut self.p,
        );
        self.keep(updates);
        if self.p.executions.len() > before {
            let id = self.p.executions.len() as u64;
            let e = self.p.executions[id as usize - 1].clone();
            let updates = self.a.on_validated(id, e, &mut self.docs, &mut self.p);
            self.keep(updates);
        }
    }

    fn keep(&mut self, updates: Vec<Update>) {
        for u in updates {
            if let Update::Validated(_, r) = u {
                self.stored.push(*r);
            }
        }
    }

    fn last_prompt(&self) -> String {
        self.p
            .sent
            .last()
            .expect("a request was sent")
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

const WRONG: &str = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders) count))\n";
const RIGHT: &str = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders)\n               (fn [os] (reduce + (map :amount os)))))\n";

fn explanation() -> Value {
    json!({"summary": "Groups then sums.", "steps": [], "symbols": []})
}

#[test]
fn a_wrong_candidate_is_repaired_from_real_diagnostics() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    h.a.generate(&d, model(), false, &mut h.p);
    for _ in 0..SAMPLES {
        h.answer(json!({"code": WRONG, "assumptions": []}));
    }

    let first = &h.stored[0];
    assert!(!first.passed());
    let example = first
        .checks
        .iter()
        .find(|c| matches!(c.subject, CheckSubject::Example(_)))
        .unwrap();
    assert_eq!(example.status, CheckStatus::Failed);
    assert_eq!(
        example.actual.as_ref().map(Edn::as_str),
        Some("{\"a\" 2, \"b\" 1}")
    );
    // The repair request holds the real values the harness reported.
    let prompt = h.last_prompt();
    let repair = SAMPLES + 1;
    assert!(
        prompt.contains(&format!("attempt {repair} of {MAX_ATTEMPTS}")),
        "{prompt}"
    );
    assert!(prompt.contains("Actual: {\"a\" 2, \"b\" 1}"), "{prompt}");
    assert!(prompt.contains("at [\"a\"]: expected 5 but got 2"), "{prompt}");

    h.answer(json!({"code": RIGHT, "assumptions": []}));
    let last = h.stored.last().unwrap();
    assert!(last.passed(), "{:?}", last.checks);
    assert_eq!(h.p.sent.last().unwrap().role, Role::Explanation);
    h.answer(explanation());

    let Some(Report::Generated { attempts, .. }) = h.a.report(h.id) else {
        panic!("{:?}", h.a.report(h.id))
    };
    assert_eq!(attempts.len(), repair as usize);
    let i = h.docs[&h.id].implementation.as_ref().unwrap();
    assert_eq!(i.source, RIGHT);
    assert!(
        h.stored
            .last()
            .unwrap()
            .freshness(&h.docs[&h.id].basis())
            .is_current()
    );
}

#[test]
fn lint_and_load_failures_reach_the_repair_prompt() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    h.a.generate(&d, model(), false, &mut h.p);
    let code = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (sum-by :amount orders))\n";
    for _ in 0..SAMPLES {
        h.answer(json!({"code": code, "assumptions": []}));
    }
    let r = &h.stored[0];
    let status = |s: CheckSubject| r.checks.iter().find(|c| c.subject == s).map(|c| c.status);
    assert_eq!(status(CheckSubject::Lint), Some(CheckStatus::Failed));
    assert_eq!(status(CheckSubject::Load), Some(CheckStatus::Error));
    let prompt = h.last_prompt();
    assert!(
        prompt.contains("line 4: Unable to resolve symbol: sum-by"),
        "{prompt}"
    );
}

#[test]
fn every_attempt_failing_ends_unresolved_with_every_report_stored() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    h.a.generate(&d, model(), false, &mut h.p);
    for _ in 0..MAX_ATTEMPTS {
        h.answer(json!({"code": WRONG, "assumptions": []}));
    }
    assert_eq!(h.p.sent.len() as u32, MAX_ATTEMPTS);
    assert_eq!(h.stored.len() as u32, MAX_ATTEMPTS);
    assert!(h.a.job().is_none());
    assert!(matches!(h.a.report(h.id), Some(Report::Unresolved { .. })));
    assert!(h.docs[&h.id].implementation.is_none());
}

#[test]
fn an_exception_shows_the_locals_of_the_running_code() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    // `:amt` is missing, so `+` meets nil inside `amount-of`.
    let broken = "(ns order-totals.core)\n\n(defn- amount-of [order]\n  (+ 0 (:amt order)))\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders)\n               (fn [os] (reduce + (map amount-of os)))))\n";
    h.a.generate(&d, model(), false, &mut h.p);
    h.answer(json!({"code": broken, "assumptions": []}));

    let example = h.stored[0]
        .checks
        .iter()
        .find(|c| matches!(c.subject, CheckSubject::Example(_)))
        .unwrap();
    assert_eq!(example.status, CheckStatus::Error);
    let diagnostics = example.diagnostics.join("\n");
    for needle in [
        "Values of the locals in the running code when it threw",
        "in (defn- amount-of [order]) at line 3: order = {:customer \"a\", :amount 2}",
        "in (fn [os]) at line 8: os = [{:customer \"a\", :amount 2} {:customer \"a\", :amount 3}]",
    ] {
        assert!(
            diagnostics.contains(needle),
            "missing {needle:?} in\n{diagnostics}"
        );
    }
    // The empty example did not throw, so it was not run again.
    assert!(h.p.executions[0].outcome.as_ref().unwrap().report.probes.len() == 1);

    // Repairs see the values.
    for _ in 1..SAMPLES {
        h.answer(json!({"code": broken, "assumptions": []}));
    }
    assert!(
        h.last_prompt().contains("order = {:customer \"a\", :amount 2}"),
        "{}",
        h.last_prompt()
    );
}

#[test]
fn a_wrong_value_shows_each_steps_value_and_the_locals_entered() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    let wrong = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (let [groups (group-by :customer orders)]\n    (update-vals groups count)))\n";
    let answer = json!({
        "code": wrong,
        "steps": [{"step": 1, "code": "(group-by :customer orders)"},
                  {"step": 2, "code": "(update-vals groups count)"}],
        "assumptions": []
    });
    h.a.generate(&d, model(), false, &mut h.p);
    h.answer(answer.clone());
    let example = h.stored[0]
        .checks
        .iter()
        .find(|c| matches!(c.subject, CheckSubject::Example(_)))
        .unwrap();
    assert_eq!(example.status, CheckStatus::Failed);
    let diagnostics = example.diagnostics.join("\n");
    for needle in [
        "Values after each transformation step in this call",
        "step 1 \"group orders by :customer\": value of `(group-by :customer orders)` (line 4) = {\"a\" [{:customer \"a\", :amount 2} {:customer \"a\", :amount 3}], \"b\" [{:customer \"b\", :amount 1}]}",
        "step 2 \"sum :amount per group\": value of `(update-vals groups count)` (line 5) = {\"a\" 2, \"b\" 1}",
        "Values of the locals in each scope the call entered, in source order:",
        "in (defn order-totals [orders]) at line 3 (entered once): orders = [",
        "in let at line 4 (entered once): groups = {\"a\"",
    ] {
        assert!(
            diagnostics.contains(needle),
            "missing {needle:?} in\n{diagnostics}"
        );
    }
    // The passing example was not run again.
    assert_eq!(h.p.executions[0].outcome.as_ref().unwrap().report.probes.len(), 1);
    // A repair carries the values.
    for _ in 1..SAMPLES {
        h.answer(answer.clone());
    }
    assert!(
        h.last_prompt().contains("step 2 \"sum :amount per group\""),
        "{}",
        h.last_prompt()
    );
}
