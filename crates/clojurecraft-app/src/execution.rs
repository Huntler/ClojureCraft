//! Runs code in the sandboxed Clojure runtime off the UI thread, and installs that runtime.
//!
//! Validation runs are queued on one worker thread and executed one at a time, each in a fresh
//! sandboxed JVM. Every run has an identifier and a cancellation flag; a cancelled run stops its
//! JVM promptly and its result is never delivered.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;

use clojurecraft_clojure::{ClojureRuntime, EvalRequest, Limits};
use clojurecraft_domain::ExecutionEnvironment;
use clojurecraft_runtime::{Cancel, DataDirs, Manifest, Progress, components, platform_key};
use clojurecraft_workflow::assistant::Execution;

use crate::jobs::Event;

pub enum ExecutionEvent {
    Finished { id: u64, execution: Box<Execution> },
    InstallProgress { file: String, progress: Progress },
    Installed(Result<(), String>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum InstallState {
    #[default]
    Idle,
    Running {
        file: String,
        progress: Progress,
    },
    Failed(String),
}

struct Run {
    id: u64,
    request: EvalRequest,
    runtime: ClojureRuntime,
    environment: ExecutionEnvironment,
    cancel: Arc<AtomicBool>,
}

pub struct ExecutionState {
    worker: Sender<Run>,
    events: Sender<Event>,
    ctx: egui::Context,
    env: Result<(DataDirs, Manifest), String>,
    next_id: u64,
    running: HashMap<u64, Arc<AtomicBool>>,
    /// Finished runs, taken by the application each frame.
    pub finished: Vec<(u64, Execution)>,
    pub install: InstallState,
    install_cancel: Option<Cancel>,
}

fn sandbox_helper() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|d| d.join(clojurecraft_sandbox::HELPER_NAME))
        .filter(|p| p.is_file())
}

