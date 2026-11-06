//! Parent side: starts the helper, feeds stdin, captures bounded output, and enforces the
//! wall-clock limit.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Policy, SETUP_FAILED_EXIT};

#[derive(Clone, Debug)]
pub struct RunRequest {
    /// Path of the `clojurecraft-sandbox` helper executable.
    pub helper: PathBuf,
    pub policy: Policy,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Environment for the program; everything else from the parent is removed.
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
    pub wall_timeout: Duration,
    pub max_stdout: usize,
    pub max_stderr: usize,
    /// Set by another thread to stop the program early.
    pub cancel: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Termination {
    Exited(i32),
    /// Killed by a signal not sent by the runner (e.g. `SIGXCPU`/`SIGKILL` from the CPU limit).
    Signaled(i32),
    TimedOut,
    OutputLimitExceeded,
    /// Stopped through [`RunRequest::cancel`].
    Cancelled,
    /// The helper could not establish the sandbox; the program never ran.
    SandboxSetupFailed,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub termination: Termination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("could not start the sandbox helper {path}: {source}")]
    Spawn { path: PathBuf, source: std::io::Error },
    #[error("waiting for the sandboxed process failed: {0}")]
    Wait(std::io::Error),
}

/// Reads up to `cap` bytes; sets `overflow` and stops reading if more arrive.
fn capped_reader(
    mut src: impl Read + Send + 'static,
    cap: usize,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(out.len());
                    out.extend_from_slice(&buf[..n.min(room)]);
                    if n > room {
                        overflow.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        out
    })
}

pub fn run(req: &RunRequest) -> Result<Outcome, RunError> {
    let mut cmd = Command::new(&req.helper);
    cmd.arg(req.policy.to_arg())
        .arg(&req.program)
        .args(&req.args)
        .env_clear()
        .envs(req.env.iter().map(|(k, v)| (k, v)))
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Keep terminal signals aimed at the application away from the child.
        cmd.process_group(0);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
        path: req.helper.clone(),
        source,
    })?;

    let overflow = Arc::new(AtomicBool::new(false));
    let stdout = capped_reader(
        child.stdout.take().expect("piped stdout"),
        req.max_stdout,
        overflow.clone(),
    );
    let stderr = capped_reader(
        child.stderr.take().expect("piped stderr"),
        req.max_stderr,
        overflow.clone(),
    );
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = req.stdin.clone();
    let writer = thread::spawn(move || {
        // A child that exits early closes the pipe; that is not an error for the runner.
        let _ = stdin.write_all(&input);
    });

    let deadline = start + req.wall_timeout;
    let termination = loop {
        if let Some(status) = child.try_wait().map_err(RunError::Wait)? {
            break classify(status);
        }
        if overflow.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            break Termination::OutputLimitExceeded;
        }
        if req.cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
            let _ = child.kill();
            let _ = child.wait();
            break Termination::Cancelled;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Termination::TimedOut;
        }
        thread::sleep(Duration::from_millis(5));
    };
    let elapsed = start.elapsed();
    let _ = writer.join();
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    // Output that overflowed after the process exited still counts as a violation.
    let termination = match termination {
        _ if overflow.load(Ordering::SeqCst) => Termination::OutputLimitExceeded,
        // The program itself might exit with the same status; the helper's message disambiguates.
        Termination::Exited(SETUP_FAILED_EXIT) if stderr.starts_with(b"clojurecraft-sandbox:") => {
            Termination::SandboxSetupFailed
        }
        t => t,
    };
    Ok(Outcome {
        termination,
        stdout,
        stderr,
        elapsed,
    })
}

fn classify(status: std::process::ExitStatus) -> Termination {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Termination::Signaled(sig);
        }
    }
    Termination::Exited(status.code().unwrap_or(-1))
}
