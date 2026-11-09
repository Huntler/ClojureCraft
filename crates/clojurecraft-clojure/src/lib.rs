//! Runs candidate Clojure functions on example inputs inside the OS sandbox.
//!
//! A fresh JVM is started for every evaluation, restricted by [`clojurecraft_sandbox`]: it can read
//! only the Java runtime, the Clojure jars, the harness script, and system libraries, and it
//! cannot write files, open sockets, or start processes. The harness additionally rejects code
//! that uses dynamic evaluation, namespace reflection, Java interop outside a few numeric
//! classes, or namespaces outside the supported standard library.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clojurecraft_sandbox::{Outcome, Policy, RunRequest, Termination, run};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub mod policy;
pub mod probe;

pub use policy::{HarnessPolicy, harness_policy, is_class_name};
pub use probe::{LocalValue, ProbeCall, ProbeCapture, ProbeStatus, ProbeValue, ValueKind};

pub const HARNESS_SOURCE: &str = include_str!("../../../resources/clojure/harness.clj");

/// Prints the documentation of the supported standard-library namespaces as JSON.
pub const API_DOCS_SOURCE: &str = include_str!("../../../resources/clojure/api-docs.clj");

/// The JVM's exit status for `-XX:+ExitOnOutOfMemoryError`.
const JVM_OOM_EXIT: i32 = 3;

#[derive(Clone, Debug)]
pub struct ClojureRuntime {
    /// The `java` executable.
    pub java: PathBuf,
    /// The Java runtime's installation root (granted read and execute access).
    pub java_home: PathBuf,
    /// Clojure and its dependency jars.
    pub jars: Vec<PathBuf>,
    /// The `clojurecraft-sandbox` helper executable.
    pub sandbox_helper: PathBuf,
    /// Directory for the materialized harness script.
    pub work_dir: PathBuf,
    /// The open project's classpath (source directories and dependency jars), readable and on
    /// the classpath only for requests in project mode.
    pub project_classpath: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub wall: Duration,
    pub cpu_seconds: u64,
    pub heap_mb: u64,
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall: Duration::from_secs(20),
            cpu_seconds: 30,
            heap_mb: 512,
            max_output_bytes: 1 << 20,
        }
    }
}

/// Time for starting the JVM, reading, checking, linting, and loading the code.
const STARTUP_ALLOWANCE: Duration = Duration::from_secs(20);

impl Limits {
    /// Limits for a request: start-up plus every call's own time limit, so the process limit is
    /// only a backstop behind the per-call limits enforced inside the JVM.
    pub fn for_request(req: &EvalRequest) -> Self {
        // An instrumented copy may run every call a second time.
        let runs = if req.probe_code.is_some() { 2 } else { 1 };
        let calls = u32::try_from(req.calls.len())
            .unwrap_or(u32::MAX)
            .saturating_mul(runs);
        let wall = STARTUP_ALLOWANCE
            .saturating_add(
                req.call_timeout
                    .saturating_add(Duration::from_millis(500))
                    .saturating_mul(calls),
            )
            .min(Duration::from_secs(600));
        Self {
            wall,
            cpu_seconds: wall.as_secs() + 10,
            ..Self::default()
        }
    }
}

/// One call of the candidate function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The input as EDN text.
    pub input: String,
    /// The expected output as EDN text; without it the call only reports the value.
    pub expected: Option<String>,
}

/// How results are compared with expected values. Apart from the options here, comparison
/// follows Clojure's `=`: sequential collections (vectors, lists, sequences) are equal when their
/// elements are equal in order, maps and sets regardless of order, and integers regardless of
/// their class (`1` equals `1N`). Numbers of different categories are never equal: an integer is
/// not equal to a floating-point number or a ratio (`1` differs from `1.0` and from `1/1` written
/// as `1.0`), and a BigDecimal only equals a BigDecimal of the same value (`1.50M` equals `1.5M`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Equality {
    /// Floating-point numbers are equal when they differ by at most this fraction of the larger
    /// magnitude (or by 1e-12 near zero). `NaN` equals `NaN`. Map keys and set elements are
    /// compared exactly.
    pub float_tolerance: f64,
    /// The top-level output, when sequential, is compared as a multiset: order is ignored, but
    /// every element must be matched once.
    pub unordered: bool,
}

