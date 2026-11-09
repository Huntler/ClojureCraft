//! A fine-tuning dataset made from saved functions: for each function whose code passes its
//! checks, the opening generation request (the model input) and the validated code (the output),
//! one JSON object per line. The code is either generated and unedited, or written by hand (for
//! example imported from a project and described from its code).
//!
//! The recorded prompt is used while it still matches the current specification. Otherwise (the
//! specification changed after generation, the code predates recorded prompts, or was written by
//! hand) the prompt is rebuilt from the current specification, without retrieved documentation.

use std::collections::BTreeSet;

use clojurecraft_domain::{FunctionDocument, FunctionId, PromptMessage, ValidationReport};
use serde::Serialize;

use crate::{Names, deps, export, requests};

/// Where an entry's prompt came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSource {
    /// Exactly as sent when the code was generated.
    Recorded,
    /// Rebuilt from the current specification, without retrieved documentation.
    Rebuilt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub function_id: FunctionId,
    pub name: String,
    pub messages: Vec<PromptMessage>,
    pub output: String,
    pub prompt_source: PromptSource,
    pub model: String,
    pub attempts: u32,
    pub checks_passed: usize,
}

/// The `model` of an entry whose code was written by hand.
pub const HAND_WRITTEN: &str = "hand-written";

/// The dataset entry of `doc`, or why it has none. `reports` are its validation reports, newest
/// first.
pub fn entry(doc: &FunctionDocument, reports: &[ValidationReport]) -> Result<Entry, String> {
    let implementation = doc.implementation.as_ref().ok_or("no implementation")?;
    let generation = implementation.generation.as_ref();
    match generation {
        Some(_) if implementation.is_manually_edited() => {
            return Err("the generated code was edited by hand".into());
        }
        None if doc.description.trim().is_empty() => {
            return Err("the code was written by hand and has no description".into());
        }
        _ => {}
    }
    let basis = doc.basis();
    let report = reports
        .iter()
        .find(|r| r.basis == basis)
        .ok_or("not checked since the last change")?;
    if !report.passed() {
        return Err("its checks do not pass".into());
    }
    let names = export::names(doc);
    let rebuilt = rebuilt_prompt(doc, &names, &report.environment.clojure_version);
    let (messages, prompt_source) = match generation {
        Some(g) if matches(&g.prompt, &rebuilt) => (g.prompt.clone(), PromptSource::Recorded),
        _ => (rebuilt, PromptSource::Rebuilt),
    };
    Ok(Entry {
        function_id: doc.id,
        name: doc.name.clone(),
        messages,
        output: implementation.source.clone(),
        prompt_source,
        model: generation.map_or_else(|| HAND_WRITTEN.to_owned(), |g| g.model.display_name.clone()),
        attempts: generation.map_or(0, |g| g.attempts),
        checks_passed: report.checks.len(),
    })
}

fn rebuilt_prompt(doc: &FunctionDocument, names: &Names, clojure_version: &str) -> Vec<PromptMessage> {
    let allowed: BTreeSet<String> = deps::allowed(doc);
    let request = requests::generation_request(requests::GenerationInput {
        doc,
        names,
        allowed: &allowed,
        clojure_version,
        documentation: None,
        seed: 0,
    });
    requests::prompt_messages(&request, None)
}

/// Whether `recorded` is `rebuilt` plus, at most, documentation appended to the last message.
fn matches(recorded: &[PromptMessage], rebuilt: &[PromptMessage]) -> bool {
    let (Some((last, init)), Some((rebuilt_last, rebuilt_init))) =
        (recorded.split_last(), rebuilt.split_last())
    else {
        return false;
    };
    init == rebuilt_init
        && last.role == rebuilt_last.role
        && last
            .content
            .strip_prefix(&rebuilt_last.content)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("\n\n"))
}

