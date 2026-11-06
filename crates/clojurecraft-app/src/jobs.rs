//! Background work reports typed events over one channel; the UI drains it each frame and never
//! blocks on a job.

use std::sync::mpsc::{Receiver, Sender, channel};

use crate::execution::ExecutionEvent;
use crate::inference::InferenceEvent;
use crate::knowledge::KnowledgeEvent;
use crate::library::LibraryEvent;

pub enum Event {
    Library(LibraryEvent),
    Inference(Box<InferenceEvent>),
    Knowledge(Box<KnowledgeEvent>),
    Execution(Box<ExecutionEvent>),
}

pub struct Jobs {
    tx: Sender<Event>,
    rx: Receiver<Event>,
}

impl Jobs {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx }
    }

    pub fn drain(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// A sender for background services that report through the same channel.
    pub fn sender(&self) -> Sender<Event> {
        self.tx.clone()
    }
}
