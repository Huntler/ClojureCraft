//! Example inputs taken from other functions' outputs: choosing the upstream function, revision,
//! and example; checking references against the library; and running upstream examples to
//! record their output.
//!
//! A worker thread with its own store connection answers library questions (revisions and
//! examples to choose from, the state of an open function's references, the upstream revision to
//! run) and plans upstream runs. The runs themselves go through the shared execution worker, and
//! their outputs are recorded into the open document only if the example still refers to exactly
//! the same upstream example and revision as when the run started.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use clojurecraft_domain::{
    ExampleId, ExampleInput, FunctionDocument, FunctionId, OutputReference, RevisionId, RevisionKind,
    RevisionPin, SecretRegistry, Timestamp,
};
use clojurecraft_runtime::DataDirs;
use clojurecraft_store::{DATABASE_FILE, Listing, ReferenceProblem, Store, StoreError};
use clojurecraft_workflow::assistant::Execution;
use clojurecraft_workflow::references::{self, ReferenceError, Resolution, Upstream, UpstreamPlan};
use clojurecraft_workflow::spec;

use crate::execution::ExecutionState;
use crate::jobs::Event;

/// Revisions offered when choosing a pinned revision, newest first.
const REVISIONS_OFFERED: usize = 50;

/// An upstream example as offered for selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExampleChoice {
    pub id: ExampleId,
    pub label: String,
}

/// A revision of a function as offered for selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionChoice {
    pub id: RevisionId,
    pub number: u32,
    pub created_at: Timestamp,
    pub reason: String,
    pub has_implementation: bool,
    pub examples: Vec<ExampleChoice>,
}

/// What can be referenced in one function: its revisions, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    pub function: FunctionId,
    pub name: String,
    pub revisions: Vec<RevisionChoice>,
}

impl Catalog {
    pub fn revision(&self, id: RevisionId) -> Option<&RevisionChoice> {
        self.revisions.iter().find(|r| r.id == id)
    }

    /// The revision a pin resolves to now.
    pub fn resolve(&self, pin: RevisionPin) -> Option<&RevisionChoice> {
        match pin {
            RevisionPin::Latest => self.revisions.first(),
            RevisionPin::Pinned(id) => self.revision(id),
        }
    }
}

/// The library's view of one referenced example input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceCheck {
    pub upstream_name: Option<String>,
    /// The latest revision of the upstream function (id and number).
    pub latest: Option<(RevisionId, u32)>,
    /// The number of the revision the snapshot was recorded from, when it still exists.
    pub snapshot_number: Option<u32>,
    /// Problems with the reference, as messages.
    pub problems: Vec<String>,
}

/// What the worker knows about the references of one open document.
#[derive(Clone, Debug, Default)]
pub struct DocumentChecks {
    pub examples: HashMap<ExampleId, ReferenceCheck>,
}

enum Command {
    Catalog(FunctionId),
    Check(Box<FunctionDocument>),
    Plan {
        document: Box<FunctionDocument>,
        example: ExampleId,
        source: OutputReference,
    },
}

pub enum ReferenceEvent {
    Catalog(FunctionId, Result<Catalog, String>),
    Checked(FunctionId, DocumentChecks),
    Planned {
        function: FunctionId,
        example: ExampleId,
        plan: Result<Box<UpstreamPlan>, String>,
    },
}

/// A reference selection being made in the editor, before it is applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    pub function: Option<FunctionId>,
    pub pin: Option<RevisionPin>,
    pub example: Option<ExampleId>,
}

impl Choice {
    pub fn reference(&self) -> Option<OutputReference> {
        Some(OutputReference {
            function: self.function?,
            revision: self.pin?,
            example: self.example?,
        })
    }
}

/// Something the editor asks for.
pub enum Request {
    /// Run the upstream example `source` names and, when it succeeds, make it the example's
    /// input with the recorded output.
    Resolve {
        function: FunctionId,
        example: ExampleId,
        source: OutputReference,
    },
    Cancel {
        function: FunctionId,
        example: ExampleId,
    },
}

enum Resolving {
    Planning,
    Running(u64),
}

/// An upstream run in progress, with the downstream input as it was when the run started: the
/// output is used only if that input is still unchanged when the run finishes.
struct Run {
    function: FunctionId,
    before: ExampleInput,
    plan: Box<UpstreamPlan>,
}

