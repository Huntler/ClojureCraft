//! The function library service. A worker thread owns a store connection and performs library
//! operations in order; draft edits go through a separate [`Autosaver`] with its own connection.
//! Results come back as [`LibraryEvent`]s, so the UI never waits on disk.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use clojurecraft_domain::format::FormatError;
use clojurecraft_domain::{FunctionDocument, FunctionId, RevisionId, SecretRegistry, ValidationReport};
use clojurecraft_runtime::DataDirs;
use clojurecraft_store::{
    AutosaveEvent, Autosaver, DATABASE_FILE, Draft, FunctionSummary, Listing, Revision, RevisionSummary,
    Store, StoreError,
};

use crate::jobs::Event;

/// Quiet period after the last keystroke before a draft is written.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(600);

pub enum Command {
    Open(FunctionId),
    Create(String),
    /// Stores a complete document as a new function (duplicating an open function).
    Insert(Box<FunctionDocument>),
    /// Duplicates the stored draft of a function that is not open.
    Duplicate {
        id: FunctionId,
        name: String,
    },
    Trash {
        id: FunctionId,
        name: String,
    },
    Restore(FunctionId),
    Purge(FunctionId),
    /// Replaces an unreadable draft with the latest revision.
    Recover(FunctionId),
    /// Appends a validation report.
    RecordValidation(FunctionId, Box<ValidationReport>),
    /// Reads a function's validation reports, newest first.
    LoadValidations(FunctionId),
    /// Lists a function's revisions, newest first.
    LoadRevisions(FunctionId),
    /// Reads one revision with its content.
    LoadRevision(RevisionId),
}

pub enum LibraryEvent {
    Ready(Autosaver),
    Unavailable(String),
    Listed {
        active: Vec<FunctionSummary>,
        trash: Vec<FunctionSummary>,
    },
    Opened {
        draft: Box<Draft>,
        /// Newly created (rather than an existing function opened).
        created: bool,
    },
    OpenFailed {
        id: FunctionId,
        error: String,
        /// The draft is unreadable and the latest revision can replace it.
        recoverable: bool,
    },
    Trashed {
        id: FunctionId,
        name: String,
    },
    Purged(FunctionId),
    Failed {
        action: &'static str,
        error: String,
    },
    Autosave(AutosaveEvent),
    /// A function's stored validation reports, newest first.
    Validations {
        function: FunctionId,
        reports: Vec<ValidationReport>,
    },
    /// A function's revisions, newest first.
    Revisions {
        function: FunctionId,
        revisions: Vec<RevisionSummary>,
    },
    Revision(Box<Revision>),
}

/// Validation reports kept per function for display; older ones stay in the library.
pub const VALIDATIONS_SHOWN: usize = 40;

pub struct Library {
    tx: Sender<Command>,
}

impl Library {
    /// Opens (creating or migrating as needed) the library on a worker thread.
    pub fn start(events: Sender<Event>, ctx: egui::Context, secrets: Arc<SecretRegistry>) -> Self {
        let (tx, rx) = channel();
        let spawned = thread::Builder::new()
            .name("library".into())
            .spawn(move || run(&rx, &events, &ctx, secrets));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "could not start the library worker");
        }
        Self { tx }
    }

    pub fn send(&self, command: Command) {
        if self.tx.send(command).is_err() {
            tracing::error!("the library worker has stopped");
        }
    }
}

