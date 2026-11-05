//! A llama.cpp server running as a child process.
//!
//! The server listens only on the loopback interface, on a port chosen at start, and requires a
//! random per-session API key, so other local users and web pages cannot drive it. Its log output
//! is kept in a bounded buffer for diagnostics.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const LOG_LINES: usize = 400;

#[derive(Debug, thiserror::Error)]
pub enum InferenceError {
    #[error("could not start llama.cpp server {path}: {source}")]
    Spawn { path: PathBuf, source: std::io::Error },
    #[error("no free local port: {0}")]
    Port(std::io::Error),
    #[error("could not generate a session key: {0}")]
    Random(String),
    #[error("llama.cpp server exited during startup ({status}); last log lines:\n{log}")]
    ExitedEarly { status: String, log: String },
    #[error("llama.cpp server did not become ready within {0:?}")]
    StartupTimeout(Duration),
    #[error("request failed: {0}")]
    Http(String),
    #[error("malformed server response: {0}")]
    Protocol(String),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub server: PathBuf,
    /// The model file; for split models, the first shard.
    pub model: PathBuf,
    pub context: u32,
    /// Layers to offload to the GPU; `None` offloads all of them.
    pub gpu_layers: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(c: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: c.into(),
        }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: c.into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    pub text: String,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Server-reported generation speed.
    pub tokens_per_second: Option<f64>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct LlamaServer {
    child: Child,
    port: u16,
    api_key: String,
    log: Arc<Mutex<VecDeque<String>>>,
    agent: ureq::Agent,
}

fn free_port() -> Result<u16, InferenceError> {
    // The port is released before the server binds it; a collision makes startup fail visibly.
    let l = TcpListener::bind("127.0.0.1:0").map_err(InferenceError::Port)?;
    Ok(l.local_addr().map_err(InferenceError::Port)?.port())
}

fn session_key() -> Result<String, InferenceError> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| InferenceError::Random(e.to_string()))?;
    Ok(hex::encode(b))
}

fn pump_log(src: impl Read + Send + 'static, log: Arc<Mutex<VecDeque<String>>>) {
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

/// Lists the devices a llama.cpp build can use (`llama-server --list-devices`).
pub fn list_devices(server: &Path) -> Result<String, InferenceError> {
    let out = Command::new(server)
        .arg("--list-devices")
        .output()
        .map_err(|source| InferenceError::Spawn {
            path: server.to_owned(),
            source,
        })?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

impl LlamaServer {
    pub fn start(cfg: &ServerConfig) -> Result<Self, InferenceError> {
        let port = free_port()?;
        let api_key = session_key()?;
        let ngl = cfg.gpu_layers.map_or_else(|| "999".to_owned(), |n| n.to_string());
        let mut child = Command::new(&cfg.server)
            .arg("--model")
            .arg(&cfg.model)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .args(["--ctx-size", &cfg.context.to_string()])
            .args(["--n-gpu-layers", &ngl])
            .args(["--parallel", "1", "--no-webui", "--jinja"])
            // The key is passed through the environment so it does not appear in process listings.
            .env("LLAMA_API_KEY", &api_key)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| InferenceError::Spawn { path: cfg.server.clone(), source })?;
        let log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(o) = child.stdout.take() {
            pump_log(o, log.clone());
        }
        if let Some(e) = child.stderr.take() {
            pump_log(e, log.clone());
        }
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        Ok(Self {
            child,
            port,
            api_key,
            log,
            agent,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn log_tail(&self, n: usize) -> String {
        let l = self
            .log
            .lock()
            .map(|l| l.iter().rev().take(n).cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        l.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Waits until the model is loaded (`/health` returns 200).
    pub fn wait_ready(&mut self, timeout: Duration, cancel: &Cancel) -> Result<Duration, InferenceError> {
        let start = Instant::now();
        loop {
            if cancel.is_cancelled() {
                return Err(InferenceError::Cancelled);
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                thread::sleep(Duration::from_millis(100));
                return Err(InferenceError::ExitedEarly {
                    status: status.to_string(),
                    log: self.log_tail(30),
                });
            }
            let ok = self
                .agent
                .get(&self.url("/health"))
                .header("Authorization", &format!("Bearer {}", self.api_key))
                .call()
                .map(|r| r.status().as_u16() == 200)
                .unwrap_or(false);
            if ok {
                return Ok(start.elapsed());
            }
            if start.elapsed() > timeout {
                return Err(InferenceError::StartupTimeout(timeout));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    /// Returns the HTTP status of an unauthenticated request (expected: 401).
    pub fn probe_without_key(&self) -> Result<u16, InferenceError> {
        self.agent
            .post(&self.url("/v1/chat/completions"))
            .send_json(serde_json::json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 1}))
            .map(|r| r.status().as_u16())
            .map_err(|e| InferenceError::Http(e.to_string()))
    }

    /// Streams a chat completion, calling `on_delta` for each text fragment. Cancelling stops
    /// reading and closes the connection, which makes the server abort the generation.
    pub fn chat(
        &self,
        messages: &[Message],
        max_tokens: u32,
        temperature: f32,
        cancel: &Cancel,
        mut on_delta: impl FnMut(&str),
    ) -> Result<Completion, InferenceError> {
        let start = Instant::now();
        let body = serde_json::json!({
            "messages": messages,
            "max_tokens": max_tokens,
            "temperature": temperature,
            "stream": true,
            "stream_options": {"include_usage": true},
            "timings_per_token": false,
        });
        let resp = self
            .agent
            .post(&self.url("/v1/chat/completions"))
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(&body)
            .map_err(|e| InferenceError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let text = resp.into_body().read_to_string().unwrap_or_default();
            return Err(InferenceError::Http(format!("status {status}: {text}")));
        }
        let mut out = Completion::default();
        let reader = BufReader::new(resp.into_body().into_reader());
        for line in reader.lines() {
            if cancel.is_cancelled() {
                return Err(InferenceError::Cancelled);
            }
            let line = line.map_err(|e| InferenceError::Http(e.to_string()))?;
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data.trim() == "[DONE]" {
                break;
            }
            let chunk: StreamChunk =
                serde_json::from_str(data).map_err(|e| InferenceError::Protocol(format!("{e}: {data}")))?;
            for c in &chunk.choices {
                if let Some(t) = c.delta.content.as_deref() {
                    out.text.push_str(t);
                    on_delta(t);
                }
            }
            if let Some(u) = chunk.usage {
                out.prompt_tokens = u.prompt_tokens;
                out.completion_tokens = u.completion_tokens;
            }
            if let Some(t) = chunk.timings {
                out.tokens_per_second = t.predicted_per_second;
            }
        }
        out.elapsed = start.elapsed();
        Ok(out)
    }

    pub fn stop(mut self) {
        self.kill();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<Usage>,
    timings: Option<Timings>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Timings {
    predicted_per_second: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_chunks_with_and_without_content() {
        let c: StreamChunk = serde_json::from_str(r#"{"choices":[{"delta":{"content":"(defn"}}]}"#).unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("(defn"));
        let c: StreamChunk =
            serde_json::from_str(r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7},"timings":{"predicted_per_second":42.5}}"#)
                .unwrap();
        assert_eq!(c.usage.unwrap().completion_tokens, Some(7));
        let c: StreamChunk = serde_json::from_str(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap();
        assert!(c.choices[0].delta.content.is_none());
    }

    #[test]
    fn session_keys_are_random_and_long() {
        let a = session_key().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, session_key().unwrap());
    }
}
