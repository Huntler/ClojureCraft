//! The OpenAI-compatible chat protocol as spoken by `llama-server`: request bodies, streamed
//! chunks (answer text, reasoning text, tool calls, usage), and error responses.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::GenerationError;
use crate::schema::Schema;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments exactly as the model produced them (JSON text, possibly invalid).
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "wire_calls")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn system(c: impl Into<String>) -> Self {
        Self::new("system", c)
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self::new("user", c)
    }
    pub fn assistant(c: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls,
            ..Self::new("assistant", c)
        }
    }
    pub fn tool_result(call_id: &str, c: impl Into<String>) -> Self {
        Self {
            tool_call_id: Some(call_id.into()),
            ..Self::new("tool", c)
        }
    }
}

/// Tool calls in the wire shape `{id, type, function: {name, arguments}}`.
mod wire_calls {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::ToolCall;

    #[derive(Serialize, Deserialize)]
    struct Function {
        name: String,
        arguments: String,
    }
    #[derive(Serialize, Deserialize)]
    struct Wire {
        id: String,
        #[serde(rename = "type")]
        kind: String,
        function: Function,
    }

    pub fn serialize<S: Serializer>(calls: &[ToolCall], s: S) -> Result<S::Ok, S::Error> {
        calls
            .iter()
            .map(|c| Wire {
                id: c.id.clone(),
                kind: "function".into(),
                function: Function {
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                },
            })
            .collect::<Vec<_>>()
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ToolCall>, D::Error> {
        Ok(Vec::<Wire>::deserialize(d)?
            .into_iter()
            .map(|w| ToolCall {
                id: w.id,
                name: w.function.name,
                arguments: w.function.arguments,
            })
            .collect())
    }
}

/// A tool as advertised to the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Schema,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Nucleus sampling; the server's default when `None`.
    pub top_p: Option<f32>,
    /// Constrains the answer to JSON matching this schema (llama.cpp grammar sampling).
    pub response_schema: Option<Schema>,
    /// A GBNF grammar that constrains the answer more tightly than `response_schema` can; sent
    /// instead of the schema (llama.cpp refuses both at once). The answer is still validated
    /// against `response_schema`.
    pub grammar: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub seed: Option<u64>,
}

impl ChatRequest {
    pub fn body(&self, stream: bool) -> Value {
        let mut body = json!({
            "messages": self.messages,
            "max_tokens": self.max_tokens,
            "temperature": self.temperature,
            "stream": stream,
            // The prompt prefix is reused within the single server slot across repair rounds.
            "cache_prompt": true,
        });
        if stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        if let Some(p) = self.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(grammar) = &self.grammar {
            body["grammar"] = json!(grammar);
        } else if let Some(schema) = &self.response_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {"name": "answer", "strict": true, "schema": schema.to_json()},
            });
        }
        if !self.tools.is_empty() {
            body["tools"] = Value::Array(
                self.tools
                    .iter()
                    .map(|t| {
                        json!({"type": "function", "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters.to_json(),
                        }})
                    })
                    .collect(),
            );
        }
        if let Some(seed) = self.seed {
            body["seed"] = json!(seed);
        }
        body
    }

    /// The body for `/apply-template`, which renders the prompt without generating.
    pub fn template_body(&self) -> Value {
        let mut b = self.body(false);
        if let Some(o) = b.as_object_mut() {
            o.retain(|k, _| k == "messages" || k == "tools");
        }
        b
    }
}

/// Something the server streamed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    Content(String),
    /// Text the model produced while reasoning (thinking models), kept apart from the answer.
    Reasoning(String),
    /// The model started a tool call.
    ToolCall {
        name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Stop,
    /// The answer reached the token limit and is cut off.
    Length,
    ToolCalls,
    /// The stream ended without saying why.
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
    pub tokens_per_second: Option<f64>,
}

/// One completed model turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish: Finish,
    pub usage: Usage,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Assembles a streamed turn from `data:` payloads.
#[derive(Default)]
pub struct Accumulator {
    content: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    finish: Option<Finish>,
    usage: Usage,
    done: bool,
}

/// Upper bound on accumulated text, so a runaway stream cannot exhaust memory.
const MAX_TEXT: usize = 4 * 1024 * 1024;

