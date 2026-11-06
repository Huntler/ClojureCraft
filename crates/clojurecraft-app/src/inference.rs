//! Inference setup state: hardware, backend, the llama.cpp runtime, installed models, the
//! Hugging Face token, and the downloads in progress.
//!
//! Everything slow (detection, downloads, hashing, credential-store access, Hub lookups) runs on
//! worker threads that report [`InferenceEvent`]s; the UI only reads this state and calls methods
//! that start work.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;

use clojurecraft_credentials::{CredentialStore, HUGGING_FACE_TOKEN, MemoryStore, Secret, platform_store};
use clojurecraft_domain::SecretRegistry;
use clojurecraft_hardware::{Backend, HardwareReport, MemoryBudget, default_budget};
use clojurecraft_runtime::catalog::{Catalog, OffloadPlan, estimate_from_metadata, offload_plan};
use clojurecraft_runtime::components::{self, LlamaBuild};
use clojurecraft_runtime::hub::{GgufOption, Hub, Inspection, RepoInfo, gguf_options, inspect};
use clojurecraft_runtime::models::{FileProblem, ModelRecord, ModelStore};
use clojurecraft_runtime::settings::{InferenceSettings, SetupState};
use clojurecraft_runtime::usage::{DiskUsage, clear_cached_archives, disk_usage};
use clojurecraft_runtime::{Cancel, DataDirs, Manifest, Progress, platform_key};

use crate::jobs::Event;
use crate::model::ModelRuntime;

/// Results of background inference work.
pub enum InferenceEvent {
    Detected(Box<HardwareReport>),
    Refreshed(Box<Snapshot>),
    RuntimeProgress {
        file: String,
        progress: Progress,
    },
    RuntimeDone {
        backend: Backend,
        result: Result<String, String>,
    },
    ModelProgress {
        key: String,
        file: String,
        progress: Progress,
    },
    ModelDone {
        key: String,
        result: Result<ModelRecord, String>,
    },
    Verified {
        key: String,
        problems: Vec<FileProblem>,
    },
    Resolved {
        request: u64,
        result: Result<RepoInfo, String>,
    },
    Inspected {
        request: u64,
        index: usize,
        result: Result<Inspection, String>,
    },
    /// The credential store was opened (or not) and the saved token read.
    Credentials {
        store: Result<Arc<dyn CredentialStore>, String>,
        token: Result<Option<Secret>, String>,
    },
    TokenSaved(Result<(), String>),
    TokenRemoved(Result<(), String>),
    /// Lifecycle and generation events of the inference service.
    Service(Box<clojurecraft_inference::ServiceEvent>),
}

/// What is installed on disk, gathered off the UI thread.
#[derive(Default)]
pub struct Snapshot {
    pub builds: Vec<LlamaBuild>,
    /// Devices the active llama.cpp build reports, or why it could not run.
    pub devices: Option<Result<Vec<String>, String>>,
    pub models: Vec<ModelRecord>,
    /// Quick (size-only) problems per installed model.
    pub problems: Vec<(String, Vec<FileProblem>)>,
    pub unreadable: Vec<String>,
    pub usage: DiskUsage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    Running {
        file: String,
        progress: Option<Progress>,
    },
    Queued,
    Failed(String),
}

pub struct ModelTask {
    pub record: ModelRecord,
    pub state: TaskState,
    cancel: Cancel,
}

pub struct RuntimeTask {
    pub backend: Backend,
    pub state: TaskState,
    cancel: Cancel,
}

/// Lookup of a repository outside the catalog.
#[derive(Default)]
pub struct CustomLookup {
    pub repo: String,
    pub revision: String,
    request: u64,
    pub busy: bool,
    pub result: Option<Result<RepoInfo, String>>,
    pub options: Vec<GgufOption>,
    /// Per option: inspection in progress, or its result.
    pub inspections: Vec<Option<Result<Inspection, String>>>,
    pub inspecting: Vec<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenState {
    Loading,
    Saved,
    NotSaved,
    /// Held for this session only because no credential store is available.
    SessionOnly,
}

/// Something generation needs that is not ready yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Missing {
    Runtime,
    Model,
    ActiveModelDamaged(String),
}

