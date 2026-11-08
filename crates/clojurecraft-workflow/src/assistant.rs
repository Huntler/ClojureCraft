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
//! - generated code is validated (see [`crate::validation`]). A new generation first writes up to
//!   [`SAMPLES`] independent candidates (new seed, higher temperature), because failures are
//!   mostly sampling noise and checking a candidate is cheap; then the best failing candidate is
//!   sent back with its diagnostics for repair, at most [`MAX_REPAIRS`] times. The first passing
//!   candidate ends the loop;
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

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, RandomState};
use std::time::Instant;

use crate::answers::{self, InterpretationAnswer};
use crate::intermediate::StepLines;
use crate::requests::{self, DOCUMENTATION_DROP_ORDER};
use crate::validation::{self, Plan, ReportContext};
use crate::{Candidate, GenerationInput, Names, Provenance, deps, readiness};
use crate::{cleanup, delimiters, edge_cases};
use clojurecraft_clojure::{EvalOutcome, EvalRequest};
use clojurecraft_domain::{
    DocumentationRef, ExecutionEnvironment, Explanation, Fingerprint, FunctionDocument, FunctionId,
    GenerationRunId, GenerationSettings, InferenceRuntime, Interpretation, ModelIdentity, RevisionKind,
    RevisionReason, Timestamp, ValidationReport,
};
use clojurecraft_inference::{
    GenerateRequest, Generation, GenerationError, Remedy, RequestId, Role, ServiceEvent, StreamEvent,
};
use clojurecraft_knowledge::{CollectionInfo, Embedder, Hit, Query, Retriever, Semantic};

/// Documentation excerpts given to the generator at most.
pub const MAX_EXCERPTS: usize = 10;

/// Extra answer budget for models that reason before answering. Their thinking counts against
/// the answer's token limit; the service still caps the total at half the context.
pub const REASONING_ALLOWANCE: u32 = 6144;

/// Candidates written for a new generation before any repair.
pub const SAMPLES: u32 = 3;

/// Repairs after the candidates (or after the first repair of a fix).
pub const MAX_REPAIRS: u32 = 3;

/// Attempts of a generation at most: the candidates plus the repairs.
pub const MAX_ATTEMPTS: u32 = SAMPLES + MAX_REPAIRS;

/// Characters of failure diagnostics sent with a repair request.
const MAX_FAILURE_CHARS: usize = 6000;

/// Documentation to look up for a generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gather {
    /// Query text and how many of its results to keep.
    pub queries: Vec<(String, usize)>,
    pub clojure_version: Option<String>,
    /// Symbols to check for being `clojure.core` vars.
    pub core_candidates: Vec<String>,
}

pub struct Gathered {
    pub hits: Vec<Hit>,
    pub info: CollectionInfo,
    pub collection_version: String,
    /// The candidates that name `clojure.core` vars.
    pub core: BTreeSet<String>,
}

/// Runs a [`Gather`] against an open collection: each query contributes up to its own count of
/// results not already found, at most [`MAX_EXCERPTS`] in total. Returns whether semantic search
/// failed, in which case the caller should restart its embedding model.
pub fn gather(
    retriever: &mut Retriever,
    mut embedder: Option<&mut dyn Embedder>,
    g: &Gather,
    collection_version: &str,
) -> Result<(Gathered, bool), String> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut semantic_failed = false;
    for (text, take) in &g.queries {
        if hits.len() >= MAX_EXCERPTS || text.trim().is_empty() {
            continue;
        }
        let mut q = Query::new(text.clone());
        q.limit = (*take).max(1) + 4;
        q.clojure_version.clone_from(&g.clojure_version);
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
            .filter(|h| !hits.iter().any(|x| x.chunk.id == h.chunk.id))
            .take(*take)
            .collect();
        hits.extend(fresh);
    }
    hits.truncate(MAX_EXCERPTS);
    let core = g
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
    Ok((
        Gathered {
            hits,
            info: retriever.info().clone(),
            collection_version: collection_version.to_owned(),
            core,
        },
        semantic_failed,
    ))
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
    Repairing,
    Explaining,
    ProposingTests,
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
            Self::Repairing => "Repairing the code",
            Self::Explaining => "Explaining the code",
            Self::ProposingTests => "Proposing edge cases",
        }
    }
}

