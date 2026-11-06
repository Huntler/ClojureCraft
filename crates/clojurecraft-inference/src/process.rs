//! The `llama-server` child process.
//!
//! The server listens on a Unix-domain socket inside a fresh directory only the current user can
//! enter, so neither other local users nor web pages can reach it; TCP on the loopback interface
//! is used only if the socket path would be too long. Requests also need a random per-session API
//! key, passed through the environment so it never appears in process listings. Environment
//! variables that llama.cpp reads as configuration are removed so they cannot enable features
//! such as its built-in file tools. The server's log is kept in a bounded buffer for diagnostics.

use std::collections::VecDeque;
use std::fs::{self, DirBuilder};
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::LoadError;
use crate::http::{self, Cancel, Control, Endpoint, HttpError, Response};
use crate::logscan::{self, Failure};

const LOG_LINES: usize = 600;
/// Longest Unix socket path accepted everywhere (macOS allows 104 bytes including the NUL).
const MAX_SOCKET_PATH: usize = 100;
const STOP_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    pub server: PathBuf,
    /// The model file; for split models, the first part.
    pub model: PathBuf,
    pub context: u32,
    /// Layers placed on the GPU; `None` places all of them there.
    pub gpu_layers: Option<u32>,
    pub threads: Option<u32>,
    /// Records the running server so one left behind by a crash of the application can be
    /// stopped on the next start.
    pub pid_file: Option<PathBuf>,
    /// Where the private socket directory is created; defaults to the per-user runtime directory.
    pub socket_base: Option<PathBuf>,
}

/// Command-line arguments for the server; `host` is the socket path or loopback address.
pub fn server_args(cfg: &ServerConfig, host: &str, port: Option<u16>) -> Vec<String> {
    let mut a: Vec<String> = vec!["--model".into(), cfg.model.to_string_lossy().into_owned()];
    a.extend(["--host".into(), host.to_owned()]);
    if let Some(p) = port {
        a.extend(["--port".into(), p.to_string()]);
    }
    a.extend(["--ctx-size".into(), cfg.context.to_string()]);
    a.extend([
        "--n-gpu-layers".into(),
        cfg.gpu_layers.map_or_else(|| "999".to_owned(), |n| n.to_string()),
    ]);
    if let Some(t) = cfg.threads {
        a.extend(["--threads".into(), t.to_string()]);
    }
    a.extend(
        [
            // One request at a time, with the whole context available to it.
            "--parallel",
            "1",
            // Use exactly the configured context and offload; report memory failures instead.
            "--fit",
            "off",
            // No host-side prompt cache beyond the slot (it would use RAM outside the budget).
            "--cache-ram",
            "0",
            // Never discard earlier context silently when the answer runs long.
            "--no-context-shift",
            // The model's own chat template (Jinja), needed for tool calls and structured output.
            "--jinja",
            // Thinking models: reasoning arrives separately from the answer.
            "--reasoning-format",
            "deepseek",
            "--no-webui",
            "--no-slots",
            "--sse-ping-interval",
            "10",
            "--log-colors",
            "off",
        ]
        .map(str::to_owned),
    );
    a
}

fn random_hex(bytes: usize) -> Result<String, LoadError> {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).map_err(|e| LoadError::Spawn(format!("no random numbers available: {e}")))?;
    Ok(hex::encode(b))
}

fn default_socket_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

fn free_port() -> Result<u16, LoadError> {
    // The port is released before the server binds it; a collision makes startup fail visibly.
    let l =
        TcpListener::bind("127.0.0.1:0").map_err(|e| LoadError::Spawn(format!("no free local port: {e}")))?;
    Ok(l.local_addr()
        .map_err(|e| LoadError::Spawn(format!("no free local port: {e}")))?
        .port())
}

type Log = Arc<Mutex<VecDeque<String>>>;

fn pump_log(src: impl Read + Send + 'static, log: Log) {
    thread::spawn(move || {
        for line in BufReader::new(src).lines().map_while(Result::ok) {
            if let Ok(mut l) = log.lock() {
                if l.len() == LOG_LINES {
                    l.pop_front();
                }
                l.push_back(line);
            }
        }
    });
}

pub struct LlamaServer {
    child: Child,
    endpoint: Endpoint,
    api_key: String,
    log: Log,
    socket_dir: Option<PathBuf>,
    pid_file: Option<PathBuf>,
    stopped: bool,
}