impl Missing {
    pub fn describe(&self) -> String {
        match self {
            Self::Runtime => "The llama.cpp inference runtime is not installed.".into(),
            Self::Model => "No coding model is installed and selected.".into(),
            Self::ActiveModelDamaged(why) => format!("The selected model needs attention: {why}."),
        }
    }
}

pub struct InferenceState {
    tx: Sender<Event>,
    ctx: egui::Context,
    secrets: Arc<SecretRegistry>,
    pub dirs: Option<DataDirs>,
    pub manifest: Option<Manifest>,
    pub catalog: Option<Catalog>,
    pub platform: String,
    pub settings: InferenceSettings,
    /// Problem with the settings file or with saving it.
    pub settings_problem: Option<String>,
    pub detected: Option<HardwareReport>,
    pub snapshot: Option<Snapshot>,
    pub runtime_task: Option<RuntimeTask>,
    pub model_tasks: VecDeque<ModelTask>,
    pub lookup: CustomLookup,
    pub verifying: Vec<String>,
    pub verified: Vec<(String, Vec<FileProblem>)>,
    store: Result<Arc<dyn CredentialStore>, String>,
    session_store: Arc<MemoryStore>,
    token: Option<Secret>,
    pub token_state: TokenState,
    pub token_problem: Option<String>,
    pub notices: Vec<String>,
    /// The loaded model and requests to it.
    pub model: ModelRuntime,
}

impl InferenceState {
    pub fn new(tx: Sender<Event>, ctx: egui::Context, secrets: Arc<SecretRegistry>) -> Self {
        let dirs = DataDirs::discover();
        let (settings, settings_problem) = dirs
            .as_ref()
            .map(|d| InferenceSettings::load(&d.settings()))
            .unwrap_or_else(|| {
                (
                    InferenceSettings::default(),
                    Some("No data directory is available.".into()),
                )
            });
        let manifest = Manifest::bundled()
            .map_err(|e| tracing::error!(error = %e, "runtime manifest"))
            .ok();
        let catalog = Catalog::bundled()
            .map_err(|e| tracing::error!(error = %e, "model catalog"))
            .ok();
        let mut state = Self {
            tx,
            ctx,
            secrets,
            dirs,
            manifest,
            catalog,
            platform: platform_key(),
            settings,
            settings_problem,
            detected: None,
            snapshot: None,
            runtime_task: None,
            model_tasks: VecDeque::new(),
            lookup: CustomLookup::default(),
            verifying: Vec::new(),
            verified: Vec::new(),
            store: Err(String::new()),
            session_store: Arc::new(MemoryStore::default()),
            token: None,
            token_state: TokenState::Loading,
            token_problem: None,
            notices: Vec::new(),
            model: ModelRuntime::default(),
        };
        state.model = ModelRuntime::start(state.emit());
        state.detect();
        state.refresh();
        state.load_token();
        state
    }

