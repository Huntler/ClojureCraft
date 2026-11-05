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
use std::time::Duration;

use clojurecraft_sandbox::{Outcome, Policy, RunRequest, Termination, run};
use sha2::{Digest, Sha256};

pub const HARNESS_SOURCE: &str = include_str!("../../../resources/clojure/harness.clj");

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

#[derive(Clone, Debug)]
pub struct EvalRequest {
    pub code: String,
    pub function: String,
    /// Each input as EDN text.
    pub inputs: Vec<String>,
    /// Static checks; only disabled to demonstrate that the OS sandbox holds on its own.
    pub static_checks: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalStatus {
    /// The harness produced a response (which may still report check, load, or call errors).
    Completed,
    TimedOut,
    OutputLimitExceeded,
    MemoryLimitExceeded,
    /// The JVM ended without a response (crash, CPU limit, or startup failure).
    Crashed(String),
    SandboxUnavailable(String),
}

#[derive(Clone, Debug)]
pub struct EvalOutcome {
    pub status: EvalStatus,
    /// The harness's EDN response when `status` is `Completed`.
    pub response: String,
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

impl EvalRequest {
    pub fn to_edn(&self) -> String {
        let inputs: Vec<String> = self.inputs.iter().map(|i| edn_string(i)).collect();
        format!(
            "{{:code {} :fn {} :inputs [{}] :checks {}}}",
            edn_string(&self.code),
            edn_string(&self.function),
            inputs.join(" "),
            self.static_checks
        )
    }
}

impl ClojureRuntime {
    /// Writes the harness under a content-addressed name so concurrent versions never collide.
    fn harness_path(&self) -> io::Result<PathBuf> {
        let hash = hex::encode(Sha256::digest(HARNESS_SOURCE.as_bytes()));
        let path = self.work_dir.join(format!("harness-{}.clj", &hash[..16]));
        if fs::read_to_string(&path).ok().as_deref() != Some(HARNESS_SOURCE) {
            fs::create_dir_all(&self.work_dir)?;
            // Unique per writer, so concurrent evaluations never rename a half-written file.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
            fs::write(&tmp, HARNESS_SOURCE)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(path)
    }

    fn policy(&self, harness: &Path, limits: &Limits) -> Policy {
        let mut exec = Policy::system_library_dirs();
        exec.push(self.java_home.clone());
        let mut read = Policy::system_read_files();
        read.extend(self.jars.iter().cloned());
        read.push(harness.to_owned());
        Policy {
            read,
            exec,
            cpu_seconds: limits.cpu_seconds,
            address_space_bytes: None,
            max_open_files: 256,
        }
    }

    fn jvm_args(&self, harness: &Path, limits: &Limits) -> Vec<String> {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let cp: Vec<String> = self.jars.iter().map(|j| j.display().to_string()).collect();
        vec![
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
            "-cp".into(),
            cp.join(sep),
            "clojure.main".into(),
            harness.display().to_string(),
        ]
    }

    pub fn evaluate(&self, req: &EvalRequest, limits: &Limits) -> Result<EvalOutcome, EvalError> {
        let harness = self.harness_path()?;
        let outcome: Outcome = run(&RunRequest {
            helper: self.sandbox_helper.clone(),
            policy: self.policy(&harness, limits),
            program: self.java.clone(),
            args: self.jvm_args(&harness, limits),
            env: vec![("LANG".into(), "C.UTF-8".into())],
            stdin: req.to_edn().into_bytes(),
            wall_timeout: limits.wall,
            max_stdout: limits.max_output_bytes,
            max_stderr: 64 * 1024,
        })?;
        let stdout = String::from_utf8_lossy(&outcome.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&outcome.stderr).into_owned();
        let status = match outcome.termination {
            Termination::Exited(0) if stdout.trim_start().starts_with('{') => EvalStatus::Completed,
            Termination::Exited(JVM_OOM_EXIT) => EvalStatus::MemoryLimitExceeded,
            Termination::Exited(c) => EvalStatus::Crashed(format!("exit status {c}")),
            Termination::Signaled(s) => EvalStatus::Crashed(format!("killed by signal {s}")),
            Termination::TimedOut => EvalStatus::TimedOut,
            Termination::OutputLimitExceeded => EvalStatus::OutputLimitExceeded,
            Termination::SandboxSetupFailed => EvalStatus::SandboxUnavailable(stderr.clone()),
        };
        let response = if status == EvalStatus::Completed {
            stdout
        } else {
            String::new()
        };
        Ok(EvalOutcome {
            status,
            response,
            stderr,
            elapsed: outcome.elapsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edn_strings_escape_quotes_and_backslashes() {
        assert_eq!(edn_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn request_is_one_edn_map() {
        let r = EvalRequest {
            code: "(ns a)".into(),
            function: "f".into(),
            inputs: vec!["{:a 1}".into(), "\"s\"".into()],
            static_checks: true,
        };
        assert_eq!(
            r.to_edn(),
            r#"{:code "(ns a)" :fn "f" :inputs ["{:a 1}" "\"s\""] :checks true}"#
        );
    }
}
