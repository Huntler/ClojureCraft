//! Application-defined tools the model may call.
//!
//! Only tools registered here exist for the model; there is no shell, file, or network tool.
//! Every call is checked against the tool's argument schema before it runs, runs with a time
//! limit, and its result is truncated to a fixed size before it is shown to the model. Calls are
//! limited per turn and per request.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::http::Cancel;
use crate::protocol::{ToolCall, ToolSpec};
use crate::schema::Schema;

/// What a running tool may consult to stop early.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub cancel: Cancel,
    pub deadline: Instant,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Schema;
    /// Runs the tool with arguments that already passed validation. The text returned is shown
    /// to the model as reference data.
    fn call(&self, args: &Value, cx: &ToolContext) -> Result<String, String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLimits {
    /// Model turns that may request tools before the request fails.
    pub max_rounds: u32,
    /// Calls honoured per turn; further calls in the same turn are refused.
    pub max_calls_per_round: usize,
    pub call_timeout: Duration,
    pub max_result_chars: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_rounds: 4,
            max_calls_per_round: 4,
            call_timeout: Duration::from_secs(5),
            max_result_chars: 8_000,
        }
    }
}

/// How a call ended; refused and failed calls are reported back to the model as errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok(String),
    /// The call was not run: unknown tool, invalid arguments, or over the per-turn limit.
    Refused(String),
    Failed(String),
    TimedOut,
}

impl ToolOutcome {
    /// The text sent back to the model as the tool's result.
    pub fn message(&self) -> String {
        match self {
            Self::Ok(t) => t.clone(),
            Self::Refused(why) => format!("error: call refused: {why}"),
            Self::Failed(why) => format!("error: {why}"),
            Self::TimedOut => "error: the tool did not finish in time".into(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolSet {
    tools: Vec<Arc<dyn Tool>>,
    pub limits: ToolLimits,
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSet")
            .field(
                "tools",
                &self.tools.iter().map(|t| t.name().to_owned()).collect::<Vec<_>>(),
            )
            .field("limits", &self.limits)
            .finish()
    }
}

impl ToolSet {
    pub fn new(tools: Vec<Arc<dyn Tool>>, limits: ToolLimits) -> Self {
        Self { tools, limits }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_owned(),
                description: t.description().to_owned(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// Validates and runs one call.
    pub fn execute(&self, call: &ToolCall, cancel: &Cancel) -> ToolOutcome {
        let Some(tool) = self.tools.iter().find(|t| t.name() == call.name) else {
            return ToolOutcome::Refused(format!("there is no tool named {:?}", call.name));
        };
        let args: Value = if call.arguments.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            match serde_json::from_str(&call.arguments) {
                Ok(v) => v,
                Err(e) => return ToolOutcome::Refused(format!("arguments are not valid JSON ({e})")),
            }
        };
        if let Err(e) = tool.parameters().validate(&args) {
            return ToolOutcome::Refused(format!("invalid arguments: {e}"));
        }
        let cx = ToolContext {
            cancel: cancel.clone(),
            deadline: Instant::now() + self.limits.call_timeout,
        };
        let (tx, rx) = mpsc::channel();
        let tool = tool.clone();
        // A tool that overruns is abandoned; its late result is discarded.
        thread::spawn(move || {
            let _ = tx.send(tool.call(&args, &cx));
        });
        match rx.recv_timeout(self.limits.call_timeout) {
            Ok(Ok(text)) => ToolOutcome::Ok(truncate(&text, self.limits.max_result_chars)),
            Ok(Err(why)) => ToolOutcome::Failed(truncate(&why, 500)),
            Err(mpsc::RecvTimeoutError::Timeout) => ToolOutcome::TimedOut,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                ToolOutcome::Failed("the tool stopped unexpectedly".into())
            }
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::schema::Field;

    struct Echo;
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Repeats text"
        }
        fn parameters(&self) -> Schema {
            Schema::object(vec![
                Field::required("text", Schema::string()),
                Field::optional("times", Schema::integer(Some(1), Some(3))),
            ])
        }
        fn call(&self, args: &Value, _cx: &ToolContext) -> Result<String, String> {
            let n = args["times"].as_u64().unwrap_or(1);
            let t = args["text"].as_str().unwrap_or_default();
            if t == "fail" {
                return Err("asked to fail".into());
            }
            if t == "slow" {
                thread::sleep(Duration::from_millis(500));
            }
            Ok(t.repeat(usize::try_from(n).unwrap_or(1)))
        }
    }

    fn set() -> ToolSet {
        ToolSet::new(
            vec![Arc::new(Echo)],
            ToolLimits {
                call_timeout: Duration::from_millis(100),
                max_result_chars: 10,
                ..ToolLimits::default()
            },
        )
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn runs_valid_calls_and_truncates_results() {
        let c = Cancel::default();
        assert_eq!(
            set().execute(&call("echo", json!({"text": "ab", "times": 2})), &c),
            ToolOutcome::Ok("abab".into())
        );
        let ToolOutcome::Ok(t) = set().execute(&call("echo", json!({"text": "abcdef", "times": 3})), &c)
        else {
            panic!()
        };
        assert_eq!(t, "abcdefabcd\n[truncated]");
    }

    #[test]
    fn refuses_unknown_tools_and_invalid_arguments_without_running_them() {
        let c = Cancel::default();
        let s = set();
        assert!(matches!(
            s.execute(&call("shell", json!({})), &c),
            ToolOutcome::Refused(_)
        ));
        assert!(matches!(
            s.execute(&call("echo", json!({})), &c),
            ToolOutcome::Refused(_)
        ));
        assert!(matches!(
            s.execute(&call("echo", json!({"text": "x", "times": 9})), &c),
            ToolOutcome::Refused(_)
        ));
        assert!(matches!(
            s.execute(&call("echo", json!({"text": "x", "path": "/etc"})), &c),
            ToolOutcome::Refused(_)
        ));
        let mut bad = call("echo", json!({}));
        bad.arguments = "{\"text\":".into();
        assert!(matches!(s.execute(&bad, &c), ToolOutcome::Refused(_)));
    }

    #[test]
    fn failures_and_overruns_are_reported() {
        let c = Cancel::default();
        assert_eq!(
            set().execute(&call("echo", json!({"text": "fail"})), &c),
            ToolOutcome::Failed("asked to fail".into())
        );
        assert_eq!(
            set().execute(&call("echo", json!({"text": "slow"})), &c),
            ToolOutcome::TimedOut
        );
        assert!(ToolOutcome::TimedOut.message().starts_with("error:"));
    }
}
