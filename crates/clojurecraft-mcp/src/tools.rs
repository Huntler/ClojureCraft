//! The tool catalogue offered to agents and the typed calls the application carries out.
//!
//! Every tool that changes a function, starts work, or reads results takes the `function_id`
//! returned by `create_function` or `list_functions`. Model work (test-case proposals and
//! generation) runs in the background: the tool returns at once and `status` reports progress,
//! optionally waiting for the work to finish so that the agent does not poll.

use clojurecraft_domain::FunctionId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::views::Detail;

/// The longest `status` may wait for background work before answering.
pub const MAX_WAIT_SECONDS: u64 = 300;

/// A function's specification as an agent writes it. Absent fields are left unchanged.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SpecArgs {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Replaces every step, in order.
    pub steps: Option<Vec<String>>,
    /// Replaces every example, in order.
    pub examples: Option<Vec<ExampleArg>>,
    /// An object sets (and accepts) the contract; `null` removes it; absence keeps it.
    #[serde(default, deserialize_with = "present")]
    pub contract: Option<Option<ContractArg>>,
}

/// Distinguishes an explicit `null` from an absent field.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExampleArg {
    #[serde(default)]
    pub name: String,
    /// The input value as EDN.
    pub input: String,
    /// The expected output as EDN.
    pub expected: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContractArg {
    #[serde(default)]
    pub input: ShapeArg,
    #[serde(default)]
    pub output: ShapeArg,
    /// `unspecified`, `preserve_input`, `unordered`, or `sorted: <how>`.
    #[serde(default)]
    pub ordering: Option<String>,
    /// Namespaces the code may require; defaults to `clojure.string` and `clojure.set`.
    #[serde(default)]
    pub allowed_namespaces: Option<Vec<String>>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ShapeArg {
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "type")]
    pub value_type: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyArg>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KeyArg {
    /// The key as EDN, for example `:id`.
    pub key: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, rename = "type")]
    pub value_type: Option<String>,
    /// `unspecified`, `disallowed`, `treat_as_missing`, `pass_through`, or `default`.
    #[serde(default)]
    pub nil: Option<String>,
    /// The default value as EDN, with `nil: "default"`.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaseSource {
    /// From the title, description, and examples only.
    #[default]
    Examples,
    /// From the whole specification (steps, contract, answers, accepted cases too).
    Specification,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaseVerb {
    Accept,
    Reject,
    Reopen,
    Remove,
    Edit,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CaseAction {
    pub id: String,
    pub action: CaseVerb,
    /// For `edit`: new values; absent fields are kept.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub expected: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NewCase {
    #[serde(default)]
    pub name: String,
    pub input: String,
    pub expected: String,
    /// Accept as a regression test right away (default).
    #[serde(default = "yes")]
    pub accept: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenerateMode {
    /// Write the code from the specification.
    #[default]
    Fresh,
    /// Repair the current code, starting from its failing checks.
    Fix,
}

/// A tool call with validated arguments.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    ListFunctions,
    CreateFunction(SpecArgs),
    CreateFunctions(Vec<SpecArgs>),
    /// Not a tool: answers a create call once its new functions are open.
    Created {
        functions: Vec<FunctionId>,
        batch: bool,
    },
    GetFunction(FunctionId),
    UpdateFunction(FunctionId, SpecArgs),
    GenerateTestCases(FunctionId, CaseSource),
    ReviewTestCases {
        function: FunctionId,
        actions: Vec<CaseAction>,
        add: Vec<NewCase>,
    },
    RunTests(FunctionId),
    /// Fill the empty parts of the specification from the function's code.
    Describe(FunctionId),
    Generate(FunctionId, GenerateMode),
    GenerateMany(Vec<FunctionId>, GenerateMode),
    Status(FunctionId, Detail),
    StatusMany(Vec<FunctionId>),
    Result(FunctionId, Detail),
    AcceptCode(FunctionId),
    /// Code written by the agent, to become the function's code and be checked.
    SubmitCode(FunctionId, String),
    ExportProject(ExportArgs),
    Cancel,
}

/// Where and what `export_project` writes.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportArgs {
    /// Functions to export, in order.
    pub functions: Vec<FunctionId>,
    /// An absolute directory; created when missing.
    pub directory: std::path::PathBuf,
    /// Replace existing files whose content differs.
    pub overwrite: bool,
}