    fn emit(&self) -> impl Fn(InferenceEvent) + Send + Clone + 'static {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        move |e| {
            let _ = tx.send(Event::Inference(Box::new(e)));
            ctx.request_repaint();
        }
    }

    // ---- derived values ---------------------------------------------------------------------

    /// Detection with the user's corrections applied.
    pub fn report(&self) -> Option<HardwareReport> {
        self.detected.as_ref().map(|d| self.settings.hardware.apply(d))
    }

    /// Backends with a pinned build for this platform, best first for the corrected hardware.
    pub fn offered_backends(&self) -> Vec<Backend> {
        let pinned = self
            .manifest
            .as_ref()
            .map(|m| m.llama_backends(&self.platform))
            .unwrap_or_default();
        let mut out: Vec<Backend> = self
            .report()
            .map(|r| r.candidate_backends())
            .unwrap_or_else(|| vec![Backend::Cpu])
            .into_iter()
            .filter(|b| pinned.contains(b))
            .collect();
        for b in pinned {
            if !out.contains(&b) {
                out.push(b);
            }
        }
        out
    }

    /// Backends the corrected hardware can use, best first.
    pub fn suitable_backends(&self) -> Vec<Backend> {
        self.report()
            .map(|r| r.candidate_backends())
            .unwrap_or_else(|| vec![Backend::Cpu])
    }

    pub fn recommended_backend(&self) -> Backend {
        let offered = self.offered_backends();
        self.suitable_backends()
            .into_iter()
            .find(|b| offered.contains(b))
            .unwrap_or(Backend::Cpu)
    }

    pub fn backend(&self) -> Backend {
        self.settings
            .backend
            .unwrap_or_else(|| self.recommended_backend())
    }

    pub fn derived_budget(&self) -> Option<MemoryBudget> {
        Some(default_budget(&self.report()?, self.backend()))
    }

    pub fn budget(&self) -> Option<MemoryBudget> {
        Some(self.settings.effective_budget(self.derived_budget()?))
    }

    pub fn default_context(&self) -> u32 {
        self.catalog.as_ref().map_or(16384, |c| c.default_context)
    }

    pub fn active_build(&self) -> Option<&LlamaBuild> {
        self.snapshot.as_ref()?.builds.iter().find(|b| b.active)
    }

    pub fn installed_build(&self, backend: Backend) -> Option<&LlamaBuild> {
        self.snapshot
            .as_ref()?
            .builds
            .iter()
            .find(|b| b.backend == Some(backend) && b.pinned)
    }

    pub fn models(&self) -> &[ModelRecord] {
        self.snapshot.as_ref().map_or(&[], |s| &s.models)
    }

    pub fn installed(&self, key: &str) -> Option<&ModelRecord> {
        self.models().iter().find(|m| m.key == key)
    }

    pub fn active_model(&self) -> Option<&ModelRecord> {
        self.installed(self.settings.active_model.as_deref()?)
    }

    pub fn problems(&self, key: &str) -> &[FileProblem] {
        let verified = self
            .verified
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, p)| p.as_slice());
        let quick = self
            .snapshot
            .as_ref()
            .and_then(|s| s.problems.iter().find(|(k, _)| k == key))
            .map(|(_, p)| p.as_slice());
        match (verified, quick) {
            (Some(v), _) if !v.is_empty() => v,
            (_, Some(q)) => q,
            _ => &[],
        }
    }

    pub fn context_for(&self, record: &ModelRecord) -> u32 {
        self.settings
            .effective_context(self.default_context(), record.context_length)
    }

    /// Estimated GPU/RAM split for an installed model under the current budget.
    pub fn offload_for(&self, record: &ModelRecord) -> Option<OffloadPlan> {
        let est = estimate_from_metadata(
            record.total_size(),
            record.kv_bytes_per_token?,
            self.context_for(record),
        );
        Some(offload_plan(&est, record.layers?, &self.budget()?))
    }

    /// What generation still needs; empty when setup is complete.
    pub fn missing(&self) -> Vec<Missing> {
        let mut out = Vec::new();
        if self.snapshot.is_none() {
            return out;
        }
        if self.active_build().is_none() {
            out.push(Missing::Runtime);
        }
        match self.active_model() {
            None => out.push(Missing::Model),
            Some(m) => {
                if let Some(p) = self.problems(&m.key).first() {
                    out.push(Missing::ActiveModelDamaged(p.to_string()));
                }
            }
        }
        out
    }

    pub fn token_present(&self) -> bool {
        self.token.is_some()
    }

    fn hub(&self) -> Hub {
        Hub::new(self.token.as_ref().map(|t| t.expose().to_owned()))
    }

    pub fn store_name(&self) -> Option<String> {
        self.store.as_ref().ok().map(|s| s.name().to_owned())
    }

    pub fn store_problem(&self) -> Option<&str> {
        self.store
            .as_ref()
            .err()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    // ---- settings ---------------------------------------------------------------------------

    pub fn save_settings(&mut self) {
        let Some(dirs) = &self.dirs else { return };
        match self.settings.save(&dirs.settings()) {
            Ok(()) => self.settings_problem = None,
            Err(e) => {
                tracing::warn!(error = %e, "could not save settings");
                self.settings_problem = Some(format!("Settings could not be saved: {e}"));
            }
        }
    }

    pub fn set_setup(&mut self, state: SetupState) {
        self.settings.setup = state;
        self.save_settings();
    }

    // ---- background work --------------------------------------------------------------------

    pub fn detect(&self) {
        let emit = self.emit();
        thread::spawn(move || {
            emit(InferenceEvent::Detected(
                Box::new(clojurecraft_hardware::detect()),
            ))
        });
    }

    /// Re-reads installed runtimes and models.
    pub fn refresh(&self) {
        let (Some(dirs), Some(manifest)) = (self.dirs.clone(), self.manifest.clone()) else {
            return;
        };
        let emit = self.emit();
        thread::spawn(move || {
            let builds = components::llama_builds(&manifest, &dirs);
            let devices = builds
                .iter()
                .find(|b| b.active)
                .map(|b| components::check_llama(&manifest, &dirs, &b.version));
            let store = ModelStore::new(dirs.models(), dirs.downloads());
            store.clean_leftovers();
            let (models, unreadable) = store.list();
            let problems = models
                .iter()
                .map(|m| (m.key.clone(), store.health(m, false)))
                .filter(|(_, p)| !p.is_empty())
                .collect();
            emit(InferenceEvent::Refreshed(Box::new(Snapshot {
                builds,
                devices,
                problems,
                unreadable: unreadable.iter().map(ToString::to_string).collect(),
                models,
                usage: disk_usage(&dirs),
            })));
        });
    }

    pub fn install_runtime(&mut self, backend: Backend) {
        if self.runtime_task.is_some() {
            return;
        }
        let (Some(dirs), Some(manifest)) = (self.dirs.clone(), self.manifest.clone()) else {
            return;
        };
        let cancel = Cancel::default();
        self.runtime_task = Some(RuntimeTask {
            backend,
            state: TaskState::Running {
                file: String::new(),
                progress: None,
            },
            cancel: cancel.clone(),
        });
        let platform = self.platform.clone();
        let emit = self.emit();
        thread::spawn(move || {
            let mut on_progress = |file: &str, progress: Progress| {
                emit(InferenceEvent::RuntimeProgress {
                    file: file.to_owned(),
                    progress,
                });
            };
            let result =
                components::install_llama(&manifest, &dirs, &platform, backend, &cancel, &mut on_progress)
                    .map(|i| i.version)
                    .map_err(|e| e.to_string());
            emit(InferenceEvent::RuntimeDone { backend, result });
        });
    }

    pub fn cancel_runtime(&mut self) {
        if let Some(t) = &self.runtime_task {
            t.cancel.cancel();
        }
    }

    pub fn dismiss_runtime_error(&mut self) {
        if matches!(
            self.runtime_task.as_ref().map(|t| &t.state),
            Some(TaskState::Failed(_))
        ) {
            self.runtime_task = None;
        }
    }

    /// Queues a model download; models install one at a time.
    pub fn install_model(&mut self, record: ModelRecord) {
        if self
            .model_tasks
            .iter()
            .any(|t| t.record.key == record.key && !matches!(t.state, TaskState::Failed(_)))
        {
            return;
        }
        self.model_tasks.retain(|t| t.record.key != record.key);
        self.model_tasks.push_back(ModelTask {
            record,
            state: TaskState::Queued,
            cancel: Cancel::default(),
        });
        self.start_next_model();
    }

    fn start_next_model(&mut self) {
        if self
            .model_tasks
            .iter()
            .any(|t| matches!(t.state, TaskState::Running { .. }))
        {
            return;
        }
        let Some(dirs) = self.dirs.clone() else { return };
        let hub = self.hub();
        let emit = self.emit();
        let Some(task) = self.model_tasks.iter_mut().find(|t| t.state == TaskState::Queued) else {
            return;
        };
        task.state = TaskState::Running {
            file: String::new(),
            progress: None,
        };
        let record = task.record.clone();
        let cancel = task.cancel.clone();
        thread::spawn(move || {
            let store = ModelStore::new(dirs.models(), dirs.downloads());
            let key = record.key.clone();
            let mut on_progress = |file: &str, progress: Progress| {
                emit(InferenceEvent::ModelProgress {
                    key: key.clone(),
                    file: file.to_owned(),
                    progress,
                });
            };
            let result = store
                .install(&record, &hub, &cancel, &mut on_progress)
                .map_err(|e| e.to_string());
            emit(InferenceEvent::ModelDone { key, result });
        });
    }

    pub fn cancel_model(&mut self, key: &str) {
        if let Some(i) = self.model_tasks.iter().position(|t| t.record.key == key) {
            match self.model_tasks[i].state {
                TaskState::Running { .. } => self.model_tasks[i].cancel.cancel(),
                _ => {
                    self.model_tasks.remove(i);
                }
            }
        }
    }

    pub fn model_task(&self, key: &str) -> Option<&ModelTask> {
        self.model_tasks.iter().find(|t| t.record.key == key)
    }

    pub fn remove_model(&mut self, key: &str) {
        let Some(dirs) = self.dirs.clone() else { return };
        self.release_model(key);
        let store = ModelStore::new(dirs.models(), dirs.downloads());
        match store.remove(key) {
            Ok(_) => {
                if self.settings.active_model.as_deref() == Some(key) {
                    self.settings.active_model = None;
                    self.save_settings();
                }
                self.verified.retain(|(k, _)| k != key);
            }
            Err(e) => self.notices.push(format!("Could not remove the model: {e}")),
        }
        self.refresh();
    }

    pub fn verify_model(&mut self, key: &str) {
        let (Some(dirs), Some(record)) = (self.dirs.clone(), self.installed(key).cloned()) else {
            return;
        };
        if self.verifying.iter().any(|k| k == key) {
            return;
        }
        self.verifying.push(key.to_owned());
        let emit = self.emit();
        thread::spawn(move || {
            let store = ModelStore::new(dirs.models(), dirs.downloads());
            let problems = store.health(&record, true);
            emit(InferenceEvent::Verified {
                key: record.key,
                problems,
            });
        });
    }

    pub fn use_model(&mut self, key: &str) {
        self.settings.active_model = Some(key.to_owned());
        self.save_settings();
    }

    pub fn activate_build(&mut self, version: &str) {
        if let Some(dirs) = &self.dirs
            && let Err(e) = components::activate_llama(dirs, version)
        {
            self.notices.push(format!("Could not switch the runtime: {e}"));
        }
        self.refresh();
    }

    pub fn rollback_build(&mut self) {
        if let Some(dirs) = &self.dirs
            && let Err(e) = components::rollback_llama(dirs)
        {
            self.notices.push(format!("Could not roll back: {e}"));
        }
        self.refresh();
    }

    pub fn remove_build(&mut self, version: &str) {
        if let Some(dirs) = &self.dirs
            && let Err(e) = components::remove_llama(dirs, version)
        {
            self.notices.push(format!("Could not remove the runtime: {e}"));
        }
        self.refresh();
    }

    pub fn discard_partial_downloads(&mut self) {
        if let Some(dirs) = &self.dirs {
            ModelStore::new(dirs.models(), dirs.downloads()).discard_partial_downloads();
        }
        self.refresh();
    }

    pub fn clear_cached_archives(&mut self) {
        if let Some(dirs) = &self.dirs {
            clear_cached_archives(dirs);
        }
        self.refresh();
    }

    // ---- custom repositories ----------------------------------------------------------------

    pub fn look_up(&mut self) {
        self.lookup.request += 1;
        self.lookup.busy = true;
        self.lookup.result = None;
        self.lookup.options.clear();
        self.lookup.inspections.clear();
        self.lookup.inspecting.clear();
        let (repo, rev, request) = (
            self.lookup.repo.trim().to_owned(),
            self.lookup.revision.trim().to_owned(),
            self.lookup.request,
        );
        let hub = self.hub();
        let emit = self.emit();
        thread::spawn(move || {
            let result = hub
                .repo_info(&repo, Some(rev.as_str()).filter(|r| !r.is_empty()))
                .map_err(|e| e.to_string());
            emit(InferenceEvent::Resolved { request, result });
        });
    }

    pub fn inspect_option(&mut self, index: usize) {
        let (Some(Ok(info)), Some(option), Some(catalog)) = (
            self.lookup.result.clone(),
            self.lookup.options.get(index).cloned(),
            self.catalog.clone(),
        ) else {
            return;
        };
        if self.lookup.inspecting.get(index).copied().unwrap_or(false) {
            return;
        }
        if let Some(flag) = self.lookup.inspecting.get_mut(index) {
            *flag = true;
        }
        let request = self.lookup.request;
        let hub = self.hub();
        let emit = self.emit();
        thread::spawn(move || {
            let result = inspect(&hub, &info, &option, &catalog).map_err(|e| e.to_string());
            emit(InferenceEvent::Inspected {
                request,
                index,
                result,
            });
        });
    }

    // ---- token ------------------------------------------------------------------------------

    fn load_token(&mut self) {
        let emit = self.emit();
        thread::spawn(move || match platform_store() {
            Ok(store) => {
                let token = store.get(HUGGING_FACE_TOKEN).map_err(|e| e.to_string());
                emit(InferenceEvent::Credentials {
                    store: Ok(store),
                    token,
                });
            }
            Err(e) => emit(InferenceEvent::Credentials {
                store: Err(e.to_string()),
                token: Ok(None),
            }),
        });
    }

    /// Uses the token now and saves it in the credential store (in the background, since the
    /// store may ask the user to unlock it).
    pub fn save_token(&mut self, value: &str) {
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        let secret = Secret::new(value);
        self.secrets.register(value);
        self.token = Some(secret.clone());
        self.token_problem = None;
        match self.store.clone() {
            Ok(store) => {
                self.token_state = TokenState::Loading;
                let emit = self.emit();
                thread::spawn(move || {
                    let result = store.set(HUGGING_FACE_TOKEN, &secret).map_err(|e| e.to_string());
                    emit(InferenceEvent::TokenSaved(result));
                });
            }
            Err(e) => self.session_only(&secret, &e),
        }
    }

    fn session_only(&mut self, secret: &Secret, why: &str) {
        let _ = self.session_store.set(HUGGING_FACE_TOKEN, secret);
        self.token_state = TokenState::SessionOnly;
        self.token_problem = Some(if why.is_empty() {
            "The token is used for this session only.".to_owned()
        } else {
            format!("The token could not be saved ({why}); it is used for this session only.")
        });
    }

    pub fn remove_token(&mut self) {
        let _ = self.session_store.delete(HUGGING_FACE_TOKEN);
        self.token = None;
        self.token_problem = None;
        match self.store.clone() {
            Ok(store) => {
                self.token_state = TokenState::Loading;
                let emit = self.emit();
                thread::spawn(move || {
                    let result = store.delete(HUGGING_FACE_TOKEN).map_err(|e| e.to_string());
                    emit(InferenceEvent::TokenRemoved(result));
                });
            }
            Err(_) => self.token_state = TokenState::NotSaved,
        }
    }

    // ---- events -----------------------------------------------------------------------------

    pub fn handle(&mut self, event: InferenceEvent) {
        match event {
            InferenceEvent::Detected(report) => self.detected = Some(*report),
            InferenceEvent::Service(event) => self.handle_service(*event),
            InferenceEvent::Refreshed(snapshot) => {
                self.snapshot = Some(*snapshot);
                // A single installed model is selected automatically.
                if self.active_model().is_none()
                    && let [only] = self.models()
                {
                    let key = only.key.clone();
                    self.use_model(&key);
                }
            }
            InferenceEvent::RuntimeProgress { file, progress } => {
                if let Some(t) = &mut self.runtime_task {
                    t.state = TaskState::Running {
                        file,
                        progress: Some(progress),
                    };
                }
            }
            InferenceEvent::RuntimeDone { backend, result } => {
                let cancelled = self
                    .runtime_task
                    .as_ref()
                    .is_some_and(|t| t.cancel.is_cancelled());
                match result {
                    Ok(_) => {
                        self.runtime_task = None;
                        self.settings.backend = Some(backend);
                        self.save_settings();
                    }
                    Err(_) if cancelled => {
                        self.runtime_task = None;
                        self.notices.push(
                            "Runtime download cancelled. Downloading again continues where it stopped."
                                .into(),
                        );
                    }
                    Err(e) => {
                        if let Some(t) = &mut self.runtime_task {
                            t.state = TaskState::Failed(e);
                        }
                    }
                }
                self.refresh();
            }
            InferenceEvent::ModelProgress { key, file, progress } => {
                if let Some(t) = self.model_tasks.iter_mut().find(|t| t.record.key == key) {
                    t.state = TaskState::Running {
                        file,
                        progress: Some(progress),
                    };
                }
            }
            InferenceEvent::ModelDone { key, result } => {
                let cancelled = self.model_task(&key).is_some_and(|t| t.cancel.is_cancelled());
                match result {
                    Err(_) if cancelled => {
                        self.model_tasks.retain(|t| t.record.key != key);
                        self.notices.push(
                            "Model download cancelled. Downloading it again continues where it stopped."
                                .into(),
                        );
                    }
                    Ok(record) => {
                        self.model_tasks.retain(|t| t.record.key != key);
                        self.verified.retain(|(k, _)| *k != key);
                        if self.active_model().is_none() {
                            self.use_model(&record.key);
                        }
                    }
                    Err(e) => {
                        if let Some(t) = self.model_tasks.iter_mut().find(|t| t.record.key == key) {
                            t.state = TaskState::Failed(e);
                        }
                    }
                }
                self.start_next_model();
                self.refresh();
            }
            InferenceEvent::Verified { key, problems } => {
                self.verifying.retain(|k| *k != key);
                self.verified.retain(|(k, _)| *k != key);
                if problems.is_empty() {
                    self.notices
                        .push("All files match their recorded checksums.".into());
                }
                self.verified.push((key, problems));
            }
            InferenceEvent::Resolved { request, result } => {
                if request == self.lookup.request {
                    self.lookup.busy = false;
                    if let Ok(info) = &result {
                        self.lookup.options = gguf_options(info);
                        self.lookup.inspections = vec![None; self.lookup.options.len()];
                        self.lookup.inspecting = vec![false; self.lookup.options.len()];
                    }
                    self.lookup.result = Some(result);
                }
            }
            InferenceEvent::Inspected {
                request,
                index,
                result,
            } => {
                if request == self.lookup.request
                    && let (Some(slot), Some(flag)) = (
                        self.lookup.inspections.get_mut(index),
                        self.lookup.inspecting.get_mut(index),
                    )
                {
                    *slot = Some(result);
                    *flag = false;
                }
            }
            InferenceEvent::Credentials { store, token } => {
                self.store = store;
                match token {
                    // A token entered while the store was opening takes precedence.
                    _ if self.token.is_some() => {}
                    Ok(Some(token)) => {
                        self.secrets.register(token.expose());
                        self.token = Some(token);
                        self.token_state = TokenState::Saved;
                    }
                    Ok(None) => self.token_state = TokenState::NotSaved,
                    Err(e) => {
                        self.token_state = TokenState::NotSaved;
                        self.token_problem = Some(format!("The saved token could not be read: {e}"));
                    }
                }
            }
            InferenceEvent::TokenSaved(result) => match result {
                Ok(()) => self.token_state = TokenState::Saved,
                Err(e) => {
                    if let Some(secret) = self.token.clone() {
                        self.session_only(&secret, &e);
                    }
                }
            },
            InferenceEvent::TokenRemoved(result) => {
                self.token_state = TokenState::NotSaved;
                if let Err(e) = result {
                    self.token_problem = Some(format!("The saved token could not be removed: {e}"));
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use clojurecraft_hardware::{CpuInfo, DetectionSource, GIB, GpuDevice, GpuVendor};

    use super::*;

    pub(crate) fn state() -> InferenceState {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut s = InferenceState {
            tx,
            ctx: egui::Context::default(),
            secrets: Arc::new(SecretRegistry::new()),
            dirs: None,
            manifest: Manifest::bundled().ok(),
            catalog: Catalog::bundled().ok(),
            platform: "linux-x86_64".into(),
            settings: InferenceSettings::default(),
            settings_problem: None,
            detected: None,
            snapshot: None,
            runtime_task: None,
            model_tasks: VecDeque::new(),
            lookup: CustomLookup::default(),
            verifying: Vec::new(),
            verified: Vec::new(),
            store: Err(String::new()),
            session_store: Arc::new(MemoryStore::default()),
            token: None,
            token_state: TokenState::NotSaved,
            token_problem: None,
            notices: Vec::new(),
            model: ModelRuntime::default(),
        };
        s.detected = Some(HardwareReport {
            os: "linux".into(),
            arch: "x86_64".into(),
            total_ram_bytes: Some(32 * GIB),
            available_ram_bytes: Some(30 * GIB),
            cpu: CpuInfo::default(),
            gpus: vec![GpuDevice {
                vendor: GpuVendor::Nvidia,
                name: "RTX".into(),
                vram_total_bytes: Some(8 * GIB),
                vram_used_bytes: None,
                unified_memory: false,
                driver_version: None,
                source: DetectionSource::NvidiaSmi,
            }],
            notes: vec![],
        });
        s
    }

    fn record(key: &str) -> ModelRecord {
        let catalog = Catalog::bundled().expect("catalog");
        let m = &catalog.models[0];
        let mut r = ModelRecord::from_catalog(m, &m.quants[0]);
        r.key = key.into();
        r
    }

    #[test]
    fn backend_follows_hardware_unless_chosen() {
        let mut s = state();
        assert_eq!(s.recommended_backend(), Backend::Cuda);
        assert_eq!(s.offered_backends()[0], Backend::Cuda);
        assert!(s.offered_backends().contains(&Backend::Cpu));
        s.settings.backend = Some(Backend::Vulkan);
        assert_eq!(s.backend(), Backend::Vulkan);
        // Without a GPU the recommendation falls back to the CPU.
        s.settings.backend = None;
        if let Some(d) = &mut s.detected {
            d.gpus.clear();
        }
        assert_eq!(s.backend(), Backend::Cpu);
    }

    #[test]
    fn missing_pieces_are_reported_until_installed() {
        let mut s = state();
        assert!(s.missing().is_empty(), "nothing is claimed before the first scan");
        s.snapshot = Some(Snapshot::default());
        assert_eq!(s.missing(), vec![Missing::Runtime, Missing::Model]);
        s.snapshot = Some(Snapshot {
            builds: vec![LlamaBuild {
                version: "b1-cuda".into(),
                release: "b1".into(),
                backend: Some(Backend::Cuda),
                active: true,
                pinned: true,
            }],
            models: vec![record("m/Q4")],
            problems: vec![("m/Q4".into(), vec![FileProblem::Missing("x.gguf".into())])],
            ..Snapshot::default()
        });
        s.settings.active_model = Some("m/Q4".into());
        assert!(matches!(s.missing().as_slice(), [Missing::ActiveModelDamaged(_)]));
    }

    #[test]
    fn model_downloads_queue_one_at_a_time() {
        let mut s = state();
        s.model_tasks.push_back(ModelTask {
            record: record("a/Q4"),
            state: TaskState::Running {
                file: String::new(),
                progress: None,
            },
            cancel: Cancel::default(),
        });
        s.install_model(record("b/Q4"));
        s.install_model(record("b/Q4"));
        assert_eq!(s.model_tasks.len(), 2, "a model is queued once");
        assert_eq!(s.model_task("b/Q4").map(|t| &t.state), Some(&TaskState::Queued));
        s.cancel_model("b/Q4");
        assert!(
            s.model_task("b/Q4").is_none(),
            "a queued download is simply dropped"
        );
        s.cancel_model("a/Q4");
        assert!(s.model_task("a/Q4").expect("running").cancel.is_cancelled());
    }

    #[test]
    fn saving_a_token_registers_it_as_a_secret() {
        let mut s = state();
        s.save_token("  hf_abcdefghijklmnopqrstuvwxyz0123  ");
        assert!(s.token_present());
        assert_eq!(
            s.token_state,
            TokenState::SessionOnly,
            "no store in tests: kept for the session"
        );
        assert!(s.secrets.contains_secret("x hf_abcdefghijklmnopqrstuvwxyz0123 y"));
        assert!(!format!("{:?}", s.hub()).contains("hf_abcdef"));
        s.remove_token();
        assert!(!s.token_present());
    }

    #[test]
    fn stale_lookups_are_ignored() {
        let mut s = state();
        s.lookup.request = 2;
        s.handle(InferenceEvent::Resolved {
            request: 1,
            result: Err("old".into()),
        });
        assert!(s.lookup.result.is_none());
    }
}