/// One generation or repair attempt and how it fared.
#[derive(Clone, Debug, PartialEq)]
pub struct Attempt {
    /// 1 for the initial generation.
    pub number: u32,
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
}

struct GenState {
    model: ModelContext,
    names: Option<Names>,
    allowed: BTreeSet<String>,
    documentation: Vec<DocumentationRef>,
    docs_section: Option<clojurecraft_inference::Section>,
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
    /// attempt repairs instead of writing code afresh.
    start: Option<(String, ValidationReport)>,
    /// The namespace and function of the code being fixed.
    start_names: Option<Names>,
    /// What was changed in the current candidate before it was checked.
    prepared: Vec<String>,
}

impl GenState {
    fn seed_now(&self) -> u64 {
        self.seed.wrapping_add(u64::from(self.attempt - 1))
    }

    /// Attempts this generation may make: a fix starts with a repair, so it has no candidates.
    fn max_attempts(&self) -> u32 {
        if self.start.is_some() {
            1 + MAX_REPAIRS
        } else {
            MAX_ATTEMPTS
        }
    }

    /// Whether `attempt` writes a fresh candidate rather than repairing one.
    fn is_sample(&self, attempt: u32) -> bool {
        self.start.is_none() && attempt <= SAMPLES
    }

    /// The sampling temperature `attempt` ran with.
    fn temperature(&self, attempt: u32) -> f32 {
        match (self.is_sample(attempt), attempt) {
            (true, 1) => Role::Generation.profile().temperature,
            (true, _) => requests::SAMPLE_TEMPERATURE,
            (false, _) => requests::REPAIR_TEMPERATURE,
        }
    }

    /// Repairs made up to and including `attempt`.
    fn repairs(&self, attempt: u32) -> u32 {
        if self.start.is_some() {
            attempt
        } else {
            attempt.saturating_sub(SAMPLES)
        }
    }

