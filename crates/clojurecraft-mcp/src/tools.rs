//! The tool catalogue offered to agents and the typed calls the application carries out.
//!
//! Every tool that changes a function, starts work, or reads results takes a `function_id`: the
//! id returned by `create_function` or `list_functions`, or a name the application resolves
//! (see [`Call::Resolve`]). Model work (test-case proposals and generation) runs in the
//! background; tools that start it take `wait_seconds` to answer with the finished `status`, so a
//! basic task needs one call instead of a start-and-poll loop.

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
    /// Project mode, on creation only: the namespace to add the function to.
    pub namespace: Option<String>,
    /// Project mode, on creation only: how many arguments the function takes.
    pub arguments: Option<u32>,
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
    /// With an optional case-insensitive filter on name, namespace, and description.
    ListFunctions(Option<String>),
    /// Not a tool: function names (`name`, `function`, or `namespace/function`) to ids, in order.
    Resolve(Vec<String>),
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

/// A parsed `tools/call`: the call, how long to wait for the work it starts, and whether new
/// functions are generated right away.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    pub call: Call,
    pub wait: u64,
    pub generate: bool,
}

/// Tools that take `wait_seconds`.
const WAITING: [&str; 9] = [
    "create_function",
    "update_function",
    "review_test_cases",
    "generate",
    "generate_test_cases",
    "describe",
    "run_tests",
    "submit_code",
    "status",
];

/// Tools that take `generate`: change the specification or tests, then generate at once.
const GENERATING: [&str; 3] = ["create_function", "update_function", "review_test_cases"];

impl Call {
    /// Parses a `tools/call`. `Ok(None)` means the tool does not exist.
    pub fn parse(name: &str, args: &Value) -> Result<Option<Parsed>, String> {
        let mut args = match args {
            Value::Null => json!({}),
            Value::Object(_) => args.clone(),
            _ => return Err("arguments must be an object".into()),
        };
        let mut wait = 0;
        if WAITING.contains(&name)
            && let Some(w) = args.as_object_mut().and_then(|o| o.remove("wait_seconds"))
        {
            wait = w
                .as_u64()
                .ok_or("`wait_seconds` must be a non-negative integer")?
                .min(MAX_WAIT_SECONDS);
        }
        let mut generate = false;
        if GENERATING.contains(&name)
            && let Some(g) = args.as_object_mut().and_then(|o| o.remove("generate"))
        {
            generate = g.as_bool().ok_or("`generate` must be a boolean")?;
        }
        let args = &args;
        let id = || function_id(args);
        let call = match name {
            "list_functions" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct A {
                    query: Option<String>,
                }
                let a: A = parse(args)?;
                Self::ListFunctions(a.query.map(|q| q.trim().to_owned()).filter(|q| !q.is_empty()))
            }
            "create_function" if args.get("functions").is_none() => {
                let spec: SpecArgs = parse(args)?;
                if spec.name.as_deref().is_none_or(|n| n.trim().is_empty()) {
                    return Err("`name` is required".into());
                }
                Self::CreateFunction(spec)
            }
            // `create_functions` is the former name, kept for agents that remember it.
            "create_function" | "create_functions" => {
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
                let spec: SpecArgs = parse(&without(args, &["function_id"]))?;
                if spec.namespace.is_some() || spec.arguments.is_some() {
                    return Err("`namespace` and `arguments` are set on creation only".into());
                }
                Self::UpdateFunction(function, spec)
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
                    verbose: bool,
                }
                let a: A = parse(args)?;
                match one_or_many(args)? {
                    One(f) => Self::Status(f, detail(a.verbose)),
                    Many(fs) => Self::StatusMany(fs),
                }
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
        Ok(Some(Parsed { call, wait, generate }))
    }

    /// The `status` that reports on the work this call starts, if it starts any.
    pub fn follow_up(&self) -> Option<Self> {
        match self {
            Self::GenerateTestCases(id, _)
            | Self::Describe(id)
            | Self::Generate(id, _)
            | Self::RunTests(id)
            | Self::SubmitCode(id, _) => Some(Self::Status(*id, Detail::Compact)),
            Self::Status(..) | Self::StatusMany(_) => Some(self.clone()),
            Self::GenerateMany(ids, _) => Some(Self::StatusMany(ids.clone())),
            _ => None,
        }
    }
}

