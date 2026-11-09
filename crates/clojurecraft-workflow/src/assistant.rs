//! Model work on a function's specification: interpreting it, proposing a contract, and
//! generating an implementation that is validated and, when it fails, repaired.
//!
//! One job runs at a time. A job is a sequence of model requests (and, for generation, a
//! documentation lookup and sandboxed validation runs); each response is matched by its request
//! number, so a cancelled or superseded request can never change a function. Results change the
//! function only in ways that keep the user in control:
//!
//! - an interpretation is stored next to the user's text, and its questions stay open until the
//!   user answers them;
//! - a contract proposal is stored as proposed, and never replaces an accepted contract;
//! - generated code is validated (see [`crate::validation`]). Failing code starts a
//!   conversation: the model's answer stays in the history, the reply shows the checked code and
//!   its failures and asks the model to find out first why the first failing example failed, and
//!   the model answers with an analysis and only the forms it changes (see [`crate::edits`]). At
//!   most [`MAX_FIXES`] fixes follow a generation. A conversation that is stuck (a fix that
//!   returns earlier code, or fails exactly like the code before it) starts over with a fresh
//!   generation and an empty history. The first passing code ends the loop;
//! - before checking, closing delimiters that do not match are inferred from the indentation
//!   (see [`crate::delimiters`]) and harmless extras are removed (see [`crate::cleanup`]); both
//!   are noted on the attempt;
//! - passing code replaces the current implementation only when that is absent or exactly what
//!   the model generated earlier, when it uses no namespace the user has not allowed, and when
//!   the specification did not change while the job ran; otherwise it is held for review;
//! - code that still fails after the last attempt is never applied automatically; the attempts
//!   and their diagnostics are shown, and the user may still choose a candidate;
//! - code accepted for edge-case testing is never replaced without review, and a fix for failing
//!   cases (the same loop, starting from the current code and its failures) is always shown
//!   for review;
//! - proposed edge cases are added as unaccepted proposals (see [`crate::edge_cases`]).
//!
//! Every attempt's validation report (with the code it checked) is handed to the application to
//! be stored, so all candidates remain inspectable. Examples, contracts, and accepted tests are
//! read from a snapshot taken when the job starts and are never changed by it.
//!
//! Inference, retrieval, and execution are reached through [`Ports`], so the sequencing is
//! testable without a model or a Java runtime.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::time::Instant;

use crate::answers::{self, InterpretationAnswer};
use crate::requests::{self, DOCUMENTATION_DROP_ORDER};
use crate::validation::{self, Plan, ReportContext};
use crate::{Candidate, GenerationInput, Names, Provenance, deps, readiness};
use crate::{cleanup, delimiters, describe, edge_cases, edits};
use clojurecraft_clojure::{ClojureRuntime, EvalOutcome, EvalRequest};
use clojurecraft_domain::{
    DocumentationRef, ExecutionEnvironment, Fingerprint, FunctionDocument, FunctionId, GenerationRunId,
    GenerationSettings, InferenceRuntime, Interpretation, ModelIdentity, PromptMessage, RevisionKind,
    RevisionReason, Step, Timestamp, ValidationReport,
};
use clojurecraft_inference::{
    GenerateRequest, Generation, GenerationError, Remedy, RequestId, Role, ServiceEvent, StreamEvent,
};
use clojurecraft_knowledge::{CollectionInfo, Embedder, Hit, Query, Retriever, Semantic};

/// Documentation excerpts given to the generator at most.
pub const MAX_EXCERPTS: usize = 10;

/// Project definitions shown with a request at most.
pub const MAX_PROJECT_EXCERPTS: usize = 6;

/// Extra answer budget for models that reason before answering. Their thinking counts against
/// the answer's token limit; the service still caps the total at half the context.
pub const REASONING_ALLOWANCE: u32 = 6144;

/// Fixes after the first generation, each continuing the conversation about the failing code.
pub const MAX_FIXES: u32 = 5;

/// Model answers of a generation at most: the generation and its fixes.
pub const MAX_ATTEMPTS: u32 = 1 + MAX_FIXES;

/// Characters of failure diagnostics sent with a repair request.
const MAX_FAILURE_CHARS: usize = 6000;

/// Documentation and project code to look up for a generation or a description.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gather {
    /// Query text and how many of its results to keep.
    pub queries: Vec<(String, usize)>,
    pub clojure_version: Option<String>,
    /// Symbols to check for being `clojure.core` vars.
    pub core_candidates: Vec<String>,
    /// The namespace of a project function: its project code is searched too, leaving out the
    /// namespace itself (its definitions are shown anyway) and, unless `include_restricted`,
    /// the namespaces that depend on it, which it cannot require.
    pub project_namespace: Option<String>,
    /// Also return project definitions generated code may not call, for describing code.
    pub include_restricted: bool,
}

pub struct Gathered {
    pub hits: Vec<Hit>,
    /// The documentation collection searched; `None` when none is installed.
    pub info: Option<CollectionInfo>,
    pub collection_version: String,
    /// The candidates that name `clojure.core` vars.
    pub core: BTreeSet<String>,
    /// Definitions found in the project index.
    pub project: Vec<Hit>,
}

/// The project index to search, with the namespace graph its code was indexed with.
pub struct ProjectSearch<'a> {
    pub retriever: &'a mut Retriever,
    pub graph: &'a BTreeMap<String, BTreeSet<String>>,
}

/// Runs `queries` against one index, each contributing up to its own count of results not
/// already found and accepted by `keep`, at most `max` in total. Returns whether semantic search
/// failed.
fn search_all(
    retriever: &mut Retriever,
    embedder: &mut Option<&mut dyn Embedder>,
    g: &Gather,
    max: usize,
    keep: &dyn Fn(&Hit) -> bool,
    hits: &mut Vec<Hit>,
) -> Result<bool, String> {
    let mut semantic_failed = false;
    for (text, take) in &g.queries {
        if hits.len() >= max || text.trim().is_empty() {
            continue;
        }
        let mut q = Query::new(text.clone());
        q.limit = (*take).max(1) + 4;
        q.clojure_version.clone_from(&g.clojure_version);
        q.include_restricted = g.include_restricted;
        let embed = if semantic_failed {
            None
        } else {
            // Reborrowed with a shorter trait-object lifetime for this one search.
            embedder.as_mut().map(|e| &mut **e as &mut dyn Embedder)
        };
        let res = retriever.search(&q, embed).map_err(|e| e.to_string())?;
        semantic_failed |= matches!(res.semantic, Semantic::Failed(_));
        let fresh: Vec<Hit> = res
            .hits
            .into_iter()
            .filter(|h| keep(h) && !hits.iter().any(|x| x.chunk.id == h.chunk.id))
            .take(*take)
            .collect();
        hits.extend(fresh);
    }
    hits.truncate(max);
    Ok(semantic_failed)
}

/// Runs a [`Gather`] against the open documentation collection (`docs`, with its version) and
/// project index, either of which may be missing: at most [`MAX_EXCERPTS`] documentation
/// excerpts and [`MAX_PROJECT_EXCERPTS`] project definitions. Returns whether semantic search
/// failed, in which case the caller should restart its embedding model.
pub fn gather(
    docs: Option<(&mut Retriever, &str)>,
    project: Option<ProjectSearch<'_>>,
    mut embedder: Option<&mut dyn Embedder>,
    g: &Gather,
) -> Result<(Gathered, bool), String> {
    let mut semantic_failed = false;
    let mut found = Gathered {
        hits: Vec::new(),
        info: None,
        collection_version: String::new(),
        core: BTreeSet::new(),
        project: Vec::new(),
    };
    if let Some((retriever, version)) = docs {
        let mut hits = Vec::new();
        semantic_failed |= search_all(retriever, &mut embedder, g, MAX_EXCERPTS, &|_| true, &mut hits)?;
        found.core = g
            .core_candidates
            .iter()
            .filter(|c| {
                retriever.lookup(&format!("clojure.core/{c}")).is_ok_and(|found| {
                    found.iter().any(|h| {
                        h.chunk.namespace.as_deref() == Some("clojure.core")
                            && h.chunk.symbol.as_deref() == Some(c.as_str())
                    })
                })
            })
            .cloned()
            .collect();
        found.hits = hits;
        found.info = Some(retriever.info().clone());
        version.clone_into(&mut found.collection_version);
    }
    if let (Some(p), Some(namespace)) = (project, &g.project_namespace) {
        let blocked = if g.include_restricted {
            BTreeSet::from([namespace.clone()])
        } else {
            crate::project_index::dependents(p.graph, namespace)
        };
        let keep = |h: &Hit| h.chunk.namespace.as_ref().is_none_or(|ns| !blocked.contains(ns));
        let mut hits = Vec::new();
        semantic_failed |= search_all(
            p.retriever,
            &mut embedder,
            g,
            MAX_PROJECT_EXCERPTS,
            &keep,
            &mut hits,
        )?;
        found.project = hits;
    }
    Ok((found, semantic_failed))
}

/// The result of running code in the sandboxed runtime, with the environment it ran in.
#[derive(Clone, Debug)]
pub struct Execution {
    /// The harness outcome, or why the code could not be run at all.
    pub outcome: Result<EvalOutcome, String>,
    pub environment: ExecutionEnvironment,
}

/// How the assistant reaches inference, documentation, and execution.
pub trait Ports {
    /// Starts a model request.
    fn generate(&mut self, request: GenerateRequest) -> Result<RequestId, String>;
    fn cancel(&mut self, request: RequestId);
    /// Starts a documentation lookup; `None` when no documentation is installed.
    fn gather(&mut self, gather: Gather) -> Option<u64>;
    /// Starts running code in the sandboxed runtime; the result arrives through
    /// [`Assistant::on_validated`]. `Err` explains why nothing can be run (for example, no Java
    /// runtime is installed).
    fn validate(&mut self, request: EvalRequest) -> Result<u64, String>;
    fn cancel_validation(&mut self, id: u64);
    /// The sandboxed runtime, for the tool that lets the model evaluate expressions while it
    /// analyses failing code; `None` when code cannot be run.
    fn runtime(&self) -> Option<ClojureRuntime> {
        None
    }
}

/// The open functions the assistant may change.
pub trait Documents {
    fn document(&mut self, id: FunctionId) -> Option<&mut FunctionDocument>;
}

impl Documents for HashMap<FunctionId, FunctionDocument> {
    fn document(&mut self, id: FunctionId) -> Option<&mut FunctionDocument> {
        self.get_mut(&id)
    }
}

/// What the application should do after the assistant changed a function.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// Save the draft.
    Edited(FunctionId),
    /// Save the draft and record it as a revision.
    Commit(FunctionId, RevisionReason),
    /// Store a validation report.
    Validated(FunctionId, Box<ValidationReport>),
}

/// The loaded model and runtime, captured when a generation starts.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelContext {
    pub label: String,
    pub model: ModelIdentity,
    pub runtime: InferenceRuntime,
    pub context_length: u32,
    pub clojure_version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Interpret,
    ProposeContract,
    Generate,
    ProposeTests,
    Explain,
    /// Writes the specification of existing code and records example outputs by running it.
    Describe,
}

impl JobKind {
    /// What the job does, as in "Could not {label}"; a failed job's report names it so.
    pub fn label(self) -> &'static str {
        match self {
            Self::Interpret => "check the specification",
            Self::ProposeContract => "propose a contract",
            Self::Generate => "generate the implementation",
            Self::ProposeTests => "propose edge cases",
            Self::Explain => "explain the code",
            Self::Describe => "describe the code",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Interpreting,
    Asking,
    Proposing,
    Planning,
    Retrieving,
    Generating,
    Validating,
    /// Finding out why the code failed, before fixing it.
    Analyzing,
    Repairing,
    Explaining,
    ProposingTests,
    Describing,
    /// Running existing code on the described example inputs.
    Recording,
    /// Correcting the description against what the code returned.
    Revising,
    /// Running the current and the new code on variants of the examples.
    Comparing,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Interpreting => "Reading the specification",
            Self::Asking => "Writing questions",
            Self::Proposing => "Proposing a contract",
            Self::Planning => "Choosing documentation",
            Self::Retrieving => "Looking up documentation",
            Self::Generating => "Writing the code",
            Self::Validating => "Checking the code",
            Self::Analyzing => "Finding the cause",
            Self::Repairing => "Fixing the code",
            Self::Explaining => "Explaining the code",
            Self::ProposingTests => "Proposing edge cases",
            Self::Describing => "Describing the code",
            Self::Recording => "Running the examples",
            Self::Revising => "Checking the description",
            Self::Comparing => "Comparing with the current code",
        }
    }
}

/// What an attempt asked the model for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    /// Code written from the specification.
    Generate,
    /// Code written afresh because the conversation about failing code got stuck.
    Restart,
    /// The n-th fix of failing code in this job.
    Fix(u32),
}

/// One model answer of a generation and how it fared.
#[derive(Clone, Debug, PartialEq)]
pub struct Attempt {
    /// The answer's position in the job, from 1.
    pub number: u32,
    pub kind: AttemptKind,
    /// The code, when the model produced usable code.
    pub code: Option<String>,
    /// The validation report, when the code was checked.
    pub report: Option<ValidationReport>,
    /// Why the attempt has no report, or other remarks.
    pub note: Option<String>,
}

impl Attempt {
    pub fn passed(&self) -> bool {
        self.report.as_ref().is_some_and(ValidationReport::passed)
    }

    /// How the attempt is named: "Generate", "Attempt 2", or "Fresh start".
    pub fn label(&self) -> String {
        self.kind.label()
    }
}

impl AttemptKind {
    pub fn label(self) -> String {
        match self {
            Self::Generate => "Generate".into(),
            Self::Restart => "Fresh start".into(),
            Self::Fix(n) => format!("Attempt {n}"),
        }
    }

    /// The sampling temperature this kind of attempt runs with.
    pub fn temperature(self) -> f32 {
        match self {
            Self::Generate => Role::Generation.profile().temperature,
            Self::Restart => requests::RESTART_TEMPERATURE,
            Self::Fix(_) => requests::FIX_TEMPERATURE,
        }
    }
}

struct GenState {
    model: ModelContext,
    names: Option<Names>,
    allowed: BTreeSet<String>,
    documentation: Vec<DocumentationRef>,
    docs_section: Option<clojurecraft_inference::Section>,
    /// Definitions of the project that may be reused.
    project_section: Option<clojurecraft_inference::Section>,
    seed: u64,
    omitted: Vec<String>,
    run: GenerationRunId,
    /// The specification as it was when the job started; every attempt works from it.
    spec: FunctionDocument,
    /// The attempt in progress, from 1.
    attempt: u32,
    attempts: Vec<Attempt>,
    /// The candidate of the attempt in progress and its validation plan.
    candidate: Option<Candidate>,
    plan: Option<Plan>,
    /// For a fix: the current code and the report of its failing checks, which the first
    /// attempt fixes instead of writing code afresh.
    start: Option<(String, ValidationReport)>,
    /// What the attempt in progress asks for.
    kind: AttemptKind,
    /// The conversation about failing code so far; empty before the first fix.
    turns: Vec<requests::Turn>,
    /// The model's last usable answer, as given, for the conversation history.
    answer: Option<String>,
    /// The code the fix in progress changes.
    base: Option<String>,
    /// The namespace and function of the code being fixed.
    start_names: Option<Names>,
    /// What was changed in the current candidate before it was checked.
    prepared: Vec<String>,
    /// The last generation request as sent; empty for a job that only fixes existing code.
    prompt: Vec<PromptMessage>,
}

impl GenState {
    fn seed_now(&self) -> u64 {
        self.seed.wrapping_add(u64::from(self.attempt - 1))
    }

    /// Attempts this generation may make: a fix of the current code starts with a fix, so it
    /// has no generation.
    fn max_attempts(&self) -> u32 {
        if self.start.is_some() {
            MAX_FIXES
        } else {
            MAX_ATTEMPTS
        }
    }

    /// The kind of the next fix: fixes are numbered through the whole job.
    fn next_fix(&self) -> AttemptKind {
        let done = self
            .attempts
            .iter()
            .filter(|a| matches!(a.kind, AttemptKind::Fix(_)))
            .count();
        AttemptKind::Fix(u32::try_from(done).unwrap_or(u32::MAX) + 1)
    }

    fn input(&self) -> GenerationInput<'_> {
        GenerationInput {
            doc: &self.spec,
            names: self.names.as_ref().expect("names are set before code is written"),
            allowed: &self.allowed,
            clojure_version: &self.model.clojure_version,
            documentation: self.docs_section.clone(),
            project_code: self.project_section.clone(),
            seed: self.seed_now(),
        }
    }
}

pub struct Job {
    pub function: FunctionId,
    pub kind: JobKind,
    pub phase: Phase,
    pub started: Instant,
    /// Streamed answer text of the current request.
    pub output: String,
    /// Streamed reasoning text of the current request.
    pub reasoning: String,
    request: Option<RequestId>,
    /// The request last sent, before any reasoning allowance, kept to send it again.
    last_request: Option<GenerateRequest>,
    gather: Option<u64>,
    validation: Option<u64>,
    spec_basis: Fingerprint,
    notes: Vec<String>,
    interpretation: Option<InterpretationAnswer>,
    generation: Option<GenState>,
    /// For an explanation: the code explained and the steps numbered in the request.
    explaining: Option<(String, Vec<Step>)>,
    describing: Option<Describing>,
    comparing: Option<Comparing>,
    /// Proposed cases being run with the current code, and the report that ends the job.
    annotating: Option<(Vec<clojurecraft_domain::TestCaseId>, Report)>,
}

/// Passing code held for review while it is compared with the code it would replace.
struct Comparing {
    pending: Box<Pending>,
    probes: Vec<String>,
    /// What the current code did on the probes, once run.
    current: Option<EvalOutcome>,
}

/// A description in progress: the code described and the model's answer.
struct Describing {
    /// The function as it was when the job started.
    spec: FunctionDocument,
    code: String,
    answer: Option<describe::DescriptionAnswer>,
    /// The recorded examples, kept while the description is revised against them.
    recorded: Option<Vec<clojurecraft_domain::Example>>,
    /// The parts of the specification filled so far.
    filled: Vec<String>,
    /// The project code found for the request, kept for a retry.
    project_code: Option<clojurecraft_inference::Section>,
    /// Whether new inputs were already asked for because none could be recorded.
    retried: bool,
    /// The user's description is kept and only checked against the results.
    auditing: bool,
}

impl Job {
    /// The attempt in progress and the maximum, for a generation.
    pub fn attempt(&self) -> Option<(u32, u32)> {
        self.generation.as_ref().map(|g| (g.attempt, g.max_attempts()))
    }

    /// What the attempt in progress asks for, for a generation.
    pub fn attempt_kind(&self) -> Option<AttemptKind> {
        self.generation.as_ref().map(|g| g.kind)
    }

