//! Example inputs taken from another function's output.
//!
//! Resolving a reference runs one example of an upstream revision in the sandbox and records the
//! printed output as the downstream example's input. Nothing here performs I/O: the caller loads
//! the upstream revision and the library's reference graph, runs the planned request, and passes
//! the outcome back to [`resolve`].
//!
//! References only supply example data. They are never turned into calls in generated or
//! exported code.
//!
//! Before anything runs, [`plan`] refuses references that point at the function itself or close
//! a dependency cycle (directly or through other functions' references), upstream revisions
//! without the example or without runnable code, upstream examples without an input, and
//! accepted contracts whose types cannot match. After the run, [`resolve`] refuses failed calls,
//! outputs that are not EDN data or too long to record exactly, and values that violate the
//! downstream function's accepted input contract.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use clojurecraft_clojure::{CallStatus, EvalOutcome, EvalRequest, EvalStatus};
use clojurecraft_domain::{
    ContractStatus, Edn, ExampleId, ExampleInput, FunctionDocument, FunctionId, NilBehavior, OutputReference,
    ReferencedValue, RevisionId, RevisionPin, Shape, Timestamp, ValueType,
};
use clojurecraft_edn::{Node, NodeKind, Severity, parse_value};

use crate::Names;
use crate::deps;
use crate::spec;
use crate::structure::{self, Expectations};
use crate::validation::{self, CALL_TIMEOUT, MAX_ELEMENTS};

/// Longest upstream output recorded as an input, in characters of printed EDN. Longer outputs
/// are refused rather than recorded shortened.
pub const MAX_SNAPSHOT_CHARS: usize = 4_000_000;

/// Why a reference cannot be resolved. Each message names what to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceError {
    /// The example would take input from its own function.
    SelfReference,
    /// Taking input from the upstream function would close a dependency cycle. The path starts
    /// and ends with the downstream function, using the names given to [`describe_cycle`].
    Cycle(Vec<FunctionId>),
    /// The upstream revision has no example with the referenced identifier.
    MissingExample { revision_number: u32 },
    /// The upstream revision has no implementation to run.
    NoImplementation { revision_number: u32 },
    /// The upstream example has no input to run with.
    UpstreamInputMissing { example: String, reason: String },
    /// The upstream code does not pass the structural check.
    InvalidUpstreamCode(Vec<String>),
    /// Running the upstream example failed.
    UpstreamFailed(String),
    /// The upstream output is not EDN data (for example a function or a Java object).
    OutputNotData(String),
    /// The upstream output is too long to record exactly.
    OutputTooLarge,
    /// The accepted contracts, or the upstream output and the downstream input contract, do not
    /// fit together.
    IncompatibleContract(Vec<String>),
    /// Code cannot be run on this computer right now.
    Unavailable(String),
}

impl fmt::Display for ReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelfReference => f.write_str("An example cannot take its input from its own function."),
            Self::Cycle(_) => f.write_str(
                "This reference would create a dependency cycle: the upstream function already takes \
                 input, directly or through other functions, from this one.",
            ),
            Self::MissingExample { revision_number } => write!(
                f,
                "Revision {revision_number} of the upstream function has no such example; choose another \
                 example or revision."
            ),
            Self::NoImplementation { revision_number } => write!(
                f,
                "Revision {revision_number} of the upstream function has no implementation to run."
            ),
            Self::UpstreamInputMissing { example, reason } => {
                write!(f, "The upstream {example} cannot be run: {reason}")
            }
            Self::InvalidUpstreamCode(problems) => {
                write!(f, "The upstream code cannot be run: {}", problems.join("; "))
            }
            Self::UpstreamFailed(why) => write!(f, "Running the upstream example failed: {why}"),
            Self::OutputNotData(why) => write!(
                f,
                "The upstream output is not EDN data and cannot be used as an input: {why}"
            ),
            Self::OutputTooLarge => write!(
                f,
                "The upstream output is longer than {} characters and cannot be recorded exactly.",
                MAX_SNAPSHOT_CHARS
            ),
            Self::IncompatibleContract(problems) => {
                write!(f, "The contracts do not fit together: {}", problems.join("; "))
            }
            Self::Unavailable(why) => f.write_str(why),
        }
    }
}

/// Names a cycle for a message, for example "Totals → Orders → Totals".
pub fn describe_cycle(path: &[FunctionId], name: impl Fn(FunctionId) -> String) -> String {
    path.iter().map(|f| name(*f)).collect::<Vec<_>>().join(" → ")
}

/// The upstream revision a reference resolves to.
#[derive(Clone, Debug)]
pub struct Upstream<'a> {
    pub revision: RevisionId,
    pub number: u32,
    pub document: &'a FunctionDocument,
}

