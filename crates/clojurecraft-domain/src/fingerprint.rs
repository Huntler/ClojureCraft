//! Content fingerprints and staleness.
//!
//! Specification, code, and accepted tests are fingerprinted independently. Each fingerprint is a
//! SHA-256 over a versioned, canonical projection of only the content that affects results, so
//! cosmetic changes (renaming the function or an example, reordering proposals) keep results
//! current, and any material change makes them stale.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::function::{
    BenchmarkConfig, Contract, ContractStatus, Edn, ExampleInput, FunctionDocument, OutputReference,
};
use crate::ids::{ExampleId, RevisionId, TestCaseId};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Hashes `value` as JSON under a domain-separation label. The label carries the projection's
    /// version, so a changed projection can never collide with an older one.
    pub fn of<T: Serialize + ?Sized>(label: &str, value: &T) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(label.as_bytes());
        hasher.update(b"\n");
        // Serialising plain structs, vectors, and strings to JSON cannot fail.
        let json = serde_json::to_vec(value).unwrap_or_default();
        hasher.update(&json);
        Self(format!("sha256:{}", hex::encode(hasher.finalize())))
    }

    pub fn from_stored(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The independently tracked parts of a function that results depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Specification,
    Code,
    Tests,
    BenchmarkConfiguration,
}

/// The fingerprints a result was computed from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Basis {
    pub spec: Fingerprint,
    pub code: Fingerprint,
    pub tests: Fingerprint,
}

impl Basis {
    pub fn freshness(&self, current: &Basis) -> Freshness {
        let mut changed = Vec::new();
        if self.spec != current.spec {
            changed.push(Component::Specification);
        }
        if self.code != current.code {
            changed.push(Component::Code);
        }
        if self.tests != current.tests {
            changed.push(Component::Tests);
        }
        Freshness::from_changes(changed)
    }

    /// One fingerprint for the combination, used to identify which results belong together.
    pub fn validation_fingerprint(&self) -> Fingerprint {
        Fingerprint::of("clojurecraft/validation-basis/v1", self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "changed")]
pub enum Freshness {
    Current,
    /// Kept for inspection, but must not be presented as a current result.
    Stale(Vec<Component>),
}

impl Freshness {
    pub(crate) fn from_changes(changed: Vec<Component>) -> Self {
        if changed.is_empty() {
            Self::Current
        } else {
            Self::Stale(changed)
        }
    }

    pub fn is_current(&self) -> bool {
        matches!(self, Self::Current)
    }
}

#[derive(Serialize)]
struct SpecProjection<'a> {
    description: &'a str,
    contract: Option<&'a Contract>,
    steps: Vec<&'a str>,
    examples: Vec<ExampleProjection<'a>>,
    /// Omitted when empty, so specifications without answers keep their earlier fingerprint.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    clarifications: Vec<(&'a str, &'a str)>,
    /// Omitted for one argument, so earlier fingerprints stay valid.
    #[serde(skip_serializing_if = "is_one")]
    arguments: u32,
}

fn is_one(n: &u32) -> bool {
    *n == 1
}

#[derive(Serialize)]
struct ExampleProjection<'a> {
    id: ExampleId,
    input: InputProjection<'a>,
    expected: &'a Edn,
}

#[derive(Serialize)]
enum InputProjection<'a> {
    Direct(&'a Edn),
    /// The recorded value and the upstream revision it was produced by, so refreshing a
    /// reference from a newer revision makes earlier results stale even when the value is equal.
    Referenced {
        source: &'a OutputReference,
        value: Option<&'a Edn>,
        revision: Option<RevisionId>,
    },
}

#[derive(Serialize)]
struct TestProjection<'a> {
    id: TestCaseId,
    input: &'a Edn,
    expected: &'a Edn,
}

impl FunctionDocument {
    /// Description, accepted contract, step texts in order, examples (input value as used for
    /// validation, plus expected output), and answered clarifications. Names, step identifiers,
    /// proposals, the model's interpretation, and open or dismissed questions are excluded.
    pub fn spec_fingerprint(&self) -> Fingerprint {
        let projection = SpecProjection {
            description: &self.description,
            contract: self
                .contract
                .as_ref()
                .filter(|c| c.status == ContractStatus::Accepted),
            steps: self.steps.iter().map(|s| s.text.as_str()).collect(),
            examples: self
                .examples
                .iter()
                .map(|e| ExampleProjection {
                    id: e.id,
                    input: match &e.input {
                        ExampleInput::Direct { value } => InputProjection::Direct(value),
                        ExampleInput::FunctionOutput { source, snapshot } => InputProjection::Referenced {
                            source,
                            value: snapshot.as_ref().map(|s| &s.value),
                            revision: snapshot.as_ref().map(|s| s.revision),
                        },
                    },
                    expected: &e.expected,
                })
                .collect(),
            clarifications: self
                .answered_clarifications()
                .map(|c| (c.question.as_str(), c.answer.as_str()))
                .collect(),
            arguments: self.arguments(),
        };
        Fingerprint::of("clojurecraft/spec/v1", &projection)
    }

    /// The implementation source text exactly; no implementation has its own fingerprint.
    pub fn code_fingerprint(&self) -> Fingerprint {
        code_fingerprint(self.implementation.as_ref().map(|i| i.source.as_str()))
    }

    /// The basis of a result for `source` checked against this document's specification and
    /// tests, for code that is not (or not yet) the implementation.
    pub fn basis_with_code(&self, source: &str) -> Basis {
        Basis {
            code: code_fingerprint(Some(source)),
            ..self.basis()
        }
    }

    /// Accepted tests with a defined expectation, in order. Proposed, rejected, and ambiguous
    /// cases do not affect results.
    pub fn tests_fingerprint(&self) -> Fingerprint {
        let tests: Vec<_> = self
            .tests
            .iter()
            .filter(|t| t.is_active())
            .filter_map(|t| match &t.expectation {
                crate::function::Expectation::Value { expected } => Some(TestProjection {
                    id: t.id,
                    input: &t.input,
                    expected,
                }),
                crate::function::Expectation::NeedsClarification { .. } => None,
            })
            .collect();
        Fingerprint::of("clojurecraft/tests/v1", &tests)
    }

    pub fn basis(&self) -> Basis {
        Basis {
            spec: self.spec_fingerprint(),
            code: self.code_fingerprint(),
            tests: self.tests_fingerprint(),
        }
    }

    /// The entire document, including names; distinguishes an unchanged draft from an edited one.
    pub fn content_fingerprint(&self) -> Fingerprint {
        Fingerprint::of("clojurecraft/document/v1", self)
    }
}

/// The fingerprint of implementation source text (`None` for no implementation).
pub fn code_fingerprint(source: Option<&str>) -> Fingerprint {
    Fingerprint::of("clojurecraft/code/v1", &source)
}

impl BenchmarkConfig {
    /// Everything that determines what a benchmark measures; the display name is excluded.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut c = self.clone();
        c.name.clear();
        for input in &mut c.inputs {
            input.label.clear();
        }
        Fingerprint::of("clojurecraft/benchmark-config/v1", &c)
    }
}