impl ExecutionState {
    pub fn new(events: Sender<Event>, ctx: egui::Context) -> Self {
        let (worker, rx) = mpsc::channel::<Run>();
        let (tx, wake) = (events.clone(), ctx.clone());
        let spawned = thread::Builder::new().name("execution".into()).spawn(move || {
            for run in rx {
                if run.cancel.load(Ordering::SeqCst) {
                    continue;
                }
                let limits = Limits::for_request(&run.request);
                let outcome = run
                    .runtime
                    .evaluate(&run.request, &limits, Some(run.cancel.clone()))
                    .map_err(|e| e.to_string());
                if run.cancel.load(Ordering::SeqCst) {
                    continue;
                }
                let execution = Execution {
                    outcome,
                    environment: run.environment,
                };
                let _ = tx.send(Event::Execution(Box::new(ExecutionEvent::Finished {
                    id: run.id,
                    execution: Box::new(execution),
                })));
                wake.request_repaint();
            }
        });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "could not start the execution worker");
        }
        let env = DataDirs::discover()
            .ok_or_else(|| "No data directory is available.".to_owned())
            .and_then(|d| Ok((d, Manifest::bundled().map_err(|e| e.to_string())?)));
        Self {
            worker,
            events,
            ctx,
            env,
            next_id: 0,
            running: HashMap::new(),
            finished: Vec::new(),
            install: InstallState::Idle,
            install_cancel: None,
        }
    }

    pub fn handle(&mut self, event: ExecutionEvent) {
        match event {
            ExecutionEvent::Finished { id, execution } => {
                if self.running.remove(&id).is_some() {
                    self.finished.push((id, *execution));
                }
            }
            ExecutionEvent::InstallProgress { file, progress } => {
                self.install = InstallState::Running { file, progress };
            }
            ExecutionEvent::Installed(result) => {
                self.install_cancel = None;
                self.install = match result {
                    Ok(()) => InstallState::Idle,
                    Err(e) => InstallState::Failed(e),
                };
            }
        }
    }

    /// The runtime used for validation, or why it cannot be used.
    pub fn runtime(&self) -> Result<ClojureRuntime, String> {
        let (dirs, manifest) = self.env.as_ref().map_err(Clone::clone)?;
        let missing = "The Java runtime and Clojure are not installed, so code cannot be run.";
        let (java, java_home) = components::java(manifest, dirs, &platform_key()).ok_or(missing)?;
        let jars = components::clojure_jars(manifest, dirs).ok_or(missing)?;
        let helper = sandbox_helper().ok_or_else(|| {
            format!(
                "The sandbox helper ({}) was not found next to the application, so code cannot be run \
                 safely.",
                clojurecraft_sandbox::HELPER_NAME
            )
        })?;
        Ok(ClojureRuntime {
            java,
            java_home,
            jars,
            sandbox_helper: helper,
            work_dir: dirs.root().join("harness"),
        })
    }

    /// Whether installing the Java runtime and Clojure would make validation possible.
    pub fn installable(&self) -> bool {
        let Ok((dirs, manifest)) = &self.env else {
            return false;
        };
        components::java(manifest, dirs, &platform_key()).is_none()
            || components::clojure_jars(manifest, dirs).is_none()
    }

    pub fn environment(&self) -> ExecutionEnvironment {
        let (clojure_version, java_version) = match &self.env {
            Ok((_, m)) => (
                m.clojure.version.clone(),
                format!("{} {}", m.jre.vendor, m.jre.version),
            ),
            Err(_) => (String::new(), String::new()),
        };
        ExecutionEnvironment {
            clojure_version,
            java_version,
            platform: platform_key(),
        }
    }

    /// Queues `request`; the result arrives in [`Self::finished`].
    pub fn start(&mut self, request: EvalRequest) -> Result<u64, String> {
        let runtime = self.runtime()?;
        self.next_id += 1;
        let id = self.next_id;
        let cancel = Arc::new(AtomicBool::new(false));
        self.worker
            .send(Run {
                id,
                request,
                runtime,
                environment: self.environment(),
                cancel: cancel.clone(),
            })
            .map_err(|_| "The execution worker is not running.".to_owned())?;
        self.running.insert(id, cancel);
        Ok(id)
    }

    pub fn cancel(&mut self, id: u64) {
        if let Some(flag) = self.running.remove(&id) {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Downloads and verifies the pinned Java runtime and Clojure.
    pub fn install(&mut self) {
        if matches!(self.install, InstallState::Running { .. }) {
            return;
        }
        let Ok((dirs, manifest)) = self.env.clone() else {
            return;
        };
        let cancel = Cancel::default();
        self.install_cancel = Some(cancel.clone());
        self.install = InstallState::Running {
            file: String::new(),
            progress: Progress {
                received: 0,
                total: 0,
            },
        };
        let (events, ctx) = (self.events.clone(), self.ctx.clone());
        let spawned = thread::Builder::new()
            .name("clojure-install".into())
            .spawn(move || {
                let emit = |e: ExecutionEvent| {
                    let _ = events.send(Event::Execution(Box::new(e)));
                    ctx.request_repaint();
                };
                let mut progress = |file: &str, progress: Progress| {
                    emit(ExecutionEvent::InstallProgress {
                        file: file.to_owned(),
                        progress,
                    });
                };
                let result =
                    components::install_jre(&manifest, &dirs, &platform_key(), &cancel, &mut progress)
                        .and_then(|_| components::install_clojure(&manifest, &dirs, &cancel, &mut progress))
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                emit(ExecutionEvent::Installed(result));
            });
        if let Err(e) = spawned {
            self.install = InstallState::Failed(e.to_string());
        }
    }

    pub fn cancel_install(&mut self) {
        if let Some(c) = &self.install_cancel {
            c.cancel();
        }
    }
}

impl Drop for ExecutionState {
    fn drop(&mut self) {
        for flag in self.running.values() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}
