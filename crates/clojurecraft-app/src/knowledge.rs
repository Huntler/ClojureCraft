//! Documentation collection state: installation and updates, the embedding model, and searches.
//!
//! Building a collection, downloading the embedding model, and gathering what is installed run on
//! worker threads. Searches run on one long-lived thread that owns the open collection and, when
//! the collection has vectors, an embedding server on the CPU, started on the first search and
//! stopped after a few idle minutes or when the application exits. Every search carries a request
//! number, and only the newest request of each search field is applied. The same thread gathers
//! the documentation given to the model when generating code.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clojurecraft_clojure::ClojureRuntime;
use clojurecraft_domain::FunctionId;
use clojurecraft_knowledge::build::{BuildError, Sources, build_collection, extract_api, fetch_pages};
use clojurecraft_knowledge::embed::{EmbedError, Embedder, LlamaEmbedder, LlamaEmbedderConfig, QUERY_TOKENS};
use clojurecraft_knowledge::{
    BuildProgress, CollectionInfo, EmbedderIdentity, Hit, InstalledCollection, KnowledgeDirs,
    KnowledgeManifest, Provenance, Query, Retriever, Semantic, UpdateReason, update_reasons,
};
use clojurecraft_runtime::hub::Hub;
use clojurecraft_runtime::models::{FileProblem, ModelRecord};
use clojurecraft_runtime::{Cancel, DataDirs, Manifest, Progress, components, platform_key};

use crate::jobs::Event;
use clojurecraft_workflow::assistant::{self, Gather, Gathered};

/// The query-time embedding server is stopped after this long without searches.
const EMBEDDER_IDLE: Duration = Duration::from_secs(300);
/// After the embedding server failed to start, searches use text only for this long.
const EMBEDDER_RETRY: Duration = Duration::from_secs(120);

pub enum KnowledgeEvent {
    Snapshot(Box<KnowledgeSnapshot>),
    Stage(Stage),
    BuildDone(Result<String, BuildFailure>),
    ModelProgress(Progress),
    ModelDone(Result<(), String>),
    RolledBack(Result<String, String>),
    Searched {
        slot: Slot,
        request: u64,
        result: Box<Result<SearchOutcome, String>>,
    },
    Gathered {
        request: u64,
        result: Box<Result<Gathered, String>>,
    },
}

/// What a running build is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Runtime { file: String, progress: Progress },
    StartingEmbedder { gpu: bool },
    Build(BuildProgress),
}

#[derive(Clone, Debug)]
pub struct BuildFailure {
    pub message: String,
    /// The embedding model failed; building without semantic search may still succeed.
    pub embedding: bool,
}

/// What is installed, gathered off the UI thread.
#[derive(Default)]
pub struct KnowledgeSnapshot {
    pub active: Option<Result<InstalledCollection, String>>,
    pub previous: Option<InstalledCollection>,
    pub sources: Vec<Provenance>,
    pub model: Option<ModelRecord>,
    pub model_problems: Vec<FileProblem>,
    pub java: bool,
    pub clojure: bool,
    pub llama: bool,
    pub collections_bytes: u64,
    pub model_bytes: u64,
}

/// A search field: the knowledge page's browser, or a function's related-documentation search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    Browser,
    Function(FunctionId),
}

pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub semantic: Semantic,
    pub info: CollectionInfo,
    pub elapsed: Duration,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub include_restricted: bool,
    /// When the query last changed without being searched yet.
    pub dirty_since: Option<Instant>,
    request: u64,
    pub busy: bool,
    pub result: Option<Result<SearchOutcome, String>>,
    /// Identifier of the expanded result.
    pub expanded: Option<String>,
}

pub struct BuildJob {
    pub stage: Option<Stage>,
    cancel: Cancel,
    handle: Option<JoinHandle<()>>,
}

pub struct ModelTask {
    pub progress: Option<Progress>,
    cancel: Cancel,
}

enum Command {
    Search {
        slot: Slot,
        request: u64,
        query: Query,
    },
    Gather {
        request: u64,
        gather: Gather,
    },
    /// The active collection or the embedding model changed.
    Reload,
    Shutdown,
}

#[derive(Clone)]
struct Env {
    dirs: DataDirs,
    runtime: Manifest,
    manifest: KnowledgeManifest,
    platform: String,
}

