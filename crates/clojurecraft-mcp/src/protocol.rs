//! JSON-RPC 2.0 messages of the Model Context Protocol, one per line (the stdio transport).
//!
//! Only the server side of `initialize`, `ping`, `tools/list`, and `tools/call` is implemented;
//! notifications are accepted and ignored. Tool failures are reported as tool results with
//! `isError` so the agent can read and correct them; protocol mistakes are JSON-RPC errors.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Reply;
use crate::tools::{self, Call};

/// Protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// How often `status` with `wait_seconds` asks again whether the work finished.
const POLL: Duration = Duration::from_millis(500);

/// Read by agents on connection: what the server is for and the intended order of calls.
pub const INSTRUCTIONS: &str = "ClojureCraft writes, tests, and repairs small pure Clojure functions \
(one argument in, one value out, no side effects) with a local model, so you only specify and read \
the result. Typical flow: create_function with a clear description, steps, and several EDN \
examples (input -> expected) -> optionally generate_test_cases, then status with wait_seconds, \
get_function, and review_test_cases (accept good cases, fix expected values, add your own) -> \
generate -> status with wait_seconds until working is false -> result for the code. If checks \
fail, read the failures in status, then improve the specification or tests and generate again, or \
generate with mode=fix; after repeated failures, write the code yourself and check it with \
submit_code. For several functions use create_functions, then generate and status with \
function_ids (generations queue and run one after another). To use the functions in a project, \
export_project writes their code, clojure.test files from the examples and accepted tests, and \
deps.edn.";

/// Carries out tool calls.
pub trait Handler {
    fn call(&self, call: Call) -> Reply;
}

/// Handles one message and returns the response line, if the message expects one.
pub fn handle_line(line: &str, handler: &dyn Handler) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let message: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(Value::Null, PARSE_ERROR, &format!("not JSON: {e}")).to_string()),
    };
    handle(&message, handler).map(|v| v.to_string())
}

fn handle(message: &Value, handler: &dyn Handler) -> Option<Value> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or garbage.
        return message
            .get("id")
            .map(|id| error(id.clone(), INVALID_REQUEST, "expected a request"));
    };
    // Notifications (no id) never get a response.
    let id = message.get("id")?.clone();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::catalogue()})),
        "tools/call" => call(&params, handler),
        _ => Err((METHOD_NOT_FOUND, format!("unknown method `{method}`"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => error(id, code, &message),
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "clojurecraft", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

fn call(params: &Value, handler: &dyn Handler) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "`name` is required".to_owned()))?;
    let args = params.get("arguments").unwrap_or(&Value::Null);
    let (call, wait) = match Call::parse(name, args) {
        Ok(Some(c)) => c,
        Ok(None) => return Err((INVALID_PARAMS, format!("unknown tool `{name}`"))),
        Err(e) => return Ok(tool_result(Err(format!("Invalid arguments for {name}: {e}")))),
    };
    let reply = if wait > 0 {
        wait_until_idle(&call, Duration::from_secs(wait), handler)
    } else {
        handler.call(call)
    };
    Ok(tool_result(reply))
}

/// Calls `status` until nothing is working on the function or the time is up.
fn wait_until_idle(call: &Call, wait: Duration, handler: &dyn Handler) -> Reply {
    let deadline = Instant::now() + wait;
    loop {
        let reply = handler.call(call.clone());
        let working = matches!(&reply, Ok(v) if v.get("working") == Some(&Value::Bool(true)));
        if !working || Instant::now() + POLL > deadline {
            return reply;
        }
        std::thread::sleep(POLL);
    }
}

fn tool_result(reply: Reply) -> Value {
    match reply {
        Ok(value) => json!({
            "content": [{"type": "text", "text": value.to_string()}],
            "isError": false,
        }),
        Err(message) => json!({
            "content": [{"type": "text", "text": message}],
            "isError": true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use clojurecraft_domain::FunctionId;

    use super::*;

    /// Answers `status` as working for the first `busy` calls.
    struct Fake {
        calls: Mutex<Vec<Call>>,
        busy: usize,
    }

    impl Handler for Fake {
        fn call(&self, call: Call) -> Reply {
            let mut calls = self.calls.lock().unwrap();
            calls.push(call);
            Ok(json!({"working": calls.len() <= self.busy}))
        }
    }

    fn fake(busy: usize) -> Fake {
        Fake {
            calls: Mutex::new(Vec::new()),
            busy,
        }
    }

    fn send(h: &Fake, v: Value) -> Option<Value> {
        handle_line(&v.to_string(), h).map(|s| serde_json::from_str(&s).unwrap())
    }

    #[test]
    fn initialize_negotiates_the_version_and_lists_tools() {
        let h = fake(0);
        let r = send(
            &h,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2025-03-26", "capabilities": {}}}),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = send(
            &h,
            json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": "1999"}}),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);

        assert!(
            send(
                &h,
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none(),
            "notifications get no response"
        );
        let r = send(&h, json!({"jsonrpc": "2.0", "id": "x", "method": "tools/list"})).unwrap();
        assert_eq!(r["id"], "x");
        assert!(r["result"]["tools"].as_array().unwrap().len() > 10);
    }

    #[test]
    fn errors_are_reported_where_the_agent_can_fix_them() {
        let h = fake(0);
        assert_eq!(
            serde_json::from_str::<Value>(&handle_line("{nope", &h).unwrap()).unwrap()["error"]["code"],
            PARSE_ERROR
        );
        let r = send(&h, json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"})).unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let r = send(
            &h,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "fly"}}),
        )
        .unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = send(
            &h,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": "status", "arguments": {}}}),
        )
        .unwrap();
        assert_eq!(
            r["result"]["isError"], true,
            "bad arguments are a tool error the agent sees"
        );
        assert!(h.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn status_can_wait_for_the_work_to_finish() {
        let h = fake(2);
        let r = send(
            &h,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": "status", "arguments":
                       {"function_id": FunctionId::generate().to_string(), "wait_seconds": 30}}}),
        )
        .unwrap();
        let text: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["working"], false);
        assert_eq!(h.calls.lock().unwrap().len(), 3);
    }
}
