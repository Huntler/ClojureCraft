//! The inference service: one worker thread owns the loaded model and handles requests in order.
//!
//! Callers never block. Commands go over a channel, and everything that happens comes back as
//! [`ServiceEvent`]s through a callback. Cancelling a request or unloading the model takes effect
//! immediately through shared flags, even while the worker waits on the server. Every request has
//! an identifier, every event carries it, and a cancelled request ends with
//! `Err(Cancelled)` and emits nothing after it is cancelled, so its output cannot overwrite a
//! newer request's.
//!
//! The worker checks the server process while idle. A crash is reported and the model is loaded
//! again automatically a limited number of times; after that the user decides.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::context::{self, Placement, PromptPlan, Section, TokenCounter};
use crate::engine::{Engine, Health, LoadPhase, LoadSpec, MemoryNeed, Session, SessionInfo};
use crate::error::{GenerationError, LoadError, MemoryKind, Remedy, remedies_for_load};
use crate::http::{Cancel, Control};
use crate::protocol::{ChatRequest, Finish, Message, StreamEvent, ToolCall, ToolSpec, Usage};
use crate::roles::Role;
use crate::schema::Schema;
use crate::tools::{ToolOutcome, ToolSet};

pub type RequestId = u64;

/// Free memory right now; `None` where it cannot be observed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryAvailable {
    pub device: Option<u64>,
    pub host: Option<u64>,
}

pub trait MemoryProbe: Send {
    fn available(&self) -> MemoryAvailable;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    Unloaded,
    Loading {
        model: String,
        phase: LoadPhase,
    },
    Ready {
        model: String,
        info: SessionInfo,
    },
    Failed {
        model: String,
        error: LoadError,
        remedies: Vec<Remedy>,
        log: String,
    },
    /// The server stopped unexpectedly. With `restarting` the model is being loaded again.
    Crashed {
        model: String,
        detail: String,
        restarting: bool,
        log: String,
    },
}

/// A generation request for one role.
#[derive(Clone, Debug)]
pub struct GenerateRequest {
    pub role: Role,
    /// The prompt content; the role's instructions are added in front automatically.
    pub sections: Vec<Section>,
    /// Constrains and validates the answer as JSON.
    pub schema: Option<Schema>,
    pub tools: ToolSet,
    pub max_output_tokens: Option<u32>,
    pub timeout: Option<Duration>,
    pub seed: Option<u64>,
    /// Overrides the role's sampling temperature.
    pub temperature: Option<f32>,
}