impl Default for Equality {
    fn default() -> Self {
        Self {
            float_tolerance: 1e-9,
            unordered: false,
        }
    }
}

/// The type a value must have, as checked by the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeRule {
    Nil,
    Boolean,
    Integer,
    Number,
    String,
    Keyword,
    Symbol,
    Map,
    Vector,
    /// A list or any other sequence.
    List,
    Set,
    Sequential,
}

impl TypeRule {
    fn keyword(self) -> &'static str {
        match self {
            Self::Nil => ":nil",
            Self::Boolean => ":boolean",
            Self::Integer => ":integer",
            Self::Number => ":number",
            Self::String => ":string",
            Self::Keyword => ":keyword",
            Self::Symbol => ":symbol",
            Self::Map => ":map",
            Self::Vector => ":vector",
            Self::List => ":list",
            Self::Set => ":set",
            Self::Sequential => ":sequential",
        }
    }
}

/// A rule for one key of the output map (or of each map in an output collection).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRule {
    /// The key as EDN text.
    pub key: String,
    pub required: bool,
    pub value_type: Option<TypeRule>,
    pub nil_allowed: bool,
}

/// The checkable part of an output contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputRules {
    pub value_type: Option<TypeRule>,
    pub keys: Vec<KeyRule>,
}

impl OutputRules {
    pub fn is_empty(&self) -> bool {
        self.value_type.is_none() && self.keys.is_empty()
    }

    fn to_edn(&self) -> String {
        let ty = |t: Option<TypeRule>| t.map_or(":any", TypeRule::keyword);
        let keys: Vec<String> = self
            .keys
            .iter()
            .map(|k| {
                format!(
                    "{{:key {} :required {} :type {} :nil {}}}",
                    edn_string(&k.key),
                    k.required,
                    ty(k.value_type),
                    if k.nil_allowed { ":allowed" } else { ":disallowed" }
                )
            })
            .collect();
        format!(
            "{{:output {{:type {} :keys [{}]}}}}",
            ty(self.value_type),
            keys.join(" ")
        )
    }
}

#[derive(Clone, Debug)]
pub struct EvalRequest {
    pub code: String,
    pub function: String,
    pub calls: Vec<Call>,
    /// Static checks; only disabled to demonstrate that the OS sandbox holds on its own.
    pub static_checks: bool,
    /// Analyse the code after macroexpansion before loading it.
    pub lint: bool,
    pub output_rules: Option<OutputRules>,
    pub equality: Equality,
    /// Time limit of each call, including realizing its result.
    pub call_timeout: Duration,
    /// Elements a result may have once realized; more is reported as too large (and is how an
    /// infinite lazy sequence shows up).
    pub max_elements: u64,
    /// Print each result without collection or nesting bounds, cut at this many characters
    /// (reported as [`CallResult::truncated`]), so that a complete print is the exact value.
    pub output_chars: Option<usize>,
    /// An instrumented copy of `code` whose intermediate values are recorded (see [`probe`]).
    pub probe_code: Option<String>,
    /// Run the instrumented copy only for calls that failed (threw, returned a wrong value, or
    /// broke the output contract).
    pub probe_failing_only: bool,
    /// Each input is a vector of arguments the function is applied to.
    pub spread: bool,
    /// `code` is a whole project namespace file (see [`ProjectEval`]).
    pub project: Option<ProjectEval>,
}

