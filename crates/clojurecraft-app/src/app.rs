//! The application shell: function library on the left, specification in the centre, and
//! implementation and results on the right.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use clojurecraft_clojure::EvalRequest;
use clojurecraft_domain::{
    Basis, FunctionId, InferenceRuntime, ModelIdentity, RevisionReason, SecretRegistry, ValidationReport,
};
use clojurecraft_inference::{GenerateRequest, RequestId};
use clojurecraft_store::{Autosaver, FunctionSummary};
use egui::{Id, Key, RichText, Ui};

use crate::dataset_view::{self, DatasetAction, DatasetPage};
use crate::editor::{self, Action, AssistStatus, EditorContext, ExpandTarget, Focus, Undo};
use crate::edn_view::EdnCache;
use crate::execution::ExecutionState;
use crate::generation_view::PanelAction;
use crate::implementation::{self, ColumnContext, Tab, ValidationContext};
use crate::inference::InferenceState;
use crate::jobs::{Event, Jobs};
use crate::keys::{self, DUPLICATE, FIND, NEW_FUNCTION, RENAME, SAVE};
use crate::knowledge::KnowledgeState;
use crate::knowledge_view;
use crate::library::{Command, Library, LibraryEvent, VALIDATIONS_SHOWN};
use crate::model::RemedyAction;
use crate::model_view;
use crate::models_view::ModelsPage;
use crate::project::Project;
use crate::project_view::{self, DialogAction, ProjectDialog, Selection, TreeAction};
use crate::references::{ReferencesState, Request};
use crate::session::{self, OpenDocs, OpenDocument};
use crate::setup_view::{self, SetupAction, SetupPage};
use crate::step_inspection::StepInspections;
use crate::step_preview_view::{StepAction, StepsContext};
use crate::theme;
use crate::views::{self, SettingsAction, TrashAction, WelcomeAction};
use clojurecraft_runtime::settings::SetupState;
use clojurecraft_workflow::Names;
use clojurecraft_workflow::assistant::{Assistant, Gather, JobKind, ModelContext, Ports, Update};
use clojurecraft_workflow::validation::{self, Plan, ReportContext};

use crate::review::ReviewState;

mod mcp_actions;
mod review_actions;

/// Toasts stay this long after the pointer or keyboard focus last left them.
const TOAST_LIFETIME: Duration = Duration::from_secs(12);
/// The centre column never gets narrower than this; side columns give way first.
const MIN_CENTER_WIDTH: f32 = 440.0;
const STORAGE_LAST_FUNCTION: &str = "last-function";
const STORAGE_LAST_PROJECT: &str = "last-project";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Welcome,
    Function(FunctionId),
    Trash,
    Dataset,
    Knowledge,
    Settings,
    Setup,
}

enum LibraryState {
    Opening,
    Ready,
    Unavailable(String),
}

struct Toast {
    message: String,
    undo: Option<Undo>,
    error: bool,
    shown_at: Instant,
}

/// A check of a function's current code that the user started.
struct ManualCheck {
    run: u64,
    plan: Plan,
    /// The function's content when the check started; the report is about exactly this.
    basis: Basis,
}

enum Dialog {
    Purge(FunctionSummary),
    Recover(FunctionId),
    QuitWithUnsaved,
}

pub struct ClojureCraftApp {
    jobs: Jobs,
    library: Library,
    autosaver: Option<Autosaver>,
    library_state: LibraryState,
    functions: Vec<FunctionSummary>,
    trash: Vec<FunctionSummary>,
    search: String,
    view: View,
    /// Function to open once the library is listed (restored from the previous session).
    reopen: Option<FunctionId>,
    docs: HashMap<FunctionId, OpenDocument>,
    loading: HashSet<FunctionId>,
    open_errors: HashMap<FunctionId, (String, bool)>,
    inference: InferenceState,
    knowledge: KnowledgeState,
    assistant: Assistant,
    execution: ExecutionState,
    /// Stored validation reports of open functions, newest first.
    validations: HashMap<FunctionId, Vec<ValidationReport>>,
    checks: HashMap<FunctionId, ManualCheck>,
    /// Example inputs taken from other functions.
    references: ReferencesState,
    /// Executed intermediate values of transformation steps.
    step_inspections: StepInspections,
    /// Reports of finished checks, applied with the frame's other updates.
    pending_reports: Vec<(FunctionId, ValidationReport)>,
    setup_page: SetupPage,
    models_page: ModelsPage,
    dataset_page: DatasetPage,
    toasts: Vec<Toast>,
    dialog: Option<Dialog>,
    edn: EdnCache,
    focus: Option<Focus>,
    expanded: Option<ExpandTarget>,
    right_tab: Tab,
    allow_close: bool,
    review: ReviewState,
    /// Serves MCP clients while the application runs; `None` when the socket is unavailable.
    _mcp_server: Option<clojurecraft_mcp::Server>,
    mcp_requests: Option<std::sync::mpsc::Receiver<clojurecraft_mcp::Request>>,
    /// Tool calls waiting for the library or a function to open.
    mcp_waiting: Vec<mcp_actions::Parked>,
    /// Generations requested by MCP clients while the model was busy, run in order.
    mcp_queue: mcp_actions::Queue,
    /// Functions opened for an MCP client; opening them does not change the view.
    quiet_open: HashSet<FunctionId>,
    /// The open Clojure project; the sidebar shows its namespaces instead of the library.
    project: Option<Project>,
    /// What is selected in the project tree.
    project_selection: Selection,
    /// The project's classpath while its dependencies are being resolved.
    classpath: Option<Receiver<Result<Vec<PathBuf>, String>>>,
    project_dialog: Option<ProjectDialog>,
    /// The folder picker's answer, while it is showing.
    picker: Option<Receiver<Result<Option<PathBuf>, String>>>,
}

