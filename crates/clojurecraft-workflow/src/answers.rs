//! The structured answers each request asks for: the schema sent to llama.cpp (which constrains
//! sampling) and conversion of the validated JSON into domain values.
//!
//! The inference service has already validated each answer against its schema. Prose fields carry
//! a length limit in the schema, which stops small models that start repeating themselves inside a
//! string; the limits stay small because llama.cpp expands each bound into grammar rules. Code has
//! no bound in the schema and is limited here instead.

use clojurecraft_domain::{
    Clarification, Contract, ContractStatus, Edn, Explanation, Issue, IssueKind, KeySpec, NilBehavior,
    OrderingRequirement, Shape, Step, StepMapping, SymbolUse, ValueType,
};
use clojurecraft_inference::{Field, Schema};
use serde::Deserialize;
use serde_json::Value;

use crate::locate::locate;

/// Generated source larger than this is refused rather than shown.
pub const MAX_CODE_CHARS: usize = 20_000;
const MAX_PROSE: usize = 2_000;
const MAX_ITEM: usize = 400;

/// Trimmed and shortened to at most `max` characters.
fn text(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

fn items(v: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in v {
        let s = text(s, max);
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Bounds for prose in answers, in characters.
const SHORT: usize = 120;
const ITEM: usize = 300;
const PARAGRAPH: usize = 800;

fn prose(max: usize) -> Schema {
    Schema::bounded_string(max)
}

fn strings(max_items: usize) -> Schema {
    Schema::array(prose(ITEM), Some(max_items))
}

fn labels(max_items: usize) -> Schema {
    Schema::array(prose(SHORT), Some(max_items))
}

fn decode<T: for<'de> Deserialize<'de>>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("the answer could not be read: {e}"))
}

// ---- interpretation ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretationAnswer {
    pub restatement: String,
    pub assumptions: Vec<String>,
    pub issues: Vec<Issue>,
}

pub fn interpretation_schema() -> Schema {
    Schema::object(vec![
        Field::required("restatement", prose(PARAGRAPH)),
        Field::required("assumptions", strings(8)),
        Field::required(
            "issues",
            Schema::array(
                Schema::object(vec![
                    Field::required("kind", Schema::one_of(&["contradiction", "gap"])),
                    Field::required("detail", prose(ITEM)),
                    Field::required("refers_to", labels(6)),
                ]),
                Some(8),
            ),
        ),
    ])
}

#[derive(Deserialize)]
struct RawIssue {
    kind: String,
    detail: String,
    refers_to: Vec<String>,
}

#[derive(Deserialize)]
struct RawInterpretation {
    restatement: String,
    assumptions: Vec<String>,
    issues: Vec<RawIssue>,
}

pub fn parse_interpretation(v: &Value) -> Result<InterpretationAnswer, String> {
    let raw: RawInterpretation = decode(v)?;
    let restatement = text(&raw.restatement, MAX_PROSE);
    if restatement.is_empty() {
        return Err("the interpretation is empty".into());
    }
    Ok(InterpretationAnswer {
        restatement,
        assumptions: items(&raw.assumptions, MAX_ITEM),
        issues: raw
            .issues
            .iter()
            .filter(|i| !i.detail.trim().is_empty())
            .map(|i| Issue {
                kind: if i.kind == "contradiction" {
                    IssueKind::Contradiction
                } else {
                    IssueKind::Gap
                },
                detail: text(&i.detail, MAX_ITEM),
                refers_to: items(&i.refers_to, 40),
            })
            .collect(),
    })
}

// ---- clarification questions ------------------------------------------------------------------

pub fn questions_schema() -> Schema {
    Schema::object(vec![Field::required(
        "questions",
        Schema::array(
            Schema::object(vec![
                Field::required("question", prose(ITEM)),
                Field::required("why", prose(ITEM)),
                Field::required("options", labels(5)),
                Field::required("refers_to", labels(6)),
            ]),
            Some(6),
        ),
    )])
}

#[derive(Deserialize)]
struct RawQuestion {
    question: String,
    why: String,
    options: Vec<String>,
    refers_to: Vec<String>,
}