/// The entries as JSON Lines.
pub fn jsonl<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> String {
    entries
        .into_iter()
        .map(|e| serde_json::to_string(e).expect("entries serialize") + "\n")
        .collect()
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::{
        Check, CheckStatus, CheckSubject, Edn, Example, ExecutionEnvironment, GenerationRecord,
        GenerationSettings, Implementation, InferenceRuntime, ModelIdentity, Timestamp, ValidationId,
    };

    use super::*;

    const CODE: &str = "(ns double-it.core)\n\n(defn double-it [x]\n  (* 2 x))\n";

    fn doc(prompt: impl FnOnce(&FunctionDocument) -> Vec<PromptMessage>) -> FunctionDocument {
        let mut doc = FunctionDocument::new("double it");
        doc.description = "Double the number".into();
        doc.examples
            .push(Example::direct("two", Edn::new("2"), Edn::new("4")));
        doc.implementation = Some(Implementation::manual(CODE));
        let prompt = prompt(&doc);
        doc.implementation.as_mut().unwrap().generation = Some(GenerationRecord {
            generated_source: CODE.into(),
            model: ModelIdentity {
                display_name: "Qwen".into(),
                repository: String::new(),
                revision: String::new(),
                file: String::new(),
                sha256: String::new(),
                quantization: String::new(),
                architecture: String::new(),
            },
            runtime: InferenceRuntime {
                llama_cpp_release: String::new(),
                backend: String::new(),
            },
            settings: GenerationSettings {
                temperature: 0.2,
                top_p: 0.9,
                seed: None,
                context_length: 8192,
                max_output_tokens: 1800,
            },
            documentation: Vec::new(),
            spec_basis: doc.spec_fingerprint(),
            attempts: 2,
            created_at: Timestamp::now(),
            namespace: "double-it.core".into(),
            function_name: "double-it".into(),
            requires: Vec::new(),
            assumptions: Vec::new(),
            omitted_context: Vec::new(),
            explanation: None,
            prompt,
        });
        doc
    }

    fn with_docs(doc: &FunctionDocument) -> Vec<PromptMessage> {
        let mut prompt = rebuilt(doc);
        prompt
            .last_mut()
            .unwrap()
            .content
            .push_str("\n\nReference material: (doc *)");
        prompt
    }

    fn rebuilt(doc: &FunctionDocument) -> Vec<PromptMessage> {
        rebuilt_prompt(doc, &export::names(doc), "1.12.6")
    }

    fn report(doc: &FunctionDocument, status: CheckStatus) -> ValidationReport {
        ValidationReport {
            id: ValidationId::generate(),
            created_at: Timestamp::now(),
            basis: doc.basis(),
            environment: ExecutionEnvironment {
                clojure_version: "1.12.6".into(),
                java_version: "21".into(),
                platform: "macos-aarch64".into(),
            },
            checks: vec![Check {
                subject: CheckSubject::Load,
                status,
                expected: None,
                actual: None,
                diagnostics: Vec::new(),
                duration_ms: None,
            }],
            attempt: None,
            run: None,
            source: None,
            settings: None,
        }
    }

    #[test]
    fn the_recorded_prompt_with_its_documentation_is_the_input() {
        let doc = doc(with_docs);
        let e = entry(&doc, &[report(&doc, CheckStatus::Passed)]).unwrap();
        assert_eq!(e.prompt_source, PromptSource::Recorded);
        assert!(e.messages.last().unwrap().content.ends_with("(doc *)"));
        assert_eq!(e.output, CODE);
        assert_eq!(e.messages[0].role, "system");
    }

    #[test]
    fn a_specification_changed_after_generation_rebuilds_the_prompt() {
        let mut doc = doc(with_docs);
        doc.description = "Double the given number".into();
        let e = entry(&doc, &[report(&doc, CheckStatus::Passed)]).unwrap();
        assert_eq!(e.prompt_source, PromptSource::Rebuilt);
        assert_eq!(e.messages, rebuilt(&doc));
        assert!(e.messages[1].content.contains("Double the given number"));
    }

    #[test]
    fn code_without_a_recorded_prompt_gets_a_rebuilt_one() {
        let doc = doc(|_| Vec::new());
        let e = entry(&doc, &[report(&doc, CheckStatus::Passed)]).unwrap();
        assert_eq!(e.prompt_source, PromptSource::Rebuilt);
    }

    #[test]
    fn hand_written_code_with_a_description_qualifies_with_a_rebuilt_prompt() {
        let mut manual = doc(with_docs);
        manual.implementation.as_mut().unwrap().generation = None;
        let e = entry(&manual, &[report(&manual, CheckStatus::Passed)]).unwrap();
        assert_eq!(e.prompt_source, PromptSource::Rebuilt);
        assert_eq!(e.messages, rebuilt(&manual));
        assert_eq!(e.model, HAND_WRITTEN);
        assert_eq!(e.output, CODE);

        manual.description = "  ".into();
        assert!(entry(&manual, &[report(&manual, CheckStatus::Passed)]).is_err());
    }

    #[test]
    fn only_unedited_generated_or_hand_written_code_with_current_passing_checks_qualifies() {
        let good = doc(with_docs);
        let passed = report(&good, CheckStatus::Passed);
        assert!(entry(&good, &[report(&good, CheckStatus::Failed)]).is_err());
        assert!(entry(&good, &[]).is_err());

        let mut edited = good.clone();
        edited.implementation.as_mut().unwrap().source = CODE.replace('2', "3");
        assert!(entry(&edited, &[report(&edited, CheckStatus::Passed)]).is_err());

        let mut stale = good.clone();
        stale.examples[0].expected = Edn::new("5");
        assert!(entry(&stale, std::slice::from_ref(&passed)).is_err());

        let newest_first = [report(&good, CheckStatus::Failed), passed];
        assert!(entry(&good, &newest_first).is_err());
    }

    #[test]
    fn each_entry_is_one_json_line() {
        let doc = doc(with_docs);
        let e = entry(&doc, &[report(&doc, CheckStatus::Passed)]).unwrap();
        let text = jsonl([&e, &e]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["output"], CODE);
        assert_eq!(v["prompt_source"], "recorded");
        assert_eq!(v["messages"][1]["role"], "user");
    }
}