impl Call {
    /// Parses a `tools/call`. `Ok(None)` means the tool does not exist.
    pub fn parse(name: &str, args: &Value) -> Result<Option<(Self, u64)>, String> {
        let args = match args {
            Value::Null => &json!({}),
            Value::Object(_) => args,
            _ => return Err("arguments must be an object".into()),
        };
        let id = || function_id(args);
        let call = match name {
            "list_functions" => Self::ListFunctions,
            "create_function" => {
                let spec: SpecArgs = parse(args)?;
                if spec.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
                    return Err("`name` is required".into());
                }
                Self::CreateFunction(spec)
            }
            "create_functions" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    functions: Vec<Value>,
                }
                let a: A = parse(args)?;
                if a.functions.is_empty() {
                    return Err("`functions` must hold at least one specification".into());
                }
                let mut specs = Vec::new();
                for (i, f) in a.functions.iter().enumerate() {
                    let spec: SpecArgs = parse(f).map_err(|e| format!("functions[{i}]: {e}"))?;
                    if spec.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
                        return Err(format!("functions[{i}]: `name` is required"));
                    }
                    specs.push(spec);
                }
                Self::CreateFunctions(specs)
            }
            "get_function" => Self::GetFunction(id()?),
            "update_function" => {
                let function = id()?;
                Self::UpdateFunction(function, parse(&without(args, &["function_id"]))?)
            }
            "generate_test_cases" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    #[allow(dead_code)]
                    function_id: String,
                    #[serde(default)]
                    source: CaseSource,
                }
                let a: A = parse(args)?;
                Self::GenerateTestCases(id()?, a.source)
            }
            "review_test_cases" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    #[allow(dead_code)]
                    function_id: String,
                    #[serde(default)]
                    actions: Vec<CaseAction>,
                    #[serde(default)]
                    add: Vec<NewCase>,
                }
                let a: A = parse(args)?;
                if a.actions.is_empty() && a.add.is_empty() {
                    return Err("give `actions`, `add`, or both".into());
                }
                Self::ReviewTestCases {
                    function: id()?,
                    actions: a.actions,
                    add: a.add,
                }
            }
            "run_tests" => Self::RunTests(id()?),
            "describe" => Self::Describe(id()?),
            "generate" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    #[allow(dead_code)]
                    function_id: Option<String>,
                    #[allow(dead_code)]
                    function_ids: Option<Vec<String>>,
                    #[serde(default)]
                    mode: GenerateMode,
                }
                let a: A = parse(args)?;
                match one_or_many(args)? {
                    One(f) => Self::Generate(f, a.mode),
                    Many(fs) => Self::GenerateMany(fs, a.mode),
                }
            }
            "status" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    #[allow(dead_code)]
                    function_id: Option<String>,
                    #[allow(dead_code)]
                    function_ids: Option<Vec<String>>,
                    #[serde(default)]
                    wait_seconds: u64,
                    #[serde(default)]
                    verbose: bool,
                }
                let a: A = parse(args)?;
                let call = match one_or_many(args)? {
                    One(f) => Self::Status(f, detail(a.verbose)),
                    Many(fs) => Self::StatusMany(fs),
                };
                return Ok(Some((call, a.wait_seconds.min(MAX_WAIT_SECONDS))));
            }
            "result" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    #[allow(dead_code)]
                    function_id: String,
                    #[serde(default)]
                    verbose: bool,
                }
                let a: A = parse(args)?;
                Self::Result(id()?, detail(a.verbose))
            }
            "accept_code" => Self::AcceptCode(id()?),
            "submit_code" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    #[allow(dead_code)]
                    function_id: String,
                    code: String,
                }
                let a: A = parse(args)?;
                if a.code.trim().is_empty() {
                    return Err("`code` is empty".into());
                }
                Self::SubmitCode(id()?, a.code)
            }
            "export_project" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    function_ids: Vec<String>,
                    directory: String,
                    #[serde(default)]
                    overwrite: bool,
                }
                let a: A = parse(args)?;
                if a.function_ids.is_empty() {
                    return Err("`function_ids` must name at least one function".into());
                }
                let mut functions = Vec::new();
                for text in &a.function_ids {
                    let id: FunctionId = text
                        .trim()
                        .parse()
                        .map_err(|_| format!("`{text}` is not a function id"))?;
                    if !functions.contains(&id) {
                        functions.push(id);
                    }
                }
                let directory = std::path::PathBuf::from(a.directory.trim());
                if !directory.is_absolute() {
                    return Err("`directory` must be an absolute path".into());
                }
                Self::ExportProject(ExportArgs {
                    functions,
                    directory,
                    overwrite: a.overwrite,
                })
            }
            "cancel" => Self::Cancel,
            _ => return Ok(None),
        };
        Ok(Some((call, 0)))
    }
}