    /// The attempt in progress, named as the attempts list names it, with how many model
    /// answers remain after it: "Attempt 2 (3 more at most)".
    pub fn progress(&self) -> Option<String> {
        let g = self.generation.as_ref()?;
        let left = g.max_attempts().saturating_sub(g.attempt);
        Some(match left {
            0 => format!("{}, the last", g.kind.label()),
            n => format!("{} ({n} more at most)", g.kind.label()),
        })
    }

    /// Attempts finished so far in this generation.
    pub fn attempts(&self) -> &[Attempt] {
        self.generation.as_ref().map_or(&[], |g| g.attempts.as_slice())
    }

    /// Whether this generation fixes the current code for failing checks.
    pub fn fixing(&self) -> bool {
        self.generation.as_ref().is_some_and(|g| g.start.is_some())
    }
}

/// Generated code waiting for the user.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub candidate: Candidate,
    pub names: Names,
    pub provenance: Provenance,
    /// Supported namespaces the code uses that the function does not allow yet.
    pub needs_approval: Vec<String>,
    /// The current implementation was written or edited by hand.
    pub replaces_manual: bool,
    /// The current implementation is the code accepted for edge-case testing.
    pub replaces_accepted: bool,
    /// A fix of the current code for failing checks; always reviewed before it is used.
    pub fix: bool,
    /// The specification or accepted tests changed while the code was generated and checked.
    pub spec_changed: bool,
    /// The candidate's validation report; `None` when it could not be run.
    pub validation: Option<ValidationReport>,
    pub attempts: Vec<Attempt>,
    pub notes: Vec<String>,
    /// Inputs outside the examples and tests on which the code behaves differently from the
    /// code it would replace.
    pub differences: Vec<String>,
}

impl Pending {
    /// Whether the candidate passed validation.
    pub fn passed(&self) -> bool {
        self.validation.as_ref().is_some_and(ValidationReport::passed)
    }
}

/// How the last job for a function ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    Interpreted {
        questions: usize,
        notes: Vec<String>,
    },
    ContractProposed {
        notes: Vec<String>,
    },
    /// Edge cases were proposed and added for review.
    TestsProposed {
        added: usize,
        ambiguous: usize,
        notes: Vec<String>,
    },
    /// The code's explanation was stored.
    Explained {
        notes: Vec<String>,
    },
    /// The specification of existing code was written: `filled` names the parts filled.
    Described {
        filled: Vec<String>,
        notes: Vec<String>,
    },
    /// Code was applied to the function.
    Generated {
        notes: Vec<String>,
        attempts: Vec<Attempt>,
    },
    /// Code is waiting for the user's review.
    Pending(Box<Pending>),
    /// Every attempt failed validation. The best candidate whose structure is acceptable, if
    /// any, can still be chosen.
    Unresolved {
        attempts: Vec<Attempt>,
        best: Option<Box<Pending>>,
        notes: Vec<String>,
    },
    Failed {
        what: &'static str,
        message: String,
        remedies: Vec<Remedy>,
        /// The model's answer, when it was malformed.
        raw: Option<String>,
    },
    /// The specification is not complete enough to start.
    Blocked(Vec<String>),
    Cancelled,
}

impl Report {
    /// The attempts of a generation, for any report that has them.
    pub fn attempts(&self) -> &[Attempt] {
        match self {
            Self::Generated { attempts, .. } | Self::Unresolved { attempts, .. } => attempts,
            Self::Pending(p) => &p.attempts,
            _ => &[],
        }
    }
}

#[derive(Default)]
pub struct Assistant {
    job: Option<Job>,
    reports: HashMap<FunctionId, Report>,
    /// The loaded model reasons before answering, so requests get [`REASONING_ALLOWANCE`].
    model_reasons: bool,
}

/// Why the current code cannot be explained: only unedited generated code can.
pub fn explain_blocker(doc: &FunctionDocument) -> Option<&'static str> {
    match &doc.implementation {
        Some(i) if i.source.trim().is_empty() => Some("There is no code to explain yet."),
        Some(i) if i.generation.is_none() => Some("Only generated code can be explained."),
        Some(i) if i.is_manually_edited() => {
            Some("The code was edited by hand; only unedited generated code can be explained.")
        }
        Some(_) => None,
        None => Some("Generate the code first."),
    }
}

fn random_seed() -> u64 {
    // RandomState is seeded randomly per instance.
    RandomState::new().hash_one(Instant::now())
}

/// The provenance of a project definition shown to the model.
fn project_ref(h: &Hit) -> DocumentationRef {
    DocumentationRef {
        source_url: h.chunk.url.clone(),
        title: h.chunk.title.clone(),
        symbol: h.chunk.symbol.clone(),
        namespace: h.chunk.namespace.clone(),
        clojure_version: String::new(),
        collection_revision: clojurecraft_knowledge::project::PROJECT_REVISION.to_owned(),
    }
}

fn job_label(kind: JobKind) -> &'static str {
    kind.label()
}

impl Assistant {
    pub fn job(&self) -> Option<&Job> {
        self.job.as_ref()
    }

    pub fn busy_with(&self, function: FunctionId) -> Option<&Job> {
        self.job.as_ref().filter(|j| j.function == function)
    }

    pub fn report(&self, function: FunctionId) -> Option<&Report> {
        self.reports.get(&function)
    }

    pub fn dismiss_report(&mut self, function: FunctionId) {
        self.reports.remove(&function);
    }

    fn begin(&mut self, doc: &FunctionDocument, kind: JobKind, phase: Phase) -> Result<(), ()> {
        if self.job.is_some() {
            return Err(());
        }
        let r = readiness(doc);
        if !r.is_ready() {
            self.reports.insert(doc.id, Report::Blocked(r.blocking));
            return Err(());
        }
        self.start(doc, kind, phase, r.notes);
        Ok(())
    }

    /// Starts a job without checking the specification.
    fn start(&mut self, doc: &FunctionDocument, kind: JobKind, phase: Phase, notes: Vec<String>) {
        self.reports.remove(&doc.id);
        self.job = Some(Job {
            function: doc.id,
            kind,
            phase,
            started: Instant::now(),
            output: String::new(),
            reasoning: String::new(),
            request: None,
            last_request: None,
            gather: None,
            validation: None,
            spec_basis: doc.spec_fingerprint(),
            notes,
            interpretation: None,
            generation: None,
            explaining: None,
            describing: None,
            comparing: None,
            annotating: None,
        });
    }

    /// Sends `request` for the current job, ending the job if it cannot be sent.
    fn send(&mut self, request: GenerateRequest, phase: Phase, ports: &mut dyn Ports) {
        let Some(job) = &mut self.job else { return };
        job.phase = phase;
        job.output.clear();
        job.reasoning.clear();
        job.last_request = Some(request.clone());
        let mut request = request;
        if self.model_reasons {
            let base = request
                .max_output_tokens
                .unwrap_or(request.role.profile().max_output_tokens);
            request.max_output_tokens = Some(base + REASONING_ALLOWANCE);
            // llama.cpp applies a plain grammar from the first token, which keeps a reasoning
            // model from thinking at all; the schema's grammar only starts after the reasoning.
            request.grammar = None;
        }
        match ports.generate(request) {
            Ok(id) => job.request = Some(id),
            Err(message) => {
                let what = job_label(job.kind);
                self.end(Report::Failed {
                    what,
                    message,
                    remedies: Vec::new(),
                    raw: None,
                });
            }
        }
    }

    fn end(&mut self, report: Report) {
        if let Some(job) = self.job.take() {
            self.reports.insert(job.function, report);
        }
    }

    pub fn interpret(&mut self, doc: &FunctionDocument, ports: &mut dyn Ports) {
        if self.begin(doc, JobKind::Interpret, Phase::Interpreting).is_ok() {
            self.send(requests::interpretation_request(doc), Phase::Interpreting, ports);
        }
    }

    pub fn propose_contract(&mut self, doc: &FunctionDocument, ports: &mut dyn Ports) {
        if self
            .begin(doc, JobKind::ProposeContract, Phase::Proposing)
            .is_ok()
        {
            self.send(requests::contract_request(doc), Phase::Proposing, ports);
        }
    }

    /// Proposes edge cases from the specification. They are added to the function as
    /// unaccepted proposals.
    pub fn propose_tests(&mut self, doc: &FunctionDocument, ports: &mut dyn Ports) {
        if self
            .begin(doc, JobKind::ProposeTests, Phase::ProposingTests)
            .is_ok()
        {
            self.send(edge_cases::proposal_request(doc), Phase::ProposingTests, ports);
        }
    }

    /// Generates edge cases from the title, description, and examples only (no steps, contract,
    /// answers, accepted tests, or code). They are added as unaccepted proposals, like
    /// [`Self::propose_tests`]; no accepted or passing code is needed.
    pub fn generate_tests_from_examples(&mut self, doc: &FunctionDocument, ports: &mut dyn Ports) {
        if let Some(why) = edge_cases::examples_blocker(doc) {
            self.reports.insert(doc.id, Report::Blocked(vec![why.to_owned()]));
            return;
        }
        if self
            .begin(doc, JobKind::ProposeTests, Phase::ProposingTests)
            .is_ok()
        {
            self.send(
                edge_cases::example_proposal_request(doc),
                Phase::ProposingTests,
                ports,
            );
        }
    }

    /// Explains the current generated code: which code carries out each step, and the decisions
    /// it makes where the specification is silent. The explanation is stored with the code.
    pub fn explain(&mut self, doc: &FunctionDocument, ports: &mut dyn Ports) {
        if let Some(why) = explain_blocker(doc) {
            self.reports.insert(doc.id, Report::Blocked(vec![why.to_owned()]));
            return;
        }
        let code = doc
            .implementation
            .as_ref()
            .map(|i| i.source.clone())
            .unwrap_or_default();
        if self.begin(doc, JobKind::Explain, Phase::Explaining).is_ok() {
            if let Some(job) = &mut self.job {
                let steps = requests::numbered_steps(doc).into_iter().cloned().collect();
                job.explaining = Some((code.clone(), steps));
            }
            self.send(
                requests::explanation_request(doc, &code),
                Phase::Explaining,
                ports,
            );
        }
    }

    /// Writes the specification of the current code, which may be hand-written: the model
    /// describes it and proposes example inputs, the code is run to record their outputs, and a
    /// contract is proposed from the result. Only empty parts of the specification are filled;
    /// examples are added. The specification need not be complete to start.
    pub fn describe(&mut self, doc: &FunctionDocument, ports: &mut dyn Ports) {
        if self.job.is_some() {
            return;
        }
        if let Some(why) = describe::blocker(doc) {
            self.reports.insert(doc.id, Report::Blocked(vec![why.to_owned()]));
            return;
        }
        let code = doc
            .implementation
            .as_ref()
            .map(|i| i.source.clone())
            .unwrap_or_default();
        self.start(doc, JobKind::Describe, Phase::Describing, Vec::new());
        let Some(job) = &mut self.job else { return };
        job.describing = Some(Describing {
            code: code.clone(),
            answer: None,
            recorded: None,
            filled: Vec::new(),
            spec: doc.clone(),
            project_code: None,
            retried: false,
            auditing: false,
        });
        // What the code calls in other namespaces, so the description can say what it does.
        let calls = doc
            .project
            .as_ref()
            .map(|p| crate::project_index::external_calls(&p.file, &code))
            .unwrap_or_default();
        if let Some(p) = doc.project.as_ref().filter(|_| !calls.is_empty()) {
            let gather = Gather {
                queries: calls.into_iter().take(12).map(|c| (c, 1)).collect(),
                project_namespace: Some(p.namespace.clone()),
                include_restricted: true,
                ..Gather::default()
            };
            if let Some(id) = ports.gather(gather) {
                job.phase = Phase::Retrieving;
                job.gather = Some(id);
                return;
            }
        }
        self.send(describe::request(doc, &code, None), Phase::Describing, ports);
    }

