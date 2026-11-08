//! A Clojure project made from saved functions: each function's code under `src/`, a
//! `clojure.test` namespace per function under `test/` built from its examples and accepted test
//! cases, a `deps.edn`, and a small comparison namespace so the exported tests judge results the
//! way ClojureCraft's checks did (float tolerance, number kinds, unordered output).
//!
//! The project is only rendered here; writing it is left to the caller. Output contracts are not
//! exported: they are checked by the evaluation harness, not by plain tests.

use std::collections::BTreeMap;

use clojurecraft_domain::{ExampleInput, Expectation, FunctionDocument, FunctionId};

use crate::edge_cases::{self, CaseState};
use crate::structure::{self, Expectations};
use crate::{Names, deps, validation};

/// The comparison namespace shipped with every exported project.
pub const CHECK_SOURCE: &str = include_str!("../../../resources/clojure/export-check.clj");
pub const CHECK_NAMESPACE: &str = "clojurecraft.check";

/// Version of `io.github.cognitect-labs/test-runner` referenced by the `:test` alias.
const TEST_RUNNER: &str = r#"{:git/tag "v0.5.1" :git/sha "dfb30dd"}"#;

/// One file of the project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    /// Relative to the project root, with `/` separators.
    pub path: String,
    pub content: String,
}

/// What was exported for one function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exported {
    pub id: FunctionId,
    pub namespace: String,
    pub function: String,
    pub test_namespace: String,
    /// Examples and accepted test cases turned into assertions.
    pub assertions: usize,
    /// Examples left out, with the reason.
    pub left_out: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub files: Vec<File>,
    pub functions: Vec<Exported>,
}

impl Project {
    pub fn test_namespaces(&self) -> Vec<&str> {
        self.functions.iter().map(|f| f.test_namespace.as_str()).collect()
    }
}

/// The file holding `namespace`, following Clojure's convention (`a-b.core` → `a_b/core.clj`).
pub fn namespace_path(namespace: &str) -> String {
    format!("{}.clj", namespace.replace('-', "_").replace('.', "/"))
}

/// The namespace and public function `doc`'s code defines: read from the code itself, else as
/// recorded when it was generated, else derived from the name.
pub fn names(doc: &FunctionDocument) -> Names {
    let recorded = match doc.implementation.as_ref().and_then(|i| i.generation.as_ref()) {
        Some(g) if !g.namespace.is_empty() && !g.function_name.is_empty() => Names {
            namespace: g.namespace.clone(),
            function: g.function_name.clone(),
        },
        _ => Names::for_document(doc, |_| false),
    };
    let Some(code) = &doc.implementation else {
        return recorded;
    };
    let allowed = deps::allowed(doc);
    let found = structure::check(
        &code.source,
        &Expectations {
            namespace: &recorded.namespace,
            function: &recorded.function,
            allowed: &allowed,
        },
    );
    Names {
        namespace: found.namespace.unwrap_or(recorded.namespace),
        function: found.function.unwrap_or(recorded.function),
    }
}

/// Renders a project from `docs`, in their order. Fails when a function has no code or two
/// functions define the same namespace.
pub fn project(docs: &[&FunctionDocument], clojure_version: &str) -> Result<Project, String> {
    let mut files = Vec::new();
    let mut functions = Vec::new();
    let mut taken: BTreeMap<String, &str> = BTreeMap::new();
    for doc in docs {
        let code = doc
            .implementation
            .as_ref()
            .ok_or_else(|| format!("“{}” has no code yet.", doc.name))?;
        let n = names(doc);
        if n.namespace == CHECK_NAMESPACE {
            return Err(format!(
                "“{}” uses the reserved namespace {CHECK_NAMESPACE}.",
                doc.name
            ));
        }
        if let Some(other) = taken.insert(n.namespace.clone(), &doc.name) {
            return Err(format!(
                "“{}” and “{other}” both define the namespace {}; rename one of them.",
                doc.name, n.namespace
            ));
        }
        let mut source = code.source.trim_end().to_owned();
        source.push('\n');
        files.push(File {
            path: format!("src/{}", namespace_path(&n.namespace)),
            content: source,
        });
        let test_namespace = format!("{}-test", n.namespace);
        let (content, assertions, left_out) = test_file(doc, &n, &test_namespace);
        files.push(File {
            path: format!("test/{}", namespace_path(&test_namespace)),
            content,
        });
        functions.push(Exported {
            id: doc.id,
            namespace: n.namespace,
            function: n.function,
            test_namespace,
            assertions,
            left_out,
        });
    }
    files.push(File {
        path: format!("test/{}", namespace_path(CHECK_NAMESPACE)),
        content: CHECK_SOURCE.to_owned(),
    });
    files.push(File {
        path: "deps.edn".into(),
        content: deps_edn(clojure_version),
    });
    Ok(Project { files, functions })
}

