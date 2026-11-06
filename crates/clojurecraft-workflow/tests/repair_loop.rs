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
use clojurecraft_workflow::assistant::{Assistant, Execution, Gather, ModelContext, Ports, Report, Update};
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
    h.answer(json!({"code": WRONG, "assumptions": []}));

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
    assert!(prompt.contains("attempt 2 of 4"), "{prompt}");
    assert!(prompt.contains("Actual: {\"a\" 2, \"b\" 1}"), "{prompt}");
    assert!(prompt.contains("at [\"a\"]: expected 5 but got 2"), "{prompt}");

    h.answer(json!({"code": RIGHT, "assumptions": []}));
    assert!(h.stored[1].passed(), "{:?}", h.stored[1].checks);
    assert_eq!(h.p.sent.last().unwrap().role, Role::Explanation);
    h.answer(explanation());

    let Some(Report::Generated { attempts, .. }) = h.a.report(h.id) else {
        panic!("{:?}", h.a.report(h.id))
    };
    assert_eq!(attempts.len(), 2);
    let i = h.docs[&h.id].implementation.as_ref().unwrap();
    assert_eq!(i.source, RIGHT);
    assert!(h.stored[1].freshness(&h.docs[&h.id].basis()).is_current());
}

#[test]
fn lint_and_load_failures_reach_the_repair_prompt() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    h.a.generate(&d, model(), false, &mut h.p);
    h.answer(json!({"code": "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (sum-by :amount orders))\n", "assumptions": []}));
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
fn every_attempt_failing_ends_unresolved_with_four_stored_reports() {
    let Some(rt) = runtime() else { return };
    let mut h = Harness::new(rt);
    let d = h.docs[&h.id].clone();
    h.a.generate(&d, model(), false, &mut h.p);
    for _ in 0..4 {
        h.answer(json!({"code": WRONG, "assumptions": []}));
    }
    assert_eq!(h.p.sent.len(), 4);
    assert_eq!(h.stored.len(), 4);
    assert!(h.a.job().is_none());
    assert!(matches!(h.a.report(h.id), Some(Report::Unresolved { .. })));
    assert!(h.docs[&h.id].implementation.is_none());
}