impl ClojureCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let jobs = Jobs::new();
        // Every credential the application handles is registered here so it can never be
        // written into function history or results.
        let secrets = Arc::new(SecretRegistry::new());
        let library = Library::start(jobs.sender(), cc.egui_ctx.clone(), secrets.clone());
        let references = ReferencesState::new(jobs.sender(), cc.egui_ctx.clone(), secrets.clone());
        let inference = InferenceState::new(jobs.sender(), cc.egui_ctx.clone(), secrets);
        let knowledge = KnowledgeState::new(jobs.sender(), cc.egui_ctx.clone());
        let mut execution = ExecutionState::new(jobs.sender(), cc.egui_ctx.clone());
        if !execution.installed() {
            execution.install();
        }
        let first_run = inference.settings.setup == SetupState::NotStarted;
        let (mcp_server, mcp_requests) = start_mcp(&cc.egui_ctx);
        let reopen = cc
            .storage
            .and_then(|s| eframe::get_value::<Option<FunctionId>>(s, STORAGE_LAST_FUNCTION))
            .flatten()
            .filter(|_| !first_run);
        let last_project = cc
            .storage
            .and_then(|s| eframe::get_value::<Option<PathBuf>>(s, STORAGE_LAST_PROJECT))
            .flatten()
            .filter(|_| !first_run);
        let mut app = Self {
            jobs,
            library,
            autosaver: None,
            library_state: LibraryState::Opening,
            functions: Vec::new(),
            trash: Vec::new(),
            search: String::new(),
            view: if first_run { View::Setup } else { View::Welcome },
            reopen,
            docs: HashMap::new(),
            loading: HashSet::new(),
            open_errors: HashMap::new(),
            inference,
            knowledge,
            assistant: Assistant::default(),
            execution,
            validations: HashMap::new(),
            checks: HashMap::new(),
            references,
            step_inspections: StepInspections::default(),
            pending_reports: Vec::new(),
            setup_page: SetupPage::default(),
            models_page: ModelsPage::default(),
            dataset_page: DatasetPage::default(),
            toasts: Vec::new(),
            dialog: None,
            edn: EdnCache::default(),
            focus: None,
            expanded: None,
            right_tab: Tab::default(),
            allow_close: false,
            review: ReviewState::default(),
            _mcp_server: mcp_server,
            mcp_requests,
            mcp_waiting: Vec::new(),
            mcp_queue: mcp_actions::Queue::default(),
            quiet_open: HashSet::new(),
            project: None,
            project_selection: Selection::default(),
            classpath: None,
            project_dialog: None,
            picker: None,
        };
        if let Some(path) = last_project
            && let Err(e) = app.open_project(&path, &cc.egui_ctx)
        {
            app.toast(format!("The last project could not be opened: {e}"), None);
        }
        app
    }

    /// Whether a documentation build may compute embeddings on the GPU: only with a GPU backend
    /// and while no generator model holds GPU memory.
    fn gpu_free(&self) -> bool {
        use clojurecraft_inference::ModelStatus;
        self.inference.backend().uses_gpu()
            && matches!(
                self.inference.model.status,
                ModelStatus::Unloaded | ModelStatus::Failed { .. }
            )
    }

    // ---- events ---------------------------------------------------------------------------

    fn handle_events(&mut self) {
        for event in self.jobs.drain() {
            match event {
                Event::Library(event) => self.library_event(event),
                Event::Inference(event) => self.inference.handle(*event),
                Event::Knowledge(event) => self.knowledge.handle(*event),
                Event::Execution(event) => self.execution.handle(*event),
                Event::References(event) => {
                    if let Some((function, example, plan)) = self.references.handle(*event) {
                        self.references
                            .start(&mut self.execution, function, example, plan);
                    }
                }
            }
        }
        // Responses for the assistant, routed after the services updated their own state.
        let unclaimed = std::mem::take(&mut self.inference.model.unclaimed);
        let gathered = std::mem::take(&mut self.knowledge.gathered);
        let mut executed = std::mem::take(&mut self.execution.finished);
        // Checks the user started are completed here; the rest belong to the assistant.
        executed.retain(|(run, execution)| {
            let Some(function) = self.checks.iter().find(|(_, c)| c.run == *run).map(|(f, _)| *f) else {
                return true;
            };
            if let Some(check) = self.checks.remove(&function) {
                let report = validation::report(
                    &check.plan,
                    ReportContext {
                        basis: check.basis,
                        environment: execution.environment.clone(),
                        attempt: None,
                        run: None,
                        outcome: execution.outcome.as_ref().map_err(String::as_str),
                    },
                );
                self.pending_reports.push((function, report));
            }
            false
        });
        // Upstream runs and step inspections are not the assistant's either.
        for function in self.references.claim(&mut executed, &mut self.docs) {
            self.submit(function, false);
        }
        self.step_inspections.claim(&mut executed);
        for notice in std::mem::take(&mut self.references.notices) {
            self.toast(notice, None);
        }
        let mut updates: Vec<Update> = std::mem::take(&mut self.pending_reports)
            .into_iter()
            .map(|(f, r)| Update::Validated(f, Box::new(r)))
            .collect();
        {
            let mut ports = AppPorts {
                inference: &mut self.inference,
                knowledge: &mut self.knowledge,
                execution: &mut self.execution,
            };
            for event in &unclaimed {
                updates.extend(
                    self.assistant
                        .on_model(event, &mut OpenDocs(&mut self.docs), &mut ports),
                );
            }
            for (request, result) in gathered {
                self.assistant.on_gathered(request, result, &mut ports);
            }
            for (run, execution) in executed {
                updates.extend(self.assistant.on_validated(
                    run,
                    execution,
                    &mut OpenDocs(&mut self.docs),
                    &mut ports,
                ));
            }
        }
        self.apply_updates(updates);
        for notice in std::mem::take(&mut self.inference.notices) {
            self.toast(notice, None);
        }
        self.handle_mcp();
    }

    // ---- assistant --------------------------------------------------------------------------

    fn apply_updates(&mut self, updates: Vec<Update>) {
        for u in updates {
            match u {
                Update::Edited(id) => self.submit(id, false),
                Update::Commit(id, reason) => self.commit(id, reason),
                Update::Validated(id, report) => {
                    let list = self.validations.entry(id).or_default();
                    list.insert(0, (*report).clone());
                    list.truncate(VALIDATIONS_SHOWN);
                    self.library.send(Command::RecordValidation(id, report));
                }
            }
        }
    }

    /// Checks the current code of `id` in the sandbox.
    fn run_checks(&mut self, id: FunctionId) {
        if let Err(e) = self.try_run_checks(id) {
            self.toast(e, None);
        }
    }

    /// Starts checking the current code of `id`, or says why it cannot be checked. A check
    /// already running counts as started.
    fn try_run_checks(&mut self, id: FunctionId) -> Result<(), String> {
        if self.checks.contains_key(&id) {
            return Ok(());
        }
        let Some(doc) = self.docs.get(&id).map(|o| o.doc.clone()) else {
            return Err("The function is not open.".into());
        };
        let Some(source) = doc.implementation.as_ref().map(|i| i.source.clone()) else {
            return Err("The function has no code yet.".into());
        };
        let names = match doc.implementation.as_ref().and_then(|i| i.generation.as_ref()) {
            Some(g) if !g.namespace.is_empty() && !g.function_name.is_empty() => Names {
                namespace: g.namespace.clone(),
                function: g.function_name.clone(),
                arguments: doc.arguments(),
            },
            _ => Names::for_document(&doc, |_| false),
        };
        let plan = validation::plan(&doc, &source, &names);
        let basis = doc.basis();
        let Some(request) = plan.request.clone() else {
            // Parse or structure problems: complete without running anything.
            let report = validation::report(
                &plan,
                ReportContext {
                    basis,
                    environment: self.execution.environment(),
                    attempt: None,
                    run: None,
                    outcome: Err("not run"),
                },
            );
            self.apply_updates(vec![Update::Validated(id, Box::new(report))]);
            return Ok(());
        };
        let run = self.execution.start(request)?;
        self.checks.insert(id, ManualCheck { run, plan, basis });
        Ok(())
    }

    fn cancel_checks(&mut self, id: FunctionId) {
        if let Some(check) = self.checks.remove(&id) {
            self.execution.cancel(check.run);
        }
    }

    /// Saves the draft and records it as a revision, in order with pending autosaves.
    fn commit(&mut self, id: FunctionId, reason: RevisionReason) {
        // The project's file first: recording updates the document's binding.
        self.record_project(id);
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        let seq = open.edited();
        if let Some(autosaver) = &self.autosaver {
            autosaver.commit(open.doc.clone(), seq, reason);
        }
    }

    /// The loaded (or now loading) model, as recorded with generated code.
    fn model_context(&mut self) -> Result<ModelContext, String> {
        let (spec, key) = self.inference.ensure_model()?;
        let record = self
            .inference
            .installed(&key)
            .ok_or("The selected model is not installed.")?
            .clone();
        let file = record.files.first().ok_or("The selected model has no files.")?;
        let release = self
            .inference
            .manifest
            .as_ref()
            .map(|m| m.llama_cpp.release.clone())
            .unwrap_or_default();
        let clojure_version = self
            .knowledge
            .runtime_clojure()
            .map(str::to_owned)
            .or_else(|| {
                self.inference
                    .manifest
                    .as_ref()
                    .map(|m| m.clojure.version.clone())
            })
            .unwrap_or_default();
        Ok(ModelContext {
            label: spec.label.clone(),
            model: ModelIdentity {
                display_name: record.display_name.clone(),
                repository: record.repo.clone(),
                revision: record.revision.clone(),
                file: file.repo_path.clone(),
                sha256: file.sha256.clone(),
                quantization: record.quant.clone(),
                architecture: record.architecture.clone(),
            },
            runtime: InferenceRuntime {
                llama_cpp_release: release,
                backend: spec.backend.to_lowercase(),
            },
            context_length: spec.context,
            clojure_version,
        })
    }

    /// Why no model work can start for `id` right now.
    fn assist_blocked(&self, id: FunctionId) -> Option<String> {
        if let Some(m) = self.inference.missing().first() {
            return Some(m.describe());
        }
        let job = self.assistant.job()?;
        if job.function == id {
            return Some(format!(
                "The model is busy: {}.",
                job.phase.label().to_lowercase()
            ));
        }
        let name = self
            .docs
            .get(&job.function)
            .map_or("another function", |o| views::display_name(&o.doc.name));
        Some(format!("The model is busy with “{name}”."))
    }

    fn start_job(&mut self, id: FunctionId, kind: JobKind) {
        if let Err(e) = self.try_start_job(id, kind) {
            self.toast(e, None);
        }
    }

    /// Starts a model job, or says why it cannot start. The assistant may still end it at once
    /// (an incomplete specification); its report says so.
    fn try_start_job(&mut self, id: FunctionId, kind: JobKind) -> Result<(), String> {
        let Some(doc) = self.docs.get(&id).map(|o| o.doc.clone()) else {
            return Err("The function is not open.".into());
        };
        if let Some(why) = self.assist_blocked(id) {
            return Err(why);
        }
        let model = match kind {
            JobKind::Generate => Some(self.model_context()?),
            _ => None,
        };
        let documentation = self.knowledge.active().is_some();
        let mut ports = AppPorts {
            inference: &mut self.inference,
            knowledge: &mut self.knowledge,
            execution: &mut self.execution,
        };
        match (kind, model) {
            (JobKind::Interpret, _) => self.assistant.interpret(&doc, &mut ports),
            (JobKind::ProposeContract, _) => self.assistant.propose_contract(&doc, &mut ports),
            (JobKind::Generate, Some(model)) => {
                self.assistant.generate(&doc, model, documentation, &mut ports)
            }
            (JobKind::Generate, None) => {}
            (JobKind::ProposeTests, _) => self.assistant.propose_tests(&doc, &mut ports),
            (JobKind::Explain, _) => self.assistant.explain(&doc, &mut ports),
        }
        Ok(())
    }

    /// Generates edge cases from the title, description, and examples only.
    fn generate_cases(&mut self, id: FunctionId) {
        if let Err(e) = self.try_generate_cases(id) {
            self.toast(e, None);
        }
    }

    fn try_generate_cases(&mut self, id: FunctionId) -> Result<(), String> {
        let Some(doc) = self.docs.get(&id).map(|o| o.doc.clone()) else {
            return Err("The function is not open.".into());
        };
        if let Some(why) = self.assist_blocked(id) {
            return Err(why);
        }
        let mut ports = AppPorts {
            inference: &mut self.inference,
            knowledge: &mut self.knowledge,
            execution: &mut self.execution,
        };
        self.assistant.generate_tests_from_examples(&doc, &mut ports);
        Ok(())
    }

    fn cancel_job(&mut self) {
        let mut ports = AppPorts {
            inference: &mut self.inference,
            knowledge: &mut self.knowledge,
            execution: &mut self.execution,
        };
        self.assistant.cancel(&mut ports);
    }

    fn panel_action(&mut self, id: FunctionId, action: PanelAction) -> RemedyAction {
        match action {
            PanelAction::Generate => self.start_job(id, JobKind::Generate),
            PanelAction::Explain => self.start_job(id, JobKind::Explain),
            PanelAction::Cancel => self.cancel_job(),
            PanelAction::AcceptPending => {
                let result = match self.docs.get_mut(&id) {
                    Some(open) => self.assistant.accept_pending(&mut open.doc),
                    None => Ok(Vec::new()),
                };
                match result {
                    Ok(updates) => self.apply_updates(updates),
                    Err(e) => self.toast(format!("The code could not be used: {e}."), None),
                }
            }
            PanelAction::DiscardPending => self.assistant.discard_pending(id),
            PanelAction::Dismiss => self.assistant.dismiss_report(id),
            PanelAction::Remedy(r) => return self.inference.apply_remedy(&r),
            PanelAction::RunChecks => self.run_checks(id),
            PanelAction::CancelChecks => self.cancel_checks(id),
            PanelAction::InstallRuntime => self.execution.install(),
            PanelAction::CancelInstall => self.execution.cancel_install(),
        }
        RemedyAction::None
    }

    fn load_dataset(&mut self) {
        self.dataset_page.loading = true;
        self.library.send(Command::LoadDataset);
    }

    fn library_event(&mut self, event: LibraryEvent) {
        match event {
            LibraryEvent::Dataset(rows) => {
                self.dataset_page.loading = false;
                self.dataset_page.rows = Some(rows);
            }
            LibraryEvent::Ready(autosaver) => {
                self.autosaver = Some(autosaver);
                self.library_state = LibraryState::Ready;
            }
            LibraryEvent::Unavailable(e) => self.library_state = LibraryState::Unavailable(e),
            LibraryEvent::Listed { active, trash } => {
                self.references.library_changed();
                self.functions = active;
                self.trash = trash;
                // Keep names and descriptions typed since the listing was read.
                for f in &mut self.functions {
                    if let Some(open) = self.docs.get(&f.id) {
                        f.name.clone_from(&open.doc.name);
                        f.description.clone_from(&open.doc.description);
                    }
                }
                if let Some(id) = self.reopen.take()
                    && self.functions.iter().any(|f| f.id == id)
                {
                    self.open(id);
                }
            }
            LibraryEvent::Opened { draft, created } => {
                let id = draft.document.id;
                self.loading.remove(&id);
                self.open_errors.remove(&id);
                // The in-memory copy is never older than the stored draft.
                if !self.docs.contains_key(&id) {
                    self.library.send(Command::LoadValidations(id));
                    self.library.send(Command::LoadRevisions(id));
                }
                self.docs.entry(id).or_insert_with(|| OpenDocument::new(*draft));
                self.sync_project_document(id);
                if self.quiet_open.remove(&id) {
                    return;
                }
                if let Some((ns, f)) = self.project.as_ref().and_then(|p| p.locate(id)) {
                    self.project_selection = Selection {
                        path: Some(ns.to_owned()),
                        function: Some(f.to_owned()),
                    };
                }
                self.view = View::Function(id);
                if created {
                    self.focus = Some(Focus::Name);
                }
            }
            LibraryEvent::OpenFailed {
                id,
                error,
                recoverable,
            } => {
                self.loading.remove(&id);
                self.quiet_open.remove(&id);
                self.open_errors.insert(id, (error, recoverable));
            }
            LibraryEvent::Trashed { id, name } => {
                if self.view == View::Function(id) {
                    self.view = View::Welcome;
                }
                self.toast(
                    format!("Moved “{}” to the trash", views::display_name(&name)),
                    Some(Undo::Function(id)),
                );
            }
            LibraryEvent::Validations { function, reports } => {
                // Reports produced since the request was sent are newer than any stored ones.
                let list = self.validations.entry(function).or_default();
                for r in reports {
                    if !list.iter().any(|x| x.id == r.id) {
                        list.push(r);
                    }
                }
                list.sort_by_key(|r| std::cmp::Reverse(r.created_at));
                list.truncate(VALIDATIONS_SHOWN);
            }
            LibraryEvent::Purged(id) => {
                self.docs.remove(&id);
                self.validations.remove(&id);
                self.toast("Deleted permanently".into(), None);
            }
            LibraryEvent::Failed { action, error } => {
                self.dataset_page.loading = false;
                self.toasts.push(Toast {
                    message: format!("Could not {action}: {error}"),
                    undo: None,
                    error: true,
                    shown_at: Instant::now(),
                });
            }
            LibraryEvent::Autosave(event) => {
                let id = event.function();
                if let clojurecraft_store::AutosaveEvent::CommitFailed { error, .. } = &event {
                    self.toasts.push(Toast {
                        message: format!(
                            "The revision could not be recorded: {error}. The draft is still saved."
                        ),
                        undo: None,
                        error: true,
                        shown_at: Instant::now(),
                    });
                }
                if matches!(event, clojurecraft_store::AutosaveEvent::Committed { .. }) {
                    // A new revision may be newer than outputs other functions recorded.
                    self.references.library_changed();
                }
                if let Some(open) = self.docs.get_mut(&id) {
                    open.apply(&event);
                }
                if matches!(event, clojurecraft_store::AutosaveEvent::Committed { .. }) {
                    self.library.send(Command::LoadRevisions(id));
                }
            }
            LibraryEvent::Revisions { function, revisions } => {
                self.review.revisions.insert(function, revisions);
            }
            LibraryEvent::Revision(revision) => self.revision_loaded(*revision),
        }
    }

    fn toast(&mut self, message: String, undo: Option<Undo>) {
        self.toasts.push(Toast {
            message,
            undo,
            error: false,
            shown_at: Instant::now(),
        });
    }

    // ---- library operations ---------------------------------------------------------------

    fn ready(&self) -> bool {
        matches!(self.library_state, LibraryState::Ready)
    }

    fn open(&mut self, id: FunctionId) {
        self.view = View::Function(id);
        self.expanded = None;
        if !self.docs.contains_key(&id) && self.loading.insert(id) {
            self.open_errors.remove(&id);
            self.library.send(Command::Open(id));
        }
    }

    fn create(&mut self) {
        if !self.ready() {
            return;
        }
        if let Some(project) = &self.project {
            let path = self.project_selection.path.clone();
            self.project_dialog = Some(match path {
                Some(ns) if project.contains(&ns) => ProjectDialog::new_function(ns),
                other => ProjectDialog::new_namespace(other.as_deref()),
            });
            return;
        }
        let name = session::unique_name(
            "Untitled function",
            self.functions.iter().map(|f| f.name.as_str()),
        );
        self.library.send(Command::Create(name));
    }

    fn duplicate(&mut self, id: FunctionId) {
        let Some(open) = self.docs.get(&id) else {
            // Never opened in this session, so the stored draft is the newest version.
            let name = self
                .functions
                .iter()
                .find(|f| f.id == id)
                .map(|f| session::copy_name(&f.name, self.functions.iter().map(|f| f.name.as_str())))
                .unwrap_or_default();
            self.library.send(Command::Duplicate { id, name });
            return;
        };
        let mut copy = open.doc.clone();
        copy.id = FunctionId::generate();
        // A copy is a library function, not a second definition of the project namespace.
        copy.project = None;
        copy.name = session::copy_name(&open.doc.name, self.functions.iter().map(|f| f.name.as_str()));
        self.library.send(Command::Insert(Box::new(copy)));
    }

    fn trash_function(&mut self, id: FunctionId) {
        let name = self
            .docs
            .get(&id)
            .map(|o| o.doc.name.clone())
            .or_else(|| self.functions.iter().find(|f| f.id == id).map(|f| f.name.clone()))
            .unwrap_or_default();
        self.library.send(Command::Trash { id, name });
    }

    /// Hands the document to the autosaver under a new edit sequence.
    fn submit(&mut self, id: FunctionId, retry: bool) {
        // The project's file first: recording updates the document's binding.
        self.record_project(id);
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        let seq = if retry { open.retry() } else { open.edited() };
        if let Some(autosaver) = &self.autosaver {
            autosaver.submit(open.doc.clone(), seq);
        }
        if let Some(summary) = self.functions.iter_mut().find(|f| f.id == id) {
            summary.name.clone_from(&open.doc.name);
            summary.description.clone_from(&open.doc.description);
        }
    }

    fn undo(&mut self, undo: Undo) {
        match undo {
            Undo::Function(id) => {
                self.library.send(Command::Restore(id));
                self.open(id);
            }
            Undo::Step {
                function,
                index,
                step,
            } => {
                let step_id = step.id;
                if let Some(open) = self.docs.get_mut(&function)
                    && open.doc.insert_step(index, step)
                {
                    self.focus = Some(Focus::Step(step_id));
                    self.submit(function, false);
                }
            }
            Undo::Example {
                function,
                index,
                example,
            } => {
                if let Some(open) = self.docs.get_mut(&function) {
                    let example_id = example.id;
                    if open.doc.insert_example(index, *example) {
                        open.view.selected_example = Some(example_id);
                        self.submit(function, false);
                    }
                }
            }
            Undo::Contract { function, contract } => {
                if let Some(open) = self.docs.get_mut(&function)
                    && open.doc.contract.is_none()
                {
                    open.doc.contract = Some(*contract);
                    self.submit(function, false);
                }
            }
        }
    }

    fn current_function(&self) -> Option<FunctionId> {
        match self.view {
            View::Function(id) if self.docs.contains_key(&id) => Some(id),
            _ => None,
        }
    }

    // ---- project --------------------------------------------------------------------------

    /// Opens the project at `path`, replacing (and saving) the one open before, and starts
    /// resolving its dependencies.
    fn open_project(&mut self, path: &std::path::Path, ctx: &egui::Context) -> Result<(), String> {
        let project = Project::open(path)?;
        let (root, sources) = (project.root().to_owned(), project.source_dirs());
        self.execution.project_classpath.clone_from(&sources);
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::project::resolve_classpath(&root, &sources));
            ctx.request_repaint();
        });
        self.classpath = Some(rx);
        self.project = Some(project);
        self.project_selection = Selection::default();
        self.view = View::Welcome;
        Ok(())
    }

    fn close_project(&mut self) {
        self.project = None;
        self.project_selection = Selection::default();
        self.classpath = None;
        self.execution.project_classpath.clear();
        if matches!(self.view, View::Function(_)) {
            self.view = View::Welcome;
        }
    }

    fn poll_classpath(&mut self) {
        let Some(rx) = &self.classpath else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.classpath = None;
        match result {
            Ok(classpath) => self.execution.project_classpath = classpath,
            Err(e) => self.toast_error(format!(
                "The project's dependencies could not be resolved ({e}); checks only see its own sources."
            )),
        }
    }

    /// Reads the project again, picking up namespaces and code changed outside the application.
    fn reload_project(&mut self, ctx: &egui::Context) {
        let Some(root) = self.project.as_ref().map(|p| p.root().to_owned()) else {
            return;
        };
        let selection = self.project_selection.clone();
        let view = self.view;
        // Dropping the project writes its pending specification edits first.
        self.project = None;
        match self.open_project(&root, ctx) {
            Ok(()) => {
                self.project_selection = selection;
                self.view = view;
                let open: Vec<FunctionId> = self.docs.keys().copied().collect();
                for id in open {
                    self.sync_project_document(id);
                }
            }
            Err(e) => self.toast_error(format!("The project could not be reloaded: {e}")),
        }
    }

    fn open_function(&mut self, namespace: &str, function: &str) {
        let Some(project) = &self.project else {
            return;
        };
        let Some(id) = project.id_of(namespace, function) else {
            return;
        };
        let document = (!self.docs.contains_key(&id))
            .then(|| project.document(namespace, function))
            .flatten();
        self.project_selection = Selection {
            path: Some(namespace.to_owned()),
            function: Some(function.to_owned()),
        };
        self.view = View::Function(id);
        self.expanded = None;
        match document {
            None => self.sync_project_document(id),
            Some(doc) => {
                if self.loading.insert(id) {
                    self.open_errors.remove(&id);
                    self.library.send(Command::OpenOrInsert(Box::new(doc)));
                }
            }
        }
    }

    fn create_namespace(&mut self, namespace: &str) -> Result<(), String> {
        let project = self.project.as_mut().ok_or("No project is open.")?;
        project.add_namespace(namespace)?;
        self.project_selection = Selection {
            path: Some(namespace.trim().to_owned()),
            function: None,
        };
        Ok(())
    }

    fn create_function(&mut self, namespace: &str, name: &str, arguments: u32) -> Result<(), String> {
        let project = self.project.as_mut().ok_or("No project is open.")?;
        project.add_function(namespace, name, arguments)?;
        self.open_function(namespace, name.trim());
        Ok(())
    }

    /// Takes the project's specification and code on disk into an open document of the project.
    fn sync_project_document(&mut self, id: FunctionId) {
        let (Some(project), Some(open)) = (self.project.as_mut(), self.docs.get_mut(&id)) else {
            return;
        };
        match project.sync(&mut open.doc) {
            Ok(true) => self.submit(id, false),
            Ok(false) => {}
            Err(e) => self.toast_error(e),
        }
    }

    /// Writes a project function's specification and code to the project, then gives the other
    /// open functions of its namespace the file as written.
    fn record_project(&mut self, id: FunctionId) {
        let (Some(project), Some(open)) = (self.project.as_mut(), self.docs.get_mut(&id)) else {
            return;
        };
        let before = open.doc.project.as_ref().map(|p| p.file.clone());
        let result = project.record(&mut open.doc);
        let after = open
            .doc
            .project
            .as_ref()
            .map(|p| (p.namespace.clone(), p.file.clone()));
        let mut refreshed = Vec::new();
        if let Some((namespace, file)) = after
            && before.as_ref() != Some(&file)
        {
            for (other, o) in &mut self.docs {
                let same = o.doc.project.as_ref().is_some_and(|p| p.namespace == namespace);
                if *other != id && same && project.refresh_file(&mut o.doc) {
                    refreshed.push(*other);
                }
            }
        }
        if let Err(e) = result {
            self.toast_error(e);
        }
        for other in refreshed {
            self.submit(other, false);
        }
    }

    /// Shows an error unless the same one is already showing.
    fn toast_error(&mut self, message: String) {
        if let Some(t) = self.toasts.iter_mut().find(|t| t.message == message) {
            t.shown_at = Instant::now();
            return;
        }
        self.toasts.push(Toast {
            message,
            undo: None,
            error: true,
            shown_at: Instant::now(),
        });
    }

    fn save_project(&mut self, ctx: &egui::Context, force: bool) {
        let Some(project) = &mut self.project else {
            return;
        };
        match project.save_pending(force) {
            Ok(Some(wait)) => ctx.request_repaint_after(wait),
            Ok(None) => {}
            Err(e) => self.toast_error(e),
        }
    }

    fn poll_picker(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.picker else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Ok(None),
        };
        self.picker = None;
        let Some(ProjectDialog::Open { path, error, picking }) = &mut self.project_dialog else {
            return;
        };
        *picking = false;
        match result {
            Ok(Some(folder)) => {
                *path = folder.display().to_string();
                *error = None;
                match self.open_project(&folder, ctx) {
                    Ok(()) => self.project_dialog = None,
                    Err(e) => {
                        if let Some(d) = &mut self.project_dialog {
                            d.fail(e);
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => *error = Some(e),
        }
    }

    fn project_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.project_dialog else {
            return;
        };
        let result = match project_view::dialog(ctx, dialog) {
            DialogAction::None => Ok(()),
            DialogAction::Close => {
                self.project_dialog = None;
                self.picker = None;
                Ok(())
            }
            DialogAction::Browse => {
                if let ProjectDialog::Open { picking, .. } = dialog {
                    *picking = true;
                }
                self.picker = Some(project_view::pick_folder(ctx));
                Ok(())
            }
            DialogAction::Open(path) => self.open_project(&path, ctx).map(|()| self.project_dialog = None),
            DialogAction::CreateNamespace(namespace) => self
                .create_namespace(&namespace)
                .map(|()| self.project_dialog = None),
            DialogAction::CreateFunction {
                namespace,
                name,
                arguments,
            } => self
                .create_function(&namespace, &name, arguments)
                .map(|()| self.project_dialog = None),
        };
        if let (Err(e), Some(d)) = (result, &mut self.project_dialog) {
            d.fail(e);
        }
    }

    fn project_sidebar(&mut self, ui: &mut Ui) {
        let Some(project) = &self.project else {
            return;
        };
        let mut command = None;
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(project.title()).heading()).truncate())
                .on_hover_text(project.root().display().to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("More…", |ui| {
                    if ui.button("New namespace…").clicked() {
                        command = Some("namespace");
                    }
                    if ui.button("Reload from disk").clicked() {
                        command = Some("reload");
                    }
                    if ui.button("Open another project…").clicked() {
                        command = Some("open");
                    }
                    if ui.button("Close project").clicked() {
                        command = Some("close");
                    }
                });
                let in_namespace = self
                    .project_selection
                    .path
                    .as_deref()
                    .is_some_and(|p| project.contains(p));
                let r = ui
                    .add_enabled(self.ready(), egui::Button::new("+ New"))
                    .on_hover_text(format!(
                        "{} ({})",
                        if in_namespace {
                            "New function in the selected namespace"
                        } else {
                            "New namespace below the selection"
                        },
                        keys::label(&NEW_FUNCTION)
                    ));
                if r.clicked() {
                    command = Some("new");
                }
            });
        });
        let status = if self.classpath.is_some() {
            "Namespaces and functions · resolving dependencies…"
        } else {
            "Namespaces and functions"
        };
        ui.label(RichText::new(status).small().color(theme::MUTED));
        ui.add_space(6.0);
        let action = project_view::tree(ui, project, &self.project_selection);
        match action {
            Some(TreeAction::Open { namespace, function }) => self.open_function(&namespace, &function),
            Some(TreeAction::Select(path)) => {
                self.project_selection = Selection {
                    path: Some(path),
                    function: None,
                };
            }
            None => {}
        }
        match command {
            Some("namespace") => {
                self.project_dialog = Some(ProjectDialog::new_namespace(
                    self.project_selection.path.as_deref(),
                ));
            }
            Some("reload") => self.reload_project(&ui.ctx().clone()),
            Some("open") => self.project_dialog = Some(ProjectDialog::open(String::new())),
            Some("close") => self.close_project(),
            Some(_) => self.create(),
            None => {}
        }
    }

    // ---- input ----------------------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() || self.project_dialog.is_some() || self.expanded.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&NEW_FUNCTION)) {
            self.create();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&FIND)) {
            self.focus = Some(Focus::Search);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SAVE))
            && let Some(autosaver) = &self.autosaver
        {
            autosaver.flush();
        }
        if let Some(id) = self.current_function() {
            if ctx.input_mut(|i| i.consume_shortcut(&DUPLICATE)) {
                self.duplicate(id);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&RENAME)) {
                self.focus = Some(Focus::Name);
            }
        }
    }

    /// Before closing, writes pending edits; if any could not be saved, asks first.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.allow_close {
            return;
        }
        let flushed = self.autosaver.as_ref().is_none_or(Autosaver::flush);
        let problem = self.docs.values().any(|d| d.status().is_problem());
        if !flushed || problem {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::QuitWithUnsaved);
        }
    }

    // ---- columns --------------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut Ui) {
        let mut open = None;
        let mut actions: Vec<(FunctionId, &'static str)> = Vec::new();
        egui::Panel::bottom("navigation")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 8)))
            .show(ui, |ui| {
                ui.separator();
                let trash_label = if self.trash.is_empty() {
                    "Trash".to_owned()
                } else {
                    format!("Trash ({})", self.trash.len())
                };
                let missing = self.inference.missing();
                let r = list_item(ui, self.view == View::Setup, "Setup");
                if !missing.is_empty() {
                    ui.label(
                        RichText::new("Incomplete: no model can run yet")
                            .small()
                            .color(theme::WARN),
                    );
                }
                let r = if missing.is_empty() {
                    r.on_hover_text("Runtime and model are ready.")
                } else {
                    r.on_hover_text(
                        missing
                            .iter()
                            .map(|m| m.describe())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                };
                if r.clicked() {
                    self.view = View::Setup;
                }
                for (view, label) in [
                    (View::Dataset, "Fine-tune dataset".to_owned()),
                    (View::Knowledge, "Knowledge sources".to_owned()),
                    (View::Settings, "Settings".to_owned()),
                    (View::Trash, trash_label),
                ] {
                    let r = list_item(ui, self.view == view, &label);
                    if r.clicked() {
                        if view == View::Dataset && self.view != View::Dataset {
                            self.load_dataset();
                        }
                        self.view = view;
                    }
                }
            });
        ui.add_space(8.0);
        if self.project.is_some() {
            self.project_sidebar(ui);
            return;
        }
        ui.horizontal(|ui| {
            ui.heading("Functions");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui
                    .add_enabled(self.ready(), egui::Button::new("+ New"))
                    .on_hover_text(format!("New function ({})", keys::label(&NEW_FUNCTION)));
                if r.clicked() {
                    self.create();
                }
            });
        });
        if ui.button("Open project…").clicked() {
            self.project_dialog = Some(ProjectDialog::open(String::new()));
        }
        ui.add_space(4.0);
        let search = ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .id(Id::unique("search"))
                .hint_text(format!("Search  {}", keys::label(&FIND)))
                .desired_width(f32::INFINITY),
        );
        if self.focus == Some(Focus::Search) {
            self.focus = None;
            search.request_focus();
        }
        if search.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.search.clear();
        }
        let matching: Vec<&FunctionSummary> = self
            .functions
            .iter()
            .filter(|f| session::matches_search(f, &self.search))
            .collect();
        if search.lost_focus()
            && ui.input(|i| i.key_pressed(Key::Enter))
            && let Some(first) = matching.first()
        {
            open = Some(first.id);
        }
        ui.add_space(6.0);
        match &self.library_state {
            LibraryState::Opening => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Opening library…").color(theme::MUTED));
                });
            }
            LibraryState::Unavailable(_) => {
                ui.label(RichText::new("The function library could not be opened.").color(theme::BAD));
            }
            LibraryState::Ready if self.functions.is_empty() => {
                ui.label(RichText::new("No saved functions yet.").color(theme::MUTED));
            }
            LibraryState::Ready if matching.is_empty() => {
                ui.label(RichText::new("No functions match the search.").color(theme::MUTED));
            }
            LibraryState::Ready => {
                egui::ScrollArea::vertical()
                    .id_salt("functions")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for f in matching {
                            let selected = self.view == View::Function(f.id);
                            let r = list_item(ui, selected, views::display_name(&f.name)).on_hover_text(
                                if f.description.trim().is_empty() {
                                    views::display_name(&f.name).to_owned()
                                } else {
                                    f.description.clone()
                                },
                            );
                            if r.clicked() {
                                open = Some(f.id);
                            }
                            r.context_menu(|ui| {
                                if ui.button("Open").clicked() {
                                    open = Some(f.id);
                                }
                                if ui.button("Rename").clicked() {
                                    actions.push((f.id, "rename"));
                                }
                                if ui.button("Duplicate").clicked() {
                                    actions.push((f.id, "duplicate"));
                                }
                                if ui.button("Move to trash").clicked() {
                                    actions.push((f.id, "trash"));
                                }
                            });
                        }
                    });
            }
        }
        if let Some(id) = open {
            self.open(id);
        }
        for (id, action) in actions {
            match action {
                "rename" => {
                    self.open(id);
                    self.focus = Some(Focus::Name);
                }
                "duplicate" => self.duplicate(id),
                _ => self.trash_function(id),
            }
        }
    }

    fn center(&mut self, ui: &mut Ui) {
        match self.view {
            View::Welcome => {
                let error = match &self.library_state {
                    LibraryState::Unavailable(e) => Some(e.as_str()),
                    _ => None,
                };
                match views::welcome(ui, error, self.project.is_some()) {
                    WelcomeAction::Create => self.create(),
                    WelcomeAction::OpenProject => {
                        self.project_dialog = Some(ProjectDialog::open(String::new()));
                    }
                    WelcomeAction::None => {}
                }
            }
            View::Trash => match views::trash(ui, &self.trash) {
                Some(TrashAction::Restore(f)) => self.library.send(Command::Restore(f.id)),
                Some(TrashAction::Purge(f)) => self.dialog = Some(Dialog::Purge(f)),
                None => {}
            },
            View::Dataset => {
                if let DatasetAction::Refresh = dataset_view::page(ui, &mut self.dataset_page) {
                    self.load_dataset();
                }
            }
            View::Knowledge => {
                let cx = knowledge_view::PageContext {
                    gpu_free: self.gpu_free(),
                };
                knowledge_view::page(ui, &mut self.knowledge, &cx);
            }
            View::Settings => {
                if let SettingsAction::OpenSetup =
                    views::settings(ui, &mut self.inference, &mut self.models_page)
                {
                    self.view = View::Setup;
                }
            }
            View::Setup => {
                let action = egui::ScrollArea::vertical()
                    .id_salt("setup")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width().min(900.0));
                        setup_view::page(ui, &mut self.inference, &mut self.setup_page)
                    })
                    .inner;
                if !matches!(action, SetupAction::None) {
                    self.view = View::Welcome;
                }
            }
            View::Function(id) => self.function_editor(ui, id),
        }
    }

    fn function_editor(&mut self, ui: &mut Ui, id: FunctionId) {
        if let Some((error, recoverable)) = self.open_errors.get(&id).cloned() {
            views::page_title(ui, "This function could not be opened", "");
            ui.label(RichText::new(error).color(theme::BAD));
            ui.add_space(8.0);
            if recoverable && ui.button("Replace the draft with the latest revision…").clicked() {
                self.dialog = Some(Dialog::Recover(id));
            }
            return;
        }
        if !self.docs.contains_key(&id) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Opening…").color(theme::MUTED));
            });
            return;
        }
        let mut actions = Vec::new();
        let assist = AssistStatus {
            blocked: self.assist_blocked(id),
            job: self.assistant.busy_with(id).map(|j| (j.kind, j.phase)),
            report: self.assistant.report(id).cloned(),
        };
        let unavailable = self.execution.runtime().err();
        let Some(open) = self.docs.get_mut(&id) else {
            return;
        };
        self.references.ensure_checked(&open.doc);
        let changed = egui::ScrollArea::vertical()
            .id_salt(("editor", id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Keep the editor readable on very wide windows.
                ui.set_max_width(ui.available_width().min(980.0));
                let mut cx = EditorContext {
                    cache: &mut self.edn,
                    focus: &mut self.focus,
                    expanded: &mut self.expanded,
                    actions: &mut actions,
                    assist: &assist,
                    references: crate::reference_view::ReferenceUi {
                        state: &mut self.references,
                        functions: &self.functions,
                        unavailable: unavailable.as_deref(),
                    },
                };
                editor::show(ui, open, &mut cx)
            })
            .inner;
        if changed {
            self.submit(id, false);
        }
        for request in std::mem::take(&mut self.references.requests) {
            match request {
                Request::Resolve {
                    function,
                    example,
                    source,
                } => {
                    if let Some(open) = self.docs.get(&function) {
                        self.references.resolve(&open.doc, example, source);
                    }
                }
                Request::Cancel { function, example } => {
                    self.references.cancel(&mut self.execution, function, example);
                }
            }
        }
        for action in actions {
            match action {
                Action::Duplicate => self.duplicate(id),
                Action::Trash => self.trash_function(id),
                Action::RetrySave => self.submit(id, true),
                Action::Removed { message, undo } => self.toast(message, Some(undo)),
                Action::Interpret => self.start_job(id, JobKind::Interpret),
                Action::ProposeContract => self.start_job(id, JobKind::ProposeContract),
                Action::CancelJob => self.cancel_job(),
                Action::DismissReport => self.assistant.dismiss_report(id),
            }
        }
    }

    // ---- overlays -------------------------------------------------------------------------

    fn toasts(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts
            .retain(|t| now.duration_since(t.shown_at) < TOAST_LIFETIME);
        if self.toasts.is_empty() {
            return;
        }
        // Keep repainting so toasts disappear on time even without input.
        ctx.request_repaint_after(Duration::from_millis(500));
        let mut undo = None;
        let mut dismiss = None;
        let mut engaged = false;
        egui::Area::new(Id::unique("toasts"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -18.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (i, toast) in self.toasts.iter().enumerate().rev().take(3) {
                    egui::Frame::new()
                        .fill(theme::CONTROL)
                        .stroke(egui::Stroke::new(
                            1.0,
                            if toast.error {
                                theme::BAD
                            } else {
                                theme::LINE_STRONG
                            },
                        ))
                        .corner_radius(theme::RADIUS)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let color = if toast.error { theme::BAD } else { theme::INK };
                                ui.label(RichText::new(&toast.message).color(color));
                                if toast.undo.is_some() {
                                    let r = ui.button("Undo");
                                    engaged |= r.has_focus();
                                    if r.clicked() {
                                        undo = Some(i);
                                    }
                                }
                                let r = ui.small_button("Dismiss");
                                engaged |= r.has_focus();
                                if r.clicked() {
                                    dismiss = Some(i);
                                }
                            });
                        });
                    ui.add_space(4.0);
                }
                engaged |= ui.ui_contains_pointer();
            });
        // Hold toasts open while someone is reaching for them.
        if engaged {
            for t in &mut self.toasts {
                t.shown_at = now;
            }
        }
        if let Some(i) = undo {
            let toast = self.toasts.remove(i);
            if let Some(u) = toast.undo {
                self.undo(u);
            }
        } else if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
    }

    fn dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let mut close = false;
        let mut confirmed = false;
        let (title, body, confirm) = match dialog {
            Dialog::Purge(f) => (
                "Delete permanently?".to_owned(),
                format!(
                    "“{}” and its entire history will be deleted. This cannot be undone.",
                    views::display_name(&f.name)
                ),
                "Delete permanently",
            ),
            Dialog::Recover(_) => (
                "Replace the unreadable draft?".to_owned(),
                "The draft will be replaced by the latest saved revision. The unreadable draft \
                 cannot be recovered afterwards."
                    .to_owned(),
                "Replace draft",
            ),
            Dialog::QuitWithUnsaved => (
                "Quit without saving?".to_owned(),
                "Some changes could not be saved and will be lost if you quit now.".to_owned(),
                "Quit anyway",
            ),
        };
        let response = egui::Modal::new(Id::unique("dialog")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(title);
            ui.add_space(6.0);
            ui.label(body);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let cancel = ui.button("Cancel");
                if ui.button(RichText::new(confirm).color(theme::BAD)).clicked() {
                    confirmed = true;
                }
                // The safe choice has focus, so Enter does not destroy anything by accident.
                if !ui.memory(|m| m.focused().is_some()) {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    close = true;
                }
            });
        });
        if response.should_close() {
            close = true;
        }
        if confirmed {
            match self.dialog.take() {
                Some(Dialog::Purge(f)) => self.library.send(Command::Purge(f.id)),
                Some(Dialog::Recover(id)) => {
                    self.open_errors.remove(&id);
                    self.loading.insert(id);
                    self.library.send(Command::Recover(id));
                }
                Some(Dialog::QuitWithUnsaved) => {
                    self.allow_close = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                None => {}
            }
        } else if close {
            self.dialog = None;
        }
    }
}