impl LlamaServer {
    pub fn start(cfg: &ServerConfig) -> Result<Self, LoadError> {
        if !cfg.server.is_file() {
            return Err(LoadError::RuntimeMissing(cfg.server.clone()));
        }
        if !cfg.model.is_file() {
            return Err(LoadError::ModelMissing(cfg.model.clone()));
        }
        if let Some(p) = &cfg.pid_file {
            reap_stale(p, &cfg.server);
        }
        let api_key = random_hex(32)?;
        let base = cfg.socket_base.clone().unwrap_or_else(default_socket_base);
        let dir = base.join(format!("clojurecraft-{}", random_hex(6)?));
        let socket = dir.join("llama.sock");
        let (endpoint, socket_dir, args) = if socket.as_os_str().len() <= MAX_SOCKET_PATH {
            DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|e| LoadError::Spawn(format!("could not create {}: {e}", dir.display())))?;
            let args = server_args(cfg, &socket.to_string_lossy(), None);
            (Endpoint::Unix(socket), Some(dir), args)
        } else {
            let port = free_port()?;
            (
                Endpoint::Tcp(port),
                None,
                server_args(cfg, "127.0.0.1", Some(port)),
            )
        };
        let mut cmd = Command::new(&cfg.server);
        cmd.args(&args);
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if k.starts_with("LLAMA_") || k.starts_with("HF_") || k == "HUGGING_FACE_HUB_TOKEN" {
                cmd.env_remove(k.as_ref());
            }
        }
        if let Some(dir) = cfg.server.parent() {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .env("LLAMA_API_KEY", &api_key)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if let Some(d) = &socket_dir {
                    let _ = fs::remove_dir_all(d);
                }
                LoadError::Spawn(format!("{}: {e}", cfg.server.display()))
            })?;
        let log: Log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(o) = child.stdout.take() {
            pump_log(o, log.clone());
        }
        if let Some(e) = child.stderr.take() {
            pump_log(e, log.clone());
        }
        if let Some(p) = &cfg.pid_file {
            if let Some(d) = p.parent() {
                let _ = fs::create_dir_all(d);
            }
            let _ = fs::write(p, format!("{}\n{}\n", child.id(), cfg.server.display()));
        }
        Ok(Self {
            child,
            endpoint,
            api_key,
            log,
            socket_dir,
            pid_file: cfg.pid_file.clone(),
            stopped: false,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn log_lines(&self) -> Vec<String> {
        self.log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn log_tail(&self, n: usize) -> String {
        let lines = self.log_lines();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }

    /// The exit status if the process has ended.
    pub fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(describe_status(status)),
            Ok(None) => None,
            Err(e) => Some(format!("unknown ({e})")),
        }
    }

    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        control: &Control,
    ) -> Result<Response, HttpError> {
        http::request(&self.endpoint, Some(&self.api_key), method, path, body, control)
    }

    /// Explains why the process ended while loading, from its log.
    pub fn load_failure(&self, status: String, backend: &str) -> LoadError {
        // Let the log readers catch up with the last lines.
        thread::sleep(Duration::from_millis(150));
        let lines = self.log_lines();
        match logscan::scan(&lines) {
            Some((Failure::OutOfMemory(kind), line)) => LoadError::OutOfMemory {
                kind,
                detail: line.trim().to_owned(),
            },
            Some((Failure::Backend, line)) => LoadError::BackendUnavailable {
                backend: backend.to_owned(),
                detail: line.trim().to_owned(),
            },
            Some((Failure::UnsupportedArchitecture, line)) => {
                LoadError::UnsupportedModel(line.trim().to_owned())
            }
            Some((Failure::ChatTemplate, line)) => LoadError::ChatTemplate(line.trim().to_owned()),
            Some((Failure::ModelFile, line)) => LoadError::ModelFile(line.trim().to_owned()),
            None => LoadError::Exited { status },
        }
    }

    /// Waits until the model is loaded (`/health` answers 200).
    pub fn wait_ready(
        &mut self,
        timeout: Duration,
        cancel: &Cancel,
        backend: &str,
    ) -> Result<Duration, LoadError> {
        let start = Instant::now();
        loop {
            if cancel.is_cancelled() {
                return Err(LoadError::Cancelled);
            }
            if let Some(status) = self.exited() {
                return Err(self.load_failure(status, backend));
            }
            let probe = Control::new(cancel.clone(), Some(Duration::from_secs(2)), None);
            if let Ok(r) = self.request("GET", "/health", None, &probe)
                && r.status == 200
            {
                return Ok(start.elapsed());
            }
            if start.elapsed() > timeout {
                return Err(LoadError::Timeout(timeout));
            }
            thread::sleep(Duration::from_millis(150));
        }
    }

    /// Asks the server to exit, then kills it if it does not, and removes its socket directory.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if self.child.try_wait().ok().flatten().is_none() {
            let pid = rustix::process::Pid::from_child(&self.child);
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            let until = Instant::now() + STOP_GRACE;
            while Instant::now() < until && self.child.try_wait().ok().flatten().is_none() {
                thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(d) = &self.socket_dir {
            let _ = fs::remove_dir_all(d);
        }
        if let Some(p) = &self.pid_file {
            let _ = fs::remove_file(p);
        }
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn describe_status(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(9)) => "killed (signal 9), possibly by the system for lack of memory".into(),
        (None, Some(sig)) => format!("terminated by signal {sig}"),
        _ => status.to_string(),
    }
}

