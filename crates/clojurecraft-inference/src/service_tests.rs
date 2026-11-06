//! Behaviour of the inference service against a scripted engine.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::context::{Placement, Section};
use crate::engine::{LoadPhase, LoadSpec, MemoryNeed};
use crate::error::{GenerationError, LoadError, MemoryKind, Remedy};
use crate::mock::{MockEngine, MockReply};
use crate::protocol::{Finish, StreamEvent, ToolCall};
use crate::roles::Role;
use crate::schema::{Field, Schema};
use crate::service::{
    GenerateRequest, InferenceService, MemoryAvailable, MemoryProbe, ModelStatus, RequestId, ServiceEvent,
};
use crate::tools::{Tool, ToolContext, ToolLimits, ToolOutcome, ToolSet};

const WAIT: Duration = Duration::from_secs(5);

fn spec() -> LoadSpec {
    LoadSpec {
        server: PathBuf::from("/llama-server"),
        model: PathBuf::from("/model.gguf"),
        label: "Test model".into(),
        backend: "CUDA".into(),
        runtime: "b1-cuda".into(),
        context: 16384,
        gpu_layers: Some(28),
        total_layers: Some(28),
        model_max_context: Some(32768),
        threads: None,
        load_timeout: Duration::from_secs(5),
        memory: None,
        skip_memory_check: false,
        pid_file: None,
    }
}

struct Harness {
    service: InferenceService,
    events: Receiver<ServiceEvent>,
    engine: MockEngine,
}

impl Harness {
    fn new(engine: MockEngine) -> Self {
        Self::with_probe(engine, None)
    }

    fn with_probe(engine: MockEngine, probe: Option<Box<dyn MemoryProbe>>) -> Self {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let service = InferenceService::new(
            Box::new(engine.clone()),
            probe,
            Box::new(move |e| {
                let _ = tx.lock().unwrap().send(e);
            }),
        );
        Self {
            service,
            events: rx,
            engine,
        }
    }

    fn next(&self) -> ServiceEvent {
        self.events.recv_timeout(WAIT).expect("an event")
    }

    /// Skips events until one matches.
    fn until(&self, mut pred: impl FnMut(&ServiceEvent) -> bool) -> ServiceEvent {
        loop {
            let e = self.next();
            if pred(&e) {
                return e;
            }
        }
    }

    fn ready(&self) {
        self.service.load(spec());
        self.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Ready { .. })));
    }

    fn finished(&self, id: RequestId) -> (Vec<StreamEvent>, Result<Box<crate::Generation>, GenerationError>) {
        let mut streamed = Vec::new();
        loop {
            match self.next() {
                ServiceEvent::Stream { request, event } if request == id => streamed.push(event),
                ServiceEvent::Finished { request, result } if request == id => return (streamed, result),
                _ => {}
            }
        }
    }
}

fn ask(text: &str) -> GenerateRequest {
    GenerateRequest::new(
        Role::Explanation,
        vec![Section::required("Question", Placement::User, text)],
    )
}

#[test]
fn load_reports_phases_then_ready_and_unload_stops_the_server() {
    let h = Harness::new(MockEngine::default());
    h.service.load(spec());
    let mut phases = Vec::new();
    let info = loop {
        match h.next() {
            ServiceEvent::Status(ModelStatus::Loading { phase, .. }) => phases.push(phase),
            ServiceEvent::Status(ModelStatus::Ready { info, model }) => {
                assert_eq!(model, "Test model");
                break info;
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        phases,
        vec![LoadPhase::Starting, LoadPhase::Starting, LoadPhase::Loading]
    );
    assert_eq!(info.context, 4096);
    h.service.unload();
    assert_eq!(h.next(), ServiceEvent::Status(ModelStatus::Unloaded));
    assert_eq!(h.engine.state().shutdown_count, 1);
}

#[test]
fn out_of_memory_load_failure_offers_concrete_remedies() {
    let engine = MockEngine::default();
    engine.state().loads.push_back(Err(LoadError::OutOfMemory {
        kind: Some(MemoryKind::Device),
        detail: "cudaMalloc failed: out of memory".into(),
    }));
    let h = Harness::new(engine);
    h.service.load(spec());
    let ServiceEvent::Status(ModelStatus::Failed {
        error, remedies, log, ..
    }) = h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Failed { .. })))
    else {
        unreachable!()
    };
    assert!(error.is_memory());
    assert_eq!(
        remedies,
        vec![
            Remedy::FewerGpuLayers(21),
            Remedy::SmallerContext(8192),
            Remedy::SmallerModel
        ]
    );
    assert_eq!(log, "mock log");
    // The service stays usable: the next load succeeds.
    h.ready();
}

