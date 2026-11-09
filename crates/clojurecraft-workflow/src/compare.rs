//! Comparing new code with the code it would replace, on inputs the specification does not
//! cover. Passing the examples and tests says nothing about other inputs, where a model often
//! adds guards or drops behavior; the user should see those changes before accepting the code.
//!
//! The probe inputs are variants of the examples (a collection emptied, a map entry removed, a
//! value or an argument nil). The current code is run on them first; the new code is then run
//! with the current outputs as expected values, so the harness compares the results as values.

use clojurecraft_clojure::{CallStatus, EvalOutcome, EvalRequest};
use clojurecraft_domain::FunctionDocument;
use clojurecraft_edn::{Node, NodeKind, parse_value};

use crate::{describe, spec};

/// Probe inputs at most.
pub const MAX_PROBES: usize = 12;
/// Differences reported at most.
pub const MAX_DIFFERENCES: usize = 5;
/// Variants made from one example at most.
const PER_EXAMPLE: usize = 6;
/// How deep into an input variants are made.
const MAX_DEPTH: usize = 4;

/// Variants of `doc`'s example inputs that are not inputs of an example already.
pub fn probes(doc: &FunctionDocument) -> Vec<String> {
    let inputs: Vec<String> = spec::usable_examples(doc)
        .into_iter()
        .map(|(_, _, i)| i.trim().to_owned())
        .collect();
    let mut out: Vec<String> = Vec::new();
    for input in &inputs {
        let mut added = 0;
        for v in variants(input, doc.arguments()) {
            if added == PER_EXAMPLE || out.len() == MAX_PROBES {
                break;
            }
            if !inputs.contains(&v) && !out.contains(&v) {
                out.push(v);
                added += 1;
            }
        }
    }
    out
}

/// Variants of one input, the shallowest changes first. With several arguments the input is
/// their vector, which keeps its length; each argument is changed instead.
fn variants(input: &str, arguments: u32) -> Vec<String> {
    let parsed = parse_value(input);
    let Some(root) = parsed.values.first().filter(|_| !parsed.has_errors()) else {
        return Vec::new();
    };
    let targets: Vec<&Node> = match &root.kind {
        NodeKind::Vector(args) if arguments > 1 => args.iter().collect(),
        _ => vec![root],
    };
    let mut edits: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    // A whole argument nil tells least about the code, so it comes last.
    let nils: Vec<(std::ops::Range<usize>, &str)> = targets
        .iter()
        .filter(|t| !matches!(t.kind, NodeKind::Nil))
        .map(|t| (t.span.clone(), "nil"))
        .collect();
    let mut level: Vec<&Node> = targets;
    for _ in 0..MAX_DEPTH {
        let mut next = Vec::new();
        for node in level {
            match &node.kind {
                NodeKind::Vector(items) | NodeKind::List(items) | NodeKind::Set(items) => {
                    if !items.is_empty() {
                        edits.push((node.span.clone(), empty(&node.kind)));
                    }
                    next.extend(items.iter());
                }
                NodeKind::Map(entries) => {
                    if !entries.is_empty() {
                        edits.push((node.span.clone(), "{}"));
                    }
                    for (k, v) in entries {
                        let end = v.span.end
                            + input[v.span.end..]
                                .find(|c: char| !matches!(c, ' ' | ',' | '\n' | '\t' | '\r'))
                                .unwrap_or(input.len() - v.span.end);
                        let end = if input[end..].starts_with(['}', ']', ')']) {
                            v.span.end
                        } else {
                            end
                        };
                        edits.push((k.span.start..end, ""));
                        if !matches!(v.kind, NodeKind::Nil) {
                            edits.push((v.span.clone(), "nil"));
                        }
                        next.push(v);
                    }
                }
                _ => {}
            }
        }
        level = next;
    }
    edits
        .into_iter()
        .chain(nils)
        .map(|(range, text)| {
            let mut v = input.to_owned();
            v.replace_range(range, text);
            v
        })
        .collect()
}

fn empty(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::List(_) => "()",
        NodeKind::Set(_) => "#{}",
        _ => "[]",
    }
}

/// A run of the current code on the probes, recording what it returns.
pub fn current_run(doc: &FunctionDocument, code: &str, probes: &[String]) -> Result<EvalRequest, String> {
    describe::run_request(doc, code, &named(probes))
}

/// A run of the new code on the probes, expecting what the current code returned.
pub fn candidate_run(
    doc: &FunctionDocument,
    code: &str,
    probes: &[String],
    current: &EvalOutcome,
) -> Result<EvalRequest, String> {
    let mut r = describe::run_request(doc, code, &named(probes))?;
    for (i, call) in r.calls.iter_mut().enumerate() {
        call.expected = current
            .report
            .result(i)
            .filter(|c| c.status == CallStatus::Ok && !c.truncated)
            .and_then(|c| c.actual.clone());
    }
    Ok(r)
}

fn named(probes: &[String]) -> Vec<(String, String)> {
    probes
        .iter()
        .enumerate()
        .map(|(i, p)| (format!("probe {}", i + 1), p.clone()))
        .collect()
}

