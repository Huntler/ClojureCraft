//! A scripted engine for testing code that drives the inference service without llama.cpp.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::engine::{Engine, Health, LoadPhase, LoadSpec, Session, SessionInfo};
use crate::error::{GenerationError, LoadError};
use crate::http::{Cancel, Control};
use crate::protocol::{ChatRequest, Finish, StreamEvent, ToolCall, Turn, Usage};

/// One scripted model turn.
#[derive(Clone, Debug)]
pub enum MockReply {
    /// Streams the pieces as answer text, pausing before each.
    Text {
        pieces: Vec<String>,
        pause: Duration,
        finish: Finish,
    },
    ToolCalls(Vec<ToolCall>),
    Fail(GenerationError),
    /// Streams the pieces, then the server process dies.
    CrashAfter(Vec<String>),
    /// Sends nothing until cancelled or timed out.
    Hang,
}

impl MockReply {
    pub fn text(s: &str) -> Self {
        Self::Text {
            pieces: vec![s.to_owned()],
            pause: Duration::ZERO,
            finish: Finish::Stop,
        }
    }
    pub fn slow(pieces: &[&str], pause: Duration) -> Self {
        Self::Text {
            pieces: pieces.iter().map(|p| (*p).to_owned()).collect(),
            pause,
            finish: Finish::Stop,
        }
    }
}

#[derive(Debug)]
pub struct MockState {
    /// Results of successive loads; when empty, loads succeed.
    pub loads: VecDeque<Result<(), LoadError>>,
    pub load_delay: Duration,
    pub replies: VecDeque<MockReply>,
    pub context: u32,
    pub supports_tools: bool,
    /// Every chat request received.
    pub requests: Vec<ChatRequest>,
    pub load_count: usize,
    pub shutdown_count: usize,
    /// Whether the current session's process is alive.
    pub alive: bool,
    generation: u64,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            loads: VecDeque::new(),
            load_delay: Duration::ZERO,
            replies: VecDeque::new(),
            context: 4096,
            supports_tools: true,
            requests: Vec::new(),
            load_count: 0,
            shutdown_count: 0,
            alive: false,
            generation: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct MockEngine {
    state: Arc<Mutex<MockState>>,
}

impl MockEngine {
    pub fn state(&self) -> MutexGuard<'_, MockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn reply(&self, r: MockReply) {
        self.state().replies.push_back(r);
    }
    /// Makes the running server process die.
    pub fn crash(&self) {
        self.state().alive = false;
    }
}

fn wait(control: &Control, d: Duration) -> Result<(), GenerationError> {
    let until = Instant::now() + d;
    loop {
        if control.cancel.is_cancelled() {
            return Err(GenerationError::Cancelled);
        }
        if control.deadline.is_some_and(|t| Instant::now() >= t) {
            return Err(GenerationError::Timeout(control.timeout.unwrap_or_default()));
        }
        if Instant::now() >= until {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5).min(until - Instant::now()));
    }
}

impl Engine for MockEngine {
    fn load(
        &mut self,
        _spec: &LoadSpec,
        cancel: &Cancel,
        phase: &mut dyn FnMut(LoadPhase),
    ) -> Result<Box<dyn Session>, (LoadError, String)> {
        phase(LoadPhase::Starting);
        let delay = self.state().load_delay;
        phase(LoadPhase::Loading);
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if cancel.is_cancelled() {
                return Err((LoadError::Cancelled, String::new()));
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mut s = self.state();
        s.load_count += 1;
        if let Some(Err(e)) = s.loads.pop_front() {
            return Err((e, "mock log".into()));
        }
        s.alive = true;
        s.generation += 1;
        Ok(Box::new(MockSession {
            info: SessionInfo {
                context: s.context,
                build: Some("mock".into()),
                supports_tools: s.supports_tools,
                pid: None,
                load_time: delay,
            },
            generation: s.generation,
            state: self.state.clone(),
        }))
    }
}

struct MockSession {
    info: SessionInfo,
    generation: u64,
    state: Arc<Mutex<MockState>>,
}

impl MockSession {
    fn state(&self) -> MutexGuard<'_, MockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn alive(&self) -> bool {
        let s = self.state();
        s.alive && s.generation == self.generation
    }
}

impl Session for MockSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn health(&mut self) -> Health {
        if self.alive() {
            Health::Alive
        } else {
            Health::Exited {
                detail: "terminated by signal 11".into(),
                log: "mock log".into(),
            }
        }
    }

    fn count_text(&mut self, text: &str, _control: &Control) -> Result<u32, GenerationError> {
        Ok(u32::try_from(text.len().div_ceil(4)).unwrap_or(u32::MAX))
    }

    fn count_prompt(&mut self, request: &ChatRequest, control: &Control) -> Result<u32, GenerationError> {
        let mut n = 4;
        for m in &request.messages {
            n += self.count_text(&m.content, control)? + 4;
        }
        Ok(n)
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        control: &Control,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> Result<Turn, GenerationError> {
        if !self.alive() {
            return Err(GenerationError::ServerCrashed("terminated by signal 11".into()));
        }
        let reply = {
            let mut s = self.state();
            s.requests.push(request.clone());
            s.replies.pop_front().unwrap_or_else(|| MockReply::text("ok"))
        };
        let mut turn = Turn {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            finish: Finish::Stop,
            usage: Usage::default(),
        };
        match reply {
            MockReply::Text {
                pieces,
                pause,
                finish,
            } => {
                for p in pieces {
                    wait(control, pause)?;
                    turn.content.push_str(&p);
                    on_event(StreamEvent::Content(p));
                }
                turn.finish = finish;
            }
            MockReply::ToolCalls(calls) => {
                for c in &calls {
                    on_event(StreamEvent::ToolCall { name: c.name.clone() });
                }
                turn.tool_calls = calls;
                turn.finish = Finish::ToolCalls;
            }
            MockReply::Fail(e) => return Err(e),
            MockReply::CrashAfter(pieces) => {
                for p in pieces {
                    on_event(StreamEvent::Content(p));
                }
                self.state().alive = false;
                return Err(GenerationError::ServerCrashed("terminated by signal 11".into()));
            }
            MockReply::Hang => loop {
                wait(control, Duration::from_millis(20))?;
            },
        }
        Ok(turn)
    }

    fn log_tail(&self, _lines: usize) -> String {
        "mock log".into()
    }

    fn shutdown(&mut self) {
        let mut s = self.state();
        s.shutdown_count += 1;
        if s.generation == self.generation {
            s.alive = false;
        }
    }
}
