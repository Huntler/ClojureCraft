//! Restricted execution of untrusted programs.
//!
//! The parent ([`run`]) starts the `clojurecraft-sandbox` helper with a [`Policy`]. The helper
//! restricts itself and then replaces itself with the target program, so the restrictions are
//! inherited and cannot be removed:
//!
//! * **Linux**: resource limits, `no_new_privs`, a Landlock ruleset that only grants read access
//!   to listed paths (and execute access to listed runtime directories), and a seccomp filter
//!   that refuses socket creation, process creation (`fork`, `vfork`, non-thread `clone`,
//!   `clone3`), tracing, namespaces, and other kernel interfaces untrusted code never needs.
//! * **macOS**: resource limits, then `sandbox-exec` with a deny-by-default profile generated
//!   from the same policy.
//!
//! The parent additionally enforces a wall-clock timeout, caps captured output, clears the
//! environment, and kills the child when any limit is exceeded.

mod policy;
mod runner;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

pub use policy::Policy;
pub use runner::{Outcome, RunError, RunRequest, Termination, run};

/// Name of the helper executable, expected next to the application binary.
pub const HELPER_NAME: &str = "clojurecraft-sandbox";

/// Exit status the helper uses when it could not establish the sandbox.
pub const SETUP_FAILED_EXIT: i32 = 97;