/// A resolution's result for display next to the example.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LastResult {
    Recorded { notes: Vec<String> },
    Failed(String),
}

pub struct ReferencesState {
    tx: Sender<Command>,
    catalogs: HashMap<FunctionId, Result<Catalog, String>>,
    catalog_requested: HashSet<FunctionId>,
    checks: HashMap<FunctionId, DocumentChecks>,
    /// The references each document's checks were requested for; checks are requested again
    /// when they change or the library changes.
    checked_for: HashMap<FunctionId, Vec<(ExampleId, OutputReference)>>,
    checks_in_flight: HashSet<FunctionId>,
    resolving: HashMap<(FunctionId, ExampleId), Resolving>,
    before: HashMap<(FunctionId, ExampleId), ExampleInput>,
    runs: HashMap<u64, Run>,
    results: HashMap<(FunctionId, ExampleId), LastResult>,
    /// Selections in progress, per example.
    pub choices: HashMap<(FunctionId, ExampleId), Choice>,
    /// The example whose recorded input is shown in the large inspector.
    pub viewing: Option<(FunctionId, ExampleId)>,
    pub requests: Vec<Request>,
    /// Messages for the user (for example a result that arrived for an edited example).
    pub notices: Vec<String>,
}

impl ReferencesState {
    pub fn new(events: Sender<Event>, ctx: egui::Context, secrets: Arc<SecretRegistry>) -> Self {
        let (tx, rx) = channel();
        let spawned = thread::Builder::new()
            .name("references".into())
            .spawn(move || run(&rx, &events, &ctx, &secrets));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "could not start the reference worker");
        }
        Self {
            tx,
            catalogs: HashMap::new(),
            catalog_requested: HashSet::new(),
            checks: HashMap::new(),
            checked_for: HashMap::new(),
            checks_in_flight: HashSet::new(),
            resolving: HashMap::new(),
            before: HashMap::new(),
            runs: HashMap::new(),
            results: HashMap::new(),
            choices: HashMap::new(),
            viewing: None,
            requests: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn send(&self, command: Command) {
        if self.tx.send(command).is_err() {
            tracing::error!("the reference worker has stopped");
        }
    }

    pub fn handle(&mut self, event: ReferenceEvent) -> Option<(FunctionId, ExampleId, Box<UpstreamPlan>)> {
        match event {
            ReferenceEvent::Catalog(function, catalog) => {
                self.catalog_requested.remove(&function);
                self.catalogs.insert(function, catalog);
                None
            }
            ReferenceEvent::Checked(function, checks) => {
                self.checks_in_flight.remove(&function);
                self.checks.insert(function, checks);
                None
            }
            ReferenceEvent::Planned {
                function,
                example,
                plan,
            } => {
                if !matches!(
                    self.resolving.get(&(function, example)),
                    Some(Resolving::Planning)
                ) {
                    return None;
                }
                match plan {
                    Ok(plan) => Some((function, example, plan)),
                    Err(e) => {
                        self.resolving.remove(&(function, example));
                        self.before.remove(&(function, example));
                        self.results.insert((function, example), LastResult::Failed(e));
                        None
                    }
                }
            }
        }
    }

    /// The catalog of a function, requesting it when unknown.
    pub fn catalog(&mut self, function: FunctionId) -> Option<&Result<Catalog, String>> {
        if !self.catalogs.contains_key(&function) && self.catalog_requested.insert(function) {
            self.send(Command::Catalog(function));
        }
        self.catalogs.get(&function)
    }

    /// Forgets what is known about the library (after functions or revisions changed).
    pub fn library_changed(&mut self) {
        self.catalogs.clear();
        self.checked_for.clear();
    }

    /// Requests checks of `doc`'s references when they changed since the last request.
    pub fn ensure_checked(&mut self, doc: &FunctionDocument) {
        let refs: Vec<(ExampleId, OutputReference)> =
            doc.output_references().map(|(e, r)| (e, r.clone())).collect();
        if refs.is_empty() || self.checks_in_flight.contains(&doc.id) {
            return;
        }
        if self.checked_for.get(&doc.id) == Some(&refs) {
            return;
        }
        self.checked_for.insert(doc.id, refs);
        self.checks_in_flight.insert(doc.id);
        self.send(Command::Check(Box::new(doc.clone())));
    }

    pub fn check(&self, function: FunctionId, example: ExampleId) -> Option<&ReferenceCheck> {
        self.checks.get(&function)?.examples.get(&example)
    }

    pub fn is_resolving(&self, function: FunctionId, example: ExampleId) -> bool {
        self.resolving.contains_key(&(function, example))
    }

    pub fn last_result(&self, function: FunctionId, example: ExampleId) -> Option<&LastResult> {
        self.results.get(&(function, example))
    }

    /// Plans running the upstream example `source` names for `doc`'s example. The plan is made
    /// for the document as it would be with that reference, so cycles it would close are found.
    pub fn resolve(&mut self, doc: &FunctionDocument, example: ExampleId, source: OutputReference) {
        let Some(before) = doc.example(example).map(|e| e.input.clone()) else {
            return;
        };
        if self.is_resolving(doc.id, example) {
            return;
        }
        let mut prospective = doc.clone();
        prospective.set_input_reference(example, source.clone());
        self.results.remove(&(doc.id, example));
        self.resolving.insert((doc.id, example), Resolving::Planning);
        self.before.insert((doc.id, example), before);
        self.send(Command::Plan {
            document: Box::new(prospective),
            example,
            source,
        });
    }

    /// Starts a planned upstream run.
    pub fn start(
        &mut self,
        execution: &mut ExecutionState,
        function: FunctionId,
        example: ExampleId,
        plan: Box<UpstreamPlan>,
    ) {
        let Some(before) = self.before.remove(&(function, example)) else {
            return;
        };
        match execution.start(plan.request.clone()) {
            Ok(run) => {
                self.resolving
                    .insert((function, example), Resolving::Running(run));
                self.runs.insert(
                    run,
                    Run {
                        function,
                        before,
                        plan,
                    },
                );
            }
            Err(e) => {
                self.resolving.remove(&(function, example));
                self.results.insert((function, example), LastResult::Failed(e));
            }
        }
    }

    pub fn cancel(&mut self, execution: &mut ExecutionState, function: FunctionId, example: ExampleId) {
        self.before.remove(&(function, example));
        if let Some(Resolving::Running(run)) = self.resolving.remove(&(function, example)) {
            execution.cancel(run);
            self.runs.remove(&run);
        }
    }

    /// Takes the finished upstream runs out of `executed` and records their outputs into the
    /// open documents. Returns the documents that changed.
    pub fn claim(
        &mut self,
        executed: &mut Vec<(u64, Execution)>,
        docs: &mut HashMap<FunctionId, crate::session::OpenDocument>,
    ) -> Vec<FunctionId> {
        let mut changed = Vec::new();
        executed.retain(|(run, execution)| {
            let Some(Run {
                function,
                before,
                plan,
            }) = self.runs.remove(run)
            else {
                return true;
            };
            let key = (function, plan.example);
            self.resolving.remove(&key);
            let resolved = references::resolve(
                &plan,
                execution.outcome.as_ref().map_err(String::as_str),
                Timestamp::now(),
            );
            let Resolution { snapshot, notes } = match resolved {
                Ok(r) => r,
                Err(e) => {
                    self.results.insert(key, LastResult::Failed(e.to_string()));
                    return false;
                }
            };
            let example = docs
                .get_mut(&function)
                .and_then(|open| open.doc.example_mut(plan.example));
            match example {
                Some(e) if e.input == before => {
                    let mut edited = e.set_reference(plan.source.clone());
                    edited |= e.record_snapshot(&plan.source, snapshot);
                    if edited {
                        changed.push(function);
                    }
                    self.choices.remove(&key);
                    self.results.insert(key, LastResult::Recorded { notes });
                }
                _ => self.notices.push(
                    "The example's input changed while the upstream example ran, so its output was not \
                     used."
                        .into(),
                ),
            }
            false
        });
        changed
    }
}

