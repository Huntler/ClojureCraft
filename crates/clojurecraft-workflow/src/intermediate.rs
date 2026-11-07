//! Executed intermediate values of transformation steps.
//!
//! The explanation of generated code attributes source lines to each step. For each step this
//! module picks one expression inside those lines whose value can be observed without changing
//! what the code does: an expression in an evaluated position (a function body, a `let` value, a
//! call argument, a branch), or a step of a `->`/`->>` pipeline, after which the threaded value
//! is observed. An instrumented copy of the source with a probe at each chosen place is run on
//! every usable example in the sandbox (see [`clojurecraft_clojure::probe`]).
//!
//! Values are only ever executed results, never the model's predictions. A step has no value
//! shown when its code could not be located, when no single expression can be observed, when
//! the expression was never evaluated, or when the instrumented run did not end exactly like the
//! uninstrumented one. Steps carried out by the same expression (for example fused into one
//! transducer or one threading step) share one value, and the mapping says so.

use std::ops::Range;

use clojurecraft_clojure::probe::{PROBE, PROBE_FIRST};
use clojurecraft_clojure::{Call, EvalOutcome, EvalRequest, EvalStatus, ProbeValue, ValueKind};
use clojurecraft_domain::{ExampleId, Fingerprint, FunctionDocument, LineRange, StepId, StepMapping};
use clojurecraft_edn::{Delim, TokenKind, line_col};

use crate::deps;
use crate::references::names_for;
use crate::spec;
use crate::structure::{self, Expectations, Form};
use crate::validation::{CALL_TIMEOUT, MAX_ELEMENTS};

/// Where a probe observes a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub lines: LineRange,
    /// The observed code, shortened to one line.
    pub excerpt: String,
    /// The value is the one threaded out of this pipeline step, not the step form itself.
    pub after_pipeline_step: bool,
}

impl Target {
    /// What the value is, for example "value of `(filter :paid orders)` (line 3)".
    pub fn describe(&self) -> String {
        let place = if self.lines.start == self.lines.end {
            format!("line {}", self.lines.start)
        } else {
            format!("lines {}–{}", self.lines.start, self.lines.end)
        };
        if self.after_pipeline_step {
            format!("value after the pipeline step `{}` ({place})", self.excerpt)
        } else {
            format!("value of `{}` ({place})", self.excerpt)
        }
    }
}

/// How a step's intermediate value is obtained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepPlan {
    /// Observed by probe `probe`. `fused_with` lists the other steps whose code is the same
    /// expression, so the value is the combined result of all of them.
    Probed { probe: usize, fused_with: Vec<StepId> },
    /// No trustworthy value can be obtained, and why.
    Unavailable(String),
}

/// The instrumented source and what each step's value will be.
#[derive(Clone, Debug)]
pub struct Instrumentation {
    pub code: String,
    pub targets: Vec<Target>,
    pub steps: Vec<(StepId, StepPlan)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SiteKind {
    Expression,
    /// A step of `->`/`some->` (value inserted first) or `->>`/`some->>` (value inserted last).
    ThreadStep {
        first: bool,
    },
}

#[derive(Clone, Debug)]
struct Site {
    span: Range<usize>,
    kind: SiteKind,
}

/// Forms that bind names in a vector of name/value pairs, followed by a body.
const BINDING_FORMS: [&str; 7] = [
    "let",
    "loop",
    "when-let",
    "if-let",
    "when-some",
    "if-some",
    "when-first",
];
/// Forms whose arguments are not all evaluated expressions, and which are not entered.
const OPAQUE_FORMS: [&str; 9] = [
    "quote", "comment", "cond->", "cond->>", "as->", "condp", "letfn", "declare", "def",
];
const THREAD_FIRST: [&str; 2] = ["->", "some->"];
const THREAD_LAST: [&str; 2] = ["->>", "some->>"];

/// Whether `form` contains a `recur` that refers to a loop or function outside it; wrapping such
/// a form would move the `recur` out of tail position.
fn has_free_recur(form: &Form) -> bool {
    match form.bare() {
        Form::Seq { delim, items, .. } => {
            if *delim == Delim::Paren {
                match items.first().and_then(Form::symbol) {
                    Some("recur") => return true,
                    Some("loop" | "fn" | "fn*") => return false,
                    _ => {}
                }
            }
            items.iter().any(has_free_recur)
        }
        // `#(...)` is a function of its own.
        Form::Prefixed { prefix, inner, .. } => prefix != "#" && has_free_recur(inner),
        Form::Atom { .. } | Form::Meta { .. } => false,
    }
}

struct Walker {
    sites: Vec<Site>,
}

impl Walker {
    fn site(&mut self, form: &Form, kind: SiteKind) {
        if !has_free_recur(form) {
            self.sites.push(Site {
                span: form.span(),
                kind,
            });
        }
    }

