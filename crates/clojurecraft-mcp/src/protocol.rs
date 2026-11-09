//! JSON-RPC 2.0 messages of the Model Context Protocol, one per line (the stdio transport).
//!
//! Only the server side of `initialize`, `ping`, `tools/list`, and `tools/call` is implemented;
//! notifications are accepted and ignored. Tool failures are reported as tool results with
//! `isError` so the agent can read and correct them; protocol mistakes are JSON-RPC errors.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use clojurecraft_domain::FunctionId;

use crate::Reply;
use crate::tools::{self, Call, GenerateMode, Parsed};

/// Protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// How often `status` with `wait_seconds` asks again whether the work finished.
const POLL: Duration = Duration::from_millis(500);

/// Read by agents on connection: what the server is for and the intended order of calls.
pub const INSTRUCTIONS: &str = "ClojureCraft writes, tests, and repairs small pure Clojure functions with a \
local model; you write the specification and read checked code. All values (input, expected, keys, \
defaults) are EDN strings, e.g. \"[{:id 1}]\". function_id takes an id, a name, or namespace/function. \
Fast path: create_function with name, description, steps, 2-3 one-case examples, generate=true, \
wait_seconds=300; status in the reply has result.code once all checks pass. Tools that start work \
take wait_seconds and answer with the finished status, whose next field says what to do. On \
failures, fix the specification or tests with update_function or review_test_cases, passing \
generate=true and wait_seconds to regenerate in the same call; after 3 failed generations write the code yourself and check it with submit_code. Several \
functions: create_function with functions=[...], generate with function_ids. export_project writes \
them as a project with tests. With a project open, create_function needs a namespace.";

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
    if !tools::exists(name) {
        return Err((INVALID_PARAMS, format!("unknown tool `{name}`")));
    }
    let args = match resolve(args, handler) {
        Ok(a) => a,
        Err(e) => return Ok(tool_result(Err(e))),
    };
    let parsed = match Call::parse(name, &args) {
        Ok(Some(c)) => c,
        Ok(None) => return Err((INVALID_PARAMS, format!("unknown tool `{name}`"))),
        Err(e) => return Ok(tool_result(Err(format!("Invalid arguments for {name}: {e}")))),
    };
    Ok(tool_result(run(parsed, handler)))
}

/// The arguments with function names in `function_id` and `function_ids` replaced by ids.
fn resolve(args: &Value, handler: &dyn Handler) -> Result<Value, String> {
    let named = |v: &Value| v.as_str().is_some_and(|t| !tools::is_id(t));
    let single = args.get("function_id").filter(|v| named(v));
    let list = args
        .get("function_ids")
        .and_then(Value::as_array)
        .filter(|l| l.iter().any(named));
    let mut args = args.clone();
    if let Some(v) = single {
        let names = vec![v.as_str().unwrap_or_default().to_owned()];
        args["function_id"] = ids(handler, names)?.swap_remove(0);
    }
    if let Some(l) = list {
        let names = l
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect();
        args["function_ids"] = Value::Array(ids(handler, names)?);
    }
    Ok(args)
}

fn ids(handler: &dyn Handler, names: Vec<String>) -> Result<Vec<Value>, String> {
    let n = names.len();
    match handler.call(Call::Resolve(names))? {
        Value::Array(ids) if ids.len() == n => Ok(ids),
        _ => Err("Function names could not be resolved.".into()),
    }
}

/// Carries out the call, generates new functions when asked, and waits for the work it started.
fn run(parsed: Parsed, handler: &dyn Handler) -> Reply {
    let Parsed { call, wait, generate } = parsed;
    let batch = matches!(call, Call::CreateFunctions(_));
    let created_key = if batch || matches!(call, Call::CreateFunction(_)) {
        "created"
    } else {
        "changed"
    };
    let reply = handler.call(call.clone())?;
    let started = if generate {
        let created: Vec<FunctionId> = match (&call, reply.get("functions")) {
            (Call::UpdateFunction(id, _) | Call::ReviewTestCases { function: id, .. }, _) => vec![*id],
            (_, Some(Value::Array(list))) => list.iter().filter_map(|f| id_in(&f["id"])).collect(),
            _ => id_in(&reply["id"]).into_iter().collect(),
        };
        let generation = match created.as_slice() {
            [one] if !batch => Call::Generate(*one, GenerateMode::Fresh),
            _ => Call::GenerateMany(created, GenerateMode::Fresh),
        };
        match handler.call(generation.clone()) {
            Ok(_) => Some(generation),
            Err(e) => return Ok(with(reply, "generate_error", json!(e))),
        }
    } else {
        Some(call)
    };
    let follow = started.as_ref().and_then(Call::follow_up);
    let status = match follow {
        Some(status) if wait > 0 => wait_until_idle(&status, Duration::from_secs(wait), handler)?,
        _ if generate => return Ok(with(reply, "next", json!("Call status with wait_seconds."))),
        _ => return Ok(reply),
    };
    Ok(if generate {
        with(status, created_key, reply)
    } else {
        status
    })
}