fn run(rx: &Receiver<Command>, events: &Sender<Event>, ctx: &egui::Context, secrets: &Arc<SecretRegistry>) {
    let emit = |event: ReferenceEvent| {
        let _ = events.send(Event::References(Box::new(event)));
        ctx.request_repaint();
    };
    let mut store: Option<Store> = None;
    for command in rx {
        // Opened on first use, after the library service has opened (and migrated) the library.
        if store.is_none() {
            store = DataDirs::discover()
                .and_then(|d| Store::open(&d.library().join(DATABASE_FILE), secrets.clone()).ok())
                .map(|(s, _)| s);
        }
        let Some(store) = &store else {
            let message = "The function library could not be opened.".to_owned();
            match command {
                Command::Catalog(f) => emit(ReferenceEvent::Catalog(f, Err(message))),
                Command::Check(doc) => emit(ReferenceEvent::Checked(doc.id, DocumentChecks::default())),
                Command::Plan {
                    document, example, ..
                } => emit(ReferenceEvent::Planned {
                    function: document.id,
                    example,
                    plan: Err(message),
                }),
            }
            continue;
        };
        match command {
            Command::Catalog(function) => {
                emit(ReferenceEvent::Catalog(
                    function,
                    catalog(store, function).map_err(|e| e.to_string()),
                ));
            }
            Command::Check(doc) => {
                let checks = check(store, &doc).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "could not check references");
                    DocumentChecks::default()
                });
                emit(ReferenceEvent::Checked(doc.id, checks));
            }
            Command::Plan {
                document,
                example,
                source,
            } => {
                let plan = plan(store, &document, example, &source).map(Box::new);
                emit(ReferenceEvent::Planned {
                    function: document.id,
                    example,
                    plan,
                });
            }
        }
    }
}