impl Env {
    fn kdirs(&self) -> KnowledgeDirs {
        KnowledgeDirs::new(&self.dirs)
    }

    fn installed_model(&self) -> Option<(ModelRecord, PathBuf)> {
        let store = self.kdirs().model_store();
        let record = store.get(&self.manifest.embedding.record().key)?;
        if !store.health(&record, false).is_empty() {
            return None;
        }
        let file = store.primary_file(&record)?;
        Some((record, file))
    }

    fn embedder_config(&self, cpu_only: bool, query: bool) -> Option<LlamaEmbedderConfig> {
        let (_, server) = components::llama_server(&self.runtime, &self.dirs)?;
        let (_, model_file) = self.installed_model()?;
        Some(LlamaEmbedderConfig {
            server,
            model_file,
            model: self.manifest.embedding.clone(),
            cpu_only,
            threads: None,
            pid_file: Some(self.kdirs().embed_pid_file()),
            max_input_tokens: query.then_some(QUERY_TOKENS),
        })
    }
}

pub struct KnowledgeState {
    tx: Sender<Event>,
    ctx: egui::Context,
    env: Result<Env, String>,
    pub snapshot: Option<KnowledgeSnapshot>,
    pub job: Option<BuildJob>,
    pub failure: Option<BuildFailure>,
    /// Shown after a build or rollback succeeded.
    pub notice: Option<String>,
    pub model_task: Option<ModelTask>,
    pub model_error: Option<String>,
    searches: HashMap<Slot, SearchState>,
    next_request: u64,
    /// Finished documentation lookups for the assistant.
    pub gathered: Vec<(u64, Result<Gathered, String>)>,
    worker: Sender<Command>,
    worker_handle: Option<JoinHandle<()>>,
}

fn emit_fn(tx: &Sender<Event>, ctx: &egui::Context) -> impl Fn(KnowledgeEvent) + Send + Clone + 'static {
    let (tx, ctx) = (tx.clone(), ctx.clone());
    move |e| {
        let _ = tx.send(Event::Knowledge(Box::new(e)));
        ctx.request_repaint();
    }
}

impl KnowledgeState {
    pub fn new(tx: Sender<Event>, ctx: egui::Context) -> Self {
        let env = (|| {
            Ok(Env {
                dirs: DataDirs::discover().ok_or("No data directory is available.")?,
                runtime: Manifest::bundled().map_err(|e| e.to_string())?,
                manifest: KnowledgeManifest::bundled().map_err(|e| e.to_string())?,
                platform: platform_key(),
            })
        })();
        let (worker, rx) = mpsc::channel();
        let worker_handle = env.as_ref().ok().cloned().map(|env| {
            let emit = emit_fn(&tx, &ctx);
            thread::spawn(move || search_worker(&env, &rx, &emit))
        });
        let state = Self {
            tx,
            ctx,
            env,
            snapshot: None,
            job: None,
            failure: None,
            notice: None,
            model_task: None,
            model_error: None,
            searches: HashMap::new(),
            next_request: 0,
            gathered: Vec::new(),
            worker,
            worker_handle,
        };
        state.refresh();
        state
    }