#[derive(Deserialize)]
struct RawQuestions {
    questions: Vec<RawQuestion>,
}

/// New open questions, leaving out any that repeat an existing question.
pub fn parse_questions(v: &Value, existing: &[Clarification]) -> Result<Vec<Clarification>, String> {
    let raw: RawQuestions = decode(v)?;
    let norm = |s: &str| s.trim().to_lowercase();
    let mut out: Vec<Clarification> = Vec::new();
    for q in raw.questions {
        let question = text(&q.question, MAX_ITEM);
        if question.is_empty()
            || existing
                .iter()
                .chain(&out)
                .any(|c| norm(&c.question) == norm(&question))
        {
            continue;
        }
        let mut c = Clarification::open(question, text(&q.why, MAX_ITEM));
        c.options = items(&q.options, 200);
        c.refers_to = items(&q.refers_to, 40);
        out.push(c);
    }
    Ok(out)
}

// ---- retrieval planning -----------------------------------------------------------------------

pub fn planning_schema() -> Schema {
    Schema::object(vec![
        Field::required("symbols", labels(12)),
        Field::required("concepts", labels(6)),
    ])
}

#[derive(Deserialize)]
struct RawPlan {
    symbols: Vec<String>,
    concepts: Vec<String>,
}

/// Documentation queries: symbols (each looked up exactly) followed by concept phrases.
pub fn parse_planning(v: &Value) -> Result<Vec<String>, String> {
    let raw: RawPlan = decode(v)?;
    let symbols = raw
        .symbols
        .iter()
        .map(|s| s.trim().trim_matches('`').trim_start_matches('(').to_owned())
        .filter(|s| !s.is_empty() && s.len() <= 60 && !s.contains(char::is_whitespace));
    let concepts = raw
        .concepts
        .iter()
        .map(|s| text(s, 120))
        .filter(|s| !s.is_empty());
    let mut out: Vec<String> = Vec::new();
    for q in symbols.chain(concepts) {
        if !out.contains(&q) {
            out.push(q);
        }
    }
    Ok(out)
}

// ---- contract proposals -----------------------------------------------------------------------

const TYPES: [&str; 13] = [
    "any",
    "nil",
    "boolean",
    "integer",
    "number",
    "string",
    "keyword",
    "symbol",
    "map",
    "vector",
    "list",
    "set",
    "sequential",
];
const NIL: [&str; 5] = [
    "unspecified",
    "disallowed",
    "treat_as_missing",
    "pass_through",
    "default",
];
const ORDERING: [&str; 4] = ["unspecified", "preserve_input", "unordered", "sorted"];

fn value_type(s: &str) -> ValueType {
    match s {
        "nil" => ValueType::Nil,
        "boolean" => ValueType::Boolean,
        "integer" => ValueType::Integer,
        "number" => ValueType::Number,
        "string" => ValueType::String,
        "keyword" => ValueType::Keyword,
        "symbol" => ValueType::Symbol,
        "map" => ValueType::Map,
        "vector" => ValueType::Vector,
        "list" => ValueType::List,
        "set" => ValueType::Set,
        "sequential" => ValueType::Sequential,
        _ => ValueType::Any,
    }
}

fn shape_schema() -> Schema {
    Schema::object(vec![
        Field::required("description", prose(ITEM)),
        Field::required("value_type", Schema::one_of(&TYPES)),
        Field::required(
            "keys",
            Schema::array(
                Schema::object(vec![
                    Field::required("key", prose(SHORT)).describe("the key as EDN, e.g. :customer-id"),
                    Field::required("required", Schema::Boolean),
                    Field::required("value_type", Schema::one_of(&TYPES)),
                    Field::required("nil", Schema::one_of(&NIL)),
                    Field::required("default", prose(SHORT))
                        .describe("EDN value used when nil is \"default\", otherwise empty"),
                    Field::required("notes", prose(ITEM)),
                ]),
                Some(24),
            ),
        ),
    ])
}

