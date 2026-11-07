//! Exact output printing and instrumented runs in the real sandbox.
//!
//! Like the other sandbox tests, these need the pinned Java runtime and Clojure jars and the
//! sandbox helper; without them they print why and pass without running.

use std::time::Duration;

use clojurecraft_clojure::{
    Call, CallStatus, ClojureRuntime, EvalOutcome, EvalRequest, EvalStatus, Limits, ProbeStatus, ValueKind,
};
use clojurecraft_runtime::{DataDirs, Manifest, components, platform_key};

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
        work_dir: std::env::temp_dir().join(format!("clojurecraft-probe-test-{}", std::process::id())),
    })
}

fn calls(inputs: &[&str]) -> Vec<Call> {
    inputs
        .iter()
        .map(|i| Call {
            input: (*i).to_owned(),
            expected: None,
        })
        .collect()
}

fn run(rt: &ClojureRuntime, req: &EvalRequest) -> EvalOutcome {
    let limits = Limits {
        wall: Duration::from_secs(30),
        ..Limits::default()
    };
    let out = rt.evaluate(req, &limits, None).expect("evaluate");
    assert_eq!(
        out.status,
        EvalStatus::Completed,
        "{}\n{:?}",
        out.stderr,
        out.report
    );
    out
}

const PIPELINE: &str = "(ns p.core)
(defn totals [orders]
  (let [paid (filter :paid orders)]
    (->> paid
         (map :amount)
         (reduce +))))";

const PIPELINE_PROBED: &str = "(ns p.core)
(defn totals [orders]
  (let [paid (clojurecraft.harness/probe! 0 (filter :paid orders))]
    (->> paid
         (map :amount) (clojurecraft.harness/probe! 1)
         (reduce +))))";

#[test]
fn exact_output_is_printed_without_collection_bounds_or_reported_as_truncated() {
    let Some(rt) = runtime() else { return };
    let code = "(ns r.core)\n(defn f [n] (vec (range n)))";
    let mut req = EvalRequest::new(code, "f", calls(&["200000", "5"]));
    req.output_chars = Some(100_000);
    let out = run(&rt, &req);
    let big = out.report.result(0).expect("first");
    assert_eq!(big.status, CallStatus::Ok);
    assert!(big.truncated, "a print longer than the limit is reported as cut");
    assert_eq!(big.actual.as_deref().map(str::len), Some(100_000));
    let small = out.report.result(1).expect("second");
    assert!(!small.truncated);
    assert_eq!(small.actual.as_deref(), Some("[0 1 2 3 4]"));

    // Without the option, long collections are elided as before and never marked as cut.
    let req = EvalRequest::new(code, "f", calls(&["200000"]));
    let out = run(&rt, &req);
    let r = out.report.result(0).expect("result");
    assert!(!r.truncated);
    assert!(r.actual.as_deref().is_some_and(|a| a.ends_with(" …")));
}

#[test]
fn probes_record_bounded_intermediate_values_of_the_checked_code() {
    let Some(rt) = runtime() else { return };
    let many = format!(
        "[{}]",
        (0..100)
            .map(|i| format!("{{:amount {i} :paid true}}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut req = EvalRequest::new(
        PIPELINE,
        "totals",
        calls(&[
            "[{:amount 3 :paid true} {:amount 4} {:amount 5 :paid true}]",
            &many,
        ]),
    );
    req.probe_code = Some(PIPELINE_PROBED.into());
    let out = run(&rt, &req);
    assert_eq!(out.report.probe_setup, Some(Ok(())), "{:?}", out.report);
    assert_eq!(out.report.probes.len(), 2);

    let small = &out.report.probes[0];
    assert_eq!(small.status, ProbeStatus::Ok);
    assert!(small.consistent, "the instrumented run gives the same result");
    let paid = small.capture(0).expect("probe 0 ran");
    assert_eq!(paid.evaluations, 1);
    assert_eq!(paid.values[0].kind, ValueKind::Value);
    assert_eq!(
        paid.values[0].text,
        "({:amount 3, :paid true} {:amount 5, :paid true})"
    );
    assert!(paid.values[0].complete);
    let amounts = small.capture(1).expect("probe 1 ran");
    assert_eq!(amounts.values[0].text, "(3 5)");

    // A long intermediate value is shortened and marked as a preview.
    let large = &out.report.probes[1];
    let amounts = large.capture(1).expect("probe 1 ran");
    assert!(!amounts.values[0].complete);
    assert!(
        amounts.values[0].text.ends_with("...)"),
        "{}",
        amounts.values[0].text
    );
}

#[test]
fn probes_count_repeated_evaluations_and_report_functions_without_data() {
    let Some(rt) = runtime() else { return };
    let code = "(ns q.core)
(defn f [xs]
  (into [] (comp (map inc) (filter even?)) (map #(* 2 %) xs)))";
    let probed = "(ns q.core)
(defn f [xs]
  (into [] (clojurecraft.harness/probe! 0 (comp (map inc) (filter even?))) (map #(clojurecraft.harness/probe! 1 (* 2 %)) xs)))";
    let mut req = EvalRequest::new(code, "f", calls(&["[1 2 3 4 5]"]));
    req.probe_code = Some(probed.into());
    let out = run(&rt, &req);
    assert_eq!(out.report.probe_setup, Some(Ok(())), "{:?}", out.report);
    let call = &out.report.probes[0];
    assert!(call.consistent);
    let xf = call.capture(0).expect("probe 0");
    assert_eq!(xf.values[0].kind, ValueKind::Function);
    let doubled = call.capture(1).expect("probe 1");
    assert_eq!(doubled.evaluations, 5);
    assert_eq!(doubled.values.len(), 3, "only the first values are kept");
    assert_eq!(doubled.values[0].text, "2");
}

#[test]
fn instrumented_code_that_differs_from_the_checked_code_is_refused() {
    let Some(rt) = runtime() else { return };
    let tampered = PIPELINE_PROBED.replace("(reduce +)", "(reduce *)");
    let mut req = EvalRequest::new(PIPELINE, "totals", calls(&["[{:amount 3 :paid true}]"]));
    req.probe_code = Some(tampered);
    let out = run(&rt, &req);
    assert!(
        matches!(&out.report.probe_setup, Some(Err(m)) if m.contains("not the checked code")),
        "{:?}",
        out.report.probe_setup
    );
    assert!(out.report.probes.is_empty());
    // The ordinary result is still reported.
    assert_eq!(out.report.result(0).and_then(|r| r.actual.as_deref()), Some("3"));
}

#[test]
fn a_call_that_throws_reports_the_values_recorded_before_the_error() {
    let Some(rt) = runtime() else { return };
    let code = "(ns t.core)\n(defn f [xs]\n  (let [n (count xs)]\n    (/ 10 (- n 2))))";
    let probed = "(ns t.core)\n(defn f [xs]\n  (let [n (clojurecraft.harness/probe! 0 (count xs))]\n    (/ 10 (- n 2))))";
    let mut req = EvalRequest::new(code, "f", calls(&["[1 2]"]));
    req.probe_code = Some(probed.into());
    let out = run(&rt, &req);
    let call = &out.report.probes[0];
    assert_eq!(call.status, ProbeStatus::Error);
    assert!(call.consistent, "both runs throw the same exception");
    assert_eq!(call.capture(0).expect("probe 0").values[0].text, "2");
}
