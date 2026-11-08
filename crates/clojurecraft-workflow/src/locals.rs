//! The values of locals when a call threw, for diagnostics that show why it threw.
//!
//! An exception such as `NullPointerException ... because "s" is null` names neither the value
//! nor where it came from: often a destructuring pattern that did not fit the data, several
//! calls further out. So validation also runs every call that threw with an instrumented copy
//! of the code: each function arity (`defn`, `defn-`, `fn`) and each `let`, `loop`, `when-let`,
//! and `if-let` body records the values of the locals it binds whenever it is entered, keeping
//! only the latest values (see [`clojurecraft_clojure::probe::PROBE_LOCALS`]). The stack trace
//! names the lines of the candidate's code that were running; for each of them the scopes around
//! that line, up to the enclosing function, show their locals:
//!
//! ```text
//! in (defn- parse-weight [weight]) at line 15: weight = nil
//! in (fn [[date exercise sets-reps weight]]) at line 24: date = "2026-03-02", …, weight = nil
//! ```
//!
//! The instrumented copy inserts text only within lines, so line numbers stay the same, and the
//! harness checks that removing the probes gives back exactly the checked code. Values are shown
//! only when the instrumented run threw the same exception class as the checked one.

use std::ops::Range;

use clojurecraft_clojure::probe::PROBE_LOCALS;
use clojurecraft_clojure::{HarnessError, ProbeCall, ValueKind};
use clojurecraft_edn::{Delim, TokenKind, line_col};

use crate::insert::Insertion;
use crate::structure::{Form, read_all};

/// Locals recorded per scope at most.
const MAX_NAMES: usize = 12;
/// Characters of one value in a diagnostic.
const MAX_VALUE_CHARS: usize = 240;
/// Characters of the whole description.
const MAX_CHARS: usize = 1500;

/// A function arity or a binding form whose locals are recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The probe id.
    pub id: usize,
    /// How the scope is named in diagnostics, such as `(defn- parse-weight [weight])` or `let`.
    pub label: String,
    /// 1-based first and last line of the scope.
    pub lines: (usize, usize),
    /// A function arity, rather than a binding form inside one.
    pub function: bool,
    span: Range<usize>,
}

/// The scopes of a source whose locals are recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locals {
    pub scopes: Vec<Scope>,
}

struct Walker<'a> {
    src: &'a str,
    first_id: usize,
    scopes: Vec<Scope>,
    insertions: Vec<Insertion>,
}

fn one_line(text: &str, max: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match t.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t,
    }
}

/// The names a binding pattern binds, in order.
fn pattern_names(p: &Form, out: &mut Vec<String>) {
    let p = p.bare();
    if let Some(sym) = p.symbol() {
        if sym != "&" && !sym.starts_with('_') && !sym.contains('/') {
            out.push(sym.to_owned());
        }
        return;
    }
    if let Some(items) = p.vector() {
        let mut i = 0;
        while i < items.len() {
            if items[i].keyword() == Some(":as") {
                if let Some(next) = items.get(i + 1) {
                    pattern_names(next, out);
                }
                i += 2;
                continue;
            }
            pattern_names(&items[i], out);
            i += 1;
        }
        return;
    }
    if let Form::Seq {
        delim: Delim::Brace,
        items,
        ..
    } = p
    {
        for kv in items.chunks(2) {
            let [k, v] = kv else { continue };
            match k.keyword() {
                Some(kw) if kw.ends_with("keys") || kw.ends_with("strs") || kw.ends_with("syms") => {
                    for item in v.vector().unwrap_or_default() {
                        let text = item
                            .symbol()
                            .or_else(|| item.keyword().map(|k| k.trim_start_matches(':')));
                        if let Some(t) = text {
                            let name = t.rsplit('/').next().unwrap_or(t);
                            out.push(name.to_owned());
                        }
                    }
                }
                Some(":as") => pattern_names(v, out),
                Some(_) => {}
                None => pattern_names(k, out),
            }
        }
    }
}

fn names_of<'f>(patterns: impl Iterator<Item = &'f Form>) -> Vec<String> {
    let mut out = Vec::new();
    for p in patterns {
        pattern_names(p, &mut out);
    }
    let mut seen = Vec::new();
    out.retain(|n| {
        let new = !seen.contains(n);
        seen.push(n.clone());
        new
    });
    out.truncate(MAX_NAMES);
    out
}

