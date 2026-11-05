//! Demonstrates the OS-level boundary with ordinary system programs, independent of any
//! language-level checks: what the policy does not grant must fail.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use clojurecraft_sandbox::{Outcome, Policy, RunRequest, Termination, run};

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_clojurecraft-sandbox"))
}

fn policy(read: Vec<PathBuf>) -> Policy {
    let mut exec = Policy::system_library_dirs();
    exec.extend(["/bin", "/usr/bin"].map(PathBuf::from));
    let mut r = Policy::system_read_files();
    r.extend(read);
    Policy {
        read: r,
        exec,
        cpu_seconds: 5,
        address_space_bytes: None,
        max_open_files: 64,
    }
}

fn sh(script: &str, policy: Policy) -> Outcome {
    run(&RunRequest {
        helper: helper(),
        policy,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        stdin: vec![],
        wall_timeout: Duration::from_secs(5),
        max_stdout: 64 * 1024,
        max_stderr: 64 * 1024,
    })
    .expect("run")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn granted_file_is_readable_but_others_are_not() {
    let dir = tempfile::tempdir().expect("tmp");
    let allowed = dir.path().join("allowed.txt");
    std::fs::write(&allowed, "visible").expect("write");
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "hidden").expect("write");

    let out = sh(
        &format!("exec cat {}", allowed.display()),
        policy(vec![allowed.clone()]),
    );
    assert_eq!(out.termination, Termination::Exited(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "visible");

    let out = sh(&format!("exec cat {}", secret.display()), policy(vec![allowed]));
    assert_ne!(out.termination, Termination::Exited(0));
    assert!(!text(&out.stdout).contains("hidden"));
    assert!(
        text(&out.stderr).contains("Permission denied"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn home_directory_and_etc_are_not_readable() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let out = sh(&format!("exec ls {home}"), policy(vec![]));
    assert_ne!(out.termination, Termination::Exited(0));
    assert!(
        text(&out.stderr).contains("Permission denied"),
        "{}",
        text(&out.stderr)
    );
    let out = sh("exec cat /etc/hostname", policy(vec![]));
    assert_ne!(out.termination, Termination::Exited(0));
}

#[test]
fn files_cannot_be_written() {
    let dir = tempfile::tempdir().expect("tmp");
    let target = dir.path().join("written.txt");
    let out = sh(
        &format!("echo data > {}", target.display()),
        policy(vec![dir.path().to_owned()]),
    );
    assert_ne!(out.termination, Termination::Exited(0));
    assert!(!target.exists());
}

#[test]
fn processes_cannot_be_spawned() {
    // `sh` must fork to run a command that is not the last one; the fork is refused, so the
    // external `echo` never runs.
    let out = sh("/bin/echo child-ran; echo parent-done", policy(vec![]));
    assert!(!text(&out.stdout).contains("child-ran"), "a child process ran");
    assert!(
        text(&out.stderr).contains("Cannot fork"),
        "stderr: {}",
        text(&out.stderr)
    );
}

#[test]
fn network_connections_are_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    // bash opens a TCP socket for /dev/tcp redirections; fall back gracefully if bash is absent.
    let bash = ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let Some(bash) = bash else { return };
    let out = run(&RunRequest {
        helper: helper(),
        policy: policy(vec![]),
        program: bash.into(),
        args: vec![
            "-c".into(),
            format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected"),
        ],
        env: vec![],
        stdin: vec![],
        wall_timeout: Duration::from_secs(5),
        max_stdout: 4096,
        max_stderr: 4096,
    })
    .expect("run");
    assert!(!text(&out.stdout).contains("connected"), "{:?}", out);
    let accepted = listener.accept().is_ok();
    assert!(!accepted, "the sandboxed process reached the listener");
}

#[test]
fn runaway_loop_hits_wall_clock_timeout() {
    let mut p = policy(vec![]);
    p.cpu_seconds = 60;
    let started = std::time::Instant::now();
    let out = run(&RunRequest {
        helper: helper(),
        policy: p,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "while :; do :; done".into()],
        env: vec![],
        stdin: vec![],
        wall_timeout: Duration::from_millis(800),
        max_stdout: 1024,
        max_stderr: 1024,
    })
    .expect("run");
    assert_eq!(out.termination, Termination::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn cpu_limit_kills_busy_process() {
    let mut p = policy(vec![]);
    p.cpu_seconds = 1;
    let out = run(&RunRequest {
        helper: helper(),
        policy: p,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "while :; do :; done".into()],
        env: vec![],
        stdin: vec![],
        wall_timeout: Duration::from_secs(10),
        max_stdout: 1024,
        max_stderr: 1024,
    })
    .expect("run");
    assert!(
        matches!(out.termination, Termination::Signaled(_)),
        "{:?}",
        out.termination
    );
}

#[test]
fn excessive_output_is_cut_off() {
    let out = run(&RunRequest {
        helper: helper(),
        policy: policy(vec![]),
        program: "/usr/bin/yes".into(),
        args: vec![],
        env: vec![],
        stdin: vec![],
        wall_timeout: Duration::from_secs(5),
        max_stdout: 10_000,
        max_stderr: 1024,
    })
    .expect("run");
    assert_eq!(out.termination, Termination::OutputLimitExceeded);
    assert!(out.stdout.len() <= 10_000);
}

#[test]
fn environment_is_not_inherited() {
    // The test runner's environment contains HOME; the child must not see it.
    let out = sh("echo \"home=${HOME:-unset}\"", policy(vec![]));
    assert_eq!(text(&out.stdout).trim(), "home=unset");
}

#[test]
fn invalid_policy_never_runs_the_program() {
    let mut child = std::process::Command::new(helper())
        .args(["{not json", "/bin/echo", "ran"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut s = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut s)
        .expect("read");
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(clojurecraft_sandbox::SETUP_FAILED_EXIT));
    assert!(s.is_empty());
}

#[test]
fn seccomp_filter_compiles_for_this_architecture() {
    assert!(
        !clojurecraft_sandbox::linux::seccomp_program()
            .expect("program")
            .is_empty()
    );
}