/// Why a run gives no results to compare: the code could not be loaded or run at all.
pub fn unusable(outcome: &EvalOutcome) -> Option<String> {
    let r = &outcome.report;
    [&r.read, &r.check, &r.load]
        .into_iter()
        .find_map(|s| s.as_ref().and_then(|s| s.as_ref().err()))
        .map(|e| e.message.clone())
        .or_else(|| match &outcome.status {
            clojurecraft_clojure::EvalStatus::SandboxUnavailable(why) => Some(why.trim().to_owned()),
            _ => None,
        })
}

/// The probes on which the new code behaves differently from the current code: different values
/// first, then the rest; inputs the current code throws on are summed up when there are many,
/// as new code often adds guards for them.
pub fn differences(probes: &[String], current: &EvalOutcome, candidate: &EvalOutcome) -> Vec<String> {
    let mut values = Vec::new();
    let mut others = Vec::new();
    let mut guarded = Vec::new();
    for (i, input) in probes.iter().enumerate() {
        let (Some(old), Some(new)) = (current.report.result(i), candidate.report.result(i)) else {
            continue;
        };
        let value = |c: &clojurecraft_clojure::CallResult| {
            format!("returns {}", excerpt(c.actual.as_deref().unwrap_or_default()))
        };
        let thrown = |c: &clojurecraft_clojure::CallResult| {
            let e = c.error.clone().unwrap_or_default();
            format!("throws {}", e.class.rsplit('.').next().unwrap_or_default())
        };
        let line = |was: String, now: String| {
            format!(
                "For {}, the current code {was}, the new code {now}.",
                excerpt(input)
            )
        };
        match (old.status, new.status) {
            (CallStatus::Ok, CallStatus::Ok) if new.matches == Some(false) => {
                values.push(line(value(old), value(new)));
            }
            (CallStatus::Error, CallStatus::Ok) => guarded.push(line(thrown(old), value(new))),
            (CallStatus::Ok, CallStatus::Error) => others.push(line(value(old), thrown(new))),
            (CallStatus::Timeout, CallStatus::Ok) => others.push(line("does not finish".into(), value(new))),
            (CallStatus::Ok, CallStatus::Timeout) => others.push(line(value(old), "does not finish".into())),
            _ => {}
        }
    }
    let mut out: Vec<String> = values.into_iter().chain(others).collect();
    if guarded.len() > 2 {
        let first = guarded.swap_remove(0);
        out.push(format!(
            "{first} It also returns a value on {} more inputs where the current code throws.",
            guarded.len()
        ));
    } else {
        out.extend(guarded);
    }
    out.truncate(MAX_DIFFERENCES);
    out
}

fn excerpt(text: &str) -> String {
    const MAX: usize = 200;
    match text.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_empty_collections_drop_entries_and_nil_values_shallowest_first() {
        assert_eq!(
            variants("{:items [1 2] :id 7}", 1),
            [
                "{}",
                "{:id 7}",
                "{:items nil :id 7}",
                "{:items [1 2] }",
                "{:items [1 2] :id nil}",
                "{:items [] :id 7}",
                "nil",
            ]
        );
    }

    #[test]
    fn with_several_arguments_each_argument_is_changed_and_the_vector_kept() {
        let v = variants("[{:a 1} {\"m\" 2}]", 2);
        assert!(v.iter().all(|x| x.starts_with('[') && x.ends_with(']')), "{v:?}");
        assert!(v.contains(&"[nil {\"m\" 2}]".to_owned()));
        assert!(v.contains(&"[{:a 1} {}]".to_owned()));
        assert!(!v.contains(&"[]".to_owned()));
    }

    fn outcome(results: &[&str]) -> EvalOutcome {
        let mut out = String::from("{\"event\":\"load\",\"ok\":true}\n");
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("{{\"event\":\"result\",\"index\":{i},{r}}}\n"));
        }
        EvalOutcome {
            status: clojurecraft_clojure::EvalStatus::Completed,
            report: clojurecraft_clojure::HarnessReport::parse(&out),
            stderr: String::new(),
            elapsed: std::time::Duration::ZERO,
        }
    }

    #[test]
    fn value_differences_lead_and_many_added_guards_are_summed_up() {
        let threw = r#""status":"error","error":{"phase":"call","class":"java.lang.NullPointerException","message":"x"}"#;
        let current = outcome(&[threw, threw, threw, r#""status":"ok","actual":"1""#]);
        let new = outcome(&[
            r#""status":"ok","actual":"0""#,
            r#""status":"ok","actual":"0""#,
            r#""status":"ok","actual":"0""#,
            r#""status":"ok","actual":"2","match":false"#,
        ]);
        let probes: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        assert_eq!(
            differences(&probes, &current, &new),
            [
                "For d, the current code returns 1, the new code returns 2.",
                "For a, the current code throws NullPointerException, the new code returns 0. It also \
                 returns a value on 2 more inputs where the current code throws."
            ]
        );
    }

    #[test]
    fn probes_skip_inputs_that_are_examples_already() {
        let mut doc = FunctionDocument::new("f");
        doc.examples = vec![
            clojurecraft_domain::Example::direct(
                "",
                clojurecraft_domain::Edn::new("[1]"),
                clojurecraft_domain::Edn::new("1"),
            ),
            clojurecraft_domain::Example::direct(
                "",
                clojurecraft_domain::Edn::new("nil"),
                clojurecraft_domain::Edn::new("0"),
            ),
        ];
        assert_eq!(probes(&doc), ["[]"]);
    }
}
