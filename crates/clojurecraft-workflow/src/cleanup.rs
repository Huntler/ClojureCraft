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

    // Names small models invent for clojure.string functions, where the real one is unambiguous.
    let string_aliases: Vec<String> = ns
        .and_then(Form::list)
        .unwrap_or_default()
        .iter()
        .filter_map(Form::list)
        .filter(|c| c.first().and_then(Form::keyword) == Some(":require"))
        .flat_map(|c| c[1..].iter())
        .filter_map(|spec| match spec.vector()? {
            [lib, opts @ ..] if lib.symbol() == Some("clojure.string") => opts
                .chunks(2)
                .find(|p| p[0].keyword() == Some(":as"))
                .and_then(|p| p.get(1)?.symbol())
                .map(str::to_owned),
            _ => None,
        })
        .chain(std::iter::once("clojure.string".to_owned()))
        .collect();
    let mut renamed = Vec::new();
    for t in tokenize(code) {
        if t.kind != TokenKind::Symbol || t.span.start < ns.map_or(0, |n| n.span().end) {
            continue;
        }
        let text = &code[t.span.clone()];
        let Some((prefix, name)) = text.split_once('/') else {
            continue;
        };
        let Some(real) = STRING_RENAMES
            .iter()
            .find(|(wrong, _)| *wrong == name)
            .map(|(_, r)| *r)
        else {
            continue;
        };
        if string_aliases.iter().any(|a| a == prefix) {
            edits.push((t.span.start + prefix.len() + 1..t.span.end, real.to_owned()));
            renamed.push(format!("{text} as {prefix}/{real}"));
        }
    }
    if !renamed.is_empty() {
        renamed.dedup();
        notes.push(format!(
            "Wrote {}: the former does not exist in clojure.string.",
            renamed.join(", ")
        ));
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

/// Prefixes of qualified symbols and the require each one needs.
const CONVENTIONAL_REQUIRES: [(&str, &str); 4] = [
    ("str", "[clojure.string :as str]"),
    ("set", "[clojure.set :as set]"),
    ("clojure.string", "[clojure.string]"),
    ("clojure.set", "[clojure.set]"),
];

/// Invented clojure.string names and the existing function with the same meaning.
const STRING_RENAMES: [(&str, &str); 8] = [
    ("contains?", "includes?"),
    ("includes", "includes?"),
    ("starts-with", "starts-with?"),
    ("ends-with", "ends-with?"),
    ("lowercase", "lower-case"),
    ("uppercase", "upper-case"),
    ("blank", "blank?"),
    ("index", "index-of"),
];

/// Fixes two slips that otherwise cost a whole attempt: `str/…` used without requiring
/// `clojure.string` (its conventional alias), and helpers defined with `defn` instead of `defn-`
/// next to the public `function`.
pub fn conventions(code: &str, function: &str) -> Tidied {
    let unchanged = || Tidied {
        code: code.to_owned(),
        notes: Vec::new(),
    };
    let Ok(forms) = read_all(code) else {
        return unchanged();
    };
    let head = |f: &Form| {
        f.list()
            .and_then(|l| l.first())
            .and_then(Form::symbol)
            .map(str::to_owned)
    };
    let Some(ns) = forms.first().filter(|f| head(f).as_deref() == Some("ns")) else {
        return unchanged();
    };
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut notes = Vec::new();

    let ns_items = ns.list().unwrap_or_default();
    // Every library the ns requires, and every alias it declares.
    let mut required: Vec<&str> = Vec::new();
    for c in ns_items.iter().filter_map(Form::list) {
        if c.first().and_then(Form::keyword) != Some(":require") {
            continue;
        }
        for spec in &c[1..] {
            match (spec.symbol(), spec.vector()) {
                (Some(lib), _) => required.push(lib),
                (None, Some([lib, opts @ ..])) => {
                    required.extend(lib.symbol());
                    for p in opts.chunks(2) {
                        if matches!(p[0].keyword(), Some(":as" | ":as-alias")) {
                            required.extend(p.get(1).and_then(Form::symbol));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut missing: Vec<&str> = Vec::new();
    for t in tokenize(code) {
        if t.kind != TokenKind::Symbol || t.span.start < ns.span().end {
            continue;
        }
        let Some((prefix, _)) = code[t.span.clone()].split_once('/') else {
            continue;
        };
        let Some(spec) = CONVENTIONAL_REQUIRES
            .iter()
            .find(|(p, _)| *p == prefix)
            .map(|(_, s)| *s)
        else {
            continue;
        };
        if !required.contains(&prefix) && !missing.contains(&spec) {
            missing.push(spec);
        }
    }
    if !missing.is_empty() {
        let require = ns_items
            .iter()
            .rfind(|c| c.list().and_then(|l| l.first()).and_then(Form::keyword) == Some(":require"));
        let specs = missing.join(" ");
        let (at, text) = match require {
            Some(r) => (r.span().end - 1, format!(" {specs}")),
            None => (ns.span().end - 1, format!("\n  (:require {specs})")),
        };
        edits.push((at..at, text));
        notes.push(format!(
            "Added {specs} to the ns declaration, which the code uses."
        ));
    }

    let defines = |f: &Form, kind: &str| {
        head(f).as_deref() == Some(kind) && f.list().and_then(|l| l.get(1)).and_then(Form::symbol).is_some()
    };
    let name_of = |f: &Form| {
        f.list()
            .and_then(|l| l.get(1))
            .and_then(Form::symbol)
            .map(str::to_owned)
    };
    if forms
        .iter()
        .any(|f| defines(f, "defn") && name_of(f).as_deref() == Some(function))
    {
        let mut made_private = Vec::new();
        for f in forms.iter().skip(1).filter(|f| defines(f, "defn")) {
            let Some(name) = name_of(f).filter(|n| n != function) else {
                continue;
            };
            let defn = f.list().unwrap_or_default()[0].span();
            edits.push((defn.end..defn.end, "-".to_owned()));
            made_private.push(name);
        }
        if !made_private.is_empty() {
            notes.push(format!(
                "Made {} private (defn-): only {function} is public.",
                made_private.join(", ")
            ));
        }
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

    #[test]
    fn invented_clojure_string_names_are_replaced_by_the_real_ones() {
        let t = tidy(
            "(ns a\n  (:require [clojure.string :as s]))\n(defn f [x] (when (s/contains? x \".\") (clojure.string/starts-with x \"a\")))\n",
        );
        assert_eq!(
            t.code,
            "(ns a\n  (:require [clojure.string :as s]))\n(defn f [x] (when (s/includes? x \".\") (clojure.string/starts-with? x \"a\")))\n"
        );
        assert_eq!(t.notes.len(), 1, "{:?}", t.notes);
        let other = "(ns a (:require [clojure.set :as s]))\n(defn f [x] (s/contains? x 1))\n";
        assert_eq!(tidy(other).code, other);
    }

    #[test]
    fn a_missing_clojure_string_require_is_added() {
        let t = conventions("(ns a)\n(defn f [s] (str/trim s))\n", "f");
        assert_eq!(
            t.code,
            "(ns a\n  (:require [clojure.string :as str]))\n(defn f [s] (str/trim s))\n"
        );
        let t = conventions(
            "(ns a (:require [clojure.set :as set]))\n(defn f [s] (str/trim s))\n",
            "f",
        );
        assert_eq!(
            t.code,
            "(ns a (:require [clojure.set :as set] [clojure.string :as str]))\n(defn f [s] (str/trim s))\n"
        );
        let fine = "(ns a (:require [clojure.string :as str]))\n(defn f [s] (str/trim (str s)))\n";
        assert_eq!(conventions(fine, "f").code, fine);
        let t = conventions("(ns a)\n(defn f [s] (set/union (clojure.string/trim s)))\n", "f");
        assert_eq!(
            t.code,
            "(ns a\n  (:require [clojure.set :as set] [clojure.string]))\n(defn f [s] (set/union (clojure.string/trim s)))\n"
        );
    }

    #[test]
    fn public_helpers_next_to_the_function_become_private() {
        let t = conventions(
            "(ns a)\n(defn parse-long [s] s)\n(defn f [s] (parse-long s))\n",
            "f",
        );
        assert_eq!(
            t.code,
            "(ns a)\n(defn- parse-long [s] s)\n(defn f [s] (parse-long s))\n"
        );
        assert_eq!(t.notes, ["Made parse-long private (defn-): only f is public."]);
        // Without the expected function, the structure check reports the names instead.
        let other = "(ns a)\n(defn g [s] s)\n(defn h [s] s)\n";
        assert_eq!(conventions(other, "f").code, other);
    }
}
