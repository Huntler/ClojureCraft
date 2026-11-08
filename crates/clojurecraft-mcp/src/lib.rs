//! A Model Context Protocol server that lets an outside agent use ClojureCraft as a tool: the
//! agent specifies a function (title, description, steps, examples, contract), reviews test cases,
//! starts generation, and reads back checked code, while writing, running, and repairing the code
//! happens inside the application with its local model.
//!
//! - [`tools`]: the tool catalogue and typed tool calls;
//! - [`edit`]: applying an agent's specification and test-case review to a function;
//! - [`export`]: writing functions as a Clojure project with tests and `deps.edn`;
//! - [`views`]: compact JSON views of functions, progress, and results for the agent;
//! - [`protocol`]: JSON-RPC 2.0 / MCP message handling over newline-delimited JSON;
//! - [`server`]: the application side, a private Unix socket whose tool calls are handed to the
//!   application's frame loop;
//! - [`bridge`]: `clojurecraft mcp`, the stdio process an MCP client launches, which relays to
//!   that socket (starting the application when it is not running).
//!
//! The server lives in the running application rather than in a separate headless process so
//! that the agent and the person share one library, one loaded model, and one job queue, and the
//! person can watch and intervene.

pub mod bridge;
pub mod edit;
pub mod export;
pub mod protocol;
pub mod server;
pub mod tools;
pub mod views;

pub use protocol::Handler;
pub use server::{Request, Server};
pub use tools::Call;

/// What a tool call returns: a JSON value for the agent, or a message explaining the failure.
pub type Reply = Result<serde_json::Value, String>;