impl Walker<'_> {
    fn lines(&self, span: &Range<usize>) -> (usize, usize) {
        (line_col(self.src, span.start).0, line_col(self.src, span.end).0)
    }

    /// Records `names` before the first form of `body` that is evaluated as an expression.
    fn scope(
        &mut self,
        label: String,
        function: bool,
        span: Range<usize>,
        names: Vec<String>,
        body: &[Form],
    ) {
        // A leading condition map of a function arity is not an expression, when more follows.
        let first = match body {
            [
                Form::Seq {
                    delim: Delim::Brace, ..
                },
                second,
                ..,
            ] if function => second,
            [first, ..] => first,
            [] => return,
        };
        if names.is_empty() {
            return;
        }
        let id = self.first_id + self.scopes.len();
        let lines = self.lines(&span);
        self.insertions.push(Insertion::Wrap {
            span: first.span(),
            open: format!("({PROBE_LOCALS} {id} [{}] ", names.join(" ")),
            close: ")".into(),
        });
        self.scopes.push(Scope {
            id,
            label,
            lines,
            function,
            span,
        });
    }

    /// An arity `[params] body…` of a function labelled `head`.
    fn arity(&mut self, head: &str, params: &Form, body: &[Form], span: Range<usize>) {
        let names = names_of(params.vector().unwrap_or_default().iter());
        let label = format!("({head} {})", one_line(&self.src[params.span()], 60));
        self.scope(label, true, span, names, body);
    }

    /// The arities of `(defn name …)` or `(fn name? …)`, given the forms after the name.
    fn arities(&mut self, head: &str, rest: &[Form], whole: &Form) {
        let mut rest = rest;
        while let [first, tail @ ..] = rest {
            let skip = matches!(
                first.bare(),
                Form::Atom {
                    kind: TokenKind::String { .. },
                    ..
                }
            ) || matches!(
                first.bare(),
                Form::Seq {
                    delim: Delim::Brace,
                    ..
                }
            );
            if !skip {
                break;
            }
            rest = tail;
        }
        match rest {
            [params, body @ ..] if params.vector().is_some() => {
                self.arity(head, params, body, whole.span());
            }
            arities => {
                for a in arities {
                    if let Some([params, body @ ..]) = a.list()
                        && params.vector().is_some()
                    {
                        self.arity(head, params, body, a.span());
                    }
                }
            }
        }
    }

    fn walk(&mut self, form: &Form) {
        match form {
            Form::Prefixed { prefix, inner, .. } => {
                if prefix != "'" {
                    self.walk(inner);
                }
                return;
            }
            Form::Meta { target, .. } => {
                self.walk(target);
                return;
            }
            Form::Atom { .. } => return,
            Form::Seq { .. } => {}
        }
        if let Some(items) = form.list() {
            match items.first().and_then(Form::symbol) {
                Some("quote") => return,
                Some(head @ ("defn" | "defn-")) => {
                    if let Some(name) = items.get(1).and_then(Form::symbol) {
                        let head = format!("{head} {name}");
                        self.arities(&head, &items[2..], form);
                    }
                }
                Some("fn") => match items.get(1) {
                    Some(name) if name.symbol().is_some() => {
                        let head = format!("fn {}", name.symbol().unwrap_or_default());
                        self.arities(&head, &items[2..], form);
                    }
                    _ => self.arities("fn", &items[1..], form),
                },
                Some(head @ ("let" | "loop" | "when-let" | "if-let" | "when-some" | "if-some")) => {
                    if let Some(bindings) = items.get(1).and_then(Form::vector) {
                        let names = names_of(bindings.iter().step_by(2));
                        self.scope(head.to_owned(), false, form.span(), names, &items[2..]);
                    }
                }
                _ => {}
            }
        }
        if let Form::Seq { items, .. } = form {
            for item in items {
                self.walk(item);
            }
        }
    }
}

/// The scopes of `source` and the probes that record their locals, numbered from `first_id`;
/// `None` when the source cannot be read or binds no locals.
pub(crate) fn instrument(source: &str, first_id: usize) -> Option<(Locals, Vec<Insertion>)> {
    let forms = read_all(source).ok()?;
    let mut w = Walker {
        src: source,
        first_id,
        scopes: Vec::new(),
        insertions: Vec::new(),
    };
    for f in forms.iter().skip(1) {
        w.walk(f);
    }
    if w.scopes.is_empty() {
        return None;
    }
    Some((Locals { scopes: w.scopes }, w.insertions))
}

