//! A generated implementation between the model's answer and the function: its structural check,
//! the provenance recorded with it, and whether it may replace the current code.

use std::collections::BTreeSet;

use clojurecraft_domain::{
    DocumentationRef, Fingerprint, FunctionDocument, GenerationRecord, GenerationSettings, Implementation,
    InferenceRuntime, ModelIdentity, PromptMessage, Timestamp,
};

use crate::answers::GeneratedCode;
use crate::requests::Names;
use crate::structure::{self, CodeStructure, Expectations};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub code: String,
    pub structure: CodeStructure,
}

impl Candidate {
    pub fn check(generated: GeneratedCode, names: &Names, allowed: &BTreeSet<String>) -> Self {
        let structure = structure::check(
            &generated.code,
            &Expectations {
                namespace: &names.namespace,
                function: &names.function,
                allowed,
                arguments: names.arguments,
            },
        );
        Self {
            code: generated.code,
            structure,
        }
    }

    /// Checks again after the allowed namespaces changed (for example after an approval).
    pub fn recheck(&mut self, names: &Names, allowed: &BTreeSet<String>) {
        let generated = GeneratedCode {
            code: std::mem::take(&mut self.code),
            unescaped: false,
        };
        *self = Self::check(generated, names, allowed);
    }
}

/// Where a generation came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Provenance {
    pub model: ModelIdentity,
    pub runtime: InferenceRuntime,
    pub settings: GenerationSettings,
    pub documentation: Vec<DocumentationRef>,
    /// The specification fingerprint when the request was made.
    pub spec_basis: Fingerprint,
    pub omitted_context: Vec<String>,
    /// The initial generation plus repairs it took.
    pub attempts: u32,
    /// The opening generation request as sent.
    pub prompt: Vec<PromptMessage>,
}

/// The implementation for an acceptable candidate; `None` while it has problems or needs
/// approval.
pub fn implementation(candidate: &Candidate, provenance: Provenance) -> Option<Implementation> {
    if !candidate.structure.is_acceptable() {
        return None;
    }
    Some(Implementation {
        source: candidate.code.clone(),
        generation: Some(GenerationRecord {
            generated_source: candidate.code.clone(),
            model: provenance.model,
            runtime: provenance.runtime,
            settings: provenance.settings,
            documentation: provenance.documentation,
            spec_basis: provenance.spec_basis,
            attempts: provenance.attempts.max(1),
            created_at: Timestamp::now(),
            namespace: candidate.structure.namespace.clone().unwrap_or_default(),
            function_name: candidate.structure.function.clone().unwrap_or_default(),
            requires: candidate.structure.requires.clone(),
            assumptions: Vec::new(),
            omitted_context: provenance.omitted_context,
            explanation: None,
            prompt: provenance.prompt,
        }),
    })
}

/// Whether new generated code may replace the current implementation without review: only when
/// there is none, or it is exactly what the model generated. Code written or edited by hand is
/// never replaced silently.
pub fn replaces_silently(doc: &FunctionDocument) -> bool {
    doc.implementation
        .as_ref()
        .is_none_or(|i| i.source.trim().is_empty() || !i.is_manually_edited())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance {
            model: ModelIdentity {
                display_name: "Qwen".into(),
                repository: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF".into(),
                revision: "abc".into(),
                file: "q.gguf".into(),
                sha256: "00".into(),
                quantization: "Q4_K_M".into(),
                architecture: "qwen2".into(),
            },
            runtime: InferenceRuntime {
                llama_cpp_release: "b12138".into(),
                backend: "cuda".into(),
            },
            settings: GenerationSettings {
                temperature: 0.2,
                top_p: 0.9,
                seed: Some(1),
                context_length: 8192,
                max_output_tokens: 2500,
            },
            documentation: vec![],
            spec_basis: Fingerprint::from_stored("sha256:1"),
            omitted_context: vec!["Clojure documentation".into()],
            attempts: 3,
            prompt: Vec::new(),
        }
    }

    fn names() -> Names {
        Names {
            namespace: "f.core".into(),
            function: "f".into(),
            arguments: 1,
        }
    }

    #[test]
    fn only_acceptable_candidates_become_implementations() {
        let allowed = BTreeSet::new();
        let bad = Candidate::check(
            GeneratedCode {
                code: "(ns f.core)\n(defn f [x] (eval x))\n".into(),
                unescaped: false,
            },
            &names(),
            &allowed,
        );
        assert!(implementation(&bad, provenance()).is_none());

        let mut walk = Candidate::check(
            GeneratedCode {
                code: "(ns f.core (:require [clojure.walk :as w]))\n(defn f [x] (w/keywordize-keys x))\n"
                    .into(),
                unescaped: false,
            },
            &names(),
            &allowed,
        );
        assert_eq!(walk.structure.needs_approval, ["clojure.walk"]);
        assert!(implementation(&walk, provenance()).is_none());
        walk.recheck(&names(), &BTreeSet::from(["clojure.walk".to_owned()]));
        let i = implementation(&walk, provenance()).unwrap();
        let g = i.generation.as_ref().unwrap();
        assert!(!i.is_manually_edited());
        assert_eq!(g.requires, ["clojure.walk"]);
        assert_eq!((g.namespace.as_str(), g.function_name.as_str()), ("f.core", "f"));
        assert_eq!(g.model.repository, "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF");
        assert_eq!(g.runtime.llama_cpp_release, "b12138");
        assert_eq!(g.omitted_context, ["Clojure documentation"]);
        assert_eq!(g.attempts, 3);
    }

    #[test]
    fn hand_edited_code_is_never_replaced_silently() {
        let mut doc = FunctionDocument::new("f");
        assert!(replaces_silently(&doc));
        let c = Candidate::check(
            GeneratedCode {
                code: "(ns f.core)\n(defn f [x] x)\n".into(),
                unescaped: false,
            },
            &names(),
            &BTreeSet::new(),
        );
        doc.implementation = implementation(&c, provenance());
        assert!(replaces_silently(&doc));
        doc.implementation
            .as_mut()
            .unwrap()
            .source
            .push_str(";; edited\n");
        assert!(!replaces_silently(&doc));
        doc.implementation = Some(Implementation::manual("(ns f.core)"));
        assert!(!replaces_silently(&doc));
    }
}