/// A planned run of an upstream example.
#[derive(Clone, Debug)]
pub struct UpstreamPlan {
    /// The downstream example whose input is being resolved.
    pub example: ExampleId,
    /// The reference exactly as it was when the run was planned.
    pub source: OutputReference,
    pub revision: RevisionId,
    pub revision_number: u32,
    /// The upstream example's label, for messages.
    pub upstream_label: String,
    pub request: EvalRequest,
    /// The downstream function's accepted input shape, checked against the output.
    pub input_shape: Option<Shape>,
    /// Notes found while planning (for example keys the upstream contract does not promise).
    pub notes: Vec<String>,
}

/// A recorded upstream output and what to tell the user about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub snapshot: ReferencedValue,
    pub notes: Vec<String>,
}

/// The namespace and function a document's code is expected to define.
pub fn names_for(doc: &FunctionDocument) -> Names {
    match doc.implementation.as_ref().and_then(|i| i.generation.as_ref()) {
        Some(g) if !g.namespace.is_empty() && !g.function_name.is_empty() => Names {
            namespace: g.namespace.clone(),
            function: g.function_name.clone(),
            arguments: doc.arguments(),
        },
        _ => Names::for_document(doc, |_| false),
    }
}

/// The library's reference graph with `doc`'s own references taken from `doc` (which may be
/// newer than its stored draft) and the references recorded in the upstream revision added.
pub fn graph_with(
    stored: &[(FunctionId, FunctionId)],
    doc: &FunctionDocument,
    upstream: Option<&FunctionDocument>,
) -> Vec<(FunctionId, FunctionId)> {
    let mut edges: BTreeSet<(FunctionId, FunctionId)> = stored
        .iter()
        .copied()
        .filter(|(from, _)| *from != doc.id)
        .collect();
    edges.extend(doc.output_references().map(|(_, r)| (doc.id, r.function)));
    if let Some(up) = upstream {
        edges.extend(up.output_references().map(|(_, r)| (up.id, r.function)));
    }
    edges.into_iter().collect()
}

/// The dependency cycle that taking input from `upstream` would close for `downstream`, as a
/// path `downstream → upstream → … → downstream`, or `None`.
pub fn find_cycle(
    edges: &[(FunctionId, FunctionId)],
    downstream: FunctionId,
    upstream: FunctionId,
) -> Option<Vec<FunctionId>> {
    if downstream == upstream {
        return Some(vec![downstream, downstream]);
    }
    let mut next: BTreeMap<FunctionId, Vec<FunctionId>> = BTreeMap::new();
    for (from, to) in edges {
        next.entry(*from).or_default().push(*to);
    }
    // Breadth-first from the upstream function, remembering how each function was reached.
    let mut came_from: BTreeMap<FunctionId, FunctionId> = BTreeMap::new();
    let mut queue = VecDeque::from([upstream]);
    let mut seen = BTreeSet::from([upstream]);
    while let Some(f) = queue.pop_front() {
        for &to in next.get(&f).map_or(&[][..], Vec::as_slice) {
            if to == downstream {
                let mut path = vec![downstream, f];
                let mut at = f;
                while let Some(&prev) = came_from.get(&at) {
                    path.push(prev);
                    at = prev;
                }
                path[1..].reverse();
                let mut cycle = vec![downstream];
                cycle.extend(path.into_iter().skip(1));
                cycle.push(downstream);
                return Some(cycle);
            }
            if seen.insert(to) {
                came_from.insert(to, f);
                queue.push_back(to);
            }
        }
    }
    None
}

fn accepted(doc: &FunctionDocument) -> Option<&clojurecraft_domain::Contract> {
    doc.contract
        .as_ref()
        .filter(|c| c.status == ContractStatus::Accepted)
}

/// Whether a value of type `up` can be a value of type `down`: `Some(true)` always, `Some(false)`
/// never, `None` sometimes.
fn types_fit(up: &ValueType, down: &ValueType) -> Option<bool> {
    use ValueType::{Any, Integer, List, Number, Other, Sequential, Vector};
    match (up, down) {
        (Any | Other(_), _) | (_, Any | Other(_)) => None,
        _ if up == down => Some(true),
        (Integer, Number) | (Vector | List, Sequential) => Some(true),
        (Number, Integer) | (Sequential, Vector | List) => None,
        _ => Some(false),
    }
}

