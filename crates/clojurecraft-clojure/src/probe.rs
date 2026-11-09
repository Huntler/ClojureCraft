//! Intermediate values recorded by an instrumented run.
//!
//! An [`EvalRequest`](crate::EvalRequest) with `probe_code` asks the harness to run every call a
//! second time with an instrumented copy of the code: the same source with calls of
//! `clojurecraft.harness/probe!` (or `probe-first` as a step of `->`) inserted around selected
//! expressions. The harness refuses the copy unless removing the probes gives back exactly the
//! forms it checked, so the recorded values are those of the checked code. Each call reports
//! whether the instrumented run produced the same result as the uninstrumented one; values from
//! a run that differs must not be presented as the code's intermediate values.

use serde::Deserialize;

/// The qualified name of the probe that wraps an expression (`(probe! id expr)`) or follows a
/// step of `->>` (`(probe! id)`).
pub const PROBE: &str = "clojurecraft.harness/probe!";
/// The probe used as a step of `->` (`(probe-first id)`).
pub const PROBE_FIRST: &str = "clojurecraft.harness/probe-first";
/// The probe that records locals before an expression (`(probe-locals! id [a b] expr)`); only
/// the latest values are kept.
pub const PROBE_LOCALS: &str = "clojurecraft.harness/probe-locals!";

/// Values kept per probe and call; later evaluations are only counted.
pub const VALUES_KEPT: usize = 3;
/// Bounds of each recorded value's print: elements per collection, nesting, and characters.
pub const PRINT_LENGTH: usize = 20;
pub const PRINT_LEVEL: usize = 6;
pub const PRINT_CHARS: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueKind {
    /// Data, printed readably within the bounds.
    Value,
    /// A function (for example a transducer or a composed step), which has no data to show.
    Function,
    /// Printing the value failed (for example a lazy sequence that throws when realized).
    Error,
}

/// One recorded evaluation of a probed expression.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ProbeValue {
    pub kind: ValueKind,
    /// The value printed with at most [`PRINT_LENGTH`] elements per collection, [`PRINT_LEVEL`]
    /// levels, and [`PRINT_CHARS`] characters.
    #[serde(default)]
    pub text: String,
    /// Whether `text` shows the whole value; otherwise it is a shortened preview.
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// What one probe recorded during one call.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ProbeCapture {
    pub id: usize,
    /// How often the expression was evaluated during the call.
    #[serde(rename = "count")]
    pub evaluations: u64,
    /// The first [`VALUES_KEPT`] values, in evaluation order.
    #[serde(default)]
    pub values: Vec<ProbeValue>,
    /// For a probe of locals ([`PROBE_LOCALS`]): their values the last time it was reached.
    #[serde(default)]
    pub locals: Vec<LocalValue>,
    /// For a probe of locals reached more than once: their values on each of the first few
    /// times, in order.
    #[serde(default)]
    pub entries: Vec<Vec<LocalValue>>,
}

/// A local's value recorded by [`PROBE_LOCALS`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LocalValue {
    pub name: String,
    #[serde(flatten)]
    pub value: ProbeValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeStatus {
    Ok,
    Error,
    Timeout,
}

/// The instrumented run of one call.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ProbeCall {
    pub index: usize,
    pub status: ProbeStatus,
    #[serde(default)]
    pub message: Option<String>,
    /// Whether the instrumented run ended exactly like the uninstrumented call (equal result,
    /// or the same exception class).
    #[serde(default)]
    pub consistent: bool,
    /// Probes that were evaluated at least once; a probe that is missing was never reached.
    #[serde(default)]
    pub probes: Vec<ProbeCapture>,
}

impl ProbeCall {
    pub fn capture(&self, id: usize) -> Option<&ProbeCapture> {
        self.probes.iter().find(|p| p.id == id)
    }
}