    fn input(&self) -> GenerationInput<'_> {
        GenerationInput {
            doc: &self.spec,
            names: self.names.as_ref().expect("names are set before code is written"),
            allowed: &self.allowed,
            clojure_version: &self.model.clojure_version,
            documentation: self.docs_section.clone(),
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
}

impl Job {
    /// The attempt in progress and the maximum, for a generation.
    pub fn attempt(&self) -> Option<(u32, u32)> {
        self.generation.as_ref().map(|g| (g.attempt, g.max_attempts()))
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
    pub explanation: Option<Explanation>,
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

fn random_seed() -> u64 {
    // RandomState is seeded randomly per instance.
    RandomState::new().hash_one(Instant::now())
}

fn job_label(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Interpret => "check the specification",
        JobKind::ProposeContract => "propose a contract",
        JobKind::Generate => "generate the implementation",
        JobKind::ProposeTests => "propose edge cases",
    }
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
            notes: r.notes,
            interpretation: None,
            generation: None,
        });
        Ok(())
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
        let (request, phase) = match &g.start {
            Some((code, report)) => {
                let names = g.names.as_ref().expect("names were just set");
                let plan = validation::plan(&g.spec, code, names);
                let failures = validation::failure_summary(&plan, report, MAX_FAILURE_CHARS);
                (
                    requests::repair_request(g.input(), code, &failures, 1, g.max_attempts(), None),
                    Phase::Repairing,
                )
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
        let mut core = BTreeSet::new();
        match result {
            Ok(found) => {
                core = found.core;
                if let Some(g) = &mut job.generation
                    && !found.hits.is_empty()
                {
                    g.documentation = found
                        .hits
                        .iter()
                        .map(|h| {
                            clojurecraft_knowledge::prompt::documentation_ref(
                                h,
                                &found.info,
                                &found.collection_version,
                            )
                        })
                        .collect();
                    g.docs_section = Some(clojurecraft_knowledge::prompt::reference_section(
                        &found.hits,
                        &found.info,
                        DOCUMENTATION_DROP_ORDER,
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
            // Planning only improves retrieval; continue without it.
            Phase::Planning => {
                job.notes.push(format!(
                    "Choosing documentation failed ({e}); the description and steps were used instead."
                ));
                self.retrieve(Vec::new(), ports);
                Vec::new()
            }
            // The code is kept without an explanation.
            Phase::Explaining => {
                job.notes.push(format!("The code could not be explained: {e}."));
                self.finish_passing(doc, None)
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
        let Some(value) = &g.value else {
            match job.phase {
                Phase::Generating | Phase::Repairing => {
                    return self.attempt_without_code("The model gave no structured answer.".into(), ports);
                }
                // Checked code is kept without an explanation.
                Phase::Explaining => {
                    job.notes
                        .push("The code could not be explained: the model gave no structured answer.".into());
                    return self.finish_passing(doc, None);
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
                match answers::parse_contract(value, allowed) {
                    Ok((contract, mut notes)) => {
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
                    doc.tests.extend(proposals.cases);
                    self.end(Report::TestsProposed {
                        added,
                        ambiguous,
                        notes,
                    });
                    vec![Update::Edited(function)]
                }
                Err(e) => {
                    self.malformed(e, &g.text);
                    Vec::new()
                }
            },
            Phase::Retrieving | Phase::Validating => Vec::new(),
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
                let (code, prepared) = prepare(&generated.code);
                generated.code = code;
                gen_state.prepared = prepared;
                let Some(names) = gen_state.names.clone() else {
                    return Vec::new();
                };
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
                let quoted = std::mem::take(&mut generated.steps);
                let candidate = Candidate::check(generated, &names, &gen_state.allowed);
                let lines = step_lines(&gen_state.spec, &candidate.code, &quoted);
                let plan = validation::plan_with_steps(&gen_state.spec, &candidate.code, &names, lines);
                let request = plan.request.clone();
                gen_state.candidate = Some(candidate);
                gen_state.plan = Some(plan);
                match request {
                    // Parse or structure problems: the report is complete without running.
                    None => self.validated(Err("not run".into()), None, ports),
                    Some(r) => match ports.validate(r) {
                        Ok(id) => {
                            job.validation = Some(id);
                            job.phase = Phase::Validating;
                            job.output.clear();
                            job.reasoning.clear();
                            Vec::new()
                        }
                        Err(why) => self.unvalidated(why, ports),
                    },
                }
            }
            Phase::Explaining => {
                let code = job
                    .generation
                    .as_ref()
                    .and_then(|g| g.candidate.as_ref())
                    .map(|c| c.code.clone())
                    .unwrap_or_default();
                let steps = requests::numbered_steps(doc);
                let explanation = match answers::parse_explanation(value, &steps, &code) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        job.notes.push(format!("The code could not be explained: {e}."));
                        None
                    }
                };
                self.finish_passing(doc, explanation)
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
        if docs.document(job.function).is_none() {
            self.end(Report::Cancelled);
            return Vec::new();
        }
        self.validated(execution.outcome, Some(execution.environment), ports)
    }

    /// Records the current candidate's report and decides what follows: an explanation of
    /// passing code, a repair, or the end of the loop.
    fn validated(
        &mut self,
        outcome: Result<EvalOutcome, String>,
        environment: Option<ExecutionEnvironment>,
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
            code: Some(candidate.code.clone()),
            report: Some(report.clone()),
            note: (!note.is_empty()).then(|| note.join(" ")),
        });
        let mut updates = vec![Update::Validated(function, Box::new(report))];
        if passed {
            let request = requests::explanation_request(&g.spec, &candidate.code);
            self.send(request, Phase::Explaining, ports);
        } else {
            updates.extend(self.next_attempt(ports));
        }
        updates
    }

    /// The candidate could not be run at all (for example, no Java runtime). It is kept, as
    /// before validation existed, but marked as unchecked.
    fn unvalidated(&mut self, why: String, ports: &mut dyn Ports) -> Vec<Update> {
        if let Some(job) = &mut self.job {
            job.notes.push(format!("The code was not run: {why}"));
            if let Some(g) = &mut job.generation
                && let Some(c) = &g.candidate
            {
                let mut note = std::mem::take(&mut g.prepared);
                note.push(format!("Not run: {why}"));
                g.attempts.push(Attempt {
                    number: g.attempt,
                    code: Some(c.code.clone()),
                    report: None,
                    note: Some(note.join(" ")),
                });
                let request = requests::explanation_request(&g.spec, &c.code);
                self.send(request, Phase::Explaining, ports);
            }
        }
        Vec::new()
    }

    /// The attempt produced no code to check.
    fn attempt_without_code(&mut self, note: String, ports: &mut dyn Ports) -> Vec<Update> {
        if let Some(g) = self.job.as_mut().and_then(|j| j.generation.as_mut()) {
            g.candidate = None;
            g.plan = None;
            g.prepared.clear();
            g.attempts.push(Attempt {
                number: g.attempt,
                code: None,
                report: None,
                note: Some(note),
            });
        }
        self.next_attempt(ports)
    }

    /// Starts the next attempt, or ends the loop after the last one: another candidate while
    /// candidates remain, then a repair of the best candidate so far.
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
        if g.is_sample(g.attempt) {
            let mut request = requests::generation_request(g.input());
            request.temperature = Some(requests::SAMPLE_TEMPERATURE);
            self.send(request, Phase::Generating, ports);
            return Vec::new();
        }
        let Some(best) = best_attempt(&g.attempts) else {
            // No candidate had code to repair: write the code afresh.
            let mut request = requests::generation_request(g.input());
            request.temperature = Some(requests::REPAIR_TEMPERATURE);
            self.send(request, Phase::Repairing, ports);
            return Vec::new();
        };
        let (Some(code), Some(report)) = (best.code.clone(), best.report.clone()) else {
            return Vec::new();
        };
        let names = g.names.as_ref().expect("names are set before code is written");
        let summary = |code: &str, r: &ValidationReport| {
            let plan = validation::plan(&g.spec, code, names);
            validation::failure_summary(&plan, r, MAX_FAILURE_CHARS)
        };
        let failures = summary(&code, &report);
        // Another attempt failing exactly like the one being repaired (same code, or the same
        // failures) means the model is stuck on one approach.
        let repeated = g.attempts.iter().rev().find_map(|e| match (&e.code, &e.report) {
            _ if e.number == best.number => None,
            (Some(c), _) if *c == code => Some((best.number, e.number)),
            (Some(c), Some(er)) if summary(c, er) == failures => Some((best.number, e.number)),
            _ => None,
        });
        let request =
            requests::repair_request(g.input(), &code, &failures, g.attempt, g.max_attempts(), repeated);
        self.send(request, Phase::Repairing, ports);
        Vec::new()
    }

    fn provenance(g: &GenState, spec_basis: Fingerprint) -> Provenance {
        let profile = Role::Generation.profile();
        Provenance {
            model: g.model.model.clone(),
            runtime: g.model.runtime.clone(),
            settings: GenerationSettings {
                temperature: g.temperature(g.attempt),
                top_p: profile.top_p,
                seed: Some(g.seed_now()),
                context_length: g.model.context_length,
                max_output_tokens: profile.max_output_tokens,
            },
            documentation: g.documentation.clone(),
            spec_basis,
            omitted_context: g.omitted.clone(),
            attempts: g.attempt,
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
                    assumptions: Vec::new(),
                    unescaped: false,
                    steps: Vec::new(),
                },
                &names,
                &g.allowed,
            );
            if !candidate.structure.problems.is_empty() {
                return None;
            }
            let mut provenance = Self::provenance(g, job.spec_basis.clone());
            provenance.settings.seed = Some(g.seed.wrapping_add(u64::from(attempt.number - 1)));
            provenance.settings.temperature = g.temperature(attempt.number);
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
                explanation: None,
                attempts: Vec::new(),
                notes: Vec::new(),
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
    fn finish_passing(
        &mut self,
        doc: &mut FunctionDocument,
        explanation: Option<Explanation>,
    ) -> Vec<Update> {
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
        let repairs = g.repairs(g.attempt);
        if repairs > 0 {
            notes.push(format!(
                "The code passed after {repairs} repair{}.",
                if repairs == 1 { "" } else { "s" }
            ));
        } else if g.attempt > 1 {
            notes.push(format!(
                "Candidate {} passed; the earlier ones failed their checks.",
                g.attempt
            ));
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
            explanation,
            attempts: g.attempts,
            notes,
        };
        if pending.needs_approval.is_empty()
            && !pending.replaces_manual
            && !pending.replaces_accepted
            && !pending.fix
            && !pending.spec_changed
        {
            self.apply(job.function, doc, pending)
        } else {
            self.reports
                .insert(job.function, Report::Pending(Box::new(pending)));
            Vec::new()
        }
    }

    fn apply(&mut self, function: FunctionId, doc: &mut FunctionDocument, pending: Pending) -> Vec<Update> {
        let attempts = pending.attempts.clone();
        let passing_fix = pending
            .validation
            .as_ref()
            .filter(|r| pending.fix && r.passed())
            .cloned();
        match crate::implementation(&pending.candidate, pending.explanation, pending.provenance) {
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
fn prepare(code: &str) -> (String, Vec<String>) {
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
    (tidied.code, notes)
}

/// Where the code of each step is, from the model's quotes of it; `None` without steps.
fn step_lines(spec: &FunctionDocument, code: &str, quoted: &[(u32, String)]) -> Option<StepLines> {
    let steps = requests::numbered_steps(spec);
    if steps.is_empty() {
        return None;
    }
    Some(
        steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let number = u32::try_from(i + 1).unwrap_or(u32::MAX);
                let lines = quoted
                    .iter()
                    .find(|(n, _)| *n == number)
                    .ok_or_else(|| "The model attributed no code to this step.".to_owned())
                    .and_then(|(_, quote)| {
                        std::iter::once(quote.clone())
                            .chain(pipeline_stage_variants(quote))
                            .find_map(|q| crate::locate::locate(code, &q))
                            .ok_or_else(|| "The code quoted for this step was not found.".to_owned())
                    });
                (step.id, lines)
            })
            .collect(),
    )
}

/// A quoted pipeline stage as it appears inside `->>` or `->`: models often quote
/// `(group-by :customer orders)` for the stage `(group-by :customer)`, so the call is also tried
/// without its last argument and without its first.
fn pipeline_stage_variants(quote: &str) -> Vec<String> {
    let Ok(forms) = crate::structure::read_all(quote) else {
        return Vec::new();
    };
    let [form] = forms.as_slice() else {
        return Vec::new();
    };
    let Some(items) = form.list().filter(|i| i.len() >= 3) else {
        return Vec::new();
    };
    let text = |skip: usize| {
        let parts: Vec<&str> = items
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .map(|(_, f)| &quote[f.span()])
            .collect();
        format!("({})", parts.join(" "))
    };
    vec![text(items.len() - 1), text(1)]
}

/// The failed attempt most worth repairing: code that loads before code that does not, then the
/// most passing checks, then the most recent.
fn best_attempt(attempts: &[Attempt]) -> Option<&Attempt> {
    attempts
        .iter()
        .filter(|a| a.code.is_some() && a.report.is_some())
        .max_by_key(|a| {
            let report = a.report.as_ref().expect("filtered above");
            let loads = report.checks.iter().any(|c| {
                c.subject == clojurecraft_domain::CheckSubject::Load
                    && c.status == clojurecraft_domain::CheckStatus::Passed
            });
            (loads, validation::tally(report).passed, a.number)
        })
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
        });
    }
    let allowed = deps::allowed(doc);
    let s = crate::structure::check(
        &i.source,
        &crate::structure::Expectations {
            namespace: "",
            function: "",
            allowed: &allowed,
        },
    );
    Some(Names {
        namespace: s.namespace?,
        function: s.function?,
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
    use clojurecraft_inference::{Finish, PromptPlan, Usage};
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
        let req = p.validations.last().unwrap();
        let mut out = String::from(
            "{\"event\":\"read\",\"ok\":true}\n{\"event\":\"check\",\"ok\":true}\n\
             {\"event\":\"lint\",\"findings\":[]}\n{\"event\":\"load\",\"ok\":true}\n",
        );
        for i in 0..req.calls.len() {
            out.push_str(&format!(
                "{{\"event\":\"result\",\"index\":{i},\"status\":\"ok\",\"actual\":\"{{1 3}}\",\"match\":{pass},\"difference\":{},\"elapsed-ns\":1000}}\n",
                if pass { "null" } else { "\"at [1]: expected 2 but got 3\"" }
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
               "symbols": [{"symbol": "update-vals", "purpose": "sums each group"}]})
    }

    fn gathered(core: &[&str]) -> Result<Gathered, String> {
        Ok(Gathered {
            hits: vec![],
            info: CollectionInfo {
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
            },
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
    fn a_generation_plans_retrieves_writes_validates_explains_and_commits() {
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

        let updates = reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"code": CODE, "assumptions": ["amounts are numbers"]}),
        );
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
        assert_eq!(p.last().1, Role::Explanation);

        let updates = reply(&mut a, &mut p, &mut d, explanation());
        assert!(
            matches!(&updates[..], [Update::Commit(f, r)] if *f == id && r.kind == RevisionKind::Generated)
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
        assert_eq!(g.assumptions, ["amounts are numbers"]);
        assert_eq!(g.function_name, "order-totals");
        assert_eq!(g.attempts, 1);
        let e = g.explanation.as_ref().unwrap();
        assert_eq!(e.steps[0].lines.map(|l| l.start), Some(5));
        // The stored report describes the code that is now the implementation.
        assert!(stored[0].freshness(&doc.basis()).is_current());
    }

    /// Answers the running request with `code` and lets its validation fail, `n` times.
    fn fail_with(a: &mut Assistant, p: &mut FakePorts, d: &mut Docs, code: &str, n: u32) {
        for _ in 0..n {
            reply(a, p, d, json!({"code": code, "assumptions": []}));
            validated(a, p, d, false);
        }
    }

    #[test]
    fn failing_candidates_are_followed_by_fresh_ones_before_any_repair() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        let updates = validated(&mut a, &mut p, &mut d, false);
        assert!(!reports(&updates)[0].passed());

        // The second candidate is the same generation request with a new seed and more variety.
        let job = a.job().unwrap();
        assert_eq!(
            (job.phase, job.attempt()),
            (Phase::Generating, Some((2, MAX_ATTEMPTS)))
        );
        let (first, second) = (&p.sent[0].1, &p.sent[1].1);
        assert_ne!(first.seed, second.seed, "each candidate samples anew");
        assert_eq!(first.temperature, None);
        assert_eq!(second.temperature, Some(requests::SAMPLE_TEMPERATURE));
        assert!(!p.last_text().contains("Your previous implementation"));
        assert!(first.grammar.is_some() && second.grammar.is_some());

        reply(&mut a, &mut p, &mut d, json!({"code": CODE, "assumptions": []}));
        validated(&mut a, &mut p, &mut d, true);
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { attempts, notes }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(attempts.len(), 2);
        assert!(
            notes.iter().any(|n| n.contains("Candidate 2 passed")),
            "{notes:?}"
        );
        let g = d.0[&id]
            .implementation
            .as_ref()
            .unwrap()
            .generation
            .clone()
            .unwrap();
        assert_eq!(g.attempts, 2);
        assert_eq!(g.settings.temperature, requests::SAMPLE_TEMPERATURE);
    }

    #[test]
    fn a_failing_candidate_is_repaired_with_its_diagnostics_until_one_passes() {
        let (mut a, mut p, mut d, id) = setup();
        let before = d.0[&id].clone();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        let updates = validated(&mut a, &mut p, &mut d, false);
        assert!(!reports(&updates)[0].passed());
        fail_with(&mut a, &mut p, &mut d, WRONG, SAMPLES - 1);

        // The repair request carries the specification, the code, and expected and actual values.
        assert_eq!(p.last().1, Role::Generation);
        assert_eq!(a.job().unwrap().phase, Phase::Repairing);
        let first_repair = SAMPLES + 1;
        assert_eq!(a.job().unwrap().attempt(), Some((first_repair, MAX_ATTEMPTS)));
        let text = p.last_text();
        for needle in [
            &format!("attempt {first_repair} of {MAX_ATTEMPTS}"),
            "count))",
            "Example 1 — failed",
            "Expected: {1 2}",
            "Actual: {1 3}",
            "at [1]: expected 2 but got 3",
            "Total amounts per customer",
            "Input (EDN):\n[{:customer 1 :amount 2}]\nExpected output (EDN):\n{1 2}",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        assert_eq!(
            p.sent.last().unwrap().1.temperature,
            Some(requests::REPAIR_TEMPERATURE)
        );

        reply(&mut a, &mut p, &mut d, json!({"code": CODE, "assumptions": []}));
        let updates = validated(&mut a, &mut p, &mut d, true);
        assert_eq!(reports(&updates)[0].attempt, Some(first_repair));
        reply(&mut a, &mut p, &mut d, explanation());
        let Some(Report::Generated { attempts, notes }) = a.report(id) else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(attempts.len(), first_repair as usize);
        assert!(attempts.last().unwrap().passed());
        assert!(attempts[..attempts.len() - 1].iter().all(|a| !a.passed()));
        assert!(
            notes.iter().any(|n| n.contains("passed after 1 repair")),
            "{notes:?}"
        );
        let doc = &d.0[&id];
        assert_eq!(doc.implementation.as_ref().unwrap().source, CODE);
        assert_eq!(
            doc.implementation
                .as_ref()
                .unwrap()
                .generation
                .as_ref()
                .unwrap()
                .attempts,
            first_repair
        );
        // The repair changed only the code.
        assert_eq!(doc.examples, before.examples);
        assert_eq!(doc.tests, before.tests);
        assert_eq!(doc.spec_fingerprint(), before.spec_fingerprint());
        // All runs share the generation run.
        assert_eq!(
            attempts[0].report.as_ref().unwrap().run,
            attempts.last().unwrap().report.as_ref().unwrap().run
        );
    }

    #[test]
    fn the_candidate_that_got_furthest_is_repaired() {
        let (mut a, mut p, mut d, id) = setup();
        // Unreadable, then loading but wrong, then structurally invalid.
        generate_code(
            &mut a,
            &mut p,
            &mut d,
            id,
            "(ns order-totals.core)\n(defn order-totals [x] (str \"x))",
        );
        reply(&mut a, &mut p, &mut d, json!({"code": WRONG, "assumptions": []}));
        validated(&mut a, &mut p, &mut d, false);
        reply(
            &mut a,
            &mut p,
            &mut d,
            json!({"code": "(ns order-totals.core)\n(defn order-totals [x] (slurp x))", "assumptions": []}),
        );
        assert_eq!(a.job().unwrap().phase, Phase::Repairing);
        let text = p.last_text();
        assert!(
            text.contains("count))"),
            "the loading candidate is repaired:\n{text}"
        );
        assert!(!text.contains("slurp"));
    }

    #[test]
    fn the_last_failed_attempt_ends_with_visible_diagnostics() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, WRONG);
        let mut stored = Vec::new();
        for attempt in 1..=MAX_ATTEMPTS {
            assert_eq!(a.job().unwrap().attempt(), Some((attempt, MAX_ATTEMPTS)));
            stored.extend(
                reports(&validated(&mut a, &mut p, &mut d, false))
                    .into_iter()
                    .cloned(),
            );
            if attempt < MAX_ATTEMPTS {
                reply(&mut a, &mut p, &mut d, json!({"code": WRONG, "assumptions": []}));
            }
        }
        assert!(a.job().is_none());
        assert_eq!(
            p.sent.len() as u32,
            SAMPLES + MAX_REPAIRS,
            "the candidates and the repairs"
        );
        let first_repair: String = p.sent[SAMPLES as usize]
            .1
            .sections
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            first_repair.contains(&format!(
                "Attempt {SAMPLES} failed in exactly the same way as attempt {}",
                SAMPLES - 1
            )),
            "a stuck model is told so:\n{first_repair}"
        );
        assert_eq!(
            stored.len() as u32,
            MAX_ATTEMPTS,
            "every candidate's report is stored"
        );
        let Some(Report::Unresolved {
            attempts,
            best,
            notes,
        }) = a.report(id)
        else {
            panic!("{:?}", a.report(id))
        };
        assert_eq!(attempts.len() as u32, MAX_ATTEMPTS);
        assert!(
            attempts
                .iter()
                .all(|a| a.report.as_ref().is_some_and(|r| !r.passed()))
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
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, WRONG);
        let Some(Report::Generated { notes, .. }) = a.report(id) else {
            panic!()
        };
        assert!(notes.iter().any(|n| n.contains("did not pass every check")));
    }

    #[test]
    fn structurally_invalid_code_is_a_failed_attempt_and_is_repaired() {
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
        for _ in 1..SAMPLES {
            reply(&mut a, &mut p, &mut d, json!({"code": slurp, "assumptions": []}));
        }
        assert_eq!(a.job().unwrap().phase, Phase::Repairing);
        assert!(p.last_text().contains("slurp is not allowed"));
        assert!(d.0[&id].implementation.is_none());
    }

    #[test]
    fn steps_quoted_with_the_threaded_value_are_found_in_the_pipeline() {
        let code =
            "(ns a)\n(defn f [orders]\n  (->> orders\n       (group-by :customer)\n       (into [])))\n";
        let mut spec = doc();
        spec.steps = vec![Step::new("group"), Step::new("collect")];
        let quoted = [
            (1, "(group-by :customer orders)".to_owned()),
            (2, "(into [] x)".to_owned()),
        ];
        let lines = step_lines(&spec, code, &quoted).unwrap();
        assert_eq!(lines[0].1.as_ref().unwrap().start, 4);
        assert_eq!(lines[1].1.as_ref().unwrap().start, 5);
        assert!(
            step_lines(&spec, code, &[(1, "(map inc xs)".to_owned())]).unwrap()[0]
                .1
                .is_err()
        );
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
    fn a_failed_explanation_keeps_the_code() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        let (req, _) = p.last();
        let updates = finish(
            &mut a,
            &mut p,
            &mut d,
            req,
            Err(GenerationError::Timeout(Duration::from_secs(1))),
        );
        assert_eq!(updates.len(), 1);
        let g = d.0[&id]
            .implementation
            .as_ref()
            .unwrap()
            .generation
            .clone()
            .unwrap();
        assert!(g.explanation.is_none());
    }

    #[test]
    fn an_unstructured_explanation_keeps_the_checked_code() {
        let (mut a, mut p, mut d, id) = setup();
        generate_code(&mut a, &mut p, &mut d, id, CODE);
        validated(&mut a, &mut p, &mut d, true);
        let (req, role) = p.last();
        let mut g = answer(role, json!({}), vec![]);
        g.value = None;
        let updates = finish(&mut a, &mut p, &mut d, req, Ok(g));
        assert!(matches!(&updates[..], [Update::Commit(..)]));
        assert_eq!(d.0[&id].implementation.as_ref().unwrap().source, CODE);
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

    /// Edge cases, accepted code, and fixes for failing cases.
    mod review;
}