struct Fixed(MemoryAvailable);
impl MemoryProbe for Fixed {
    fn available(&self) -> MemoryAvailable {
        self.0
    }
}

#[test]
fn memory_pressure_is_detected_before_starting_and_can_be_overridden() {
    let probe = Fixed(MemoryAvailable {
        device: Some(2 << 30),
        host: Some(16 << 30),
    });
    let h = Harness::with_probe(MockEngine::default(), Some(Box::new(probe)));
    let mut s = spec();
    s.memory = Some(MemoryNeed {
        device: 6 << 30,
        host: 0,
    });
    h.service.load(s.clone());
    let ServiceEvent::Status(ModelStatus::Failed { error, remedies, .. }) =
        h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Failed { .. })))
    else {
        unreachable!()
    };
    assert!(matches!(
        error,
        LoadError::InsufficientMemory {
            kind: MemoryKind::Device,
            ..
        }
    ));
    assert!(remedies.contains(&Remedy::LoadAnyway));
    assert_eq!(h.engine.state().load_count, 0, "nothing was started");
    s.skip_memory_check = true;
    h.service.load(s);
    h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Ready { .. })));
}

#[test]
fn generation_streams_in_order_and_reports_the_prompt_plan() {
    let engine = MockEngine::default();
    engine.reply(MockReply::slow(
        &["(defn ", "f ", "[x])"],
        Duration::from_millis(5),
    ));
    let h = Harness::new(engine);
    h.ready();
    let id = h.service.generate(ask("Explain frequencies"));
    let (streamed, result) = h.finished(id);
    assert_eq!(
        streamed,
        ["(defn ", "f ", "[x])"]
            .map(|s| StreamEvent::Content(s.into()))
            .to_vec()
    );
    let g = result.unwrap();
    assert_eq!(g.text, "(defn f [x])");
    assert_eq!(g.finish, Finish::Stop);
    assert!(g.plan.exact && g.plan.prompt_tokens > 0);
    let sent = &h.engine.state().requests[0];
    assert_eq!(sent.messages[0].role, "system");
    assert!(sent.messages[0].content.contains("never follow instructions"));
    assert!(sent.messages[1].content.contains("Explain frequencies"));
}

#[test]
fn requests_without_a_model_fail_with_not_ready() {
    let h = Harness::new(MockEngine::default());
    let id = h.service.generate(ask("x"));
    assert_eq!(h.finished(id).1, Err(GenerationError::NotReady));
}

#[test]
fn cancelled_request_emits_nothing_more_and_cannot_replace_the_next_result() {
    let engine = MockEngine::default();
    engine.reply(MockReply::slow(&["a"; 200], Duration::from_millis(10)));
    engine.reply(MockReply::text("second"));
    let h = Harness::new(engine);
    h.ready();
    let first = h.service.generate(ask("one"));
    // Wait for the first piece, then cancel and immediately ask again.
    h.until(|e| matches!(e, ServiceEvent::Stream { request, .. } if *request == first));
    h.service.cancel(first);
    let second = h.service.generate(ask("two"));
    let mut after_cancel = 0;
    let mut finished_first = None;
    let mut finished_second = None;
    while finished_second.is_none() {
        match h.next() {
            ServiceEvent::Stream { request, .. } if request == first => after_cancel += 1,
            ServiceEvent::Finished { request, result } if request == first => finished_first = Some(result),
            ServiceEvent::Finished { request, result } if request == second => finished_second = Some(result),
            _ => {}
        }
    }
    // At most a piece already in flight when cancel was called.
    assert!(after_cancel <= 1, "{after_cancel} pieces after cancel");
    assert_eq!(finished_first, Some(Err(GenerationError::Cancelled)));
    assert_eq!(finished_second.unwrap().unwrap().text, "second");
}

