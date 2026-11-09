//! Project functions end to end: generated as a fragment, checked inside their whole namespace
//! file in the real sandbox.
//!
//! Needs the pinned Java runtime and Clojure (`clojurecraft-devtool install-runtime`) and the
//! sandbox helper (built by `cargo test --workspace`); without them the tests print why and pass
//! without running.

use std::collections::HashMap;
use std::time::Duration;

use clojurecraft_clojure::{ClojureRuntime, EvalRequest, Limits};
use clojurecraft_domain::{
    Basis, CheckStatus, CheckSubject, Edn, Example, ExecutionEnvironment, FunctionDocument, FunctionId,
    InferenceRuntime, ModelIdentity, ProjectBinding,
};
use clojurecraft_inference::{
    Cancel, Finish, GenerateRequest, Generation, PromptPlan, RequestId, ServiceEvent, ToolCall, Usage,
};
use clojurecraft_runtime::{DataDirs, Manifest, components, platform_key};
use clojurecraft_workflow::Names;
use clojurecraft_workflow::assistant::{Assistant, Execution, Gather, ModelContext, Ports, Update};
use clojurecraft_workflow::validation::{self, ReportContext};
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
        work_dir: std::env::temp_dir().join(format!("clojurecraft-project-test-{}", std::process::id())),
        project_classpath: Vec::new(),
    })
}

fn environment() -> ExecutionEnvironment {
    ExecutionEnvironment {
        clojure_version: "1.12.6".into(),
        java_version: "25".into(),
        platform: platform_key(),
    }
}

struct RealPorts {
    rt: ClojureRuntime,
    sent: Vec<GenerateRequest>,
    executions: Vec<Execution>,
    requests: Vec<EvalRequest>,
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
        self.requests.push(request);
        self.executions.push(Execution {
            outcome,
            environment: environment(),
        });
        Ok(self.executions.len() as u64)
    }
    fn cancel_validation(&mut self, _: u64) {}
    fn runtime(&self) -> Option<ClojureRuntime> {
        Some(self.rt.clone())
    }
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

/// The rest of the namespace uses things generated code may not (atoms, interop, a multimethod);
/// it loads as it is.
const FILE: &str = "\
(ns shop.orders
  (:require [clojure.string :as str]))

(def rate 2)

(def seen (atom 0))

(defmulti kind :type)

(defn- shared [x] (swap! seen inc) x)

(defn other [x] (shared (.toUpperCase ^String x)))
";

fn doc() -> FunctionDocument {
    let mut d = FunctionDocument::new("price");
    d.description = "The order's amount times the rate, plus the fee.".into();
    d.examples = vec![
        Example::direct("", Edn::new("[{:amount 10} 3]"), Edn::new("23")),
        Example::direct("no amount", Edn::new("[{} 1]"), Edn::new("1")),
    ];
    d.project = Some(ProjectBinding {
        namespace: "shop.orders".into(),
        function: "price".into(),
        helpers: Vec::new(),
        arguments: 2,
        file: FILE.into(),
    });
    d
}

const RIGHT: &str = "(ns shop.orders\n  (:require [clojure.string :as str]))\n\n\
(defn- amount-of [order] (get order :amount 0))\n\n\
(defn price [order fee]\n  (+ (* rate (amount-of order)) fee))\n";

fn answer(
    a: &mut Assistant,
    p: &mut RealPorts,
    docs: &mut HashMap<FunctionId, FunctionDocument>,
    value: Value,
) -> Vec<Update> {
    let request = p.sent.len() as RequestId;
    let role = p.sent.last().expect("a request").role;
    let before = p.executions.len();
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
    let mut updates = a.on_model(
        &ServiceEvent::Finished {
            request,
            result: Ok(Box::new(generation)),
        },
        docs,
        p,
    );
    if p.executions.len() > before {
        let id = p.executions.len() as u64;
        let e = p.executions[id as usize - 1].clone();
        updates.extend(a.on_validated(id, e, docs, p));
    }
    updates
}

#[test]
fn a_generated_fragment_is_checked_in_its_whole_file_and_kept_as_a_fragment() {
    let Some(rt) = runtime() else { return };
    let d = doc();
    let id = d.id;
    let mut docs = HashMap::from([(id, d.clone())]);
    let mut p = RealPorts {
        rt,
        sent: Vec::new(),
        executions: Vec::new(),
        requests: Vec::new(),
    };
    let mut a = Assistant::default();
    a.generate(&d, model(), false, &mut p);
    let prompt: String = p.sent[0].sections.iter().map(|s| s.text.as_str()).collect();
    assert!(prompt.contains("(defn other [x])"), "{prompt}");
    let updates = answer(&mut a, &mut p, &mut docs, json!({ "code": RIGHT }));

    let request = p.requests.first().expect("validated");
    assert!(request.code.contains("(defmulti kind :type)") && request.code.contains("(defn price"));
    assert!(request.spread);
    let report = updates
        .iter()
        .find_map(|u| match u {
            Update::Validated(_, r) => Some(r.clone()),
            _ => None,
        })
        .expect("a report");
    assert!(report.passed(), "{:#?}", report.checks);
    let implementation = docs[&id].implementation.clone().expect("the code is used");
    assert_eq!(
        implementation.source.trim(),
        RIGHT.trim(),
        "the implementation is the fragment"
    );
}

#[test]
fn errors_are_reported_at_the_fragments_lines() {
    let Some(rt) = runtime() else { return };
    let d = doc();
    let fragment = "(ns shop.orders\n  (:require [clojure.string :as str]))\n\n\
                    (defn price [order fee]\n  (+ (* rate (missing-fn order)) fee))\n";
    let names = Names::for_document(&d, |_| false);
    let plan = validation::plan(&d, fragment, &names);
    let request = plan.request.clone().expect("runnable");
    let outcome = rt
        .evaluate(&request, &Limits::for_request(&request), None)
        .expect("ran");
    let report = validation::report(
        &plan,
        ReportContext {
            basis: d.basis_with_code(fragment),
            environment: environment(),
            attempt: None,
            run: None,
            outcome: Ok(&outcome),
        },
    );
    let lint = report
        .checks
        .iter()
        .find(|c| c.subject == CheckSubject::Lint)
        .expect("linted");
    assert_eq!(lint.status, CheckStatus::Failed, "{lint:?}");
    assert!(
        lint.diagnostics
            .iter()
            .any(|m| m.contains("line 5") && m.contains("missing-fn")),
        "{:?}",
        lint.diagnostics
    );
    let _: Basis = report.basis;
}

#[test]
fn the_models_eval_tool_runs_in_the_whole_namespace() {
    let Some(rt) = runtime() else { return };
    let d = doc();
    let tools = clojurecraft_workflow::eval_tool::EvalTool::set(rt, RIGHT, &d);
    let call = |expression: &str| {
        tools
            .execute(
                &ToolCall {
                    id: "1".into(),
                    name: "clojure_eval".into(),
                    arguments: json!({ "expression": expression }).to_string(),
                },
                &Cancel::default(),
            )
            .message()
    };
    assert_eq!(call("(price {:amount 10} 3)"), "(price {:amount 10} 3) => 23");
    assert_eq!(call("(other \"ab\")"), "(other \"ab\") => \"AB\"");
}
