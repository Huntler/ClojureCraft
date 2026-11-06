//! Results derived from a function: validation reports and benchmark measurements. Each result
//! records the fingerprints it was computed from, so its freshness can always be decided against
//! the current content instead of being cleared when content changes.

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Basis, Component, Fingerprint, Freshness};
use crate::function::Edn;
use crate::ids::{
    BenchmarkConfigId, BenchmarkInputId, BenchmarkResultId, ExampleId, GenerationRunId, TestCaseId,
    ValidationId,
};
use crate::time::Timestamp;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEnvironment {
    pub clojure_version: String,
    pub java_version: String,
    /// For example `linux-x86_64` or `macos-aarch64`.
    pub platform: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub id: ValidationId,
    pub created_at: Timestamp,
    pub basis: Basis,
    pub environment: ExecutionEnvironment,
    pub checks: Vec<Check>,
    /// The generation or repair attempt this report belongs to, when produced by that workflow.
    pub attempt: Option<u32>,
    /// The generation run (initial generation plus repairs) this report belongs to.
    #[serde(default)]
    pub run: Option<GenerationRunId>,
    /// The code that was checked. Candidates that were never applied to the function are kept
    /// here, so every attempt stays inspectable.
    #[serde(default)]
    pub source: Option<String>,
    /// How results were compared and execution was bounded.
    #[serde(default)]
    pub settings: Option<ValidationSettings>,
}

/// The comparison rules and limits a validation used.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ValidationSettings {
    /// Relative tolerance for floating-point numbers.
    pub float_tolerance: f64,
    /// Whether a sequential output was compared without regard to order.
    pub unordered_output: bool,
    pub call_timeout_ms: u64,
    /// Elements a result may have once lazy sequences are realized.
    pub max_elements: u64,
}

impl ValidationReport {
    /// A report passes only if it ran at least one check and every check passed.
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.status == CheckStatus::Passed)
    }

    pub fn freshness(&self, current: &Basis) -> Freshness {
        self.basis.freshness(current)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub subject: CheckSubject,
    pub status: CheckStatus,
    pub expected: Option<Edn>,
    pub actual: Option<Edn>,
    /// Bounded diagnostic text (compiler messages, exceptions, captured output).
    pub diagnostics: Vec<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum CheckSubject {
    Parse,
    Structure,
    Lint,
    Load,
    Contract,
    Example(ExampleId),
    Test(TestCaseId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Error,
    Timeout,
    ResourceLimit,
    Forbidden,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkEnvironment {
    pub execution: ExecutionEnvironment,
    pub os_version: String,
    pub cpu: String,
    /// Whether a model was loaded in the inference server while measuring.
    pub inference_server_loaded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub id: BenchmarkResultId,
    pub created_at: Timestamp,
    pub config_id: BenchmarkConfigId,
    pub config_fingerprint: Fingerprint,
    pub basis: Basis,
    /// What was measured, for example "baseline" or "optimized candidate 2".
    pub candidate_label: String,
    pub environment: BenchmarkEnvironment,
    /// Process start-up and load time, kept separate from steady-state measurements.
    pub startup_ms: Option<f64>,
    pub measurements: Vec<Measurement>,
}

impl BenchmarkResult {
    /// A benchmark is current only while the function content and its configuration are
    /// unchanged.
    pub fn freshness(&self, current: &Basis, current_config: Option<&Fingerprint>) -> Freshness {
        let mut changed = match self.basis.freshness(current) {
            Freshness::Current => Vec::new(),
            Freshness::Stale(c) => c,
        };
        if current_config != Some(&self.config_fingerprint) {
            changed.push(Component::BenchmarkConfiguration);
        }
        Freshness::from_changes(changed)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub input_id: BenchmarkInputId,
    pub input_fingerprint: Fingerprint,
    pub samples: u32,
    pub median_ns: f64,
    pub p25_ns: f64,
    pub p75_ns: f64,
    pub min_ns: f64,
    pub max_ns: f64,
    /// Only present when the runtime reports allocation reliably.
    pub allocated_bytes: Option<u64>,
}