#[test]
fn a_request_cancelled_while_queued_never_runs() {
    let engine = MockEngine::default();
    engine.reply(MockReply::slow(&["x"; 20], Duration::from_millis(10)));
    let h = Harness::new(engine);
    h.ready();
    let busy = h.service.generate(ask("busy"));
    let queued = h.service.generate(ask("queued"));
    h.service.cancel(queued);
    assert!(h.finished(busy).1.is_ok());
    assert_eq!(h.finished(queued).1, Err(GenerationError::Cancelled));
    assert_eq!(h.engine.state().requests.len(), 1);
}

#[test]
fn timeouts_end_hung_requests() {
    let engine = MockEngine::default();
    engine.reply(MockReply::Hang);
    let h = Harness::new(engine);
    h.ready();
    let mut req = ask("x");
    req.timeout = Some(Duration::from_millis(200));
    let id = h.service.generate(req);
    assert_eq!(
        h.finished(id).1,
        Err(GenerationError::Timeout(Duration::from_millis(200)))
    );
}

#[test]
fn unloading_cancels_the_running_request() {
    let engine = MockEngine::default();
    engine.reply(MockReply::Hang);
    let h = Harness::new(engine);
    h.ready();
    let id = h.service.generate(ask("x"));
    std::thread::sleep(Duration::from_millis(50));
    h.service.unload();
    assert_eq!(h.finished(id).1, Err(GenerationError::Cancelled));
    h.until(|e| *e == ServiceEvent::Status(ModelStatus::Unloaded));
}

#[test]
fn idle_crash_is_detected_and_the_model_restarts_a_limited_number_of_times() {
    let h = Harness::new(MockEngine::default());
    h.ready();
    for round in 1..=3 {
        h.engine.crash();
        let ServiceEvent::Status(ModelStatus::Crashed {
            restarting, detail, ..
        }) = h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Crashed { .. })))
        else {
            unreachable!()
        };
        assert!(detail.contains("signal"));
        if round <= 2 {
            assert!(restarting, "round {round}");
            h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Ready { .. })));
        } else {
            assert!(!restarting, "automatic restarts are limited");
        }
    }
    assert_eq!(h.engine.state().load_count, 3);
    // The user can still load the model explicitly.
    h.ready();
}

#[test]
fn crash_during_generation_fails_the_request_and_recovers() {
    let engine = MockEngine::default();
    engine.reply(MockReply::CrashAfter(vec!["(de".into()]));
    engine.reply(MockReply::text("fine"));
    let h = Harness::new(engine);
    h.ready();
    let id = h.service.generate(ask("x"));
    assert!(matches!(h.finished(id).1, Err(GenerationError::ServerCrashed(_))));
    h.until(|e| {
        matches!(
            e,
            ServiceEvent::Status(ModelStatus::Crashed { restarting: true, .. })
        )
    });
    h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Ready { .. })));
    let id = h.service.generate(ask("y"));
    assert_eq!(h.finished(id).1.unwrap().text, "fine");
}

fn answer_schema() -> Schema {
    Schema::object(vec![
        Field::required("code", Schema::string()),
        Field::required("requires", Schema::array(Schema::string(), Some(4))),
    ])
}