    /// Applies the description and the recorded examples, then proposes a contract unless one is
    /// accepted.
    fn described(
        &mut self,
        examples: Vec<clojurecraft_domain::Example>,
        notes: Vec<String>,
        doc: &mut FunctionDocument,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        let Some(d) = &mut job.describing else {
            return Vec::new();
        };
        let Some(answer) = &d.answer else {
            return Vec::new();
        };
        if doc.implementation.as_ref().is_none_or(|i| i.source != d.code) {
            self.code_changed_while_described();
            return Vec::new();
        }
        // No input could be recorded: ask once more, showing what went wrong with each.
        let rejected = describe::rejected(answer, &notes);
        let retry = if d.retried {
            None
        } else if examples.is_empty() && !rejected.is_empty() {
            Some((
                format!(
                    "No example could be recorded from the first answer ({} input{} rejected), so \
                     new inputs were asked for.",
                    rejected.len(),
                    if rejected.len() == 1 { "" } else { "s" }
                ),
                describe::Retry::Rejected(rejected),
            ))
        } else {
            describe::same_output(doc, &examples).map(|r| {
                (
                    format!(
                        "All {} recorded examples of the first answer had the same output, so inputs \
                         with other results were asked for.",
                        examples.len()
                    ),
                    r,
                )
            })
        };
        if let Some((note, retry)) = retry {
            d.retried = true;
            let request = describe::retry_request(&d.spec, &d.code, d.project_code.clone(), &retry);
            job.notes = vec![note];
            self.send(request, Phase::Describing, ports);
            return Vec::new();
        }
        // The draft was written before the code ran; correct it against the results when it is
        // going to fill the specification.
        let fills = doc.description.trim().is_empty() || doc.steps.iter().all(|s| s.text.trim().is_empty());
        let evidence = describe::evidence(answer, &examples, &notes);
        job.notes.extend(notes);
        if fills && !evidence.is_empty() {
            let request = describe::revision_request(&d.code, answer, &evidence);
            d.recorded = Some(examples);
            self.send(request, Phase::Revising, ports);
            return Vec::new();
        }
        // The user's text stays, but where the results contradict it they are told.
        if !evidence.is_empty() {
            let steps: Vec<String> = doc
                .steps
                .iter()
                .map(|s| s.text.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            let request = describe::audit_request(&d.code, &doc.description, &steps, &evidence);
            d.recorded = Some(examples);
            d.auditing = true;
            self.send(request, Phase::Revising, ports);
            return Vec::new();
        }
        self.apply_description(examples, doc, ports)
    }

    fn code_changed_while_described(&mut self) {
        self.end(Report::Failed {
            what: job_label(JobKind::Describe),
            message: "the code changed while it was being described".into(),
            remedies: Vec::new(),
            raw: None,
        });
    }

    /// Applies the (revised) description and the recorded examples, then proposes a contract
    /// unless one is accepted.
    fn apply_description(
        &mut self,
        examples: Vec<clojurecraft_domain::Example>,
        doc: &mut FunctionDocument,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        let function = job.function;
        let Some(d) = &mut job.describing else {
            return Vec::new();
        };
        let Some(answer) = &d.answer else {
            return Vec::new();
        };
        if doc.implementation.as_ref().is_none_or(|i| i.source != d.code) {
            self.code_changed_while_described();
            return Vec::new();
        }
        d.filled = describe::apply(doc, answer, examples);
        let accepted = doc
            .contract
            .as_ref()
            .is_some_and(|c| c.status == clojurecraft_domain::ContractStatus::Accepted);
        if accepted {
            self.finish_description();
        } else {
            self.send(requests::contract_request(doc), Phase::Proposing, ports);
        }
        vec![Update::Edited(function)]
    }

    fn finish_description(&mut self) {
        let Some(job) = &mut self.job else { return };
        let filled = job.describing.take().map(|d| d.filled).unwrap_or_default();
        let notes = std::mem::take(&mut job.notes);
        self.end(Report::Described { filled, notes });
    }

    /// Starts a generation; `documentation` says whether a documentation collection is installed.
    pub fn generate(
        &mut self,
        doc: &FunctionDocument,
        model: ModelContext,
        documentation: bool,
        ports: &mut dyn Ports,
    ) {
        self.start_generation(doc, model, documentation, None, ports);
    }

    /// Repairs the current implementation so that it passes the checks that failed in `failing`
    /// (a report about exactly the current content), with the same attempts as a generation.
    /// The result is always held for review.
    pub fn fix_failing(
        &mut self,
        doc: &FunctionDocument,
        model: ModelContext,
        documentation: bool,
        failing: &ValidationReport,
        ports: &mut dyn Ports,
    ) {
        let Some(code) = doc.implementation.as_ref().map(|i| i.source.clone()) else {
            return;
        };
        if failing.basis != doc.basis() || failing.passed() {
            self.reports.insert(
                doc.id,
                Report::Blocked(vec![
                    "Run the checks on the current code first; a fix starts from its failures.".into(),
                ]),
            );
            return;
        }
        self.start_generation(doc, model, documentation, Some((code, failing.clone())), ports);
    }

    fn start_generation(
        &mut self,
        doc: &FunctionDocument,
        model: ModelContext,
        documentation: bool,
        start: Option<(String, ValidationReport)>,
        ports: &mut dyn Ports,
    ) {
        if self.begin(doc, JobKind::Generate, Phase::Planning).is_err() {
            return;
        }
        let start_names = start.as_ref().and_then(|_| current_names(doc));
        if let Some(job) = &mut self.job {
            job.generation = Some(GenState {
                model,
                names: None,
                allowed: deps::allowed(doc),
                documentation: Vec::new(),
                docs_section: None,
                project_section: None,
                seed: random_seed(),
                omitted: Vec::new(),
                run: GenerationRunId::generate(),
                spec: doc.clone(),
                attempt: 1,
                attempts: Vec::new(),
                candidate: None,
                plan: None,
                start,
                start_names,
                prepared: Vec::new(),
                kind: AttemptKind::Generate,
                turns: Vec::new(),
                answer: None,
                base: None,
                prompt: Vec::new(),
            });
            if !documentation {
                job.notes
                    .push("No Clojure documentation is installed, so none was used.".into());
            }
        }
        match (documentation, requests::planning_request(doc)) {
            (true, Some(plan)) => self.send(plan, Phase::Planning, ports),
            (true, None) => self.retrieve(Vec::new(), ports),
            (false, _) => self.write_code(&BTreeSet::new(), ports),
        }
    }

    pub fn cancel(&mut self, ports: &mut dyn Ports) {
        if let Some(job) = &self.job {
            if let Some(id) = job.request {
                ports.cancel(id);
            }
            if let Some(id) = job.validation {
                ports.cancel_validation(id);
            }
        }
        if let Some((_, report)) = self.job.as_mut().and_then(|j| j.annotating.take()) {
            self.end(report);
            return;
        }
        // Passing code that was being compared is still held for review.
        if let Some(c) = self.job.as_mut().and_then(|j| j.comparing.take()) {
            let mut pending = c.pending;
            pending
                .notes
                .push("The comparison with the current code was cancelled.".into());
            self.end(Report::Pending(pending));
            return;
        }
        self.end(Report::Cancelled);
    }

    fn retrieve(&mut self, planned: Vec<String>, ports: &mut dyn Ports) {
        let Some(job) = &mut self.job else { return };
        let Some(g) = &job.generation else { return };
        job.phase = Phase::Retrieving;
        job.request = None;
        let doc = &g.spec;
        let mut queries = vec![(spec_query(doc), 4)];
        queries.extend(
            requests::numbered_steps(doc)
                .iter()
                .take(8)
                .map(|s| (s.text.trim().to_owned(), 2)),
        );
        queries.extend(planned.into_iter().take(12).map(|q| (q, 1)));
        let names = Names::for_document(doc, |_| false);
        let gather = Gather {
            queries,
            clojure_version: Some(g.model.clojure_version.clone()),
            core_candidates: vec![names.function],
            project_namespace: doc.project.as_ref().map(|p| p.namespace.clone()),
            include_restricted: false,
        };
        match ports.gather(gather) {
            Some(id) => job.gather = Some(id),
            None => {
                job.notes
                    .push("The Clojure documentation could not be searched, so none was used.".into());
                self.write_code(&BTreeSet::new(), ports);
            }
        }
    }

    fn write_code(&mut self, core: &BTreeSet<String>, ports: &mut dyn Ports) {
        let Some(job) = &mut self.job else { return };
        let Some(g) = &mut job.generation else { return };
        g.names = Some(
            g.start_names
                .clone()
                .unwrap_or_else(|| Names::for_document(&g.spec, |s| core.contains(s))),
        );
        let (request, phase) = match g.start.clone() {
            Some((code, report)) => {
                let names = g.names.as_ref().expect("names were just set");
                let plan = validation::plan(&g.spec, &code, names);
                let failures = validation::failure_summary(&plan, &report, MAX_FAILURE_CHARS);
                let focus = validation::focus(&plan, &report);
                let (feedback, fix) = requests::feedback(
                    &code,
                    &failures,
                    focus.as_ref(),
                    validation::readable(&report),
                    ports.runtime().is_some(),
                );
                g.turns = vec![requests::Turn {
                    answer: requests::existing_code_answer(&code),
                    feedback,
                    analysis: None,
                    fix,
                }];
                g.base = Some(code);
                g.kind = AttemptKind::Fix(1);
                self.analyze(ports);
                return;
            }
            None => (requests::generation_request(g.input()), Phase::Generating),
        };
        self.send(request, phase, ports);
    }

    /// Applies documentation found for the running generation.
    pub fn on_gathered(&mut self, request: u64, result: Result<Gathered, String>, ports: &mut dyn Ports) {
        let Some(job) = &mut self.job else { return };
        if job.gather != Some(request) {
            return;
        }
        job.gather = None;
        if job.kind == JobKind::Describe {
            let Some(d) = &mut job.describing else { return };
            let section = match result {
                Ok(found) if !found.project.is_empty() => {
                    Some(clojurecraft_knowledge::prompt::project_section(
                        &found.project,
                        false,
                        requests::PROJECT_CODE_DROP_ORDER,
                    ))
                }
                Ok(_) => None,
                Err(e) => {
                    job.notes.push(format!(
                        "The project code could not be searched ({e}), so none was used."
                    ));
                    None
                }
            };
            let request = describe::request(&d.spec, &d.code, section.clone());
            d.project_code = section;
            self.send(request, Phase::Describing, ports);
            return;
        }
        let mut core = BTreeSet::new();
        match result {
            Ok(found) => {
                core = found.core;
                if let Some(g) = &mut job.generation
                    && let Some(info) = found.info.as_ref().filter(|_| !found.hits.is_empty())
                {
                    g.documentation = found
                        .hits
                        .iter()
                        .map(|h| {
                            clojurecraft_knowledge::prompt::documentation_ref(
                                h,
                                info,
                                &found.collection_version,
                            )
                        })
                        .collect();
                    g.docs_section = Some(clojurecraft_knowledge::prompt::reference_section(
                        &found.hits,
                        info,
                        DOCUMENTATION_DROP_ORDER,
                    ));
                }
                if let Some(g) = &mut job.generation
                    && !found.project.is_empty()
                {
                    g.documentation.extend(found.project.iter().map(project_ref));
                    g.project_section = Some(clojurecraft_knowledge::prompt::project_section(
                        &found.project,
                        true,
                        requests::PROJECT_CODE_DROP_ORDER,
                    ));
                }
            }
            Err(e) => job.notes.push(format!(
                "The Clojure documentation could not be searched ({e}), so none was used."
            )),
        }
        self.write_code(&core, ports);
    }

    /// Applies an inference event. Events of requests other than the current one are ignored.
    pub fn on_model(
        &mut self,
        event: &ServiceEvent,
        docs: &mut dyn Documents,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        match event {
            ServiceEvent::Stream { request, event } if job.request == Some(*request) => {
                match event {
                    StreamEvent::Content(t) => job.output.push_str(t),
                    StreamEvent::Reasoning(t) => job.reasoning.push_str(t),
                    StreamEvent::ToolCall { .. } => {}
                }
                Vec::new()
            }
            ServiceEvent::Finished { request, result } if job.request == Some(*request) => {
                job.request = None;
                let function = job.function;
                let Some(doc) = docs.document(function) else {
                    self.end(Report::Cancelled);
                    return Vec::new();
                };
                match result {
                    Ok(g) => self.finished(g, doc, ports),
                    Err(GenerationError::Cancelled) => {
                        self.end(Report::Cancelled);
                        Vec::new()
                    }
                    Err(e) => self.failed(e, doc, ports),
                }
            }
            _ => Vec::new(),
        }
    }

    fn failed(
        &mut self,
        e: &GenerationError,
        doc: &mut FunctionDocument,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        // A reasoning model that used up its budget thinking is asked once more with room to
        // think, and keeps that room for later requests.
        if let GenerationError::CutOff { reasoning_chars, .. } = e
            && *reasoning_chars > 0
            && !self.model_reasons
            && let Some(job) = &mut self.job
            && let Some(request) = job.last_request.take()
        {
            self.model_reasons = true;
            job.notes.push(
                "The model reasons before answering and ran out of room, so it was asked again with a \
                 larger answer budget."
                    .into(),
            );
            let phase = job.phase;
            self.send(request, phase, ports);
            return Vec::new();
        }
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        match job.phase {
            // The draft is still usable without the check.
            Phase::Revising => {
                job.notes.push(format!(
                    "The description was not checked against the results: {e}"
                ));
                let examples = job
                    .describing
                    .as_mut()
                    .and_then(|d| d.recorded.take())
                    .unwrap_or_default();
                self.apply_description(examples, doc, ports)
            }
            // The description is already applied; only the contract is missing.
            Phase::Proposing if job.kind == JobKind::Describe => {
                job.notes.push(format!("No contract was proposed: {e}"));
                self.finish_description();
                Vec::new()
            }
            // Planning only improves retrieval; continue without it.
            Phase::Planning => {
                job.notes.push(format!(
                    "Choosing documentation failed ({e}); the description and steps were used instead."
                ));
                self.retrieve(Vec::new(), ports);
                Vec::new()
            }
            // A fix can still be asked for without an analysis.
            Phase::Analyzing
                if matches!(
                    e,
                    GenerationError::Malformed { .. }
                        | GenerationError::CutOff { .. }
                        | GenerationError::ToolRounds(_)
                ) =>
            {
                job.notes
                    .push(format!("The analysis of a failure was not usable: {e}"));
                self.analyzed(String::new(), ports);
                Vec::new()
            }
            // A malformed or cut-off answer is a failed attempt; others may still succeed.
            Phase::Generating | Phase::Repairing
                if matches!(
                    e,
                    GenerationError::Malformed { .. } | GenerationError::CutOff { .. }
                ) =>
            {
                self.attempt_without_code(format!("The model's answer was not usable: {e}"), ports)
            }
            _ => {
                let what = job_label(job.kind);
                let remedies = match e {
                    GenerationError::ContextExceeded { remedies, .. } => remedies.clone(),
                    _ => Vec::new(),
                };
                let raw = match e {
                    GenerationError::Malformed { raw, .. } | GenerationError::CutOff { raw, .. } => {
                        Some(raw.clone())
                    }
                    _ => None,
                };
                let message = e.to_string();
                self.end_generation_failure(what, message, remedies, raw);
                Vec::new()
            }
        }
    }

    /// Ends the job with a failure, keeping the attempts made so far visible.
    fn end_generation_failure(
        &mut self,
        what: &'static str,
        message: String,
        remedies: Vec<Remedy>,
        raw: Option<String>,
    ) {
        let attempts = self
            .job
            .as_ref()
            .map(|j| j.attempts().to_vec())
            .unwrap_or_default();
        if attempts.is_empty() {
            self.end(Report::Failed {
                what,
                message,
                remedies,
                raw,
            });
        } else {
            let notes = vec![format!("Could not {what}: {message}")];
            self.end_unresolved(attempts, notes);
        }
    }

    fn malformed(&mut self, problem: String, raw: &str) {
        let what = self.job.as_ref().map_or("", |j| job_label(j.kind));
        self.end(Report::Failed {
            what,
            message: format!("the model's answer is not usable: {problem}"),
            remedies: Vec::new(),
            raw: Some(raw.to_owned()),
        });
    }

    fn finished(&mut self, g: &Generation, doc: &mut FunctionDocument, ports: &mut dyn Ports) -> Vec<Update> {
        if !g.reasoning.trim().is_empty() {
            self.model_reasons = true;
        }
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        if job.phase == Phase::Analyzing {
            let mut analysis = g.text.trim().to_owned();
            let checked = crate::eval_tool::transcript(&g.tool_calls);
            if !checked.is_empty() {
                analysis.push_str(&format!("\n\nChecked in the sandbox:\n{checked}"));
            }
            self.analyzed(analysis, ports);
            return Vec::new();
        }
        // Generation answers are plain text, read into the same shape as a structured answer; a
        // fix's changed forms are merged into the code it fixes first.
        let from_text;
        let value = match (&g.value, job.phase) {
            (Some(v), _) => Some(v),
            (None, Phase::Generating | Phase::Repairing) => {
                let read = if g.finish == clojurecraft_inference::Finish::Length {
                    Err("the answer was cut off at its length limit".to_owned())
                } else if job.phase == Phase::Repairing
                    && let Some(state) = &mut job.generation
                {
                    fix_answer(state, &g.text)
                } else {
                    answers::generation_from_text(&g.text)
                };
                match read {
                    Ok(v) => {
                        if let Some(state) = &mut job.generation {
                            state.answer = Some(g.text.clone());
                        }
                        from_text = v;
                        Some(&from_text)
                    }
                    Err(e) => {
                        return self
                            .attempt_without_code(format!("The model's answer was not usable: {e}"), ports);
                    }
                }
            }
            (None, _) => None,
        };
        let Some(value) = value else {
            match job.phase {
                Phase::Generating | Phase::Repairing => {
                    return self.attempt_without_code("The model gave no structured answer.".into(), ports);
                }
                Phase::Revising => {
                    job.notes.push(
                        "The description was not checked against the results: the model gave no \
                         structured answer."
                            .into(),
                    );
                    let examples = job
                        .describing
                        .as_mut()
                        .and_then(|d| d.recorded.take())
                        .unwrap_or_default();
                    return self.apply_description(examples, doc, ports);
                }
                _ => {
                    self.malformed("no structured answer".into(), &g.text);
                    return Vec::new();
                }
            }
        };
        match job.phase {
            Phase::Interpreting => match answers::parse_interpretation(value) {
                Ok(answer) => {
                    doc.interpretation = Some(Interpretation {
                        restatement: answer.restatement.clone(),
                        assumptions: answer.assumptions.clone(),
                        issues: answer.issues.clone(),
                        spec_basis: job.spec_basis.clone(),
                        model: g.model.clone(),
                        created_at: Timestamp::now(),
                    });
                    let function = job.function;
                    if answer.issues.is_empty() {
                        let notes = std::mem::take(&mut job.notes);
                        // No issues means no open questions remain from earlier readings.
                        doc.replace_open_questions(Vec::new());
                        self.end(Report::Interpreted { questions: 0, notes });
                    } else {
                        let request = requests::questions_request(doc, &answer);
                        job.interpretation = Some(answer);
                        self.send(request, Phase::Asking, ports);
                    }
                    vec![Update::Edited(function)]
                }
                Err(e) => {
                    self.malformed(e, &g.text);
                    Vec::new()
                }
            },
            Phase::Asking => match answers::parse_questions(value, &doc.clarifications) {
                Ok(questions) => {
                    let n = questions.len();
                    doc.replace_open_questions(questions);
                    let function = job.function;
                    let notes = std::mem::take(&mut job.notes);
                    self.end(Report::Interpreted { questions: n, notes });
                    vec![Update::Edited(function)]
                }
                Err(e) => {
                    self.malformed(e, &g.text);
                    Vec::new()
                }
            },
            Phase::Proposing if job.kind == JobKind::Describe => {
                let function = job.function;
                let accepted = doc
                    .contract
                    .as_ref()
                    .is_some_and(|c| c.status == clojurecraft_domain::ContractStatus::Accepted);
                let allowed: Vec<String> = deps::allowed(doc).into_iter().collect();
                let mut updates = Vec::new();
                match answers::parse_contract(value, allowed, doc.arguments()) {
                    Ok(_) if accepted => job
                        .notes
                        .push("A contract was accepted meanwhile, so none was proposed.".into()),
                    Ok((mut contract, notes)) => {
                        let fitted = crate::spec::fit_contract(&mut contract, doc);
                        doc.contract = Some(contract);
                        job.notes.extend(notes);
                        job.notes.extend(fitted);
                        if let Some(d) = &mut job.describing {
                            d.filled.push("a proposed contract".into());
                        }
                        updates.push(Update::Edited(function));
                    }
                    Err(e) => job.notes.push(format!("No contract was proposed: {e}.")),
                }
                self.finish_description();
                updates
            }
            Phase::Proposing => {
                if doc
                    .contract
                    .as_ref()
                    .is_some_and(|c| c.status == clojurecraft_domain::ContractStatus::Accepted)
                {
                    self.end(Report::Failed {
                        what: "propose a contract",
                        message: "a contract was accepted while the proposal was being written, so the \
                                  proposal was not applied"
                            .into(),
                        remedies: Vec::new(),
                        raw: None,
                    });
                    return Vec::new();
                }
                let allowed: Vec<String> = deps::allowed(doc).into_iter().collect();
                match answers::parse_contract(value, allowed, doc.arguments()) {
                    Ok((mut contract, mut notes)) => {
                        notes.extend(crate::spec::fit_contract(&mut contract, doc));
                        doc.contract = Some(contract);
                        let function = job.function;
                        notes.extend(std::mem::take(&mut job.notes));
                        self.end(Report::ContractProposed { notes });
                        vec![Update::Edited(function)]
                    }
                    Err(e) => {
                        self.malformed(e, &g.text);
                        Vec::new()
                    }
                }
            }
            Phase::Planning => {
                let planned = answers::parse_planning(value).unwrap_or_default();
                self.retrieve(planned, ports);
                Vec::new()
            }
            Phase::ProposingTests => match edge_cases::parse_proposals(value, doc) {
                Ok(proposals) => {
                    let function = job.function;
                    let mut notes = std::mem::take(&mut job.notes);
                    notes.extend(proposals.notes.iter().cloned());
                    if doc.spec_fingerprint() != job.spec_basis {
                        notes.push(
                            "The specification changed while the cases were being proposed; check \
                             them against the current specification."
                                .into(),
                        );
                    }
                    let (added, ambiguous) = (proposals.cases.len(), proposals.ambiguous());
                    let ids: Vec<clojurecraft_domain::TestCaseId> =
                        proposals.cases.iter().map(|c| c.id).collect();
                    doc.tests.extend(proposals.cases);
                    let report = Report::TestsProposed {
                        added,
                        ambiguous,
                        notes,
                    };
                    // What the current code does with each case, to review it by; never its
                    // expected output.
                    let started = edge_cases::current_code_run(doc, &ids).map(|r| ports.validate(r));
                    match started {
                        Some(Ok(id)) => {
                            job.validation = Some(id);
                            job.phase = Phase::Comparing;
                            job.annotating = Some((ids, report));
                        }
                        _ => self.end(report),
                    }
                    vec![Update::Edited(function)]
                }
                Err(e) => {
                    self.malformed(e, &g.text);
                    Vec::new()
                }
            },
            Phase::Retrieving
            | Phase::Validating
            | Phase::Analyzing
            | Phase::Recording
            | Phase::Comparing => Vec::new(),
            Phase::Revising => {
                let Some(d) = &mut job.describing else {
                    return Vec::new();
                };
                if d.auditing {
                    match describe::parse_audit(value) {
                        Ok(found) => job.notes.extend(found),
                        Err(e) => job.notes.push(format!(
                            "The description was not checked against the results: {e}."
                        )),
                    }
                } else if let Some(answer) = &mut d.answer
                    && let Err(e) = describe::revise(answer, value)
                {
                    job.notes.push(format!(
                        "The description was not checked against the results: {e}."
                    ));
                }
                let examples = d.recorded.take().unwrap_or_default();
                self.apply_description(examples, doc, ports)
            }
            Phase::Describing => {
                let answer = match describe::parse(value, doc.arguments()) {
                    Ok(a) => a,
                    Err(e) => {
                        self.malformed(e, &g.text);
                        return Vec::new();
                    }
                };
                let Some(d) = &mut job.describing else {
                    return Vec::new();
                };
                let code = d.code.clone();
                job.notes.extend(answer.notes.iter().cloned());
                let inputs = answer.inputs.clone();
                d.answer = Some(answer);
                if inputs.is_empty() {
                    return self.described(Vec::new(), Vec::new(), doc, ports);
                }
                let started = describe::run_request(doc, &code, &inputs).and_then(|r| ports.validate(r));
                match started {
                    Ok(id) => {
                        job.validation = Some(id);
                        job.phase = Phase::Recording;
                        job.output.clear();
                        job.reasoning.clear();
                        Vec::new()
                    }
                    Err(why) => {
                        let note = format!("No example outputs were recorded: {why}.");
                        self.described(Vec::new(), vec![note], doc, ports)
                    }
                }
            }
            Phase::Generating | Phase::Repairing => {
                let Some(gen_state) = &mut job.generation else {
                    return Vec::new();
                };
                let mut generated = match answers::parse_generation(value) {
                    Ok(c) => c,
                    Err(e) => {
                        return self
                            .attempt_without_code(format!("The model's answer was not usable: {e}"), ports);
                    }
                };
                let Some(names) = gen_state.names.clone() else {
                    return Vec::new();
                };
                // Copies of the rest of the namespace go before the conventions, which would
                // otherwise make them private and note it.
                let mut copies = crate::project::Copies::default();
                if let Some(p) = &gen_state.spec.project {
                    let (code, dropped) = crate::project::drop_copies(p, &generated.code);
                    generated.code = code;
                    copies = dropped;
                }
                let (code, prepared) = prepare(&generated.code, &names.function);
                generated.code = code;
                gen_state.prepared.extend(prepared);
                if let Some(p) = &gen_state.spec.project {
                    // Code that only parses once its delimiters are repaired.
                    let (code, more) = crate::project::drop_copies(p, &generated.code);
                    generated.code = code;
                    copies.unchanged.extend(more.unchanged);
                    copies.changed.extend(more.changed);
                    gen_state.prepared.extend(copies.note(p));
                }
                gen_state.omitted = g.plan.omitted.iter().map(|(label, _)| label.clone()).collect();
                if gen_state
                    .omitted
                    .iter()
                    .any(|l| l == clojurecraft_knowledge::prompt::SECTION_LABEL)
                    && !gen_state.documentation.is_empty()
                {
                    gen_state.documentation.clear();
                    job.notes.push(
                        "The documentation excerpts did not fit into the context and were left out.".into(),
                    );
                }
                if generated.unescaped {
                    job.notes.push(
                        "The model escaped the code's line breaks twice; they were restored before checking."
                            .into(),
                    );
                }
                if job.phase == Phase::Generating
                    && let Some(request) = &job.last_request
                {
                    gen_state.prompt = requests::prompt_messages(request, Some(&g.plan.included));
                }
                let candidate = Candidate::check(generated, &names, &gen_state.allowed);
                let plan = validation::plan(&gen_state.spec, &candidate.code, &names);
                let request = plan.request.clone();
                gen_state.candidate = Some(candidate);
                gen_state.plan = Some(plan);
                match request {
                    // Parse or structure problems: the report is complete without running.
                    None => self.validated(Err("not run".into()), None, doc, ports),
                    Some(r) => match ports.validate(r) {
                        Ok(id) => {
                            job.validation = Some(id);
                            job.phase = Phase::Validating;
                            job.output.clear();
                            job.reasoning.clear();
                            Vec::new()
                        }
                        Err(why) => self.unvalidated(why, doc),
                    },
                }
            }
            Phase::Explaining => {
                let function = job.function;
                let (code, steps) = job.explaining.clone().unwrap_or_default();
                let current = doc
                    .implementation
                    .as_mut()
                    .filter(|i| i.source == code)
                    .and_then(|i| i.generation.as_mut());
                let Some(generation) = current else {
                    self.end(Report::Failed {
                        what: job_label(JobKind::Explain),
                        message: "the code changed while it was being explained".into(),
                        remedies: Vec::new(),
                        raw: None,
                    });
                    return Vec::new();
                };
                let steps: Vec<&Step> = steps.iter().collect();
                match answers::parse_explanation(value, &steps, &code) {
                    Ok(e) => {
                        generation.explanation = Some(e);
                        let notes = std::mem::take(&mut job.notes);
                        self.end(Report::Explained { notes });
                        vec![Update::Edited(function)]
                    }
                    Err(e) => {
                        self.malformed(e, &g.text);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Applies the result of a validation run started by the current job.
    pub fn on_validated(
        &mut self,
        id: u64,
        execution: Execution,
        docs: &mut dyn Documents,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        if job.validation != Some(id) {
            return Vec::new();
        }
        job.validation = None;
        let Some(doc) = docs.document(job.function) else {
            self.end(Report::Cancelled);
            return Vec::new();
        };
        if job.comparing.is_some() {
            return self.compared(execution.outcome, doc, ports);
        }
        if let Some((ids, mut report)) = job.annotating.take() {
            let function = job.function;
            let summary = edge_cases::annotate(doc, &ids, execution.outcome.as_ref().ok());
            if let (Some(s), Report::TestsProposed { notes, .. }) = (summary, &mut report) {
                notes.push(s);
            }
            self.end(report);
            return vec![Update::Edited(function)];
        }
        if job.kind == JobKind::Describe {
            let inputs = job
                .describing
                .as_ref()
                .and_then(|d| d.answer.as_ref())
                .map(|a| a.inputs.clone())
                .unwrap_or_default();
            let (examples, notes) =
                describe::recorded_examples(&inputs, execution.outcome.as_ref().map_err(String::as_str));
            return self.described(examples, notes, doc, ports);
        }
        self.validated(execution.outcome, Some(execution.environment), doc, ports)
    }

    /// Records the current candidate's report and decides what follows: passing code ends the
    /// loop, failing code is repaired.
    fn validated(
        &mut self,
        outcome: Result<EvalOutcome, String>,
        environment: Option<ExecutionEnvironment>,
        doc: &mut FunctionDocument,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        let function = job.function;
        let Some(g) = &mut job.generation else {
            return Vec::new();
        };
        let (Some(plan), Some(candidate)) = (&g.plan, &g.candidate) else {
            return Vec::new();
        };
        let report = validation::report(
            plan,
            ReportContext {
                basis: g.spec.basis_with_code(&candidate.code),
                environment: environment.unwrap_or_else(|| ExecutionEnvironment {
                    clojure_version: g.model.clojure_version.clone(),
                    java_version: String::new(),
                    platform: String::new(),
                }),
                attempt: Some(g.attempt),
                run: Some(g.run),
                outcome: outcome.as_ref().map_err(String::as_str),
            },
        );
        let passed = report.passed();
        let note = std::mem::take(&mut g.prepared);
        g.attempts.push(Attempt {
            number: g.attempt,
            kind: g.kind,
            code: Some(candidate.code.clone()),
            report: Some(report.clone()),
            note: (!note.is_empty()).then(|| note.join(" ")),
        });
        let mut updates = vec![Update::Validated(function, Box::new(report))];
        if passed {
            updates.extend(self.finish_passing(doc, Some(ports)));
        } else {
            updates.extend(self.next_attempt(ports));
        }
        updates
    }

    /// The candidate could not be run at all (for example, no Java runtime). It is kept, as
    /// before validation existed, but marked as unchecked.
    fn unvalidated(&mut self, why: String, doc: &mut FunctionDocument) -> Vec<Update> {
        if let Some(job) = &mut self.job {
            job.notes.push(format!("The code was not run: {why}"));
            if let Some(g) = &mut job.generation
                && let Some(c) = &g.candidate
            {
                let mut note = std::mem::take(&mut g.prepared);
                note.push(format!("Not run: {why}"));
                g.attempts.push(Attempt {
                    number: g.attempt,
                    kind: g.kind,
                    code: Some(c.code.clone()),
                    report: None,
                    note: Some(note.join(" ")),
                });
            }
        }
        self.finish_passing(doc, None)
    }

    /// The attempt produced no code to check.
    fn attempt_without_code(&mut self, note: String, ports: &mut dyn Ports) -> Vec<Update> {
        if let Some(g) = self.job.as_mut().and_then(|j| j.generation.as_mut()) {
            g.candidate = None;
            g.plan = None;
            g.prepared.clear();
            g.attempts.push(Attempt {
                number: g.attempt,
                kind: g.kind,
                code: None,
                report: None,
                note: Some(note),
            });
        }
        self.next_attempt(ports)
    }

    /// Starts the next attempt, or ends the loop after the last one. Failing code continues the
    /// conversation with a fix; a stuck conversation starts over with a fresh generation; an
    /// unusable answer is asked for again from the same point.
    fn next_attempt(&mut self, ports: &mut dyn Ports) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        let Some(g) = &mut job.generation else {
            return Vec::new();
        };
        if g.attempt >= g.max_attempts() {
            let attempts = g.attempts.clone();
            let notes = std::mem::take(&mut job.notes);
            self.end_unresolved(attempts, notes);
            return Vec::new();
        }
        g.attempt += 1;
        let checked = g
            .attempts
            .last()
            .and_then(|a| Some((a.code.clone()?, a.report.clone()?)));
        let Some((code, report)) = checked else {
            // The last answer was unusable: ask again with a new seed.
            g.answer = None;
            if g.turns.is_empty() {
                g.kind = AttemptKind::Restart;
                let mut request = requests::generation_request(g.input());
                request.temperature = Some(requests::RESTART_TEMPERATURE);
                self.send(request, Phase::Generating, ports);
            } else {
                g.kind = g.next_fix();
                let request = requests::fix_request(g.input(), &g.turns);
                self.send(request, Phase::Repairing, ports);
            }
            return Vec::new();
        };
        let names = g.names.as_ref().expect("names are set before code is written");
        let summary = |code: &str, r: &ValidationReport| {
            let plan = validation::plan(&g.spec, code, names);
            validation::failure_summary(&plan, r, MAX_FAILURE_CHARS)
        };
        let failures = summary(&code, &report);
        if let Some(why) = broken_file(&g.attempts) {
            job.notes.push(format!(
                "The namespace file failed to load outside this function twice in a row ({why}), which \
                 no change of the function can fix, so generation stopped. Check the file and the \
                 project's dependencies."
            ));
            let attempts = g.attempts.clone();
            let notes = std::mem::take(&mut job.notes);
            self.end_unresolved(attempts, notes);
            return Vec::new();
        }
        if conversation_stuck(&g.attempts, &code, &failures, &summary) {
            job.notes.push(format!(
                "{} returned code that failed exactly like earlier code, so the conversation started \
                 over with a fresh generation.",
                g.attempts.last().map_or_else(String::new, Attempt::label)
            ));
            g.turns.clear();
            g.base = None;
            g.answer = None;
            g.kind = AttemptKind::Restart;
            let mut request = requests::generation_request(g.input());
            request.temperature = Some(requests::RESTART_TEMPERATURE);
            self.send(request, Phase::Generating, ports);
            return Vec::new();
        }
        let plan = validation::plan(&g.spec, &code, names);
        let focus = validation::focus(&plan, &report);
        let answer = g
            .answer
            .take()
            .unwrap_or_else(|| requests::existing_code_answer(&code));
        let (mut feedback, fix) = requests::feedback(
            &code,
            &failures,
            focus.as_ref(),
            validation::readable(&report),
            ports.runtime().is_some(),
        );
        if let Some(ago) = regression(&g.attempts) {
            feedback = format!(
                "This error is back: the code you wrote {ago} answers ago failed like this, and the answer after it \
                 fixed that. Keep that fix while you fix what failed since.\n\n{feedback}"
            );
        }
        g.turns.push(requests::Turn {
            answer,
            feedback,
            analysis: None,
            fix,
        });
        g.base = Some(code);
        g.kind = g.next_fix();
        self.analyze(ports);
        Vec::new()
    }

    /// Asks the model why the code it is fixing failed, with the sandbox at hand.
    fn analyze(&mut self, ports: &mut dyn Ports) {
        let Some(g) = self.job.as_ref().and_then(|j| j.generation.as_ref()) else {
            return;
        };
        let tools = match (ports.runtime(), &g.base) {
            (Some(rt), Some(code)) => crate::eval_tool::EvalTool::set(rt, code, &g.spec),
            _ => clojurecraft_inference::ToolSet::default(),
        };
        let request = requests::analysis_request(g.input(), &g.turns, tools);
        self.send(request, Phase::Analyzing, ports);
    }

    /// Keeps the analysis (with what the model evaluated) in the conversation and asks for the
    /// fix it names.
    fn analyzed(&mut self, analysis: String, ports: &mut dyn Ports) {
        let Some(g) = self.job.as_mut().and_then(|j| j.generation.as_mut()) else {
            return;
        };
        if let Some(t) = g.turns.last_mut() {
            t.analysis = Some(analysis);
        }
        let request = requests::fix_request(g.input(), &g.turns);
        self.send(request, Phase::Repairing, ports);
    }

    fn provenance(g: &GenState, spec_basis: Fingerprint) -> Provenance {
        let profile = Role::Generation.profile();
        Provenance {
            model: g.model.model.clone(),
            runtime: g.model.runtime.clone(),
            settings: GenerationSettings {
                temperature: g.kind.temperature(),
                top_p: profile.top_p,
                seed: Some(g.seed_now()),
                context_length: g.model.context_length,
                max_output_tokens: profile.max_output_tokens,
            },
            documentation: g.documentation.clone(),
            spec_basis,
            omitted_context: g.omitted.clone(),
            attempts: g.attempt,
            prompt: if g.prompt.is_empty() {
                requests::prompt_messages(&requests::generation_request(g.input()), None)
            } else {
                g.prompt.clone()
            },
        }
    }

    /// Ends a loop in which no attempt passed, offering the most successful acceptable candidate.
    fn end_unresolved(&mut self, attempts: Vec<Attempt>, mut notes: Vec<String>) {
        let Some(job) = &self.job else { return };
        let best = job.generation.as_ref().and_then(|g| {
            let names = g.names.clone()?;
            let attempt = attempts
                .iter()
                .filter(|a| a.code.is_some())
                .filter(|a| {
                    a.report.as_ref().is_some_and(|r| {
                        r.checks.iter().any(|c| {
                            c.subject == clojurecraft_domain::CheckSubject::Load
                                && c.status == clojurecraft_domain::CheckStatus::Passed
                        })
                    })
                })
                .max_by_key(|a| {
                    (
                        a.report.as_ref().map_or(0, |r| validation::tally(r).passed),
                        a.number,
                    )
                })?;
            let code = attempt.code.clone()?;
            let candidate = Candidate::check(
                answers::GeneratedCode {
                    code,
                    unescaped: false,
                },
                &names,
                &g.allowed,
            );
            if !candidate.structure.problems.is_empty() {
                return None;
            }
            let mut provenance = Self::provenance(g, job.spec_basis.clone());
            provenance.settings.seed = Some(g.seed.wrapping_add(u64::from(attempt.number - 1)));
            provenance.settings.temperature = attempt.kind.temperature();
            provenance.attempts = attempt.number;
            Some(Box::new(Pending {
                needs_approval: candidate.structure.needs_approval.clone(),
                replaces_manual: false,
                replaces_accepted: false,
                fix: g.start.is_some(),
                spec_changed: false,
                validation: attempt.report.clone(),
                candidate,
                names,
                provenance,
                attempts: Vec::new(),
                notes: Vec::new(),
                differences: Vec::new(),
            }))
        });
        notes.push(format!(
            "No attempt passed every check after {} attempts.",
            attempts.len()
        ));
        self.end(Report::Unresolved {
            attempts,
            best,
            notes,
        });
    }

    /// Applies passing (or unchecked) code, or holds it for review.
    fn finish_passing(&mut self, doc: &mut FunctionDocument, ports: Option<&mut dyn Ports>) -> Vec<Update> {
        let Some(mut job) = self.job.take() else {
            return Vec::new();
        };
        let Some(g) = job.generation.take() else {
            return Vec::new();
        };
        let (Some(candidate), Some(names)) = (g.candidate.clone(), g.names.clone()) else {
            return Vec::new();
        };
        let provenance = Self::provenance(&g, job.spec_basis.clone());
        let mut notes = std::mem::take(&mut job.notes);
        notes.extend(candidate.structure.warnings.iter().map(|w| format!("Note: {w}.")));
        match g.kind {
            AttemptKind::Generate => {}
            AttemptKind::Restart => notes.push(
                "The code passed after a fresh start; the earlier attempts failed their checks.".into(),
            ),
            AttemptKind::Fix(n) => notes.push(format!(
                "The code passed after {n} fix{}.",
                if n == 1 { "" } else { "es" }
            )),
        }
        // Results computed against an earlier specification must not replace current work.
        let spec_changed = doc.spec_fingerprint() != g.spec.spec_fingerprint()
            || doc.tests_fingerprint() != g.spec.tests_fingerprint();
        if spec_changed {
            notes.push(
                "The specification or tests changed while the code was being generated and checked; it \
                 was checked against the earlier version, so it is shown for review."
                    .into(),
            );
        }
        let validation = g
            .attempts
            .last()
            .filter(|a| a.code.as_deref() == Some(candidate.code.as_str()))
            .and_then(|a| a.report.clone());
        let fix = g.start.is_some();
        if fix {
            notes.push("This is a fix of the current code; review the changes before using it.".into());
        }
        let pending = Pending {
            needs_approval: candidate.structure.needs_approval.clone(),
            replaces_manual: !crate::replaces_silently(doc),
            replaces_accepted: doc.is_accepted_code(),
            fix,
            spec_changed,
            validation,
            candidate,
            names,
            provenance,
            attempts: g.attempts,
            notes,
            differences: Vec::new(),
        };
        if pending.needs_approval.is_empty()
            && !pending.replaces_manual
            && !pending.replaces_accepted
            && !pending.fix
            && !pending.spec_changed
        {
            self.apply(job.function, doc, pending)
        } else {
            let current = doc
                .implementation
                .as_ref()
                .map(|i| i.source.clone())
                .filter(|c| !c.trim().is_empty());
            let probes = crate::compare::probes(doc);
            let compare = (pending.replaces_manual || pending.replaces_accepted) && pending.passed();
            let mut pending = pending;
            if let (Some(p), Some(current)) = (&doc.project, &current) {
                let dropped = crate::project::dropped_references(p, current, &pending.candidate.code);
                if !dropped.is_empty() {
                    pending.differences.push(format!(
                        "The new code no longer uses {}, which the current code uses; check that it \
                         did not write a value or body in place of the definition.",
                        dropped.join(", ")
                    ));
                }
            }
            if let (true, Some(current), Some(ports)) = (compare && !probes.is_empty(), current, ports) {
                match crate::compare::current_run(doc, &current, &probes).and_then(|r| ports.validate(r)) {
                    Ok(id) => {
                        job.phase = Phase::Comparing;
                        job.validation = Some(id);
                        job.comparing = Some(Comparing {
                            pending: Box::new(pending),
                            probes,
                            current: None,
                        });
                        self.job = Some(job);
                        return Vec::new();
                    }
                    Err(why) => {
                        pending
                            .notes
                            .push(format!("The code was not compared with the current code: {why}."));
                        self.reports
                            .insert(job.function, Report::Pending(Box::new(pending)));
                        return Vec::new();
                    }
                }
            }
            self.reports
                .insert(job.function, Report::Pending(Box::new(pending)));
            Vec::new()
        }
    }

    /// Records a run of the comparison: after the current code's run the new code is run on the
    /// same probes; after the new code's run the differences are added and the code is held for
    /// review. A comparison that cannot be made only leaves a note.
    fn compared(
        &mut self,
        outcome: Result<EvalOutcome, String>,
        doc: &FunctionDocument,
        ports: &mut dyn Ports,
    ) -> Vec<Update> {
        let Some(job) = &mut self.job else {
            return Vec::new();
        };
        let Some(c) = &mut job.comparing else {
            return Vec::new();
        };
        let failure = match &outcome {
            Ok(o) => crate::compare::unusable(o),
            Err(e) => Some(e.clone()),
        };
        let done = match (failure, outcome, c.current.take()) {
            (Some(why), _, _) => {
                c.pending
                    .notes
                    .push(format!("The code was not compared with the current code: {why}."));
                true
            }
            (None, Ok(current), None) => {
                let started =
                    crate::compare::candidate_run(doc, &c.pending.candidate.code, &c.probes, &current)
                        .and_then(|r| ports.validate(r));
                c.current = Some(current);
                match started {
                    Ok(id) => {
                        job.validation = Some(id);
                        false
                    }
                    Err(why) => {
                        c.pending
                            .notes
                            .push(format!("The code was not compared with the current code: {why}."));
                        true
                    }
                }
            }
            (None, Ok(candidate), Some(current)) => {
                let found = crate::compare::differences(&c.probes, &current, &candidate);
                c.pending.differences.extend(found);
                true
            }
            (None, Err(_), _) => true,
        };
        if done
            && let Some(job) = self.job.take()
            && let Some(c) = job.comparing
        {
            self.reports.insert(job.function, Report::Pending(c.pending));
        }
        Vec::new()
    }

    fn apply(&mut self, function: FunctionId, doc: &mut FunctionDocument, pending: Pending) -> Vec<Update> {
        let attempts = pending.attempts.clone();
        let passing_fix = pending
            .validation
            .as_ref()
            .filter(|r| pending.fix && r.passed())
            .cloned();
        match crate::implementation(&pending.candidate, pending.provenance) {
            Some(implementation) => {
                doc.implementation = Some(implementation);
                let mut notes = pending.notes;
                // A reviewed fix that passes every accepted case (checked against exactly the
                // current specification and tests) becomes the accepted version; the previous one
                // stays in the history.
                let reason = match passing_fix.map(|r| doc.accept_implementation(&r)) {
                    Some(Ok(())) => {
                        notes.push("The fixed code is now the accepted version.".into());
                        RevisionReason::with_note(
                            RevisionKind::Accepted,
                            "Fix for failing edge cases, reviewed and accepted",
                        )
                    }
                    _ => RevisionReason::with_note(RevisionKind::Generated, "Generated by the local model"),
                };
                self.reports
                    .insert(function, Report::Generated { notes, attempts });
                vec![Update::Commit(function, reason)]
            }
            None => {
                let notes = vec![format!(
                    "The code could not be used: {}",
                    pending
                        .candidate
                        .structure
                        .problems
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                )];
                self.reports.insert(
                    function,
                    Report::Unresolved {
                        attempts,
                        best: None,
                        notes,
                    },
                );
                Vec::new()
            }
        }
    }

    /// Uses held code after the user reviewed it, first allowing the namespaces it needs. This
    /// also applies the best candidate of an unresolved generation when the user chooses it.
    pub fn accept_pending(&mut self, doc: &mut FunctionDocument) -> Result<Vec<Update>, String> {
        let (mut pending, attempts) = match self.reports.get(&doc.id) {
            Some(Report::Pending(p)) => ((**p).clone(), p.attempts.clone()),
            Some(Report::Unresolved {
                best: Some(p),
                attempts,
                ..
            }) => ((**p).clone(), attempts.clone()),
            _ => return Ok(Vec::new()),
        };
        pending.attempts = attempts;
        let mut updates = Vec::new();
        if !pending.needs_approval.is_empty() {
            deps::approve(doc, &pending.needs_approval).map_err(|e| e.to_string())?;
            pending.notes.push(format!(
                "Allowed {} for this function.",
                pending.needs_approval.join(", ")
            ));
            updates.push(Update::Edited(doc.id));
        }
        if !pending.passed() {
            pending
                .notes
                .push("This code did not pass every check; see its validation results.".into());
        }
        let allowed = deps::allowed(doc);
        pending.candidate.recheck(&pending.names, &allowed);
        let function = doc.id;
        updates.extend(self.apply(function, doc, pending));
        Ok(updates)
    }

    pub fn discard_pending(&mut self, function: FunctionId) {
        if matches!(
            self.reports.get(&function),
            Some(Report::Pending(_) | Report::Unresolved { .. })
        ) {
            self.reports.remove(&function);
        }
    }
}

/// Generated code made ready for checking: closing delimiters inferred from the indentation when
/// the code cannot be read, and harmless extras removed. Returns the code and what was changed.
/// Reads a fix answer and merges its changed forms into the code it fixes, into the shape of a
/// generation answer; the merge is noted on the attempt.
fn fix_answer(g: &mut GenState, text: &str) -> Result<serde_json::Value, String> {
    let (_, edit) = answers::fix_from_text(text)?;
    let base = g.base.clone().unwrap_or_default();
    let function = g.names.as_ref().map_or("", |n| n.function.as_str());
    let applied = edits::apply(&base, &edit, function)?;
    g.prepared.push(applied.summary());
    Ok(serde_json::json!({"code": applied.code}))
}

/// Whether the conversation since its last generation is going in circles: the newest code is
/// code it already checked, or fails exactly like the code before it.
fn conversation_stuck(
    attempts: &[Attempt],
    code: &str,
    failures: &str,
    summary: &dyn Fn(&str, &ValidationReport) -> String,
) -> bool {
    let start = attempts
        .iter()
        .rposition(|a| matches!(a.kind, AttemptKind::Generate | AttemptKind::Restart))
        .unwrap_or(0);
    let Some((last, earlier)) = attempts[start..].split_last() else {
        return false;
    };
    if !matches!(last.kind, AttemptKind::Fix(_)) {
        return false;
    }
    let checked: Vec<(&String, &ValidationReport)> = earlier
        .iter()
        .filter_map(|a| Some((a.code.as_ref()?, a.report.as_ref()?)))
        .collect();
    checked.iter().any(|(c, _)| c.as_str() == code)
        || checked.last().is_some_and(|(c, r)| summary(c, r) == failures)
}

/// The first failure of a report, with line numbers left out so that moved code still matches.
fn failure_key(report: &ValidationReport) -> Option<String> {
    let check = report
        .checks
        .iter()
        .find(|c| c.status != clojurecraft_domain::CheckStatus::Passed)?;
    let first = check.diagnostics.first().map_or_else(
        || format!("{:?} {:?}", check.expected, check.actual),
        |d| d.lines().take(2).collect::<Vec<_>>().join("\n"),
    );
    let mut key = format!("{:?} ", check.subject);
    let mut rest = first.as_str();
    while let Some(at) = rest.find("line ") {
        key.push_str(&rest[..at + 5]);
        rest = rest[at + 5..].trim_start_matches(|c: char| c.is_ascii_digit());
    }
    key.push_str(rest);
    Some(key)
}

/// How many answers ago the conversation's newest code last failed the way it fails now, when an
/// answer in between had fixed that failure: a fix got lost.
fn regression(attempts: &[Attempt]) -> Option<usize> {
    let start = attempts
        .iter()
        .rposition(|a| matches!(a.kind, AttemptKind::Generate | AttemptKind::Restart))
        .unwrap_or(0);
    let keys: Vec<Option<String>> = attempts[start..]
        .iter()
        .filter_map(|a| a.report.as_ref())
        .map(failure_key)
        .collect();
    let (last, earlier) = keys.split_last()?;
    let last = last.as_ref()?;
    if earlier.last()?.as_ref() == Some(last) {
        return None;
    }
    let at = earlier.iter().rposition(|k| k.as_ref() == Some(last))?;
    Some(earlier.len() - at)
}

/// The load failure outside the function that the last two checked attempts share: the rest of
/// the namespace file does not load, whatever the function is.
fn broken_file(attempts: &[Attempt]) -> Option<String> {
    let outside = |a: &Attempt| {
        a.report.as_ref()?.checks.iter().find_map(|c| {
            (c.subject == clojurecraft_domain::CheckSubject::Load
                && c.status != clojurecraft_domain::CheckStatus::Passed)
                .then(|| {
                    c.diagnostics
                        .iter()
                        .find(|d| d.contains(validation::OUTSIDE_FUNCTION))
                })
                .flatten()
                .cloned()
        })
    };
    let mut checked = attempts.iter().rev().filter(|a| a.report.is_some());
    let last = outside(checked.next()?)?;
    (outside(checked.next()?)? == last).then_some(last)
}

fn prepare(code: &str, function: &str) -> (String, Vec<String>) {
    let mut notes = Vec::new();
    let mut code = code.to_owned();
    if let Some(fixed) = delimiters::repair(&code) {
        code = fixed;
        notes.push(
            "The closing delimiters did not match, so they were inferred from the indentation.".to_owned(),
        );
    }
    let tidied = cleanup::tidy(&code);
    notes.extend(tidied.notes);
    let fixed = cleanup::conventions(&tidied.code, function);
    notes.extend(fixed.notes);
    (fixed.code, notes)
}

/// The namespace and function the current implementation defines: as recorded when it was
/// generated, or as read from the code.
fn current_names(doc: &FunctionDocument) -> Option<Names> {
    let i = doc.implementation.as_ref()?;
    if let Some(g) = &i.generation
        && !i.is_manually_edited()
        && !g.namespace.is_empty()
        && !g.function_name.is_empty()
    {
        return Some(Names {
            namespace: g.namespace.clone(),
            function: g.function_name.clone(),
            arguments: doc.arguments(),
        });
    }
    let allowed = deps::allowed(doc);
    let s = crate::structure::check(
        &i.source,
        &crate::structure::Expectations {
            namespace: "",
            function: "",
            allowed: &allowed,
            arguments: doc.arguments(),
        },
    );
    Some(Names {
        namespace: s.namespace?,
        function: s.function?,
        arguments: doc.arguments(),
    })
}

/// The description and steps, the text searched for related documentation.
fn spec_query(doc: &FunctionDocument) -> String {
    let mut text = doc.description.trim().to_owned();
    for s in requests::numbered_steps(doc) {
        text.push('\n');
        text.push_str(s.text.trim());
    }
    let mut end = text.len().min(1200);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clojurecraft_clojure::{EvalStatus, HarnessReport};
    use clojurecraft_domain::{
        CheckStatus, CheckSubject, ClarificationStatus, Contract, ContractStatus, Edn, Example,
        Implementation, Step,
    };
    use clojurecraft_inference::{Finish, Placement, Priority, PromptPlan, Usage};
    use serde_json::{Value, json};

    use super::*;

    #[derive(Default)]
    struct FakePorts {
        sent: Vec<(RequestId, GenerateRequest)>,
        cancelled: Vec<RequestId>,
        gathers: Vec<Gather>,
        validations: Vec<EvalRequest>,
        cancelled_validations: Vec<u64>,
        no_model: bool,
        no_docs: bool,
        no_runtime: bool,
    }

    impl Ports for FakePorts {
        fn generate(&mut self, request: GenerateRequest) -> Result<RequestId, String> {
            if self.no_model {
                return Err("No model is selected.".into());
            }
            let id = self.sent.len() as RequestId + 1;
            self.sent.push((id, request));
            Ok(id)
        }
        fn cancel(&mut self, request: RequestId) {
            self.cancelled.push(request);
        }
        fn gather(&mut self, gather: Gather) -> Option<u64> {
            if self.no_docs {
                return None;
            }
            self.gathers.push(gather);
            Some(self.gathers.len() as u64)
        }
        fn validate(&mut self, request: EvalRequest) -> Result<u64, String> {
            if self.no_runtime {
                return Err("The Java runtime is not installed.".into());
            }
            self.validations.push(request);
            Ok(self.validations.len() as u64)
        }
        fn cancel_validation(&mut self, id: u64) {
            self.cancelled_validations.push(id);
        }
    }

    impl FakePorts {
        fn last(&self) -> (RequestId, Role) {
            let (id, r) = self.sent.last().unwrap();
            (*id, r.role)
        }

        fn last_text(&self) -> String {
            self.sent
                .last()
                .unwrap()
                .1
                .sections
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    struct Docs(HashMap<FunctionId, FunctionDocument>);

    impl Documents for Docs {
        fn document(&mut self, id: FunctionId) -> Option<&mut FunctionDocument> {
            self.0.get_mut(&id)
        }
    }

    fn doc() -> FunctionDocument {
        let mut d = FunctionDocument::new("Order totals");
        d.description = "Total amounts per customer".into();
        d.steps = vec![Step::new("group orders by :customer"), Step::new("sum :amount")];
        d.examples = vec![Example::direct(
            "",
            Edn::new("[{:customer 1 :amount 2}]"),
            Edn::new("{1 2}"),
        )];
        d
    }

    fn setup() -> (Assistant, FakePorts, Docs, FunctionId) {
        let d = doc();
        let id = d.id;
        (
            Assistant::default(),
            FakePorts::default(),
            Docs(HashMap::from([(id, d)])),
            id,
        )
    }

    fn model() -> ModelContext {
        ModelContext {
            label: "Qwen · Q4_K_M".into(),
            model: ModelIdentity {
                display_name: "Qwen".into(),
                repository: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF".into(),
                revision: "abc".into(),
                file: "q.gguf".into(),
                sha256: "00".into(),
                quantization: "Q4_K_M".into(),
                architecture: "qwen2".into(),
            },
            runtime: InferenceRuntime {
                llama_cpp_release: "b12138".into(),
                backend: "CUDA".into(),
            },
            context_length: 8192,
            clojure_version: "1.12.6".into(),
        }
    }

    fn answer(role: Role, value: Value, omitted: Vec<(String, u32)>) -> Box<Generation> {
        Box::new(Generation {
            role,
            model: "Qwen · Q4_K_M".into(),
            text: value.to_string(),
            reasoning: String::new(),
            value: Some(value),
            finish: Finish::Stop,
            usage: Usage::default(),
            plan: PromptPlan {
                included: vec![],
                omitted,
                prompt_tokens: 100,
                reserved_output: 100,
                context: 8192,
                exact: true,
            },
            tool_calls: vec![],
            tools_unavailable: false,
            elapsed: Duration::from_millis(5),
        })
    }

    fn finish(
        a: &mut Assistant,
        p: &mut FakePorts,
        d: &mut Docs,
        request: RequestId,
        result: Result<Box<Generation>, GenerationError>,
    ) -> Vec<Update> {
        a.on_model(&ServiceEvent::Finished { request, result }, d, p)
    }

    fn reply(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs, value: Value) -> Vec<Update> {
        let (id, role) = p.last();
        finish(a, p, d, id, Ok(answer(role, value, vec![])))
    }

    fn env() -> ExecutionEnvironment {
        ExecutionEnvironment {
            clojure_version: "1.12.6".into(),
            java_version: "25.0.5".into(),
            platform: "linux-x86_64".into(),
        }
    }

    /// Harness output for the last validation request: every call returns `actual`, matching when
    /// `pass`.
    fn execution(p: &FakePorts, pass: bool) -> Execution {
        execution_with(p, (!pass).then_some("3"))
    }

    /// Like [`execution`], failing with `wrong` as the value at `[1]` unless it is `None`.
    fn execution_with(p: &FakePorts, wrong: Option<&str>) -> Execution {
        let pass = wrong.is_none();
        let wrong = wrong.unwrap_or("2");
        let req = p.validations.last().unwrap();
        let mut out = String::from(
            "{\"event\":\"read\",\"ok\":true}\n{\"event\":\"check\",\"ok\":true}\n\
             {\"event\":\"lint\",\"findings\":[]}\n{\"event\":\"load\",\"ok\":true}\n",
        );
        for i in 0..req.calls.len() {
            out.push_str(&format!(
                "{{\"event\":\"result\",\"index\":{i},\"status\":\"ok\",\"actual\":\"{{1 {wrong}}}\",\"match\":{pass},\"difference\":{},\"elapsed-ns\":1000}}\n",
                if pass { "null".to_owned() } else { format!("\"at [1]: expected 2 but got {wrong}\"") }
            ));
        }
        out.push_str("{\"event\":\"done\"}\n");
        Execution {
            outcome: Ok(EvalOutcome {
                status: EvalStatus::Completed,
                report: HarnessReport::parse(&out),
                stderr: String::new(),
                elapsed: Duration::from_millis(600),
            }),
            environment: env(),
        }
    }

    fn validated(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs, pass: bool) -> Vec<Update> {
        let id = p.validations.len() as u64;
        let e = execution(p, pass);
        a.on_validated(id, e, d, p)
    }

    /// Answers both runs of a comparison with the current code, if one is running: every probe
    /// returns the same value, so no difference is found.
    fn compared_alike(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs) {
        for _ in 0..2 {
            if a.job().is_none_or(|j| j.phase != Phase::Comparing) {
                return;
            }
            let n = p.validations.last().unwrap().calls.len();
            let e = returning(p, &vec![Some("{}"); n]);
            a.on_validated(p.validations.len() as u64, e, d, p);
        }
    }

    /// Lets the last validation fail with `wrong` as the value at `[1]`.
    fn failed_with(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs, wrong: &str) -> Vec<Update> {
        let id = p.validations.len() as u64;
        let e = execution_with(p, Some(wrong));
        a.on_validated(id, e, d, p)
    }

    /// Answers the running request with plain text, as the model does under the grammar.
    fn reply_text(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs, text: &str) -> Vec<Update> {
        let (id, role) = p.last();
        let mut g = answer(role, json!(null), vec![]);
        g.value = None;
        g.text = text.to_owned();
        finish(a, p, d, id, Ok(g))
    }

    /// A fix answer that changes the public function to `body`.
    fn fix_text(body: &str) -> String {
        format!("```clojure\n(defn order-totals [orders]\n  {body})\n```")
    }

    /// Answers the analysis request of a fix.
    fn analysis(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs) {
        assert_eq!(a.job().unwrap().phase, Phase::Analyzing);
        reply_text(a, p, d, "count is used instead of a sum");
        assert_eq!(a.job().unwrap().phase, Phase::Repairing);
    }

    fn reports(updates: &[Update]) -> Vec<&ValidationReport> {
        updates
            .iter()
            .filter_map(|u| match u {
                Update::Validated(_, r) => Some(&**r),
                _ => None,
            })
            .collect()
    }

    const CODE: &str = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (->> orders\n       (group-by :customer)\n       (update-vals (fn [os] (reduce + (map :amount os))))))\n";
    const WRONG: &str = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders) count))\n";

    fn explanation() -> Value {
        json!({"summary": "Groups, then sums.",
               "steps": [{"step": 1, "code": "(group-by :customer)", "explanation": "groups"}],
               "assumptions": ["amounts are numbers"],
               "symbols": [{"symbol": "update-vals", "purpose": "sums each group"}]})
    }

    fn gathered(core: &[&str]) -> Result<Gathered, String> {
        Ok(Gathered {
            hits: vec![],
            project: vec![],
            info: Some(CollectionInfo {
                format: 1,
                manifest_revision: "r".into(),
                clojure_version: "1.12.6".into(),
                site_commit: "c".into(),
                chunks: 0,
                api_entries: 0,
                page_sections: 0,
                pages: 0,
                embedding: None,
                built_at: 0,
            }),
            collection_version: "v".into(),
            core: core.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    /// Generates without documentation and answers with `code`.
    fn generate_code(
        a: &mut Assistant,
        p: &mut FakePorts,
        d: &mut Docs,
        id: FunctionId,
        code: &str,
    ) -> Vec<Update> {
        a.generate(&d.0[&id], model(), false, p);
        reply(a, p, d, json!({"code": code, "assumptions": []}))
    }

    #[test]
    fn copies_of_the_namespace_definitions_in_an_answer_are_dropped_before_checking() {
        let (mut a, mut p, mut d, id) = setup();
        let file = "(ns shop.orders)\n\n(def rate 2)\n\n(defn order-totals [orders] orders)\n";
        {
            let doc = d.0.get_mut(&id).unwrap();
            let binding = clojurecraft_domain::ProjectBinding {
                namespace: "shop.orders".into(),
                function: "order-totals".into(),
                helpers: Vec::new(),
                arguments: 1,
                file: file.into(),
                usable: Vec::new(),
            };
            doc.implementation = crate::project::fragment(&binding).map(Implementation::manual);
            doc.project = Some(binding);
        }
        let answer = "(ns shop.orders)\n\n(def rate   2)\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders) (fn [os] (* rate (reduce + (map :amount os))))))\n";
        generate_code(&mut a, &mut p, &mut d, id, answer);
        let run = p.validations.last().expect("checked, not refused");
        assert_eq!(run.code.matches("(def rate").count(), 1, "{}", run.code);
        validated(&mut a, &mut p, &mut d, false);
        let note = a.job().map(|j| j.attempts()[0].note.clone()).unwrap_or_default();
        assert!(note.is_some_and(|n| n.contains("rate")), "the copy is mentioned");
    }

    #[test]
    fn new_project_code_that_stops_using_a_namespace_definition_is_flagged_for_review() {
        let (mut a, mut p, mut d, id) = setup();
        let file = "(ns shop.orders)\n\n(def rate 2)\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders) (fn [os] (* rate (count os)))))\n";
        {
            let doc = d.0.get_mut(&id).unwrap();
            let binding = clojurecraft_domain::ProjectBinding {
                namespace: "shop.orders".into(),
                function: "order-totals".into(),
                helpers: Vec::new(),
                arguments: 1,
                file: file.into(),
                usable: Vec::new(),
            };
            doc.implementation = crate::project::fragment(&binding).map(Implementation::manual);
            doc.project = Some(binding);
        }
        let answer = "(ns shop.orders)\n\n(defn order-totals [orders]\n  (update-vals (group-by :customer orders) (fn [os] (* 2 (count os)))))\n";
        generate_code(&mut a, &mut p, &mut d, id, answer);
        validated(&mut a, &mut p, &mut d, true);
        compared_alike(&mut a, &mut p, &mut d);
        let Some(Report::Pending(pending)) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(pending.differences.len(), 1, "{:?}", pending.differences);
        assert!(pending.differences[0].contains("no longer uses rate"));
    }

    #[test]
    fn a_generation_plans_retrieves_writes_validates_and_commits() {
        let (mut a, mut p, mut d, id) = setup();
        a.generate(&d.0[&id], model(), true, &mut p);
        assert_eq!(p.last().1, Role::RetrievalPlanning);
        reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"symbols": ["group-by", "update-vals"], "concepts": ["summing"]}),
        );
        assert_eq!(a.job().unwrap().phase, Phase::Retrieving);
        let gather = p.gathers.last().unwrap().clone();
        assert!(gather.queries.iter().any(|(q, _)| q == "update-vals"));
        assert!(gather.queries.iter().any(|(q, _)| q == "sum :amount"));
        assert_eq!(gather.core_candidates, ["order-totals"]);
        assert_eq!(gather.clojure_version.as_deref(), Some("1.12.6"));

        a.on_gathered(1, gathered(&[]), &mut p);
        assert_eq!(p.last().1, Role::Generation);
        assert!(p.sent.last().unwrap().1.seed.is_some());
        // Streamed output of the current request is shown; another request's is not.
        let (current, _) = p.last();
        let stream = |request, t: &str| ServiceEvent::Stream {
            request,
            event: StreamEvent::Content(t.into()),
        };
        a.on_model(&stream(current, "{\"code\""), &mut d, &mut p);
        a.on_model(&stream(99, "stale"), &mut d, &mut p);
        assert_eq!(a.job().unwrap().output, "{\"code\"");

        let updates = reply(&mut a, &mut p, &mut d, json!({"code": CODE}));
        assert!(updates.is_empty());
        assert_eq!(a.job().unwrap().phase, Phase::Validating);
        assert_eq!(a.job().unwrap().attempt(), Some((1, MAX_ATTEMPTS)));
        let req = p.validations.last().unwrap();
        assert_eq!(req.function, "order-totals");
        assert_eq!(req.calls[0].expected.as_deref(), Some("{1 2}"));

        let updates = validated(&mut a, &mut p, &mut d, true);
        let stored = reports(&updates);
        assert!(stored.len() == 1 && stored[0].passed() && stored[0].attempt == Some(1));
        assert_eq!(stored[0].source.as_deref(), Some(CODE));
        assert!(
            matches!(&updates[1..], [Update::Commit(f, r)] if *f == id && r.kind == RevisionKind::Generated)
        );
        assert!(
            p.sent.iter().all(|(_, r)| r.role != Role::Explanation),
            "explaining is asked for separately"
        );
        assert!(a.job().is_none());
        let Some(Report::Generated { attempts, .. }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(attempts.len(), 1);
        assert_eq!(p.validations.len(), 1, "a passing candidate stops the loop");

        let doc = &d.0[&id];
        let i = doc.implementation.as_ref().unwrap();
        assert_eq!(i.source, CODE);
        let g = i.generation.as_ref().unwrap();
        assert_eq!(g.spec_basis, doc.spec_fingerprint());
        assert_eq!(g.model.repository, "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF");
        assert_eq!(g.runtime.llama_cpp_release, "b12138");
        assert_eq!(g.settings.context_length, 8192);
        assert_eq!(g.function_name, "order-totals");
        assert_eq!(g.attempts, 1);
        assert!(g.explanation.is_none());
        // The stored report describes the code that is now the implementation.
        assert!(stored[0].freshness(&doc.basis()).is_current());
    }

    fn sections_of(r: &GenerateRequest) -> Vec<(Placement, &str)> {
        r.sections
            .iter()
            .map(|s| (s.placement, s.text.as_str()))
            .collect()
    }

    #[test]
    fn failing_code_is_analysed_then_fixed_in_the_same_conversation() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        let updates = validated(&mut a, &mut p, &mut d, false);
        assert!(!reports(&updates)[0].passed());

        // First the model is asked why the code failed, without writing code.
        let job = a.job().unwrap();
        assert_eq!(job.phase, Phase::Analyzing);
        assert_eq!(job.attempt_kind(), Some(AttemptKind::Fix(1)));
        assert_eq!(job.progress().as_deref(), Some("Attempt 1 (4 more at most)"));
        let (first, ask) = (p.sent[0].1.clone(), p.sent[1].1.clone());
        let (first, ask) = (&first, &ask);
        assert_ne!(first.seed, ask.seed);
        assert_eq!(ask.grammar, None);
        assert_eq!(ask.max_output_tokens, Some(requests::ANALYSIS_TOKENS));
        assert!(ask.tools.is_empty(), "no runtime, no tool");
        // The first request is the opening of the conversation, then the answer and the reply.
        assert_eq!(&ask.sections[..first.sections.len()], &first.sections[..]);
        let rest = sections_of(ask)[first.sections.len()..].to_vec();
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].0, Placement::Assistant);
        assert!(
            rest[0].1.contains("count))"),
            "the model's own answer: {}",
            rest[0].1
        );
        assert_eq!(rest[1].0, Placement::User);
        for needle in [
            "count))",
            "Example 1 — failed",
            "Expected: {1 2}",
            "Actual: {1 3}",
            "at [1]: expected 2 but got 3",
            "Find out why Example 1 (input [{:customer 1 :amount 2}])",
            "Do not write code yet",
        ] {
            assert!(rest[1].1.contains(needle), "missing {needle:?} in\n{}", rest[1].1);
        }

        // Then the fix, after the analysis, with the grammar for changed forms.
        analysis(&mut a, &mut p, &mut d);
        let fix = &p.sent.last().unwrap().1;
        assert_eq!(fix.temperature, Some(requests::FIX_TEMPERATURE));
        assert_eq!(fix.grammar, Some(crate::grammar::fix()));
        let rest = sections_of(fix)[first.sections.len()..].to_vec();
        assert_eq!(rest.len(), 4);
        assert_eq!(rest[2], (Placement::Assistant, "count is used instead of a sum"));
        assert!(rest[3].1.contains("only the top-level forms you change"));

        // The answer brings only the changed function, which replaces the old one.
        let body = "(->> orders\n       (group-by :customer)\n       (update-vals (fn [os] (reduce + (map :amount os)))))";
        reply_text(&mut a, &mut p, &mut d, &fix_text(body));
        assert_eq!(p.validations.last().unwrap().code, CODE);
        let updates = validated(&mut a, &mut p, &mut d, true);
        assert_eq!(reports(&updates)[0].attempt, Some(2));
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { attempts, notes }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        let labels: Vec<String> = attempts.iter().map(Attempt::label).collect();
        assert_eq!(labels, ["Generate", "Attempt 1"]);
        assert!(
            attempts[1]
                .note
                .as_deref()
                .is_some_and(|n| n.contains("The model changed order-totals.")),
            "{attempts:?}"
        );
        assert!(
            notes.iter().any(|n| n.contains("passed after 1 fix.")),
            "{notes:?}"
        );
        let doc = &d.0[&id];
        assert_eq!(doc.implementation.as_ref().unwrap().source, CODE);
        let g = doc.implementation.as_ref().unwrap().generation.clone().unwrap();
        assert_eq!(g.attempts, 2);
        assert_eq!(g.settings.temperature, requests::FIX_TEMPERATURE);
        // All runs share the generation run.
        assert_eq!(
            attempts[0].report.as_ref().unwrap().run,
            attempts[1].report.as_ref().unwrap().run
        );
    }

    #[test]
    fn the_conversation_keeps_every_turn_with_older_ones_droppable() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        failed_with(&mut a, &mut p, &mut d, "3");
        analysis(&mut a, &mut p, &mut d);
        let one = fix_text("(update-vals (group-by :customer orders) (fn [os] (inc (count os))))");
        reply_text(&mut a, &mut p, &mut d, &one);
        failed_with(&mut a, &mut p, &mut d, "4");

        let job = a.job().unwrap();
        assert_eq!(job.attempt_kind(), Some(AttemptKind::Fix(2)));
        let req = &p.sent.last().unwrap().1;
        let opening = p.sent[0].1.sections.len();
        let turns = &req.sections[opening..];
        let placements: Vec<Placement> = turns.iter().map(|s| s.placement).collect();
        use Placement::{Assistant, User};
        assert_eq!(placements, [Assistant, User, Assistant, User, Assistant, User]);
        // The model sees its own answers exactly as it gave them.
        assert_eq!(turns[2].text, "count is used instead of a sum");
        assert_eq!(turns[4].text, one);
        assert!(turns[5].text.contains("(inc (count os))"));
        assert!(turns[5].text.contains("expected 2 but got 4"));
        assert!(matches!(
            turns[0].priority,
            Priority::Optional(requests::HISTORY_DROP_ORDER)
        ));
        assert_eq!(turns[5].priority, Priority::Required);
    }