impl GenerateRequest {
    pub fn new(role: Role, sections: Vec<Section>) -> Self {
        Self {
            role,
            sections,
            schema: None,
            tools: ToolSet::default(),
            max_output_tokens: None,
            timeout: None,
            seed: None,
            temperature: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Generation {
    pub role: Role,
    pub model: String,
    pub text: String,
    pub reasoning: String,
    /// The validated answer, for structured requests.
    pub value: Option<Value>,
    pub finish: Finish,
    pub usage: Usage,
    pub plan: PromptPlan,
    pub tool_calls: Vec<(ToolCall, ToolOutcome)>,
    /// Tools were requested but the model's chat template does not support them.
    pub tools_unavailable: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServiceEvent {
    Status(ModelStatus),
    Stream {
        request: RequestId,
        event: StreamEvent,
    },
    Tool {
        request: RequestId,
        call: ToolCall,
        outcome: ToolOutcome,
    },
    Finished {
        request: RequestId,
        result: Result<Box<Generation>, GenerationError>,
    },
}

pub type Sink = Box<dyn Fn(ServiceEvent) + Send>;

/// How often the idle worker checks the server process.
const HEALTH_INTERVAL: Duration = Duration::from_millis(1000);
/// Silence tolerated from the server; it sends keep-alive comments every 10 s while busy.
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const RESTART_WINDOW: Duration = Duration::from_secs(600);
const MAX_AUTO_RESTARTS: usize = 2;

enum Command {
    Load(Box<LoadSpec>, Cancel),
    Unload,
    Generate(RequestId, Box<GenerateRequest>, Cancel),
    Shutdown,
}

#[derive(Default)]
struct Shared {
    load: Mutex<Cancel>,
    active: Mutex<HashMap<RequestId, Cancel>>,
}

impl Shared {
    fn cancel_all(&self) {
        if let Ok(a) = self.active.lock() {
            for c in a.values() {
                c.cancel();
            }
        }
    }
    fn new_load(&self) -> Cancel {
        let fresh = Cancel::default();
        if let Ok(mut l) = self.load.lock() {
            l.cancel();
            *l = fresh.clone();
        }
        fresh
    }
}

pub struct InferenceService {
    tx: Sender<Command>,
    shared: Arc<Shared>,
    next: AtomicU64,
    worker: Option<JoinHandle<()>>,
}

impl InferenceService {
    pub fn new(engine: Box<dyn Engine>, probe: Option<Box<dyn MemoryProbe>>, sink: Sink) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let mut worker = Worker {
            engine,
            probe,
            sink,
            session: None,
            spec: None,
            crashes: Vec::new(),
            shared: shared.clone(),
        };
        let handle = thread::Builder::new()
            .name("inference".into())
            .spawn(move || {
                loop {
                    match rx.recv_timeout(HEALTH_INTERVAL) {
                        Ok(Command::Load(spec, cancel)) => worker.load(*spec, &cancel),
                        Ok(Command::Unload) => worker.unload(),
                        Ok(Command::Generate(id, req, cancel)) => worker.generate(id, *req, &cancel),
                        Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                            worker.stop_session();
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => worker.check_health(),
                    }
                }
            })
            .ok();
        Self {
            tx,
            shared,
            next: AtomicU64::new(1),
            worker: handle,
        }
    }

    /// Loads a model, replacing the current one and cancelling a load in progress.
    pub fn load(&self, spec: LoadSpec) {
        let cancel = self.shared.new_load();
        self.shared.cancel_all();
        let _ = self.tx.send(Command::Load(Box::new(spec), cancel));
    }

    /// Stops the server, cancelling the load or requests in progress.
    pub fn unload(&self) {
        self.shared.new_load();
        self.shared.cancel_all();
        let _ = self.tx.send(Command::Unload);
    }

    /// Queues a request; its events carry the returned identifier.
    pub fn generate(&self, request: GenerateRequest) -> RequestId {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let cancel = Cancel::default();
        if let Ok(mut a) = self.shared.active.lock() {
            a.insert(id, cancel.clone());
        }
        if self
            .tx
            .send(Command::Generate(id, Box::new(request), cancel))
            .is_err()
            && let Ok(mut a) = self.shared.active.lock()
        {
            a.remove(&id);
        }
        id
    }

    pub fn cancel(&self, id: RequestId) {
        if let Ok(a) = self.shared.active.lock()
            && let Some(c) = a.get(&id)
        {
            c.cancel();
        }
    }

    /// Stops all work and the server, and waits for the worker to finish.
    pub fn shutdown(&mut self) {
        self.shared.new_load();
        self.shared.cancel_all();
        let _ = self.tx.send(Command::Shutdown);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for InferenceService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    engine: Box<dyn Engine>,
    probe: Option<Box<dyn MemoryProbe>>,
    sink: Sink,
    session: Option<Box<dyn Session>>,
    spec: Option<LoadSpec>,
    crashes: Vec<Instant>,
    shared: Arc<Shared>,
}

/// Counts tokens with the loaded model's tokenizer.
struct Counter<'a> {
    session: &'a mut dyn Session,
    control: &'a Control,
    tools: &'a [ToolSpec],
}

impl TokenCounter for Counter<'_> {
    fn section(&mut self, text: &str) -> Result<u32, GenerationError> {
        self.session.count_text(text, self.control)
    }
    fn prompt(&mut self, messages: &[Message]) -> Result<u32, GenerationError> {
        let req = ChatRequest {
            messages: messages.to_vec(),
            max_tokens: 0,
            temperature: 0.0,
            top_p: None,
            response_schema: None,
            tools: self.tools.to_vec(),
            seed: None,
        };
        self.session.count_prompt(&req, self.control)
    }
    fn exact(&self) -> bool {
        true
    }
}

/// Compares an estimated need with free memory.
pub fn check_memory(need: MemoryNeed, available: MemoryAvailable) -> Result<(), LoadError> {
    for (kind, needed, free) in [
        (MemoryKind::Device, need.device, available.device),
        (MemoryKind::Host, need.host, available.host),
    ] {
        if let Some(free) = free
            && needed > free
        {
            return Err(LoadError::InsufficientMemory {
                kind,
                needed,
                available: free,
            });
        }
    }
    Ok(())
}

/// Reads a structured answer: JSON, optionally wrapped in a Markdown code fence.
pub fn parse_structured(text: &str, schema: &Schema) -> Result<Value, GenerationError> {
    let mut t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        t = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
        t = t.strip_suffix("```").unwrap_or(t).trim();
    }
    let value: Value = serde_json::from_str(t).map_err(|e| GenerationError::Malformed {
        problem: format!("the answer is not valid JSON ({e})"),
        raw: text.to_owned(),
    })?;
    schema.validate(&value).map_err(|e| GenerationError::Malformed {
        problem: format!("the answer does not match the expected structure: {e}"),
        raw: text.to_owned(),
    })?;
    Ok(value)
}

impl Worker {
    fn status(&self, s: ModelStatus) {
        (self.sink)(ServiceEvent::Status(s));
    }