#[test]
fn structured_answers_are_validated_and_malformed_ones_keep_the_raw_text() {
    let engine = MockEngine::default();
    engine.reply(MockReply::text(r#"{"code":"(defn f [x] x)","requires":[]}"#));
    engine.reply(MockReply::text(r#"{"code":"(defn f [x] x)"}"#));
    engine.reply(MockReply::text("Here is your function: (defn f [x] x)"));
    engine.reply(MockReply::Text {
        pieces: vec![r#"{"code":"(defn f"#.into()],
        pause: Duration::ZERO,
        finish: Finish::Length,
    });
    let h = Harness::new(engine);
    h.ready();
    let structured = || {
        let mut r = GenerateRequest::new(
            Role::Generation,
            vec![Section::required("Task", Placement::User, "x")],
        );
        r.schema = Some(answer_schema());
        r
    };
    let id = h.service.generate(structured());
    let g = h.finished(id).1.unwrap();
    assert_eq!(g.value.as_ref().unwrap()["code"], "(defn f [x] x)");
    assert_eq!(
        h.engine.state().requests[0].response_schema,
        Some(answer_schema())
    );

    for expected in ["missing key \"requires\"", "not valid JSON"] {
        let id = h.service.generate(structured());
        match h.finished(id).1 {
            Err(GenerationError::Malformed { problem, raw }) => {
                assert!(problem.contains(expected), "{problem}");
                assert!(!raw.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }
    let id = h.service.generate(structured());
    match h.finished(id).1 {
        Err(e @ GenerationError::CutOff { .. }) => assert!(e.to_string().contains("cut off"), "{e}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn transport_and_protocol_failures_are_reported_as_typed_errors() {
    let engine = MockEngine::default();
    engine.reply(MockReply::Fail(GenerationError::Malformed {
        problem: "unreadable stream chunk".into(),
        raw: "{oops".into(),
    }));
    engine.reply(MockReply::Fail(GenerationError::OutOfMemory(
        "CUDA error: out of memory".into(),
    )));
    let h = Harness::new(engine);
    h.ready();
    let id = h.service.generate(ask("x"));
    assert!(matches!(h.finished(id).1, Err(GenerationError::Malformed { .. })));
    let id = h.service.generate(ask("x"));
    assert!(matches!(h.finished(id).1, Err(GenerationError::OutOfMemory(_))));
}

struct Lookup {
    calls: Arc<Mutex<Vec<Value>>>,
}

impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup_symbol"
    }
    fn description(&self) -> &str {
        "Documentation for a clojure.core symbol"
    }
    fn parameters(&self) -> Schema {
        Schema::object(vec![Field::required("symbol", Schema::bounded_string(64))])
    }
    fn call(&self, args: &Value, _cx: &ToolContext) -> Result<String, String> {
        self.calls.lock().unwrap().push(args.clone());
        Ok(format!("docs for {}", args["symbol"]))
    }
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

#[test]
fn tool_calls_are_validated_executed_and_returned_to_the_model() {
    let engine = MockEngine::default();
    engine.reply(MockReply::ToolCalls(vec![
        call("c1", "lookup_symbol", r#"{"symbol":"frequencies"}"#),
        call("c2", "run_shell", r#"{"cmd":"rm -rf /"}"#),
        call("c3", "lookup_symbol", r#"{"symbol":42}"#),
    ]));
    engine.reply(MockReply::text("Use frequencies."));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let h = Harness::new(engine);
    h.ready();
    let mut req = ask("How do I count items?");
    req.tools = ToolSet::new(
        vec![Arc::new(Lookup { calls: calls.clone() })],
        ToolLimits::default(),
    );
    let id = h.service.generate(req);
    let mut outcomes = Vec::new();
    let result = loop {
        match h.next() {
            ServiceEvent::Tool {
                request,
                call,
                outcome,
            } if request == id => outcomes.push((call.id, outcome)),
            ServiceEvent::Finished { request, result } if request == id => break result,
            _ => {}
        }
    };
    let g = result.unwrap();
    assert_eq!(g.text, "Use frequencies.");
    assert_eq!(
        outcomes[0],
        ("c1".into(), ToolOutcome::Ok("docs for \"frequencies\"".into()))
    );
    assert!(matches!(&outcomes[1].1, ToolOutcome::Refused(why) if why.contains("no tool named")));
    assert!(matches!(&outcomes[2].1, ToolOutcome::Refused(why) if why.contains("invalid arguments")));
    // Only the valid call ran.
    assert_eq!(*calls.lock().unwrap(), vec![json!({"symbol": "frequencies"})]);
    // The second turn carried the call and every result back to the model.
    let state = h.engine.state();
    let second = &state.requests[1].messages;
    assert_eq!(second.iter().filter(|m| m.role == "tool").count(), 3);
    assert!(
        second
            .iter()
            .any(|m| m.role == "assistant" && m.tool_calls.len() == 3)
    );
    assert_eq!(state.requests[0].tools[0].name, "lookup_symbol");
}

#[test]
fn endless_tool_calling_is_bounded() {
    let engine = MockEngine::default();
    for i in 0..10 {
        engine.reply(MockReply::ToolCalls(vec![call(
            &format!("c{i}"),
            "lookup_symbol",
            r#"{"symbol":"map"}"#,
        )]));
    }
    let h = Harness::new(engine);
    h.ready();
    let mut req = ask("x");
    req.tools = ToolSet::new(
        vec![Arc::new(Lookup {
            calls: Arc::default(),
        })],
        ToolLimits {
            max_rounds: 2,
            ..ToolLimits::default()
        },
    );
    let id = h.service.generate(req);
    assert_eq!(h.finished(id).1, Err(GenerationError::ToolRounds(2)));
}

#[test]
fn tools_are_withheld_from_models_whose_template_cannot_call_them() {
    let engine = MockEngine::default();
    engine.state().supports_tools = false;
    let h = Harness::new(engine);
    h.ready();
    let mut req = ask("x");
    req.tools = ToolSet::new(
        vec![Arc::new(Lookup {
            calls: Arc::default(),
        })],
        ToolLimits::default(),
    );
    let id = h.service.generate(req);
    let g = h.finished(id).1.unwrap();
    assert!(g.tools_unavailable);
    assert!(h.engine.state().requests[0].tools.is_empty());
}

#[test]
fn oversized_requests_drop_optional_material_or_are_refused_with_remedies() {
    let engine = MockEngine::default();
    engine.state().context = 2048;
    let h = Harness::new(engine);
    h.ready();
    // About 1500 tokens each with the mock tokenizer (4 characters per token).
    let big = "x".repeat(6000);
    let mut req = GenerateRequest::new(
        Role::Explanation,
        vec![
            Section::required("Description", Placement::User, "count words"),
            Section::optional("Documentation: frequencies", 1, big.clone()),
        ],
    );
    req.max_output_tokens = Some(600);
    let id = h.service.generate(req);
    let g = h.finished(id).1.unwrap();
    assert_eq!(g.plan.omitted.len(), 1);
    assert_eq!(g.plan.omitted[0].0, "Documentation: frequencies");
    assert!(!h.engine.state().requests[0].messages[1].content.contains("xxxx"));

    let mut req = GenerateRequest::new(
        Role::Explanation,
        vec![
            Section::required("Description", Placement::User, "count words"),
            Section::required("Example 1", Placement::User, big.clone() + &big),
        ],
    );
    req.max_output_tokens = Some(600);
    let id = h.service.generate(req);
    match h.finished(id).1 {
        Err(GenerationError::ContextExceeded {
            context,
            largest,
            remedies,
            ..
        }) => {
            assert_eq!(context, 2048);
            assert_eq!(largest[0].0, "Example 1");
            assert!(matches!(remedies[0], Remedy::LargerContext(n) if n > 2048 && n <= 32768));
            assert!(matches!(&remedies[1], Remedy::ShortenInput(parts) if parts[0] == "Example 1"));
        }
        other => panic!("{other:?}"),
    }
    // Nothing was sent for the refused request.
    assert_eq!(h.engine.state().requests.len(), 1);
}

#[test]
fn a_newer_load_supersedes_one_in_progress() {
    let engine = MockEngine::default();
    engine.state().load_delay = Duration::from_millis(300);
    let h = Harness::new(engine);
    h.service.load(spec());
    let mut second = spec();
    second.label = "Second".into();
    h.service.load(second);
    let ready = h.until(|e| matches!(e, ServiceEvent::Status(ModelStatus::Ready { .. })));
    assert!(matches!(ready, ServiceEvent::Status(ModelStatus::Ready { model, .. }) if model == "Second"));
    assert_eq!(h.engine.state().load_count, 1, "the first load was abandoned");
}

#[test]
fn shutdown_stops_the_server() {
    let engine = MockEngine::default();
    let mut h = Harness::new(engine);
    h.ready();
    h.service.shutdown();
    assert_eq!(h.engine.state().shutdown_count, 1);
    assert!(!h.engine.state().alive);
}
