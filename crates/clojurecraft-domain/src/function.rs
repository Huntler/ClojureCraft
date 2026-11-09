//! The editable content of a function: specification, implementation, tests, and benchmark
//! configurations. A [`FunctionDocument`] is what a draft holds and what a revision freezes.

use serde::{Deserialize, Serialize};

use crate::DomainError;
use crate::fingerprint::Fingerprint;
use crate::ids::{
    BenchmarkConfigId, BenchmarkInputId, ClarificationId, ExampleId, FunctionId, RevisionId, StepId,
    TestCaseId,
};
use crate::time::Timestamp;

/// EDN source text exactly as the user (or a tool) wrote it. It is stored unparsed so that invalid
/// EDN never loses what was typed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Edn(pub String);

impl Edn {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionDocument {
    pub id: FunctionId,
    /// Display name; also the basis of the exported function name.
    pub name: String,
    pub description: String,
    pub contract: Option<Contract>,
    pub steps: Vec<Step>,
    pub examples: Vec<Example>,
    /// Questions about the specification and the user's answers. Answered questions are part of
    /// the specification; open and dismissed ones are not.
    #[serde(default)]
    pub clarifications: Vec<Clarification>,
    /// The model's restatement of the specification, kept next to (never instead of) the user's
    /// own text.
    #[serde(default)]
    pub interpretation: Option<Interpretation>,
    pub implementation: Option<Implementation>,
    pub tests: Vec<TestCase>,
    pub benchmarks: Vec<BenchmarkConfig>,
    /// The implementation accepted for edge-case testing, kept so that later changes are
    /// compared with it instead of replacing it silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<crate::review::AcceptedCode>,
}

impl FunctionDocument {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: FunctionId::generate(),
            name: name.into(),
            description: String::new(),
            contract: None,
            steps: Vec::new(),
            examples: Vec::new(),
            clarifications: Vec::new(),
            interpretation: None,
            implementation: None,
            tests: Vec::new(),
            benchmarks: Vec::new(),
            accepted: None,
        }
    }

    pub fn example(&self, id: ExampleId) -> Option<&Example> {
        self.examples.iter().find(|e| e.id == id)
    }

    pub fn test(&self, id: TestCaseId) -> Option<&TestCase> {
        self.tests.iter().find(|t| t.id == id)
    }

    pub fn benchmark(&self, id: BenchmarkConfigId) -> Option<&BenchmarkConfig> {
        self.benchmarks.iter().find(|b| b.id == id)
    }

    /// Inserts a new step after `after` (or at the end) and returns its identifier.
    pub fn add_step(&mut self, after: Option<StepId>, text: impl Into<String>) -> StepId {
        let step = Step::new(text);
        let id = step.id;
        let index = after
            .and_then(|a| self.steps.iter().position(|s| s.id == a))
            .map_or(self.steps.len(), |i| i + 1);
        self.steps.insert(index, step);
        id
    }

    /// Moves a step to `index` (clamped to the valid range). The step keeps its identifier, so
    /// anything that refers to it still does after reordering. Returns whether anything moved.
    pub fn move_step(&mut self, id: StepId, index: usize) -> bool {
        let Some(from) = self.steps.iter().position(|s| s.id == id) else {
            return false;
        };
        let to = index.min(self.steps.len() - 1);
        if from == to {
            return false;
        }
        let step = self.steps.remove(from);
        self.steps.insert(to, step);
        true
    }

    /// Removes a step, returning it with its former position so the removal can be undone with
    /// [`Self::insert_step`].
    pub fn remove_step(&mut self, id: StepId) -> Option<(usize, Step)> {
        let index = self.steps.iter().position(|s| s.id == id)?;
        Some((index, self.steps.remove(index)))
    }

    /// Reinserts a previously removed step at `index` (clamped). A step whose identifier is
    /// already present is not inserted twice.
    pub fn insert_step(&mut self, index: usize, step: Step) -> bool {
        if self.steps.iter().any(|s| s.id == step.id) {
            return false;
        }
        self.steps.insert(index.min(self.steps.len()), step);
        true
    }

    /// Appends an empty direct-input example and returns its identifier.
    pub fn add_example(&mut self, name: impl Into<String>) -> ExampleId {
        let example = Example::direct(name, Edn::default(), Edn::default());
        let id = example.id;
        self.examples.push(example);
        id
    }

    pub fn example_mut(&mut self, id: ExampleId) -> Option<&mut Example> {
        self.examples.iter_mut().find(|e| e.id == id)
    }

    /// Removes an example, returning it with its former position for undo.
    pub fn remove_example(&mut self, id: ExampleId) -> Option<(usize, Example)> {
        let index = self.examples.iter().position(|e| e.id == id)?;
        Some((index, self.examples.remove(index)))
    }

    /// Reinserts a previously removed example at `index` (clamped), unless its identifier is
    /// already present.
    pub fn insert_example(&mut self, index: usize, example: Example) -> bool {
        if self.examples.iter().any(|e| e.id == example.id) {
            return false;
        }
        self.examples.insert(index.min(self.examples.len()), example);
        true
    }

    pub fn clarification_mut(&mut self, id: ClarificationId) -> Option<&mut Clarification> {
        self.clarifications.iter_mut().find(|c| c.id == id)
    }

    /// Answered questions, which count as requirements.
    pub fn answered_clarifications(&self) -> impl Iterator<Item = &Clarification> {
        self.clarifications.iter().filter(|c| c.is_answered())
    }

    /// Replaces the open questions with `questions`. Answered and dismissed questions are kept,
    /// so a new interpretation never discards the user's answers.
    pub fn replace_open_questions(&mut self, questions: Vec<Clarification>) {
        self.clarifications
            .retain(|c| c.status != ClarificationStatus::Open);
        self.clarifications.extend(questions);
    }

    /// Every reference from an example input to another function's output.
    pub fn output_references(&self) -> impl Iterator<Item = (ExampleId, &OutputReference)> {
        self.examples.iter().filter_map(|e| match &e.input {
            ExampleInput::FunctionOutput { source, .. } => Some((e.id, source)),
            ExampleInput::Direct { .. } => None,
        })
    }
}