    /// Bodies of `fn`/`defn` arities: `[params] body…` or `([params] body…)…`.
    fn arities(&mut self, rest: &[Form]) {
        let rest = match rest.first() {
            Some(f) if f.symbol().is_some() => &rest[1..],
            _ => rest,
        };
        if let Some(first) = rest.first()
            && first.vector().is_some()
        {
            self.body(&rest[1..]);
            return;
        }
        for arity in rest {
            if let Some(items) = arity.list()
                && items.first().is_some_and(|p| p.vector().is_some())
            {
                self.body(&items[1..]);
            }
        }
    }

    fn body(&mut self, forms: &[Form]) {
        // A leading condition map (`{:pre … :post …}`) is not an expression.
        let forms = match forms {
            [first, _, ..]
                if matches!(
                    first.bare(),
                    Form::Seq {
                        delim: Delim::Brace,
                        ..
                    }
                ) =>
            {
                &forms[1..]
            }
            _ => forms,
        };
        for f in forms {
            self.expression(f);
        }
    }

    fn bindings(&mut self, vector: &Form) {
        if let Some(items) = vector.vector() {
            for pair in items.chunks(2) {
                if let [_, value] = pair {
                    self.expression(value);
                }
            }
        }
    }

    fn expression(&mut self, form: &Form) {
        match form.bare() {
            Form::Seq {
                delim: Delim::Paren,
                items,
                ..
            } => {
                let head = items.first().and_then(Form::symbol);
                match head {
                    Some(h) if OPAQUE_FORMS.contains(&h) => {
                        self.site(form, SiteKind::Expression);
                        return;
                    }
                    Some("fn" | "fn*") => {
                        self.arities(&items[1..]);
                        return;
                    }
                    Some("recur") => {
                        for f in &items[1..] {
                            self.expression(f);
                        }
                        return;
                    }
                    _ => {}
                }
                self.site(form, SiteKind::Expression);
                match head {
                    Some(h) if BINDING_FORMS.contains(&h) => {
                        if let Some(v) = items.get(1) {
                            self.bindings(v);
                        }
                        self.body(items.get(2..).unwrap_or_default());
                    }
                    Some("for" | "doseq") => {
                        if let Some(items) = items.get(1).and_then(Form::vector) {
                            for pair in items.chunks(2) {
                                match pair {
                                    [k, v] if k.symbol().is_none() && is_keyword(k, ":let") => {
                                        self.bindings(v)
                                    }
                                    [_, v] => self.expression(v),
                                    _ => {}
                                }
                            }
                        }
                        self.body(items.get(2..).unwrap_or_default());
                    }
                    Some("case") => {
                        if let Some(e) = items.get(1) {
                            self.expression(e);
                        }
                        let clauses = items.get(2..).unwrap_or_default();
                        for pair in clauses.chunks(2) {
                            match pair {
                                [_, result] => self.expression(result),
                                [default] => self.expression(default),
                                _ => {}
                            }
                        }
                    }
                    Some(h) if THREAD_FIRST.contains(&h) || THREAD_LAST.contains(&h) => {
                        let first = THREAD_FIRST.contains(&h);
                        if let Some(x) = items.get(1) {
                            self.expression(x);
                        }
                        for step in items.get(2..).unwrap_or_default() {
                            self.site(step, SiteKind::ThreadStep { first });
                            if let Some(args) = step.list() {
                                for a in args.get(1..).unwrap_or_default() {
                                    self.expression(a);
                                }
                            }
                        }
                    }
                    _ => {
                        for f in items {
                            self.expression(f);
                        }
                    }
                }
            }
            Form::Seq { items, .. } => {
                self.site(form, SiteKind::Expression);
                for f in items {
                    self.expression(f);
                }
            }
            // The body of `#(...)` is a call whose arguments are expressions.
            Form::Prefixed { prefix, inner, .. } if prefix == "#" => {
                if let Some(items) = inner.list() {
                    for f in items {
                        self.expression(f);
                    }
                }
            }
            Form::Prefixed { .. } | Form::Atom { .. } | Form::Meta { .. } => {}
        }
    }