fn reason_label(kind: &RevisionKind) -> String {
    match kind {
        RevisionKind::Checkpoint => "saved".into(),
        RevisionKind::Generated => "generated".into(),
        RevisionKind::CodeEdited => "code edited".into(),
        RevisionKind::TestsAccepted => "tests accepted".into(),
        RevisionKind::Accepted => "accepted".into(),
        RevisionKind::ImplementationChosen => "implementation chosen".into(),
        RevisionKind::BeforeRestore => "before a restore".into(),
        RevisionKind::Restored { from_number, .. } => format!("restored from revision {from_number}"),
    }
}

fn function_name(store: &Store, function: FunctionId) -> Option<String> {
    [Listing::Active, Listing::Trash].into_iter().find_map(|l| {
        store
            .list_functions(l, "")
            .ok()?
            .into_iter()
            .find(|f| f.id == function)
            .map(|f| f.name)
    })
}

fn catalog(store: &Store, function: FunctionId) -> Result<Catalog, StoreError> {
    let name = function_name(store, function).unwrap_or_default();
    let mut revisions = Vec::new();
    for summary in store.revisions(function)?.into_iter().take(REVISIONS_OFFERED) {
        let doc = store.load_revision(summary.id)?.document;
        revisions.push(RevisionChoice {
            id: summary.id,
            number: summary.number,
            created_at: summary.created_at,
            reason: reason_label(&summary.reason.kind),
            has_implementation: doc.implementation.is_some(),
            examples: doc
                .examples
                .iter()
                .enumerate()
                .map(|(i, e)| ExampleChoice {
                    id: e.id,
                    label: spec::example_label(i, e),
                })
                .collect(),
        });
    }
    Ok(Catalog {
        function,
        name,
        revisions,
    })
}

fn problem_text(problem: &ReferenceProblem, revision_number: impl Fn(RevisionId) -> Option<u32>) -> String {
    match problem {
        ReferenceProblem::MissingFunction { .. } => {
            "The upstream function no longer exists. Choose another source or write the input.".into()
        }
        ReferenceProblem::FunctionInTrash { .. } => {
            "The upstream function is in the trash. Restore it to refresh this input.".into()
        }
        ReferenceProblem::NoRevisions { .. } => {
            "The upstream function has no saved revision yet; its output can be used once it has one.".into()
        }
        ReferenceProblem::MissingRevision { revision, .. } => format!(
            "The pinned revision{} of the upstream function no longer exists.",
            revision_number(*revision)
                .map(|n| format!(" {n}"))
                .unwrap_or_default()
        ),
        ReferenceProblem::RevisionOfOtherFunction { .. } => {
            "The pinned revision belongs to a different function.".into()
        }
        ReferenceProblem::MissingExample { .. } => {
            "The upstream revision has no such example (it was removed). Choose another example.".into()
        }
    }
}