/// Starts serving MCP clients on the data directory's socket. Failure (for example a second
/// window on the same library) is logged; the application works without it.
fn start_mcp(
    ctx: &egui::Context,
) -> (
    Option<clojurecraft_mcp::Server>,
    Option<std::sync::mpsc::Receiver<clojurecraft_mcp::Request>>,
) {
    let Some(dirs) = clojurecraft_runtime::DataDirs::discover() else {
        return (None, None);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    match clojurecraft_mcp::Server::start(&dirs.mcp_socket(), tx, move || ctx.request_repaint()) {
        Ok(server) => (Some(server), Some(rx)),
        Err(e) => {
            tracing::warn!(error = %e, "not serving MCP clients");
            (None, None)
        }
    }
}

/// The assistant's access to inference, documentation, and execution.
struct AppPorts<'a> {
    inference: &'a mut InferenceState,
    knowledge: &'a mut KnowledgeState,
    execution: &'a mut ExecutionState,
}

impl Ports for AppPorts<'_> {
    fn generate(&mut self, request: GenerateRequest) -> Result<RequestId, String> {
        self.inference.ensure_model()?;
        self.inference.generate(request)
    }

    fn cancel(&mut self, request: RequestId) {
        self.inference.cancel_request(request);
    }

    fn gather(&mut self, gather: Gather) -> Option<u64> {
        self.knowledge.gather(gather)
    }

    fn validate(&mut self, request: EvalRequest) -> Result<u64, String> {
        self.execution.start(request)
    }

    fn cancel_validation(&mut self, id: u64) {
        self.execution.cancel(id);
    }

    fn runtime(&self) -> Option<clojurecraft_clojure::ClojureRuntime> {
        self.execution.runtime().ok()
    }
}

