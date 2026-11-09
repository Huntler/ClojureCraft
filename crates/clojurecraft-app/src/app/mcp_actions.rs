//! Carrying out tool calls from MCP clients (`clojurecraft_mcp`) on the application's state.
//!
//! Calls arrive on a channel and are handled once per frame, after the services' events, through
//! the same operations as the interface's buttons, so an agent's work is saved, recorded in the
//! history, and visible like the person's own. A call about a function that is not open yet (or
//! made before the library is ready) waits until it is, without switching the view.

use std::time::{Duration, Instant};

use clojurecraft_domain::{FunctionDocument, Implementation, RevisionKind};
use std::collections::{BTreeMap, HashMap, VecDeque};

use clojurecraft_mcp::tools::{CaseSource, ExportArgs, GenerateMode, SpecArgs};
use clojurecraft_mcp::{Call, Reply, Request, edit, export as mcp_export, views as mcp_views};
use clojurecraft_workflow::assistant::Report;
use clojurecraft_workflow::{edge_cases, export};
use serde_json::{Value, json};

use super::*;

/// How long a call may wait for the library or for a function to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// A call waiting for the library or a function to be ready.
pub(super) struct Parked {
    request: Request,
    since: Instant,
}

/// Generations waiting for the model, and why queued ones could not start.
#[derive(Default)]
pub(super) struct Queue {
    waiting: VecDeque<(FunctionId, GenerateMode)>,
    errors: HashMap<FunctionId, String>,
}

impl Queue {
    /// Position from 1, while waiting.
    fn position(&self, id: FunctionId) -> Option<usize> {
        self.waiting.iter().position(|(f, _)| *f == id).map(|i| i + 1)
    }
}

enum Step {
    Done(Reply),
    Wait,
}

impl ClojureCraftApp {
    /// Handles new and waiting tool calls.
    pub(super) fn handle_mcp(&mut self) {
        self.run_queue();
        let mut queue: Vec<Parked> = std::mem::take(&mut self.mcp_waiting);
        if let Some(rx) = &self.mcp_requests {
            queue.extend(rx.try_iter().map(|request| Parked {
                request,
                since: Instant::now(),
            }));
        }
        for mut parked in queue {
            match self.mcp_step(&mut parked.request.call) {
                Step::Done(reply) => parked.request.respond(reply),
                Step::Wait if parked.since.elapsed() > OPEN_TIMEOUT => parked
                    .request
                    .respond(Err("The function library did not become ready in time.".into())),
                Step::Wait => self.mcp_waiting.push(parked),
            }
        }
    }

