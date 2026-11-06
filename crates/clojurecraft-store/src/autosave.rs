//! Background autosave. Edits are submitted with their edit sequence; the worker debounces them
//! per function and writes the newest one on its own connection, so the UI thread never waits on
//! disk. Transient failures are retried with back-off; every outcome is reported.
//!
//! Recording a revision goes through the same worker, so it is ordered with the draft writes and a
//! queued older draft can never be written after the revision.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clojurecraft_domain::{FunctionDocument, FunctionId, RevisionReason};

use crate::{CommitOutcome, SaveOutcome, Store};

const MAX_ATTEMPTS: u32 = 5;
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutosaveEvent {
    Saved {
        function: FunctionId,
        seq: u64,
    },
    /// A newer edit was already stored (for example by a revision restore); nothing was written.
    Superseded {
        function: FunctionId,
        seq: u64,
        stored_seq: u64,
    },
    Failed {
        function: FunctionId,
        seq: u64,
        error: String,
        will_retry: bool,
    },
    /// The draft was saved and recorded as revision `number` (`created` is false when it equalled
    /// the latest revision, which is then reused).
    Committed {
        function: FunctionId,
        seq: u64,
        number: u32,
        created: bool,
    },
    /// The revision could not be recorded; the draft is still saved as an ordinary edit.
    CommitFailed {
        function: FunctionId,
        seq: u64,
        error: String,
    },
}

impl AutosaveEvent {
    pub fn function(&self) -> FunctionId {
        match self {
            Self::Saved { function, .. }
            | Self::Superseded { function, .. }
            | Self::Failed { function, .. }
            | Self::Committed { function, .. }
            | Self::CommitFailed { function, .. } => *function,
        }
    }
}

enum Message {
    Submit(Box<FunctionDocument>, u64),
    Commit(Box<FunctionDocument>, u64, RevisionReason),
    Flush(Sender<bool>),
}

struct Pending {
    document: Box<FunctionDocument>,
    seq: u64,
    first_submitted: Instant,
    due: Instant,
    attempts: u32,
}

pub struct Autosaver {
    tx: Option<Sender<Message>>,
    worker: Option<JoinHandle<()>>,
}

impl Autosaver {
    /// Starts the worker. `delay` is the quiet period after the last edit before writing; while
    /// edits keep arriving, a write still happens at least every four `delay`s.
    pub fn spawn(store: Store, delay: Duration, notify: impl Fn(AutosaveEvent) + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let worker = std::thread::Builder::new()
            .name("autosave".into())
            .spawn(move || {
                Worker {
                    store,
                    delay,
                    notify: Box::new(notify),
                    pending: HashMap::new(),
                }
                .run(&rx)
            })
            .ok();
        Self {
            tx: worker.is_some().then_some(tx),
            worker,
        }
    }

    /// Queues the draft for saving. Older sequences than one already queued are ignored.
    pub fn submit(&self, document: FunctionDocument, seq: u64) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Message::Submit(Box::new(document), seq));
        }
    }

    /// Saves the draft and records it as a revision, after any older queued edit of the same
    /// function and replacing it.
    pub fn commit(&self, document: FunctionDocument, seq: u64, reason: RevisionReason) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Message::Commit(Box::new(document), seq, reason));
        }
    }

    /// Writes everything queued now and waits for it. Returns whether all writes succeeded.
    pub fn flush(&self) -> bool {
        let (reply, done) = channel();
        self.tx
            .as_ref()
            .is_some_and(|tx| tx.send(Message::Flush(reply)).is_ok())
            && done.recv().unwrap_or(false)
    }
}

impl Drop for Autosaver {
    /// Writes outstanding edits before returning.
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Worker {
    store: Store,
    delay: Duration,
    notify: Box<dyn Fn(AutosaveEvent) + Send>,
    pending: HashMap<FunctionId, Pending>,
}

impl Worker {
    fn run(mut self, rx: &Receiver<Message>) {
        loop {
            let next_due = self.pending.values().map(|p| p.due).min();
            let message = match next_due {
                Some(due) => rx.recv_timeout(due.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match message {
                Ok(Message::Submit(document, seq)) => self.queue(document, seq),
                Ok(Message::Commit(document, seq, reason)) => self.commit(document, seq, reason),
                Ok(Message::Flush(reply)) => {
                    let ok = self.write_all();
                    let _ = reply.send(ok);
                }
                Err(RecvTimeoutError::Timeout) => self.write_due(),
                Err(RecvTimeoutError::Disconnected) => {
                    self.write_all();
                    return;
                }
            }
        }
    }

    fn queue(&mut self, document: Box<FunctionDocument>, seq: u64) {
        let now = Instant::now();
        let max_wait = self.delay * 4;
        match self.pending.get_mut(&document.id) {
            Some(p) if seq < p.seq => {}
            Some(p) => {
                p.document = document;
                p.seq = seq;
                p.attempts = 0;
                p.due = (now + self.delay).min(p.first_submitted + max_wait);
            }
            None => {
                self.pending.insert(
                    document.id,
                    Pending {
                        document,
                        seq,
                        first_submitted: now,
                        due: now + self.delay,
                        attempts: 0,
                    },
                );
            }
        }
    }

    fn commit(&mut self, document: Box<FunctionDocument>, seq: u64, reason: RevisionReason) {
        let id = document.id;
        // A queued older edit is replaced by this one; a newer one stays queued and is written
        // after the revision.
        if self.pending.get(&id).is_some_and(|p| p.seq <= seq) {
            self.pending.remove(&id);
        }
        let event = match self.store.commit_revision(&document, seq, reason) {
            Ok(outcome) => AutosaveEvent::Committed {
                function: id,
                seq,
                number: outcome.revision().number,
                created: matches!(outcome, CommitOutcome::Created(_)),
            },
            Err(e) => {
                tracing::warn!(function = %id, error = %e, "recording a revision failed");
                // The content must still reach the draft.
                self.queue(document, seq);
                AutosaveEvent::CommitFailed {
                    function: id,
                    seq,
                    error: e.to_string(),
                }
            }
        };
        (self.notify)(event);
    }

    fn write_due(&mut self) {
        let now = Instant::now();
        let due: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.write(id);
        }
    }

    fn write_all(&mut self) -> bool {
        let ids: Vec<_> = self.pending.keys().copied().collect();
        // Attempt every write even after a failure.
        let mut all_ok = true;
        for id in ids {
            all_ok &= self.write(id);
        }
        all_ok
    }

    fn write(&mut self, id: FunctionId) -> bool {
        let Some(mut p) = self.pending.remove(&id) else {
            return true;
        };
        p.attempts += 1;
        let (event, ok) = match self.store.save_draft(&p.document, p.seq) {
            Ok(SaveOutcome::Saved) => (
                AutosaveEvent::Saved {
                    function: id,
                    seq: p.seq,
                },
                true,
            ),
            Ok(SaveOutcome::Superseded { stored_seq }) => (
                AutosaveEvent::Superseded {
                    function: id,
                    seq: p.seq,
                    stored_seq,
                },
                true,
            ),
            Err(e) => {
                let will_retry = e.is_transient() && p.attempts < MAX_ATTEMPTS;
                tracing::warn!(function = %id, error = %e, will_retry, "autosave failed");
                let event = AutosaveEvent::Failed {
                    function: id,
                    seq: p.seq,
                    error: e.to_string(),
                    will_retry,
                };
                if will_retry {
                    p.due = Instant::now() + RETRY_DELAY * p.attempts;
                    self.pending.insert(id, p);
                }
                (event, false)
            }
        };
        (self.notify)(event);
        ok
    }
}