/// A full-width, left-aligned selectable row.
fn list_item(ui: &mut Ui, selected: bool, text: &str) -> egui::Response {
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
        ui.add(
            egui::Button::selectable(selected, text)
                .truncate()
                .min_size(egui::vec2(0.0, theme::CONTROL_H)),
        )
    })
    .inner
}

impl eframe::App for ClojureCraftApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_events();
        self.poll_picker(&ctx);
        self.poll_classpath();
        self.review_frame();
        self.handle_close(&ctx);
        self.shortcuts(&ctx);

        let total = ctx.content_rect().width();
        let side = egui::Frame::new()
            .fill(theme::PANEL)
            .inner_margin(12.0)
            .stroke(theme::hairline());
        let left_max = (total - MIN_CENTER_WIDTH - 300.0).clamp(200.0, 360.0);
        let left = egui::Panel::left("library")
            .resizable(true)
            .default_size(240.0)
            .size_range(200.0..=left_max)
            .frame(side)
            .show(ui, |ui| self.sidebar(ui))
            .response
            .rect
            .right();
        let right_max = (total - left - MIN_CENTER_WIDTH).clamp(300.0, 720.0);
        let mut open_setup = false;
        let mut open_knowledge = false;
        let mut remedy = RemedyAction::None;
        let blocked = match self.view {
            View::Function(id) => self.assist_blocked(id),
            _ => None,
        };
        let current = match self.view {
            View::Function(id) => self.docs.get(&id).map(|o| &o.doc),
            _ => None,
        };
        let mut panel = None;
        let mut review_action = None;
        let mut step_action = None;
        egui::Panel::right("implementation")
            .resizable(true)
            .default_size(400.0)
            .size_range(300.0..=right_max)
            .frame(side)
            .show(ui, |ui| {
                let missing = self.inference.missing();
                if missing.is_empty() {
                    ui.add_space(8.0);
                    egui::Frame::new()
                        .stroke(theme::hairline())
                        .corner_radius(theme::RADIUS)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            remedy = model_view::status(ui, &mut self.inference, true);
                        });
                }
                let reports = current
                    .and_then(|d| self.validations.get(&d.id))
                    .map_or(&[][..], Vec::as_slice);
                let cx = ColumnContext {
                    missing: &missing,
                    knowledge: &mut self.knowledge,
                    assistant: &self.assistant,
                    blocked,
                    validation: ValidationContext {
                        reports,
                        running: current.is_some_and(|d| self.checks.contains_key(&d.id)),
                        unavailable: self.execution.runtime().err(),
                        installable: self.execution.installable(),
                        install: &self.execution.install,
                    },
                    review: &mut self.review,
                    steps: StepsContext {
                        inspections: &self.step_inspections,
                        unavailable: self.execution.runtime().err(),
                    },
                };
                match implementation::show(ui, &mut self.right_tab, current, cx) {
                    implementation::ColumnAction::OpenSetup => open_setup = true,
                    implementation::ColumnAction::OpenKnowledge => open_knowledge = true,
                    implementation::ColumnAction::Generation(a) => panel = current.map(|d| (d.id, a)),
                    implementation::ColumnAction::Review(a) => review_action = current.map(|d| (d.id, a)),
                    implementation::ColumnAction::Steps(a) => step_action = current.map(|d| (d.id, a)),
                    implementation::ColumnAction::None => {}
                }
            });
        if let Some((id, action)) = review_action {
            self.review_action(id, action);
        }
        if let Some((id, action)) = step_action {
            match action {
                StepAction::Run => {
                    if let Some(open) = self.docs.get(&id) {
                        self.step_inspections.start(&open.doc, &mut self.execution);
                    }
                }
                StepAction::Cancel => self.step_inspections.cancel(id, &mut self.execution),
                StepAction::Select(example) => {
                    self.step_inspections.selected.insert(id, example);
                }
            }
        }
        if let Some((id, action)) = panel {
            let r = self.panel_action(id, action);
            if r != RemedyAction::None {
                remedy = r;
            }
        }
        match remedy {
            RemedyAction::OpenSetup => open_setup = true,
            RemedyAction::OpenSettings => self.view = View::Settings,
            RemedyAction::None => {}
        }
        if open_setup {
            self.view = View::Setup;
        }
        if open_knowledge {
            self.view = View::Knowledge;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(20.0))
            .show(ui, |ui| self.center(ui));

        self.save_project(&ctx, false);
        self.toasts(&ctx);
        self.dialog(&ctx);
        self.project_dialog(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let last = match self.view {
            View::Function(id) => Some(id),
            _ => None,
        };
        eframe::set_value(storage, STORAGE_LAST_FUNCTION, &last);
        let project = self.project.as_ref().map(|p| p.root().to_path_buf());
        eframe::set_value(storage, STORAGE_LAST_PROJECT, &project);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stops llama.cpp; it must not outlive the application.
        self.inference.shutdown_model();
        self.knowledge.shutdown();
        if let Some(project) = &mut self.project
            && let Err(e) = project.save_pending(true)
        {
            tracing::error!(error = %e, "the project specification could not be saved before exit");
        }
        if let Some(autosaver) = &self.autosaver
            && !autosaver.flush()
        {
            tracing::error!("some edits could not be saved before exit");
        }
    }
}