impl Accumulator {
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Handles one line of the event stream; returns what it added.
    pub fn line(&mut self, line: &str) -> Result<Vec<StreamEvent>, GenerationError> {
        let Some(data) = line.strip_prefix("data:") else {
            // Comments (keep-alive pings), event names, and blank separators.
            if let Some(err) = line.strip_prefix("error:") {
                return Err(error_from_body(500, err.trim()));
            }
            return Ok(Vec::new());
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let chunk: Chunk = serde_json::from_str(data).map_err(|e| GenerationError::Malformed {
            problem: format!("unreadable stream chunk ({e})"),
            raw: data.chars().take(400).collect(),
        })?;
        if let Some(err) = chunk.error {
            return Err(error_from_value(500, &err));
        }
        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(t) = choice.delta.content.filter(|t| !t.is_empty()) {
                self.content.push_str(&t);
                events.push(StreamEvent::Content(t));
            }
            if let Some(t) = choice.delta.reasoning_content.filter(|t| !t.is_empty()) {
                self.reasoning.push_str(&t);
                events.push(StreamEvent::Reasoning(t));
            }
            for d in choice.delta.tool_calls {
                let index = d.index.unwrap_or(self.calls.len().saturating_sub(1));
                if index > 64 {
                    return Err(GenerationError::Malformed {
                        problem: "too many tool calls".into(),
                        raw: String::new(),
                    });
                }
                while self.calls.len() <= index {
                    self.calls.push(PartialCall::default());
                }
                let call = &mut self.calls[index];
                if let Some(id) = d.id {
                    call.id = id;
                }
                if let Some(f) = d.function {
                    if let Some(n) = f.name.filter(|n| !n.is_empty()) {
                        call.name.push_str(&n);
                        events.push(StreamEvent::ToolCall {
                            name: call.name.clone(),
                        });
                    }
                    if let Some(a) = f.arguments {
                        call.arguments.push_str(&a);
                    }
                }
            }
            if let Some(f) = choice.finish_reason {
                self.finish = Some(match f.as_str() {
                    "stop" => Finish::Stop,
                    "length" => Finish::Length,
                    "tool_calls" => Finish::ToolCalls,
                    _ => Finish::Unknown,
                });
            }
        }
        if let Some(u) = chunk.usage {
            self.usage.prompt_tokens = u.prompt_tokens;
            self.usage.completion_tokens = u.completion_tokens;
            self.usage.cached_tokens = u.prompt_tokens_details.and_then(|d| d.cached_tokens);
        }
        if let Some(t) = chunk.timings {
            self.usage.tokens_per_second = t.predicted_per_second;
        }
        let total = self.content.len()
            + self.reasoning.len()
            + self.calls.iter().map(|c| c.arguments.len()).sum::<usize>();
        if total > MAX_TEXT {
            return Err(GenerationError::Malformed {
                problem: format!("the answer exceeded {} MiB", MAX_TEXT / (1024 * 1024)),
                raw: String::new(),
            });
        }
        Ok(events)
    }

    pub fn finish(self) -> Turn {
        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .enumerate()
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{i}")
                } else {
                    c.id
                },
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        let finish = match self.finish {
            Some(f) => f,
            None if !tool_calls.is_empty() => Finish::ToolCalls,
            None => Finish::Unknown,
        };
        Turn {
            content: self.content,
            reasoning: self.reasoning,
            tool_calls,
            finish,
            usage: self.usage,
        }
    }
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
    timings: Option<Timings>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<CallDelta>,
}

#[derive(Deserialize)]
struct CallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    prompt_tokens_details: Option<Details>,
}

#[derive(Deserialize)]
struct Details {
    cached_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct Timings {
    predicted_per_second: Option<f64>,
}

fn is_memory_message(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    m.contains("out of memory") || m.contains("failed to allocate") || m.contains("outofdevicememory")
}

fn error_from_value(status: u16, err: &Value) -> GenerationError {
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| err.to_string(), str::to_owned);
    let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "exceed_context_size_error" {
        let n = |k: &str| {
            err.get(k)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0)
        };
        return GenerationError::ContextExceeded {
            needed: n("n_prompt_tokens"),
            context: n("n_ctx"),
            reserved_output: 0,
            largest: Vec::new(),
            remedies: Vec::new(),
        };
    }
    if is_memory_message(&message) {
        return GenerationError::OutOfMemory(message);
    }
    GenerationError::Rejected { status, message }
}

/// Maps an error response body (`{"error": {...}}`) to a typed error.
pub fn error_from_body(status: u16, body: &str) -> GenerationError {
    match serde_json::from_str::<Value>(body) {
        Ok(v) => match v.get("error") {
            Some(e) => error_from_value(status, e),
            None => error_from_value(status, &v),
        },
        Err(_) => {
            let message: String = body.chars().take(400).collect();
            if is_memory_message(&message) {
                GenerationError::OutOfMemory(message)
            } else {
                GenerationError::Rejected { status, message }
            }
        }
    }
}

/// Facts the server reports about the loaded model (`/props`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerProps {
    pub context: Option<u32>,
    pub build: Option<String>,
    pub has_template: bool,
    pub supports_tools: bool,
    pub supports_system_role: bool,
}