fn check(store: &Store, doc: &FunctionDocument) -> Result<DocumentChecks, StoreError> {
    let problems = store.check_document_references(doc)?;
    let edges = store.reference_edges()?;
    let mut out = DocumentChecks::default();
    let mut revisions: HashMap<FunctionId, Vec<clojurecraft_store::RevisionSummary>> = HashMap::new();
    for (example, reference) in doc.output_references() {
        let history = match revisions.get(&reference.function) {
            Some(h) => h.clone(),
            None => {
                let h = store.revisions(reference.function).unwrap_or_default();
                revisions.insert(reference.function, h.clone());
                h
            }
        };
        let number = |id: RevisionId| history.iter().find(|r| r.id == id).map(|r| r.number);
        let snapshot_number = doc
            .example(example)
            .and_then(|e| e.input.snapshot())
            .and_then(|s| number(s.revision));
        let mut messages: Vec<String> = problems
            .iter()
            .filter(|p| problem_example(p) == example)
            .map(|p| problem_text(p, number))
            .collect();
        let graph = references::graph_with(&edges, doc, None);
        if let Some(path) = references::find_cycle(&graph, doc.id, reference.function) {
            messages.push(format!(
                "{} ({}).",
                ReferenceError::Cycle(path.clone())
                    .to_string()
                    .trim_end_matches('.'),
                references::describe_cycle(&path, |f| {
                    if f == doc.id {
                        doc.name.clone()
                    } else {
                        function_name(store, f).unwrap_or_else(|| "a deleted function".into())
                    }
                })
            ));
        }
        out.examples.insert(
            example,
            ReferenceCheck {
                upstream_name: function_name(store, reference.function),
                latest: history.first().map(|r| (r.id, r.number)),
                snapshot_number,
                problems: messages,
            },
        );
    }
    Ok(out)
}

fn problem_example(problem: &ReferenceProblem) -> ExampleId {
    match problem {
        ReferenceProblem::MissingFunction { example, .. }
        | ReferenceProblem::FunctionInTrash { example, .. }
        | ReferenceProblem::NoRevisions { example, .. }
        | ReferenceProblem::MissingRevision { example, .. }
        | ReferenceProblem::RevisionOfOtherFunction { example, .. }
        | ReferenceProblem::MissingExample { example, .. } => *example,
    }
}