    fn top_level(&mut self, form: &Form) {
        let Some(items) = form.list() else { return };
        if matches!(items.first().and_then(Form::symbol), Some("defn" | "defn-")) {
            let rest = items.get(2..).unwrap_or_default();
            // Skip a docstring and an attribute map before the arities.
            let rest = match rest.first() {
                Some(f) if is_string(f) => &rest[1..],
                _ => rest,
            };
            let rest = match rest.first() {
                Some(f)
                    if matches!(
                        f.bare(),
                        Form::Seq {
                            delim: Delim::Brace,
                            ..
                        }
                    ) =>
                {
                    &rest[1..]
                }
                _ => rest,
            };
            self.arities(rest);
        }
    }
}

fn is_keyword(form: &Form, name: &str) -> bool {
    matches!(form.bare(), Form::Atom { kind: TokenKind::Keyword, text, .. } if text == name)
}

fn is_string(form: &Form) -> bool {
    matches!(
        form.bare(),
        Form::Atom {
            kind: TokenKind::String { .. },
            ..
        }
    )
}

fn lines_of(src: &str, span: &Range<usize>) -> LineRange {
    let start = line_col(src, span.start).0;
    let end = line_col(src, span.end.saturating_sub(1).max(span.start)).0;
    LineRange {
        start: u32::try_from(start).unwrap_or(u32::MAX),
        end: u32::try_from(end).unwrap_or(u32::MAX),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The site observed for a step's lines: among the sites entirely inside them, the outermost
/// ones; of those, the one that ends last (the step's final value), so consecutive pipeline
/// steps show the value after the last of them.
fn choose(sites: &[(Site, LineRange)], lines: LineRange) -> Option<usize> {
    let inside: Vec<usize> = (0..sites.len())
        .filter(|&i| sites[i].1.start >= lines.start && sites[i].1.end <= lines.end)
        .collect();
    let contains = |outer: &Range<usize>, inner: &Range<usize>| {
        outer.start <= inner.start && inner.end <= outer.end && outer != inner
    };
    inside
        .iter()
        .copied()
        .filter(|&i| {
            !inside
                .iter()
                .any(|&j| j != i && contains(&sites[j].0.span, &sites[i].0.span))
        })
        .max_by_key(|&i| (sites[i].0.span.end, std::cmp::Reverse(sites[i].0.span.start)))
}

/// Why intermediate values cannot be shown for this implementation at all, if they cannot.
fn explanation(doc: &FunctionDocument) -> Result<&[StepMapping], String> {
    let Some(implementation) = &doc.implementation else {
        return Err("There is no implementation to run.".into());
    };
    if doc.steps.is_empty() {
        return Err("This function has no transformation steps to inspect.".into());
    }
    let Some(generation) = &implementation.generation else {
        return Err(
            "The code was written by hand, so no step is mapped to its code and no intermediate value \
             can be attributed to a step."
                .into(),
        );
    };
    if implementation.is_manually_edited() {
        return Err(
            "The code was edited after it was explained, so the step-to-code mapping no longer applies."
                .into(),
        );
    }
    match &generation.explanation {
        Some(e) if !e.steps.is_empty() => Ok(&e.steps),
        _ => Err("The code has no step-to-code explanation to locate the steps in.".into()),
    }
}

/// Chooses an observable expression for each step and builds the instrumented source.
pub fn instrument(doc: &FunctionDocument) -> Result<Instrumentation, String> {
    let mappings = explanation(doc)?;
    let source = doc
        .implementation
        .as_ref()
        .map(|i| i.source.as_str())
        .unwrap_or_default();
    let forms = structure::read_all(source).map_err(|(message, at)| {
        format!(
            "The code cannot be read (line {}): {message}",
            line_col(source, at).0
        )
    })?;
    let mut walker = Walker { sites: Vec::new() };
    for f in &forms {
        walker.top_level(f);
    }
    let sites: Vec<(Site, LineRange)> = walker
        .sites
        .into_iter()
        .map(|s| {
            let lines = lines_of(source, &s.span);
            (s, lines)
        })
        .collect();

    // Step → chosen site, in step order.
    let mut chosen: Vec<(StepId, Result<usize, String>)> = Vec::new();
    for step in &doc.steps {
        let choice = match mappings.iter().find(|m| m.step == step.id) {
            None => Err("The explanation attributes no code to this step.".to_owned()),
            Some(m) if m.step_text.trim() != step.text.trim() => Err(
                "The step was reworded after the code was explained, so its code is not known.".to_owned(),
            ),
            Some(StepMapping { lines: None, .. }) => Err(
                "The code the explanation attributes to this step was not found in the implementation."
                    .to_owned(),
            ),
            Some(StepMapping {
                lines: Some(lines), ..
            }) => choose(&sites, *lines).ok_or_else(|| {
                "Its lines hold no expression whose value can be observed on its own (only names, \
                 parameters, or part of a larger form)."
                    .to_owned()
            }),
        };
        chosen.push((step.id, choice));
    }

    // One probe per distinct site.
    let mut probe_sites: Vec<usize> = Vec::new();
    for (_, c) in &chosen {
        if let Ok(site) = c
            && !probe_sites.contains(site)
        {
            probe_sites.push(*site);
        }
    }
    let targets: Vec<Target> = probe_sites
        .iter()
        .map(|&i| {
            let (site, lines) = &sites[i];
            Target {
                lines: *lines,
                excerpt: one_line(&source[site.span.clone()], 60),
                after_pipeline_step: matches!(site.kind, SiteKind::ThreadStep { .. }),
            }
        })
        .collect();
    let steps = chosen
        .iter()
        .map(|(step, c)| {
            let plan = match c {
                Ok(site) => StepPlan::Probed {
                    probe: probe_sites.iter().position(|s| s == site).unwrap_or_default(),
                    fused_with: chosen
                        .iter()
                        .filter(|(other, oc)| other != step && oc.as_ref().ok() == Some(site))
                        .map(|(other, _)| *other)
                        .collect(),
                },
                Err(why) => StepPlan::Unavailable(why.clone()),
            };
            (*step, plan)
        })
        .collect();

    // Insert probes from the end of the source backwards, so earlier offsets stay valid.
    let mut insertions: Vec<(usize, String)> = Vec::new();
    for (probe, &i) in probe_sites.iter().enumerate() {
        let span = &sites[i].0.span;
        match sites[i].0.kind {
            SiteKind::Expression => {
                insertions.push((span.start, format!("({PROBE} {probe} ")));
                insertions.push((span.end, ")".to_owned()));
            }
            SiteKind::ThreadStep { first } => {
                let probe_fn = if first { PROBE_FIRST } else { PROBE };
                insertions.push((span.end, format!(" ({probe_fn} {probe})")));
            }
        }
    }
    insertions.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let mut code = source.to_owned();
    for (at, text) in insertions {
        code.insert_str(at, &text);
    }
    Ok(Instrumentation { code, targets, steps })
}

/// A planned inspection run.
#[derive(Clone, Debug)]
pub struct InspectionPlan {
    pub instrumentation: Instrumentation,
    /// The examples run, in call order, with their labels.
    pub examples: Vec<(ExampleId, String)>,
    pub request: EvalRequest,
    /// The specification and code the values belong to.
    pub spec: Fingerprint,
    pub code: Fingerprint,
}

/// Plans running every usable example with the instrumented code.
pub fn plan(doc: &FunctionDocument) -> Result<InspectionPlan, String> {
    let instrumentation = instrument(doc)?;
    if !instrumentation
        .steps
        .iter()
        .any(|(_, p)| matches!(p, StepPlan::Probed { .. }))
    {
        return Err("No step's code can be observed in this implementation.".into());
    }
    let examples = spec::usable_examples(doc);
    if examples.is_empty() {
        return Err("There is no example with an input and an expected output to run.".into());
    }
    let source = doc
        .implementation
        .as_ref()
        .map(|i| i.source.as_str())
        .unwrap_or_default();
    let names = names_for(doc);
    let allowed = deps::allowed(doc);
    let checked = structure::check(
        source,
        &Expectations {
            namespace: &names.namespace,
            function: &names.function,
            allowed: &allowed,
        },
    );
    let Some(function) = checked.function.clone().filter(|_| checked.problems.is_empty()) else {
        return Err("The code does not pass the structural check, so it cannot be run.".into());
    };
    let mut request = EvalRequest::new(
        source,
        function,
        examples
            .iter()
            .map(|(_, _, input)| Call {
                input: (*input).to_owned(),
                expected: None,
            })
            .collect(),
    );
    request.call_timeout = CALL_TIMEOUT;
    request.max_elements = MAX_ELEMENTS;
    request.probe_code = Some(instrumentation.code.clone());
    Ok(InspectionPlan {
        instrumentation,
        examples: examples
            .iter()
            .map(|(label, e, _)| (e.id, label.clone()))
            .collect(),
        request,
        spec: doc.spec_fingerprint(),
        code: doc.code_fingerprint(),
    })
}

/// A step's observed value in one example.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepValue {
    /// Executed values: how often the expression was evaluated during the call, and the first
    /// few values (each a bounded preview that says whether it is complete).
    Observed {
        evaluations: u64,
        values: Vec<ProbeValue>,
    },
    /// The expression was never evaluated in this call (for example a branch not taken).
    NotReached,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExampleInspection {
    pub example: ExampleId,
    pub label: String,
    /// Whether the call itself returned, or why not.
    pub call: Result<(), String>,
    pub steps: Vec<(StepId, StepValue)>,
}

/// The executed values of an inspection run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub spec: Fingerprint,
    pub code: Fingerprint,
    pub targets: Vec<Target>,
    pub plans: Vec<(StepId, StepPlan)>,
    /// Why no values could be obtained at all.
    pub problem: Option<String>,
    pub examples: Vec<ExampleInspection>,
}

impl Inspection {
    pub fn example(&self, id: ExampleId) -> Option<&ExampleInspection> {
        self.examples.iter().find(|e| e.example == id)
    }

    pub fn plan(&self, step: StepId) -> Option<&StepPlan> {
        self.plans.iter().find(|(s, _)| *s == step).map(|(_, p)| p)
    }
}

impl ExampleInspection {
    pub fn value(&self, step: StepId) -> Option<&StepValue> {
        self.steps.iter().find(|(s, _)| *s == step).map(|(_, v)| v)
    }
}

/// Reads the values of a finished inspection run.
pub fn inspect(plan: &InspectionPlan, outcome: Result<&EvalOutcome, &str>) -> Inspection {
    let mut inspection = Inspection {
        spec: plan.spec.clone(),
        code: plan.code.clone(),
        targets: plan.instrumentation.targets.clone(),
        plans: plan.instrumentation.steps.clone(),
        problem: None,
        examples: Vec::new(),
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            inspection.problem = Some(e.to_owned());
            return inspection;
        }
    };
    let report = &outcome.report;
    let stage_error = [&report.read, &report.check, &report.load]
        .into_iter()
        .find_map(|s| s.as_ref().and_then(|r| r.as_ref().err()));
    inspection.problem = match (&report.probe_setup, stage_error, &outcome.status) {
        (_, Some(e), _) => Some(format!("The code could not be loaded: {}", e.message)),
        (Some(Err(e)), _, _) => Some(format!("The code could not be instrumented: {e}.")),
        (None, _, EvalStatus::Completed) => Some("The runtime did not run the instrumented code.".into()),
        (None, _, status) => Some(format!(
            "The run stopped before any value was recorded ({status:?})."
        )),
        (Some(Ok(())), _, _) => None,
    };
    for (index, (example, label)) in plan.examples.iter().enumerate() {
        let call = match report.result(index) {
            Some(r) if r.status == clojurecraft_clojure::CallStatus::Ok => Ok(()),
            Some(r) => Err(r
                .error
                .as_ref()
                .map(|e| format!("The call threw: {}", e.message))
                .or_else(|| r.message.clone())
                .unwrap_or_else(|| format!("The call ended with {:?}.", r.status))),
            None => Err("The call was not run.".into()),
        };
        let probed = report.probes.iter().find(|p| p.index == index);
        let steps = plan
            .instrumentation
            .steps
            .iter()
            .map(|(step, step_plan)| {
                let value = match (step_plan, probed) {
                    (StepPlan::Unavailable(why), _) => StepValue::Unavailable(why.clone()),
                    (StepPlan::Probed { .. }, None) => StepValue::Unavailable(
                        inspection
                            .problem
                            .clone()
                            .unwrap_or_else(|| "The instrumented call did not run.".into()),
                    ),
                    (StepPlan::Probed { .. }, Some(p)) if !p.consistent => StepValue::Unavailable(
                        "The instrumented run did not end like the checked code, so its values are not \
                         shown."
                            .into(),
                    ),
                    (StepPlan::Probed { probe, .. }, Some(p)) => match p.capture(*probe) {
                        Some(c) => StepValue::Observed {
                            evaluations: c.evaluations,
                            values: c.values.clone(),
                        },
                        None => StepValue::NotReached,
                    },
                };
                (*step, value)
            })
            .collect();
        inspection.examples.push(ExampleInspection {
            example: *example,
            label: label.clone(),
            call,
            steps,
        });
    }
    inspection
}

/// Whether a recorded value is data that can be shown.
pub fn is_data(value: &ProbeValue) -> bool {
    value.kind == ValueKind::Value
}

#[cfg(test)]
mod tests {
    use clojurecraft_clojure::{CallResult, CallStatus, HarnessReport, ProbeCall, ProbeCapture, ProbeStatus};
    use clojurecraft_domain::{
        Edn, Example, Explanation, GenerationRecord, GenerationSettings, Implementation, InferenceRuntime,
        ModelIdentity, Step, Timestamp,
    };

