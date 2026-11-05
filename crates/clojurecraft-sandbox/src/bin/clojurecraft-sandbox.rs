//! Sandbox helper: `clojurecraft-sandbox <policy-json> <program> [args...]`.
//!
//! Restricts itself according to the policy, then replaces itself with `program`. If any
//! restriction cannot be established it exits with a dedicated status instead of running the
//! program unrestricted.

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use clojurecraft_sandbox::{Policy, SETUP_FAILED_EXIT};

fn fail(msg: &str) -> ExitCode {
    eprintln!("clojurecraft-sandbox: {msg}");
    ExitCode::from(u8::try_from(SETUP_FAILED_EXIT).unwrap_or(1))
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(policy), Some(program)) = (args.next(), args.next()) else {
        return fail("usage: clojurecraft-sandbox <policy-json> <program> [args...]");
    };
    let rest: Vec<_> = args.collect();
    let policy = match Policy::from_arg(&policy.to_string_lossy()) {
        Ok(p) => p,
        Err(e) => return fail(&format!("invalid policy: {e}")),
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        if let Err(e) = clojurecraft_sandbox::linux::apply(&policy) {
            return fail(&e.to_string());
        }
        let mut c = Command::new(&program);
        c.args(&rest);
        c
    };

    #[cfg(target_os = "macos")]
    let mut cmd = {
        use clojurecraft_sandbox::macos;
        if let Err(e) = macos::apply_rlimits(&policy) {
            return fail(&e.to_string());
        }
        let mut c = Command::new(macos::SANDBOX_EXEC);
        c.arg("-p").arg(macos::profile(&policy)).arg(&program).args(&rest);
        c
    };

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut cmd: Command = {
        let _ = (&policy, &program, &rest);
        return fail("no sandbox implementation for this operating system");
    };

    let err = cmd.exec();
    fail(&format!("could not start {}: {err}", program.to_string_lossy()))
}