fn parse<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|e| e.to_string())
}

fn without(args: &Value, keys: &[&str]) -> Value {
    let mut v = args.clone();
    if let Some(o) = v.as_object_mut() {
        for k in keys {
            o.remove(*k);
        }
    }
    v
}

fn detail(verbose: bool) -> Detail {
    if verbose { Detail::Verbose } else { Detail::Compact }
}

enum Target {
    One(FunctionId),
    Many(Vec<FunctionId>),
}
use Target::{Many, One};

/// `function_id`, or `function_ids` (at least one, duplicates dropped); exactly one of them.
fn one_or_many(args: &Value) -> Result<Target, String> {
    match (args.get("function_id"), args.get("function_ids")) {
        (Some(_), None) => function_id(args).map(One),
        (None, Some(list)) => {
            let list = list.as_array().ok_or("`function_ids` must be an array")?;
            let mut ids = Vec::new();
            for v in list {
                let text = v.as_str().ok_or("`function_ids` must hold strings")?;
                let id: FunctionId = text
                    .trim()
                    .parse()
                    .map_err(|_| format!("`{text}` is not a function id"))?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            if ids.is_empty() {
                return Err("`function_ids` is empty".into());
            }
            Ok(Many(ids))
        }
        _ => Err("give either `function_id` or `function_ids`".into()),
    }
}

fn function_id(args: &Value) -> Result<FunctionId, String> {
    let text = args
        .get("function_id")
        .and_then(Value::as_str)
        .ok_or("`function_id` is required")?;
    text.trim()
        .parse()
        .map_err(|_| format!("`{text}` is not a function id"))
}

// ---- catalogue -----------------------------------------------------------------------------------

const TYPES: &str = "any, nil, boolean, integer, number, string, keyword, symbol, map, vector, list, \
                     set, sequential, or your own words";

fn function_id_schema() -> Value {
    json!({"type": "string", "description": "Function id from create_function or list_functions."})
}

fn ids_schema() -> Value {
    json!({
        "type": "array", "items": {"type": "string"}, "minItems": 1,
        "description": "Several function ids, instead of function_id."
    })
}

fn spec_properties() -> serde_json::Map<String, Value> {
    let shape = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "description": {"type": "string"},
            "type": {"type": "string", "description": format!("Whole value type: {TYPES}.")},
            "keys": {
                "type": "array",
                "description": "Keys of the map, or of each map in a collection of maps.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["key"],
                    "properties": {
                        "key": {"type": "string", "description": "EDN key, e.g. \":id\"."},
                        "required": {"type": "boolean"},
                        "type": {"type": "string", "description": format!("Value type: {TYPES}.")},
                        "nil": {
                            "type": "string",
                            "enum": ["unspecified", "disallowed", "treat_as_missing", "pass_through", "default"]
                        },
                        "default": {"type": "string", "description": "EDN default when nil is \"default\"."},
                        "notes": {"type": "string"}
                    }
                }
            }
        }
    });
    let props = json!({
        "name": {"type": "string", "description": "Title; the Clojure function name is derived from it."},
        "description": {"type": "string", "description": "What the function does, in prose."},
        "steps": {
            "type": "array", "items": {"type": "string"},
            "description": "Transformation steps in order (replaces all steps)."
        },
        "examples": {
            "type": "array",
            "description": "Input/expected-output pairs as EDN (replaces all examples). The function takes one argument.",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["input", "expected"],
                "properties": {
                    "name": {"type": "string"},
                    "input": {"type": "string", "description": "EDN, e.g. \"[{:id 1 :n 2}]\""},
                    "expected": {"type": "string", "description": "EDN, or #throws ExceptionClass when the call must throw (e.g. #throws IllegalArgumentException)"}
                }
            }
        },
        "contract": {
            "type": ["object", "null"],
            "description": "Accepted input/output contract (checked mechanically on outputs); null removes it.",
            "additionalProperties": false,
            "properties": {
                "input": shape.clone(),
                "output": shape,
                "ordering": {
                    "type": "string",
                    "description": "unspecified, preserve_input, unordered (outputs compared as multisets), or \"sorted: <how>\"."
                },
                "allowed_namespaces": {
                    "type": "array", "items": {"type": "string"},
                    "description": "Namespaces the code may require. Default: clojure.string, clojure.set."
                },
                "notes": {"type": "string"}
            }
        }
    });
    match props {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    }
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": properties,
            "required": required,
        }
    })
}