fn deps_edn(clojure_version: &str) -> String {
    let version = if clojure_version.trim().is_empty() {
        "1.12.0"
    } else {
        clojure_version.trim()
    };
    format!(
        "{{:paths [\"src\"]\n \
         :deps {{org.clojure/clojure {{:mvn/version \"{version}\"}}}}\n \
         :aliases\n \
         {{:test {{:extra-paths [\"test\"]\n         \
         :extra-deps {{io.github.cognitect-labs/test-runner {TEST_RUNNER}}}\n         \
         :main-opts [\"-m\" \"cognitect.test-runner\"]\n         \
         :exec-fn cognitect.test-runner.api/test}}}}}}\n"
    )
}

/// A Clojure string literal.
fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An EDN value as a quoted literal. A value with a comment ends its line so that the comment
/// cannot swallow the code after it.
fn quoted(edn: &str) -> String {
    let edn = edn.trim();
    if edn.contains(';') {
        format!("'{edn}\n")
    } else {
        format!("'{edn}")
    }
}

struct Assertion {
    label: String,
    input: String,
    expected: String,
}

fn test_file(doc: &FunctionDocument, n: &Names, test_namespace: &str) -> (String, usize, Vec<String>) {
    let mut left_out = Vec::new();
    let mut examples = Vec::new();
    for (i, e) in doc.examples.iter().enumerate() {
        let label = if e.name.trim().is_empty() {
            format!("example {}", i + 1)
        } else {
            format!("example {}: {}", i + 1, e.name.trim())
        };
        let input = match &e.input {
            ExampleInput::Direct { value } => value.as_str().to_owned(),
            ExampleInput::FunctionOutput {
                snapshot: Some(s), ..
            } => s.value.as_str().to_owned(),
            ExampleInput::FunctionOutput { snapshot: None, .. } => {
                left_out.push(format!(
                    "{label} (its input comes from another function and was never run)"
                ));
                continue;
            }
        };
        examples.push(Assertion {
            label,
            input,
            expected: e.expected.as_str().to_owned(),
        });
    }
    let tests: Vec<Assertion> = doc
        .tests
        .iter()
        .filter(|t| edge_cases::state(t) == CaseState::Regression)
        .filter_map(|t| match &t.expectation {
            Expectation::Value { expected } => Some(Assertion {
                label: t.name.trim().to_owned(),
                input: t.input.as_str().to_owned(),
                expected: expected.as_str().to_owned(),
            }),
            Expectation::NeedsClarification { .. } => None,
        })
        .collect();

    let equality = validation::equality(doc);
    let defaults = clojurecraft_clojure::Equality::default();
    let mut opts = Vec::new();
    if equality.unordered {
        opts.push(":unordered true".to_owned());
    }
    #[allow(clippy::float_cmp)]
    if equality.float_tolerance != defaults.float_tolerance {
        opts.push(format!(":float-tolerance {:e}", equality.float_tolerance));
    }
    let opts = if opts.is_empty() {
        String::new()
    } else {
        format!(" {{{}}}", opts.join(" "))
    };

    let mut out = format!(
        ";; Generated by ClojureCraft from the examples and accepted test cases of “{}”.\n\
         (ns {test_namespace}\n  \
         (:require [clojure.test :refer [deftest is]]\n            \
         [{CHECK_NAMESPACE} :as check]\n            \
         [{} :as subject]))\n",
        doc.name.replace('\n', " "),
        n.namespace,
    );
    let mut section = |name: &str, items: &[Assertion]| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("\n(deftest {name}"));
        for a in items {
            out.push_str(&format!(
                "\n  (is (nil? (check/difference {}\n                              (subject/{} {}){opts}))\n      {})",
                quoted(&a.expected),
                n.function,
                quoted(&a.input),
                string(&a.label),
            ));
        }
        out.push_str(")\n");
    };
    section("examples", &examples);
    section("accepted-test-cases", &tests);
    (out, examples.len() + tests.len(), left_out)
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{Edn, Example, Implementation, TestCase, TestCaseId, TestOrigin, TestStatus};

    use super::*;

    fn case(name: &str, input: &str, expected: &str, status: TestStatus) -> TestCase {
        TestCase {
            id: TestCaseId::generate(),
            name: name.into(),
            input: Edn::new(input),
            expectation: Expectation::Value {
                expected: Edn::new(expected),
            },
            status,
            origin: TestOrigin::User,
            notes: String::new(),
        }
    }

    fn doc(name: &str, code: &str) -> FunctionDocument {
        let mut d = FunctionDocument::new(name);
        d.examples = vec![Example::direct("pair", Edn::new("[1 2]"), Edn::new("3"))];
        d.implementation = Some(Implementation::manual(code));
        d
    }

    #[test]
    fn functions_become_source_and_test_files() {
        let d = doc(
            "Sum numbers",
            "(ns sum-numbers.core)\n\n(defn sum-numbers [xs] (reduce + xs))",
        );
        let p = project(&[&d], "1.12.6").unwrap();
        let paths: Vec<&str> = p.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "src/sum_numbers/core.clj",
                "test/sum_numbers/core_test.clj",
                "test/clojurecraft/check.clj",
                "deps.edn"
            ]
        );
        let test = &p.files[1].content;
        assert!(test.contains("(ns sum-numbers.core-test"));
        assert!(test.contains("[sum-numbers.core :as subject]"));
        assert!(test.contains("(check/difference '3"));
        assert!(test.contains("(subject/sum-numbers '[1 2])"));
        assert!(test.contains("\"example 1: pair\""));
        assert!(!test.contains("accepted-test-cases"), "no empty deftest");
        assert!(p.files[3].content.contains("{:mvn/version \"1.12.6\"}"));
        assert_eq!(p.functions[0].assertions, 1);
        assert_eq!(p.test_namespaces(), ["sum-numbers.core-test"]);
    }

    #[test]
    fn names_come_from_the_code() {
        let d = doc("Whatever", "(ns my.lib-x)\n(defn total [xs] (reduce + xs))");
        let n = names(&d);
        assert_eq!(n.namespace, "my.lib-x");
        assert_eq!(n.function, "total");
        assert_eq!(namespace_path("my.lib-x"), "my/lib_x.clj");
    }

    #[test]
    fn accepted_cases_are_tests_and_proposed_ones_are_not() {
        let mut d = doc(
            "Sum numbers",
            "(ns sum-numbers.core)\n(defn sum-numbers [xs] (reduce + xs))",
        );
        d.tests
            .push(case("empty \"list\"", "[]", "0", TestStatus::Accepted));
        d.tests.push(case("not yet", "[5]", "5", TestStatus::Proposed));
        let p = project(&[&d], "").unwrap();
        let test = &p.files[1].content;
        assert!(test.contains("(deftest accepted-test-cases"));
        assert!(test.contains(r#""empty \"list\"""#));
        assert!(!test.contains("not yet"));
        assert_eq!(p.functions[0].assertions, 2);
    }

    #[test]
    fn comments_in_values_cannot_hide_code() {
        assert_eq!(quoted("[1 ; one\n 2]"), "'[1 ; one\n 2]\n");
        assert_eq!(quoted(" {:a 1} "), "'{:a 1}");
    }

    #[test]
    fn missing_code_and_clashing_namespaces_are_refused() {
        let mut empty = doc("Empty", "");
        empty.implementation = None;
        assert!(project(&[&empty], "").unwrap_err().contains("no code"));
        let a = doc("A", "(ns same.core)\n(defn a [x] x)");
        let b = doc("B", "(ns same.core)\n(defn b [x] x)");
        assert!(project(&[&a, &b], "").unwrap_err().contains("same.core"));
    }
}