/// Conflicts between the upstream function's accepted output contract and the downstream
/// function's accepted input contract (errors), and keys the downstream function requires that
/// the upstream contract does not promise (notes).
pub fn contract_fit(
    upstream: &FunctionDocument,
    downstream: &FunctionDocument,
) -> (Vec<String>, Vec<String>) {
    let (mut errors, mut notes) = (Vec::new(), Vec::new());
    let (Some(up), Some(down)) = (accepted(upstream), accepted(downstream)) else {
        return (errors, notes);
    };
    match types_fit(&up.output.value_type, &down.input.value_type) {
        Some(false) => errors.push(format!(
            "the upstream function returns a {} but this function's input is a {}",
            up.output.value_type.label(),
            down.input.value_type.label()
        )),
        None if !up.output.value_type.is_any() && !down.input.value_type.is_any() => notes.push(format!(
            "the upstream function returns a {}, which is not always a {} as this function's input \
             contract requires",
            up.output.value_type.label(),
            down.input.value_type.label()
        )),
        _ => {}
    }
    let promised: Vec<&str> = up.output.keys.iter().map(|k| k.key.as_str().trim()).collect();
    for k in &down.input.keys {
        let key = k.key.as_str().trim();
        if key.is_empty() {
            continue;
        }
        match up.output.keys.iter().find(|u| u.key.as_str().trim() == key) {
            Some(u) => {
                if let Some(false) = types_fit(&u.value_type, &k.value_type) {
                    errors.push(format!(
                        "key {key} is a {} in the upstream output but must be a {} here",
                        u.value_type.label(),
                        k.value_type.label()
                    ));
                }
            }
            None if k.required && !promised.is_empty() => notes.push(format!(
                "this function requires key {key}, which the upstream contract does not promise"
            )),
            None => {}
        }
    }
    (errors, notes)
}

/// Checks an upstream example, revision, and code before anything runs, and plans the run.
pub fn plan(
    downstream: &FunctionDocument,
    example: ExampleId,
    source: &OutputReference,
    upstream: &Upstream<'_>,
    edges: &[(FunctionId, FunctionId)],
) -> Result<UpstreamPlan, ReferenceError> {
    if source.function == downstream.id {
        return Err(ReferenceError::SelfReference);
    }
    let graph = graph_with(edges, downstream, Some(upstream.document));
    if let Some(path) = find_cycle(&graph, downstream.id, source.function) {
        return Err(ReferenceError::Cycle(path));
    }
    let up = upstream.document;
    let Some((index, up_example)) = up
        .examples
        .iter()
        .enumerate()
        .find(|(_, e)| e.id == source.example)
    else {
        return Err(ReferenceError::MissingExample {
            revision_number: upstream.number,
        });
    };
    let upstream_label = spec::example_label(index, up_example);
    let input = match &up_example.input {
        ExampleInput::Direct { value } => value.as_str(),
        ExampleInput::FunctionOutput {
            snapshot: Some(s), ..
        } => s.value.as_str(),
        ExampleInput::FunctionOutput { snapshot: None, .. } => {
            return Err(ReferenceError::UpstreamInputMissing {
                example: upstream_label,
                reason: "its input comes from another function and was never recorded.".into(),
            });
        }
    };
    if input.trim().is_empty() {
        return Err(ReferenceError::UpstreamInputMissing {
            example: upstream_label,
            reason: "it has no input.".into(),
        });
    }
    let parsed = parse_value(input);
    if let Some(d) = parsed.diagnostics.iter().find(|d| d.severity == Severity::Error) {
        return Err(ReferenceError::UpstreamInputMissing {
            example: upstream_label,
            reason: format!("its input is not valid EDN ({}).", d.message),
        });
    }
    let Some(code) = up.implementation.as_ref().map(|i| i.source.as_str()) else {
        return Err(ReferenceError::NoImplementation {
            revision_number: upstream.number,
        });
    };
    let names = names_for(up);
    let allowed = deps::allowed(up);
    let structure = structure::check(
        code,
        &Expectations {
            namespace: &names.namespace,
            function: &names.function,
            allowed: &allowed,
            arguments: names.arguments,
        },
    );
    let Some(function) = structure
        .function
        .clone()
        .filter(|_| structure.problems.is_empty())
    else {
        let mut problems: Vec<String> = structure.problems.iter().map(ToString::to_string).collect();
        if problems.is_empty() {
            problems.push("it does not define exactly one public function".into());
        }
        return Err(ReferenceError::InvalidUpstreamCode(problems));
    };
    let (errors, notes) = contract_fit(up, downstream);
    if !errors.is_empty() {
        return Err(ReferenceError::IncompatibleContract(errors));
    }
    let expected = up_example.expected.as_str().trim();
    let mut request = EvalRequest::new(
        code,
        function,
        vec![clojurecraft_clojure::Call {
            input: input.to_owned(),
            expected: (!expected.is_empty()).then(|| expected.to_owned()),
        }],
    );
    request.equality = validation::equality(up);
    request.call_timeout = CALL_TIMEOUT;
    request.max_elements = MAX_ELEMENTS;
    request.output_chars = Some(MAX_SNAPSHOT_CHARS);
    request.spread = up.arguments() != 1;
    crate::project::prepare(up, &mut request).map_err(|e| ReferenceError::InvalidUpstreamCode(vec![e]))?;
    Ok(UpstreamPlan {
        example,
        source: source.clone(),
        revision: upstream.revision,
        revision_number: upstream.number,
        upstream_label,
        request,
        input_shape: accepted(downstream).map(|c| c.input.clone()),
        notes,
    })
}

