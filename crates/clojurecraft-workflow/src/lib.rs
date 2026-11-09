//! From specification to generated code.
//!
//! This crate turns a function's specification into model requests and the model's structured
//! answers back into domain values, without performing any I/O itself:
//!
//! - [`spec`]: whether a specification is complete enough, and the specification as required
//!   prompt sections (nothing the user wrote is ever shortened or dropped);
//! - [`requests`]: interpretation, clarification questions, retrieval planning, contract
//!   proposals, generation, and explanation, each with its own role, task, and answer schema;
//! - [`answers`]: answer schemas and conversion of validated answers into domain values;
//! - [`structure`]: structural checks of generated source against the evaluation harness's rules,
//!   made before the code is executed;
//! - [`deps`]: which namespaces a function may require, and explicit approval of more;
//! - [`candidate`]: a checked generation, its provenance, and when it may replace current code;
//! - [`validation`]: the automatic validation sequence (parse, structure, lint, load, examples,
//!   tests, contract) and its report;
//! - [`edge_cases`]: proposing edge-case tests from the specification, reviewing them, and
//!   fixing code that fails accepted cases;
//! - [`compare`]: new code compared with the code it would replace, on variants of the
//!   examples;
//! - [`describe`]: the specification of existing code, written by the model, with example
//!   outputs recorded by running the code;
//! - [`dataset`]: a fine-tuning dataset of generation prompts and validated code;
//! - [`export`]: a Clojure project (sources, `clojure.test` files, `deps.edn`) made from saved
//!   functions;
//! - [`diff`]: line differences for reviewing replacement code;
//! - [`references`]: example inputs taken from another function's executed output (planning
//!   the upstream run, cycle and contract checks, recording the snapshot);
//! - [`intermediate`]: executed intermediate values of transformation steps, observed with an
//!   instrumented copy of the code;
//! - [`project`]: functions inside a project's namespace files: the fragment the model edits,
//!   and the whole file it is checked in;
//! - [`project_index`]: the project's code (and its dependencies' sources) as searchable
//!   chunks, which of them are pure, and which namespaces may be required without a cycle;
//! - [`assistant`]: the sequence of requests for interpreting, proposing a contract, and
//!   generating, reaching inference and documentation only through [`assistant::Ports`].

pub mod answers;
pub mod assistant;
pub mod candidate;
pub mod cleanup;
pub mod compare;
pub mod dataset;
pub mod delimiters;
pub mod deps;
pub mod describe;
pub mod diff;
pub mod edge_cases;
pub mod edits;
pub mod eval_tool;
pub mod export;
pub mod grammar;
pub mod idioms;
mod insert;
pub mod intermediate;
pub mod locals;
pub mod locate;
pub mod naming;
pub mod project;
pub mod project_index;
pub mod references;
pub mod requests;
pub mod spec;
pub mod structure;
pub mod validation;

pub use candidate::{Candidate, Provenance, implementation, replaces_silently};
pub use requests::{GenerationInput, Names};
pub use spec::{Readiness, readiness};
pub use structure::{CodeStructure, Problem, ProblemKind};