    fn stop_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.shutdown();
        }
    }

    fn unload(&mut self) {
        self.stop_session();
        self.spec = None;
        self.status(ModelStatus::Unloaded);
    }

    fn load(&mut self, spec: LoadSpec, cancel: &Cancel) {
        self.stop_session();
        self.spec = Some(spec.clone());
        let model = spec.label.clone();
        self.status(ModelStatus::Loading {
            model: model.clone(),
            phase: LoadPhase::Starting,
        });
        if let (Some(need), Some(probe), false) = (spec.memory, &self.probe, spec.skip_memory_check)
            && let Err(e) = check_memory(need, probe.available())
        {
            let remedies = remedies_for_load(&e, spec.shape());
            self.status(ModelStatus::Failed {
                model,
                error: e,
                remedies,
                log: String::new(),
            });
            return;
        }
        let sink = &self.sink;
        let mut on_phase = |phase| {
            sink(ServiceEvent::Status(ModelStatus::Loading {
                model: model.clone(),
                phase,
            }));
        };
        match self.engine.load(&spec, cancel, &mut on_phase) {
            Ok(session) => {
                if cancel.is_cancelled() {
                    let mut s = session;
                    s.shutdown();
                    return;
                }
                let info = session.info().clone();
                self.session = Some(session);
                self.status(ModelStatus::Ready { model, info });
            }
            // A newer command superseded this load; it reports its own status.
            Err((LoadError::Cancelled, _)) => {}
            Err((error, log)) => {
                tracing::warn!(error = %error, "model load failed");
                let remedies = remedies_for_load(&error, spec.shape());
                self.status(ModelStatus::Failed {
                    model,
                    error,
                    remedies,
                    log,
                });
            }
        }
    }

    fn check_health(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if let Health::Exited { detail, log } = session.health() {
            self.crashed(detail, log);
        }
    }

    fn crashed(&mut self, detail: String, log: String) {
        tracing::warn!(%detail, "llama.cpp server stopped unexpectedly");
        self.stop_session();
        let Some(spec) = self.spec.clone() else { return };
        let now = Instant::now();
        self.crashes.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        self.crashes.push(now);
        let restarting = self.crashes.len() <= MAX_AUTO_RESTARTS;
        self.status(ModelStatus::Crashed {
            model: spec.label.clone(),
            detail,
            restarting,
            log,
        });
        if restarting {
            let cancel = self.shared.new_load();
            self.load(spec, &cancel);
        }
    }

    fn generate(&mut self, id: RequestId, req: GenerateRequest, cancel: &Cancel) {
        let result = if cancel.is_cancelled() {
            Err(GenerationError::Cancelled)
        } else {
            self.run(id, req, cancel)
        };
        // A request cancelled while finishing reports the cancellation, never a late result.
        let result = if cancel.is_cancelled() {
            Err(GenerationError::Cancelled)
        } else {
            result
        };
        if let Ok(mut a) = self.shared.active.lock() {
            a.remove(&id);
        }
        let crash = match &result {
            Err(GenerationError::ServerCrashed(detail)) => Some(detail.clone()),
            _ => None,
        };
        (self.sink)(ServiceEvent::Finished {
            request: id,
            result: result.map(Box::new),
        });
        if let Some(detail) = crash {
            let log = self.session.as_ref().map(|s| s.log_tail(40)).unwrap_or_default();
            self.crashed(detail, log);
        }
    }

    fn run(
        &mut self,
        id: RequestId,
        req: GenerateRequest,
        cancel: &Cancel,
    ) -> Result<Generation, GenerationError> {
        let started = Instant::now();
        let model = self.spec.as_ref().map(|s| s.label.clone()).unwrap_or_default();
        let model_max = self.spec.as_ref().and_then(|s| s.model_max_context);
        let sink = &self.sink;
        let session = self.session.as_deref_mut().ok_or(GenerationError::NotReady)?;
        let info = session.info().clone();
        let profile = req.role.profile();
        let control = Control::new(
            cancel.clone(),
            Some(req.timeout.unwrap_or(profile.timeout)),
            Some(IDLE_LIMIT),
        );
        let reserved = req
            .max_output_tokens
            .unwrap_or(profile.max_output_tokens)
            .min(info.context / 2);
        let tools_unavailable = !req.tools.is_empty() && !info.supports_tools;
        let tools = if tools_unavailable {
            ToolSet::default()
        } else {
            req.tools
        };
        let specs = tools.specs();

        let mut sections = vec![Section::required(
            "Instructions",
            Placement::System,
            req.role.instructions(),
        )];
        sections.extend(req.sections);
        let plan = context::plan(
            &sections,
            &mut Counter {
                session: &mut *session,
                control: &control,
                tools: &specs,
            },
            info.context,
            reserved,
            model_max,
        )?;
        let kept: Vec<&Section> = plan.included.iter().map(|i| &sections[*i]).collect();
        let mut messages = context::messages(&kept);

        let mut tool_calls = Vec::new();
        let mut rounds = 0u32;
        let turn = loop {
            let request = ChatRequest {
                messages: messages.clone(),
                max_tokens: reserved,
                temperature: req.temperature.unwrap_or(profile.temperature),
                top_p: Some(profile.top_p),
                response_schema: req.schema.clone(),
                tools: specs.clone(),
                seed: req.seed,
            };
            let mut forward = |event: StreamEvent| {
                if !cancel.is_cancelled() {
                    sink(ServiceEvent::Stream { request: id, event });
                }
            };
            let turn = session.chat(&request, &control, &mut forward)?;
            if turn.tool_calls.is_empty() || tools.is_empty() {
                break turn;
            }
            rounds += 1;
            if rounds > tools.limits.max_rounds {
                return Err(GenerationError::ToolRounds(tools.limits.max_rounds));
            }
            messages.push(Message::assistant(turn.content.clone(), turn.tool_calls.clone()));
            for (i, call) in turn.tool_calls.iter().enumerate() {
                let outcome = if i >= tools.limits.max_calls_per_round {
                    ToolOutcome::Refused("too many tool calls in one turn".into())
                } else {
                    tools.execute(call, cancel)
                };
                if cancel.is_cancelled() {
                    return Err(GenerationError::Cancelled);
                }
                sink(ServiceEvent::Tool {
                    request: id,
                    call: call.clone(),
                    outcome: outcome.clone(),
                });
                messages.push(Message::tool_result(&call.id, outcome.message()));
                tool_calls.push((call.clone(), outcome));
            }
        };

        let value = match &req.schema {
            Some(_) if turn.finish == Finish::Length => {
                return Err(GenerationError::CutOff {
                    limit: reserved,
                    raw: turn.content,
                    reasoning_chars: turn.reasoning.chars().count(),
                });
            }
            Some(schema) => Some(parse_structured(&turn.content, schema)?),
            None => None,
        };
        Ok(Generation {
            role: req.role,
            model,
            text: turn.content,
            reasoning: turn.reasoning,
            value,
            finish: turn.finish,
            usage: turn.usage,
            plan,
            tool_calls,
            tools_unavailable,
            elapsed: started.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_check_reports_the_first_shortfall() {
        let need = MemoryNeed {
            device: 6 << 30,
            host: 2 << 30,
        };
        assert!(
            check_memory(
                need,
                MemoryAvailable {
                    device: Some(7 << 30),
                    host: None
                }
            )
            .is_ok()
        );
        assert!(matches!(
            check_memory(
                need,
                MemoryAvailable {
                    device: Some(5 << 30),
                    host: Some(1 << 30)
                }
            ),
            Err(LoadError::InsufficientMemory {
                kind: MemoryKind::Device,
                ..
            })
        ));
        assert!(matches!(
            check_memory(
                need,
                MemoryAvailable {
                    device: None,
                    host: Some(1 << 30)
                }
            ),
            Err(LoadError::InsufficientMemory {
                kind: MemoryKind::Host,
                ..
            })
        ));
    }

    #[test]
    fn structured_answers_may_be_fenced_but_must_match() {
        let schema = Schema::object(vec![crate::schema::Field::required("code", Schema::string())]);
        assert_eq!(
            parse_structured("```json\n{\"code\":\"x\"}\n```", &schema).unwrap()["code"],
            "x"
        );
        assert!(matches!(
            parse_structured("{\"code\": 1}", &schema),
            Err(GenerationError::Malformed { .. })
        ));
        assert!(matches!(
            parse_structured("Sure! Here it is", &schema),
            Err(GenerationError::Malformed { .. })
        ));
    }
}
