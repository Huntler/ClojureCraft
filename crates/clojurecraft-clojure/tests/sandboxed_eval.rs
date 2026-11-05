//! End-to-end checks of the Clojure runtime inside the OS sandbox.
//!
//! These need the pinned Java runtime and Clojure jars (`clojurecraft-devtool install-runtime`)
//! and the sandbox helper (built by `cargo test --workspace`). When they are missing the tests
//! print why and pass without running, so the pure unit tests stay usable on any machine.
//! Automated environments should install the runtime first so that these tests actually run.

use std::path::PathBuf;
use std::time::Duration;

use clojurecraft_clojure::{ClojureRuntime, EvalOutcome, EvalRequest, EvalStatus, Limits};
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
    })
}

fn eval(rt: &ClojureRuntime, code: &str, checks: bool, limits: Limits) -> EvalOutcome {
    rt.evaluate(
        &EvalRequest {
            code: code.into(),
            function: "f".into(),
            inputs: vec!["[1 2 3]".into()],
            static_checks: checks,
        },
        &limits,
    )
    .expect("evaluate")
}

fn quick() -> Limits {
    Limits {
        wall: Duration::from_secs(15),
        ..Limits::default()
    }
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
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    assert!(out.response.contains(r#":value "\"2,3,4\"""#), "{}", out.response);
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
        assert!(
            out.response.contains(":status :error"),
            "{code}: {}",
            out.response
        );
        assert!(out.response.contains(needle), "{code}: {}", out.response);
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
        assert_eq!(out.status, EvalStatus::Completed, "{what}: {}", out.stderr);
        assert!(out.response.contains(":phase :call"), "{what}: {}", out.response);
        assert!(!out.response.contains(":status :ok, :value"), "{what} succeeded: {}", out.response);
    }
    assert!(!marker.exists(), "sandboxed code wrote a file");
}

#[test]
fn runaway_computation_times_out() {
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
}

#[test]
fn memory_exhaustion_is_reported() {
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
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    assert!(out.response.contains("capture limit"), "{}", out.response);
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
    assert_eq!(out.status, EvalStatus::Completed, "{}", out.stderr);
    assert!(out.response.contains("StackOverflowError"), "{}", out.response);
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
    assert!(ok.response.contains(r#":value "\"1-2-3\"""#), "{}", ok.response);
    let bad = eval(
        &rt,
        "(ns t.a (:require [clojure.string :refer :all]))\n(defn f [xs] xs)",
        true,
        quick(),
    );
    assert!(
        bad.response.contains(":refer :all is not allowed"),
        "{}",
        bad.response
    );
}
