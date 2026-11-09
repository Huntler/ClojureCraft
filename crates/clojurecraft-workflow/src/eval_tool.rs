//! A tool that lets the model evaluate Clojure expressions against its own code while it finds
//! out why the code failed, for example `(parse-weight "5kg")`. The expression runs in the
//! sandbox like every check: the code that was checked, plus one private function whose body is
//! the expression, with the same static checks and limits.

use std::sync::Arc;
use std::time::Duration;

use clojurecraft_clojure::{Call, CallStatus, ClojureRuntime, EvalRequest, Limits};
use clojurecraft_inference::{Field, Schema, Tool, ToolContext, ToolLimits, ToolSet};
use serde_json::Value;

pub const NAME: &str = "clojure_eval";

/// The function the expression becomes the body of.
const PROBE_FN: &str = "clojurecraft-eval-expression";

/// Characters of a printed result shown to the model.
const RESULT_CHARS: usize = 1500;

pub struct EvalTool {
    runtime: ClojureRuntime,
    /// The code the expression is evaluated with.
    code: String,
    /// A project function's document: the expression then runs in its whole namespace file.
    project: Option<clojurecraft_domain::FunctionDocument>,
}

impl EvalTool {
    /// The tool set of an analysis: this tool, a few calls per turn, a sandbox-sized time limit.
    pub fn set(runtime: ClojureRuntime, code: &str, doc: &clojurecraft_domain::FunctionDocument) -> ToolSet {
        ToolSet::new(
            vec![Arc::new(Self {
                runtime,
                code: code.to_owned(),
                project: doc.project.is_some().then(|| doc.clone()),
            })],
            ToolLimits {
                max_rounds: 4,
                max_calls_per_round: 3,
                call_timeout: Duration::from_secs(30),
                max_result_chars: 2000,
            },
        )
    }

    /// The code with the expression as the body of a private function.
    fn program(&self, expression: &str) -> String {
        format!(
            "{}\n\n(defn- {PROBE_FN} [_]\n  {})\n",
            self.code.trim_end(),
            expression.trim()
        )
    }
}

impl Tool for EvalTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Evaluates one Clojure expression in the namespace of the code that just ran, so its \
         helpers and required aliases can be used, and returns the printed value or the error. Use \
         it to check what an expression returns for a value from the failing call, for example \
         (parse-weight \"5kg\") or (re-find #\"\\d+\" \"5kg\")."
    }

    fn parameters(&self) -> Schema {
        Schema::object(vec![
            Field::required("expression", Schema::bounded_string(1000))
                .describe("One Clojure expression, such as (my-helper \"some input\")"),
        ])
    }

    fn call(&self, args: &Value, _cx: &ToolContext) -> Result<String, String> {
        let expression = args
            .get("expression")
            .and_then(Value::as_str)
            .ok_or("no expression")?;
        let mut request = EvalRequest::new(
            self.program(expression),
            PROBE_FN,
            vec![Call {
                input: "nil".into(),
                expected: None,
            }],
        );
        request.lint = false;
        request.call_timeout = Duration::from_secs(3);
        request.output_chars = Some(RESULT_CHARS);
        if let Some(doc) = &self.project {
            crate::project::prepare(doc, &mut request)?;
        }
        // A cancelled request abandons the call (see `ToolSet::execute`); the sandbox's own
        // limits end the process.
        let outcome = self
            .runtime
            .evaluate(&request, &Limits::for_request(&request), None);
        let report = outcome.map_err(|e| e.to_string())?.report;
        for (stage, result) in [
            ("read", &report.read),
            ("check", &report.check),
            ("load", &report.load),
        ] {
            if let Some(Err(e)) = result {
                return Err(format!("the {stage} step failed: {}", e.message));
            }
        }
        let call = report.results.first().ok_or("the expression did not run")?;
        match (call.status, &call.error) {
            (CallStatus::Ok, _) => Ok(format!(
                "{expression} => {}{}",
                call.actual.as_deref().unwrap_or("nil"),
                if call.truncated { " … (shortened)" } else { "" }
            )),
            (_, Some(e)) => Ok(format!("{expression} threw {}: {}", e.class, e.message)),
            (status, None) => Ok(format!("{expression} did not return a value ({status:?})")),
        }
    }
}

/// The evaluations of an analysis, as lines to keep in the conversation with it.
pub fn transcript(
    calls: &[(
        clojurecraft_inference::ToolCall,
        clojurecraft_inference::ToolOutcome,
    )],
) -> String {
    calls
        .iter()
        .filter(|(c, _)| c.name == NAME)
        .map(|(_, outcome)| format!("- {}", outcome.message()))
        .collect::<Vec<_>>()
        .join("\n")
}