    use super::*;

    const CODE: &str = "(ns totals.core)

(defn totals [orders]
  (let [paid (filter :paid orders)]
    (->> paid
         (map :amount)
         (reduce +))))";

    fn generated(source: &str, steps: &[(&str, Option<(u32, u32)>)]) -> FunctionDocument {
        let mut doc = FunctionDocument::new("totals");
        doc.examples.push(Example::direct(
            "one",
            Edn::new("[{:amount 3 :paid true}]"),
            Edn::new("3"),
        ));
        let mut mappings = Vec::new();
        for (text, lines) in steps {
            let step = Step::new(*text);
            mappings.push(StepMapping {
                step: step.id,
                step_text: (*text).into(),
                excerpt: String::new(),
                lines: lines.map(|(start, end)| LineRange { start, end }),
                explanation: String::new(),
            });
            doc.steps.push(step);
        }
        doc.implementation = Some(Implementation {
            source: source.into(),
            generation: Some(GenerationRecord {
                generated_source: source.into(),
                model: ModelIdentity {
                    display_name: String::new(),
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
                    context_length: 0,
                    max_output_tokens: 0,
                },
                documentation: Vec::new(),
                spec_basis: Fingerprint::from_stored("x"),
                attempts: 1,
                created_at: Timestamp(0),
                namespace: "totals.core".into(),
                function_name: "totals".into(),
                requires: Vec::new(),
                assumptions: Vec::new(),
                omitted_context: Vec::new(),
                explanation: Some(Explanation {
                    summary: String::new(),
                    steps: mappings,
                    symbols: Vec::new(),
                }),
            }),
        });
        doc
    }