/// Whether a tool of this name exists.
pub fn exists(name: &str) -> bool {
    name == "create_functions"
        || catalogue()
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["name"] == name))
}

/// Whether `text` is a function id rather than a name.
pub fn is_id(text: &str) -> bool {
    text.trim().parse::<FunctionId>().is_ok()
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
//
// Sent to every agent session, so it is kept short: one sentence per tool where possible, the
// specification schema once, and conventions (EDN strings, waiting) stated in the instructions.

const TYPES: &str = "any nil boolean integer number string keyword symbol map vector list set \
                     sequential, or own words";

fn function_id_schema() -> Value {
    json!({"type": "string", "description": "Id, name, or namespace/function."})
}

fn ids_schema() -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": "Several, instead of function_id."})
}

fn wait_schema() -> Value {
    json!({"type": "integer", "description": "Max 300; answer with status once the work ends."})
}

fn generate_schema() -> Value {
    json!({"type": "boolean", "description": "Then generate the code."})
}

fn spec_properties() -> serde_json::Map<String, Value> {
    let shape = json!({
        "type": "object",
        "properties": {
            "description": {"type": "string"},
            "type": {"type": "string", "description": TYPES},
            "keys": {
                "type": "array",
                "description": "Map keys (or keys of each map in a collection).",
                "items": {
                    "type": "object",
                    "required": ["key"],
                    "properties": {
                        "key": {"type": "string", "description": "e.g. \":id\""},
                        "required": {"type": "boolean"},
                        "type": {"type": "string"},
                        "nil": {"enum": ["unspecified", "disallowed", "treat_as_missing", "pass_through", "default"]},
                        "default": {"type": "string", "description": "EDN, with nil=default"},
                        "notes": {"type": "string"}
                    }
                }
            }
        }
    });
    let props = json!({
        "name": {"type": "string", "description": "Title; the function name derives from it (in a project: the symbol)."},
        "description": {"type": "string", "description": "Input shape, output shape, rules."},
        "steps": {"type": "array", "items": {"type": "string"}, "description": "In order; replaces all."},
        "examples": {
            "type": "array",
            "description": "Replaces all. With arguments != 1, each input is a vector of the arguments.",
            "items": {
                "type": "object",
                "required": ["input", "expected"],
                "properties": {
                    "name": {"type": "string"},
                    "input": {"type": "string"},
                    "expected": {"type": "string", "description": "EDN, or #throws ExceptionClass"}
                }
            }
        },
        "contract": {
            "type": ["object", "null"],
            "description": "Checked on outputs; null removes it.",
            "properties": {
                "input": shape.clone(),
                "output": shape,
                "ordering": {"type": "string", "description": "unspecified, preserve_input, unordered, or \"sorted: <how>\""},
                "allowed_namespaces": {"type": "array", "items": {"type": "string"}, "description": "Default clojure.string, clojure.set."},
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
        "inputSchema": {"type": "object", "properties": properties, "required": required}
    })
}

/// The `tools/list` result.
pub fn catalogue() -> Value {
    let mut create = spec_properties();
    create.insert(
        "namespace".into(),
        json!({"type": "string", "description": "Project only (required there): namespace to add to; created if missing."}),
    );
    create.insert(
        "arguments".into(),
        json!({"type": "integer", "description": "Project only: argument count, default 1."}),
    );
    create.insert(
        "functions".into(),
        json!({"type": "array", "items": {"type": "object"}, "description": "Several specifications with the fields above, instead of one (all or nothing)."}),
    );
    create.insert("generate".into(), generate_schema());
    create.insert("wait_seconds".into(), wait_schema());
    // The same fields as create_function, without repeating their schemas.
    let update = json!({
        "function_id": function_id_schema(),
        "name": {"type": "string"},
        "description": {"type": "string"},
        "steps": {"type": "array", "items": {"type": "string"}},
        "examples": {"type": "array", "items": {"type": "object"}},
        "contract": {"type": ["object", "null"]},
        "generate": generate_schema(),
        "wait_seconds": wait_schema()
    });
    let only_id = json!({"function_id": function_id_schema()});
    let id_wait = json!({"function_id": function_id_schema(), "wait_seconds": wait_schema()});
    json!([
        tool(
            "list_functions",
            "Functions (id, name, description; namespace and project root while a project is open).",
            json!({"query": {"type": "string", "description": "Filter on name, namespace, description."}}),
            &[],
        ),
        tool(
            "create_function",
            "Create functions from specifications; returns ids and readiness. With generate and \
             wait_seconds, also generates and returns the finished status, with the code when it passes.",
            Value::Object(create),
            &[],
        ),
        tool(
            "get_function",
            "Specification and test cases (with ids).",
            only_id.clone(),
            &["function_id"],
        ),
        tool(
            "update_function",
            "Change the given fields of a specification (fields as in create_function); with \
             generate and wait_seconds, regenerate and return the finished status.",
            update,
            &["function_id"],
        ),
        tool(
            "generate_test_cases",
            "Model proposes edge-case tests to review. source=specification uses everything, \
             examples (default) only title, description, examples.",
            json!({
                "function_id": function_id_schema(),
                "source": {"enum": ["examples", "specification"]},
                "wait_seconds": wait_schema()
            }),
            &["function_id"],
        ),
        tool(
            "review_test_cases",
            "Act on test cases by id and add your own (accepted by default). Accepted cases must \
             pass. All or nothing.",
            json!({
                "function_id": function_id_schema(),
                "actions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["id", "action"],
                        "properties": {
                            "id": {"type": "string"},
                            "action": {"enum": ["accept", "reject", "reopen", "remove", "edit"]},
                            "name": {"type": "string"},
                            "input": {"type": "string"},
                            "expected": {"type": "string"}
                        }
                    }
                },
                "add": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["input", "expected"],
                        "properties": {
                            "name": {"type": "string"},
                            "input": {"type": "string"},
                            "expected": {"type": "string"},
                            "accept": {"type": "boolean"}
                        }
                    }
                },
                "generate": generate_schema(),
                "wait_seconds": wait_schema()
            }),
            &["function_id"],
        ),
        tool(
            "run_tests",
            "Check the current code against examples, accepted tests, and contract.",
            id_wait.clone(),
            &["function_id"],
        ),
        tool(
            "describe",
            "For existing code: fill empty description and steps, add up to 5 examples recorded \
             by running the code, propose a contract.",
            id_wait.clone(),
            &["function_id"],
        ),
        tool(
            "generate",
            "Write and validate the code, fixing failures in up to 5 attempts. Several functions \
             queue. mode=fix repairs the current failing code.",
            json!({
                "function_id": function_id_schema(),
                "function_ids": ids_schema(),
                "mode": {"enum": ["fresh", "fix"]},
                "wait_seconds": wait_schema()
            }),
            &[],
        ),
        tool(
            "status",
            "Progress, last job, failing checks, tests to review, next step; the code once all \
             checks pass. With function_ids, one line each. verbose=true: full failure values.",
            json!({
                "function_id": function_id_schema(),
                "function_ids": ids_schema(),
                "wait_seconds": wait_schema(),
                "verbose": {"type": "boolean"}
            }),
            &[],
        ),
        tool(
            "result",
            "Current code with namespace and function, whether it passes, and code waiting for acceptance.",
            json!({"function_id": function_id_schema(), "verbose": {"type": "boolean"}}),
            &["function_id"],
        ),
        tool(
            "accept_code",
            "Use the generated code waiting for acceptance.",
            only_id,
            &["function_id"],
        ),
        tool(
            "submit_code",
            "Your own code becomes the function's code and is checked. One ns form, defn- helpers, \
             one public defn; keep namespace and function from result.",
            json!({
                "function_id": function_id_schema(),
                "code": {"type": "string"},
                "wait_seconds": wait_schema()
            }),
            &["function_id", "code"],
        ),
        tool(
            "export_project",
            "Write functions as a Clojure project: src/, clojure.test files from examples and \
             accepted tests, deps.edn. Returns files and the test command.",
            json!({
                "function_ids": {"type": "array", "items": {"type": "string"}},
                "directory": {"type": "string", "description": "Absolute; created if missing."},
                "overwrite": {"type": "boolean", "description": "Replace differing files."}
            }),
            &["function_ids", "directory"],
        ),
        tool(
            "cancel",
            "Stop the model job and empty the queue.",
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
        let p = Call::parse("status", &json!({"function_id": f, "wait_seconds": 9999}))
            .unwrap()
            .unwrap();
        assert!(matches!(p.call, Call::Status(_, Detail::Compact)));
        assert_eq!(p.wait, MAX_WAIT_SECONDS);

        let Parsed { call, .. } = Call::parse(
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

        let Parsed { call, .. } = Call::parse("update_function", &json!({"function_id": f}))
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
        let Parsed { call, .. } = Call::parse("generate", &json!({"function_ids": [a, b, a], "mode": "fix"}))
            .unwrap()
            .unwrap();
        let Call::GenerateMany(ids, GenerateMode::Fix) = call else {
            panic!("{call:?}")
        };
        assert_eq!(ids.len(), 2, "duplicates are dropped");
        let p = Call::parse("status", &json!({"function_ids": [a], "wait_seconds": 5}))
            .unwrap()
            .unwrap();
        assert!(matches!(p.call, Call::StatusMany(_)));
        assert_eq!(p.wait, 5);
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
        let Parsed { call, .. } = Call::parse(
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
                "review_test_cases" => {
                    json!({"function_id": id(), "actions": [{"id": id(), "action": "accept"}]})
                }
                _ => json!({"function_id": id()}),
            };
            assert!(Call::parse(name, &args).unwrap().is_some(), "{name}");
        }
    }

    #[test]
    fn work_can_be_started_and_awaited_in_one_call() {
        let f = id();
        let p = Call::parse("generate", &json!({"function_id": f, "wait_seconds": 60}))
            .unwrap()
            .unwrap();
        assert_eq!(p.wait, 60);
        assert!(matches!(p.call.follow_up(), Some(Call::Status(..))));
        let p = Call::parse(
            "create_function",
            &json!({"name": "a", "generate": true, "wait_seconds": 30}),
        )
        .unwrap()
        .unwrap();
        assert!(p.generate);
        assert_eq!(p.wait, 30);
    }

    #[test]
    fn one_tool_creates_one_or_several_functions() {
        let p = Call::parse(
            "create_function",
            &json!({"functions": [{"name": "a"}, {"name": "b"}]}),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(p.call, Call::CreateFunctions(ref s) if s.len() == 2));
        let p = Call::parse("create_functions", &json!({"functions": [{"name": "a"}]}))
            .unwrap()
            .unwrap();
        assert!(
            matches!(p.call, Call::CreateFunctions(_)),
            "the former name still works"
        );
        let p = Call::parse(
            "create_function",
            &json!({"name": "parse-set", "namespace": "app.sets", "arguments": 2}),
        )
        .unwrap()
        .unwrap();
        let Call::CreateFunction(spec) = p.call else {
            panic!()
        };
        assert_eq!(spec.namespace.as_deref(), Some("app.sets"));
        assert_eq!(spec.arguments, Some(2));
        assert!(
            Call::parse("update_function", &json!({"function_id": id(), "namespace": "x"})).is_err(),
            "the namespace is fixed after creation"
        );
    }

    #[test]
    fn functions_can_be_filtered() {
        let p = Call::parse("list_functions", &json!({"query": "  sum "}))
            .unwrap()
            .unwrap();
        assert_eq!(p.call, Call::ListFunctions(Some("sum".into())));
        let p = Call::parse("list_functions", &json!({})).unwrap().unwrap();
        assert_eq!(p.call, Call::ListFunctions(None));
    }

    #[test]
    fn the_catalogue_stays_small() {
        let size = catalogue().to_string().len();
        assert!(
            size < 9500,
            "the tool list costs every agent session tokens: {size} bytes"
        );
    }
}
