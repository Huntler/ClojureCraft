//! macOS restriction: resource limits plus a Seatbelt profile run through `sandbox-exec`.
//!
//! `sandbox-exec` is deprecated as a public interface but remains the mechanism Apple's own tools
//! and several browsers use. The profile is deny-by-default and generated from the [`Policy`].

use std::fmt::Write as _;
use std::path::Path;

use rustix::process::{Resource, Rlimit, setrlimit};

use crate::Policy;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("setting resource limit {0}: {1}")]
    Rlimit(&'static str, rustix::io::Errno),
    #[error("{0} is missing")]
    SandboxExecMissing(&'static str),
}

fn limit(resource: Resource, name: &'static str, cur: u64, max: u64) -> Result<(), SetupError> {
    setrlimit(
        resource,
        Rlimit {
            current: Some(cur),
            maximum: Some(max),
        },
    )
    .map_err(|e| SetupError::Rlimit(name, e))
}

pub fn apply_rlimits(policy: &Policy) -> Result<(), SetupError> {
    limit(Resource::Cpu, "CPU", policy.cpu_seconds, policy.cpu_seconds + 1)?;
    limit(Resource::Core, "CORE", 0, 0)?;
    limit(Resource::Fsize, "FSIZE", 0, 0)?;
    limit(
        Resource::Nofile,
        "NOFILE",
        policy.max_open_files,
        policy.max_open_files,
    )?;
    if let Some(bytes) = policy.address_space_bytes {
        limit(Resource::As, "AS", bytes, bytes)?;
    }
    if !Path::new(SANDBOX_EXEC).exists() {
        return Err(SetupError::SandboxExecMissing(SANDBOX_EXEC));
    }
    Ok(())
}

fn quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Builds the Seatbelt profile for `policy`.
pub fn profile(policy: &Policy) -> String {
    let mut p = String::from(
        "(version 1)\n(deny default)\n(allow process-exec*)\n(deny process-fork)\n(deny network*)\n\
         (allow file-read-metadata)\n(allow sysctl-read)\n(allow signal (target self))\n\
         (allow file-read* file-write-data (literal \"/dev/null\"))\n",
    );
    for dir in &policy.exec {
        let _ = writeln!(
            p,
            "(allow file-read* file-map-executable (subpath {}))",
            quote(dir)
        );
    }
    for path in &policy.read {
        let kind = if path.is_dir() { "subpath" } else { "literal" };
        let _ = writeln!(p, "(allow file-read* ({kind} {}))", quote(path));
    }
    // The JVM needs to look up a few system services to start.
    p.push_str("(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n");
    p
}