pub fn parse_props(body: &str) -> Result<ServerProps, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let caps = v.get("chat_template_caps");
    let cap = |k: &str| {
        caps.and_then(|c| c.get(k))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    Ok(ServerProps {
        context: v
            .pointer("/default_generation_settings/n_ctx")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        build: v.get("build_info").and_then(Value::as_str).map(str::to_owned),
        has_template: v
            .get("chat_template")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty()),
        supports_tools: cap("supports_tools") && cap("supports_tool_calls"),
        supports_system_role: cap("supports_system_role"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(acc: &mut Accumulator, data: &str) -> Vec<StreamEvent> {
        acc.line(&format!("data: {data}")).unwrap()
    }

    #[test]
    fn separates_answer_reasoning_and_usage() {
        let mut a = Accumulator::default();
        assert!(
            feed(
                &mut a,
                r#"{"choices":[{"delta":{"role":"assistant","content":null}}]}"#
            )
            .is_empty()
        );
        assert_eq!(
            feed(&mut a, r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#),
            vec![StreamEvent::Reasoning("think".into())]
        );
        feed(&mut a, r#"{"choices":[{"delta":{"content":"(defn"}}]}"#);
        feed(
            &mut a,
            r#"{"choices":[{"delta":{"content":" f)"},"finish_reason":"stop"}]}"#,
        );
        feed(
            &mut a,
            r#"{"choices":[],"usage":{"prompt_tokens":31,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":8}},"timings":{"predicted_per_second":39.5}}"#,
        );
        assert!(a.line(": ping").unwrap().is_empty());
        assert!(a.line("").unwrap().is_empty());
        feed(&mut a, "[DONE]");
        assert!(a.is_done());
        let t = a.finish();
        assert_eq!(t.content, "(defn f)");
        assert_eq!(t.reasoning, "think");
        assert_eq!(t.finish, Finish::Stop);
        assert_eq!(t.usage.prompt_tokens, Some(31));
        assert_eq!(t.usage.cached_tokens, Some(8));
    }

    #[test]
    fn assembles_tool_calls_streamed_in_fragments() {
        let mut a = Accumulator::default();
        let ev = feed(
            &mut a,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"lookup","arguments":"{\"sym"}}]}}]}"#,
        );
        assert_eq!(
            ev,
            vec![StreamEvent::ToolCall {
                name: "lookup".into()
            }]
        );
        feed(
            &mut a,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"bol\":\"map\"}"}}]}}]}"#,
        );
        feed(
            &mut a,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        );
        let t = a.finish();
        assert_eq!(t.finish, Finish::ToolCalls);
        assert_eq!(
            t.tool_calls,
            vec![ToolCall {
                id: "c1".into(),
                name: "lookup".into(),
                arguments: "{\"symbol\":\"map\"}".into()
            }]
        );
    }

    #[test]
    fn malformed_chunks_and_in_stream_errors_are_typed() {
        let mut a = Accumulator::default();
        assert!(matches!(
            a.line("data: {not json"),
            Err(GenerationError::Malformed { .. })
        ));
        let mut a = Accumulator::default();
        assert!(matches!(
            a.line(r#"data: {"error":{"code":500,"message":"CUDA error: out of memory"}}"#),
            Err(GenerationError::OutOfMemory(_))
        ));
    }

    #[test]
    fn context_overflow_response_is_recognised() {
        let body = r#"{"error":{"code":400,"message":"request (3030 tokens) exceeds the available context size (2048 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":3030,"n_ctx":2048}}"#;
        assert!(matches!(
            error_from_body(400, body),
            GenerationError::ContextExceeded {
                needed: 3030,
                context: 2048,
                ..
            }
        ));
        assert!(matches!(
            error_from_body(500, "plain failure"),
            GenerationError::Rejected { status: 500, .. }
        ));
    }

    #[test]
    fn request_body_carries_schema_tools_and_tool_results() {
        let req = ChatRequest {
            messages: vec![
                Message::user("hi"),
                Message::assistant(
                    "",
                    vec![ToolCall {
                        id: "c1".into(),
                        name: "t".into(),
                        arguments: "{}".into(),
                    }],
                ),
                Message::tool_result("c1", "result"),
            ],
            max_tokens: 10,
            temperature: 0.1,
            top_p: None,
            response_schema: Some(Schema::string()),
            grammar: None,
            tools: vec![ToolSpec {
                name: "t".into(),
                description: "d".into(),
                parameters: Schema::object(vec![]),
            }],
            seed: None,
        };
        let b = req.body(true);
        assert_eq!(b["response_format"]["json_schema"]["schema"]["type"], "string");
        assert_eq!(b["tools"][0]["function"]["name"], "t");
        assert_eq!(b["messages"][1]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(b["messages"][2]["tool_call_id"], "c1");
        assert!(b["messages"][0].get("tool_calls").is_none());
        let t = req.template_body();
        assert!(t.get("max_tokens").is_none() && t.get("tools").is_some());

        // A grammar replaces the schema in the request; llama.cpp accepts only one of them.
        let g = ChatRequest {
            grammar: Some("root ::= \"x\"".into()),
            ..req
        }
        .body(false);
        assert_eq!(g["grammar"], "root ::= \"x\"");
        assert!(g.get("response_format").is_none());
    }

    #[test]
    fn props_report_context_template_and_tool_support() {
        let p = parse_props(
            r#"{"default_generation_settings":{"n_ctx":2048},"chat_template":"{{x}}","build_info":"b1","chat_template_caps":{"supports_tools":true,"supports_tool_calls":true,"supports_system_role":true}}"#,
        )
        .unwrap();
        assert_eq!(p.context, Some(2048));
        assert!(p.has_template && p.supports_tools && p.supports_system_role);
        assert!(!parse_props("{}").unwrap().has_template);
    }
}