fn plan(
    store: &Store,
    doc: &FunctionDocument,
    example: ExampleId,
    source: &OutputReference,
) -> Result<UpstreamPlan, String> {
    let problems = store.check_document_references(doc).map_err(|e| e.to_string())?;
    let history = store.revisions(source.function).map_err(|e| e.to_string())?;
    let number = |id: RevisionId| history.iter().find(|r| r.id == id).map(|r| r.number);
    if let Some(p) = problems.iter().find(|p| problem_example(p) == example) {
        return Err(problem_text(p, number));
    }
    let summary = match source.revision {
        RevisionPin::Latest => history.first().cloned(),
        RevisionPin::Pinned(id) => history.iter().find(|r| r.id == id).cloned(),
    }
    .ok_or_else(|| "The upstream revision could not be found.".to_owned())?;
    let revision = store.load_revision(summary.id).map_err(|e| e.to_string())?;
    let edges = store.reference_edges().map_err(|e| e.to_string())?;
    references::plan(
        doc,
        example,
        source,
        &Upstream {
            revision: summary.id,
            number: summary.number,
            document: &revision.document,
        },
        &edges,
    )
    .map_err(|e| match &e {
        ReferenceError::Cycle(path) => format!(
            "{} ({})",
            e,
            references::describe_cycle(path, |f| {
                if f == doc.id {
                    doc.name.clone()
                } else {
                    function_name(store, f).unwrap_or_else(|| "a deleted function".into())
                }
            })
        ),
        _ => e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clojurecraft_clojure::{CallResult, CallStatus, EvalOutcome, EvalStatus, HarnessReport};
    use clojurecraft_domain::{Edn, Example, ExecutionEnvironment, Implementation};
    use clojurecraft_store::Draft;

    use super::*;
    use crate::session::OpenDocument;

    fn state() -> ReferencesState {
        let (tx, _rx) = channel();
        ReferencesState::new(tx, egui::Context::default(), Arc::new(SecretRegistry::new()))
    }

    fn finished(actual: &str) -> Execution {
        Execution {
            outcome: Ok(EvalOutcome {
                status: EvalStatus::Completed,
                report: HarnessReport {
                    read: Some(Ok(())),
                    check: Some(Ok(())),
                    load: Some(Ok(())),
                    results: vec![CallResult {
                        index: 0,
                        status: CallStatus::Ok,
                        actual: Some(actual.into()),
                        truncated: false,
                        matches: None,
                        difference: None,
                        contract: Vec::new(),
                        out: String::new(),
                        elapsed_ns: 0,
                        error: None,
                        message: None,
                    }],
                    done: true,
                    ..HarnessReport::default()
                },
                stderr: String::new(),
                elapsed: Duration::ZERO,
            }),
            environment: ExecutionEnvironment {
                clojure_version: String::new(),
                java_version: String::new(),
                platform: String::new(),
            },
        }
    }

    /// An open downstream document whose first example is written input, and a planned run that
    /// would make it take the upstream example's output.
    fn setup() -> (HashMap<FunctionId, OpenDocument>, FunctionId, Run) {
        let mut up = FunctionDocument::new("up");
        up.examples
            .push(Example::direct("e", Edn::new("[1 2]"), Edn::new("[1 2]")));
        up.implementation = Some(Implementation::manual("(ns up.core)\n(defn up [x] x)"));
        let mut down = FunctionDocument::new("down");
        let example = down.add_example("typed");
        if let Some(e) = down.example_mut(example) {
            e.expected = Edn::new("2");
        }
        let source = OutputReference {
            function: up.id,
            revision: RevisionPin::Latest,
            example: up.examples[0].id,
        };
        let mut prospective = down.clone();
        prospective.set_input_reference(example, source.clone());
        let plan = references::plan(
            &prospective,
            example,
            &source,
            &Upstream {
                revision: RevisionId::generate(),
                number: 1,
                document: &up,
            },
            &[],
        )
        .map(Box::new)
        .map_err(|e| e.to_string());
        let run = Run {
            function: down.id,
            before: down.examples[0].input.clone(),
            plan: plan.unwrap_or_else(|e| panic!("{e}")),
        };
        let id = down.id;
        let mut docs = HashMap::new();
        docs.insert(
            id,
            OpenDocument::new(Draft {
                document: down,
                seq: 0,
                base_revision: None,
                updated_at: Timestamp(0),
                has_unrevisioned_changes: true,
            }),
        );
        (docs, id, run)
    }

    #[test]
    fn a_finished_upstream_run_becomes_the_input_only_if_the_example_is_unchanged() {
        let mut s = state();
        let (mut docs, function, run) = setup();
        let example = run.plan.example;
        s.runs.insert(7, run);
        let mut executed = vec![(7, finished("[1 2]")), (8, finished("[0]"))];
        let changed = s.claim(&mut executed, &mut docs);
        assert_eq!(changed, vec![function]);
        assert_eq!(
            executed.len(),
            1,
            "runs that are not upstream runs are left for others"
        );
        let input = &docs[&function].doc.examples[0].input;
        assert_eq!(input.snapshot().map(|v| v.value.as_str()), Some("[1 2]"));
        assert!(matches!(
            s.last_result(function, example),
            Some(LastResult::Recorded { .. })
        ));

        // The user typed into the example while the upstream example ran.
        let (mut docs, function, run) = setup();
        if let Some(e) = docs.get_mut(&function).and_then(|o| o.doc.examples.first_mut()) {
            e.input = ExampleInput::Direct {
                value: Edn::new("[5]"),
            };
        }
        s.runs.insert(9, run);
        let mut executed = vec![(9, finished("[1 2]"))];
        assert!(s.claim(&mut executed, &mut docs).is_empty());
        assert_eq!(
            docs[&function].doc.examples[0].input,
            ExampleInput::Direct {
                value: Edn::new("[5]")
            }
        );
        assert_eq!(s.notices.len(), 1);
    }

    #[test]
    fn a_failed_upstream_run_records_nothing_and_keeps_the_reason() {
        let mut s = state();
        let (mut docs, function, run) = setup();
        let example = run.plan.example;
        s.runs.insert(1, run);
        let mut failed = finished("");
        if let Ok(o) = &mut failed.outcome {
            o.report.results[0].status = CallStatus::Timeout;
        }
        let mut executed = vec![(1, failed)];
        assert!(s.claim(&mut executed, &mut docs).is_empty());
        assert!(docs[&function].doc.examples[0].input.reference().is_none());
        assert!(matches!(
            s.last_result(function, example),
            Some(LastResult::Failed(m)) if m.contains("did not finish")
        ));
    }
}