    #[test]
    fn the_same_load_failure_outside_the_function_twice_means_the_file_is_broken() {
        let attempt = |number: u32, diagnostic: &str| Attempt {
            number,
            kind: AttemptKind::Fix(number),
            code: Some("(ns a)".into()),
            report: Some(ValidationReport {
                id: clojurecraft_domain::ValidationId::generate(),
                created_at: Timestamp::now(),
                basis: FunctionDocument::new("f").basis(),
                environment: env(),
                checks: vec![clojurecraft_domain::Check {
                    subject: clojurecraft_domain::CheckSubject::Load,
                    status: clojurecraft_domain::CheckStatus::Error,
                    expected: None,
                    actual: None,
                    diagnostics: vec![diagnostic.into()],
                    duration_ms: None,
                }],
                attempt: None,
                run: None,
                source: None,
                settings: None,
            }),
            note: None,
        };
        let outside = "No such var: db/x (line 149 of the namespace file, outside this function)";
        let unusable = Attempt {
            number: 3,
            kind: AttemptKind::Fix(3),
            code: None,
            report: None,
            note: Some("cut off".into()),
        };
        assert_eq!(
            broken_file(&[attempt(1, outside)]),
            None,
            "once may be the function's doing"
        );
        assert_eq!(
            broken_file(&[attempt(1, outside), attempt(2, outside), unusable]).as_deref(),
            Some(outside)
        );
        assert_eq!(
            broken_file(&[attempt(1, outside), attempt(2, "No such var: y")]),
            None
        );
        assert_eq!(
            broken_file(&[attempt(1, "Unable to resolve z"), attempt(2, outside)]),
            None
        );
    }

