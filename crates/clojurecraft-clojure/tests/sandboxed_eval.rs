//! End-to-end checks of the Clojure runtime inside the OS sandbox.
//!
//! These need the pinned Java runtime and Clojure jars (`clojurecraft-devtool install-runtime`)
//! and the sandbox helper (built by `cargo test --workspace`). When they are missing the tests
//! print why and pass without running, so the pure unit tests stay usable on any machine.
//! Automated environments should install the runtime first so that these tests actually run.

use std::path::PathBuf;
use std::time::Duration;

use clojurecraft_clojure::{
    Call, CallResult, CallStatus, ClojureRuntime, EvalOutcome, EvalRequest, EvalStatus, HarnessError,
    KeyRule, Limits, LintLevel, OutputRules, TypeRule,
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
    let work_dir = std::env::temp_dir().join(format!("clojurecraft-test-{}", std::process::id()));
    Some(ClojureRuntime {
        java,
        java_home,
        jars,
        sandbox_helper: helper,
        work_dir,
        project_classpath: Vec::new(),
    })
}

fn call(input: &str, expected: Option<&str>) -> Call {
    Call {
        input: input.into(),
        expected: expected.map(str::to_owned),
    }
}

fn run(rt: &ClojureRuntime, req: &EvalRequest, limits: Limits) -> EvalOutcome {
    rt.evaluate(req, &limits, None).expect("evaluate")
}

fn eval(rt: &ClojureRuntime, code: &str, checks: bool, limits: Limits) -> EvalOutcome {
    let mut req = EvalRequest::new(code, "f", vec![call("[1 2 3]", None)]);
    req.static_checks = checks;
    req.lint = checks;
    run(rt, &req, limits)
}

fn quick() -> Limits {
    Limits {
        wall: Duration::from_secs(15),
        ..Limits::default()
    }
}

/// The first call's result.
fn first(out: &EvalOutcome) -> &CallResult {
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    out.report
        .results
        .first()
        .unwrap_or_else(|| panic!("no result: {:?}", out.report))
}

/// The error of the first stage that failed, or of the first call.
fn failure(out: &EvalOutcome) -> HarnessError {
    let r = &out.report;
    for stage in [&r.read, &r.check, &r.load] {
        if let Some(Err(e)) = stage {
            return e.clone();
        }
    }
    first(out)
        .error
        .clone()
        .unwrap_or_else(|| panic!("no error: {r:?}"))
}

