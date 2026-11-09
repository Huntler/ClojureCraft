//! The Seatbelt profile with ordinary system programs: granted programs start, files the policy
//! does not grant stay unreadable.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::Duration;

use clojurecraft_sandbox::{Outcome, Policy, RunRequest, Termination, run};

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
        write: Vec::new(),
    }
}

fn exec(program: &str, args: &[&str], policy: Policy) -> Outcome {
    run(&RunRequest {
        helper: PathBuf::from(env!("CARGO_BIN_EXE_clojurecraft-sandbox")),
        policy,
        program: program.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        env: vec![],
        stdin: vec![],
        wall_timeout: Duration::from_secs(5),
        max_stdout: 64 * 1024,
        max_stderr: 64 * 1024,
        cancel: None,
    })
    .expect("run")
}

#[test]
fn granted_program_starts() {
    let out = exec("/bin/echo", &["hi"], policy(vec![]));
    assert_eq!(
        out.termination,
        Termination::Exited(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"hi\n");
}

#[test]
fn granted_file_is_readable_but_others_are_not() {
    let dir = tempfile::tempdir().expect("tmp");
    let allowed = dir.path().join("allowed.txt");
    std::fs::write(&allowed, "visible").expect("write");
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "hidden").expect("write");

    let out = exec(
        "/bin/cat",
        &[&allowed.to_string_lossy()],
        policy(vec![allowed.clone()]),
    );
    assert_eq!(
        out.termination,
        Termination::Exited(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"visible");

    let out = exec("/bin/cat", &[&secret.to_string_lossy()], policy(vec![allowed]));
    assert_ne!(out.termination, Termination::Exited(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn only_granted_directories_are_writable() {
    let dir = tempfile::tempdir().expect("tmp");
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&scratch).expect("mkdir");
    let mut p = policy(vec![]);
    p.write = vec![scratch.clone()];
    let inside = scratch.join("lib.bin");
    let out = exec("/bin/cp", &["/bin/echo", &inside.to_string_lossy()], p.clone());
    assert_eq!(
        out.termination,
        Termination::Exited(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::metadata(&inside).map(|m| m.len()).ok(),
        std::fs::metadata("/bin/echo").map(|m| m.len()).ok(),
        "a native library is written whole"
    );

    let outside = dir.path().join("escape.bin");
    let out = exec("/bin/cp", &["/bin/echo", &outside.to_string_lossy()], p);
    assert_ne!(out.termination, Termination::Exited(0));
    assert!(!outside.exists());
}