/// A namespace file of a project, checked only where the function was written: the rest of the
/// file is the user's own code and is loaded as it is, with the project's classpath.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectEval {
    /// The top-level definitions the static checks and lint apply to.
    pub checked: Vec<String>,
    /// Namespaces the file requires; the checked definitions may use them.
    pub namespaces: Vec<String>,
}

impl EvalRequest {
    /// A checked and linted request with default comparison and limits.
    pub fn new(code: impl Into<String>, function: impl Into<String>, calls: Vec<Call>) -> Self {
        Self {
            code: code.into(),
            function: function.into(),
            calls,
            static_checks: true,
            lint: true,
            output_rules: None,
            equality: Equality::default(),
            call_timeout: Duration::from_secs(5),
            max_elements: 1_000_000,
            output_chars: None,
            probe_code: None,
            probe_failing_only: false,
            spread: false,
            project: None,
        }
    }

    pub fn to_edn(&self) -> String {
        let calls: Vec<String> = self
            .calls
            .iter()
            .map(|c| match &c.expected {
                Some(e) => format!("{{:input {} :expected {}}}", edn_string(&c.input), edn_string(e)),
                None => format!("{{:input {}}}", edn_string(&c.input)),
            })
            .collect();
        let contract = match &self.output_rules {
            Some(r) if !r.is_empty() => format!(" :contract {}", r.to_edn()),
            _ => String::new(),
        };
        let mut extra = String::new();
        if let Some(n) = self.output_chars {
            extra.push_str(&format!(" :output-chars {n}"));
        }
        if let Some(code) = &self.probe_code {
            extra.push_str(&format!(" :probe-code {}", edn_string(code)));
            if self.probe_failing_only {
                extra.push_str(" :probe-failing-only true");
            }
        }
        if self.spread {
            extra.push_str(" :spread true");
        }
        if let Some(p) = &self.project {
            let list = |items: &[String]| items.iter().map(|i| edn_string(i)).collect::<Vec<_>>().join(" ");
            extra.push_str(&format!(
                " :project {{:checked [{}] :namespaces [{}]}}",
                list(&p.checked),
                list(&p.namespaces)
            ));
        }
        format!(
            "{{:code {} :fn {} :calls [{}] :checks {} :lint {}{contract} :equality {{:float-tolerance {:e} :unordered {}}} :call-ms {} :max-elements {}{extra}}}",
            edn_string(&self.code),
            edn_string(&self.function),
            calls.join(" "),
            self.static_checks,
            self.lint,
            self.equality.float_tolerance,
            self.equality.unordered,
            self.call_timeout.as_millis(),
            self.max_elements,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalStatus {
    /// The harness finished (its report may still contain check, load, or call errors), or
    /// stopped itself after a call exceeded its time limit.
    Completed,
    TimedOut,
    OutputLimitExceeded,
    MemoryLimitExceeded,
    /// The JVM ended without finishing (crash, CPU limit, or startup failure).
    Crashed(String),
    SandboxUnavailable(String),
    Cancelled,
}

/// An error reported by the harness for a stage or a call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct HarnessError {
    /// `read`, `check`, `load`, `call`, `realize`, or `expected`.
    #[serde(default)]
    pub phase: String,
    /// The exception class, when there was one.
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub line: Option<u32>,
    /// The offending form, for static check violations.
    #[serde(default)]
    pub form: Option<String>,
    /// For a call error, the `clojure.core` function the exception surfaced in.
    #[serde(default, rename = "in")]
    pub within: Option<String>,
    /// For a call error, the lines of every frame in the candidate's code, innermost first: the
    /// calls of its functions that were running when the exception was thrown.
    #[serde(default)]
    pub lines: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LintLevel {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LintFinding {
    pub level: LintLevel,
    #[serde(default)]
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallStatus {
    /// The function returned and its result was realized.
    Ok,
    /// The function (or reading its input) threw.
    Error,
    /// The call exceeded its time limit; the harness stopped afterwards.
    Timeout,
    /// The realized result exceeded the element limit.
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CallResult {
    pub index: usize,
    pub status: CallStatus,
    /// The result printed readably, bounded in length.
    #[serde(default)]
    pub actual: Option<String>,
    /// With [`EvalRequest::output_chars`]: whether `actual` was cut at that length.
    #[serde(default)]
    pub truncated: bool,
    /// Whether the result equals the expected value; absent without an expected value.
    #[serde(default, rename = "match")]
    pub matches: Option<bool>,
    /// The first difference from the expected value.
    #[serde(default)]
    pub difference: Option<String>,
    /// Output contract violations.
    #[serde(default)]
    pub contract: Vec<String>,
    /// Text the function printed, bounded.
    #[serde(default)]
    pub out: String,
    #[serde(default, rename = "elapsed-ns")]
    pub elapsed_ns: u64,
    #[serde(default)]
    pub error: Option<HarnessError>,
    /// Explanation for statuses without an exception (too large).
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Line {
    Read {
        ok: bool,
        #[serde(flatten)]
        error: HarnessError,
    },
    Check {
        ok: bool,
        #[serde(flatten)]
        error: HarnessError,
    },
    Lint {
        findings: Vec<LintFinding>,
    },
    Load {
        ok: bool,
        #[serde(flatten)]
        error: HarnessError,
    },
    Start {
        index: usize,
    },
    Result(CallResult),
    #[serde(rename = "probe-setup")]
    ProbeSetup {
        ok: bool,
        #[serde(default)]
        message: String,
    },
    Probe(ProbeCall),
    Halted,
    Done,
}

/// What the harness reported, stage by stage. A stage that never reported (because an earlier
/// one failed or the process ended) is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarnessReport {
    pub read: Option<Result<(), HarnessError>>,
    pub check: Option<Result<(), HarnessError>>,
    pub lint: Option<Vec<LintFinding>>,
    pub load: Option<Result<(), HarnessError>>,
    pub results: Vec<CallResult>,
    /// The call that had started but not finished when the process ended.
    pub running: Option<usize>,
    /// The harness stopped itself after a call timed out.
    pub halted: bool,
    pub done: bool,
    /// Output lines that were not part of the protocol (at most a few, shortened).
    pub stray: Vec<String>,
    /// With [`EvalRequest::probe_code`]: whether the instrumented code was accepted and loaded.
    pub probe_setup: Option<Result<(), String>>,
    /// The instrumented run of each call.
    pub probes: Vec<ProbeCall>,
}

impl HarnessReport {
    /// Reads the harness's output, tolerating a final line cut off by the process ending.
    pub fn parse(stdout: &str) -> Self {
        let mut r = Self::default();
        for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(parsed) = serde_json::from_str::<Line>(line) else {
                if r.stray.len() < 5 {
                    r.stray.push(line.chars().take(300).collect());
                }
                continue;
            };
            match parsed {
                Line::Read { ok, error } => r.read = Some((!ok).then_some(error).map_or(Ok(()), Err)),
                Line::Check { ok, error } => r.check = Some((!ok).then_some(error).map_or(Ok(()), Err)),
                Line::Lint { findings } => r.lint = Some(findings),
                Line::Load { ok, error } => r.load = Some((!ok).then_some(error).map_or(Ok(()), Err)),
                Line::Start { index } => r.running = Some(index),
                Line::Result(result) => {
                    if r.running == Some(result.index) {
                        r.running = None;
                    }
                    r.results.push(result);
                }
                Line::ProbeSetup { ok, message } => {
                    r.probe_setup = Some(if ok { Ok(()) } else { Err(message) });
                }
                Line::Probe(call) => r.probes.push(call),
                Line::Halted => r.halted = true,
                Line::Done => r.done = true,
            }
        }
        r
    }

    pub fn result(&self, index: usize) -> Option<&CallResult> {
        self.results.iter().find(|r| r.index == index)
    }
}

#[derive(Clone, Debug)]
pub struct EvalOutcome {
    pub status: EvalStatus,
    pub report: HarnessReport,
    pub stderr: String,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("preparing the harness failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Run(#[from] clojurecraft_sandbox::RunError),
}

/// Quotes `s` as an EDN string literal.
pub fn edn_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The result of running a script shipped with the application.
#[derive(Clone, Debug)]
pub struct ScriptOutcome {
    pub status: EvalStatus,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl ClojureRuntime {
    /// Writes a script under a content-addressed name so concurrent versions never collide.
    fn script_path(&self, stem: &str, source: &str) -> io::Result<PathBuf> {
        let hash = hex::encode(Sha256::digest(source.as_bytes()));
        let path = self.work_dir.join(format!("{stem}-{}.clj", &hash[..16]));
        if fs::read_to_string(&path).ok().as_deref() != Some(source) {
            fs::create_dir_all(&self.work_dir)?;
            // Unique per writer, so concurrent evaluations never rename a half-written file.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
            fs::write(&tmp, source)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(path)
    }

    fn policy(&self, harness: &Path, limits: &Limits, project: bool, scratch: Option<&Path>) -> Policy {
        let mut exec = Policy::system_library_dirs();
        exec.push(self.java_home.clone());
        let mut read = Policy::system_read_files();
        read.extend(self.jars.iter().cloned());
        if project {
            read.extend(self.project_classpath.iter().filter(|p| p.exists()).cloned());
        }
        read.push(harness.to_owned());
        Policy {
            read,
            exec,
            cpu_seconds: limits.cpu_seconds,
            address_space_bytes: None,
            // The JVM keeps every classpath jar open while loading a project's dependencies.
            max_open_files: 256
                + if project {
                    2 * self.project_classpath.len() as u64
                } else {
                    0
                },
            write: scratch.map(Path::to_owned).into_iter().collect(),
        }
    }

    /// A fresh, empty directory for one run of project code, which may write and load native
    /// code there (libraries such as ONNX Runtime unpack their native library into the
    /// temporary directory). It is removed after the run.
    fn scratch_dir(&self) -> Option<PathBuf> {
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.work_dir.join(format!("scratch-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).ok().map(|()| dir)
    }

    fn jvm_args(
        &self,
        harness: &Path,
        limits: &Limits,
        project: bool,
        scratch: Option<&Path>,
    ) -> Vec<String> {
        let sep = if cfg!(windows) { ";" } else { ":" };
        // The runtime's own Clojure comes first, so a project's Clojure jar never replaces it.
        let extra = if project { &self.project_classpath[..] } else { &[] };
        let cp: Vec<String> = self
            .jars
            .iter()
            .chain(extra)
            .map(|j| j.display().to_string())
            .collect();
        let tmpdir = scratch.map(|d| format!("-Djava.io.tmpdir={}", d.display()));
        let mut args = vec![
            format!("-Xmx{}m", limits.heap_mb),
            "-Xss8m".into(),
            "-XX:+UseSerialGC".into(),
            "-XX:TieredStopAtLevel=1".into(),
            "-XX:-UsePerfData".into(),
            "-XX:MaxMetaspaceSize=256m".into(),
            "-XX:ReservedCodeCacheSize=64m".into(),
            "-XX:ActiveProcessorCount=2".into(),
            "-XX:+ExitOnOutOfMemoryError".into(),
            "-XX:ErrorFile=/dev/null".into(),
            "-Xshare:auto".into(),
            "-Djava.awt.headless=true".into(),
            "-Dstdout.encoding=UTF-8".into(),
            "-Dclojure.main.report=stderr".into(),
        ];
        args.extend(tmpdir);
        args.extend([
            "-cp".into(),
            cp.join(sep),
            "clojure.main".into(),
            harness.display().to_string(),
        ]);
        args
    }

    /// Runs one of the application's own scripts (`source`) in the same sandbox and with the same
    /// limits as candidate code. A script is considered completed when it exits successfully.
    pub fn run_script(
        &self,
        stem: &str,
        source: &str,
        stdin: &[u8],
        limits: &Limits,
    ) -> Result<ScriptOutcome, EvalError> {
        let script = self.script_path(stem, source)?;
        let (status, stdout, stderr, elapsed) =
            self.run_sandboxed(&script, stdin, limits, None, false, |_| true)?;
        Ok(ScriptOutcome {
            status,
            stdout,
            stderr,
            elapsed,
        })
    }

    fn run_sandboxed(
        &self,
        script: &Path,
        stdin: &[u8],
        limits: &Limits,
        cancel: Option<Arc<AtomicBool>>,
        project: bool,
        completed: impl Fn(&str) -> bool,
    ) -> Result<(EvalStatus, String, String, Duration), EvalError> {
        let scratch = project.then(|| self.scratch_dir()).flatten();
        let result = run(&RunRequest {
            helper: self.sandbox_helper.clone(),
            policy: self.policy(script, limits, project, scratch.as_deref()),
            program: self.java.clone(),
            args: self.jvm_args(script, limits, project, scratch.as_deref()),
            env: vec![("LANG".into(), "C.UTF-8".into())],
            stdin: stdin.to_vec(),
            wall_timeout: limits.wall,
            max_stdout: limits.max_output_bytes,
            max_stderr: 64 * 1024,
            cancel,
        });
        if let Some(dir) = &scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
        let outcome: Outcome = result?;
        let stdout = String::from_utf8_lossy(&outcome.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&outcome.stderr).into_owned();
        let status = match outcome.termination {
            Termination::Exited(0) if completed(&stdout) => EvalStatus::Completed,
            Termination::Exited(JVM_OOM_EXIT) => EvalStatus::MemoryLimitExceeded,
            Termination::Exited(c) => EvalStatus::Crashed(format!("exit status {c}")),
            Termination::Signaled(s) => EvalStatus::Crashed(format!("killed by signal {s}")),
            Termination::TimedOut => EvalStatus::TimedOut,
            Termination::OutputLimitExceeded => EvalStatus::OutputLimitExceeded,
            Termination::Cancelled => EvalStatus::Cancelled,
            Termination::SandboxSetupFailed => EvalStatus::SandboxUnavailable(stderr.clone()),
        };
        Ok((status, stdout, stderr, outcome.elapsed))
    }

    /// Runs `req` in a fresh sandboxed JVM. `cancel`, when set from another thread, stops it.
    pub fn evaluate(
        &self,
        req: &EvalRequest,
        limits: &Limits,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<EvalOutcome, EvalError> {
        let harness = self.script_path("harness", HARNESS_SOURCE)?;
        let (status, stdout, stderr, elapsed) = self.run_sandboxed(
            &harness,
            req.to_edn().as_bytes(),
            limits,
            cancel,
            req.project.is_some(),
            |out| {
                out.lines()
                    .any(|l| l == r#"{"event":"done"}"# || l == r#"{"event":"halted"}"#)
            },
        )?;
        Ok(EvalOutcome {
            status,
            report: HarnessReport::parse(&stdout),
            stderr,
            elapsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_project_classpath_gets_enough_open_files() {
        let cp: Vec<PathBuf> = (0..247)
            .map(|i| PathBuf::from(format!("/m2/lib-{i}.jar")))
            .collect();
        let rt = ClojureRuntime {
            java: PathBuf::from("/java"),
            java_home: PathBuf::from("/jre"),
            jars: vec![PathBuf::from("/clojure.jar")],
            sandbox_helper: PathBuf::from("/helper"),
            work_dir: PathBuf::from("/work"),
            project_classpath: cp,
        };
        let limits = Limits::default();
        let harness = Path::new("/harness.clj");
        assert_eq!(rt.policy(harness, &limits, false, None).max_open_files, 256);
        assert!(rt.policy(harness, &limits, true, None).max_open_files >= 256 + 2 * 247);
    }

    #[test]
    fn edn_strings_escape_quotes_and_backslashes() {
        assert_eq!(edn_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn request_is_one_edn_map() {
        let mut r = EvalRequest::new(
            "(ns a)",
            "f",
            vec![
                Call {
                    input: "{:a 1}".into(),
                    expected: Some("\"s\"".into()),
                },
                Call {
                    input: "2".into(),
                    expected: None,
                },
            ],
        );
        r.output_rules = Some(OutputRules {
            value_type: Some(TypeRule::Map),
            keys: vec![KeyRule {
                key: ":total".into(),
                required: true,
                value_type: Some(TypeRule::Integer),
                nil_allowed: false,
            }],
        });
        assert_eq!(
            r.to_edn(),
            r#"{:code "(ns a)" :fn "f" :calls [{:input "{:a 1}" :expected "\"s\""} {:input "2"}] :checks true :lint true :contract {:output {:type :map :keys [{:key ":total" :required true :type :integer :nil :disallowed}]}} :equality {:float-tolerance 1e-9 :unordered false} :call-ms 5000 :max-elements 1000000}"#
        );
    }

    #[test]
    fn project_requests_carry_their_checked_names_and_namespaces() {
        let mut r = EvalRequest::new("(ns a)", "f", vec![]);
        r.spread = true;
        r.project = Some(ProjectEval {
            checked: vec!["f".into(), "g".into()],
            namespaces: vec!["shop.util".into()],
        });
        assert!(
            r.to_edn()
                .ends_with(r#":spread true :project {:checked ["f" "g"] :namespaces ["shop.util"]}}"#),
            "{}",
            r.to_edn()
        );
    }

    #[test]
    fn the_report_attributes_a_process_that_died_mid_call() {
        let out = concat!(
            r#"{"event":"read","ok":true,"forms":2}"#,
            "\n",
            r#"{"event":"check","ok":true}"#,
            "\n",
            r#"{"event":"lint","findings":[{"level":"warning","line":2,"message":"x is bound but never used"}]}"#,
            "\n",
            r#"{"event":"load","ok":true}"#,
            "\n",
            r#"{"event":"start","index":0}"#,
            "\n",
            r#"{"index":0,"status":"ok","actual":"[1]","match":false,"difference":"the output: expected 2 elements","contract":[],"out":"","elapsed-ns":10,"event":"result"}"#,
            "\n",
            r#"{"event":"start","index":1}"#,
            "\n",
            r#"{"index":1,"status":"ok","act"#,
        );
        let r = HarnessReport::parse(out);
        assert_eq!(r.read, Some(Ok(())));
        assert_eq!(r.lint.as_ref().map(Vec::len), Some(1));
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].matches, Some(false));
        assert_eq!(r.running, Some(1), "the unfinished call is known");
        assert!(!r.done && !r.halted);
        assert_eq!(r.stray.len(), 1);
    }

    #[test]
    fn stage_errors_keep_their_position() {
        let r = HarnessReport::parse(
            r#"{"phase":"load","class":"java.lang.RuntimeException","message":"Unable to resolve symbol: x","line":7,"form":null,"event":"load","ok":false}"#,
        );
        let Some(Err(e)) = r.load else { panic!("{r:?}") };
        assert_eq!(
            (e.line, e.class.as_str()),
            (Some(7), "java.lang.RuntimeException")
        );
    }

    #[test]
    fn the_process_limit_covers_every_call() {
        let mut r = EvalRequest::new("", "f", vec![]);
        r.calls = vec![
            Call {
                input: "1".into(),
                expected: None,
            };
            3
        ];
        let l = Limits::for_request(&r);
        assert_eq!(l.wall, Duration::from_millis(20_000 + 3 * 5_500));
        assert!(l.cpu_seconds >= l.wall.as_secs());
    }
}
