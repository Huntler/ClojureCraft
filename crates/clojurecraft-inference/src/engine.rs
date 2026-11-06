//! The replaceable inference interface and its production implementation on `llama-server`.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{GenerationError, LoadError, LoadShape};
use crate::http::{Cancel, Control, HttpError};
use crate::process::{LlamaServer, ServerConfig};
use crate::protocol::{Accumulator, ChatRequest, StreamEvent, Turn, error_from_body, parse_props};

/// Estimated memory a load needs, compared against what is free before starting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryNeed {
    pub device: u64,
    pub host: u64,
}

/// Everything needed to load one model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadSpec {
    pub server: PathBuf,
    pub model: PathBuf,
    /// Shown to the user, e.g. "Qwen2.5-Coder 7B Instruct · Q4_K_M".
    pub label: String,
    /// Backend label, e.g. "CUDA".
    pub backend: String,
    /// Runtime build, e.g. "b12138-cuda".
    pub runtime: String,
    pub context: u32,
    /// Layers on the GPU; `None` places all of them there. Use `Some(0)` for the CPU backend.
    pub gpu_layers: Option<u32>,
    pub total_layers: Option<u32>,
    pub model_max_context: Option<u32>,
    pub threads: Option<u32>,
    pub load_timeout: Duration,
    pub memory: Option<MemoryNeed>,
    /// Load even if the memory check fails.
    pub skip_memory_check: bool,
    pub pid_file: Option<PathBuf>,
}

impl LoadSpec {
    pub fn shape(&self) -> LoadShape {
        LoadShape {
            context: self.context,
            gpu_layers: match self.gpu_layers {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(self.total_layers.unwrap_or(999)),
            },
            total_layers: self.total_layers,
        }
    }
}

/// What a loaded model offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub context: u32,
    pub build: Option<String>,
    /// The chat template supports tool calls.
    pub supports_tools: bool,
    pub pid: Option<u32>,
    pub load_time: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadPhase {
    /// Checking memory and starting the server process.
    Starting,
    /// The server runs and reads the model.
    Loading,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    Alive,
    Exited { detail: String, log: String },
}

pub trait Engine: Send {
    fn load(
        &mut self,
        spec: &LoadSpec,
        cancel: &Cancel,
        phase: &mut dyn FnMut(LoadPhase),
    ) -> Result<Box<dyn Session>, (LoadError, String)>;
}