    fn emit(&self) -> impl Fn(KnowledgeEvent) + Send + Clone + 'static {
        emit_fn(&self.tx, &self.ctx)
    }

    pub fn manifest(&self) -> Option<&KnowledgeManifest> {
        self.env.as_ref().ok().map(|e| &e.manifest)
    }

    pub fn setup_problem(&self) -> Option<&str> {
        self.env.as_ref().err().map(String::as_str)
    }

    pub fn runtime_clojure(&self) -> Option<&str> {
        self.env.as_ref().ok().map(|e| e.runtime.clojure.version.as_str())
    }

    pub fn active(&self) -> Option<&InstalledCollection> {
        self.snapshot.as_ref()?.active.as_ref()?.as_ref().ok()
    }

    /// The embedding model, if installed and intact.
    pub fn model_identity(&self) -> Option<EmbedderIdentity> {
        let s = self.snapshot.as_ref()?;
        (s.model.is_some() && s.model_problems.is_empty())
            .then(|| self.manifest().map(|m| EmbedderIdentity::of(&m.embedding)))
            .flatten()
    }

    pub fn updates(&self) -> Vec<UpdateReason> {
        match (self.active(), self.manifest(), self.runtime_clojure()) {
            (Some(a), Some(m), Some(v)) => update_reasons(&a.info, m, v, self.model_identity().as_ref()),
            _ => Vec::new(),
        }
    }

    pub fn refresh(&self) {
        let Ok(env) = self.env.clone() else { return };
        let emit = self.emit();
        thread::spawn(move || {
            let kdirs = env.kdirs();
            let collections = kdirs.collections();
            let active = collections
                .active()
                .map(|r| r.map_err(|e| format!("The installed documentation cannot be read: {e}")));
            let sources = active
                .as_ref()
                .and_then(|a| a.as_ref().ok())
                .and_then(|a| Retriever::open(&a.dir).ok())
                .map(|r| r.sources().to_vec())
                .unwrap_or_default();
            let store = kdirs.model_store();
            let model = store.get(&env.manifest.embedding.record().key);
            let model_problems = model.as_ref().map(|m| store.health(m, false)).unwrap_or_default();
            let model_bytes = model
                .as_ref()
                .map_or(0, |m| clojurecraft_runtime::models::dir_size(&store.dir(&m.key)));
            emit(KnowledgeEvent::Snapshot(Box::new(KnowledgeSnapshot {
                active,
                previous: collections.previous(),
                sources,
                model,
                model_problems,
                java: components::java(&env.runtime, &env.dirs, &env.platform).is_some(),
                clojure: components::clojure_jars(&env.runtime, &env.dirs).is_some(),
                llama: components::llama_server(&env.runtime, &env.dirs).is_some(),
                collections_bytes: collections.disk_usage(),
                model_bytes,
            })));
        });
    }

    pub fn handle(&mut self, event: KnowledgeEvent) {
        match event {
            KnowledgeEvent::Snapshot(s) => self.snapshot = Some(*s),
            KnowledgeEvent::Stage(stage) => {
                if let Some(job) = &mut self.job {
                    job.stage = Some(stage);
                }
            }
            KnowledgeEvent::BuildDone(result) => {
                if let Some(mut job) = self.job.take()
                    && let Some(h) = job.handle.take()
                {
                    let _ = h.join();
                }
                match result {
                    Ok(version) => {
                        self.failure = None;
                        self.notice = Some(format!("Documentation collection {version} is now active."));
                    }
                    Err(f) if f.message == "cancelled" => {}
                    Err(f) => self.failure = Some(f),
                }
                let _ = self.worker.send(Command::Reload);
                self.refresh();
            }
            KnowledgeEvent::ModelProgress(p) => {
                if let Some(t) = &mut self.model_task {
                    t.progress = Some(p);
                }
            }
            KnowledgeEvent::ModelDone(result) => {
                self.model_task = None;
                self.model_error = result.err().filter(|e| e != "download cancelled");
                let _ = self.worker.send(Command::Reload);
                self.refresh();
            }
            KnowledgeEvent::RolledBack(result) => {
                match result {
                    Ok(v) => self.notice = Some(format!("Documentation collection {v} is active again.")),
                    Err(e) => {
                        self.failure = Some(BuildFailure {
                            message: e,
                            embedding: false,
                        });
                    }
                }
                let _ = self.worker.send(Command::Reload);
                self.refresh();
            }
            KnowledgeEvent::Searched {
                slot,
                request,
                result,
            } => {
                if let Some(s) = self.searches.get_mut(&slot)
                    && s.request == request
                {
                    s.busy = false;
                    s.result = Some(*result);
                }
            }
            KnowledgeEvent::Gathered { request, result } => self.gathered.push((request, *result)),
        }
    }

    /// Starts gathering documentation for a generation; `None` when none is installed.
    pub fn gather(&mut self, gather: Gather) -> Option<u64> {
        self.active()?;
        self.next_request += 1;
        let request = self.next_request;
        self.worker.send(Command::Gather { request, gather }).ok()?;
        Some(request)
    }

    /// Builds (or rebuilds) the collection. `gpu_free` allows computing embeddings on the GPU,
    /// which is only offered while no generator model occupies it.
    pub fn build(&mut self, with_embeddings: bool, gpu_free: bool) {
        if self.job.is_some() {
            return;
        }
        let Ok(env) = self.env.clone() else { return };
        self.failure = None;
        self.notice = None;
        let cancel = Cancel::default();
        let emit = self.emit();
        let c = cancel.clone();
        let handle = thread::spawn(move || {
            let result = run_build(&env, with_embeddings, gpu_free, &c, &emit);
            emit(KnowledgeEvent::BuildDone(result));
        });
        self.job = Some(BuildJob {
            stage: None,
            cancel,
            handle: Some(handle),
        });
    }

    pub fn cancel_build(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
    }

    pub fn rollback(&mut self) {
        let Ok(env) = self.env.clone() else { return };
        let emit = self.emit();
        thread::spawn(move || {
            let r = env
                .kdirs()
                .collections()
                .rollback()
                .map(|c| c.version)
                .map_err(|e| format!("The previous collection could not be activated: {e}"));
            emit(KnowledgeEvent::RolledBack(r));
        });
    }

    pub fn install_model(&mut self) {
        if self.model_task.is_some() {
            return;
        }
        let Ok(env) = self.env.clone() else { return };
        self.model_error = None;
        let cancel = Cancel::default();
        let emit = self.emit();
        let c = cancel.clone();
        thread::spawn(move || {
            let record = env.manifest.embedding.record();
            let mut progress = |_: &str, p: Progress| emit(KnowledgeEvent::ModelProgress(p));
            let result = env
                .kdirs()
                .model_store()
                .install(&record, &Hub::new(None), &c, &mut progress)
                .map(|_| ())
                .map_err(|e| e.to_string());
            emit(KnowledgeEvent::ModelDone(result));
        });
        self.model_task = Some(ModelTask {
            progress: None,
            cancel,
        });
    }

    pub fn cancel_model(&mut self) {
        if let Some(t) = &self.model_task {
            t.cancel.cancel();
        }
    }

    pub fn remove_model(&mut self) {
        let Ok(env) = self.env.clone() else { return };
        // The search worker must let go of the running server first.
        let _ = self.worker.send(Command::Reload);
        let emit = self.emit();
        thread::spawn(move || {
            let key = env.manifest.embedding.record().key;
            let result = env
                .kdirs()
                .model_store()
                .remove(&key)
                .map(|_| ())
                .map_err(|e| e.to_string());
            emit(KnowledgeEvent::ModelDone(result));
        });
    }

    pub fn search_state(&mut self, slot: Slot) -> &mut SearchState {
        self.searches.entry(slot).or_default()
    }

    /// Starts a search for the slot's current query; supersedes its previous one.
    pub fn search(&mut self, slot: Slot, query: Query) {
        self.next_request += 1;
        let request = self.next_request;
        let s = self.searches.entry(slot).or_default();
        s.request = request;
        s.busy = true;
        s.dirty_since = None;
        let _ = self.worker.send(Command::Search { slot, request, query });
    }

    /// Stops the search worker (and its embedding server) and any build in progress.
    pub fn shutdown(&mut self) {
        if let Some(job) = &mut self.job {
            job.cancel.cancel();
            if let Some(h) = job.handle.take() {
                // Builds check for cancellation between steps; an embedding batch takes seconds.
                let until = Instant::now() + Duration::from_secs(10);
                while !h.is_finished() && Instant::now() < until {
                    thread::sleep(Duration::from_millis(50));
                }
                if h.is_finished() {
                    let _ = h.join();
                }
            }
        }
        if let Some(t) = &self.model_task {
            t.cancel.cancel();
        }
        let _ = self.worker.send(Command::Shutdown);
        if let Some(h) = self.worker_handle.take() {
            let _ = h.join();
        }
    }
}

