//! Local inference for ClojureCraft.
//!
//! [`InferenceService`] owns one loaded model on a worker thread and reports what happens as
//! typed events. Models run through the [`Engine`]/[`Session`] interface; [`LlamaEngine`] is the
//! production implementation, which runs llama.cpp's `llama-server` as a child process reachable
//! only by this application. Prompts are assembled from labelled [`Section`]s under an explicit
//! context policy, answers can be constrained to a [`Schema`], and the model may call only the
//! application-defined [`Tool`]s it is given.

pub mod context;
pub mod engine;
pub mod error;
pub mod http;
mod logscan;
pub mod process;
pub mod protocol;
pub mod roles;
pub mod schema;
pub mod service;
pub mod tools;

#[cfg(any(test, feature = "mock"))]
pub mod mock;
#[cfg(test)]
mod service_tests;

pub use context::{Placement, Priority, PromptPlan, Section};
pub use engine::{Engine, Health, LlamaEngine, LoadPhase, LoadSpec, MemoryNeed, Session, SessionInfo};
pub use error::{GenerationError, LoadError, LoadShape, MemoryKind, Remedy};
pub use http::Cancel;
pub use protocol::{Finish, Message, StreamEvent, ToolCall, Usage};
pub use roles::Role;
pub use schema::{Field, Schema};
pub use service::{
    GenerateRequest, Generation, InferenceService, MemoryAvailable, MemoryProbe, ModelStatus, RequestId,
    ServiceEvent,
};
pub use tools::{Tool, ToolContext, ToolLimits, ToolOutcome, ToolSet};

use std::path::Path;
use std::process::Command;

/// Lists the devices a llama.cpp build can use (`llama-server --list-devices`).
pub fn list_devices(server: &Path) -> Result<String, LoadError> {
    let out = Command::new(server)
        .arg("--list-devices")
        .output()
        .map_err(|e| LoadError::Spawn(format!("{}: {e}", server.display())))?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}