fn id_in(v: &Value) -> Option<FunctionId> {
    v.as_str().and_then(|t| t.parse().ok())
}

fn with(mut v: Value, key: &str, value: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert(key.to_owned(), value);
    }
    v
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

    /// Answers like the application: names resolve to one id, a new function gets that id, and
    /// `status` works for the first `busy` calls.
    struct App {
        id: FunctionId,
        calls: Mutex<Vec<Call>>,
        busy: usize,
    }

    impl Handler for App {
        fn call(&self, call: Call) -> Reply {
            let mut calls = self.calls.lock().unwrap();
            calls.push(call.clone());
            let statuses = calls.iter().filter(|c| matches!(c, Call::Status(..))).count();
            match call {
                Call::Resolve(names) if names == ["sum-numbers"] => Ok(json!([self.id.to_string()])),
                Call::Resolve(_) => Err("No function is named that.".into()),
                Call::CreateFunction(_) => {
                    Ok(json!({"id": self.id.to_string(), "readiness": {"ready": true}}))
                }
                Call::Generate(..) => Ok(json!({"started": "planning"})),
                Call::Status(..) => Ok(json!({"working": statuses <= self.busy})),
                _ => Ok(json!({})),
            }
        }
    }

    fn app(busy: usize) -> App {
        App {
            id: FunctionId::generate(),
            calls: Mutex::new(Vec::new()),
            busy,
        }
    }

    fn tool(h: &dyn Handler, name: &str, args: Value) -> Value {
        let r: Value = serde_json::from_str(
            &handle_line(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": name, "arguments": args}})
                .to_string(),
                h,
            )
            .unwrap(),
        )
        .unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        if r["result"]["isError"] == true {
            return json!({"error": text});
        }
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn functions_can_be_named_instead_of_given_by_id() {
        let h = app(0);
        tool(&h, "get_function", json!({"function_id": "sum-numbers"}));
        assert_eq!(h.calls.lock().unwrap().last(), Some(&Call::GetFunction(h.id)));
        let r = tool(&h, "get_function", json!({"function_id": "nope"}));
        assert!(r["error"].as_str().unwrap().contains("No function"));
        tool(&h, "generate", json!({"function_ids": ["sum-numbers"]}));
        assert!(matches!(
            h.calls.lock().unwrap().last(),
            Some(Call::GenerateMany(ids, _)) if ids == &[h.id]
        ));
    }

    #[test]
    fn create_generate_and_wait_is_one_call() {
        let h = app(1);
        let r = tool(
            &h,
            "create_function",
            json!({"name": "Sum numbers", "generate": true, "wait_seconds": 30}),
        );
        assert_eq!(r["working"], false);
        assert_eq!(
            r["created"]["id"],
            h.id.to_string(),
            "the new id is in the answer"
        );
        let calls = h.calls.lock().unwrap();
        assert!(matches!(calls[0], Call::CreateFunction(_)));
        assert_eq!(calls[1], Call::Generate(h.id, GenerateMode::Fresh));
        assert_eq!(calls[2..].len(), 2, "status until the work finished");
    }

    #[test]
    fn a_corrected_specification_is_regenerated_in_the_same_call() {
        let h = app(0);
        let r = tool(
            &h,
            "update_function",
            json!({"function_id": "sum-numbers", "description": "d", "generate": true, "wait_seconds": 30}),
        );
        assert_eq!(r["working"], false);
        assert!(r.get("changed").is_some());
        let calls = h.calls.lock().unwrap();
        assert!(matches!(calls[1], Call::UpdateFunction(..)));
        assert_eq!(calls[2], Call::Generate(h.id, GenerateMode::Fresh));
    }

    #[test]
    fn started_work_can_be_awaited_in_the_same_call() {
        let h = app(0);
        let r = tool(
            &h,
            "run_tests",
            json!({"function_id": "sum-numbers", "wait_seconds": 10}),
        );
        assert_eq!(r["working"], false);
        assert!(matches!(h.calls.lock().unwrap().last(), Some(Call::Status(..))));
        let h = app(0);
        tool(&h, "run_tests", json!({"function_id": "sum-numbers"}));
        assert_eq!(
            h.calls.lock().unwrap().last(),
            Some(&Call::RunTests(h.id)),
            "no wait, no status"
        );
    }
}