#[test]
fn pure_function_runs_and_returns_edn() {
    let Some(rt) = runtime() else { return };
    let out = eval(
        &rt,
        "(ns t.ok (:require [clojure.string :as str]))\n(defn f [xs] (str/join \",\" (map inc xs)))",
        true,
        quick(),
    );
    let r = first(&out);
    assert_eq!(r.status, CallStatus::Ok);
    assert_eq!(r.actual.as_deref(), Some(r#""2,3,4""#));
    assert_eq!(out.report.lint, Some(vec![]));
    assert!(out.report.done);
}

#[test]
fn static_checks_reject_dynamic_evaluation_and_interop() {
    let Some(rt) = runtime() else { return };
    for (code, needle) in [
        ("(ns t.e)\n(defn f [_] (eval '(+ 1 2)))", "eval is not allowed"),
        (
            "(ns t.r)\n(defn f [_] ((resolve 'clojure.core/eval) 1))",
            "resolve is not allowed",
        ),
        (
            "(ns t.v)\n(defn f [_] (#'clojure.core/eval 1))",
            "var is not allowed",
        ),
        (
            "(ns t.i)\n(defn f [_] (System/getenv \"HOME\"))",
            "System is not allowed",
        ),
        ("(ns t.c)\n(defn f [_] (java.io.File. \"/\"))", "not allowed"),
        ("(ns t.m)\n(defn f [s] (.toUpperCase s))", "interop"),
        (
            "(ns t.q (:require [clojure.java.shell :as sh]))\n(defn f [_] 1)",
            "requiring clojure.java.shell",
        ),
        (
            "(ns t.p)\n(defn f [_] ((get (ns-publics 'clojure.core) 'eval) 1))",
            "ns-publics is not allowed",
        ),
        ("(ns t.re)\n(defn f [_] #=(+ 1 2))", "EvalReader"),
    ] {
        let out = eval(&rt, code, true, quick());
        assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
        assert!(out.report.results.is_empty(), "{code}: nothing may run");
        let e = failure(&out);
        assert!(
            e.message.contains(needle) || e.class.contains(needle),
            "{code}: {e:?}"
        );
    }
}

/// With static checks disabled, the OS sandbox alone must stop each attempt.
#[test]
fn os_sandbox_blocks_side_effects_without_static_checks() {
    let Some(rt) = runtime() else { return };
    let marker = PathBuf::from("/tmp/clojurecraft-sandbox-escape");
    let _ = std::fs::remove_file(&marker);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    for (what, code) in [
        ("read /etc", "(ns t.a)\n(defn f [_] (slurp \"/etc/hostname\"))".to_owned()),
        // File.list returns nil on denial; Files/list reports the error.
        ("read home", format!("(ns t.b)\n(defn f [_] (vec (iterator-seq (.iterator (java.nio.file.Files/list (.toPath (java.io.File. \"{home}\")))))))")),
        ("write", format!("(ns t.c)\n(defn f [_] (spit \"{}\" \"x\"))", marker.display())),
        ("network", "(ns t.d)\n(defn f [_] (.connect (java.net.Socket.) (java.net.InetSocketAddress. \"127.0.0.1\" 22) 1000))".to_owned()),
        ("process", "(ns t.e)\n(defn f [_] (.waitFor (.exec (Runtime/getRuntime) (into-array String [\"/bin/true\"]))))".to_owned()),
    ] {
        let out = eval(&rt, &code, false, quick());
        let r = first(&out);
        assert_eq!(r.status, CallStatus::Error, "{what} succeeded: {r:?}");
        assert_eq!(r.error.as_ref().map(|e| e.phase.as_str()), Some("call"), "{what}");
    }
    assert!(!marker.exists(), "sandboxed code wrote a file");
}

#[test]
fn runaway_computation_is_stopped_by_the_call_limit() {
    let Some(rt) = runtime() else { return };
    let mut req = EvalRequest::new(
        "(ns t.l)\n(defn f [x] (if (= x 2) (loop [] (recur)) x))",
        "f",
        vec![call("1", Some("1")), call("2", Some("2")), call("3", Some("3"))],
    );
    req.call_timeout = Duration::from_secs(1);
    let out = run(&rt, &req, Limits::for_request(&req));
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    let r = &out.report;
    assert_eq!(r.results[0].matches, Some(true));
    assert_eq!(r.results[1].status, CallStatus::Timeout);
    assert!(r.halted && r.results.len() == 2, "later calls are not run: {r:?}");
}

#[test]
fn the_process_wall_limit_is_a_backstop() {
    let Some(rt) = runtime() else { return };
    let out = eval(
        &rt,
        "(ns t.l)\n(defn f [_] (loop [] (recur)))",
        true,
        Limits {
            wall: Duration::from_secs(4),
            ..Limits::default()
        },
    );
    assert_eq!(out.status, EvalStatus::TimedOut);
    assert_eq!(out.report.running, Some(0), "the running call is known");
}

#[test]
fn memory_exhaustion_is_reported_for_the_running_call() {
    let Some(rt) = runtime() else { return };
    let out = eval(
        &rt,
        "(ns t.m)\n(defn f [_] (vec (repeatedly 1e9 #(long-array 1024))))",
        true,
        Limits {
            heap_mb: 128,
            ..quick()
        },
    );
    assert_eq!(out.status, EvalStatus::MemoryLimitExceeded);
    assert_eq!(out.report.running, Some(0));
}

#[test]
fn printing_inside_a_function_is_bounded() {
    let Some(rt) = runtime() else { return };
    let out = eval(
        &rt,
        "(ns t.p)\n(defn f [_] (dotimes [_ 1000000] (println \"0123456789\")))",
        true,
        quick(),
    );
    assert!(
        failure(&out).message.contains("capture limit"),
        "{:?}",
        out.report
    );
}

#[test]
fn raw_stdout_flood_hits_the_process_output_cap() {
    let Some(rt) = runtime() else { return };
    let code = "(ns t.o)\n(defn f [_] (dotimes [_ 100000000] (.println System/out \"0123456789\")))";
    let out = eval(
        &rt,
        code,
        false,
        Limits {
            max_output_bytes: 100_000,
            ..quick()
        },
    );
    assert_eq!(out.status, EvalStatus::OutputLimitExceeded);
}

#[test]
fn stack_overflow_is_an_ordinary_call_error() {
    let Some(rt) = runtime() else { return };
    let out = eval(&rt, "(ns t.s)\n(defn f [x] (f x))", true, quick());
    assert!(
        failure(&out).class.contains("StackOverflowError"),
        "{:?}",
        out.report
    );
}

#[test]
fn explicit_refer_is_allowed_but_refer_all_is_not() {
    let Some(rt) = runtime() else { return };
    let ok = eval(
        &rt,
        "(ns t.r (:require [clojure.string :refer [join]]))\n(defn f [xs] (join \"-\" xs))",
        true,
        quick(),
    );
    assert_eq!(first(&ok).actual.as_deref(), Some(r#""1-2-3""#));
    let bad = eval(
        &rt,
        "(ns t.a (:require [clojure.string :refer :all]))\n(defn f [xs] xs)",
        true,
        quick(),
    );
    assert!(failure(&bad).message.contains(":refer :all is not allowed"));
}

/// Each case: expected EDN, the function's result as code, and whether they are equal.
#[test]
fn results_are_compared_structurally_under_the_documented_rules() {
    let Some(rt) = runtime() else { return };
    let cases = [
        ("[2 3 4]", "(map inc [1 2 3])", true),
        ("(1 2)", "[1 2]", true),
        ("{:a 1 :b [1 2]}", "{:b '(1 2) :a 1}", true),
        ("#{1 2}", "#{2 1}", true),
        ("1", "1N", true),
        ("1.5M", "1.50M", true),
        ("0.3", "(+ 0.1 0.2)", true),
        ("##NaN", "Double/NaN", true),
        ("1", "1.0", false),
        ("0.5", "1/2", false),
        ("[1 2]", "[2 1]", false),
        ("[1 2]", "#{1 2}", false),
        ("nil", "[]", false),
        ("{:a 1}", "{:a 1 :b 2}", false),
        ("{:a 1}", "(sorted-map 1 2)", false),
        ("0.3", "0.30001", false),
    ];
    let code = format!(
        "(ns t.eq)\n(defn f [i]\n  (case i\n{}))",
        cases
            .iter()
            .enumerate()
            .map(|(i, (_, v, _))| format!("    {i} {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let calls = cases
        .iter()
        .enumerate()
        .map(|(i, (e, _, _))| call(&i.to_string(), Some(e)))
        .collect();
    let mut req = EvalRequest::new(code, "f", calls);
    req.lint = false;
    let out = run(&rt, &req, Limits::for_request(&req));
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    for (i, (expected, value, equal)) in cases.iter().enumerate() {
        let r = out
            .report
            .result(i)
            .unwrap_or_else(|| panic!("{i}: {:?}", out.report));
        assert_eq!(r.status, CallStatus::Ok, "{value}: {r:?}");
        assert_eq!(r.matches, Some(*equal), "{expected} vs {value}: {r:?}");
        assert_eq!(r.difference.is_some(), !equal, "{expected} vs {value}");
    }
    let category = out
        .report
        .result(8)
        .and_then(|r| r.difference.clone())
        .unwrap_or_default();
    assert!(
        category.contains("an integer 1 but got a floating-point number 1.0"),
        "{category}"
    );
}

#[test]
fn unordered_output_ignores_order_but_not_multiplicity() {
    let Some(rt) = runtime() else { return };
    let mut req = EvalRequest::new(
        "(ns t.u)\n(defn f [xs] (reverse xs))",
        "f",
        vec![call("[1 2 3]", Some("[1 2 3]")), call("[1 2 2]", Some("[1 1 2]"))],
    );
    req.equality.unordered = true;
    let out = run(&rt, &req, Limits::for_request(&req));
    assert_eq!(out.report.results[0].matches, Some(true));
    assert_eq!(out.report.results[1].matches, Some(false));
}

#[test]
fn an_infinite_lazy_result_is_reported_as_too_large() {
    let Some(rt) = runtime() else { return };
    let mut req = EvalRequest::new(
        "(ns t.z)\n(defn f [x] (if x (range) (map #(/ 1 %) [1 0])))",
        "f",
        vec![call("true", Some("[0 1]")), call("false", Some("[1]"))],
    );
    req.max_elements = 10_000;
    let out = run(&rt, &req, Limits::for_request(&req));
    let r = &out.report;
    assert_eq!(r.results[0].status, CallStatus::TooLarge, "{r:?}");
    // An error hidden in a lazy result belongs to the call that produced it.
    assert_eq!(r.results[1].status, CallStatus::Error);
    assert!(
        r.results[1]
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("Divide by zero"))
    );
}

#[test]
fn call_errors_point_at_the_line_and_the_core_function() {
    let Some(rt) = runtime() else { return };
    let code =
        "(ns t.npe)\n\n(defn f [m]\n  (update-vals m\n    (fn [v] (if (string? v) (subs v 9) (inc v)))))";
    let req = EvalRequest::new(
        code,
        "f",
        vec![call("{:a \"x\"}", Some("{}")), call("{:a nil}", Some("{}"))],
    );
    let out = run(&rt, &req, Limits::for_request(&req));
    let errors: Vec<HarnessError> = out
        .report
        .results
        .iter()
        .map(|r| r.error.clone().unwrap_or_else(|| panic!("{r:?}")))
        .collect();
    assert!(errors.iter().all(|e| e.line == Some(5)), "{errors:?}");
    assert_eq!(
        errors[0].within.as_deref(),
        Some("clojure.core/subs"),
        "{errors:?}"
    );
    // inc is inlined, so the runtime method is named instead.
    assert!(errors[1].class.contains("NullPointerException"), "{errors:?}");
    assert_eq!(
        errors[1].within.as_deref(),
        Some("clojure.lang.Numbers.inc"),
        "{errors:?}"
    );
}

#[test]
fn lint_reports_errors_and_warnings_with_lines_before_loading() {
    let Some(rt) = runtime() else { return };
    let code = "(ns t.lint)\n(defn- unused-helper [x] x)\n(defn f [xs]\n  (let [tmp 1]\n    (assoc xs)\n    (undefined-fn xs)))";
    let req = EvalRequest::new(code, "f", vec![call("{}", Some("{}"))]);
    let out = run(&rt, &req, Limits::for_request(&req));
    let lint = out.report.lint.clone().unwrap_or_default();
    let has = |level, needle: &str| {
        lint.iter()
            .any(|f| f.level == level && f.message.contains(needle) && f.line.is_some())
    };
    assert!(
        has(LintLevel::Error, "assoc is called with 1 argument"),
        "{lint:?}"
    );
    assert!(
        has(LintLevel::Error, "Unable to resolve symbol: undefined-fn"),
        "{lint:?}"
    );
    assert!(has(LintLevel::Warning, "tmp is bound but never used"), "{lint:?}");
    assert!(
        has(LintLevel::Warning, "private function unused-helper"),
        "{lint:?}"
    );
    // Loading then fails as the compiler would, with the line.
    let Some(Err(load)) = &out.report.load else {
        panic!("{:?}", out.report)
    };
    assert_eq!(load.line, Some(6));
}

#[test]
fn a_value_thrown_away_by_a_misplaced_parenthesis_is_a_lint_error() {
    let Some(rt) = runtime() else { return };
    let code = "(ns t.dropped)\n(defn f [s]\n  (let [[a b] (seq s)]\n    (if (and a b)\n      {:a a :b b})\n    nil))";
    let req = EvalRequest::new(code, "f", vec![call("\"xy\"", Some("{:a \\x :b \\y}"))]);
    let out = run(&rt, &req, Limits::for_request(&req));
    let lint = out.report.lint.clone().unwrap_or_default();
    assert!(
        lint.iter().any(|f| f.level == LintLevel::Error
            && f.line == Some(4)
            && f.message
                .starts_with("the value of (if (and a b) {:a a, :b b}) is thrown away")),
        "{lint:?}"
    );
}

#[test]
fn idiomatic_code_lints_clean() {
    let Some(rt) = runtime() else { return };
    let code = r#"(ns t.clean (:require [clojure.string :as str] [clojure.set :as set]))
(defn- total [orders] (reduce + 0 (map :amount orders)))
(defn f [{:keys [orders]}]
  (let [by-customer (group-by :customer orders)
        rows (->> by-customer
                  (map (fn [[customer os]] {:customer customer :total (total os)}))
                  (sort-by (juxt (comp - :total) :customer))
                  (into [] (comp (filter #(pos? (:total %)))
                                 (map #(update % :customer str/upper-case)))))]
    (as-> rows rs
      (cond-> rs (empty? orders) (conj {:none true}))
      (if-let [[r & more] (seq rs)] (vec (cons r more)) rs)
      (if (set/subset? #{} (set rs)) rs [])
      (vec (for [x rs :let [y x] :when y] y)))))"#;
    let req = EvalRequest::new(
        code,
        "f",
        vec![call(
            r#"{:orders [{:customer "a" :amount 1} {:customer "b" :amount 2}]}"#,
            Some(r#"[{:customer "B" :total 2} {:customer "A" :total 1}]"#),
        )],
    );
    let out = run(&rt, &req, Limits::for_request(&req));
    assert_eq!(out.report.lint, Some(vec![]), "{:?}", out.report.lint);
    assert_eq!(first(&out).matches, Some(true), "{:?}", out.report);
}

#[test]
fn output_contracts_are_checked_on_every_result() {
    let Some(rt) = runtime() else { return };
    let mut req = EvalRequest::new(
        "(ns t.c)\n(defn f [x] x)",
        "f",
        vec![
            call("[{:id 1 :total 2}]", None),
            call("[{:id 1 :total 2.5} {:total nil}]", None),
            call("{:id 1}", None),
        ],
    );
    req.output_rules = Some(OutputRules {
        value_type: Some(TypeRule::Vector),
        keys: vec![
            KeyRule {
                key: ":id".into(),
                required: true,
                value_type: Some(TypeRule::Integer),
                nil_allowed: false,
            },
            KeyRule {
                key: ":total".into(),
                required: false,
                value_type: Some(TypeRule::Integer),
                nil_allowed: false,
            },
        ],
    });
    let out = run(&rt, &req, Limits::for_request(&req));
    let r = &out.report.results;
    assert!(r[0].contract.is_empty(), "{:?}", r[0]);
    assert_eq!(
        r[1].contract,
        [
            "element 0: key :total must be an integer but is a floating-point number",
            "element 1: required key :id is missing",
            "element 1: key :total must not be nil"
        ]
    );
    assert_eq!(r[2].contract, ["the output must be a vector but is a map"]);
}

#[test]
fn cancelling_stops_the_jvm() {
    let Some(rt) = runtime() else { return };
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let setter = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        setter.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let req = EvalRequest::new(
        "(ns t.k)\n(defn f [_] (loop [] (recur)))",
        "f",
        vec![call("1", None)],
    );
    let started = std::time::Instant::now();
    let out = rt
        .evaluate(&req, &Limits::for_request(&req), Some(flag))
        .expect("evaluate");
    assert_eq!(out.status, EvalStatus::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn api_documentation_comes_from_the_installed_clojure() {
    let Some(rt) = runtime() else { return };
    let limits = Limits {
        wall: Duration::from_secs(60),
        max_output_bytes: 16 << 20,
        ..Limits::default()
    };
    let out = rt
        .run_script("api-docs", clojurecraft_clojure::API_DOCS_SOURCE, b"", &limits)
        .expect("run");
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    let doc: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("JSON");
    let manifest = Manifest::bundled().expect("manifest");
    assert_eq!(doc["clojure_version"], manifest.clojure.version.as_str());
    let entries = doc["entries"].as_array().expect("entries");
    let find = |ns: &str, name: &str| {
        entries
            .iter()
            .find(|e| e["ns"] == ns && e["name"] == name)
            .unwrap_or_else(|| panic!("{ns}/{name} missing"))
    };
    let group_by = find("clojure.core", "group-by");
    assert_eq!(group_by["kind"], "function");
    assert_eq!(group_by["added"], "1.2");
    assert_eq!(group_by["arglists"][0], "[f coll]");
    assert!(group_by["doc"].as_str().unwrap_or_default().contains("keyed by"));
    assert_eq!(find("clojure.core", "when-let")["kind"], "macro");
    assert_eq!(find("clojure.core", "if")["kind"], "special-form");
    find("clojure.string", "join");
    find("clojure.set", "rename-keys");
    // Private vars are not documentation.
    assert!(
        !entries
            .iter()
            .any(|e| e["ns"] == "clojure.core" && e["name"] == "spread")
    );
}

/// A project with `shop/util.clj` on its classpath.
fn project_runtime(rt: &ClojureRuntime) -> (ClojureRuntime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("shop")).expect("dirs");
    std::fs::write(
        src.join("shop/util.clj"),
        "(ns shop.util)\n(defn clean [s] (.trim ^String s))\n(def registry (atom {}))\n",
    )
    .expect("write");
    let mut rt = rt.clone();
    rt.project_classpath = vec![src.canonicalize().expect("canonical")];
    (rt, dir)
}

const PROJECT_FILE: &str = "\
(ns shop.orders
  (:require [shop.util :as util]
            [clojure.string :as str])
  (:import (java.util UUID)))

(def counter (atom 0))

(defn- log! [x] (swap! counter inc) (println \"seen\" x) x)

(defn- discount-of [order] (get order :discount 0))

(defn price [order tax]
  (-> (:amount order) (- (discount-of order)) (* (+ 1 tax))))

(defn label [order]
  (log! (str/upper-case (util/clean (:name order)))))
";

#[test]
fn a_project_file_loads_with_its_classpath_and_checks_only_the_function() {
    let Some(rt) = runtime() else { return };
    let (rt, _dir) = project_runtime(&rt);
    let mut req = EvalRequest::new(
        PROJECT_FILE,
        "price",
        vec![call("[{:amount 100 :discount 10} 0.5]", Some("135.0"))],
    );
    req.spread = true;
    req.project = Some(clojurecraft_clojure::ProjectEval {
        checked: vec!["discount-of".into(), "price".into()],
        namespaces: vec!["shop.util".into(), "clojure.string".into()],
    });
    let out = run(&rt, &req, Limits::default());
    assert_eq!(out.report.check, Some(Ok(())), "{out:?}");
    assert_eq!(out.report.load, Some(Ok(())), "{out:?}");
    assert_eq!(out.report.lint, Some(Vec::new()), "{out:?}");
    assert_eq!(out.report.results[0].matches, Some(true), "{out:?}");

    // A function using another project namespace through its alias.
    req.function = "label".into();
    req.spread = false;
    req.calls = vec![call("{:name \"  ann \"}", Some("\"ANN\""))];
    req.project = Some(clojurecraft_clojure::ProjectEval {
        checked: vec!["label".into()],
        namespaces: vec!["shop.util".into(), "clojure.string".into()],
    });
    let out = run(&rt, &req, Limits::default());
    assert_eq!(out.report.load, Some(Ok(())), "{out:?}");
    assert_eq!(out.report.results[0].matches, Some(true), "{out:?}");
}

#[test]
fn checked_project_definitions_stay_pure() {
    let Some(rt) = runtime() else { return };
    let (rt, _dir) = project_runtime(&rt);
    let code = format!("{PROJECT_FILE}\n(defn sneaky [path] (slurp path))\n");
    let mut req = EvalRequest::new(code, "sneaky", vec![call("\"/etc/hosts\"", None)]);
    req.project = Some(clojurecraft_clojure::ProjectEval {
        checked: vec!["sneaky".into()],
        namespaces: vec!["shop.util".into(), "clojure.string".into()],
    });
    let out = run(&rt, &req, Limits::default());
    let Some(Err(e)) = &out.report.check else {
        panic!("{out:?}")
    };
    assert!(e.message.contains("slurp"), "{e:?}");
}

#[test]
fn project_lint_reports_only_the_checked_definitions() {
    let Some(rt) = runtime() else { return };
    let (rt, _dir) = project_runtime(&rt);
    let code = format!("{PROJECT_FILE}\n(defn broken [x] (let [unused 1] x))\n");
    let mut req = EvalRequest::new(code, "broken", vec![call("1", Some("1"))]);
    req.project = Some(clojurecraft_clojure::ProjectEval {
        checked: vec!["broken".into()],
        namespaces: vec!["shop.util".into(), "clojure.string".into()],
    });
    let out = run(&rt, &req, Limits::default());
    let findings = out.report.lint.clone().expect("linted");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].message.contains("unused"), "{findings:?}");
    assert_eq!(findings[0].line, Some(18));
}