fn run(rx: &Receiver<Command>, events: &Sender<Event>, ctx: &egui::Context, secrets: Arc<SecretRegistry>) {
    let emit = |event: LibraryEvent| {
        let _ = events.send(Event::Library(event));
        ctx.request_repaint();
    };
    let mut store = match open(&emit, events, ctx, secrets) {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(error = %e, "could not open the function library");
            emit(LibraryEvent::Unavailable(e));
            return;
        }
    };
    list(&store, &emit);
    for command in rx {
        match command {
            Command::RecordValidation(function, report) => {
                if let Err(e) = store.record_validation(function, &report) {
                    emit(failed("store the validation results", &e));
                }
                continue;
            }
            Command::LoadValidations(function) => {
                match store.validation_reports(function) {
                    Ok(mut reports) => {
                        reports.truncate(VALIDATIONS_SHOWN);
                        emit(LibraryEvent::Validations { function, reports });
                    }
                    Err(e) => emit(failed("read the validation results", &e)),
                }
                continue;
            }
            Command::LoadRevisions(function) => {
                match store.revisions(function) {
                    Ok(revisions) => emit(LibraryEvent::Revisions { function, revisions }),
                    Err(e) => emit(failed("read the history", &e)),
                }
                continue;
            }
            Command::LoadRevision(id) => {
                match store.load_revision(id) {
                    Ok(revision) => emit(LibraryEvent::Revision(Box::new(revision))),
                    Err(e) => emit(failed("read the revision", &e)),
                }
                continue;
            }
            Command::Open(id) => match store.load_draft(id) {
                Ok(draft) => emit(LibraryEvent::Opened {
                    draft: Box::new(draft),
                    created: false,
                }),
                Err(e) => emit(open_failed(id, &e)),
            },
            Command::Create(name) => match store.create_function(&name) {
                Ok(draft) => emit(LibraryEvent::Opened {
                    draft: Box::new(draft),
                    created: true,
                }),
                Err(e) => emit(failed("create the function", &e)),
            },
            Command::Insert(document) => match store.insert_function(*document) {
                Ok(draft) => emit(LibraryEvent::Opened {
                    draft: Box::new(draft),
                    created: false,
                }),
                Err(e) => emit(failed("duplicate the function", &e)),
            },
            Command::Duplicate { id, name } => match store.duplicate_function(id, &name) {
                Ok(draft) => emit(LibraryEvent::Opened {
                    draft: Box::new(draft),
                    created: false,
                }),
                Err(e) => emit(failed("duplicate the function", &e)),
            },
            Command::Trash { id, name } => match store.trash_function(id) {
                Ok(()) => emit(LibraryEvent::Trashed { id, name }),
                Err(e) => emit(failed("move the function to the trash", &e)),
            },
            Command::Restore(id) => {
                if let Err(e) = store.restore_function(id) {
                    emit(failed("restore the function", &e));
                }
            }
            Command::Purge(id) => match store.purge_function(id) {
                Ok(()) => emit(LibraryEvent::Purged(id)),
                Err(e) => emit(failed("delete the function permanently", &e)),
            },
            Command::Recover(id) => match store.recover_draft(id) {
                Ok(draft) => emit(LibraryEvent::Opened {
                    draft: Box::new(draft),
                    created: false,
                }),
                Err(e) => emit(failed("recover the function from its history", &e)),
            },
        }
        list(&store, &emit);
    }
}

fn open(
    emit: &impl Fn(LibraryEvent),
    events: &Sender<Event>,
    ctx: &egui::Context,
    secrets: Arc<SecretRegistry>,
) -> Result<Store, String> {
    let dirs = DataDirs::discover().ok_or("no data directory is available for this user")?;
    let dir = dirs.library();
    let (store, migration) = Store::open_dir(&dir, secrets.clone()).map_err(|e| e.to_string())?;
    if let Some(backup) = migration.backup {
        tracing::info!(backup = %backup.display(), "library upgraded; previous version kept");
    }
    let (autosave_store, _) = Store::open(&dir.join(DATABASE_FILE), secrets).map_err(|e| e.to_string())?;
    let (events, ctx) = (events.clone(), ctx.clone());
    let autosaver = Autosaver::spawn(autosave_store, AUTOSAVE_DELAY, move |event| {
        let _ = events.send(Event::Library(LibraryEvent::Autosave(event)));
        ctx.request_repaint();
    });
    emit(LibraryEvent::Ready(autosaver));
    Ok(store)
}

fn list(store: &Store, emit: &impl Fn(LibraryEvent)) {
    let listed = store
        .list_functions(Listing::Active, "")
        .and_then(|active| Ok((active, store.list_functions(Listing::Trash, "")?)));
    match listed {
        Ok((active, trash)) => emit(LibraryEvent::Listed { active, trash }),
        Err(e) => emit(failed("list saved functions", &e)),
    }
}

fn failed(action: &'static str, error: &StoreError) -> LibraryEvent {
    tracing::warn!(action, error = %error, "library operation failed");
    LibraryEvent::Failed {
        action,
        error: error.to_string(),
    }
}

fn open_failed(id: FunctionId, error: &StoreError) -> LibraryEvent {
    let recoverable = matches!(
        error,
        StoreError::Format {
            source: FormatError::Malformed { .. },
            ..
        } | StoreError::Integrity(_)
    );
    tracing::warn!(function = %id, error = %error, "could not open function");
    LibraryEvent::OpenFailed {
        id,
        error: error.to_string(),
        recoverable,
    }
}