/// A loaded model. Methods block; the service calls them from its worker thread.
pub trait Session: Send {
    fn info(&self) -> &SessionInfo;
    fn health(&mut self) -> Health;
    /// Tokens of a piece of text on its own.
    fn count_text(&mut self, text: &str, control: &Control) -> Result<u32, GenerationError>;
    /// Tokens of the full prompt the request renders to, chat template and tools included.
    fn count_prompt(&mut self, request: &ChatRequest, control: &Control) -> Result<u32, GenerationError>;
    /// Streams one model turn.
    fn chat(
        &mut self,
        request: &ChatRequest,
        control: &Control,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> Result<Turn, GenerationError>;
    fn log_tail(&self, lines: usize) -> String;
    fn shutdown(&mut self);
}

/// Runs models with a pinned `llama-server` build.
#[derive(Default)]
pub struct LlamaEngine {
    /// Where private socket directories are created; `None` uses the per-user runtime directory.
    pub socket_base: Option<PathBuf>,
}

impl Engine for LlamaEngine {
    fn load(
        &mut self,
        spec: &LoadSpec,
        cancel: &Cancel,
        phase: &mut dyn FnMut(LoadPhase),
    ) -> Result<Box<dyn Session>, (LoadError, String)> {
        phase(LoadPhase::Starting);
        let cfg = ServerConfig {
            server: spec.server.clone(),
            model: spec.model.clone(),
            context: spec.context,
            gpu_layers: spec.gpu_layers,
            threads: spec.threads,
            pid_file: spec.pid_file.clone(),
            socket_base: self.socket_base.clone(),
        };
        let mut server = LlamaServer::start(&cfg).map_err(|e| (e, String::new()))?;
        phase(LoadPhase::Loading);
        let fail = |server: &mut LlamaServer, e: LoadError| {
            let log = server.log_tail(40);
            server.stop();
            (e, log)
        };
        let load_time = match server.wait_ready(spec.load_timeout, cancel, &spec.backend) {
            Ok(t) => t,
            Err(e) => return Err(fail(&mut server, e)),
        };
        let control = Control::new(cancel.clone(), Some(Duration::from_secs(10)), None);
        let props = server
            .request("GET", "/props", None, &control)
            .and_then(|r| r.text(4 * 1024 * 1024))
            .map_err(|e| LoadError::Transport(e.to_string()))
            .and_then(|body| parse_props(&body).map_err(LoadError::Transport));
        let props = match props {
            Ok(p) => p,
            Err(e) => return Err(fail(&mut server, e)),
        };
        if !props.has_template {
            return Err(fail(
                &mut server,
                LoadError::ChatTemplate("the model file contains no chat template".into()),
            ));
        }
        Ok(Box::new(LlamaSession {
            info: SessionInfo {
                context: props.context.unwrap_or(spec.context),
                build: props.build,
                supports_tools: props.supports_tools,
                pid: Some(server.pid()),
                load_time,
            },
            server,
        }))
    }
}

struct LlamaSession {
    server: LlamaServer,
    info: SessionInfo,
}

impl LlamaSession {
    /// Turns a transport failure into a typed error, noticing when the server has died.
    fn transport(&mut self, e: HttpError, control: &Control) -> GenerationError {
        match e {
            HttpError::Cancelled => GenerationError::Cancelled,
            HttpError::Deadline => GenerationError::Timeout(control.timeout.unwrap_or_default()),
            HttpError::Idle(d) => GenerationError::Idle(d),
            other => {
                // A dying process may still be closing its sockets.
                for _ in 0..10 {
                    if let Some(status) = self.server.exited() {
                        return GenerationError::ServerCrashed(status);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                GenerationError::Transport(other.to_string())
            }
        }
    }

    fn post_json(&mut self, path: &str, body: &Value, control: &Control) -> Result<Value, GenerationError> {
        let bytes = serde_json::to_vec(body).map_err(|e| GenerationError::Transport(e.to_string()))?;
        let r = self
            .server
            .request("POST", path, Some(&bytes), control)
            .map_err(|e| self.transport(e, control))?;
        let status = r.status;
        let text = r.text(16 * 1024 * 1024).map_err(|e| self.transport(e, control))?;
        if status != 200 {
            return Err(error_from_body(status, &text));
        }
        serde_json::from_str(&text).map_err(|e| GenerationError::Malformed {
            problem: format!("unreadable {path} response ({e})"),
            raw: text.chars().take(400).collect(),
        })
    }

    fn tokens(&mut self, content: &str, special: bool, control: &Control) -> Result<u32, GenerationError> {
        let v = self.post_json(
            "/tokenize",
            &json!({"content": content, "add_special": special, "parse_special": special}),
            control,
        )?;
        v.get("tokens")
            .and_then(Value::as_array)
            .map(|t| u32::try_from(t.len()).unwrap_or(u32::MAX))
            .ok_or_else(|| GenerationError::Malformed {
                problem: "tokenizer response without tokens".into(),
                raw: String::new(),
            })
    }
}

impl Session for LlamaSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn health(&mut self) -> Health {
        match self.server.exited() {
            None => Health::Alive,
            Some(detail) => {
                std::thread::sleep(Duration::from_millis(100));
                Health::Exited {
                    detail,
                    log: self.server.log_tail(40),
                }
            }
        }
    }

    fn count_text(&mut self, text: &str, control: &Control) -> Result<u32, GenerationError> {
        self.tokens(text, false, control)
    }

    fn count_prompt(&mut self, request: &ChatRequest, control: &Control) -> Result<u32, GenerationError> {
        let v = self.post_json("/apply-template", &request.template_body(), control)?;
        let prompt = v
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| GenerationError::Malformed {
                problem: "template response without a prompt".into(),
                raw: String::new(),
            })?
            .to_owned();
        self.tokens(&prompt, true, control)
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        control: &Control,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> Result<Turn, GenerationError> {
        let body =
            serde_json::to_vec(&request.body(true)).map_err(|e| GenerationError::Transport(e.to_string()))?;
        let mut r = self
            .server
            .request("POST", "/v1/chat/completions", Some(&body), control)
            .map_err(|e| self.transport(e, control))?;
        if r.status != 200 {
            let status = r.status;
            let text = r.text(1024 * 1024).map_err(|e| self.transport(e, control))?;
            return Err(error_from_body(status, &text));
        }
        let mut acc = Accumulator::default();
        loop {
            let line = match r.line(4 * 1024 * 1024) {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => return Err(self.transport(e, control)),
            };
            for event in acc.line(&line)? {
                on_event(event);
            }
            if acc.is_done() {
                break;
            }
        }
        if !acc.is_done() {
            if let Some(status) = self.server.exited() {
                return Err(GenerationError::ServerCrashed(status));
            }
            return Err(GenerationError::Transport("the answer stream ended early".into()));
        }
        Ok(acc.finish())
    }

    fn log_tail(&self, lines: usize) -> String {
        self.server.log_tail(lines)
    }

    fn shutdown(&mut self) {
        self.server.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;
    use std::thread;
    use std::time::Instant;

    use super::*;
    use crate::http::test_server::{Part, chunked_sse, json, serve_at};
    use crate::protocol::Message;

    fn spec(dir: &Path, server: PathBuf) -> LoadSpec {
        let model = dir.join("m.gguf");
        fs::write(&model, b"GGUF").unwrap();
        LoadSpec {
            server,
            model,
            label: "m".into(),
            backend: "CPU".into(),
            runtime: "b1-cpu".into(),
            context: 4096,
            gpu_layers: Some(0),
            total_layers: Some(4),
            model_max_context: None,
            threads: None,
            load_timeout: Duration::from_secs(10),
            memory: None,
            skip_memory_check: false,
            pid_file: None,
        }
    }

    /// Starts a stand-in server program that records its arguments, then serves `responses` on
    /// the socket path it was given.
    fn fake_server(dir: &Path, responses: Vec<Vec<Part>>) -> (PathBuf, std::sync::mpsc::Receiver<String>) {
        let args = dir.join("args");
        let script = dir.join("llama-server");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\nexec sleep 30\n", args.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until {
                if let Ok(a) = fs::read_to_string(&args)
                    && let Some(host) = a.split_whitespace().skip_while(|w| *w != "--host").nth(1)
                {
                    let (_, requests) = serve_at(PathBuf::from(host), responses);
                    while let Ok(r) = requests.recv() {
                        let _ = tx.send(r);
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        (script, rx)
    }

    fn request(text: &str) -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user(text)],
            max_tokens: 64,
            temperature: 0.0,
            response_schema: None,
            tools: vec![],
            seed: None,
        }
    }

    #[test]
    fn loads_streams_and_detects_a_crash_over_the_private_socket() {
        let d = tempfile::tempdir().unwrap();
        let props = r#"{"default_generation_settings":{"n_ctx":4096},"chat_template":"{{ messages }}","build_info":"b12138","chat_template_caps":{"supports_tools":true,"supports_tool_calls":true}}"#;
        let mut crash = chunked_sse(&[r#"{"choices":[{"delta":{"content":"(de"}}]}"#]);
        crash.truncate(2);
        crash.push(Part::Pause(Duration::from_millis(500)));
        let (server, requests) = fake_server(
            d.path(),
            vec![
                json(200, r#"{"status":"ok"}"#),
                json(200, props),
                json(200, r#"{"prompt":"<|user|>hi"}"#),
                json(200, r#"{"tokens":[1,2,3]}"#),
                chunked_sse(&[
                    r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
                    r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                    r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
                    "[DONE]",
                ]),
                json(
                    400,
                    r#"{"error":{"code":400,"type":"exceed_context_size_error","message":"too long","n_prompt_tokens":5000,"n_ctx":4096}}"#,
                ),
                crash,
            ],
        );
        let mut engine = LlamaEngine {
            socket_base: Some(d.path().to_owned()),
        };
        let mut phases = Vec::new();
        let mut session = engine
            .load(&spec(d.path(), server), &Cancel::default(), &mut |p| {
                phases.push(p)
            })
            .map_err(|e| e.0)
            .unwrap();
        assert_eq!(phases, vec![LoadPhase::Starting, LoadPhase::Loading]);
        assert_eq!(session.info().context, 4096);
        assert!(session.info().supports_tools);
        assert_eq!(session.health(), Health::Alive);

        let control = Control::default();
        assert_eq!(session.count_prompt(&request("hi"), &control), Ok(3));

        let mut events = Vec::new();
        let turn = session
            .chat(&request("hi"), &control, &mut |e| events.push(e))
            .unwrap();
        assert_eq!(turn.content, "ok");
        assert_eq!(turn.reasoning, "hmm");
        assert_eq!(turn.usage.prompt_tokens, Some(3));
        assert_eq!(events.len(), 2);

        assert!(matches!(
            session.chat(&request("long"), &control, &mut |_| {}),
            Err(GenerationError::ContextExceeded { needed: 5000, .. })
        ));

        let pid = session.info().pid.unwrap();
        let r = session.chat(&request("crash"), &control, &mut |_| {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        });
        assert!(matches!(r, Err(GenerationError::ServerCrashed(_))), "{r:?}");
        assert!(matches!(session.health(), Health::Exited { .. }));

        // Every request carried the session key.
        let seen: Vec<String> = requests.try_iter().collect();
        assert!(seen.len() >= 6);
        assert!(seen.iter().all(|r| r.contains("Authorization: Bearer ")));
        session.shutdown();
    }

    #[test]
    fn a_model_without_a_chat_template_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (server, _) = fake_server(
            d.path(),
            vec![
                json(200, r#"{"status":"ok"}"#),
                json(
                    200,
                    r#"{"default_generation_settings":{"n_ctx":4096},"chat_template":""}"#,
                ),
            ],
        );
        let mut engine = LlamaEngine {
            socket_base: Some(d.path().to_owned()),
        };
        let e = engine
            .load(&spec(d.path(), server), &Cancel::default(), &mut |_| {})
            .err()
            .map(|e| e.0);
        assert!(matches!(e, Some(LoadError::ChatTemplate(_))), "{e:?}");
    }
}