    fn mcp_step(&mut self, call: &mut Call) -> Step {
        match &self.library_state {
            LibraryState::Opening => return Step::Wait,
            LibraryState::Unavailable(e) => {
                return Step::Done(Err(format!("The function library could not be opened: {e}")));
            }
            LibraryState::Ready => {}
        }
        let id = match call {
            Call::Resolve(names) => return Step::Done(self.mcp_resolve(names)),
            Call::ListFunctions(query) => {
                let project = self.project.as_ref();
                let mut list: Vec<Value> = Vec::new();
                if let Some(p) = project {
                    for (ns, _, spec) in p.specs() {
                        let description = self
                            .docs
                            .get(&spec.id)
                            .map_or(spec.description.as_str(), |o| o.doc.description.as_str());
                        let mut v = mcp_views::summary(spec.id, &spec.name, description);
                        v["namespace"] = json!(ns);
                        list.push(v);
                    }
                }
                // With a project open, only its functions are offered: the others would be
                // checked against this project's classpath.
                if project.is_none() {
                    list.extend(
                        self.functions
                            .iter()
                            .map(|f| mcp_views::summary(f.id, &f.name, &f.description)),
                    );
                }
                if let Some(q) = query {
                    let q = q.to_lowercase();
                    list.retain(|v| {
                        ["name", "namespace", "description"]
                            .iter()
                            .any(|k| v[*k].as_str().is_some_and(|t| t.to_lowercase().contains(&q)))
                    });
                }
                let mut reply = json!({ "functions": list });
                if let Some(p) = project {
                    reply["project"] = json!(p.root().display().to_string());
                }
                return Step::Done(Ok(reply));
            }
            Call::Cancel => {
                let was = self.assistant.job().map(|j| j.function);
                let dequeued: Vec<String> = self
                    .mcp_queue
                    .waiting
                    .drain(..)
                    .map(|(f, _)| f.to_string())
                    .collect();
                self.cancel_job();
                return Step::Done(Ok(json!({
                    "cancelled": was.map(|f| f.to_string()),
                    "removed_from_queue": dequeued,
                })));
            }
            Call::ExportProject(args) => {
                for id in args.functions.clone() {
                    if let Some(step) = self.mcp_open(id) {
                        return step;
                    }
                }
                return Step::Done(self.mcp_export(args));
            }
            Call::CreateFunction(spec) => {
                // Answer once the new function is open.
                return match self.mcp_create(std::slice::from_ref(spec)) {
                    Ok(functions) => {
                        *call = Call::Created {
                            functions,
                            batch: false,
                        };
                        Step::Wait
                    }
                    Err(e) => Step::Done(Err(e)),
                };
            }
            Call::CreateFunctions(specs) => {
                return match self.mcp_create(specs) {
                    Ok(functions) => {
                        *call = Call::Created {
                            functions,
                            batch: true,
                        };
                        Step::Wait
                    }
                    Err(e) => Step::Done(Err(e)),
                };
            }
            Call::Created { functions, batch } => {
                for id in functions.clone() {
                    if let Some(step) = self.mcp_open(id) {
                        return step;
                    }
                }
                let list: Vec<Value> = functions
                    .iter()
                    .map(|id| {
                        let doc = &self.docs[id].doc;
                        json!({"id": id.to_string(), "name": doc.name, "readiness": readiness(doc)})
                    })
                    .collect();
                return Step::Done(Ok(if *batch {
                    json!({ "functions": list })
                } else {
                    list.into_iter().next().unwrap_or_default()
                }));
            }
            Call::GenerateMany(ids, mode) => {
                for id in ids.clone() {
                    if let Some(step) = self.mcp_open(id) {
                        return step;
                    }
                }
                if self.inference.snapshot.is_none() {
                    return Step::Wait;
                }
                let (ids, mode) = (ids.clone(), *mode);
                let list: Vec<Value> = ids
                    .into_iter()
                    .map(|id| {
                        let mut v = match self.mcp_generate(id, mode) {
                            Ok(v) => v,
                            Err(e) => json!({ "error": e }),
                        };
                        if let Some(o) = v.as_object_mut() {
                            o.remove("next");
                        }
                        v["id"] = json!(id.to_string());
                        v
                    })
                    .collect();
                return Step::Done(Ok(json!({
                    "functions": list,
                    "next": "Call status with function_ids and wait_seconds until working is false.",
                })));
            }
            Call::StatusMany(ids) => {
                for id in ids.clone() {
                    if let Some(step) = self.mcp_open(id) {
                        return step;
                    }
                }
                return Step::Done(Ok(self.mcp_status_many(ids)));
            }
            Call::GetFunction(id)
            | Call::UpdateFunction(id, _)
            | Call::GenerateTestCases(id, _)
            | Call::ReviewTestCases { function: id, .. }
            | Call::RunTests(id)
            | Call::Describe(id)
            | Call::Generate(id, _)
            | Call::Status(id, _)
            | Call::Result(id, _)
            | Call::AcceptCode(id)
            | Call::SubmitCode(id, _) => *id,
        };
        // Right after start-up (often by `clojurecraft mcp`) the installed runtime and models are
        // not scanned yet, and a model job would be refused as if nothing were installed.
        if matches!(
            call,
            Call::GenerateTestCases(..) | Call::Describe(_) | Call::Generate(..) | Call::GenerateMany(..)
        ) && self.inference.snapshot.is_none()
        {
            return Step::Wait;
        }
        if let Some(step) = self.mcp_open(id) {
            return step;
        }
        Step::Done(self.mcp_call(id, call))
    }