    fn probed(plan: &StepPlan) -> usize {
        match plan {
            StepPlan::Probed { probe, .. } => *probe,
            StepPlan::Unavailable(why) => panic!("unavailable: {why}"),
        }
    }

    #[test]
    fn steps_are_observed_at_let_values_and_after_pipeline_steps() {
        let doc = generated(
            CODE,
            &[
                ("keep paid orders", Some((4, 4))),
                ("take amounts", Some((6, 6))),
                ("sum", Some((7, 7))),
            ],
        );
        let i = instrument(&doc).expect("instrumented");
        assert_eq!(
            i.code,
            "(ns totals.core)

(defn totals [orders]
  (let [paid (clojurecraft.harness/probe! 0 (filter :paid orders))]
    (->> paid
         (map :amount) (clojurecraft.harness/probe! 1)
         (reduce +) (clojurecraft.harness/probe! 2))))"
        );
        assert_eq!(probed(&i.steps[1].1), 1);
        assert_eq!(
            i.targets[0].describe(),
            "value of `(filter :paid orders)` (line 4)"
        );
        assert_eq!(
            i.targets[1].describe(),
            "value after the pipeline step `(map :amount)` (line 6)"
        );
    }

    #[test]
    fn steps_sharing_one_expression_are_reported_as_fused() {
        let code = "(ns totals.core)\n(defn totals [orders]\n  (transduce (comp (filter :paid) (map :amount)) + orders))";
        let doc = generated(code, &[("keep paid", Some((3, 3))), ("amounts", Some((3, 3)))]);
        let i = instrument(&doc).expect("instrumented");
        let (first, second) = (doc.steps[0].id, doc.steps[1].id);
        assert_eq!(
            i.steps[0].1,
            StepPlan::Probed {
                probe: 0,
                fused_with: vec![second]
            }
        );
        assert_eq!(
            i.steps[1].1,
            StepPlan::Probed {
                probe: 0,
                fused_with: vec![first]
            }
        );
        assert_eq!(i.targets.len(), 1);
        assert!(i.code.contains("(clojurecraft.harness/probe! 0 (transduce"));
    }