    #[test]
    fn an_error_an_earlier_answer_had_fixed_is_reported_as_back() {
        let attempt = |number: u32, diagnostic: &str| Attempt {
            number,
            kind: if number == 0 {
                AttemptKind::Generate
            } else {
                AttemptKind::Fix(number)
            },
            code: Some(format!("(ns a) ; {number}")),
            report: Some(ValidationReport {
                id: clojurecraft_domain::ValidationId::generate(),
                created_at: Timestamp::now(),
                basis: FunctionDocument::new("f").basis(),
                environment: env(),
                checks: vec![clojurecraft_domain::Check {
                    subject: clojurecraft_domain::CheckSubject::Load,
                    status: clojurecraft_domain::CheckStatus::Error,
                    expected: None,
                    actual: None,
                    diagnostics: vec![diagnostic.into()],
                    duration_ms: None,
                }],
                attempt: None,
                run: None,
                source: None,
                settings: None,
            }),
            note: None,
        };
        let ifn = |line: u32| {
            format!(
                "The function threw line {line}: ClassCastException: class java.lang.Long cannot be cast \
                 to class clojure.lang.IFn\nThe line that threw: (-> n dec + 2)\nValues of the locals: n = {line}"
            )
        };
        let number = "The function threw line 20: ClassCastException: Duration cannot be cast to Number";
        let wrong = "the output: expected #time/duration \"PT36S\" but got #time/duration \"PT38S\"";
        let run = [
            attempt(0, number),
            attempt(1, &ifn(11)),
            attempt(2, number),
            attempt(3, wrong),
            attempt(4, &ifn(12)),
        ];
        assert_eq!(regression(&run), Some(3), "attempt 1, moved by a line");
        assert_eq!(regression(&run[..4]), None, "new failure");
        assert_eq!(regression(&run[..3]), Some(2));
        let again = [attempt(0, number), attempt(1, wrong), attempt(2, wrong)];
        assert_eq!(regression(&again), None, "never fixed: a stuck conversation");
        let restarted = [
            attempt(0, &ifn(1)),
            attempt(1, wrong),
            attempt(0, number),
            attempt(1, &ifn(1)),
        ];
        assert_eq!(regression(&restarted), None, "before the fresh start");
    }