    /// `None` when the function is open; otherwise starts opening it (quietly, without switching
    /// the view) and waits, or reports why it cannot be opened.
    fn mcp_open(&mut self, id: FunctionId) -> Option<Step> {
        if let Some(p) = &self.project
            && p.locate(id).is_none()
        {
            return Some(Step::Done(Err(outside_project(Some(p)))));
        }
        if self.docs.contains_key(&id) {
            return None;
        }
        if let Some((error, _)) = self.open_errors.get(&id) {
            return Some(Step::Done(Err(format!(
                "The function could not be opened: {error}"
            ))));
        }
        if !self.loading.contains(&id)
            && let Some(doc) = self
                .project
                .as_ref()
                .and_then(|p| p.locate(id).and_then(|(ns, f)| p.document(ns, f)))
        {
            self.loading.insert(id);
            self.quiet_open.insert(id);
            self.library.send(Command::OpenOrInsert(Box::new(doc)));
        }
        if !self.loading.contains(&id) {
            if !self.functions.iter().any(|f| f.id == id) {
                return Some(Step::Done(Err(format!(
                    "There is no function with the id {id} (it may be in the trash). Use list_functions."
                ))));
            }
            self.loading.insert(id);
            self.quiet_open.insert(id);
            self.library.send(Command::Open(id));
        }
        Some(Step::Wait)
    }

    /// Writes the open functions in `args` as a Clojure project.
    fn mcp_export(&self, args: &ExportArgs) -> Reply {
        let docs: Vec<&FunctionDocument> = args.functions.iter().map(|id| &self.docs[id].doc).collect();
        let environment = self.execution.environment();
        let project = export::project(&docs, &environment.clojure_version)?;
        let written = mcp_export::write(&args.directory, &project, args.overwrite)?;
        let passes: BTreeMap<FunctionId, Option<bool>> = docs
            .iter()
            .map(|doc| {
                let basis = doc.basis();
                let pass = self
                    .validations
                    .get(&doc.id)
                    .and_then(|reports| reports.iter().find(|r| r.basis == basis))
                    .map(ValidationReport::passed);
                (doc.id, pass)
            })
            .collect();
        let runtime = self.execution.runtime().ok();
        Ok(mcp_export::reply(
            &args.directory,
            &project,
            &written,
            &passes,
            runtime.as_ref().map(|r| mcp_export::Runtime {
                java: &r.java,
                jars: &r.jars,
            }),
        ))
    }

    /// Ids of functions given by name: the library name or derived function name, or in a project
    /// the symbol or `namespace/function`; case does not matter. Fails on unknown or ambiguous
    /// names.
    fn mcp_resolve(&self, names: &[String]) -> Reply {
        let candidates: Vec<(FunctionId, Vec<String>)> = match &self.project {
            Some(p) => p
                .specs()
                .map(|(ns, f, spec)| {
                    (
                        spec.id,
                        vec![format!("{ns}/{f}"), f.to_owned(), spec.name.clone()],
                    )
                })
                .collect(),
            None => self
                .functions
                .iter()
                .map(|f| {
                    let derived = clojurecraft_workflow::Names::for_document(
                        &FunctionDocument::new(f.name.clone()),
                        |_| false,
                    );
                    (f.id, vec![f.name.clone(), derived.function])
                })
                .collect(),
        };
        let mut ids = Vec::new();
        for name in names {
            let wanted = name.trim().to_lowercase();
            let found: Vec<FunctionId> = candidates
                .iter()
                .filter(|(_, aliases)| aliases.iter().any(|a| a.to_lowercase() == wanted))
                .map(|(id, _)| *id)
                .collect();
            match found.as_slice() {
                [id] => ids.push(json!(id.to_string())),
                [] => return Err(format!("No function is named “{name}”. Use list_functions.")),
                _ => {
                    return Err(format!(
                        "“{name}” names several functions; use namespace/function or an id from list_functions."
                    ));
                }
            }
        }
        Ok(Value::Array(ids))
    }