pub fn contract_schema() -> Schema {
    Schema::object(vec![
        Field::required("input", shape_schema()),
        Field::required("output", shape_schema()),
        Field::required("ordering", Schema::one_of(&ORDERING)),
        Field::required("sorted_by", prose(ITEM)),
        Field::required("notes", prose(PARAGRAPH)),
    ])
}

#[derive(Deserialize)]
struct RawKey {
    key: String,
    required: bool,
    value_type: String,
    nil: String,
    default: String,
    notes: String,
}

#[derive(Deserialize)]
struct RawShape {
    description: String,
    value_type: String,
    keys: Vec<RawKey>,
}

#[derive(Deserialize)]
struct RawContract {
    input: RawShape,
    output: RawShape,
    ordering: String,
    sorted_by: String,
    notes: String,
}

fn single_edn_value(s: &str) -> bool {
    let p = clojurecraft_edn::parse_value(s);
    !p.has_errors() && p.values.len() == 1
}

fn shape(raw: &RawShape, side: &str, dropped: &mut Vec<String>) -> Shape {
    let mut keys: Vec<KeySpec> = Vec::new();
    for k in &raw.keys {
        let key = k.key.trim();
        if !single_edn_value(key) {
            dropped.push(format!(
                "{side} key {key:?} is not a valid EDN value and was left out"
            ));
            continue;
        }
        if keys.iter().any(|existing| existing.key.as_str() == key) {
            continue;
        }
        let nil = match k.nil.as_str() {
            "disallowed" => NilBehavior::Disallowed,
            "treat_as_missing" => NilBehavior::TreatAsMissing,
            "pass_through" => NilBehavior::PassThrough,
            "default" if single_edn_value(k.default.trim()) => {
                NilBehavior::Default(Edn::new(k.default.trim()))
            }
            "default" => {
                dropped.push(format!(
                    "the default for {side} key {key} is not valid EDN and was left out"
                ));
                NilBehavior::Unspecified
            }
            _ => NilBehavior::Unspecified,
        };
        keys.push(KeySpec {
            key: Edn::new(key),
            required: k.required,
            value_type: value_type(&k.value_type),
            nil,
            notes: text(&k.notes, MAX_ITEM),
        });
    }
    Shape {
        description: text(&raw.description, MAX_ITEM),
        value_type: value_type(&raw.value_type),
        keys,
    }
}

/// A proposed contract. `allowed_dependencies` is carried over unchanged: a proposal never
/// widens what the implementation may use. Returns notes about parts that were left out.
pub fn parse_contract(
    v: &Value,
    allowed_dependencies: Vec<String>,
) -> Result<(Contract, Vec<String>), String> {
    let raw: RawContract = decode(v)?;
    let mut dropped = Vec::new();
    let ordering = match raw.ordering.as_str() {
        "preserve_input" => OrderingRequirement::PreserveInput,
        "unordered" => OrderingRequirement::Unordered,
        "sorted" => OrderingRequirement::Sorted(text(&raw.sorted_by, MAX_ITEM)),
        _ => OrderingRequirement::Unspecified,
    };
    let contract = Contract {
        status: ContractStatus::Proposed,
        input: shape(&raw.input, "input", &mut dropped),
        output: shape(&raw.output, "output", &mut dropped),
        ordering,
        allowed_dependencies,
        notes: text(&raw.notes, MAX_PROSE),
    };
    Ok((contract, dropped))
}

// ---- generation -------------------------------------------------------------------------------

/// Step references a generation answer may give at most, and the length of each quote.
pub const MAX_STEP_REFERENCES: usize = 20;
pub const MAX_STEP_QUOTE: usize = 400;