fn sandbox_helper() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|d| d.join(clojurecraft_sandbox::HELPER_NAME))
        .filter(|p| p.is_file())
}

fn failure(e: impl std::fmt::Display) -> BuildFailure {
    BuildFailure {
        message: e.to_string(),
        embedding: false,
    }
}

fn run_build(
    env: &Env,
    with_embeddings: bool,
    gpu_free: bool,
    cancel: &Cancel,
    emit: &(impl Fn(KnowledgeEvent) + Send + Clone + 'static),
) -> Result<String, BuildFailure> {
    let kdirs = env.kdirs();
    let mut runtime_progress = |file: &str, progress: Progress| {
        emit(KnowledgeEvent::Stage(Stage::Runtime {
            file: file.to_owned(),
            progress,
        }));
    };
    // The API documentation is read from the pinned Clojure runtime, installed here if needed.
    components::install_jre(
        &env.runtime,
        &env.dirs,
        &env.platform,
        cancel,
        &mut runtime_progress,
    )
    .map_err(failure)?;
    components::install_clojure(&env.runtime, &env.dirs, cancel, &mut runtime_progress).map_err(failure)?;
    let (java, java_home) = components::java(&env.runtime, &env.dirs, &env.platform)
        .ok_or_else(|| failure("The Java runtime is not installed."))?;
    let jars = components::clojure_jars(&env.runtime, &env.dirs)
        .ok_or_else(|| failure("Clojure is not installed."))?;
    let helper = sandbox_helper().ok_or_else(|| {
        failure(format!(
            "The sandbox helper ({}) was not found next to the application, so the Clojure runtime \
             cannot be run safely.",
            clojurecraft_sandbox::HELPER_NAME
        ))
    })?;
    let clojure = ClojureRuntime {
        java,
        java_home,
        jars,
        sandbox_helper: helper,
        work_dir: env.dirs.root().join("harness"),
        project_classpath: Vec::new(),
    };
    let mut progress = |p: BuildProgress| emit(KnowledgeEvent::Stage(Stage::Build(p)));
    let as_failure = |e: BuildError| match e {
        BuildError::Cancelled => failure("cancelled"),
        BuildError::Embedding(e) => BuildFailure {
            message: format!("Computing embeddings failed: {e}"),
            embedding: true,
        },
        other => failure(other),
    };
    let pages = fetch_pages(&env.manifest, &kdirs.downloads(), cancel, &mut progress).map_err(as_failure)?;
    progress(BuildProgress {
        phase: clojurecraft_knowledge::Phase::Api,
        done: 0,
        total: 1,
    });
    let api = extract_api(&clojure, &env.runtime.clojure.version).map_err(as_failure)?;
    let mut embedder = None;
    if with_embeddings {
        let start = |gpu: bool| -> Result<LlamaEmbedder, BuildFailure> {
            emit(KnowledgeEvent::Stage(Stage::StartingEmbedder { gpu }));
            let cfg = env.embedder_config(!gpu, false).ok_or_else(|| BuildFailure {
                message: "The embedding model or the llama.cpp runtime is not installed.".into(),
                embedding: true,
            })?;
            LlamaEmbedder::start(&cfg, &|| cancel.is_cancelled()).map_err(|e| match e {
                EmbedError::Load(clojurecraft_inference::LoadError::Cancelled) => failure("cancelled"),
                e => BuildFailure {
                    message: format!("The embedding model could not be started: {e}"),
                    embedding: true,
                },
            })
        };
        // A GPU that cannot hold the model falls back to the CPU.
        let started = match gpu_free {
            true => start(true).or_else(|e| {
                if e.message == "cancelled" {
                    Err(e)
                } else {
                    start(false)
                }
            }),
            false => start(false),
        };
        embedder = Some(started?);
    }
    let installed = build_collection(
        &env.manifest,
        &kdirs.collections(),
        &Sources { api, pages },
        embedder.as_mut().map(|e| e as &mut dyn Embedder),
        &|| cancel.is_cancelled(),
        &mut progress,
    )
    .map_err(as_failure)?;
    Ok(installed.version)
}

struct Worker<'a> {
    env: &'a Env,
    retriever: Option<Retriever>,
    /// Version of the collection `retriever` reads.
    version: String,
    embedder: Option<LlamaEmbedder>,
    embedder_failed: Option<(Instant, String)>,
    last_search: Instant,
}

