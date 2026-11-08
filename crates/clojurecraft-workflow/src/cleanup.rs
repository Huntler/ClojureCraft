//! Harmless extras removed from generated code before it is checked, so they do not cost an
//! attempt: top-level `(comment …)` forms (often left with example calls) and requires of
//! `clojure.core`, which is always available. Symbols written with a removed `clojure.core` alias
//! (`core/map`) are written out in full (`clojure.core/map`), and Java-style static calls of the
//! allowed classes (`Double.parseDouble`) are written as Clojure calls (`Double/parseDouble`).

use std::ops::Range;

use clojurecraft_edn::{TokenKind, tokenize};

use crate::deps::CORE;
use crate::structure::{Form, read_all};

/// Code after removing what does not matter, and what was removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tidied {
    pub code: String,
    pub notes: Vec<String>,
}

/// Removes top-level `(comment …)` forms and requires of `clojure.core` from readable code.
pub fn tidy(code: &str) -> Tidied {
    let unchanged = || Tidied {
        code: code.to_owned(),
        notes: Vec::new(),
    };
    let Ok(forms) = read_all(code) else {
        return unchanged();
    };
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut notes = Vec::new();

    let comments: Vec<Range<usize>> = forms
        .iter()
        .skip(1)
        .filter(|f| f.list().and_then(|l| l.first()).and_then(Form::symbol) == Some("comment"))
        .map(|f| with_line_end(code, f.span()))
        .collect();
    if !comments.is_empty() {
        notes.push(format!(
            "Removed {} top-level (comment …) form{}, which is not allowed in generated functions.",
            comments.len(),
            if comments.len() == 1 { "" } else { "s" }
        ));
        edits.extend(comments.into_iter().map(|r| (r, String::new())));
    }

    let mut aliases: Vec<String> = Vec::new();
    let ns = forms
        .first()
        .filter(|f| f.list().and_then(|l| l.first()).and_then(Form::symbol) == Some("ns"));
    if let Some(ns) = ns {
        for clause in ns.list().unwrap_or_default().iter().skip(2) {
            let Some(parts) = clause.list() else { continue };
            if parts.first().and_then(Form::keyword) != Some(":require") {
                continue;
            }
            let specs = &parts[1..];
            let core: Vec<&Form> = specs.iter().filter(|s| requires_core(s)).collect();
            if core.is_empty() {
                continue;
            }
            for spec in &core {
                if let Some([_, opts @ ..]) = spec.vector() {
                    for pair in opts.chunks(2) {
                        if matches!(pair[0].keyword(), Some(":as" | ":as-alias"))
                            && let Some(alias) = pair.get(1).and_then(Form::symbol)
                            && alias != CORE
                        {
                            aliases.push(alias.to_owned());
                        }
                    }
                }
            }
            if core.len() == specs.len() {
                edits.push((leading_space(code, clause.span()), String::new()));
            } else {
                edits.extend(
                    core.iter()
                        .map(|s| (leading_space(code, s.span()), String::new())),
                );
            }
        }
        if !aliases.is_empty() || edits.iter().any(|(r, _)| r.start < ns.span().end) {
            notes.push("Removed the require of clojure.core, which is always available.".into());
        }
        if !aliases.is_empty() {
            for t in tokenize(code) {
                if t.kind != TokenKind::Symbol || t.span.start < ns.span().end {
                    continue;
                }
                let text = &code[t.span.clone()];
                if let Some((prefix, name)) = text.split_once('/')
                    && !name.is_empty()
                    && aliases.iter().any(|a| a == prefix)
                {
                    edits.push((t.span.start..t.span.start + prefix.len(), CORE.to_owned()));
                }
            }
        }
    }

    // `Double.parseDouble` is Java syntax; Clojure calls a static method as `Double/parseDouble`.
    let policy = clojurecraft_clojure::harness_policy();
    let body_start = ns.map_or(0, |n| n.span().end);
    let mut dotted = Vec::new();
    for t in tokenize(code) {
        if t.kind != TokenKind::Symbol || t.span.start < body_start {
            continue;
        }
        let text = &code[t.span.clone()];
        if let Some((class, member)) = text.split_once('.')
            && policy.allowed_classes.contains(class)
            && !member.is_empty()
            && member.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            let dot = t.span.start + class.len();
            edits.push((dot..dot + 1, "/".to_owned()));
            dotted.push(text.to_owned());
        }
    }
    if !dotted.is_empty() {
        dotted.dedup();
        notes.push(format!(
            "Wrote {} as Clojure static method calls ({}).",
            dotted.join(", "),
            dotted
                .iter()
                .map(|d| d.replacen('.', "/", 1))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if edits.is_empty() {
        return unchanged();
    }
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = code.to_owned();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    Tidied { code: out, notes }
}

fn requires_core(spec: &Form) -> bool {
    match (spec.symbol(), spec.vector()) {
        (Some(lib), _) => lib == CORE,
        (None, Some([first, ..])) => first.symbol() == Some(CORE),
        _ => false,
    }
}

/// `span` with the spaces and the one line break that follow it.
fn with_line_end(code: &str, span: Range<usize>) -> Range<usize> {
    let rest = &code[span.end..];
    let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let newline = usize::from(rest[spaces..].starts_with('\n'));
    span.start..span.end + spaces + newline
}

/// `span` with the whitespace before it, so that removing it leaves no gap.
fn leading_space(code: &str, span: Range<usize>) -> Range<usize> {
    let before = &code[..span.start];
    let start = before.trim_end_matches([' ', '\t', '\n', '\r', ',']).len();
    start..span.end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_comment_forms_are_removed() {
        let t = tidy("(ns a)\n(defn f [x] (inc x))\n\n(comment\n  (f 1))\n");
        assert_eq!(t.code, "(ns a)\n(defn f [x] (inc x))\n\n");
        assert_eq!(t.notes.len(), 1);
        // A comment form inside a function is left to the usual checks.
        let inner = "(ns a)\n(defn f [x] (comment x) x)\n";
        assert_eq!(tidy(inner).code, inner);
    }

    #[test]
    fn requires_of_clojure_core_are_removed_and_their_alias_written_out() {
        let t = tidy(
            "(ns a\n  (:require [clojure.core :as core]\n            [clojure.string :as str]))\n(defn f [x] (core/map str/trim x))\n",
        );
        assert_eq!(
            t.code,
            "(ns a\n  (:require\n            [clojure.string :as str]))\n(defn f [x] (clojure.core/map str/trim x))\n"
        );
        let t = tidy("(ns a (:require [clojure.core :as c]))\n(defn f [x] (c/inc x))\n");
        assert_eq!(t.code, "(ns a)\n(defn f [x] (clojure.core/inc x))\n");
        assert_eq!(
            t.notes,
            ["Removed the require of clojure.core, which is always available."]
        );
    }

    #[test]
    fn other_code_is_unchanged() {
        for code in [
            "(ns a (:require [clojure.string :as str]))\n(defn f [x] (str/trim x))\n",
            "(ns a)\n(defn f [x] (inc x)",
        ] {
            assert_eq!(
                tidy(code),
                Tidied {
                    code: code.into(),
                    notes: vec![]
                }
            );
        }
    }

    #[test]
    fn java_style_static_calls_of_allowed_classes_are_rewritten() {
        let t = tidy("(ns a)\n(defn f [s] (+ (Double.parseDouble s) (Long.parseLong s) (Math.abs -1)))\n");
        assert_eq!(
            t.code,
            "(ns a)\n(defn f [s] (+ (Double/parseDouble s) (Long/parseLong s) (Math/abs -1)))\n"
        );
        assert!(t.notes[0].contains("Double.parseDouble"), "{:?}", t.notes);
        // Other classes stay as they are, so the usual check rejects them.
        let other = "(ns a)\n(defn f [s] (java.util.UUID.randomUUID))\n";
        assert_eq!(tidy(other).code, other);
    }
}