    #[test]
    fn a_stuck_conversation_starts_over_with_a_fresh_generation() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        validated(&mut a, &mut p, &mut d, false);
        analysis(&mut a, &mut p, &mut d);
        // The fix changes nothing that matters: the code fails exactly as before.
        reply_text(
            &mut a,
            &mut p,
            &mut d,
            &fix_text("(update-vals (group-by :customer orders) (fn [os] (count os)))"),
        );
        validated(&mut a, &mut p, &mut d, false);

        let job = a.job().unwrap();
        assert_eq!(job.phase, Phase::Generating);
        assert_eq!(job.attempt_kind(), Some(AttemptKind::Restart));
        let req = &p.sent.last().unwrap().1;
        assert_eq!(req.temperature, Some(requests::RESTART_TEMPERATURE));
        assert_eq!(req.sections, p.sent[0].1.sections, "the history is gone");

        reply(&mut a, &mut p, &mut d, json!({"code": CODE, "assumptions": []}));
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { attempts, notes }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        let labels: Vec<String> = attempts.iter().map(Attempt::label).collect();
        assert_eq!(labels, ["Generate", "Attempt 1", "Fresh start"]);
        assert!(notes.iter().any(|n| n.contains("started over")), "{notes:?}");
        assert!(
            notes.iter().any(|n| n.contains("after a fresh start")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_unusable_fix_is_asked_for_again_from_the_same_point() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        validated(&mut a, &mut p, &mut d, false);
        analysis(&mut a, &mut p, &mut d);
        let before = p.sent.last().unwrap().1.sections.clone();
        reply_text(&mut a, &mut p, &mut d, "no code here");
        let job = a.job().unwrap();
        assert_eq!(job.phase, Phase::Repairing);
        assert_eq!(job.attempt_kind(), Some(AttemptKind::Fix(2)));
        assert_eq!(p.sent.last().unwrap().1.sections, before);
        assert_ne!(p.sent.last().unwrap().1.seed, p.sent[p.sent.len() - 2].1.seed);
    }

    #[test]
    fn a_failed_analysis_still_leads_to_a_fix() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        validated(&mut a, &mut p, &mut d, false);
        let (request, _) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            request,
            Err(GenerationError::ToolRounds(4)),
        );
        assert_eq!(a.job().unwrap().phase, Phase::Repairing);
        let fix = sections_of(&p.sent.last().unwrap().1);
        assert!(fix.contains(&(Placement::Assistant, "(no analysis)")));
    }

    #[test]
    fn the_last_failed_attempt_ends_with_visible_diagnostics() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        let mut stored = Vec::new();
        let mut last = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            assert_eq!(a.job().unwrap().attempt(), Some((attempt, MAX_ATTEMPTS)));
            stored.extend(
                reports(&failed_with(&mut a, &mut p, &mut d, &(2 + attempt).to_string()))
                    .into_iter()
                    .cloned(),
            );
            if attempt < MAX_ATTEMPTS {
                analysis(&mut a, &mut p, &mut d);
                let body =
                    format!("(update-vals (group-by :customer orders) (fn [os] (+ {attempt} (count os))))");
                reply_text(&mut a, &mut p, &mut d, &fix_text(&body));
                last = p.validations.last().unwrap().code.clone();
            }
        }
        assert!(a.job().is_none());
        let fixes = MAX_ATTEMPTS - 1;
        assert_eq!(
            p.sent.len() as u32,
            1 + 2 * fixes,
            "the generation, then an analysis and a fix per attempt"
        );
        assert_eq!(
            stored.len() as u32,
            MAX_ATTEMPTS,
            "every attempt's report is stored"
        );
        let Some(Report::Unresolved {
            attempts,
            best,
            notes,
        }) = a.report(id)
        else {
            panic!("{:?}", a.report(id))
        };
        let labels: Vec<String> = attempts.iter().map(Attempt::label).collect();
        assert_eq!(
            labels,
            [
                "Generate",
                "Attempt 1",
                "Attempt 2",
                "Attempt 3",
                "Attempt 4",
                "Attempt 5"
            ]
        );
        assert!(notes.iter().any(|n| n.contains("No attempt passed")));
        assert!(
            d.0[&id].implementation.is_none(),
            "failing code is never applied silently"
        );

        // The user may still choose the best candidate; it is recorded as not passing.
        assert!(best.is_some());
        let updates = a.accept_pending(d.0.get_mut(&id).unwrap()).unwrap();
        assert!(matches!(&updates[..], [Update::Commit(..)]));
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, last);
        let Some(Report::Generated { notes, .. }) = a.report(id) else {
            panic!()
        };
        assert!(notes.iter().any(|n| n.contains("did not pass every check")));
    }

    #[test]
    fn structurally_invalid_code_is_a_failed_attempt_and_is_fixed() {
        let (mut a, mut p, mut d, id) = setup();
        let slurp = "(ns order-totals.core)\n(defn order-totals [x] (slurp x))";
        let updates = generate_code(&mut a, &mut p, &mut d, id, slurp);
        assert!(p.validations.is_empty(), "nothing runs");
        let r = reports(&updates)[0];
        let structure = r
            .checks
            .iter()
            .find(|c| c.subject == CheckSubject::Structure)
            .unwrap();
        assert_eq!(structure.status, CheckStatus::Forbidden);
        assert!(structure.diagnostics[0].contains("slurp is not allowed"));
        assert_eq!(a.job().unwrap().phase, Phase::Analyzing);
        let text = p.last_text();
        assert!(text.contains("slurp is not allowed"));
        assert!(
            text.contains("Find out why the check \"Structure and dependencies\" failed"),
            "{text}"
        );
        assert!(d.0[&id].implementation.is_none());
    }

    #[test]
    fn unreadable_code_is_fixed_with_a_complete_namespace() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(
            &mut a,
            &mut p,
            &mut d,
            id,
            "(ns order-totals.core)\n(defn order-totals [x] (str \"x))",
        );
        analysis(&mut a, &mut p, &mut d);
        assert!(
            p.last_text().contains("the complete namespace"),
            "{}",
            p.last_text()
        );
        reply_text(&mut a, &mut p, &mut d, &format!("```clojure\n{CODE}```"));
        assert_eq!(p.validations.last().unwrap().code, CODE);
    }

    #[test]
    fn unbalanced_delimiters_and_comment_forms_are_fixed_before_checking() {
        let (mut a, mut p, mut d, id) = setup();
        let sloppy = "(ns order-totals.core)\n\n(defn order-totals [orders]\n  (->> orders\n       (group-by :customer)\n       (update-vals (fn [os] (reduce + (map :amount os)))))))\n\n(comment\n  (order-totals []))\n";
        generate_code(&mut a, &mut p, &mut d, id, sloppy);
        let req = p.validations.last().expect("the fixed code runs");
        assert_eq!(req.code, format!("{CODE}\n"));
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { attempts, .. }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        let note = attempts[0].note.as_deref().unwrap_or_default();
        assert!(note.contains("inferred from the indentation"), "{note}");
        assert!(note.contains("(comment …)"), "{note}");
    }

    #[test]
    fn malformed_answers_count_as_attempts() {
        let (mut a, mut p, mut d, id) = setup();
        a.generate(&d.0[&id], model(), false, &mut p);
        let (req, _) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::Malformed {
                problem: "not JSON".into(),
                raw: "Sure! (defn".into(),
            }),
        );
        // Without earlier code the next attempt writes the code afresh.
        assert_eq!(a.job().unwrap().attempt(), Some((2, MAX_ATTEMPTS)));
        assert!(!p.last_text().contains("Your previous implementation"));
        assert_eq!(
            a.job().unwrap().attempts()[0]
                .note
                .as_deref()
                .map(|n| n.contains("not JSON")),
            Some(true)
        );

        reply(&mut a, &mut p, &mut d, json!({"no_code": true}));
        assert_eq!(a.job().unwrap().attempt(), Some((3, MAX_ATTEMPTS)));
    }

    #[test]
    fn editing_the_specification_during_validation_holds_the_result_for_review() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        assert_eq!(a.job().unwrap().phase, Phase::Validating);
        // The user changes an expected output while the code runs.
        d.0.get_mut(&id).unwrap().examples[0].expected = Edn::new("{1 5}");
        let updates = validated(&mut a, &mut p, &mut d, true);
        let r = reports(&updates)[0].clone();
        assert!(r.passed());
        assert!(
            !r.freshness(&d.0[&id].basis_with_code(CODE)).is_current(),
            "the report is about the earlier specification"
        );
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Pending(pending)) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert!(pending.spec_changed && pending.passed());
        assert!(
            d.0[&id].implementation.is_none(),
            "stale results never replace current work"
        );
        assert_eq!(d.0[&id].examples[0].expected.as_str(), "{1 5}");
    }

    #[test]
    fn cancelling_during_validation_stops_the_run_and_ignores_its_result() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        a.cancel(&mut p);
        assert_eq!(p.cancelled_validations, [1]);
        assert_eq!(a.report(id), Some(&Report::Cancelled));
        assert!(validated(&mut a, &mut p, &mut d, true).is_empty());
        assert!(d.0[&id].implementation.is_none());
    }

    #[test]
    fn a_late_validation_result_cannot_affect_a_newer_job() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        a.cancel(&mut p);
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        // The first run's result arrives after the second run started.
        let e = execution(&p, true);
        assert!(a.on_validated(1, e, &mut d, &mut p).is_empty());
        assert_eq!(a.job().unwrap().phase, Phase::Validating);
        assert!(a.job().unwrap().attempts().is_empty());
    }

    #[test]
    fn without_a_runtime_the_code_is_kept_but_marked_as_not_run() {
        let (mut a, mut p, mut d, id) = setup();
        p.no_runtime = true;
        let updates = generate_code(&mut a, &mut p, &mut d, id, CODE);
        assert!(reports(&updates).is_empty());
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { notes, attempts }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert!(
            notes
                .iter()
                .any(|n| n.contains("not run: The Java runtime is not installed")),
            "{notes:?}"
        );
        assert!(attempts[0].report.is_none());
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, CODE);
    }

    #[test]
    fn a_name_that_shadows_clojure_core_is_avoided() {
        let (mut a, mut p, mut d, id) = setup();
        d.0.get_mut(&id).unwrap().name = "Frequencies".into();
        a.generate(&d.0[&id], model(), true, &mut p);
        reply(&mut a, &mut p, &mut d, json!({"symbols": [], "concepts": []}));
        a.on_gathered(1, gathered(&["frequencies"]), &mut p);
        let task = &p.sent.last().unwrap().1.sections[0].text;
        assert!(task.contains("(ns frequencies-fn.core)"), "{task}");
    }

    #[test]
    fn without_documentation_generation_starts_directly() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        assert!(p.gathers.is_empty());
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { notes, .. }) = a.report(id) else {
            panic!()
        };
        assert!(notes.iter().any(|n| n.contains("No Clojure documentation")));
    }

    #[test]
    fn code_needing_another_namespace_waits_for_approval() {
        let (mut a, mut p, mut d, id) = setup();
        let code = "(ns order-totals.core (:require [clojure.walk :as walk]))\n(defn order-totals [x] (walk/keywordize-keys x))\n";
        generate_code(&mut a, &mut p, &mut d, id, code);
        validated(&mut a, &mut p, &mut d, true);
        assert!(reply(&mut a, &mut p, &mut d, explanation()).is_empty());
        let Some(Report::Pending(pending)) = a.report(id) else {
            panic!()
        };
        assert_eq!(pending.needs_approval, ["clojure.walk"]);
        assert!(
            d.0[&id].implementation.is_none(),
            "nothing changes before approval"
        );
        assert!(d.0[&id].contract.is_none());

        let doc = d.0.get_mut(&id).unwrap();
        let updates = a.accept_pending(doc).unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(
            doc.contract.as_ref().unwrap().allowed_dependencies,
            ["clojure.set", "clojure.string", "clojure.walk"]
        );
        assert_eq!(doc.implementation.as_ref().unwrap().source, code);
    }

    #[test]
    fn code_replacing_hand_written_code_is_compared_with_it_on_variants_of_the_examples() {
        let (mut a, mut p, mut d, id) = setup();
        d.0.get_mut(&id).unwrap().implementation = Some(Implementation::manual(WRONG));
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        assert_eq!(a.job().unwrap().phase, Phase::Comparing);
        let current = p.validations.last().unwrap();
        assert!(current.code.contains("count"), "the current code runs first");
        let probes: Vec<String> = current.calls.iter().map(|c| c.input.clone()).collect();
        assert!(probes.contains(&"[]".to_owned()), "{probes:?}");
        assert!(current.calls.iter().all(|c| c.expected.is_none()));

        let mut values: Vec<Option<&str>> = vec![Some("{1 1}"); probes.len()];
        values[0] = None;
        let e = returning(&p, &values);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        let candidate = p.validations.last().unwrap();
        assert!(candidate.code.contains("reduce +"), "then the new code");
        assert_eq!(candidate.calls[0].expected, None, "the current code threw");
        assert_eq!(candidate.calls[1].expected.as_deref(), Some("{1 1}"));

        let mut out = String::from("{\"event\":\"read\",\"ok\":true}\n{\"event\":\"load\",\"ok\":true}\n");
        out.push_str("{\"event\":\"result\",\"index\":0,\"status\":\"ok\",\"actual\":\"{}\"}\n");
        out.push_str(
            "{\"event\":\"result\",\"index\":1,\"status\":\"ok\",\"actual\":\"{1 2}\",\"match\":false}\n",
        );
        for i in 2..probes.len() {
            out.push_str(&format!(
                "{{\"event\":\"result\",\"index\":{i},\"status\":\"ok\",\"actual\":\"{{1 1}}\",\"match\":true}}\n"
            ));
        }
        out.push_str("{\"event\":\"done\"}\n");
        let e = Execution {
            outcome: Ok(EvalOutcome {
                status: EvalStatus::Completed,
                report: HarnessReport::parse(&out),
                stderr: String::new(),
                elapsed: Duration::from_millis(600),
            }),
            environment: env(),
        };
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert!(a.job().is_none());
        let Some(Report::Pending(pending)) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(pending.differences.len(), 2, "{:?}", pending.differences);
        assert!(pending.differences[0].contains("returns {1 1}, the new code returns {1 2}"));
        assert!(
            pending.differences[1]
                .contains("the current code throws NullPointerException, the new code returns {}")
        );
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, WRONG);
    }

    #[test]
    fn cancelling_the_comparison_still_holds_the_code_for_review() {
        let (mut a, mut p, mut d, id) = setup();
        d.0.get_mut(&id).unwrap().implementation = Some(Implementation::manual(WRONG));
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        a.cancel(&mut p);
        let Some(Report::Pending(pending)) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert!(pending.notes.iter().any(|n| n.contains("cancelled")));
    }

    #[test]
    fn hand_written_code_is_kept_until_the_user_chooses_the_new_code() {
        let (mut a, mut p, mut d, id) = setup();
        d.0.get_mut(&id).unwrap().implementation = Some(Implementation::manual("(ns mine)"));
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, "(ns mine)");
        let Some(Report::Pending(pending)) = a.report(id) else {
            panic!()
        };
        assert!(pending.replaces_manual && pending.needs_approval.is_empty());
        a.discard_pending(id);
        assert!(a.report(id).is_none());
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, "(ns mine)");
    }

    #[test]
    fn a_cancelled_request_never_changes_the_function() {
        let (mut a, mut p, mut d, id) = setup();
        a.generate(&d.0[&id], model(), false, &mut p);
        let (req, role) = p.last();
        a.cancel(&mut p);
        assert_eq!(p.cancelled, [req]);
        assert_eq!(a.report(id), Some(&Report::Cancelled));
        // The answer arriving afterwards is ignored.
        let updates = finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Ok(answer(role, json!({"code": CODE, "assumptions": []}), vec![])),
        );
        assert!(updates.is_empty());
        assert!(d.0[&id].implementation.is_none());
        assert!(p.validations.is_empty());

        // A new job is unaffected by the old request's late events.
        a.interpret(&d.0[&id], &mut p);
        let updates = finish(&mut a, &mut p, &mut d, req, Ok(answer(role, json!({}), vec![])));
        assert!(updates.is_empty());
        assert_eq!(a.job().unwrap().kind, JobKind::Interpret);
    }

    #[test]
    fn interpretation_keeps_user_text_and_asks_open_questions() {
        let (mut a, mut p, mut d, id) = setup();
        let before = d.0[&id].clone();
        a.interpret(&d.0[&id], &mut p);
        assert_eq!(p.last().1, Role::Interpretation);
        reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"restatement": "Sums each customer's amounts.", "assumptions": [],
                   "issues": [{"kind": "gap", "detail": "Empty input is not covered", "refers_to": ["description"]}]}),
        );
        assert_eq!(p.last().1, Role::Clarification);
        let updates = reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"questions": [{"question": "What should an empty input return?", "why": "gap",
                                  "options": ["{}", "nil"], "refers_to": ["description"]}]}),
        );
        assert_eq!(updates, [Update::Edited(id)]);
        let doc = &d.0[&id];
        assert_eq!(
            doc.description, before.description,
            "the user's text is never rewritten"
        );
        assert_eq!(
            doc.interpretation.as_ref().unwrap().restatement,
            "Sums each customer's amounts."
        );
        assert_eq!(doc.clarifications.len(), 1);
        assert_eq!(doc.clarifications[0].status, ClarificationStatus::Open);
        assert_eq!(
            doc.spec_fingerprint(),
            before.spec_fingerprint(),
            "nothing became a requirement"
        );
        assert!(matches!(
            a.report(id),
            Some(Report::Interpreted { questions: 1, .. })
        ));
    }

    #[test]
    fn contract_proposals_never_replace_an_accepted_contract() {
        let (mut a, mut p, mut d, id) = setup();
        let proposal = json!({"input": {"description": "orders", "value_type": "vector", "keys": []},
                              "output": {"description": "", "value_type": "map", "keys": []},
                              "ordering": "unspecified", "sorted_by": "", "notes": ""});
        a.propose_contract(&d.0[&id], &mut p);
        reply(&mut a, &mut p, &mut d, proposal.clone());
        let c = d.0[&id].contract.clone().unwrap();
        assert_eq!(c.status, ContractStatus::Proposed);
        assert_eq!(d.0[&id].spec_fingerprint(), doc_with_same_spec(&d.0[&id]));

        // The user accepts a contract while a new proposal is being written.
        a.propose_contract(&d.0[&id], &mut p);
        let mut accepted = Contract::empty(ContractStatus::Accepted);
        accepted.notes = "mine".into();
        d.0.get_mut(&id).unwrap().contract = Some(accepted.clone());
        reply(&mut a, &mut p, &mut d, proposal);
        assert_eq!(d.0[&id].contract.as_ref(), Some(&accepted));
        assert!(matches!(a.report(id), Some(Report::Failed { .. })));
    }

    fn doc_with_same_spec(d: &FunctionDocument) -> Fingerprint {
        let mut without = d.clone();
        without.contract = None;
        without.spec_fingerprint()
    }

    #[test]
    fn incomplete_specifications_and_missing_models_are_reported() {
        let (mut a, mut p, mut d, id) = setup();
        d.0.get_mut(&id).unwrap().examples[0].expected = Edn::new("{1");
        a.generate(&d.0[&id], model(), true, &mut p);
        let Some(Report::Blocked(why)) = a.report(id) else {
            panic!()
        };
        assert!(why[0].contains("expected output is not valid EDN"));
        assert!(p.sent.is_empty() && a.job().is_none());

        let (mut a, mut p, d, id) = setup();
        p.no_model = true;
        a.interpret(&d.0[&id], &mut p);
        let Some(Report::Failed { message, .. }) = a.report(id) else {
            panic!()
        };
        assert_eq!(message, "No model is selected.");
        assert!(a.job().is_none());
    }

    #[test]
    fn context_overflow_offers_its_remedies() {
        let (mut a, mut p, mut d, id) = setup();
        a.generate(&d.0[&id], model(), false, &mut p);
        let (req, _) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::ContextExceeded {
                needed: 9000,
                context: 8192,
                reserved_output: 2500,
                largest: vec![("Example 1".into(), 6000)],
                remedies: vec![
                    Remedy::LargerContext(10240),
                    Remedy::ShortenInput(vec!["Example 1".into()]),
                ],
            }),
        );
        let Some(Report::Failed { remedies, .. }) = a.report(id) else {
            panic!()
        };
        assert_eq!(remedies.len(), 2);
    }

    #[test]
    fn omitted_documentation_is_not_recorded_as_used() {
        let (mut a, mut p, mut d, id) = setup();
        a.generate(&d.0[&id], model(), true, &mut p);
        reply(&mut a, &mut p, &mut d, json!({"symbols": [], "concepts": []}));
        a.on_gathered(1, gathered(&[]), &mut p);
        let (req, role) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Ok(answer(
                role,
                json!({"code": CODE, "assumptions": []}),
                vec![(clojurecraft_knowledge::prompt::SECTION_LABEL.into(), 900)],
            )),
        );
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        let g = d.0[&id]
            .implementation
            .as_ref()
            .unwrap()
            .generation
            .clone()
            .unwrap();
        assert!(g.documentation.is_empty());
        assert_eq!(g.omitted_context, [clojurecraft_knowledge::prompt::SECTION_LABEL]);
    }

    #[test]
    fn generated_code_records_the_prompt_as_sent() {
        let (mut a, mut p, mut d, id) = setup();
        a.generate(&d.0[&id], model(), false, &mut p);
        let (req, role) = p.last();
        let sent = p.sent.last().unwrap().1.clone();
        let mut g = answer(role, json!({"code": CODE, "assumptions": []}), vec![]);
        g.plan.included = (0..=sent.sections.len()).collect();
        finish(&mut a, &mut p, &mut d, req, Ok(g));
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        let prompt = &d.0[&id]
            .implementation
            .as_ref()
            .unwrap()
            .generation
            .as_ref()
            .unwrap()
            .prompt;
        assert_eq!(*prompt, requests::prompt_messages(&sent, None));
        assert_eq!(
            prompt.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            ["system", "user"]
        );
        assert!(prompt[1].content.contains(&d.0[&id].description));
    }

    #[test]
    fn a_reasoning_model_cut_off_while_thinking_is_asked_again_with_more_room() {
        let (mut a, mut p, mut d, id) = setup();
        a.interpret(&d.0[&id], &mut p);
        let (req, _) = p.last();
        assert_eq!(p.sent[0].1.max_output_tokens, None);
        let cut = |raw: &str, reasoning_chars| {
            Err(GenerationError::CutOff {
                limit: 1500,
                raw: raw.into(),
                reasoning_chars,
            })
        };
        finish(&mut a, &mut p, &mut d, req, cut("", 9000));
        assert_eq!(p.sent.len(), 2, "sent again");
        let budget = Role::Interpretation.profile().max_output_tokens + REASONING_ALLOWANCE;
        assert_eq!(p.sent[1].1.max_output_tokens, Some(budget));
        assert_eq!(a.job().unwrap().phase, Phase::Interpreting);

        // A second cut-off ends the job, showing the partial answer.
        let (req, _) = p.last();
        finish(&mut a, &mut p, &mut d, req, cut("{\"restatement\": \"Sums", 9000));
        let Some(Report::Failed { message, raw, .. }) = a.report(id) else {
            panic!()
        };
        assert!(message.contains("reasoned at length"), "{message}");
        assert!(raw.as_deref().unwrap().starts_with("{\"restatement\""));

        // Later requests start with the larger budget.
        a.generate(&d.0[&id], model(), false, &mut p);
        let gen_budget = Role::Generation.profile().max_output_tokens + REASONING_ALLOWANCE;
        assert_eq!(p.sent.last().unwrap().1.max_output_tokens, Some(gen_budget));
        // A plain grammar would stop the model from reasoning, so only the schema is sent.
        assert_eq!(p.sent.last().unwrap().1.grammar, None);
    }

    #[test]
    fn an_answer_cut_off_without_reasoning_is_not_retried() {
        let (mut a, mut p, mut d, id) = setup();
        a.interpret(&d.0[&id], &mut p);
        let (req, _) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::CutOff {
                limit: 1500,
                raw: "{".into(),
                reasoning_chars: 0,
            }),
        );
        assert_eq!(p.sent.len(), 1);
        assert!(matches!(a.report(id), Some(Report::Failed { .. })));
    }

    #[test]
    fn explaining_generated_code_gets_the_specification_and_the_code_and_stores_steps_and_assumptions() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        a.explain(&d.0[&id], &mut p);
        let (_, request) = p.sent.last().unwrap().clone();
        assert_eq!(request.role, Role::Explanation);
        let text: Vec<&str> = request.sections.iter().map(|s| s.text.as_str()).collect();
        assert!(text.iter().any(|t| t.contains(&d.0[&id].description)));
        assert!(text.iter().any(|t| t.starts_with("Example 1")));
        assert!(text.iter().any(|t| t.contains(CODE.trim_end())));
        let updates = reply(&mut a, &mut p, &mut d, explanation());
        assert_eq!(updates, [Update::Edited(id)]);
        assert!(matches!(a.report(id), Some(Report::Explained { .. })));
        let g = d.0[&id]
            .implementation
            .as_ref()
            .unwrap()
            .generation
            .clone()
            .unwrap();
        let e = g.explanation.unwrap();
        assert_eq!(e.steps[0].lines.map(|l| l.start), Some(5));
        assert_eq!(e.assumptions, ["amounts are numbers"]);
    }

    #[test]
    fn only_unedited_generated_code_can_be_explained() {
        let (mut a, mut p, mut d, id) = setup();
        a.explain(&d.0[&id], &mut p);
        assert!(matches!(a.report(id), Some(Report::Blocked(_))));
        assert!(p.sent.is_empty());
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        let doc = d.0.get_mut(&id).unwrap();
        doc.implementation
            .as_mut()
            .unwrap()
            .source
            .push_str("(def x 1)\n");
        let sent = p.sent.len();
        a.explain(&d.0[&id], &mut p);
        assert!(matches!(a.report(id), Some(Report::Blocked(_))));
        assert_eq!(p.sent.len(), sent);
    }

    #[test]
    fn a_failed_explanation_keeps_the_code_unexplained() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        a.explain(&d.0[&id], &mut p);
        let (req, _) = p.last();
        let updates = finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::Timeout(Duration::from_secs(1))),
        );
        assert!(updates.is_empty());
        assert!(matches!(a.report(id), Some(Report::Failed { .. })));
        let i = d.0[&id].implementation.as_ref().unwrap();
        assert_eq!(i.source, CODE);
        assert!(i.generation.as_ref().unwrap().explanation.is_none());
    }

    #[test]
    fn code_changed_while_being_explained_gets_no_explanation() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        a.explain(&d.0[&id], &mut p);
        d.0.get_mut(&id).unwrap().implementation.as_mut().unwrap().source = WRONG.into();
        let updates = reply(&mut a, &mut p, &mut d, explanation());
        assert!(updates.is_empty());
        assert!(matches!(a.report(id), Some(Report::Failed { .. })));
        assert!(
            d.0[&id]
                .implementation
                .as_ref()
                .unwrap()
                .generation
                .as_ref()
                .unwrap()
                .explanation
                .is_none()
        );
    }

    #[test]
    fn an_inference_failure_during_repair_keeps_the_attempts() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        validated(&mut a, &mut p, &mut d, false);
        let (req, _) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::Timeout(Duration::from_secs(1))),
        );
        let Some(Report::Unresolved { attempts, notes, .. }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(attempts.len(), 1);
        assert!(notes[0].contains("Could not generate"), "{notes:?}");
    }

    fn hand_written(d: &mut Docs, id: FunctionId) {
        let doc = d.0.get_mut(&id).unwrap();
        doc.description.clear();
        doc.steps.clear();
        doc.examples.clear();
        doc.implementation = Some(Implementation::manual(CODE));
    }

    fn description(inputs: &[(&str, &str)]) -> Value {
        json!({"description": "Total the order amounts per customer.",
               "steps": ["Group the orders by :customer.", "Sum the :amount of each group."],
               "examples": inputs.iter().map(|(n, i)| json!({"name": n, "input": i})).collect::<Vec<_>>()})
    }

    fn contract_proposal() -> Value {
        json!({"input": {"description": "orders", "value_type": "vector", "keys": []},
               "output": {"description": "totals", "value_type": "map", "keys": []},
               "ordering": "unspecified", "sorted_by": "", "notes": ""})
    }

    /// Harness output for the last validation request: each call returns the given value, or
    /// throws when it is `None`.
    fn returning(p: &FakePorts, values: &[Option<&str>]) -> Execution {
        let mut out = String::from(
            "{\"event\":\"read\",\"ok\":true}\n{\"event\":\"check\",\"ok\":true}\n\
             {\"event\":\"lint\",\"findings\":[]}\n{\"event\":\"load\",\"ok\":true}\n",
        );
        for (i, v) in values.iter().enumerate() {
            out.push_str(&match v {
                Some(v) => format!(
                    "{{\"event\":\"result\",\"index\":{i},\"status\":\"ok\",\"actual\":{},\"elapsed-ns\":1000}}\n",
                    json!(v)
                ),
                None => format!(
                    "{{\"event\":\"result\",\"index\":{i},\"status\":\"error\",\"error\":{{\"phase\":\"call\",\"class\":\"java.lang.NullPointerException\",\"message\":\"boom\"}}}}\n"
                ),
            });
        }
        out.push_str("{\"event\":\"done\"}\n");
        assert_eq!(p.validations.last().unwrap().calls.len(), values.len());
        Execution {
            outcome: Ok(EvalOutcome {
                status: EvalStatus::Completed,
                report: HarnessReport::parse(&out),
                stderr: String::new(),
                elapsed: Duration::from_millis(600),
            }),
            environment: env(),
        }
    }

    #[test]
    fn describing_hand_written_code_fills_the_specification_with_outputs_the_code_returns() {
        let (mut a, mut p, mut d, id) = setup();
        hand_written(&mut d, id);
        a.describe(&d.0[&id], &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Describing);
        assert!(p.last_text().contains("(group-by :customer)"), "the code is sent");

        reply(
            &mut a,
            &mut p,
            &mut d,
            description(&[
                ("two orders", "[{:customer 1 :amount 2} {:customer 1 :amount 3}]"),
                ("broken", "[{:a"),
            ]),
        );
        assert_eq!(a.job().unwrap().phase, Phase::Recording);
        let run = p.validations.last().unwrap();
        assert_eq!(run.calls.len(), 1, "the unreadable input is not run");
        assert_eq!(run.calls[0].expected, None);

        let e = returning(&p, &[Some("{1 5}")]);
        let updates = a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert!(
            updates.is_empty(),
            "nothing is applied before the draft is checked"
        );
        assert_eq!(a.job().unwrap().phase, Phase::Revising);
        let text = p.last_text();
        assert!(
            text.contains("[{:customer 1 :amount 2} {:customer 1 :amount 3}] returns {1 5}"),
            "{text}"
        );
        assert!(text.contains("Total the order amounts per customer."));

        let updates = reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"description": "Sums the order amounts per customer.", "steps": ["Group the orders by :customer.", "Sum the :amount of each group."]}),
        );
        assert_eq!(updates, vec![Update::Edited(id)]);
        assert_eq!(a.job().unwrap().phase, Phase::Proposing);
        assert!(p.last_text().contains("Sums the order amounts per customer."));

        reply(&mut a, &mut p, &mut d, contract_proposal());
        let doc = &d.0[&id];
        assert_eq!(doc.description, "Sums the order amounts per customer.");
        assert_eq!(doc.steps.len(), 2);
        assert_eq!(doc.examples.len(), 1);
        assert_eq!(doc.examples[0].name, "two orders");
        assert_eq!(doc.examples[0].expected.as_str(), "{1 5}");
        assert_eq!(doc.contract.as_ref().unwrap().status, ContractStatus::Proposed);
        let Some(Report::Described { filled, notes }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(
            *filled,
            ["description", "steps", "1 example", "a proposed contract"]
        );
        assert!(notes.iter().any(|n| n.contains("broken")), "{notes:?}");
    }

    #[test]
    fn describing_keeps_the_users_text_an_accepted_contract_and_drops_throwing_inputs() {
        let (mut a, mut p, mut d, id) = setup();
        let accepted = Contract::empty(ContractStatus::Accepted);
        {
            let doc = d.0.get_mut(&id).unwrap();
            doc.implementation = Some(Implementation::manual(CODE));
            doc.contract = Some(accepted.clone());
        }
        let before = d.0[&id].clone();
        a.describe(&d.0[&id], &mut p);
        reply(
            &mut a,
            &mut p,
            &mut d,
            description(&[
                ("known", "[{:customer 1 :amount 2}]"),
                ("new", "[]"),
                ("throws", "[nil]"),
            ]),
        );
        let e = returning(&p, &[Some("{1 2}"), Some("{}"), None]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert_eq!(
            a.job().unwrap().phase,
            Phase::Revising,
            "the user's text is checked"
        );
        assert!(p.last_text().contains("[] returns {}"), "{}", p.last_text());
        reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"contradictions": ["\"Sums per customer\": nil input throws."]}),
        );

        assert_eq!(p.sent.len(), 2, "no contract is proposed over an accepted one");
        let doc = &d.0[&id];
        assert_eq!(doc.description, before.description);
        assert_eq!(doc.steps, before.steps);
        assert_eq!(doc.contract.as_ref(), Some(&accepted));
        assert_eq!(doc.examples.len(), 2, "only the new input is added");
        assert_eq!(doc.examples[1].expected.as_str(), "{}");
        let Some(Report::Described { filled, notes }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(*filled, ["1 example"]);
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("The specification may be wrong: \"Sums")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("throws") && n.contains("NullPointerException"))
        );
    }

    #[test]
    fn when_no_input_gives_an_example_new_inputs_are_asked_for_once() {
        let (mut a, mut p, mut d, id) = setup();
        hand_written(&mut d, id);
        a.describe(&d.0[&id], &mut p);
        reply(
            &mut a,
            &mut p,
            &mut d,
            description(&[("bad", "[{\"a\": 1}"), ("throws", "[nil]")]),
        );
        let e = returning(&p, &[None]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Describing);
        let retry = p.last_text();
        assert!(
            retry.contains("the function threw NullPointerException: boom (input [nil])"),
            "{retry}"
        );
        assert!(retry.contains("[{\"a\": 1}"), "{retry}");

        reply(&mut a, &mut p, &mut d, description(&[("still", "[nil]")]));
        let e = returning(&p, &[None]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Revising, "only one retry");
        assert!(
            !p.sent
                .last()
                .unwrap()
                .1
                .sections
                .iter()
                .any(|s| s.label == "Rejected inputs")
        );
    }

    #[test]
    fn when_every_example_has_the_same_output_other_inputs_are_asked_for_once() {
        let (mut a, mut p, mut d, id) = setup();
        hand_written(&mut d, id);
        a.describe(&d.0[&id], &mut p);
        let inputs = [("a", "[]"), ("b", "[{}]"), ("c", "[{:customer 1}]")];
        reply(&mut a, &mut p, &mut d, description(&inputs));
        let e = returning(&p, &[Some("{}"), Some("{}"), Some("{}")]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Describing);
        let retry = p.last_text();
        assert!(retry.contains("the code returned {} for every one"), "{retry}");
        assert!(retry.contains("[{:customer 1}]"), "{retry}");

        reply(&mut a, &mut p, &mut d, description(&inputs));
        let e = returning(&p, &[Some("{}"), Some("{}"), Some("{}")]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Revising, "only one retry");
    }

    #[test]
    fn describing_without_a_runtime_still_fills_the_description() {
        let (mut a, mut p, mut d, id) = setup();
        hand_written(&mut d, id);
        p.no_runtime = true;
        a.describe(&d.0[&id], &mut p);
        reply(&mut a, &mut p, &mut d, description(&[("empty", "[]")]));
        assert_eq!(a.job().unwrap().phase, Phase::Proposing);
        reply(&mut a, &mut p, &mut d, contract_proposal());
        assert!(d.0[&id].examples.is_empty());
        assert!(!d.0[&id].description.is_empty());
        let Some(Report::Described { notes, .. }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert!(notes.iter().any(|n| n.contains("Java runtime")), "{notes:?}");
    }

    #[test]
    fn a_failed_check_of_the_description_still_applies_the_draft() {
        let (mut a, mut p, mut d, id) = setup();
        hand_written(&mut d, id);
        a.describe(&d.0[&id], &mut p);
        reply(
            &mut a,
            &mut p,
            &mut d,
            description(&[("two", "[{:customer 1 :amount 2}]")]),
        );
        let e = returning(&p, &[Some("{1 2}")]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Revising);
        let (req, _) = p.last();
        finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::Malformed {
                problem: "not JSON".into(),
                raw: "Sure!".into(),
            }),
        );
        assert_eq!(a.job().unwrap().phase, Phase::Proposing);
        assert_eq!(d.0[&id].description, "Total the order amounts per customer.");
        assert_eq!(d.0[&id].examples.len(), 1);
        reply(&mut a, &mut p, &mut d, contract_proposal());
        let Some(Report::Described { notes, .. }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert!(notes.iter().any(|n| n.contains("not checked")), "{notes:?}");
    }

    #[test]
    fn code_changed_while_being_described_is_not_described() {
        let (mut a, mut p, mut d, id) = setup();
        hand_written(&mut d, id);
        a.describe(&d.0[&id], &mut p);
        reply(&mut a, &mut p, &mut d, description(&[("empty", "[]")]));
        d.0.get_mut(&id).unwrap().implementation = Some(Implementation::manual(WRONG));
        let e = returning(&p, &[Some("{}")]);
        a.on_validated(p.validations.len() as u64, e, &mut d, &mut p);
        assert!(d.0[&id].description.is_empty());
        assert!(matches!(a.report(id), Some(Report::Failed { .. })));
    }

    #[test]
    fn describing_a_project_function_looks_up_the_code_it_calls_in_other_namespaces() {
        let (mut a, mut p, mut d, id) = setup();
        let file =
            "(ns shop.orders (:require [shop.util :as u]))\n\n(defn label [o]\n  (u/clean (:name o)))\n";
        {
            let doc = d.0.get_mut(&id).unwrap();
            doc.description.clear();
            doc.steps.clear();
            doc.examples.clear();
            let binding = clojurecraft_domain::ProjectBinding {
                namespace: "shop.orders".into(),
                function: "label".into(),
                helpers: Vec::new(),
                arguments: 1,
                file: file.into(),
                usable: Vec::new(),
            };
            doc.implementation = crate::project::fragment(&binding).map(Implementation::manual);
            doc.project = Some(binding);
        }
        a.describe(&d.0[&id], &mut p);
        assert!(p.sent.is_empty(), "the lookup comes first");
        let g = p.gathers.last().unwrap();
        assert_eq!(g.project_namespace.as_deref(), Some("shop.orders"));
        assert!(g.include_restricted);
        assert_eq!(g.queries, [("shop.util/clean".to_owned(), 1)]);

        let hit = Hit {
            chunk: clojurecraft_knowledge::Chunk {
                id: "project:shop.util/clean".into(),
                kind: clojurecraft_knowledge::ChunkKind::Project,
                source: clojurecraft_knowledge::chunk::SourceKey::Project,
                title: "shop.util/clean".into(),
                namespace: Some("shop.util".into()),
                symbol: Some("clean".into()),
                url: "src/shop/util.clj".into(),
                since: None,
                restricted: false,
                mentions: Vec::new(),
                text: "(defn clean [s])\nTrims and lower-cases a name.".into(),
            },
            provenance: clojurecraft_knowledge::Provenance {
                source: clojurecraft_knowledge::chunk::SourceKey::Project,
                name: "project sources".into(),
                license: String::new(),
                attribution: String::new(),
                revision: String::new(),
                origin: String::new(),
            },
            score: 1.0,
            signals: clojurecraft_knowledge::Signals::default(),
        };
        let found = Gathered {
            hits: Vec::new(),
            info: None,
            collection_version: String::new(),
            core: BTreeSet::new(),
            project: vec![hit],
        };
        a.on_gathered(p.gathers.len() as u64, Ok(found), &mut p);
        assert_eq!(a.job().unwrap().phase, Phase::Describing);
        let text = p.last_text();
        assert!(text.contains("Trims and lower-cases a name."), "{text}");
        assert!(
            !text.contains("Prefer calling one of them"),
            "describing calls nothing new: {text}"
        );
    }

    #[test]
    fn functions_without_code_cannot_be_described() {
        let (mut a, mut p, d, id) = setup();
        a.describe(&d.0[&id], &mut p);
        assert!(p.sent.is_empty());
        assert!(matches!(a.report(id), Some(Report::Blocked(_))));
    }

    /// Edge cases, accepted code, and fixes for failing cases.
    mod review;
}