impl Worker<'_> {
    fn ensure_retriever(&mut self) -> Result<&mut Retriever, String> {
        if self.retriever.is_none() {
            let active = self
                .env
                .kdirs()
                .collections()
                .active()
                .ok_or("No documentation collection is installed.")?
                .map_err(|e| e.to_string())?;
            self.retriever = Some(Retriever::open(&active.dir).map_err(|e| e.to_string())?);
            self.version = active.version;
        }
        self.retriever.as_mut().ok_or_else(|| "unavailable".to_owned())
    }

    fn ensure_embedder(&mut self) {
        let wants = self
            .retriever
            .as_ref()
            .and_then(|r| r.info().embedding.as_ref())
            .is_some();
        let recently_failed = self
            .embedder_failed
            .as_ref()
            .is_some_and(|(t, _)| t.elapsed() < EMBEDDER_RETRY);
        if !wants || self.embedder.is_some() || recently_failed {
            return;
        }
        let Some(cfg) = self.env.embedder_config(true, true) else {
            return;
        };
        match LlamaEmbedder::start(&cfg, &|| false) {
            Ok(e) => {
                self.embedder = Some(e);
                self.embedder_failed = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "embedding model for search");
                self.embedder_failed = Some((Instant::now(), e.to_string()));
            }
        }
    }

    fn search(&mut self, query: &Query) -> Result<SearchOutcome, String> {
        let started = Instant::now();
        self.last_search = started;
        self.ensure_retriever()?;
        self.ensure_embedder();
        let failed = self.embedder_failed.as_ref().map(|(_, e)| e.clone());
        let retriever = self.retriever.as_mut().ok_or("unavailable")?;
        let embedder = self.embedder.as_mut().map(|e| e as &mut dyn Embedder);
        let mut res = retriever.search(query, embedder).map_err(|e| e.to_string())?;
        if let (Semantic::Unavailable, Some(why)) = (&res.semantic, failed) {
            res.semantic = Semantic::Failed(why);
        }
        if matches!(res.semantic, Semantic::Failed(_)) {
            // A server that stopped answering is replaced on the next search.
            self.embedder = None;
        }
        Ok(SearchOutcome {
            hits: res.hits,
            semantic: res.semantic,
            info: retriever.info().clone(),
            elapsed: started.elapsed(),
        })
    }
}