    /// Adds new functions made from `specs` (all or none) and returns their ids; they open
    /// shortly after.
    fn mcp_create(&mut self, specs: &[SpecArgs]) -> Result<Vec<FunctionId>, String> {
        let at = |i: usize, e: String| {
            if specs.len() > 1 {
                format!("functions[{i}]: {e}")
            } else {
                e
            }
        };
        if self.project.is_some() {
            return self.mcp_create_in_project(specs, &at);
        }
        if let Some(i) = specs
            .iter()
            .position(|s| s.namespace.is_some() || s.arguments.is_some())
        {
            return Err(at(
                i,
                "`namespace` and `arguments` apply only while a project is open.".into(),
            ));
        }
        let mut names: Vec<String> = self.functions.iter().map(|f| f.name.clone()).collect();
        let mut docs = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let mut doc = FunctionDocument::new(String::new());
            edit::apply_spec(&mut doc, spec).map_err(|e| at(i, e))?;
            doc.name = session::unique_name(&doc.name, names.iter().map(String::as_str));
            names.push(doc.name.clone());
            docs.push(doc);
        }
        Ok(docs
            .into_iter()
            .map(|doc| {
                let id = doc.id;
                self.loading.insert(id);
                self.library.send(Command::Insert(Box::new(doc)));
                id
            })
            .collect())
    }

    /// Adds functions to namespaces of the open project (creating missing namespaces), with
    /// their specifications. Everything is checked before anything is added.
    fn mcp_create_in_project(
        &mut self,
        specs: &[SpecArgs],
        at: &dyn Fn(usize, String) -> String,
    ) -> Result<Vec<FunctionId>, String> {
        let project = self.project.as_mut().ok_or("No project is open.")?;
        let mut planned: Vec<(String, String)> = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let namespace = spec
                .namespace
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| at(i, "`namespace` is required while a project is open.".into()))?;
            crate::project::validate_namespace(namespace).map_err(|e| at(i, e))?;
            let function = spec.name.as_deref().unwrap_or_default().trim();
            crate::project::validate_symbol(function).map_err(|e| at(i, e))?;
            if project.id_of(namespace, function).is_some()
                || planned.iter().any(|(n, f)| n == namespace && f == function)
            {
                return Err(at(i, format!("{function} is already defined in {namespace}")));
            }
            edit::apply_spec(&mut FunctionDocument::new(String::new()), spec).map_err(|e| at(i, e))?;
            planned.push((namespace.to_owned(), function.to_owned()));
        }
        let mut ids = Vec::new();
        for (spec, (namespace, function)) in specs.iter().zip(&planned) {
            if !project.contains(namespace) {
                project.add_namespace(namespace)?;
            }
            let id = project.add_function(namespace, function, spec.arguments.unwrap_or(1))?;
            let mut doc = project
                .document(namespace, function)
                .ok_or("The new function could not be read back.")?;
            edit::apply_spec(&mut doc, spec)?;
            // Opening syncs from ClojureCraft.json, which must already hold the specification.
            project.record(&mut doc)?;
            self.loading.insert(id);
            self.quiet_open.insert(id);
            self.library.send(Command::OpenOrInsert(Box::new(doc)));
            ids.push(id);
        }
        Ok(ids)
    }

    /// Starts generating `id`, or queues it while the model works on something else.
    fn mcp_generate(&mut self, id: FunctionId, mode: GenerateMode) -> Reply {
        self.mcp_queue.errors.remove(&id);
        if let Some(n) = self.mcp_queue.position(id) {
            return Ok(json!({ "queued": n, "next": "Call status with wait_seconds." }));
        }
        if self.assistant.job().is_some() && self.assistant.busy_with(id).is_none() {
            // Model missing or similar: say so now rather than after waiting in the queue.
            if let Some(m) = self.inference.missing().first() {
                return Err(m.describe());
            }
            self.mcp_queue.waiting.push_back((id, mode));
            return Ok(json!({
                "queued": self.mcp_queue.waiting.len(),
                "next": "Call status with wait_seconds; queued jobs start one after another.",
            }));
        }
        self.start_generation(id, mode)?;
        self.started(id)
    }

    fn start_generation(&mut self, id: FunctionId, mode: GenerateMode) -> Result<(), String> {
        match mode {
            GenerateMode::Fresh => self.try_start_job(id, JobKind::Generate),
            GenerateMode::Fix => self.try_fix_failing(id),
        }
    }

    /// Starts queued generations while the model is free. A generation that cannot start (or
    /// ends at once) does not hold up the rest.
    fn run_queue(&mut self) {
        while self.assistant.job().is_none() && self.inference.snapshot.is_some() {
            let Some((id, mode)) = self.mcp_queue.waiting.pop_front() else {
                return;
            };
            let started = if self.docs.contains_key(&id) {
                self.start_generation(id, mode)
                    .and_then(|()| self.started(id).map(drop))
            } else {
                Err("The function was closed before its turn.".into())
            };
            if let Err(e) = started {
                self.mcp_queue.errors.insert(id, e);
            }
        }
    }

    fn mcp_call(&mut self, id: FunctionId, call: &Call) -> Reply {
        match call {
            Call::GetFunction(_) => {
                let doc = &self.docs[&id].doc;
                let mut v = mcp_views::function(doc);
                v["readiness"] = readiness(doc);
                Ok(v)
            }
            Call::UpdateFunction(_, spec) => {
                let open = self.docs.get_mut(&id).ok_or("The function is not open.")?;
                edit::apply_spec(&mut open.doc, spec)?;
                let v = json!({ "updated": true, "readiness": readiness(&open.doc) });
                self.submit(id, false);
                Ok(v)
            }
            Call::GenerateTestCases(_, source) => {
                match source {
                    CaseSource::Examples => self.try_generate_cases(id)?,
                    CaseSource::Specification => self.try_start_job(id, JobKind::ProposeTests)?,
                }
                self.started(id)
            }
            Call::ReviewTestCases { actions, add, .. } => {
                let open = self.docs.get_mut(&id).ok_or("The function is not open.")?;
                let outcome = edit::review(&mut open.doc, actions, add)?;
                let (accepted, proposed, ambiguous) = edge_cases::counts(&open.doc);
                self.review.cases.retain(|case, _| open.doc.test(*case).is_some());
                if outcome.accepted_changed {
                    self.commit(
                        id,
                        RevisionReason::with_note(
                            RevisionKind::TestsAccepted,
                            "Test cases reviewed by an agent",
                        ),
                    );
                } else {
                    self.submit(id, false);
                }
                Ok(json!({
                    "added": outcome.added.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "tests": {"accepted": accepted, "proposed": proposed, "need_expected_value": ambiguous},
                }))
            }
            Call::RunTests(_) => {
                self.try_run_checks(id)?;
                Ok(json!({
                    "started": self.checks.contains_key(&id),
                    "next": "Call status with wait_seconds for the outcome.",
                }))
            }
            Call::Describe(_) => {
                self.try_start_job(id, JobKind::Describe)?;
                self.started(id)
            }
            Call::Generate(_, mode) => self.mcp_generate(id, *mode),
            Call::Status(_, detail) => Ok(self.mcp_status(id, *detail)),
            Call::Result(_, detail) => {
                let doc = &self.docs[&id].doc;
                let reports = self.validations.get(&id).map_or(&[][..], Vec::as_slice);
                Ok(mcp_views::result(
                    doc,
                    reports,
                    self.assistant.report(id),
                    *detail,
                ))
            }
            Call::AcceptCode(_) => {
                if mcp_views::pending(self.assistant.report(id)).is_none() {
                    return Err("No generated code is waiting for acceptance.".into());
                }
                let open = self.docs.get_mut(&id).ok_or("The function is not open.")?;
                let updates = self.assistant.accept_pending(&mut open.doc)?;
                self.apply_updates(updates);
                Ok(self.mcp_status(id, mcp_views::Detail::Compact))
            }
            Call::SubmitCode(_, code) => self.mcp_submit_code(id, code),
            Call::ListFunctions(_)
            | Call::Resolve(_)
            | Call::Cancel
            | Call::CreateFunction(_)
            | Call::CreateFunctions(_)
            | Call::Created { .. }
            | Call::GenerateMany(..)
            | Call::StatusMany(_)
            | Call::ExportProject(_) => Err("This call does not take a function.".into()),
        }
    }

    /// Makes `code` the function's hand-written code, recorded as a revision, and checks it.
    fn mcp_submit_code(&mut self, id: FunctionId, code: &str) -> Reply {
        if self.assistant.busy_with(id).is_some() {
            return Err(
                "A model job is working on this function and would replace the code; wait for it \
                 with status or call cancel first."
                    .into(),
            );
        }
        let mut code = code.trim_end().to_owned();
        code.push('\n');
        let open = self.docs.get_mut(&id).ok_or("The function is not open.")?;
        let replaced = open.doc.implementation.as_ref().map(|i| i.source.as_str()) != Some(&code);
        if replaced {
            open.doc.implementation = Some(Implementation::manual(code));
            self.commit(
                id,
                RevisionReason::with_note(RevisionKind::CodeEdited, "Code written by an agent"),
            );
            // A check of the previous code would otherwise count as this code's.
            self.cancel_checks(id);
        }
        self.try_run_checks(id)?;
        Ok(json!({
            "replaced": replaced,
            "started": self.checks.contains_key(&id),
            "next": "Call status with wait_seconds for the checks.",
        }))
    }

    /// The reply to a call that started a model job: started, or why it ended at once.
    fn started(&self, id: FunctionId) -> Reply {
        if let Some(job) = self.assistant.busy_with(id) {
            return Ok(json!({
                "started": job.phase.label(),
                "next": "Call status with wait_seconds until working is false.",
            }));
        }
        match self.assistant.report(id) {
            Some(Report::Blocked(reasons)) => Err(format!("Not started: {}", reasons.join(" "))),
            Some(Report::Failed { message, .. }) => Err(format!("Not started: {message}")),
            _ => Err("The job did not start.".into()),
        }
    }

    fn mcp_status(&self, id: FunctionId, detail: mcp_views::Detail) -> Value {
        let doc = &self.docs[&id].doc;
        let reports = self.validations.get(&id).map_or(&[][..], Vec::as_slice);
        let mut progress = self.progress(id);
        progress.detail = detail;
        mcp_views::status(doc, reports, &progress)
    }

    fn mcp_status_many(&self, ids: &[FunctionId]) -> Value {
        let briefs: Vec<Value> = ids
            .iter()
            .map(|id| {
                let reports = self.validations.get(id).map_or(&[][..], Vec::as_slice);
                mcp_views::brief(&self.docs[id].doc, reports, &self.progress(*id))
            })
            .collect();
        let working = briefs
            .iter()
            .any(|b| b.get("working").is_some() || b.get("queued").is_some());
        let next = if working {
            "Call status with wait_seconds to wait until the work finishes."
        } else {
            "Use status with one function_id for the full checks of a failing function, result for \
             code, or export_project."
        };
        json!({ "working": working, "functions": briefs, "next": next })
    }

    fn progress(&self, id: FunctionId) -> mcp_views::Progress<'_> {
        let job = self.assistant.busy_with(id);
        mcp_views::Progress {
            job,
            blocked: if job.is_some() {
                None
            } else {
                self.assist_blocked(id)
            },
            last: self.assistant.report(id),
            checks_running: self.checks.contains_key(&id),
            runtime_unavailable: self.execution.runtime().err(),
            queued: self.mcp_queue.position(id),
            queue_error: self.mcp_queue.errors.get(&id).cloned(),
            detail: mcp_views::Detail::Compact,
        }
    }
}

/// What still blocks generation, and what will be left out.
fn readiness(doc: &FunctionDocument) -> Value {
    let r = clojurecraft_workflow::readiness(doc);
    json!({ "ready": r.is_ready(), "blocking": r.blocking, "notes": r.notes })
}

/// Why a function outside the open project cannot be used.
fn outside_project(project: Option<&Project>) -> String {
    let root = project
        .map(|p| p.root().display().to_string())
        .unwrap_or_default();
    format!(
        "Only the functions of the open project {root} can be used while it is open (see \
         list_functions). Close the project in ClojureCraft to work on functions of the library."
    )
}