    #[test]
    fn steps_without_located_or_observable_code_are_unavailable_with_a_reason() {
        let code = "(ns f.core)\n(defn f\n  [xs]\n  (loop [acc 0 xs xs]\n    (if (seq xs)\n      (recur (+ acc (first xs)) (rest xs))\n      acc)))";
        let doc = generated(
            code,
            &[
                ("not found", None),
                ("parameters only", Some((3, 3))),
                ("accumulate", Some((5, 7))),
            ],
        );
        let i = instrument(&doc).expect("instrumented");
        assert!(matches!(&i.steps[0].1, StepPlan::Unavailable(w) if w.contains("not found")));
        assert!(matches!(&i.steps[1].1, StepPlan::Unavailable(w) if w.contains("no expression")));
        // The `if` holds a `recur` in tail position and is never wrapped, nor is the `recur`;
        // of the expressions inside them, the one evaluated last is observed.
        assert!(!i.code.contains("probe! 0 (if"));
        assert!(!i.code.contains("probe! 0 (recur"));
        assert!(
            i.code
                .contains("(recur (+ acc (first xs)) (clojurecraft.harness/probe! 0 (rest xs)))"),
            "{}",
            i.code
        );
    }

    #[test]
    fn hand_written_or_edited_code_has_no_step_values() {
        let mut doc = generated(CODE, &[("keep paid orders", Some((4, 4)))]);
        if let Some(i) = &mut doc.implementation {
            i.source.push_str("\n;; edited");
        }
        assert!(
            instrument(&doc)
                .unwrap_err()
                .contains("edited after it was explained")
        );
        doc.implementation = Some(Implementation::manual(CODE));
        assert!(instrument(&doc).unwrap_err().contains("written by hand"));
    }