/// Turns the outcome of a planned run into a snapshot, or explains why it cannot be used.
pub fn resolve(
    plan: &UpstreamPlan,
    outcome: Result<&EvalOutcome, &str>,
    now: Timestamp,
) -> Result<Resolution, ReferenceError> {
    let outcome = outcome.map_err(|e| ReferenceError::Unavailable(e.to_owned()))?;
    let report = &outcome.report;
    let stage_error = [&report.read, &report.check, &report.load]
        .into_iter()
        .find_map(|s| s.as_ref().and_then(|r| r.as_ref().err()));
    if let Some(e) = stage_error {
        let at = e.line.map(|l| format!(" (line {l})")).unwrap_or_default();
        return Err(ReferenceError::UpstreamFailed(format!(
            "the upstream code could not be loaded{at}: {}",
            e.message
        )));
    }
    let Some(result) = report.result(0) else {
        let why = match &outcome.status {
            EvalStatus::Completed => "the runtime did not report a result".to_owned(),
            EvalStatus::TimedOut => "the runtime exceeded its time limit".to_owned(),
            EvalStatus::OutputLimitExceeded => "the code wrote more output than allowed".to_owned(),
            EvalStatus::MemoryLimitExceeded => "the runtime ran out of memory".to_owned(),
            EvalStatus::Crashed(how) => format!("the runtime ended unexpectedly ({how})"),
            EvalStatus::SandboxUnavailable(why) => {
                return Err(ReferenceError::Unavailable(format!(
                    "The sandbox could not be set up, so nothing was run: {}",
                    why.trim()
                )));
            }
            EvalStatus::Cancelled => "the run was cancelled".to_owned(),
        };
        return Err(ReferenceError::UpstreamFailed(why));
    };
    match result.status {
        CallStatus::Ok => {}
        CallStatus::Error => {
            let e = result.error.clone().unwrap_or_default();
            let class = e.class.rsplit('.').next().unwrap_or_default();
            let at = e.line.map(|l| format!(" at line {l}")).unwrap_or_default();
            return Err(ReferenceError::UpstreamFailed(format!(
                "the upstream function threw {class}{at}: {}",
                e.message
            )));
        }
        CallStatus::Timeout => {
            return Err(ReferenceError::UpstreamFailed(format!(
                "the upstream function did not finish within {} s",
                CALL_TIMEOUT.as_secs()
            )));
        }
        CallStatus::TooLarge => {
            return Err(ReferenceError::UpstreamFailed(
                result
                    .message
                    .clone()
                    .unwrap_or_else(|| "the result is too large".into()),
            ));
        }
    }
    if result.truncated {
        return Err(ReferenceError::OutputTooLarge);
    }
    let text = result.actual.clone().unwrap_or_default();
    let parsed = parse_value(&text);
    if let Some(d) = parsed.diagnostics.iter().find(|d| d.severity == Severity::Error) {
        return Err(ReferenceError::OutputNotData(format!(
            "{} in {}",
            d.message,
            preview(&text)
        )));
    }
    let Some(root) = parsed.values.first() else {
        return Err(ReferenceError::OutputNotData("the output is empty".into()));
    };
    if let Some(shape) = &plan.input_shape {
        let problems = shape_problems(shape, root, &text);
        if !problems.is_empty() {
            return Err(ReferenceError::IncompatibleContract(problems));
        }
    }
    let mut notes = plan.notes.clone();
    if result.matches == Some(false) {
        notes.push(format!(
            "The upstream function's output differs from the expected output of its {}{}; the actual \
             output was recorded.",
            plan.upstream_label,
            result
                .difference
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        ));
    }
    Ok(Resolution {
        snapshot: ReferencedValue {
            value: Edn::new(text),
            revision: plan.revision,
            resolved_at: now,
        },
        notes,
    })
}

fn preview(text: &str) -> String {
    let short: String = text.chars().take(80).collect();
    if short.len() < text.len() {
        format!("{short}…")
    } else {
        short
    }
}