fn values_text(locals: &[clojurecraft_clojure::LocalValue]) -> String {
    locals
        .iter()
        .map(|l| {
            let v = match l.value.kind {
                ValueKind::Function => "a function".to_owned(),
                ValueKind::Error => "(could not be printed)".to_owned(),
                ValueKind::Value => one_line(&l.value.text, MAX_VALUE_CHARS),
            };
            format!("{} = {v}", l.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The latest locals of every scope a call that returned (with a wrong value) entered, in
/// source order, with how often each was entered; `None` when there is nothing to show.
pub fn describe_returned(locals: &Locals, run: &ProbeCall) -> Option<String> {
    if !run.consistent {
        return None;
    }
    let mut parts = Vec::new();
    let mut used = 0;
    for s in &locals.scopes {
        let Some(capture) = run.capture(s.id).filter(|c| !c.locals.is_empty()) else {
            continue;
        };
        let times = match capture.evaluations {
            1 => "entered once".to_owned(),
            n => format!("entered {n} times, latest values"),
        };
        let text = format!(
            "  in {} at line {} ({times}): {}",
            s.label,
            s.lines.0,
            values_text(&capture.locals)
        );
        if used + text.len() > MAX_CHARS {
            parts.push("  (more scopes are not shown)".to_owned());
            break;
        }
        used += text.len();
        parts.push(text);
    }
    (!parts.is_empty()).then(|| {
        format!(
            "Values of the locals in each scope the call entered, in source order:\n{}",
            parts.join("\n")
        )
    })
}

/// The values of locals around the lines `error` was thrown at, from the instrumented run of
/// the same call; `None` when there is nothing trustworthy to show.
pub fn describe(locals: &Locals, error: &HarnessError, run: &ProbeCall) -> Option<String> {
    if !run.consistent {
        return None;
    }
    let lines: Vec<usize> = if error.lines.is_empty() {
        error.line.into_iter().map(|l| l as usize).collect()
    } else {
        error.lines.iter().map(|l| *l as usize).collect()
    };
    let mut chosen: Vec<&Scope> = Vec::new();
    for line in lines {
        let mut around: Vec<&Scope> = locals
            .scopes
            .iter()
            .filter(|s| s.lines.0 <= line && line <= s.lines.1)
            .collect();
        around.sort_by_key(|s| s.span.len());
        for s in around {
            if !chosen.iter().any(|c| c.id == s.id) {
                chosen.push(s);
            }
            if s.function {
                break;
            }
        }
    }
    let mut parts = Vec::new();
    let mut used = 0;
    for s in chosen {
        let Some(capture) = run.capture(s.id).filter(|c| !c.locals.is_empty()) else {
            continue;
        };
        let text = format!(
            "  in {} at line {}: {}",
            s.label,
            s.lines.0,
            values_text(&capture.locals)
        );
        if used + text.len() > MAX_CHARS {
            break;
        }
        used += text.len();
        parts.push(text);
    }
    (!parts.is_empty()).then(|| {
        format!(
            "Values of the locals in the running code when it threw, innermost first:\n{}",
            parts.join("\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use clojurecraft_clojure::{LocalValue, ProbeCapture, ProbeStatus, ProbeValue};

    use super::*;

    const CODE: &str = "(ns w.core\n  (:require [clojure.string :as str]))\n\n(defn- parse-weight [weight]\n  (if (str/ends-with? weight \"kg\") 1 2))\n\n(defn w [input]\n  (->> (str/split-lines input)\n       (map #(str/split % #\", \"))\n       (mapv (fn [[date exercise sets-reps weight]]\n               (let [{:keys [a b] :as m} {:a 1}]\n                 {:date date :weight (parse-weight weight)})))))\n";

    /// The scopes and the instrumented code, probes numbered from 0.
    struct Instrumented {
        scopes: Vec<Scope>,
        code: String,
    }

    fn instrumented(source: &str) -> Option<Instrumented> {
        let (l, ins) = instrument(source, 0)?;
        Some(Instrumented {
            scopes: l.scopes,
            code: crate::insert::apply(source, &ins),
        })
    }

    #[test]
    fn functions_and_bindings_record_their_locals_without_moving_lines() {
        let l = instrumented(CODE).unwrap();
        let labels: Vec<&str> = l.scopes.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "(defn- parse-weight [weight])",
                "(defn w [input])",
                "(fn [[date exercise sets-reps weight]])",
                "let"
            ]
        );
        assert_eq!(l.code.lines().count(), CODE.lines().count());
        assert!(l.code.contains(
            "(clojurecraft.harness/probe-locals! 0 [weight] (if (str/ends-with? weight \"kg\") 1 2))"
        ));
        assert!(l.code.contains(
            "(clojurecraft.harness/probe-locals! 2 [date exercise sets-reps weight] (let [{:keys [a b] :as m} {:a 1}]\n                 (clojurecraft.harness/probe-locals! 3 [a b m] {:date date :weight (parse-weight weight)}))))"
        ), "{}", l.code);
        // The instrumented code reads, with every probe balanced.
        assert!(read_all(&l.code).is_ok());
    }

    #[test]
    fn code_without_locals_or_unreadable_code_is_not_instrumented() {
        assert!(instrumented("(ns a)\n(def x 1)\n").is_none());
        assert!(instrumented("(ns a)\n(defn f [x] (inc x)").is_none());
        let l = instrumented("(ns a)\n(defn f [_] 1)\n(defn g [x] {:pre [(pos? x)]} (inc x))\n").unwrap();
        assert_eq!(l.scopes.len(), 1);
        assert!(
            l.code
                .contains("{:pre [(pos? x)]} (clojurecraft.harness/probe-locals! 0 [x] (inc x))")
        );
    }

    fn local(name: &str, text: &str) -> LocalValue {
        LocalValue {
            name: name.into(),
            value: ProbeValue {
                kind: ValueKind::Value,
                text: text.into(),
                complete: true,
                message: None,
            },
        }
    }

    #[test]
    fn the_scopes_on_the_stack_show_their_latest_locals() {
        let (l, _) = instrument(CODE, 0).unwrap();
        let run = ProbeCall {
            index: 0,
            status: ProbeStatus::Error,
            message: None,
            consistent: true,
            probes: vec![
                ProbeCapture {
                    id: 0,
                    evaluations: 1,
                    values: vec![],
                    locals: vec![local("weight", "nil")],
                },
                ProbeCapture {
                    id: 1,
                    evaluations: 1,
                    values: vec![],
                    locals: vec![local("input", "\"x\"")],
                },
                ProbeCapture {
                    id: 2,
                    evaluations: 1,
                    values: vec![],
                    locals: vec![
                        local("date", "\"2026-03-02\""),
                        local("exercise", "\"Bench Press\""),
                        local("sets-reps", "\"3x8 @ 60kg\""),
                        local("weight", "nil"),
                    ],
                },
                ProbeCapture {
                    id: 3,
                    evaluations: 1,
                    values: vec![],
                    locals: vec![local("a", "1")],
                },
            ],
        };
        let error = HarnessError {
            line: Some(5),
            lines: vec![5, 12],
            ..HarnessError::default()
        };
        let d = describe(&l, &error, &run).unwrap();
        assert_eq!(
            d,
            "Values of the locals in the running code when it threw, innermost first:\n  in (defn- parse-weight [weight]) at line 4: weight = nil\n  in let at line 11: a = 1\n  in (fn [[date exercise sets-reps weight]]) at line 10: date = \"2026-03-02\", exercise = \"Bench Press\", sets-reps = \"3x8 @ 60kg\", weight = nil"
        );
        // A run that ended differently shows nothing.
        let other = ProbeCall {
            consistent: false,
            ..run
        };
        assert_eq!(describe(&l, &error, &other), None);
    }

    #[test]
    fn a_wrong_value_shows_every_scope_entered_in_source_order() {
        let (l, ins) = instrument(CODE, 7).unwrap();
        assert!(apply_ok(&ins));
        let run = ProbeCall {
            index: 0,
            status: ProbeStatus::Ok,
            message: None,
            consistent: true,
            probes: vec![
                ProbeCapture {
                    id: 9,
                    evaluations: 3,
                    values: vec![],
                    locals: vec![local("date", "\"2026-03-04\""), local("weight", "nil")],
                },
                ProbeCapture {
                    id: 8,
                    evaluations: 1,
                    values: vec![],
                    locals: vec![local("input", "\"x\"")],
                },
            ],
        };
        assert_eq!(
            describe_returned(&l, &run).unwrap(),
            "Values of the locals in each scope the call entered, in source order:\n  in (defn w [input]) at line 7 (entered once): input = \"x\"\n  in (fn [[date exercise sets-reps weight]]) at line 10 (entered 3 times, latest values): date = \"2026-03-04\", weight = nil"
        );
    }

    fn apply_ok(ins: &[Insertion]) -> bool {
        read_all(&crate::insert::apply(CODE, ins)).is_ok()
    }
}