    fn outcome(probes: Vec<ProbeCall>) -> EvalOutcome {
        EvalOutcome {
            status: EvalStatus::Completed,
            report: HarnessReport {
                read: Some(Ok(())),
                check: Some(Ok(())),
                load: Some(Ok(())),
                results: vec![CallResult {
                    index: 0,
                    status: CallStatus::Ok,
                    actual: Some("3".into()),
                    truncated: false,
                    matches: None,
                    difference: None,
                    contract: Vec::new(),
                    out: String::new(),
                    elapsed_ns: 0,
                    error: None,
                    message: None,
                }],
                probe_setup: Some(Ok(())),
                probes,
                done: true,
                ..HarnessReport::default()
            },
            stderr: String::new(),
            elapsed: std::time::Duration::ZERO,
        }
    }

    fn value(text: &str, complete: bool) -> ProbeValue {
        ProbeValue {
            kind: ValueKind::Value,
            text: text.into(),
            complete,
            message: None,
        }
    }

    #[test]
    fn only_consistent_executed_values_are_shown() {
        let doc = generated(
            CODE,
            &[
                ("keep paid orders", Some((4, 4))),
                ("take amounts", Some((6, 6))),
                ("sum", Some((7, 7))),
            ],
        );
        let p = plan(&doc).expect("plan");
        assert_eq!(
            p.request.probe_code.as_deref(),
            Some(p.instrumentation.code.as_str())
        );
        let call = ProbeCall {
            index: 0,
            status: ProbeStatus::Ok,
            message: None,
            consistent: true,
            probes: vec![
                ProbeCapture {
                    id: 0,
                    evaluations: 1,
                    values: vec![value("({:amount 3, :paid true})", true)],
                },
                ProbeCapture {
                    id: 1,
                    evaluations: 1,
                    values: vec![value("(3 …", false)],
                },
            ],
        };
        let inspection = inspect(&p, Ok(&outcome(vec![call.clone()])));
        assert_eq!(inspection.problem, None);
        let e = &inspection.examples[0];
        assert_eq!(e.call, Ok(()));
        assert!(matches!(
            e.value(doc.steps[1].id),
            Some(StepValue::Observed { values, .. }) if !values[0].complete
        ));
        assert_eq!(e.value(doc.steps[2].id), Some(&StepValue::NotReached));

        // A run that did not end like the checked code shows nothing.
        let inconsistent = ProbeCall {
            consistent: false,
            ..call
        };
        let inspection = inspect(&p, Ok(&outcome(vec![inconsistent])));
        assert!(matches!(
            inspection.examples[0].value(doc.steps[0].id),
            Some(StepValue::Unavailable(w)) if w.contains("did not end like")
        ));

        // A refused instrumentation shows nothing either, with the harness's reason.
        let mut refused = outcome(Vec::new());
        refused.report.probe_setup = Some(Err("the instrumented code is not the checked code".into()));
        let inspection = inspect(&p, Ok(&refused));
        assert!(
            inspection
                .problem
                .as_deref()
                .is_some_and(|m| m.contains("could not be instrumented"))
        );
        assert!(matches!(
            inspection.examples[0].value(doc.steps[0].id),
            Some(StepValue::Unavailable(_))
        ));
    }
}