/// The `tools/list` result.
pub fn catalogue() -> Value {
    let create = spec_properties();
    let mut update = spec_properties();
    update.insert("function_id".into(), function_id_schema());
    let only_id = json!({"function_id": function_id_schema()});
    json!([
        tool(
            "list_functions",
            "List saved functions (id, name, description). While a project is open, only its \
             functions (with their namespace) and the project root; others cannot be used then.",
            json!({}),
            &[],
        ),
        tool(
            "create_function",
            "Create a function from a specification. Returns its id and readiness (what still \
             blocks generation). Not available while a project is open.",
            Value::Object(create.clone()),
            &["name"],
        ),
        tool(
            "create_functions",
            "Create several functions at once (all or nothing). Returns each id and readiness, in order.",
            json!({
                "functions": {
                    "type": "array", "minItems": 1,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name"],
                        "properties": Value::Object(create),
                    }
                }
            }),
            &["functions"],
        ),
        tool(
            "get_function",
            "Read a function's specification, test cases (with ids for review), and whether it has code.",
            only_id.clone(),
            &["function_id"],
        ),
        tool(
            "update_function",
            "Change a function's specification. Only the given fields change; steps and examples are replaced as a whole.",
            Value::Object(update),
            &["function_id"],
        ),
        tool(
            "generate_test_cases",
            "Ask the local model to propose edge-case tests (background job; then call status with \
             wait_seconds, and review_test_cases). source=examples (default) uses title, description, \
             and examples; source=specification uses everything.",
            json!({
                "function_id": function_id_schema(),
                "source": {"type": "string", "enum": ["examples", "specification"]}
            }),
            &["function_id"],
        ),
        tool(
            "review_test_cases",
            "Accept, reject, reopen, remove, or edit proposed test cases, and add your own (accepted by \
             default). Accepted cases are regression tests that generation must pass. All-or-nothing.",
            json!({
                "function_id": function_id_schema(),
                "actions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id", "action"],
                        "properties": {
                            "id": {"type": "string", "description": "Test case id from get_function."},
                            "action": {"type": "string", "enum": ["accept", "reject", "reopen", "remove", "edit"]},
                            "name": {"type": "string"},
                            "input": {"type": "string", "description": "EDN (edit)"},
                            "expected": {"type": "string", "description": "EDN (edit); decides an ambiguous case"}
                        }
                    }
                },
                "add": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["input", "expected"],
                        "properties": {
                            "name": {"type": "string"},
                            "input": {"type": "string", "description": "EDN"},
                            "expected": {"type": "string", "description": "EDN, or #throws ExceptionClass when the call must throw"},
                            "accept": {"type": "boolean", "description": "Default true."}
                        }
                    }
                }
            }),
            &["function_id"],
        ),
        tool(
            "run_tests",
            "Run the current code against every example and accepted test case in the sandbox \
             (background; read the outcome with status).",
            only_id.clone(),
            &["function_id"],
        ),
        tool(
            "describe",
            "Let the local model describe the function's existing code (for example code from an \
             opened project): it fills the description and steps when they are empty, and adds up to \
             five examples whose expected values are recorded by running the code (background; then \
             call status with wait_seconds, and get_function). Proposes a contract unless one is \
             accepted.",
            only_id.clone(),
            &["function_id"],
        ),
        tool(
            "generate",
            "Let ClojureCraft write the code: it plans, looks up documentation, generates, validates \
             against examples, accepted tests, and the contract; failing code is fixed in up to five attempts of one \
             conversation, where the model first finds the cause by evaluating expressions in the sandbox \
             (background; then call status with wait_seconds). Give function_ids to generate several: \
             jobs run one after another, and a job for a busy model is queued, not refused. mode=fix \
             repairs the current code from its failing checks (run_tests first).",
            json!({
                "function_id": function_id_schema(),
                "function_ids": ids_schema(),
                "mode": {"type": "string", "enum": ["fresh", "fix"]}
            }),
            &[],
        ),
        tool(
            "status",
            "Progress of background work and the latest checks of the current code (passed, \
             failures with expected/actual values), plus a hint for the next step. With function_ids, \
             one short line per function (outcome, passed, first failure). wait_seconds (max 300) \
             waits until the work (queued jobs included) finishes before answering. Failures are \
             compact (the first of each kind; long values replaced by where they differ); \
             verbose=true lists up to 8 with full values.",
            json!({
                "function_id": function_id_schema(),
                "function_ids": ids_schema(),
                "wait_seconds": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECONDS},
                "verbose": {"type": "boolean"}
            }),
            &[],
        ),
        tool(
            "result",
            "The function's current code (namespace and function name included) and whether it \
             passes its checks; also code waiting for acceptance, if any (verbose=true for all of \
             its failures).",
            json!({"function_id": function_id_schema(), "verbose": {"type": "boolean"}}),
            &["function_id"],
        ),
        tool(
            "accept_code",
            "Use generated code that is waiting for review (for example a fix, code needing another \
             namespace, or the best of failed attempts).",
            only_id,
            &["function_id"],
        ),
        tool(
            "submit_code",
            "Make your own code the function's code (the previous code stays in its history) and run \
             the same checks as generation: parse, structure, lint, load, examples, accepted tests, \
             contract (background; then call status with wait_seconds). Use it when generation keeps \
             failing. The code is one ns form plus one public defn taking one argument; keep the \
             namespace and function name from result.",
            json!({
                "function_id": function_id_schema(),
                "code": {"type": "string", "description": "The whole namespace: (ns …) (defn- …)* (defn … [input] …)."}
            }),
            &["function_id", "code"],
        ),
        tool(
            "export_project",
            "Write functions as a Clojure project into a directory: each function's code under src/, \
             a clojure.test file per function built from its examples and accepted test cases (compared \
             like ClojureCraft's checks) under test/, and deps.edn with a :test alias. Existing files \
             with other content are only replaced with overwrite=true. Returns the files and how to \
             run the tests.",
            json!({
                "function_ids": {
                    "type": "array", "items": {"type": "string"}, "minItems": 1,
                    "description": "Functions to export (ids from create_function or list_functions)."
                },
                "directory": {"type": "string", "description": "Absolute path of the project root; created when missing."},
                "overwrite": {"type": "boolean", "description": "Replace existing files that differ. Default false."}
            }),
            &["function_ids", "directory"],
        ),
        tool(
            "cancel",
            "Cancel the model job in progress and empty the generation queue.",
            json!({}),
            &[],
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> String {
        FunctionId::generate().to_string()
    }

    #[test]
    fn calls_are_parsed_with_their_arguments() {
        let f = id();
        let (call, wait) = Call::parse("status", &json!({"function_id": f, "wait_seconds": 9999}))
            .unwrap()
            .unwrap();
        assert!(matches!(call, Call::Status(_, Detail::Compact)));
        assert_eq!(wait, MAX_WAIT_SECONDS);

        let (call, _) = Call::parse(
            "update_function",
            &json!({"function_id": f, "description": "d", "contract": null}),
        )
        .unwrap()
        .unwrap();
        let Call::UpdateFunction(_, spec) = call else {
            panic!()
        };
        assert_eq!(spec.description.as_deref(), Some("d"));
        assert_eq!(spec.contract, Some(None), "null removes the contract");
        assert_eq!(spec.name, None);

        let (call, _) = Call::parse("update_function", &json!({"function_id": f}))
            .unwrap()
            .unwrap();
        let Call::UpdateFunction(_, spec) = call else {
            panic!()
        };
        assert_eq!(spec.contract, None, "absent keeps it");
    }

    #[test]
    fn several_functions_can_be_named_at_once() {
        let (a, b) = (id(), id());
        let (call, _) = Call::parse("generate", &json!({"function_ids": [a, b, a], "mode": "fix"}))
            .unwrap()
            .unwrap();
        let Call::GenerateMany(ids, GenerateMode::Fix) = call else {
            panic!("{call:?}")
        };
        assert_eq!(ids.len(), 2, "duplicates are dropped");
        let (call, wait) = Call::parse("status", &json!({"function_ids": [a], "wait_seconds": 5}))
            .unwrap()
            .unwrap();
        assert!(matches!(call, Call::StatusMany(_)));
        assert_eq!(wait, 5);
        assert!(Call::parse("status", &json!({"function_id": a, "function_ids": [b]})).is_err());
        assert!(Call::parse("generate", &json!({})).is_err());
        assert!(Call::parse("generate", &json!({"function_ids": []})).is_err());
        let e = Call::parse(
            "create_functions",
            &json!({"functions": [{"name": "a"}, {"description": "x"}]}),
        )
        .unwrap_err();
        assert!(e.starts_with("functions[1]"), "{e}");
    }

    #[test]
    fn bad_arguments_and_unknown_tools_are_reported() {
        assert!(Call::parse("nope", &json!({})).unwrap().is_none());
        assert!(Call::parse("status", &json!({})).is_err());
        assert!(Call::parse("status", &json!({"function_id": "x"})).is_err());
        assert!(Call::parse("create_function", &json!({"description": "no name"})).is_err());
        assert!(
            Call::parse("create_function", &json!({"name": "a", "colour": 1})).is_err(),
            "unknown fields are refused rather than ignored"
        );
        assert!(Call::parse("review_test_cases", &json!({"function_id": id()})).is_err());
        assert!(
            Call::parse(
                "export_project",
                &json!({"function_ids": [id()], "directory": "rel"})
            )
            .is_err(),
            "relative directories are refused"
        );
        assert!(Call::parse("export_project", &json!({"function_ids": [], "directory": "/p"})).is_err());
        assert!(Call::parse("submit_code", &json!({"function_id": id(), "code": "  "})).is_err());
        let (call, _) = Call::parse(
            "review_test_cases",
            &json!({"function_id": id(), "add": [{"input": "[]", "expected": "0"}]}),
        )
        .unwrap()
        .unwrap();
        let Call::ReviewTestCases { add, .. } = call else {
            panic!()
        };
        assert!(add[0].accept, "added cases are accepted by default");
    }

    #[test]
    fn every_tool_in_the_catalogue_parses() {
        let tools = catalogue();
        for t in tools.as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            let args = match name {
                "list_functions" | "cancel" => json!({}),
                "export_project" => json!({"function_ids": [id()], "directory": "/tmp/p"}),
                "submit_code" => json!({"function_id": id(), "code": "(ns a)"}),
                "create_function" => json!({"name": "n"}),
                "create_functions" => json!({"functions": [{"name": "n"}]}),
                "review_test_cases" => {
                    json!({"function_id": id(), "actions": [{"id": id(), "action": "accept"}]})
                }
                _ => json!({"function_id": id()}),
            };
            assert!(Call::parse(name, &args).unwrap().is_some(), "{name}");
        }
    }
}