pub fn generation_schema() -> Schema {
    Schema::object(vec![
        Field::required("code", Schema::string())
            .describe("the complete Clojure source: the ns declaration followed by the definitions"),
        Field::required(
            "steps",
            Schema::array(
                Schema::object(vec![
                    Field::required("step", Schema::integer(Some(1), Some(99))),
                    Field::required("code", prose(MAX_STEP_QUOTE))
                        .describe("the code from \"code\" that carries out the step, copied exactly"),
                ]),
                Some(MAX_STEP_REFERENCES),
            ),
        ),
        Field::required("assumptions", strings(8)),
    ])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedCode {
    pub code: String,
    pub assumptions: Vec<String>,
    /// For each transformation step (numbered from 1), the code the model says carries it out.
    pub steps: Vec<(u32, String)>,
    /// The code arrived escaped twice (line breaks as `\n` text) and was unescaped.
    pub unescaped: bool,
}

/// Code escaped twice by the model: no real line breaks, but `\n` sequences between forms. Such
/// text is unescaped once when it is a valid JSON string body; anything else is left alone.
fn undo_double_escaping(code: &str) -> Option<String> {
    if code.contains('\n') || !(code.contains("\\n(") || code.contains("\\n ")) {
        return None;
    }
    serde_json::from_str::<String>(&format!("\"{code}\""))
        .ok()
        .filter(|s| s.contains('\n'))
}

#[derive(Deserialize)]
struct RawGeneration {
    code: String,
    assumptions: Vec<String>,
    #[serde(default)]
    steps: Vec<RawStepReference>,
}

#[derive(Deserialize)]
struct RawStepReference {
    step: u32,
    code: String,
}

/// Removes a Markdown code fence the model may have put around the code.
fn strip_fence(code: &str) -> &str {
    let t = code.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

pub fn parse_generation(v: &Value) -> Result<GeneratedCode, String> {
    let raw: RawGeneration = decode(v)?;
    let code = strip_fence(&raw.code);
    if code.is_empty() {
        return Err("the answer contains no code".into());
    }
    if code.chars().count() > MAX_CODE_CHARS {
        return Err(format!(
            "the code is longer than {MAX_CODE_CHARS} characters, which is far beyond a small function"
        ));
    }
    let unescaped = undo_double_escaping(code);
    let mut code = unescaped.clone().unwrap_or_else(|| code.to_owned());
    code.push('\n');
    Ok(GeneratedCode {
        code,
        assumptions: items(&raw.assumptions, MAX_ITEM),
        steps: raw
            .steps
            .iter()
            .filter(|s| !s.code.trim().is_empty())
            .map(|s| (s.step, strip_fence(&s.code).to_owned()))
            .collect(),
        unescaped: unescaped.is_some(),
    })
}

// ---- explanation ------------------------------------------------------------------------------

pub fn explanation_schema(steps: usize) -> Schema {
    let step_number = Schema::integer(
        Some(1),
        (steps > 0).then(|| i64::try_from(steps).unwrap_or(i64::MAX)),
    );
    Schema::object(vec![
        Field::required("summary", prose(PARAGRAPH)),
        Field::required(
            "steps",
            Schema::array(
                Schema::object(vec![
                    Field::required("step", step_number),
                    Field::required("code", prose(PARAGRAPH))
                        .describe("the exact code from the source that implements the step"),
                    Field::required("explanation", prose(ITEM)),
                ]),
                Some(steps),
            ),
        ),
        Field::required(
            "symbols",
            Schema::array(
                Schema::object(vec![
                    Field::required("symbol", prose(SHORT)),
                    Field::required("purpose", prose(ITEM)),
                ]),
                Some(8),
            ),
        ),
    ])
}

#[derive(Deserialize)]
struct RawStep {
    step: i64,
    code: String,
    explanation: String,
}

#[derive(Deserialize)]
struct RawSymbol {
    symbol: String,
    purpose: String,
}

#[derive(Deserialize)]
struct RawExplanation {
    summary: String,
    steps: Vec<RawStep>,
    symbols: Vec<RawSymbol>,
}

/// The explanation, with each step's quoted code located in `source`. `steps` are the steps that
/// were numbered in the request, in order.
pub fn parse_explanation(v: &Value, steps: &[&Step], source: &str) -> Result<Explanation, String> {
    let raw: RawExplanation = decode(v)?;
    let mut mapped: Vec<StepMapping> = Vec::new();
    for s in &raw.steps {
        let Some(step) = usize::try_from(s.step - 1).ok().and_then(|i| steps.get(i)) else {
            continue;
        };
        if mapped.iter().any(|m| m.step == step.id) {
            continue;
        }
        let excerpt = s.code.trim().to_owned();
        mapped.push(StepMapping {
            step: step.id,
            step_text: step.text.trim().to_owned(),
            lines: locate(source, &excerpt),
            excerpt: text(&excerpt, MAX_PROSE),
            explanation: text(&s.explanation, MAX_ITEM * 2),
        });
    }
    // Present mappings in step order.
    mapped.sort_by_key(|m| steps.iter().position(|s| s.id == m.step));
    let mut symbols: Vec<SymbolUse> = Vec::new();
    for s in &raw.symbols {
        let symbol = text(&s.symbol, 60);
        if !symbol.is_empty() && !symbols.iter().any(|x| x.symbol == symbol) {
            symbols.push(SymbolUse {
                symbol,
                purpose: text(&s.purpose, MAX_ITEM),
            });
        }
    }
    Ok(Explanation {
        summary: text(&raw.summary, MAX_PROSE),
        steps: mapped,
        symbols,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schemas_validate_their_own_examples() {
        let cases = [
            (
                interpretation_schema(),
                json!({"restatement": "r", "assumptions": [], "issues": [{"kind": "gap", "detail": "d", "refers_to": ["step 1"]}]}),
            ),
            (
                questions_schema(),
                json!({"questions": [{"question": "q", "why": "w", "options": ["a"], "refers_to": []}]}),
            ),
            (
                planning_schema(),
                json!({"symbols": ["group-by"], "concepts": []}),
            ),
            (
                contract_schema(),
                json!({"input": {"description": "", "value_type": "vector", "keys": []},
                       "output": {"description": "", "value_type": "map", "keys": []},
                       "ordering": "unspecified", "sorted_by": "", "notes": ""}),
            ),
            (
                generation_schema(),
                json!({"code": "(ns a)", "steps": [{"step": 1, "code": "(ns a)"}], "assumptions": []}),
            ),
            (
                explanation_schema(2),
                json!({"summary": "s", "steps": [{"step": 2, "code": "c", "explanation": "e"}], "symbols": []}),
            ),
        ];
        for (schema, value) in cases {
            schema.validate(&value).unwrap();
        }
        assert!(explanation_schema(2)
            .validate(&json!({"summary": "s", "steps": [{"step": 3, "code": "c", "explanation": "e"}], "symbols": []}))
            .is_err());
    }

    #[test]
    fn interpretation_answers_are_trimmed_and_bounded() {
        let v = json!({"restatement": format!("  {}  ", "x".repeat(3000)), "assumptions": ["a", "a", " "],
                       "issues": [{"kind": "contradiction", "detail": "step 2 vs example 1", "refers_to": ["step 2"]},
                                  {"kind": "gap", "detail": " ", "refers_to": []}]});
        let a = parse_interpretation(&v).unwrap();
        assert_eq!(a.restatement.chars().count(), MAX_PROSE + 1);
        assert_eq!(a.assumptions, ["a"]);
        assert_eq!(a.issues.len(), 1);
        assert_eq!(a.issues[0].kind, IssueKind::Contradiction);
        assert!(parse_interpretation(&json!({"restatement": " ", "assumptions": [], "issues": []})).is_err());
    }

    #[test]
    fn repeated_questions_are_not_asked_again() {
        let mut answered = Clarification::open("Are refunds negative?", "");
        answered.answer("yes");
        let v = json!({"questions": [
            {"question": "are refunds negative? ", "why": "", "options": [], "refers_to": []},
            {"question": "What about ties?", "why": "sorting", "options": ["by id", "by id"], "refers_to": ["step 3"]},
            {"question": "What about ties?", "why": "", "options": [], "refers_to": []}]});
        let qs = parse_questions(&v, &[answered]).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "What about ties?");
        assert_eq!(qs[0].options, ["by id"]);
        assert!(!qs[0].is_answered());
    }

    #[test]
    fn contract_proposals_are_proposed_and_never_widen_dependencies() {
        let v = json!({
            "input": {"description": "orders", "value_type": "vector", "keys": [
                {"key": ":amount", "required": false, "value_type": "number", "nil": "default", "default": "0", "notes": ""},
                {"key": "{:bad", "required": true, "value_type": "any", "nil": "unspecified", "default": "", "notes": ""},
                {"key": ":id", "required": true, "value_type": "integer", "nil": "default", "default": "{", "notes": ""}]},
            "output": {"description": "", "value_type": "map", "keys": []},
            "ordering": "sorted", "sorted_by": "total", "notes": "n"});
        let (c, dropped) = parse_contract(&v, vec!["clojure.string".into()]).unwrap();
        assert_eq!(c.status, ContractStatus::Proposed);
        assert_eq!(c.allowed_dependencies, ["clojure.string"]);
        assert_eq!(c.input.value_type, ValueType::Vector);
        assert_eq!(c.input.keys.len(), 2);
        assert_eq!(c.input.keys[0].nil, NilBehavior::Default(Edn::new("0")));
        assert_eq!(c.input.keys[1].nil, NilBehavior::Unspecified);
        assert_eq!(c.ordering, OrderingRequirement::Sorted("total".into()));
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn generated_code_is_unfenced_and_bounded() {
        let v = json!({"code": "```clojure\n(ns a)\n(defn f [x] x)\n```", "assumptions": []});
        assert_eq!(parse_generation(&v).unwrap().code, "(ns a)\n(defn f [x] x)\n");
        assert!(parse_generation(&json!({"code": "  ", "assumptions": []})).is_err());
        let huge = json!({"code": "x".repeat(MAX_CODE_CHARS + 1), "assumptions": []});
        assert!(parse_generation(&huge).unwrap_err().contains("longer than"));
        let with_steps = json!({"code": "(ns a)", "assumptions": [],
                                "steps": [{"step": 1, "code": "(inc x)"}, {"step": 2, "code": " "}]});
        assert_eq!(
            parse_generation(&with_steps).unwrap().steps,
            [(1, "(inc x)".to_owned())]
        );
    }

    #[test]
    fn code_escaped_twice_is_unescaped_once() {
        let v = json!({"code": "(ns a)\\n(defn f [x] (str \\\"a\\\" x))", "assumptions": []});
        let g = parse_generation(&v).unwrap();
        assert!(g.unescaped);
        assert_eq!(g.code, "(ns a)\n(defn f [x] (str \"a\" x))\n");
        // Ordinary code with a newline character literal or escape in a string is untouched.
        for code in [
            "(ns a)\n(defn f [s] (str/split s #\"\\n\"))",
            "(ns a) (defn f [c] (= c \\n))",
        ] {
            let g = parse_generation(&json!({"code": code, "assumptions": []})).unwrap();
            assert!(!g.unescaped, "{code}");
            assert_eq!(g.code.trim_end(), code);
        }
    }

    #[test]
    fn explanations_map_steps_to_located_code() {
        let a = Step::new("group by customer");
        let b = Step::new("sum amounts");
        let source = "(ns a)\n(defn f [xs]\n  (->> xs\n       (group-by :c)\n       (map total)))\n";
        let v = json!({"summary": "Groups then sums.",
            "steps": [{"step": 2, "code": "(map total)", "explanation": "sums"},
                      {"step": 1, "code": "(group-by :c)", "explanation": "groups"},
                      {"step": 1, "code": "dup", "explanation": "ignored"},
                      {"step": 9, "code": "x", "explanation": "unknown step"}],
            "symbols": [{"symbol": "group-by", "purpose": "groups"}, {"symbol": "group-by", "purpose": "again"}]});
        let e = parse_explanation(&v, &[&a, &b], source).unwrap();
        assert_eq!(e.steps.len(), 2);
        assert_eq!(e.steps[0].step, a.id);
        assert_eq!(e.steps[0].lines.map(|l| (l.start, l.end)), Some((4, 4)));
        assert_eq!(e.steps[1].lines.map(|l| l.start), Some(5));
        assert_eq!(e.symbols.len(), 1);
    }
}