/// Explicit input/output requirements. Only an accepted contract is a user requirement; a
/// proposal is shown for review and has no effect on validation until accepted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    pub status: ContractStatus,
    pub input: Shape,
    pub output: Shape,
    pub ordering: OrderingRequirement,
    /// Namespaces the implementation may require beyond `clojure.core`.
    pub allowed_dependencies: Vec<String>,
    pub notes: String,
}

impl Contract {
    /// A contract with no requirements yet.
    pub fn empty(status: ContractStatus) -> Self {
        Self {
            status,
            input: Shape::default(),
            output: Shape::default(),
            ordering: OrderingRequirement::Unspecified,
            allowed_dependencies: Vec::new(),
            notes: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractStatus {
    Accepted,
    Proposed,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    pub description: String,
    /// The type of the whole value.
    #[serde(default, skip_serializing_if = "ValueType::is_any")]
    pub value_type: ValueType,
    /// Keys of the value when it is a map, or of each element when it is a collection of maps.
    pub keys: Vec<KeySpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeySpec {
    /// The key as EDN, for example `:user/id` or `"name"`.
    pub key: Edn,
    pub required: bool,
    pub value_type: ValueType,
    pub nil: NilBehavior,
    pub notes: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "name")]
pub enum ValueType {
    #[default]
    Any,
    Nil,
    Boolean,
    Integer,
    Number,
    String,
    Keyword,
    Symbol,
    Map,
    Vector,
    List,
    Set,
    Sequential,
    /// A type described in the user's own words.
    Other(String),
}

impl ValueType {
    /// Every type with a fixed meaning, in the order offered to users.
    pub const KNOWN: [Self; 14] = [
        Self::Any,
        Self::Nil,
        Self::Boolean,
        Self::Integer,
        Self::Number,
        Self::String,
        Self::Keyword,
        Self::Symbol,
        Self::Map,
        Self::Vector,
        Self::List,
        Self::Set,
        Self::Sequential,
        Self::Other(String::new()),
    ];

    pub fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Any => "any value",
            Self::Nil => "nil",
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::String => "string",
            Self::Keyword => "keyword",
            Self::Symbol => "symbol",
            Self::Map => "map",
            Self::Vector => "vector",
            Self::List => "list",
            Self::Set => "set",
            Self::Sequential => "sequential collection",
            Self::Other(_) => "other",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum NilBehavior {
    Unspecified,
    /// `nil` is not a valid value here.
    Disallowed,
    /// `nil` is treated the same as an absent key.
    TreatAsMissing,
    /// `nil` passes through unchanged.
    PassThrough,
    /// `nil` (or absence) is replaced by this value.
    Default(Edn),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum OrderingRequirement {
    Unspecified,
    /// Output order follows input order.
    PreserveInput,
    /// Output order is irrelevant; compare as unordered.
    Unordered,
    /// Output is sorted as described.
    Sorted(String),
}

/// A question about the specification, asked so that an ambiguity or contradiction is decided by
/// the user instead of becoming a hidden assumption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clarification {
    pub id: ClarificationId,
    pub question: String,
    /// The contradiction or gap the question resolves.
    pub context: String,
    /// Suggested answers; the user may also answer in their own words.
    pub options: Vec<String>,
    /// Parts of the specification concerned, for example "step 2" or "example 1".
    pub refers_to: Vec<String>,
    pub answer: String,
    pub status: ClarificationStatus,
}

impl Clarification {
    pub fn open(question: impl Into<String>, context: impl Into<String>) -> Self {
        Self {
            id: ClarificationId::generate(),
            question: question.into(),
            context: context.into(),
            options: Vec::new(),
            refers_to: Vec::new(),
            answer: String::new(),
            status: ClarificationStatus::Open,
        }
    }

    /// Records the user's answer; an empty answer leaves the question open.
    pub fn answer(&mut self, answer: impl Into<String>) {
        self.answer = answer.into();
        self.status = if self.answer.trim().is_empty() {
            ClarificationStatus::Open
        } else {
            ClarificationStatus::Answered
        };
    }

    pub fn is_answered(&self) -> bool {
        self.status == ClarificationStatus::Answered && !self.answer.trim().is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationStatus {
    Open,
    Answered,
    /// The user decided the question does not matter.
    Dismissed,
}

/// The model's reading of the specification. It never replaces the user's text and has no effect
/// on generation; only the user's answers to the questions it raises do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interpretation {
    pub restatement: String,
    pub assumptions: Vec<String>,
    pub issues: Vec<Issue>,
    /// The specification fingerprint the interpretation was made from.
    pub spec_basis: Fingerprint,
    pub model: String,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub kind: IssueKind,
    pub detail: String,
    pub refers_to: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// Two parts of the specification disagree.
    Contradiction,
    /// Something that affects the output is not specified.
    Gap,
}

/// One natural-language transformation step. The identifier survives reordering and rewording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub text: String,
}

impl Step {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: StepId::generate(),
            text: text.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Example {
    pub id: ExampleId,
    pub name: String,
    pub input: ExampleInput,
    pub expected: Edn,
}

impl Example {
    pub fn direct(name: impl Into<String>, input: Edn, expected: Edn) -> Self {
        Self {
            id: ExampleId::generate(),
            name: name.into(),
            input: ExampleInput::Direct { value: input },
            expected,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExampleInput {
    /// Input written directly as EDN.
    Direct { value: Edn },
    /// Input taken from the output of another saved function's example.
    ///
    /// The value used for validation is the recorded `snapshot`, so upstream changes never alter
    /// this example until the user explicitly refreshes it.
    FunctionOutput {
        source: OutputReference,
        snapshot: Option<ReferencedValue>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputReference {
    pub function: FunctionId,
    pub revision: RevisionPin,
    pub example: ExampleId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "revision")]
pub enum RevisionPin {
    /// Follow the latest revision when the user refreshes.
    Latest,
    /// Always use this revision.
    Pinned(RevisionId),
}

/// An executed upstream output, recorded with where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencedValue {
    pub value: Edn,
    pub revision: RevisionId,
    pub resolved_at: Timestamp,
}

/// The current implementation and, if it came from the model, the generation that produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub source: String,
    pub generation: Option<GenerationRecord>,
}

impl Implementation {
    pub fn manual(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            generation: None,
        }
    }

    /// Whether the code differs from what the model generated. Hand-written code (no generation)
    /// counts as manually edited.
    pub fn is_manually_edited(&self) -> bool {
        self.generation
            .as_ref()
            .is_none_or(|g| g.generated_source != self.source)
    }
}

/// Provenance of model-generated code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationRecord {
    /// The code exactly as generated, kept so manual edits can be reviewed against it.
    pub generated_source: String,
    pub model: ModelIdentity,
    pub runtime: InferenceRuntime,
    pub settings: GenerationSettings,
    pub documentation: Vec<DocumentationRef>,
    /// The specification fingerprint the code was generated from.
    pub spec_basis: Fingerprint,
    /// 1 for the initial generation, plus one per repair attempt.
    pub attempts: u32,
    pub created_at: Timestamp,
    /// The namespace and public function the code defines.
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub function_name: String,
    /// Namespaces required by the code, besides `clojure.core`.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Decisions the model made where the specification was silent; only code generated before
    /// assumptions moved to the explanation has them.
    #[serde(default)]
    pub assumptions: Vec<String>,
    /// Optional prompt content left out to fit the context (for example documentation).
    #[serde(default)]
    pub omitted_context: Vec<String>,
    #[serde(default)]
    pub explanation: Option<Explanation>,
    /// The opening generation request as it was sent (system and user messages); the input the
    /// code answers. Empty for code generated before prompts were recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt: Vec<PromptMessage>,
}

/// One chat message of a model request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: String,
}

/// A plain-language explanation of generated code.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    pub summary: String,
    /// Which code implements each transformation step.
    pub steps: Vec<StepMapping>,
    /// Decisions the code makes where the specification is silent.
    #[serde(default)]
    pub assumptions: Vec<String>,
    /// Standard-library functions used and what each does here.
    pub symbols: Vec<SymbolUse>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepMapping {
    pub step: StepId,
    /// The step's text when the code was explained.
    pub step_text: String,
    /// The code the model attributes to the step, as quoted by the model.
    pub excerpt: String,
    /// Lines (1-based, inclusive) where the excerpt was found in the source; `None` when it could
    /// not be located, in which case the mapping is shown as approximate.
    pub lines: Option<LineRange>,
    pub explanation: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolUse {
    pub symbol: String,
    pub purpose: String,
}

/// Which model produced a result. Locations are repository coordinates, never download URLs, so
/// no access tokens or signed query strings can be recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub display_name: String,
    pub repository: String,
    pub revision: String,
    pub file: String,
    pub sha256: String,
    pub quantization: String,
    pub architecture: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceRuntime {
    /// llama.cpp release tag, for example `b12138`.
    pub llama_cpp_release: String,
    /// `metal`, `cuda`, `vulkan`, `rocm`, or `cpu`.
    pub backend: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationSettings {
    pub temperature: f32,
    pub top_p: f32,
    pub seed: Option<u64>,
    pub context_length: u32,
    pub max_output_tokens: u32,
}

/// A retrieved documentation excerpt that informed a generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentationRef {
    pub source_url: String,
    pub title: String,
    pub symbol: Option<String>,
    pub namespace: Option<String>,
    pub clojure_version: String,
    /// Version of the installed documentation collection.
    pub collection_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCase {
    pub id: TestCaseId,
    pub name: String,
    pub input: Edn,
    pub expectation: Expectation,
    pub status: TestStatus,
    pub origin: TestOrigin,
    pub notes: String,
}

impl TestCase {
    /// Accepts the case as a regression test. A case whose expected result is still an open
    /// question cannot be accepted, so ambiguities never become hidden requirements.
    pub fn accept(&mut self) -> Result<(), DomainError> {
        match self.expectation {
            Expectation::Value { .. } => {
                self.status = TestStatus::Accepted;
                Ok(())
            }
            Expectation::NeedsClarification { .. } => Err(DomainError::AmbiguousTest(self.id)),
        }
    }

    /// Whether this case takes part in validation.
    pub fn is_active(&self) -> bool {
        self.status == TestStatus::Accepted && matches!(self.expectation, Expectation::Value { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Expectation {
    Value {
        expected: Edn,
    },
    /// The specification does not determine the result; the user must decide.
    NeedsClarification {
        question: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Proposed,
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestOrigin {
    User,
    Model,
    /// Proposed by the model, then changed by the user.
    ModelEdited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    pub id: BenchmarkConfigId,
    pub name: String,
    pub inputs: Vec<BenchmarkInput>,
    pub warmup_runs: u32,
    pub measured_runs: u32,
    /// Upper bound on the whole benchmark's wall-clock time.
    pub max_duration_ms: u64,
    pub max_heap_mib: u32,
    pub realization: Realization,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkInput {
    pub id: BenchmarkInputId,
    pub label: String,
    pub value: Edn,
    /// The user's description of the input size, for example "10k orders".
    pub size_label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Realization {
    /// Fully realize lazy results inside the measurement.
    Full,
    /// Measure the call only, as returned.
    AsReturned,
}