impl Worker<'_> {
    fn gather(&mut self, g: &Gather) -> Result<Gathered, String> {
        self.last_search = Instant::now();
        self.ensure_retriever()?;
        self.ensure_embedder();
        let retriever = self.retriever.as_mut().ok_or("unavailable")?;
        let embedder = self.embedder.as_mut().map(|e| e as &mut dyn Embedder);
        let (found, semantic_failed) = assistant::gather(retriever, embedder, g, &self.version)?;
        if semantic_failed {
            // A server that stopped answering is replaced on the next search.
            self.embedder = None;
        }
        Ok(found)
    }
}

fn search_worker(env: &Env, rx: &Receiver<Command>, emit: &impl Fn(KnowledgeEvent)) {
    if let Some((_, server)) = components::llama_server(&env.runtime, &env.dirs) {
        clojurecraft_inference::process::reap_stale(&env.kdirs().embed_pid_file(), &server);
    }
    let mut w = Worker {
        env,
        retriever: None,
        version: String::new(),
        embedder: None,
        embedder_failed: None,
        last_search: Instant::now(),
    };
    loop {
        let first = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(c) => c,
            Err(RecvTimeoutError::Timeout) => {
                if w.embedder.is_some() && w.last_search.elapsed() > EMBEDDER_IDLE {
                    w.embedder = None;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Only the newest search of each field matters.
        let mut pending: Vec<Command> = vec![first];
        pending.extend(rx.try_iter());
        let mut latest: HashMap<Slot, (u64, Query)> = HashMap::new();
        let mut gathers = Vec::new();
        for c in pending {
            match c {
                Command::Shutdown => return,
                Command::Gather { request, gather } => gathers.push((request, gather)),
                Command::Reload => {
                    w.retriever = None;
                    w.embedder = None;
                    w.embedder_failed = None;
                }
                Command::Search { slot, request, query } => {
                    latest.insert(slot, (request, query));
                }
            }
        }
        for (request, gather) in gathers {
            let result = Box::new(w.gather(&gather));
            emit(KnowledgeEvent::Gathered { request, result });
        }
        for (slot, (request, query)) in latest {
            let result = Box::new(w.search(&query));
            emit(KnowledgeEvent::Searched {
                slot,
                request,
                result,
            });
        }
    }
}