/// Whether a parsed value has a type.
fn node_is(node: &Node, t: &ValueType) -> Option<bool> {
    Some(match t {
        ValueType::Any | ValueType::Other(_) => return None,
        ValueType::Nil => matches!(node.kind, NodeKind::Nil),
        ValueType::Boolean => matches!(node.kind, NodeKind::Boolean(_)),
        ValueType::Integer => matches!(node.kind, NodeKind::Integer),
        ValueType::Number => matches!(node.kind, NodeKind::Integer | NodeKind::Float | NodeKind::Ratio),
        ValueType::String => matches!(node.kind, NodeKind::String(_)),
        ValueType::Keyword => matches!(node.kind, NodeKind::Keyword),
        ValueType::Symbol => matches!(node.kind, NodeKind::Symbol),
        ValueType::Map => matches!(node.kind, NodeKind::Map(_)),
        ValueType::Vector => matches!(node.kind, NodeKind::Vector(_)),
        // A printed lazy sequence reads back as a list.
        ValueType::List => matches!(node.kind, NodeKind::List(_)),
        ValueType::Sequential => matches!(node.kind, NodeKind::List(_) | NodeKind::Vector(_)),
        ValueType::Set => matches!(node.kind, NodeKind::Set(_)),
    })
}

/// Where a value violates an input shape: its type, and for a map (or each map in a collection)
/// required keys, value types, and disallowed nils. At most a few problems are listed.
pub fn shape_problems(shape: &Shape, root: &Node, src: &str) -> Vec<String> {
    const MAX: usize = 6;
    let mut out = Vec::new();
    if node_is(root, &shape.value_type) == Some(false) {
        out.push(format!(
            "the output is a {} but this function's input must be a {}",
            root.type_name(),
            shape.value_type.label()
        ));
        return out;
    }
    let keys: Vec<_> = shape
        .keys
        .iter()
        .filter(|k| !k.key.as_str().trim().is_empty())
        .collect();
    if keys.is_empty() {
        return out;
    }
    let maps: Vec<(String, &Vec<(Node, Node)>)> = match &root.kind {
        NodeKind::Map(entries) => vec![("the output".into(), entries)],
        NodeKind::Vector(items) | NodeKind::List(items) | NodeKind::Set(items) => items
            .iter()
            .enumerate()
            .filter_map(|(i, n)| match &n.kind {
                NodeKind::Map(entries) => Some((format!("element {i}"), entries)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for (place, entries) in maps {
        for k in &keys {
            let key = k.key.as_str().trim();
            let value = entries.iter().find(|(kn, _)| kn.text(src) == key).map(|(_, v)| v);
            match value {
                None if k.required => out.push(format!("{place} has no required key {key}")),
                Some(v) if matches!(v.kind, NodeKind::Nil) && k.nil == NilBehavior::Disallowed => {
                    out.push(format!("{place} has nil for key {key}, which is not allowed"));
                }
                Some(v) if !matches!(v.kind, NodeKind::Nil) && node_is(v, &k.value_type) == Some(false) => {
                    out.push(format!(
                        "{place} has a {} for key {key}, which must be a {}",
                        v.type_name(),
                        k.value_type.label()
                    ));
                }
                None | Some(_) => {}
            }
            if out.len() >= MAX {
                return out;
            }
        }
    }
    out
}

/// How a referenced input relates to the upstream function's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotState {
    /// No output has been recorded; the example is not used until it is refreshed.
    NotRecorded,
    /// The recorded output comes from the revision the reference resolves to.
    Current,
    /// The reference follows the latest revision, and a newer one exists than the recorded
    /// output's. The example keeps using the recorded output until it is refreshed.
    NewerRevision { latest: RevisionId },
    /// The upstream function has no revisions at all.
    NoRevision,
}

/// Compares a referenced input's snapshot with the upstream function's latest revision.
pub fn snapshot_state(input: &ExampleInput, latest: Option<RevisionId>) -> Option<SnapshotState> {
    let ExampleInput::FunctionOutput { source, snapshot } = input else {
        return None;
    };
    Some(match (snapshot, source.revision, latest) {
        (None, _, _) => SnapshotState::NotRecorded,
        (Some(_), RevisionPin::Pinned(_), _) => SnapshotState::Current,
        (Some(_), RevisionPin::Latest, None) => SnapshotState::NoRevision,
        (Some(s), RevisionPin::Latest, Some(l)) if s.revision == l => SnapshotState::Current,
        (Some(_), RevisionPin::Latest, Some(l)) => SnapshotState::NewerRevision { latest: l },
    })
}

#[cfg(test)]
mod tests {
    use clojurecraft_clojure::{CallResult, HarnessReport};
    use clojurecraft_domain::{Contract, Example, Implementation, KeySpec};

    use super::*;

    const UP_CODE: &str = "(ns up.core)\n(defn up [orders]\n  (filterv :paid orders))";

    fn upstream() -> FunctionDocument {
        let mut up = FunctionDocument::new("up");
        up.examples.push(Example::direct(
            "paid",
            Edn::new("[{:id 1 :paid true} {:id 2}]"),
            Edn::new("[{:id 1 :paid true}]"),
        ));
        up.implementation = Some(Implementation::manual(UP_CODE));
        up
    }

    fn downstream_of(up: &FunctionDocument) -> (FunctionDocument, OutputReference) {
        let mut down = FunctionDocument::new("down");
        let id = down.add_example("from up");
        let source = OutputReference {
            function: up.id,
            revision: RevisionPin::Latest,
            example: up.examples[0].id,
        };
        down.set_input_reference(id, source.clone());
        (down, source)
    }

    fn outcome(result: CallResult) -> EvalOutcome {
        EvalOutcome {
            status: EvalStatus::Completed,
            report: HarnessReport {
                read: Some(Ok(())),
                check: Some(Ok(())),
                load: Some(Ok(())),
                results: vec![result],
                done: true,
                ..HarnessReport::default()
            },
            stderr: String::new(),
            elapsed: std::time::Duration::ZERO,
        }
    }

    fn ok(actual: &str) -> CallResult {
        CallResult {
            index: 0,
            status: CallStatus::Ok,
            actual: Some(actual.into()),
            truncated: false,
            matches: Some(true),
            difference: None,
            contract: Vec::new(),
            out: String::new(),
            elapsed_ns: 0,
            error: None,
            message: None,
        }
    }

    fn planned(down: &FunctionDocument, up: &FunctionDocument, source: &OutputReference) -> UpstreamPlan {
        let upstream = Upstream {
            revision: RevisionId::generate(),
            number: 3,
            document: up,
        };
        plan(down, down.examples[0].id, source, &upstream, &[]).expect("plan")
    }

    #[test]
    fn the_planned_run_uses_the_upstream_input_and_asks_for_the_exact_output() {
        let up = upstream();
        let (down, source) = downstream_of(&up);
        let p = planned(&down, &up, &source);
        assert_eq!(p.request.function, "up");
        assert_eq!(p.request.calls[0].input, "[{:id 1 :paid true} {:id 2}]");
        assert_eq!(p.request.output_chars, Some(MAX_SNAPSHOT_CHARS));
        assert_eq!(p.upstream_label, "Example 1 (paid)");
    }

    #[test]
    fn cycles_are_found_directly_and_through_other_functions() {
        let (a, b, c, d) = (
            FunctionId::generate(),
            FunctionId::generate(),
            FunctionId::generate(),
            FunctionId::generate(),
        );
        // b takes input from c, and c from a: a taking input from b closes a → b → c → a.
        let edges = vec![(b, c), (c, a), (d, b)];
        assert_eq!(find_cycle(&edges, a, b), Some(vec![a, b, c, a]));
        assert_eq!(find_cycle(&edges, a, d), Some(vec![a, d, b, c, a]));
        assert_eq!(find_cycle(&edges, d, a), None);
        assert_eq!(find_cycle(&[], a, a), Some(vec![a, a]));
    }

    #[test]
    fn a_reference_closing_a_cycle_is_refused_using_the_editors_own_references() {
        let mut up = upstream();
        let (down, source) = downstream_of(&up);
        // The upstream revision's own example takes input from the downstream function.
        let back = up.add_example("from down");
        up.set_input_reference(
            back,
            OutputReference {
                function: down.id,
                revision: RevisionPin::Latest,
                example: down.examples[0].id,
            },
        );
        let upstream = Upstream {
            revision: RevisionId::generate(),
            number: 1,
            document: &up,
        };
        let err = plan(&down, down.examples[0].id, &source, &upstream, &[]).unwrap_err();
        assert_eq!(err, ReferenceError::Cycle(vec![down.id, up.id, down.id]));

        // Through a third function whose stored draft takes input from the downstream one.
        let up = self::upstream();
        let third = FunctionId::generate();
        let stored = vec![(up.id, third), (third, down.id)];
        let upstream = Upstream {
            revision: RevisionId::generate(),
            number: 1,
            document: &up,
        };
        let source = OutputReference {
            function: up.id,
            revision: RevisionPin::Latest,
            example: up.examples[0].id,
        };
        let err = plan(&down, down.examples[0].id, &source, &upstream, &stored).unwrap_err();
        assert_eq!(err, ReferenceError::Cycle(vec![down.id, up.id, third, down.id]));
        assert_eq!(
            describe_cycle(&[down.id, up.id, down.id], |f| if f == down.id {
                "down".into()
            } else {
                "up".into()
            }),
            "down → up → down"
        );
        assert!(plan(&down, down.examples[0].id, &source, &upstream, &stored[..1]).is_ok());
    }

    #[test]
    fn upstream_problems_are_reported_before_anything_runs() {
        let up = upstream();
        let (down, source) = downstream_of(&up);
        let mut no_code = up.clone();
        no_code.implementation = None;
        fn at(doc: &FunctionDocument) -> Upstream<'_> {
            Upstream {
                revision: RevisionId::generate(),
                number: 2,
                document: doc,
            }
        }
        let example = down.examples[0].id;
        assert_eq!(
            plan(&down, example, &source, &at(&no_code), &[]).unwrap_err(),
            ReferenceError::NoImplementation { revision_number: 2 }
        );
        let mut no_example = up.clone();
        no_example.examples.clear();
        assert_eq!(
            plan(&down, example, &source, &at(&no_example), &[]).unwrap_err(),
            ReferenceError::MissingExample { revision_number: 2 }
        );
        let mut broken = up.clone();
        broken.implementation = Some(Implementation::manual("(ns up.core)\n(defn up [x] (slurp x))"));
        assert!(matches!(
            plan(&down, example, &source, &at(&broken), &[]).unwrap_err(),
            ReferenceError::InvalidUpstreamCode(p) if p.iter().any(|m| m.contains("slurp"))
        ));
        let mut unresolved = up.clone();
        let inner = OutputReference {
            function: FunctionId::generate(),
            revision: RevisionPin::Latest,
            example: ExampleId::generate(),
        };
        let id = unresolved.examples[0].id;
        unresolved.set_input_reference(id, inner);
        assert!(matches!(
            plan(&down, example, &source, &at(&unresolved), &[]).unwrap_err(),
            ReferenceError::UpstreamInputMissing { .. }
        ));
        let mut own = down.clone();
        own.examples[0].input = ExampleInput::FunctionOutput {
            source: OutputReference {
                function: down.id,
                ..source.clone()
            },
            snapshot: None,
        };
        let self_source = own.examples[0].input.reference().cloned().expect("reference");
        assert_eq!(
            plan(&own, example, &self_source, &at(&up), &[]).unwrap_err(),
            ReferenceError::SelfReference
        );
    }

    fn with_contract(doc: &mut FunctionDocument, input: Option<Shape>, output: Option<Shape>) {
        let mut c = Contract::empty(ContractStatus::Accepted);
        if let Some(i) = input {
            c.input = i;
        }
        if let Some(o) = output {
            c.output = o;
        }
        doc.contract = Some(c);
    }

    fn shape(value_type: ValueType, keys: Vec<KeySpec>) -> Shape {
        Shape {
            description: String::new(),
            value_type,
            keys,
        }
    }

    fn key(k: &str, required: bool, value_type: ValueType) -> KeySpec {
        KeySpec {
            key: Edn::new(k),
            required,
            value_type,
            nil: NilBehavior::Disallowed,
            notes: String::new(),
        }
    }

    #[test]
    fn contracts_that_cannot_fit_are_refused_and_doubtful_ones_noted() {
        let mut up = upstream();
        with_contract(&mut up, None, Some(shape(ValueType::Map, vec![])));
        let (mut down, source) = downstream_of(&up);
        with_contract(&mut down, Some(shape(ValueType::Vector, vec![])), None);
        let upstream = Upstream {
            revision: RevisionId::generate(),
            number: 1,
            document: &up,
        };
        let err = plan(&down, down.examples[0].id, &source, &upstream, &[]).unwrap_err();
        assert!(
            matches!(&err, ReferenceError::IncompatibleContract(p) if p[0].contains("returns a map")),
            "{err}"
        );

        with_contract(
            &mut up,
            None,
            Some(shape(
                ValueType::Sequential,
                vec![key(":id", true, ValueType::Integer)],
            )),
        );
        with_contract(
            &mut down,
            Some(shape(
                ValueType::Vector,
                vec![key(":paid", true, ValueType::Boolean)],
            )),
            None,
        );
        let upstream = Upstream {
            revision: RevisionId::generate(),
            number: 1,
            document: &up,
        };
        let p = plan(&down, down.examples[0].id, &source, &upstream, &[]).expect("doubtful is allowed");
        assert_eq!(p.notes.len(), 2, "{:?}", p.notes);
    }

    #[test]
    fn outputs_are_checked_against_the_downstream_input_contract() {
        let up = upstream();
        let (mut down, source) = downstream_of(&up);
        with_contract(
            &mut down,
            Some(shape(
                ValueType::Vector,
                vec![
                    key(":id", true, ValueType::Integer),
                    key(":paid", false, ValueType::Boolean),
                ],
            )),
            None,
        );
        let p = planned(&down, &up, &source);
        let fits = resolve(&p, Ok(&outcome(ok("[{:id 1, :paid true}]"))), Timestamp(5)).expect("fits");
        assert_eq!(fits.snapshot.value.as_str(), "[{:id 1, :paid true}]");
        assert_eq!(fits.snapshot.revision, p.revision);

        let err = resolve(&p, Ok(&outcome(ok("[{:paid nil} 3]"))), Timestamp(5)).unwrap_err();
        let ReferenceError::IncompatibleContract(problems) = err else {
            panic!("{err:?}")
        };
        assert_eq!(
            problems,
            vec![
                "element 0 has no required key :id".to_owned(),
                "element 0 has nil for key :paid, which is not allowed".to_owned()
            ]
        );
        let err = resolve(&p, Ok(&outcome(ok("{:id 1}"))), Timestamp(5)).unwrap_err();
        assert!(
            err.to_string()
                .contains("the output is a map but this function's input must be a vector")
        );
    }

    #[test]
    fn failed_truncated_and_non_data_outputs_are_never_recorded() {
        let up = upstream();
        let (down, source) = downstream_of(&up);
        let p = planned(&down, &up, &source);

        let mut threw = ok("");
        threw.status = CallStatus::Error;
        threw.actual = None;
        threw.error = Some(clojurecraft_clojure::HarnessError {
            phase: "call".into(),
            class: "java.lang.ArithmeticException".into(),
            message: "Divide by zero".into(),
            line: Some(3),
            ..Default::default()
        });
        let err = resolve(&p, Ok(&outcome(threw)), Timestamp(1)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Running the upstream example failed: the upstream function threw ArithmeticException at \
             line 3: Divide by zero"
        );

        let mut cut = ok("[1 2 3");
        cut.truncated = true;
        assert_eq!(
            resolve(&p, Ok(&outcome(cut)), Timestamp(1)).unwrap_err(),
            ReferenceError::OutputTooLarge
        );

        let err = resolve(
            &p,
            Ok(&outcome(ok("#object[clojure.core$inc 0x1 \"f\"]"))),
            Timestamp(1),
        )
        .unwrap_err();
        assert!(matches!(err, ReferenceError::OutputNotData(_)), "{err:?}");

        let mut crashed = outcome(ok("[]"));
        crashed.report.results.clear();
        crashed.status = EvalStatus::Crashed("killed by signal 9".into());
        assert!(
            resolve(&p, Ok(&crashed), Timestamp(1))
                .unwrap_err()
                .to_string()
                .contains("ended unexpectedly")
        );
        assert!(matches!(
            resolve(&p, Err("The Java runtime is not installed."), Timestamp(1)).unwrap_err(),
            ReferenceError::Unavailable(_)
        ));
    }

    #[test]
    fn an_output_that_differs_from_the_upstream_expectation_is_recorded_with_a_note() {
        let up = upstream();
        let (down, source) = downstream_of(&up);
        let p = planned(&down, &up, &source);
        let mut differs = ok("[]");
        differs.matches = Some(false);
        differs.difference = Some("the output: expected 1 element but got 0".into());
        let r = resolve(&p, Ok(&outcome(differs)), Timestamp(1)).expect("recorded");
        assert!(r.notes[0].contains("differs from the expected output of its Example 1 (paid)"));
    }

    #[test]
    fn snapshots_report_newer_upstream_revisions_without_changing() {
        let up = upstream();
        let (mut down, source) = downstream_of(&up);
        let id = down.examples[0].id;
        assert_eq!(
            snapshot_state(&down.examples[0].input, None),
            Some(SnapshotState::NotRecorded)
        );
        let r1 = RevisionId::generate();
        down.record_reference_snapshot(
            id,
            &source,
            ReferencedValue {
                value: Edn::new("[1]"),
                revision: r1,
                resolved_at: Timestamp(1),
            },
        );
        let before = down.spec_fingerprint();
        let r2 = RevisionId::generate();
        assert_eq!(
            snapshot_state(&down.examples[0].input, Some(r1)),
            Some(SnapshotState::Current)
        );
        assert_eq!(
            snapshot_state(&down.examples[0].input, Some(r2)),
            Some(SnapshotState::NewerRevision { latest: r2 })
        );
        assert_eq!(
            before,
            down.spec_fingerprint(),
            "a newer upstream revision changes nothing by itself"
        );
        let pinned = OutputReference {
            revision: RevisionPin::Pinned(r1),
            ..source
        };
        down.set_input_reference(id, pinned);
        assert_eq!(
            snapshot_state(&down.examples[0].input, Some(r2)),
            Some(SnapshotState::Current)
        );
    }
}