/// Stops a server recorded in `pid_file` that outlived the application, if the recorded process
/// is still that server program. Unrelated processes that reused the number are left alone.
pub fn reap_stale(pid_file: &Path, server: &Path) {
    let Ok(text) = fs::read_to_string(pid_file) else {
        return;
    };
    let _ = fs::remove_file(pid_file);
    let mut lines = text.lines();
    let (Some(pid), Some(recorded)) = (
        lines.next().and_then(|p| p.trim().parse::<i32>().ok()),
        lines.next(),
    ) else {
        return;
    };
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return;
    };
    let Some(exe) = executable_of(pid) else { return };
    let same = |a: &Path, b: &Path| a == b || a.canonicalize().ok() == b.canonicalize().ok();
    if !(same(&exe, Path::new(recorded)) || same(&exe, server)) {
        return;
    }
    tracing::warn!(
        pid = pid.as_raw_nonzero().get(),
        "stopping a llama.cpp server left from an earlier run"
    );
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    let until = Instant::now() + STOP_GRACE;
    while Instant::now() < until && executable_of(pid).is_some() {
        thread::sleep(Duration::from_millis(50));
    }
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
}

#[cfg(target_os = "linux")]
fn executable_of(pid: rustix::process::Pid) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{}/exe", pid.as_raw_nonzero())).ok()
}

#[cfg(not(target_os = "linux"))]
fn executable_of(pid: rustix::process::Pid) -> Option<PathBuf> {
    let out = Command::new("/bin/ps")
        .args(["-o", "comm=", "-p", &pid.as_raw_nonzero().to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !s.is_empty()).then(|| PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn cfg(server: &Path, model: &Path, base: &Path) -> ServerConfig {
        ServerConfig {
            server: server.to_owned(),
            model: model.to_owned(),
            context: 4096,
            gpu_layers: Some(12),
            threads: None,
            pid_file: Some(base.join("server.pid")),
            socket_base: Some(base.to_owned()),
        }
    }

    /// A stand-in server program: a shell script.
    fn script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-server");
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn arguments_pin_the_safety_relevant_options() {
        let c = ServerConfig {
            server: "/s".into(),
            model: "/m.gguf".into(),
            context: 8192,
            gpu_layers: None,
            threads: Some(4),
            pid_file: None,
            socket_base: None,
        };
        let a = server_args(&c, "/run/x/llama.sock", None).join(" ");
        for expected in [
            "--host /run/x/llama.sock",
            "--ctx-size 8192",
            "--n-gpu-layers 999",
            "--threads 4",
            "--fit off",
            "--no-context-shift",
            "--no-webui",
            "--parallel 1",
        ] {
            assert!(a.contains(expected), "{expected} missing from {a}");
        }
        assert!(!a.contains("--tools"), "built-in tools must stay disabled");
        assert!(
            !a.contains("--api-key"),
            "the key goes through the environment only"
        );
        assert!(
            server_args(&c, "127.0.0.1", Some(5000))
                .join(" ")
                .contains("--port 5000")
        );
    }

    #[test]
    fn missing_files_are_reported_before_starting() {
        let d = tempfile::tempdir().unwrap();
        let model = d.path().join("m.gguf");
        let e = LlamaServer::start(&cfg(&d.path().join("none"), &model, d.path())).err();
        assert!(matches!(e, Some(LoadError::RuntimeMissing(_))));
        let s = script(d.path(), "exit 0");
        let e = LlamaServer::start(&cfg(&s, &model, d.path())).err();
        assert!(matches!(e, Some(LoadError::ModelMissing(_))));
    }

    #[test]
    fn startup_failure_is_classified_from_the_log() {
        let d = tempfile::tempdir().unwrap();
        let model = d.path().join("m.gguf");
        fs::write(&model, b"GGUF").unwrap();
        let s = script(
            d.path(),
            "echo 'llama_model_load: loading' >&2\n\
             echo 'ggml_backend_cuda_buffer_type_alloc_buffer: allocating 6144.00 MiB on device 0: cudaMalloc failed: out of memory' >&2\n\
             exit 1",
        );
        let mut srv = LlamaServer::start(&cfg(&s, &model, d.path())).unwrap();
        let e = srv
            .wait_ready(Duration::from_secs(10), &Cancel::default(), "CUDA")
            .unwrap_err();
        assert!(matches!(e, LoadError::OutOfMemory { .. }), "{e:?}");
        srv.stop();
        // The private socket directory and the pid file are gone.
        let left: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("clojurecraft-") || n == "server.pid")
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn server_gets_a_private_socket_directory_and_a_scrubbed_environment() {
        let d = tempfile::tempdir().unwrap();
        let model = d.path().join("m.gguf");
        fs::write(&model, b"GGUF").unwrap();
        let out = d.path().join("seen");
        let s = script(
            d.path(),
            &format!("env > {0}.env; echo \"$@\" > {0}.args; sleep 30", out.display()),
        );
        let mut srv = LlamaServer::start(&cfg(&s, &model, d.path())).unwrap();
        let Endpoint::Unix(sock) = srv.endpoint().clone() else {
            panic!("expected a socket")
        };
        let dir = sock.parent().unwrap().to_owned();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let until = Instant::now() + Duration::from_secs(5);
        while !out.with_extension("args").exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(50));
        let env = fs::read_to_string(out.with_extension("env")).unwrap();
        assert!(
            env.lines()
                .any(|l| l.starts_with("LLAMA_API_KEY=") && l.len() == 14 + 64)
        );
        let args = fs::read_to_string(out.with_extension("args")).unwrap();
        assert!(!args.contains(&env.lines().find(|l| l.starts_with("LLAMA_API_KEY=")).unwrap()[14..]));
        let start = Instant::now();
        srv.stop();
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(srv.exited().is_some());
        assert!(!dir.exists());
    }

    #[test]
    fn startup_cancellation_and_timeout() {
        let d = tempfile::tempdir().unwrap();
        let model = d.path().join("m.gguf");
        fs::write(&model, b"GGUF").unwrap();
        let s = script(d.path(), "sleep 30");
        let mut srv = LlamaServer::start(&cfg(&s, &model, d.path())).unwrap();
        assert_eq!(
            srv.wait_ready(Duration::from_millis(300), &Cancel::default(), "CPU"),
            Err(LoadError::Timeout(Duration::from_millis(300)))
        );
        let c = Cancel::default();
        c.cancel();
        assert_eq!(
            srv.wait_ready(Duration::from_secs(5), &c, "CPU"),
            Err(LoadError::Cancelled)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_server_from_a_previous_run_is_stopped_but_strangers_are_not() {
        let d = tempfile::tempdir().unwrap();
        let s = script(d.path(), "sleep 30");
        let mut child = Command::new(&s).spawn().unwrap();
        // A shell script's executable is the shell; record the shell so the check matches.
        let exe =
            fs::read_link(format!("/proc/{}/exe", child.id())).unwrap_or_else(|_| PathBuf::from("/bin/sh"));
        let pid_file = d.path().join("server.pid");
        fs::write(
            &pid_file,
            format!("{}\n{}\n", child.id(), d.path().join("other").display()),
        )
        .unwrap();
        reap_stale(&pid_file, Path::new("/nonexistent/llama-server"));
        assert!(
            child.try_wait().unwrap().is_none(),
            "an unrelated process was stopped"
        );
        fs::write(&pid_file, format!("{}\n{}\n", child.id(), exe.display())).unwrap();
        reap_stale(&pid_file, Path::new("/nonexistent/llama-server"));
        let until = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(child.try_wait().unwrap().is_some());
        assert!(!pid_file.exists());
    }
}
